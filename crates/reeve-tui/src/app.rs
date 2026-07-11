//! TUI event loop for the Reeve lead chat screen.
//!
//! [`run`] owns terminal lifecycle (raw mode, alternate screen) and drives the
//! main event loop. The loop reacts to two event sources:
//!
//! 1. **Filesystem watcher events** — reloads agent state from disk and
//!    triggers a redraw. Debounced to 250 ms by [`crate::watcher`].
//! 2. **Crossterm keyboard events** — updates the input buffer, submits
//!    messages, or exits.
//!
//! The TUI talks to the runtime only through the filesystem. No sockets, no RPC.
//!
//! # Terminal cleanup
//!
//! Raw mode and alternate screen are restored on exit regardless of how `run`
//! returns (normal exit or error), via an RAII-style cleanup guard.

use std::io::{self, Stdout};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reeve_runtime::{IdentityRegistry, OperatorKeyStore};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEvent,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand as _};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use reeve_runtime::capability::{CapabilityProfile, Thresholds};
use reeve_runtime::{audit_log_path, AgentDirs, AgentRegistry};

use crate::panopticon::read_snapshot as read_panopticon_snapshot;
use crate::panopticon::AUDIT_TAIL_BYTES;
use crate::reader::{read_authority_decisions_tail, read_conversation, read_cost, read_status};
use crate::session::{self, Session};
use crate::state::{AppState, AuthorityDecision, InspectTab, Screen};
use crate::submit::submit_message;
use crate::watcher::watch_tree;

/// Poll timeout for crossterm events. The loop also reacts to watcher signals,
/// so this just sets the maximum latency between a watcher event and a redraw.
const POLL_TIMEOUT: Duration = Duration::from_millis(100);

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors surfaced by the TUI event loop.
#[derive(Debug)]
pub enum TuiError {
    /// Terminal setup, draw, or restore failed.
    Terminal(io::Error),
    /// Filesystem watcher could not be started.
    Watcher(notify::Error),
    /// A message submission attempt failed.
    Submit(crate::submit::SubmitError),
}

impl std::fmt::Display for TuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminal(err) => write!(f, "terminal error: {err}"),
            Self::Watcher(err) => write!(f, "filesystem watcher error: {err}"),
            Self::Submit(err) => write!(f, "message submit error: {err}"),
        }
    }
}

impl std::error::Error for TuiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Terminal(err) => Some(err),
            Self::Watcher(err) => Some(err),
            Self::Submit(err) => Some(err),
        }
    }
}

// ── Terminal RAII guard ───────────────────────────────────────────────────────

/// RAII guard that restores the terminal on drop.
///
/// Constructed by [`setup_terminal`]; dropped at the end of [`run`] or on
/// early error return. Raw mode and alternate screen are always restored.
///
/// No stdout is stored here: `CrosstermBackend` already owns one handle, and
/// storing a second long-lived handle would give two independent `io::Stdout`
/// values writing to fd 1. Instead, `drop` creates an ephemeral handle only at
/// cleanup time, after the event loop (and the backend) have exited.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Ignore errors during cleanup: nothing useful can be done if restore fails.
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Enable raw mode and alternate screen, returning a guard that restores them.
fn setup_terminal() -> Result<(Terminal<CrosstermBackend<Stdout>>, TerminalGuard), TuiError> {
    enable_raw_mode().map_err(TuiError::Terminal)?;
    // Guard is constructed immediately after raw mode is enabled so that, if
    // EnterAlternateScreen fails, drop() will still call disable_raw_mode().
    // The LeaveAlternateScreen in drop() is harmless when the alternate screen
    // was never entered.
    let guard = TerminalGuard;
    let mut stdout = io::stdout();
    stdout
        .execute(EnterAlternateScreen)
        .map_err(TuiError::Terminal)?;
    // Capture mouse so wheel events route into the event loop for
    // scrolling the conversation pane. DisableMouseCapture runs in
    // TerminalGuard::drop so the terminal is restored even on panic.
    stdout
        .execute(EnableMouseCapture)
        .map_err(TuiError::Terminal)?;
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend).map_err(TuiError::Terminal)?;
    Ok((terminal, guard))
}

/// Pick the first operator-typed identity from the registry. Used at startup
/// to seed `AppState.operator_id` so the renderer can label inbound entries
/// signed by the operator as `"you"`. Returns `None` if no operator exists or
/// the registry read fails — the renderer falls back to a short-id label.
fn first_operator_id(registry: &IdentityRegistry) -> Option<reeve_types::IdentityId> {
    registry.list().ok()?.into_iter().find_map(|stored| {
        (stored.identity().identity_type == reeve_types::IdentityType::Operator)
            .then(|| stored.identity().identity_id)
    })
}

// ── Session memory ────────────────────────────────────────────────────────────

/// Decide which screen to open on startup, given the session file and the
/// agent registry.
///
/// Returns [`Screen::Chat`] only when **all** of these hold:
/// - The session file records a `last_agent` value.
/// - That agent name is present in the agent registry.
/// - The registry marks the agent as Running (a stopped agent's chat is
///   not a useful landing place).
///
/// Otherwise returns [`Screen::Panopticon`]: no session, missing agent, or
/// non-running agent all collapse to "open the panopticon instead." This
/// is the operator-facing behaviour the spec mandates and the only
/// reasonable fallback when the recorded chat target is unreachable.
fn initial_screen_for_session(session: &Session, agent_registry_path: &Path) -> Screen {
    let Some(name) = session.last_agent.as_deref() else {
        return Screen::Panopticon;
    };
    let Ok(registry) = AgentRegistry::open(agent_registry_path.to_path_buf()) else {
        return Screen::Panopticon;
    };
    match registry.lookup(name) {
        Some(record) if matches!(record.status, reeve_runtime::AgentStatus::Running) => {
            Screen::Chat
        }
        _ => Screen::Panopticon,
    }
}

/// Write the session record on clean TUI exit. Per the spec, only chat-screen
/// exits should record `last_agent`; an exit from the panopticon must not
/// overwrite a valid prior chat record.
///
/// Errors are deliberately swallowed: the operator should never see a TUI
/// quit fail because of a session-file write hiccup. The worst-case
/// presentation is "next launch lands on the panopticon" — the same
/// fallback as a missing session file.
fn record_exit_to_session(session_path: &Path, state: &AppState) {
    if state.screen != Screen::Chat {
        return;
    }
    let session = Session {
        last_agent: Some(state.chat_agent_name.clone()),
    };
    let _ = session::write(session_path, &session);
}

// ── State loading ─────────────────────────────────────────────────────────────

/// Reload all on-disk state into `state`.
///
/// Called on every watcher callback. Reads chat-screen data from the
/// currently selected chat agent (`state.chat_agent_name`) and refreshes
/// the panopticon snapshot from the agent registry. Individual reads
/// return safe defaults on any filesystem error (see `crate::reader`).
///
/// `data_dir` is the runtime data root; per-agent dirs are derived from
/// it plus the chat-agent name on every reload, so switching the
/// chat-agent (e.g., Enter on a panopticon row) requires only a state
/// mutation followed by a reload trigger — no signature changes
/// downstream.
fn reload_state(state: &mut AppState, data_dir: &Path, agent_registry_path: &Path) {
    let active = active_agent_name(state).to_owned();
    if let Ok(dirs) = AgentDirs::open(data_dir, &active) {
        state.status = read_status(&dirs.status_path());
        state.conversation = read_conversation(&dirs.conversation_path());
        state.cost_usd = read_cost(&dirs.cost_path());
        if let Some(snapshot) = read_spawn_snapshot(&dirs) {
            state.model_id.clear();
            state.model_id.push_str(snapshot.model());
            state.persona_name = snapshot.persona_name;
        }
        // Load thresholds for the Model tab — only when not actively editing
        // so a slow reload doesn't overwrite what the operator is typing.
        if !state.inspect_model_editing {
            if let Ok(profile) =
                reeve_runtime::capability::load_capability_profile(&dirs.profile_path())
            {
                state.inspect_thresholds = profile.thresholds;
            }
        }
    }
    state.inspect_authority_decisions =
        load_inspect_decisions(state, data_dir, agent_registry_path);
    state.panopticon = read_panopticon_snapshot(data_dir, agent_registry_path, state.operator_id);
    // The quarantine snapshot is rebuilt on every reload so the
    // panopticon's queue count and the review screen's entry list stay
    // in sync. The reader walks every agent's `inbox/quarantine/`
    // directory and tolerates missing/unreadable directories silently
    // (see `quarantine_view::read_files`), so this is safe to run even
    // when the operator has never opened the review screen.
    state.quarantine = crate::quarantine_view::read_snapshot(data_dir);
    // Discard or watcher activity can shrink the entry list out from
    // under a stale focus index. Clamp here, in the reload, rather
    // than at every render-site read.
    let entry_count = state.quarantine.entries.len();
    if entry_count == 0 {
        state.quarantine_focus = 0;
    } else if state.quarantine_focus >= entry_count {
        state.quarantine_focus = entry_count - 1;
    }
    resolve_pending_engagement(state, data_dir);
}

/// Resolve a pending `/engagement` operation against the audit log.
///
/// The coordinator writes exactly one `engagement.*` event per operation
/// (success or refusal), so the audit tail is the confirmation channel —
/// polling the engagement record instead could mistake a pre-existing
/// record for the operation's own effect. Runs on every reload tick; the
/// daemon's processing of the envelope itself generates filesystem events
/// under `agents/`, so the resolving reload usually fires immediately
/// after the operation lands.
fn resolve_pending_engagement(state: &mut AppState, data_dir: &Path) {
    let Some(pending) = state.pending_engagement.clone() else {
        return;
    };
    let outcome = crate::reader::read_engagement_outcome_tail(
        &audit_log_path(data_dir),
        AUDIT_TAIL_BYTES,
        &pending.name,
        pending.sent_at,
    );
    if let Some(outcome) = outcome {
        state.notice = Some(match outcome.kind.as_str() {
            "engagement.opened" => format!("engagement opened: {}", pending.name),
            "engagement.closed" => format!("engagement closed: {}", pending.name),
            "engagement.reopened" => format!("engagement reopened: {}", pending.name),
            _ => format!(
                "engagement {} {} refused: {}",
                pending.verb,
                pending.name,
                outcome.reason.as_deref().unwrap_or("unknown"),
            ),
        });
        state.pending_engagement = None;
    } else if std::time::Instant::now() >= pending.deadline {
        state.notice = Some(format!(
            "no confirmation for {} {} — check `reeve engagement list` and the audit log",
            pending.verb, pending.name,
        ));
        state.pending_engagement = None;
    }
}

/// Load the inspected agent's authority decisions, newest first, from the
/// audit-log tail.
///
/// Returns empty unless the inspect screen is open with a resolvable target:
/// the Decisions tab is the only consumer, so every other screen skips the
/// audit read entirely rather than paying it on every watcher tick. The
/// audit log keys decisions on identity, so the agent's `identity_id` is
/// looked up from the registry by role name and used as the filter; the
/// chronological tail is reversed because the tab renders newest first.
fn load_inspect_decisions(
    state: &AppState,
    data_dir: &Path,
    agent_registry_path: &Path,
) -> Vec<AuthorityDecision> {
    if state.screen != Screen::Inspect {
        return Vec::new();
    }
    let Some(name) = state.inspect_agent_name.as_deref() else {
        return Vec::new();
    };
    let Ok(registry) = AgentRegistry::open(agent_registry_path.to_path_buf()) else {
        return Vec::new();
    };
    let Some(record) = registry.lookup(name) else {
        return Vec::new();
    };
    let agent_id = record.identity_id;
    let mut decisions: Vec<AuthorityDecision> =
        read_authority_decisions_tail(&audit_log_path(data_dir), AUDIT_TAIL_BYTES)
            .into_iter()
            .filter(|decision| decision.agent_id == agent_id)
            .collect();
    decisions.reverse();
    decisions
}

/// Resolve the agent whose disk state should populate the per-agent
/// fields (conversation, status, cost, persona, model) on the next
/// reload.
///
/// On [`Screen::Inspect`] the inspect target wins so the drill-in view
/// reads the agent the operator just Enter-ed into. Everywhere else
/// (Chat, Panopticon, Quarantine) the chat target wins so the chat
/// screen's data is fresh when the operator Tab's back to it. Falling
/// back to `chat_agent_name` when `inspect_agent_name` is None is
/// defensive — `Enter` from the panopticon always sets the inspect
/// target before switching screens, so the None branch is unreachable
/// under normal flow.
fn active_agent_name(state: &AppState) -> &str {
    match (state.screen, state.inspect_agent_name.as_deref()) {
        (Screen::Inspect, Some(name)) => name,
        _ => &state.chat_agent_name,
    }
}

/// Read the spawn snapshot for the agent at `dirs.agent_toml_path()`.
/// Returns `None` on any filesystem or parse error — the chat title bar
/// keeps its previous persona/model labels rather than going blank.
fn read_spawn_snapshot(dirs: &AgentDirs) -> Option<reeve_runtime::SpawnSnapshot> {
    let body = std::fs::read_to_string(dirs.agent_toml_path()).ok()?;
    toml::from_str(&body).ok()
}

// ── Event loop ────────────────────────────────────────────────────────────────

/// Run the TUI until the user quits.
///
/// Blocks until `q` / `Esc` is pressed or an unrecoverable error occurs.
/// Terminal is restored (raw mode off, alternate screen left) on all exit paths.
///
/// # Parameters
///
/// - `data_dir`: runtime data root. Per-agent `AgentDirs` are derived from
///   here and the currently focused chat agent on every reload — switching
///   the chat target is a state mutation, not a signature change.
/// - `agent_registry_path`: agent registry TOML; drives the panopticon
///   snapshot and resolves chat-resume targets at startup.
/// - `session_path`: per-operator session memory file
///   (`<state_dir>/session.toml`).
/// - `registry`: the identity registry, used by [`submit_message`] to
///   locate the operator identity.
/// - `keystore`: the platform keystore, used by [`submit_message`] to
///   retrieve the operator signing key.
///
/// # Errors
///
/// Returns [`TuiError::Terminal`] on terminal I/O failure,
/// [`TuiError::Watcher`] if the filesystem watcher cannot start, or
/// [`TuiError::Submit`] if a message write fails (not currently surfaced to the
/// user — future iterations should show an inline error).
#[expect(
    clippy::too_many_arguments,
    reason = "run is the multi-screen TUI entry point and threads four \
              filesystem roots, the operator credentials, plus an optional \
              CLI-supplied chat target. Bundling into a context struct \
              trades clarity for indirection at the only non-test call \
              sites (cmd_reeve and cmd_attach in reeve-cli)."
)]
#[expect(
    clippy::too_many_lines,
    reason = "linear event loop: each branch is short but the full keyboard \
              and reload surface must be wired in one place to stay auditable"
)]
pub fn run(
    data_dir: &Path,
    agent_registry_path: &Path,
    session_path: &Path,
    initial_chat_agent: Option<&str>,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let (mut terminal, _guard) = setup_terminal()?;

    let needs_reload = Arc::new(AtomicBool::new(true)); // true = load immediately on start
    let needs_reload_clone = Arc::clone(&needs_reload);

    // Watch the full `<data_dir>/agents/` tree so the panopticon sees
    // worker transitions (status/cost/conversation changes in any agent's
    // dir) and registry changes (a new spawn rewrites
    // `agents/registry.toml`). One recursive watch covers every agent
    // regardless of count — no inotify accounting per agent. Kept alive
    // until run() returns.
    let agents_root = data_dir.join("agents");
    let _watcher = watch_tree(&agents_root, move || {
        needs_reload_clone.store(true, Ordering::Release);
    })
    .map_err(TuiError::Watcher)?;

    let mut state = AppState::default();
    // Resolve the operator identity once at startup so inbound entries can
    // render with "you" for the operator and a distinct sender label for
    // worker/peer replies. If lookup fails the label falls back to a short
    // id, which is still better than the pre-attribution "you for everyone".
    state.operator_id = first_operator_id(registry);
    // Pick the starting screen + chat target from session memory: chat
    // when the last agent the operator was talking to is still running,
    // panopticon otherwise (no session, agent gone, agent stopped). The
    // default `Screen::Chat` + `chat_agent_name = "lead"` on `AppState`
    // are overridden here because the panopticon-as-home story applies
    // at startup; the defaults exist for tests and constructed-in-place
    // states.
    if let Some(name) = initial_chat_agent {
        // `reeve attach <name>` — explicit override; open that agent's
        // chat unconditionally. Skips the session-memory consultation:
        // the operator told us which chat they want, that's the chat
        // they get.
        state.chat_agent_name.clear();
        state.chat_agent_name.push_str(name);
        state.screen = Screen::Chat;
    } else {
        let session = session::read(session_path);
        state.screen = initial_screen_for_session(&session, agent_registry_path);
        if state.screen == Screen::Chat {
            if let Some(name) = session.last_agent.as_deref() {
                state.chat_agent_name.clear();
                state.chat_agent_name.push_str(name);
            }
        }
    }

    let mut last_forced_reload = std::time::Instant::now();
    loop {
        // Force a reload every 2 seconds regardless of watcher events.
        // notify's recommended_watcher on macOS (both kqueue and FSEvents)
        // silently stops delivering events in certain conditions. Polling
        // at 2 s is fast enough to feel live and is the pragmatic fix
        // until the notify reliability issue is resolved upstream.
        if last_forced_reload.elapsed() >= Duration::from_secs(2) {
            needs_reload.store(true, Ordering::Release);
            last_forced_reload = std::time::Instant::now();
        }

        if needs_reload.swap(false, Ordering::Acquire) {
            reload_state(&mut state, data_dir, agent_registry_path);
        }

        terminal
            .draw(|frame| match state.screen {
                Screen::Chat => crate::ui::draw(frame, &state),
                Screen::Panopticon => {
                    crate::ui_panopticon::draw(frame, &state.panopticon, state.panopticon_focus);
                }
                Screen::Inspect => crate::ui_inspect::draw(frame, &state),
                Screen::Quarantine => crate::ui_quarantine::draw(frame, &state),
                Screen::QuarantineCompose => {
                    crate::ui_quarantine::draw_compose(frame, &state);
                }
            })
            .map_err(TuiError::Terminal)?;

        // Short timeout keeps watcher latency bounded.
        if event::poll(POLL_TIMEOUT).map_err(TuiError::Terminal)? {
            match event::read().map_err(TuiError::Terminal)? {
                Event::Key(key) => {
                    let prev_screen = state.screen;
                    let prev_chat_agent = state.chat_agent_name.clone();
                    let prev_inspect_agent = state.inspect_agent_name.clone();
                    if handle_key(
                        key,
                        &mut state,
                        data_dir,
                        agent_registry_path,
                        registry,
                        keystore,
                    )? {
                        // Record the chat target the operator was on when
                        // they quit, so the next launch can resume it.
                        // Quitting from the panopticon does not write
                        // (would overwrite a valid prior chat record).
                        record_exit_to_session(session_path, &state);
                        return Ok(());
                    }
                    update_slash_suggestions(&mut state, data_dir);
                    // Force a fresh panopticon read only on the *transition*
                    // into the panopticon screen, so the operator's first
                    // frame shows current state. Refreshing on every
                    // keystroke while already in the panopticon walks every
                    // agent's status / cost / conversation files per j/k
                    // press — pointless IO at typing cadence.
                    if state.screen == Screen::Panopticon && prev_screen != Screen::Panopticon {
                        state.panopticon = read_panopticon_snapshot(
                            data_dir,
                            agent_registry_path,
                            state.operator_id,
                        );
                    }
                    // Trigger a reload whenever the *active* agent (the one
                    // populating state.conversation/status/cost) could
                    // have changed:
                    //
                    // - `chat_agent_name` differs: only happens at startup
                    //   today, but defensive in case a future surface
                    //   introduces mid-session switching.
                    // - `inspect_agent_name` differs: Enter on a panopticon
                    //   row picked a new inspect target.
                    // - the screen transitioned between Chat and Inspect:
                    //   `active_agent_name` returns a different name
                    //   depending on which screen is up, so the on-screen
                    //   data must refresh even if neither name changed.
                    let active_screen_changed = prev_screen != state.screen
                        && matches!(state.screen, Screen::Chat | Screen::Inspect);
                    if state.chat_agent_name != prev_chat_agent
                        || state.inspect_agent_name != prev_inspect_agent
                        || active_screen_changed
                    {
                        needs_reload.store(true, Ordering::Release);
                    }
                }
                Event::Mouse(mouse) => handle_mouse(mouse, &mut state),
                // Resize triggers a full redraw on the next iteration naturally.
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        }
    }
}

/// Mouse wheel maps to conversation-pane scroll. The wheel typically emits
/// one event per detent on macOS / Linux; bumping three rows per detent
/// matches the cadence of every other terminal app the operator is used to.
fn handle_mouse(event: MouseEvent, state: &mut AppState) {
    const ROWS_PER_DETENT: u16 = 3;
    match event.kind {
        MouseEventKind::ScrollUp => state.scroll_up(ROWS_PER_DETENT),
        MouseEventKind::ScrollDown => state.scroll_down(ROWS_PER_DETENT),
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Moved
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {}
    }
}

/// Handle one keyboard event. Returns `true` when the TUI should exit.
///
/// Dispatch is screen-aware: chat keys (input typing, scrolling, submit)
/// only fire on [`Screen::Chat`]; panopticon keys (`j`/`k` navigate,
/// `Enter` open focused agent) only fire on [`Screen::Panopticon`].
///
/// `q` (no modifier) quits from either screen; `Tab` toggles. `Esc` is
/// screen-aware: from chat it quits (consistent with prior behavior); from
/// the panopticon it pops back to chat — the vim / k9s / lazygit
/// convention of "Esc = back" that the operator brings with them. Without
/// the screen-aware behavior, an operator who hits Esc expecting to leave
/// the panopticon exits the whole TUI instead.
#[expect(
    clippy::too_many_arguments,
    reason = "each parameter is a distinct runtime resource with no natural grouping peer"
)]
fn handle_key(
    key: event::KeyEvent,
    state: &mut AppState,
    data_dir: &Path,
    agent_registry_path: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    // Any keystroke dismisses the transient footer notice; whatever the
    // operator does next supersedes it.
    state.notice = None;
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) => return Ok(true),
        (KeyCode::Tab, _) => {
            // Tab toggles Chat ↔ Panopticon for the top-level navigation.
            // From Quarantine, Tab pops back to the panopticon (its
            // conceptual parent screen). On Inspect, Tab is screen-local
            // — it cycles tabs across the top of the inspect view — so
            // the global Tab handler defers to handle_key_inspect.
            // On Chat with a slash-command in the input, Tab is completion
            // — the operator is mid-command, not navigating.
            match state.screen {
                Screen::Chat if state.input.trim_start().starts_with('/') => {
                    complete_slash_command(state, data_dir);
                }
                Screen::Chat | Screen::Panopticon => state.toggle_screen(),
                // Tab from Quarantine/QuarantineCompose pops back to
                // panopticon. QuarantineCompose is a modal; Tab cancels
                // the compose and goes to the quarantine list's parent
                // rather than staying in the review screen.
                Screen::Quarantine | Screen::QuarantineCompose => {
                    quarantine_compose_cancel(state);
                    state.screen = Screen::Panopticon;
                }
                Screen::Inspect => {
                    return handle_key_inspect(key, state, data_dir, registry, keystore)
                }
            }
            return Ok(false);
        }
        (KeyCode::Esc, _) => {
            match state.screen {
                Screen::Chat => return Ok(true),
                Screen::Panopticon => state.screen = Screen::Chat,
                // Inspect and Quarantine both pop one level back to the
                // panopticon.
                Screen::Inspect | Screen::Quarantine => state.screen = Screen::Panopticon,
                // Esc from the compose surface cancels without sending
                // and returns to the quarantine review list.
                Screen::QuarantineCompose => {
                    quarantine_compose_cancel(state);
                    state.screen = Screen::Quarantine;
                }
            }
            return Ok(false);
        }
        _ => {}
    }

    match state.screen {
        Screen::Chat => handle_key_chat(key, state, data_dir, registry, keystore),
        Screen::Panopticon => Ok(handle_key_panopticon(key, state, agent_registry_path)),
        Screen::Inspect => handle_key_inspect(key, state, data_dir, registry, keystore),
        Screen::Quarantine => Ok(handle_key_quarantine(key, state)),
        Screen::QuarantineCompose => {
            handle_key_quarantine_compose(key, state, data_dir, registry, keystore)
        }
    }
}

/// Chat-screen key bindings: typing, scrolling, submit on Enter.
fn handle_key_chat(
    key: event::KeyEvent,
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    // Conversation-pane scroll. ROWS_PER_PAGE is intentionally a constant
    // rather than terminal height so the operator's muscle memory ('PgUp
    // moves about a screenful') doesn't change with window size.
    const ROWS_PER_PAGE: u16 = 10;
    const ROWS_PER_LINE_NUDGE: u16 = 1;

    match (key.code, key.modifiers) {
        (KeyCode::Enter, KeyModifiers::NONE) => {
            submit_input(state, data_dir, registry, keystore)?;
        }

        (KeyCode::Backspace, _) => {
            let mut s = state.input.clone();
            if !s.is_empty() {
                // Remove the last UTF-8 character (not the last byte).
                let trim_pos = s.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                s.truncate(trim_pos);
            }
            state.set_input(s);
        }

        (KeyCode::PageUp, _) => state.scroll_up(ROWS_PER_PAGE),
        (KeyCode::PageDown, _) => state.scroll_down(ROWS_PER_PAGE),
        // Shift+Up/Down: fine-grained nudges. Plain arrow keys are reserved
        // for future input-cursor movement so we don't claim them here.
        (KeyCode::Up, KeyModifiers::SHIFT) => state.scroll_up(ROWS_PER_LINE_NUDGE),
        (KeyCode::Down, KeyModifiers::SHIFT) => state.scroll_down(ROWS_PER_LINE_NUDGE),
        (KeyCode::End, _) => state.scroll_to_bottom(),

        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            let mut s = state.input.clone();
            s.push(c);
            state.set_input(s);
        }

        _ => {}
    }

    Ok(false)
}

/// Panopticon-screen key bindings.
fn handle_key_panopticon(
    key: event::KeyEvent,
    state: &mut AppState,
    agent_registry_path: &Path,
) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
            state.panopticon_focus_down();
        }
        (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
            state.panopticon_focus_up();
        }
        (KeyCode::Enter, _) => {
            if let Some(agent) = state.panopticon.agents.get(state.panopticon_focus) {
                state.inspect_agent_name = Some(agent.name.clone());
                state.inspect_tab = InspectTab::Thread;
                state.scroll_to_bottom();
            }
            state.screen = Screen::Inspect;
        }
        // `c` opens a full-screen chat session with the focused agent.
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            if let Some(agent) = state.panopticon.agents.get(state.panopticon_focus) {
                state.chat_agent_name.clear();
                state.chat_agent_name.push_str(&agent.name);
                state.scroll_to_bottom();
                state.screen = Screen::Chat;
            }
        }
        // `d` removes the focused agent's registry record.
        // The lead is protected; all other agents may be deleted regardless
        // of running state. The agent directory is left on disk.
        (KeyCode::Char('d'), KeyModifiers::NONE) => {
            if let Some(agent) = state.panopticon.agents.get(state.panopticon_focus) {
                if agent.name != "lead" {
                    if let Ok(mut reg) = AgentRegistry::open(agent_registry_path.to_path_buf()) {
                        let _ = reg.remove(&agent.name);
                    }
                }
            }
        }
        (KeyCode::Char('Q'), _) => state.screen = Screen::Quarantine,
        _ => {}
    }
    false
}

/// Inspect-screen key bindings.
///
/// On the Thread tab the inspect screen doubles as a chat interface: typing
/// populates the input buffer and Enter submits to the inspected agent.
/// Tab/Shift+Tab switch between inspect tabs (clearing the input buffer when
/// leaving Thread so stale text doesn't bleed into other tabs).
///
/// `h`/`Esc` return to the panopticon. `q` and global `Esc`/`Tab` are
/// consumed by the dispatcher before reaching here.
#[expect(
    clippy::too_many_lines,
    reason = "three tab-specific input modes (Thread chat, Model editor, global \
              navigation) must be dispatched in one function to share the key event"
)]
fn handle_key_inspect(
    key: event::KeyEvent,
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    const ROWS_PER_PAGE: u16 = 10;
    const ROWS_PER_LINE_NUDGE: u16 = 1;

    // On the Model tab, j/k navigate threshold fields; Enter begins editing;
    // while editing, chars update the input buffer, Enter saves, Esc cancels.
    if state.inspect_tab == InspectTab::Model {
        let n = crate::state::MODEL_FIELD_LABELS.len();
        if state.inspect_model_editing {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => {
                    state.inspect_model_editing = false;
                    state.set_input(String::new());
                    return Ok(false);
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    save_threshold(state, data_dir);
                    state.inspect_model_editing = false;
                    state.set_input(String::new());
                    return Ok(false);
                }
                (KeyCode::Backspace, _) => {
                    let mut s = state.input.clone();
                    if !s.is_empty() {
                        let trim = s.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                        s.truncate(trim);
                    }
                    state.set_input(s);
                    return Ok(false);
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    let mut s = state.input.clone();
                    s.push(c);
                    state.set_input(s);
                    return Ok(false);
                }
                _ => {}
            }
        } else {
            match (key.code, key.modifiers) {
                (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
                    state.inspect_model_field =
                        (state.inspect_model_field + 1).min(n.saturating_sub(1));
                    return Ok(false);
                }
                (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
                    state.inspect_model_field = state.inspect_model_field.saturating_sub(1);
                    return Ok(false);
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    state.inspect_model_editing = true;
                    let current = threshold_field_display(
                        &state.inspect_thresholds,
                        state.inspect_model_field,
                    );
                    state.set_input(current);
                    return Ok(false);
                }
                _ => {}
            }
        }
    }

    // On the Thread tab, character input and Enter go to the agent.
    if state.inspect_tab == InspectTab::Thread {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::NONE) => {
                submit_inspect_input(state, data_dir, registry, keystore)?;
                return Ok(false);
            }
            (KeyCode::Backspace, _) => {
                let mut s = state.input.clone();
                if !s.is_empty() {
                    let trim_pos = s.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                    s.truncate(trim_pos);
                }
                state.set_input(s);
                return Ok(false);
            }
            // `1`-`5` with no modifier are reserved for tab switching;
            // let them fall through to the outer match rather than entering
            // the digit literally into the chat input.
            (KeyCode::Char(c), KeyModifiers::NONE) if !matches!(c, '1'..='5') => {
                let mut s = state.input.clone();
                s.push(c);
                state.set_input(s);
                return Ok(false);
            }
            (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                let mut s = state.input.clone();
                s.push(c);
                state.set_input(s);
                return Ok(false);
            }
            _ => {}
        }
    }

    match (key.code, key.modifiers) {
        (KeyCode::Tab, KeyModifiers::NONE) => {
            state.set_input(String::new());
            state.inspect_model_editing = false;
            state.inspect_tab = state.inspect_tab.next();
        }
        (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _) => {
            state.set_input(String::new());
            state.inspect_model_editing = false;
            state.inspect_tab = state.inspect_tab.prev();
        }
        (KeyCode::Char(c @ '1'..='5'), _) => {
            state.set_input(String::new());
            state.inspect_model_editing = false;
            let tab = match c {
                '1' => InspectTab::Thread,
                '2' => InspectTab::Tools,
                '3' => InspectTab::Model,
                '4' => InspectTab::Decisions,
                '5' => InspectTab::Memory,
                _ => return Ok(false),
            };
            state.inspect_tab = tab;
        }
        (KeyCode::Char('h'), KeyModifiers::NONE) => {
            state.set_input(String::new());
            state.inspect_model_editing = false;
            state.screen = Screen::Panopticon;
        }
        // `c` opens full-screen chat for the currently inspected agent.
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            if let Some(name) = state.inspect_agent_name.clone() {
                state.set_input(String::new());
                state.chat_agent_name.clear();
                state.chat_agent_name.push_str(&name);
                state.scroll_to_bottom();
                state.screen = Screen::Chat;
            }
        }
        (KeyCode::PageUp, _) => state.scroll_up(ROWS_PER_PAGE),
        (KeyCode::PageDown, _) => state.scroll_down(ROWS_PER_PAGE),
        (KeyCode::Up, KeyModifiers::SHIFT) => state.scroll_up(ROWS_PER_LINE_NUDGE),
        (KeyCode::Down, KeyModifiers::SHIFT) => state.scroll_down(ROWS_PER_LINE_NUDGE),
        (KeyCode::End, _) => state.scroll_to_bottom(),
        _ => {}
    }
    Ok(false)
}

/// Quarantine-screen key bindings.
///
/// - `j`/`k` (and Down/Up arrows) navigate the entry list.
/// - `d` (first press): enter discard-confirm mode. The renderer
///   replaces the footer with a prompt so the operator sees the
///   pending action.
/// - `d` or `y` (second press, confirm mode): delete the quarantine
///   file at the focused entry's path. The file removal is fire-and-
///   forget — the recursive watcher picks up the `inotify`/`FSEvents`
///   deletion event within the 250 ms debounce window and triggers a
///   full reload, which clamps the focus index and refreshes the list.
/// - Any other key while in confirm mode: cancel without deleting.
/// - `Q`: close quarantine and return to panopticon.
/// - `o`: begin convert flow (opens the compose sub-surface).
/// - `Esc`, `Tab`, `q` are handled by the global dispatcher before
///   this function is called and are never received here.
fn handle_key_quarantine(key: event::KeyEvent, state: &mut AppState) -> bool {
    // Confirm-discard mode: the next `d` or `y` executes the delete;
    // any other key cancels. Clear first so the `d` arm below can
    // branch on whether it was the FIRST or SECOND `d` press.
    if state.quarantine_confirm_discard {
        match (key.code, key.modifiers) {
            (KeyCode::Char('d' | 'y'), _) => {
                if let Some(entry) = state.quarantine.entries.get(state.quarantine_focus) {
                    // Fire-and-forget: watcher drives the list refresh.
                    let _ = std::fs::remove_file(&entry.path);
                }
                state.quarantine_confirm_discard = false;
            }
            // `Q` while confirming: cancel the confirm AND leave to the
            // panopticon, so Q always means "close quarantine" regardless
            // of pending confirm state.
            (KeyCode::Char('Q'), _) => {
                state.quarantine_confirm_discard = false;
                state.screen = Screen::Panopticon;
            }
            _ => {
                state.quarantine_confirm_discard = false;
            }
        }
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
            state.quarantine_focus_down();
        }
        (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
            state.quarantine_focus_up();
        }
        (KeyCode::Char('d'), _) if !state.quarantine.entries.is_empty() => {
            state.quarantine_confirm_discard = true;
        }
        (KeyCode::Char('o'), _) => {
            let _ = handle_quarantine_convert(key, state);
        }
        (KeyCode::Char('Q'), _) => {
            state.quarantine_confirm_discard = false;
            state.screen = Screen::Panopticon;
        }
        _ => {}
    }
    false
}

/// Open the quarantine compose surface for the focused entry.
/// Pre-fills the input buffer with the raw body and records the
/// recipient so the submit path knows who to address the new envelope
/// to. `Esc` cancels and returns to the quarantine list; `Tab` cancels
/// and returns to the panopticon (per the global Tab handler). `Enter`
/// submits a fresh operator-signed envelope and returns to the quarantine
/// list.
fn handle_quarantine_convert(_key: event::KeyEvent, state: &mut AppState) -> bool {
    let Some(entry) = state.quarantine.entries.get(state.quarantine_focus) else {
        return false;
    };
    let recipient = entry.recipient.clone();
    let body = entry.raw_body.clone();
    state.quarantine_compose_recipient = recipient;
    state.set_input(body);
    state.quarantine_confirm_discard = false;
    state.screen = Screen::QuarantineCompose;
    false
}

/// Reset compose state without submitting.
fn quarantine_compose_cancel(state: &mut AppState) {
    state.set_input(String::new());
    state.quarantine_compose_recipient.clear();
}

/// Compose-surface key bindings: typing, backspace, Enter to submit.
///
/// `Enter` delivers a new operator-signed envelope to
/// `state.quarantine_compose_recipient`. The original quarantine file
/// is NOT deleted — it stays as the audit record. Esc and Tab are
/// handled by the global dispatcher before reaching here and switch the
/// screen back to `Screen::Quarantine`.
fn handle_key_quarantine_compose(
    key: event::KeyEvent,
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    match (key.code, key.modifiers) {
        (KeyCode::Enter, KeyModifiers::NONE) => {
            submit_quarantine_compose(state, data_dir, registry, keystore)?;
            state.screen = Screen::Quarantine;
        }
        (KeyCode::Backspace, _) => {
            let mut s = state.input.clone();
            if !s.is_empty() {
                let trim_pos = s.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                s.truncate(trim_pos);
            }
            state.set_input(s);
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            let mut s = state.input.clone();
            s.push(c);
            state.set_input(s);
        }
        _ => {}
    }
    Ok(false)
}

/// Submit the compose buffer as a new operator-signed envelope to the
/// quarantine compose recipient. On empty/whitespace input does nothing
/// (identical to `submit_input` for the chat screen). The original
/// quarantine file is intentionally not touched — it stays as the
/// audit record for the blocked message.
fn submit_quarantine_compose(
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let payload = state.input.trim().to_owned();
    if payload.is_empty() {
        return Ok(());
    }
    let recipient = state.quarantine_compose_recipient.clone();
    if recipient.is_empty() {
        return Ok(());
    }
    let dirs = AgentDirs::open(data_dir, &recipient).map_err(|err| {
        TuiError::Submit(crate::submit::SubmitError::Io {
            path: data_dir.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, err.to_string()),
        })
    })?;
    submit_message(&payload, &dirs, registry, keystore).map_err(TuiError::Submit)?;
    quarantine_compose_cancel(state);
    Ok(())
}

fn submit_input(
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    if is_engagement_command(&state.input) {
        return submit_engagement_command(state, data_dir, registry, keystore);
    }
    submit_to_agent(
        state,
        data_dir,
        &state.chat_agent_name.clone(),
        registry,
        keystore,
    )
}

/// True only when the first whitespace-delimited token is exactly
/// `/engagement` — a prefix match would also swallow chat messages like
/// `/engagements …` that merely share the spelling.
fn is_engagement_command(input: &str) -> bool {
    input.split_whitespace().next() == Some("/engagement")
}

/// Handle a `/engagement <verb> …` chat command by signing an operator
/// operation envelope to the estate coordinator — the same payload and
/// transport the `reeve engagement` CLI uses, so the audit trail is
/// identical regardless of front door.
///
/// Grammar (whitespace-separated):
/// - `/engagement open <name> <purpose…>` — root resolves like the CLI:
///   the VCS toplevel of the TUI's working directory.
/// - `/engagement close <name>`
/// - `/engagement reopen <name>`
///
/// Parse and lookup errors are recoverable: they surface as a transient
/// footer notice while the operator's typed command stays in the input
/// buffer for correction — the input is never overwritten with error text
/// (which would also risk sending that text to the lead as chat).
fn submit_engagement_command(
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let input = state.input.trim().to_owned();
    let op = match parse_engagement_command(&input) {
        Ok(op) => op,
        Err(msg) => {
            state.notice = Some(msg);
            return Ok(());
        }
    };

    let agent_registry_path = reeve_runtime::RuntimeLayout::new(data_dir).agent_registry_path();
    let estate_record = AgentRegistry::open(agent_registry_path)
        .ok()
        .and_then(|r| r.lookup(reeve_runtime::ESTATE_AGENT_NAME).cloned());
    let Some(estate_record) = estate_record else {
        state.notice = Some("estate coordinator not registered; is the daemon running?".to_owned());
        return Ok(());
    };

    let payload = serde_json::to_string(&op)
        .map_err(|e| TuiError::Submit(crate::submit::SubmitError::Serialize(e)))?;
    crate::submit::submit_payload_to(
        &payload,
        estate_record.identity_id,
        &estate_record.inbox_dir,
        registry,
        keystore,
    )
    .map_err(TuiError::Submit)?;
    state.notice = Some(format!(
        "engagement operation sent: {} {} — awaiting confirmation",
        op.verb(),
        op.name()
    ));
    state.pending_engagement = Some(crate::state::PendingEngagementOp {
        verb: op.verb(),
        name: op.name().to_owned(),
        sent_at: time::OffsetDateTime::now_utc(),
        deadline: std::time::Instant::now() + Duration::from_secs(10),
    });
    state.set_input(String::new());
    Ok(())
}

// ── Slash-command completion ──────────────────────────────────────────────────

const SLASH_COMMANDS: &[&str] = &["/engagement"];
const ENGAGEMENT_VERBS: &[&str] = &["open", "close", "reopen"];

/// Compute completion candidates for the token currently being typed.
///
/// `open_names` / `closed_names` feed name completion for `close` and
/// `reopen` respectively (`open` takes a fresh name, which cannot be
/// completed). A trailing space means the operator is starting the next
/// token, so every candidate for that position matches. Engagement names
/// containing spaces do not complete cleanly — tokenization is
/// whitespace-based — which degrades to no suggestion, never a wrong one.
fn slash_completion_candidates(
    input: &str,
    open_names: &[String],
    closed_names: &[String],
) -> Vec<String> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return Vec::new();
    }
    let starting_next_token = input.ends_with(char::is_whitespace);
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    // Index of the token being completed and the prefix already typed.
    let (position, prefix) = if starting_next_token {
        (tokens.len(), "")
    } else {
        (tokens.len() - 1, *tokens.last().unwrap_or(&""))
    };
    let matches = |pool: &[&str]| -> Vec<String> {
        pool.iter()
            .filter(|c| c.starts_with(prefix) && **c != prefix)
            .map(|c| (*c).to_owned())
            .collect()
    };
    match position {
        0 => matches(SLASH_COMMANDS),
        1 if tokens[0] == "/engagement" => matches(ENGAGEMENT_VERBS),
        2 if tokens[0] == "/engagement" => {
            let pool: Vec<&str> = match tokens[1] {
                "close" => open_names.iter().map(String::as_str).collect(),
                "reopen" => closed_names.iter().map(String::as_str).collect(),
                _ => return Vec::new(),
            };
            matches(&pool)
        }
        _ => Vec::new(),
    }
}

/// Replace the token being typed with `candidate` (or append it when the
/// input ends at a token boundary), leaving a trailing space so the
/// operator flows straight into the next token.
fn apply_completion(input: &str, candidate: &str) -> String {
    if input.ends_with(char::is_whitespace) || input.is_empty() {
        format!("{input}{candidate} ")
    } else {
        let boundary = input.rfind(char::is_whitespace).map_or(0, |pos| {
            pos + input[pos..].chars().next().map_or(1, char::len_utf8)
        });
        format!("{}{candidate} ", &input[..boundary])
    }
}

/// Longest common prefix of the candidates (for multi-candidate Tab).
fn common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for c in &candidates[1..] {
        while !c.starts_with(&prefix) {
            prefix.pop();
        }
    }
    prefix
}

/// Load engagement names partitioned by state for name completion. Errors
/// degrade to empty pools — completion is a convenience, never a gate.
fn engagement_name_pools(data_dir: &Path) -> (Vec<String>, Vec<String>) {
    let root = reeve_runtime::RuntimeLayout::new(data_dir).engagements_root();
    let Ok(registry) = reeve_runtime::EngagementRegistry::open(root) else {
        return (Vec::new(), Vec::new());
    };
    let Ok(records) = registry.list() else {
        return (Vec::new(), Vec::new());
    };
    let (open, closed): (Vec<_>, Vec<_>) = records
        .into_iter()
        .partition(|r| r.state == reeve_runtime::EngagementState::Open);
    (
        open.into_iter().map(|r| r.name).collect(),
        closed.into_iter().map(|r| r.name).collect(),
    )
}

/// Recompute [`AppState::slash_suggestions`] from the current input.
fn update_slash_suggestions(state: &mut AppState, data_dir: &Path) {
    if state.screen != Screen::Chat || !state.input.trim_start().starts_with('/') {
        state.slash_suggestions.clear();
        return;
    }
    let (open_names, closed_names) = engagement_name_pools(data_dir);
    state.slash_suggestions = slash_completion_candidates(&state.input, &open_names, &closed_names);
}

/// Tab-complete the slash command in the chat input: a unique candidate
/// completes fully; multiple candidates extend to their longest common
/// prefix (the footer hint shows the remaining choices).
fn complete_slash_command(state: &mut AppState, data_dir: &Path) {
    update_slash_suggestions(state, data_dir);
    match state.slash_suggestions.as_slice() {
        [] => {}
        [only] => {
            let completed = apply_completion(&state.input, &only.clone());
            state.set_input(completed);
        }
        many => {
            let prefix = common_prefix(many);
            let last = state
                .input
                .split_whitespace()
                .next_back()
                .unwrap_or("")
                .to_owned();
            if prefix.len() > last.len() && !state.input.ends_with(char::is_whitespace) {
                let boundary = state.input.len() - last.len();
                let completed = format!("{}{prefix}", &state.input[..boundary]);
                state.set_input(completed);
            }
        }
    }
    update_slash_suggestions(state, data_dir);
}

/// Parse the chat slash-command grammar into an [`reeve_runtime::EstateOp`].
fn parse_engagement_command(input: &str) -> Result<reeve_runtime::EstateOp, String> {
    const USAGE: &str = "usage: /engagement open <name> <purpose…> | close <name> | reopen <name>";
    let mut tokens = input.split_whitespace();
    let _command = tokens.next();
    let verb = tokens.next().ok_or(USAGE)?;
    let name = tokens.next().ok_or(USAGE)?.to_owned();
    let rest: Vec<&str> = tokens.collect();
    match verb {
        "open" => {
            if rest.is_empty() {
                return Err(
                    "open requires a purpose: /engagement open <name> <purpose…>".to_owned(),
                );
            }
            let root = std::env::current_dir()
                .map_err(|e| format!("cannot resolve working directory: {e}"))
                .and_then(|cwd| {
                    reeve_runtime::engagement::resolve_vcs_toplevel(&cwd)
                        .map_err(|e| format!("cannot resolve VCS toplevel: {e}"))
                })?;
            Ok(reeve_runtime::EstateOp::OpenEngagement {
                name,
                purpose: rest.join(" "),
                root: Some(root),
            })
        }
        "close" if rest.is_empty() => Ok(reeve_runtime::EstateOp::CloseEngagement { name }),
        "reopen" if rest.is_empty() => Ok(reeve_runtime::EstateOp::ReopenEngagement { name }),
        _ => Err(USAGE.to_owned()),
    }
}

fn submit_inspect_input(
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let Some(agent_name) = state.inspect_agent_name.clone() else {
        return Ok(());
    };
    submit_to_agent(state, data_dir, &agent_name, registry, keystore)
}

fn submit_to_agent(
    state: &mut AppState,
    data_dir: &Path,
    agent_name: &str,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let payload = state.input.trim().to_owned();
    if payload.is_empty() {
        return Ok(());
    }
    let dirs = AgentDirs::open(data_dir, agent_name).map_err(|err| {
        TuiError::Submit(crate::submit::SubmitError::Io {
            path: data_dir.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, err.to_string()),
        })
    })?;
    submit_message(&payload, &dirs, registry, keystore).map_err(TuiError::Submit)?;
    state.set_input(String::new());
    state.scroll_to_bottom();
    Ok(())
}

// ── Model tab threshold editor ────────────────────────────────────────────────

/// Format the current value of a threshold field for display/pre-fill.
/// Returns an empty string for `None` (operator clears to remove the limit).
pub(crate) fn threshold_field_display(t: &Thresholds, field: usize) -> String {
    match field {
        0 => t
            .cost_per_agent
            .map_or(String::new(), |v| format!("{v:.6}")),
        1 => t
            .cost_per_session
            .map_or(String::new(), |v| format!("{v:.6}")),
        2 => t
            .max_concurrent_subordinates
            .map_or(String::new(), |v| v.to_string()),
        3 => t
            .max_task_duration_secs
            .map_or(String::new(), |v| v.to_string()),
        _ => String::new(),
    }
}

/// Parse `state.input` and write the updated threshold to
/// `agents/<name>/profile.toml`. Silently ignores parse/write errors so
/// a bad value leaves the existing file untouched.
fn save_threshold(state: &mut AppState, data_dir: &Path) {
    let Some(agent_name) = state.inspect_agent_name.clone() else {
        return;
    };
    let Ok(dirs) = AgentDirs::open(data_dir, &agent_name) else {
        return;
    };
    let profile_path = dirs.profile_path();
    let raw = state.input.trim().to_owned();

    let mut profile = reeve_runtime::capability::load_capability_profile(&profile_path)
        .unwrap_or_else(|_| CapabilityProfile {
            name: state.persona_name.clone(),
            version: 1,
            enabled_categories: None,
            thresholds: Thresholds::default(),
        });

    // Apply the edit. Empty input explicitly clears the limit (None). A
    // non-empty value must parse to a valid positive number — an invalid or
    // non-positive input leaves the file untouched rather than silently
    // clearing an existing limit.
    let wrote = if raw.is_empty() {
        match state.inspect_model_field {
            0 => {
                profile.thresholds.cost_per_agent = None;
                true
            }
            1 => {
                profile.thresholds.cost_per_session = None;
                true
            }
            2 => {
                profile.thresholds.max_concurrent_subordinates = None;
                true
            }
            3 => {
                profile.thresholds.max_task_duration_secs = None;
                true
            }
            _ => false,
        }
    } else {
        match state.inspect_model_field {
            0 => match raw
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
            {
                Some(v) => {
                    profile.thresholds.cost_per_agent = Some(v);
                    true
                }
                None => false,
            },
            1 => match raw
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
            {
                Some(v) => {
                    profile.thresholds.cost_per_session = Some(v);
                    true
                }
                None => false,
            },
            2 => match raw.parse::<u32>().ok().filter(|v| *v > 0) {
                Some(v) => {
                    profile.thresholds.max_concurrent_subordinates = Some(v);
                    true
                }
                None => false,
            },
            3 => match raw.parse::<u64>().ok().filter(|v| *v > 0) {
                Some(v) => {
                    profile.thresholds.max_task_duration_secs = Some(v);
                    true
                }
                None => false,
            },
            _ => false,
        }
    };

    if wrote && reeve_runtime::capability::write_capability_profile(&profile_path, &profile).is_ok()
    {
        // Mirror the change immediately so the display is consistent before
        // the next reload cycle picks up the new file from disk.
        state.inspect_thresholds = profile.thresholds;
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panopticon::{AgentRow, PanopticonSnapshot};
    use crate::state::AgentStatus;
    use time::{Duration, OffsetDateTime};

    fn key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn engagement_command_matches_exact_first_token_only() {
        assert!(is_engagement_command("/engagement open n purpose"));
        assert!(is_engagement_command("  /engagement close n"));
        assert!(!is_engagement_command("/engagements are fun"));
        assert!(!is_engagement_command("/engagementX open n p"));
        assert!(!is_engagement_command("tell me about /engagement"));
    }

    #[test]
    fn slash_completion_walks_command_verb_and_name_positions() {
        let open = vec!["billing".to_owned(), "docs".to_owned()];
        let closed = vec!["archive-2025".to_owned()];

        assert_eq!(
            slash_completion_candidates("/eng", &open, &closed),
            vec!["/engagement"]
        );
        assert_eq!(
            slash_completion_candidates("/engagement ", &open, &closed),
            vec!["open", "close", "reopen"]
        );
        assert_eq!(
            slash_completion_candidates("/engagement re", &open, &closed),
            vec!["reopen"]
        );
        // close completes against OPEN engagements only.
        assert_eq!(
            slash_completion_candidates("/engagement close ", &open, &closed),
            vec!["billing", "docs"]
        );
        assert_eq!(
            slash_completion_candidates("/engagement close b", &open, &closed),
            vec!["billing"]
        );
        // reopen completes against CLOSED engagements only.
        assert_eq!(
            slash_completion_candidates("/engagement reopen ", &open, &closed),
            vec!["archive-2025"]
        );
        // open takes a fresh name: nothing to complete.
        assert!(slash_completion_candidates("/engagement open ", &open, &closed).is_empty());
        // Past the name position there is nothing to complete.
        assert!(
            slash_completion_candidates("/engagement close billing ", &open, &closed).is_empty()
        );
        // Non-slash input never suggests.
        assert!(slash_completion_candidates("hello /engagement", &open, &closed).is_empty());
        // A token that already equals the only candidate suggests nothing.
        assert!(slash_completion_candidates("/engagement", &open, &closed).is_empty());
    }

    #[test]
    fn apply_completion_replaces_partial_token_and_appends_at_boundary() {
        assert_eq!(apply_completion("/eng", "/engagement"), "/engagement ");
        assert_eq!(
            apply_completion("/engagement re", "reopen"),
            "/engagement reopen "
        );
        assert_eq!(
            apply_completion("/engagement close ", "billing"),
            "/engagement close billing "
        );
    }

    #[test]
    fn tab_in_chat_completes_slash_command_instead_of_toggling_screens() {
        let mut state = AppState::default();
        state.screen = Screen::Chat;
        state.set_input("/eng".to_owned());
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let was_exit = handle_key(
            key(KeyCode::Tab),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit);
        assert_eq!(
            state.screen,
            Screen::Chat,
            "Tab mid-command must not navigate"
        );
        assert_eq!(state.input, "/engagement ");
        assert_eq!(
            state.slash_suggestions,
            vec!["open", "close", "reopen"],
            "post-completion suggestions show the next position"
        );

        // Tab with multiple candidates extends nothing (no common prefix
        // growth from empty) but leaves the input intact.
        let was_exit = handle_key(
            key(KeyCode::Tab),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit);
        assert_eq!(state.input, "/engagement ");
    }

    #[test]
    fn engagement_parse_rejects_bad_grammar_and_accepts_verbs() {
        assert!(parse_engagement_command("/engagement").is_err());
        assert!(parse_engagement_command("/engagement open onlyname").is_err());
        assert!(parse_engagement_command("/engagement close n extra").is_err());
        assert!(parse_engagement_command("/engagement bogus n").is_err());
        assert!(matches!(
            parse_engagement_command("/engagement close billing"),
            Ok(reeve_runtime::EstateOp::CloseEngagement { name }) if name == "billing"
        ));
        assert!(matches!(
            parse_engagement_command("/engagement reopen billing"),
            Ok(reeve_runtime::EstateOp::ReopenEngagement { name }) if name == "billing"
        ));
    }

    fn state_with_agents(count: usize) -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Panopticon;
        state.panopticon = PanopticonSnapshot {
            agents: (0..count)
                .map(|i| AgentRow {
                    name: format!("agent-{i}"),
                    persona_name: None,
                    status: AgentStatus::Idle,
                    is_running: true,
                    is_ghost: false,
                    cost_usd: 0.0,
                    elapsed: Duration::seconds(0),
                    state_changed_at: None,
                })
                .collect(),
            recent_events: Vec::new(),
            queue_counts: crate::panopticon::QueueCounts::default(),
            total_cost_usd: 0.0,
            session_elapsed: Some(Duration::seconds(0)),
            pending_decisions: Vec::new(),
            refusal_count: 0,
        };
        state
    }

    // A1: `j` and Down both move focus down by one row; the helper clamps
    // at the last agent so spamming `j` cannot run off the end.
    #[test]
    fn handle_key_panopticon_j_and_down_move_focus_down_with_clamp() {
        let mut state = state_with_agents(3);
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('j')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 1);
        assert!(!handle_key_panopticon(
            key(KeyCode::Down),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 2);
        // Already at last agent; further presses clamp.
        assert!(!handle_key_panopticon(
            key(KeyCode::Down),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 2);
    }

    // A2: `k` and Up both move focus up by one; saturate at zero.
    #[test]
    fn handle_key_panopticon_k_and_up_move_focus_up_with_clamp() {
        let mut state = state_with_agents(3);
        state.panopticon_focus = 2;
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('k')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 1);
        assert!(!handle_key_panopticon(
            key(KeyCode::Up),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 0);
        // Saturate at 0.
        assert!(!handle_key_panopticon(
            key(KeyCode::Up),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 0);
    }

    // A3: Enter on the panopticon opens the per-agent inspect screen.
    // Per the Phase 7 done-when: chat is no longer reachable from the
    // panopticon — only `reeve attach <name>` opens a chat.
    #[test]
    fn handle_key_panopticon_enter_switches_to_inspect() {
        let mut state = state_with_agents(2);
        assert!(!handle_key_panopticon(
            key(KeyCode::Enter),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.screen, Screen::Inspect);
    }

    // A3b: Enter on a non-zero panopticon row sets `inspect_agent_name`
    // to that row's agent and lands on the Thread tab. The chat target
    // is intentionally unchanged — the operator keeps their typing
    // surface even while drilling into another agent.
    #[test]
    fn handle_key_panopticon_enter_targets_focused_agent_for_inspect() {
        let mut state = state_with_agents(3);
        state.chat_agent_name = "lead".to_owned();
        state.panopticon_focus = 2;
        let expected = state.panopticon.agents[2].name.clone();
        assert!(!handle_key_panopticon(
            key(KeyCode::Enter),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.screen, Screen::Inspect);
        assert_eq!(state.inspect_agent_name.as_deref(), Some(expected.as_str()));
        assert_eq!(state.inspect_tab, InspectTab::Thread);
        // Chat target left alone: inspect is a drill-in, not a switch.
        assert_eq!(state.chat_agent_name, "lead");
    }

    // A3c: Enter snaps scroll to the bottom on inspect entry so the
    // operator sees the most recent activity for the focused agent.
    // The input buffer is *not* cleared (inspect has no input pane;
    // a chat draft survives an inspect detour).
    #[test]
    fn handle_key_panopticon_enter_scrolls_inspect_to_bottom() {
        let mut state = state_with_agents(2);
        state.set_input("draft for chat agent".to_owned());
        state.scroll_up(20);
        state.panopticon_focus = 1;
        assert!(!handle_key_panopticon(
            key(KeyCode::Enter),
            &mut state,
            Path::new("")
        ));
        assert!(
            state.is_at_bottom(),
            "scroll should snap to bottom on inspect entry"
        );
        assert_eq!(
            state.input, "draft for chat agent",
            "input buffer must survive an inspect detour"
        );
    }

    // A3d: Enter with an out-of-range focus index (transient empty
    // snapshot) is defensive: switches screens but leaves the inspect
    // target alone. Showing the previously-inspected agent's data is
    // less confusing than showing a crash or an empty view.
    #[test]
    fn handle_key_panopticon_enter_with_empty_table_leaves_inspect_target() {
        let mut state = state_with_agents(0);
        state.inspect_agent_name = Some("previous-agent".to_owned());
        assert!(!handle_key_panopticon(
            key(KeyCode::Enter),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.screen, Screen::Inspect);
        assert_eq!(state.inspect_agent_name.as_deref(), Some("previous-agent"));
    }

    // A4: unrelated keys are no-ops; focus and screen stay put.
    #[test]
    fn handle_key_panopticon_ignores_unrelated_keys() {
        let mut state = state_with_agents(3);
        state.panopticon_focus = 1;
        let before = state.panopticon_focus;
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('x')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, before);
        assert_eq!(state.screen, Screen::Panopticon);
    }

    // A5: focus-down on an empty agent table is a no-op rather than an
    // overflow. Real users hit this transiently at startup (registry
    // empty, snapshot still rendering).
    #[test]
    fn handle_key_panopticon_focus_down_is_noop_on_empty_table() {
        let mut state = state_with_agents(0);
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('j')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 0);
    }

    // A6: `Q` on the panopticon opens the quarantine review stub. This
    // is the one operator-facing key the queue-strip footer advertises.
    #[test]
    fn handle_key_panopticon_q_opens_quarantine() {
        let mut state = state_with_agents(2);
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('Q')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.screen, Screen::Quarantine);
    }

    // A7: `Q` on the quarantine screen closes back to the panopticon —
    // toggle-on / toggle-off with the same key. Matches the operator's
    // muscle memory for press-the-same-key-to-leave panels.
    #[test]
    fn handle_key_quarantine_q_returns_to_panopticon() {
        let mut state = state_with_agents(2);
        state.screen = Screen::Quarantine;
        assert!(!handle_key_quarantine(key(KeyCode::Char('Q')), &mut state,));
        assert_eq!(state.screen, Screen::Panopticon);
    }

    // A8: unrelated keys on quarantine are no-ops. The global handler
    // already covers Esc and Tab; the stub itself has no other actions
    // until Phase 8 wires approve/release/discard.
    #[test]
    fn handle_key_quarantine_ignores_unrelated_keys() {
        let mut state = state_with_agents(2);
        state.screen = Screen::Quarantine;
        assert!(!handle_key_quarantine(key(KeyCode::Char('x')), &mut state,));
        assert_eq!(state.screen, Screen::Quarantine);
    }

    // ── Session-driven startup screen ────────────────────────────────

    /// `AgentRegistry::open` enforces `0o700` on its parent directory;
    /// `tempdir()` creates with `0o755`. Apply the expected mode before
    /// opening so the registry doesn't reject the directory as
    /// misconfigured.
    #[cfg(unix)]
    fn chmod_700(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Register `name` as a running agent in a fresh on-disk registry at
    /// `path` so `initial_screen_for_session` can find it.
    fn register_running_agent(path: &Path, name: &str) {
        use reeve_runtime::{
            AgentRecord, AgentRegistry, AgentStatus as RuntimeAgentStatus, ValidatedAgentName,
        };
        #[cfg(unix)]
        chmod_700(path.parent().unwrap());
        let mut registry = AgentRegistry::open(path.to_path_buf()).unwrap();
        registry
            .register(AgentRecord {
                name: ValidatedAgentName::new(name).unwrap(),
                identity_id: reeve_types::IdentityId::new().unwrap(),
                inbox_dir: path.parent().unwrap().join(name).join("inbox"),
                persona_name: Some(name.to_owned()),
                spawned_at: OffsetDateTime::now_utc(),
                status: RuntimeAgentStatus::Running,
                stopped_reason: None,
            })
            .unwrap();
    }

    // SA1: no session → panopticon. The "first launch" path: the operator
    // has never quit a chat, so there's nothing to resume.
    #[test]
    fn initial_screen_with_no_session_is_panopticon() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a registry path inside tmp but never write one — open will
        // succeed against the empty directory.
        let registry_path = tmp.path().join("registry.toml");
        let screen = initial_screen_for_session(&Session::default(), &registry_path);
        assert_eq!(screen, Screen::Panopticon);
    }

    // SA2: session names a running agent → that agent's chat. The "resume"
    // path the spec's done-when criterion calls out.
    #[test]
    fn initial_screen_with_running_agent_in_session_is_chat() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("registry.toml");
        register_running_agent(&registry_path, "lead");

        let screen = initial_screen_for_session(
            &Session {
                last_agent: Some("lead".to_owned()),
            },
            &registry_path,
        );
        assert_eq!(screen, Screen::Chat);
    }

    // SA3: session names an agent that is no longer in the registry →
    // panopticon. The registry was rebuilt, or the agent was unregistered;
    // we fall back to the global view rather than landing on a stale chat.
    #[test]
    fn initial_screen_with_unknown_agent_in_session_is_panopticon() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("registry.toml");
        register_running_agent(&registry_path, "lead");

        let screen = initial_screen_for_session(
            &Session {
                last_agent: Some("worker-gone".to_owned()),
            },
            &registry_path,
        );
        assert_eq!(screen, Screen::Panopticon);
    }

    // SA4: session names a stopped agent → panopticon. A stopped agent's
    // chat is not a useful landing place; the spec specifies the fallback.
    #[test]
    fn initial_screen_with_stopped_agent_in_session_is_panopticon() {
        use reeve_runtime::{
            AgentRecord, AgentRegistry, AgentStatus as RuntimeAgentStatus, ValidatedAgentName,
        };
        let tmp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        chmod_700(tmp.path());
        let registry_path = tmp.path().join("registry.toml");
        let mut registry = AgentRegistry::open(registry_path.clone()).unwrap();
        registry
            .register(AgentRecord {
                name: ValidatedAgentName::new("lead").unwrap(),
                identity_id: reeve_types::IdentityId::new().unwrap(),
                inbox_dir: tmp.path().join("lead").join("inbox"),
                persona_name: Some("lead".to_owned()),
                spawned_at: OffsetDateTime::now_utc(),
                status: RuntimeAgentStatus::Stopped,
                stopped_reason: None,
            })
            .unwrap();

        let screen = initial_screen_for_session(
            &Session {
                last_agent: Some("lead".to_owned()),
            },
            &registry_path,
        );
        assert_eq!(screen, Screen::Panopticon);
    }

    // SA5: unreadable registry path → panopticon. Best-effort: if the
    // registry can't be opened for any reason, fall back rather than
    // panic.
    #[test]
    fn initial_screen_with_unreadable_registry_is_panopticon() {
        let screen = initial_screen_for_session(
            &Session {
                last_agent: Some("lead".to_owned()),
            },
            Path::new("/nonexistent/no-such-registry.toml"),
        );
        assert_eq!(screen, Screen::Panopticon);
    }

    // ── reload_state per-agent dispatch ────────────────────────────────

    /// Provision an agent dir at `<data_dir>/agents/<name>/log/` and seed
    /// its conversation.jsonl with one inbound entry whose payload is
    /// `marker`. The marker lets a test distinguish which agent's file
    /// was read.
    fn seed_agent_conversation(data_dir: &Path, name: &str, marker: &str) {
        let log_dir = data_dir.join("agents").join(name).join("log");
        std::fs::create_dir_all(&log_dir).unwrap();
        let conv_path = log_dir.join("conversation.jsonl");
        let line = format!(
            r#"{{"type":"inbound","payload":"{marker}","timestamp_utc":"2026-05-25T20:00:00Z"}}"#
        );
        std::fs::write(&conv_path, format!("{line}\n")).unwrap();
    }

    // RS1: reload_state with chat_agent_name = "lead" reads lead's file;
    // changing chat_agent_name to "worker" and reloading reads worker's
    // file. This is the load-bearing invariant behind per-agent chat:
    // the conversation pane MUST track state.chat_agent_name on every
    // reload, no caching, no staleness.
    #[test]
    fn reload_state_switches_conversation_per_chat_agent_name() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        seed_agent_conversation(data_dir, "lead", "LEAD-MARKER");
        seed_agent_conversation(data_dir, "worker-x", "WORKER-MARKER");
        // Registry path doesn't need to exist for reload_state's
        // panopticon-snapshot read to succeed defensively (it returns an
        // empty snapshot on unreadable registries). The per-agent reads
        // are what we're verifying here.
        let registry_path = tmp.path().join("nonexistent-registry.toml");

        let mut state = AppState::default();
        state.chat_agent_name = "lead".to_owned();
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation.len(), 1);
        assert_eq!(state.conversation[0].text, "LEAD-MARKER");

        state.chat_agent_name = "worker-x".to_owned();
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation.len(), 1);
        assert_eq!(state.conversation[0].text, "WORKER-MARKER");

        // And switching back picks up lead again — no caching anywhere.
        state.chat_agent_name = "lead".to_owned();
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation[0].text, "LEAD-MARKER");
    }

    // RS2: reload_state reads the *inspect* agent's file when the
    // operator is on Screen::Inspect — even if the chat target is a
    // different agent. This is the load-bearing invariant for the
    // per-agent inspect drill-in: the screen MUST show the agent the
    // operator Enter-ed into, not the agent they were chatting with.
    #[test]
    fn reload_state_reads_inspect_agent_on_inspect_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        seed_agent_conversation(data_dir, "lead", "LEAD-MARKER");
        seed_agent_conversation(data_dir, "worker-y", "WORKER-Y-MARKER");
        let registry_path = tmp.path().join("nonexistent-registry.toml");

        let mut state = AppState::default();
        state.chat_agent_name = "lead".to_owned();
        // Operator is inspecting worker-y while their chat target is
        // still lead. reload must read worker-y, not lead.
        state.screen = Screen::Inspect;
        state.inspect_agent_name = Some("worker-y".to_owned());
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation[0].text, "WORKER-Y-MARKER");

        // Tab/Esc back to Chat: reload must now read the chat target
        // again, not whatever inspect was looking at.
        state.screen = Screen::Chat;
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation[0].text, "LEAD-MARKER");
    }

    // RS3: defensive — Screen::Inspect with None inspect_agent_name
    // falls back to the chat target rather than panicking. This branch
    // is unreachable in normal flow (Enter from panopticon always sets
    // the inspect target before switching screens), but the fallback
    // keeps reload_state robust against future call sites that might
    // construct the state differently.
    #[test]
    fn reload_state_inspect_with_none_target_falls_back_to_chat_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        seed_agent_conversation(data_dir, "lead", "LEAD-MARKER");
        let registry_path = tmp.path().join("nonexistent-registry.toml");

        let mut state = AppState::default();
        state.chat_agent_name = "lead".to_owned();
        state.screen = Screen::Inspect;
        state.inspect_agent_name = None;
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation[0].text, "LEAD-MARKER");
    }

    // ── Inspect-screen key bindings ───────────────────────────────────

    fn state_in_inspect() -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Inspect;
        state.inspect_agent_name = Some("worker-x".to_owned());
        state.inspect_tab = InspectTab::Thread;
        state.panopticon_focus = 2; // preserved through the round trip
        state
    }

    // IK1: Tab cycles tabs forward through the five-entry list and
    // wraps from Memory back to Thread. The cycle order matches the
    // wireframe (Thread → Tools → Model → Decisions → Memory → Thread).
    #[test]
    fn handle_key_inspect_tab_cycles_forward_with_wrap() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let order = [
            InspectTab::Tools,
            InspectTab::Model,
            InspectTab::Decisions,
            InspectTab::Memory,
            InspectTab::Thread,
        ];
        for expected in order {
            let quit = handle_key_inspect(
                key(KeyCode::Tab),
                &mut state,
                tmp.path(),
                &registry,
                &keystore,
            )
            .unwrap();
            assert!(!quit);
            assert_eq!(state.inspect_tab, expected);
        }
    }

    // IK2: BackTab (Shift+Tab on most terminals) cycles backward and
    // wraps from Thread back to Memory. Symmetric with IK1.
    #[test]
    fn handle_key_inspect_backtab_cycles_backward_with_wrap() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let order = [
            InspectTab::Memory,
            InspectTab::Decisions,
            InspectTab::Model,
            InspectTab::Tools,
            InspectTab::Thread,
        ];
        for expected in order {
            let quit = handle_key_inspect(
                key(KeyCode::BackTab),
                &mut state,
                tmp.path(),
                &registry,
                &keystore,
            )
            .unwrap();
            assert!(!quit);
            assert_eq!(state.inspect_tab, expected);
        }
    }

    // IK3: 1-5 jump directly to a specific tab in display order when on a
    // non-Thread tab. On the Thread tab, character keys go to the input
    // buffer instead, so numeric shortcuts work from Tools and above.
    #[test]
    fn handle_key_inspect_numeric_keys_jump_to_tab() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();

        // Start on Tools tab (Tab away from Thread) so numeric keys navigate.
        state.inspect_tab = InspectTab::Tools;

        let cases = [
            ('1', InspectTab::Thread),
            ('2', InspectTab::Tools),
            ('3', InspectTab::Model),
            ('4', InspectTab::Decisions),
            ('5', InspectTab::Memory),
        ];
        for (ch, expected) in cases {
            // Move off Thread tab before each numeric press so the key
            // reaches the navigation arm rather than the input buffer.
            if state.inspect_tab == InspectTab::Thread {
                state.inspect_tab = InspectTab::Tools;
            }
            let quit = handle_key_inspect(
                key(KeyCode::Char(ch)),
                &mut state,
                tmp.path(),
                &registry,
                &keystore,
            )
            .unwrap();
            assert!(!quit, "key '{ch}' should not quit");
            assert_eq!(
                state.inspect_tab, expected,
                "key '{ch}' should jump to {expected:?}"
            );
        }
        // '6' is out of range on Tools tab; stays at Tools.
        state.inspect_tab = InspectTab::Tools;
        let quit = handle_key_inspect(
            key(KeyCode::Char('6')),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!quit);
        assert_eq!(state.inspect_tab, InspectTab::Tools);
    }

    // IK4: `h` returns to the panopticon from a non-Thread tab. On the
    // Thread tab, `h` is a regular character that goes to the input buffer;
    // Esc (handled by the global dispatcher) is the back key from Thread.
    #[test]
    fn handle_key_inspect_h_returns_to_panopticon_from_non_thread_tab() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        state.inspect_tab = InspectTab::Tools; // not Thread
        assert_eq!(state.panopticon_focus, 2);
        let quit = handle_key_inspect(
            key(KeyCode::Char('h')),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!quit);
        assert_eq!(state.screen, Screen::Panopticon);
        assert_eq!(state.panopticon_focus, 2);
    }

    // IK5: Esc behaves the same way as `h` — back to panopticon, focus
    // preserved. Two key bindings for the same action match the rest
    // of the TUI (h is vim canon, Esc is general muscle memory).
    // Routed through the global handle_key dispatcher since Esc is a
    // global key.
    #[test]
    fn handle_key_esc_from_inspect_returns_to_panopticon() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let was_exit = handle_key(
            key(KeyCode::Esc),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit, "Esc from inspect must not exit the TUI");
        assert_eq!(state.screen, Screen::Panopticon);
        assert_eq!(state.panopticon_focus, 2);
    }

    // A malformed /engagement command surfaces its error as a footer
    // notice and leaves the operator's typed text in the input buffer —
    // stuffing the error into the input both destroys the command the
    // operator was editing and risks sending the error text to the lead
    // as chat on the next Enter.
    #[test]
    fn engagement_parse_error_sets_notice_and_preserves_input() {
        let mut state = AppState::default();
        state.screen = Screen::Chat;
        state.set_input("/engagement open onlyname".to_owned());
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let was_exit = handle_key(
            key(KeyCode::Enter),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit);
        assert_eq!(
            state.input, "/engagement open onlyname",
            "the typed command must survive a parse error"
        );
        let notice = state
            .notice
            .as_deref()
            .expect("parse error must set notice");
        assert!(notice.contains("purpose"), "notice: {notice}");

        // The next keystroke dismisses the notice.
        let _ = handle_key(
            key(KeyCode::Char('x')),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(state.notice.is_none(), "any keystroke clears the notice");
    }

    // A pending engagement op resolves from the audit tail: refusals
    // surface their reason, and resolution survives an intervening
    // keystroke that cleared the "sent" notice.
    #[test]
    fn pending_engagement_resolves_from_audit_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_dir = tmp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).unwrap();
        std::fs::write(
            audit_dir.join("log.jsonl"),
            concat!(
                r#"{"kind":"engagement.op_refused","name":"billing","reason":"name_taken","at":"2030-01-01T00:00:10Z"}"#,
                "\n",
            ),
        )
        .unwrap();

        let mut state = AppState::default();
        state.notice = None; // "sent" notice already dismissed by a keystroke
        state.pending_engagement = Some(crate::state::PendingEngagementOp {
            verb: "open-engagement",
            name: "billing".to_owned(),
            sent_at: OffsetDateTime::from_unix_timestamp(1_893_456_000).unwrap(), // 2030-01-01T00:00:00Z
            deadline: std::time::Instant::now() + std::time::Duration::from_mins(1),
        });

        resolve_pending_engagement(&mut state, tmp.path());

        let notice = state.notice.as_deref().expect("refusal must set a notice");
        assert!(notice.contains("refused"), "notice: {notice}");
        assert!(notice.contains("name_taken"), "notice: {notice}");
        assert!(state.pending_engagement.is_none());
    }

    #[test]
    fn pending_engagement_times_out_with_a_pointer_to_the_audit_log() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = AppState::default();
        state.pending_engagement = Some(crate::state::PendingEngagementOp {
            verb: "close-engagement",
            name: "billing".to_owned(),
            sent_at: OffsetDateTime::now_utc(),
            deadline: std::time::Instant::now(), // already expired
        });

        resolve_pending_engagement(&mut state, tmp.path());

        let notice = state.notice.as_deref().expect("timeout must set a notice");
        assert!(notice.contains("no confirmation"), "notice: {notice}");
        assert!(state.pending_engagement.is_none());
    }

    // IK6: Tab on the inspect screen is screen-local — it cycles tabs,
    // not screens. Without this, the global Tab handler would toggle
    // Chat ↔ Panopticon and drop the operator out of inspect on every
    // Tab press.
    #[test]
    fn handle_key_tab_on_inspect_stays_in_inspect_and_cycles_tab() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let was_exit = handle_key(
            key(KeyCode::Tab),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit);
        assert_eq!(state.screen, Screen::Inspect, "Tab must not leave inspect");
        assert_eq!(state.inspect_tab, InspectTab::Tools);
    }

    /// Construct registry + keystore for the few inspect tests that
    /// route through the global `handle_key`. Neither is exercised by
    /// inspect dispatch (the global handler returns early on Esc/Tab
    /// without touching the chat helper) but `handle_key` requires the
    /// references.
    ///
    /// The tempdir's lifetime is leaked into the registry by tying it
    /// to a `Box::leak`'d path; the test process owns the directory
    /// until exit.
    fn test_registry_and_keystore() -> (
        IdentityRegistry,
        reeve_runtime::keychain::memory::MemoryKeyStore,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        chmod_700(tmp.path());
        let path = tmp.keep();
        let registry = IdentityRegistry::open(path).unwrap();
        (
            registry,
            reeve_runtime::keychain::memory::MemoryKeyStore::new(),
        )
    }

    // ── Quarantine key bindings ───────────────────────────────────────

    fn quarantine_state(entries: Vec<crate::quarantine_view::QuarantineEntry>) -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Quarantine;
        state.quarantine = crate::quarantine_view::QuarantineSnapshot {
            entries,
            truncated: false,
        };
        state
    }

    fn fake_entry(name: &str, reason: &str) -> crate::quarantine_view::QuarantineEntry {
        crate::quarantine_view::QuarantineEntry {
            path: std::path::PathBuf::from(format!("/x/{name}.{reason}")),
            recipient: name.to_owned(),
            arrived: None,
            reason: reason.to_owned(),
            meta: crate::quarantine_view::EnvelopeMeta::ParseFailure {
                filename: format!("{name}.{reason}"),
            },
            raw_body: format!("body for {name}"),
            body_lossy: false,
        }
    }

    // QK1: j/k and arrow keys navigate with clamping — no underflow
    // or overflow.
    #[test]
    fn quarantine_j_k_navigate_with_clamp() {
        let mut state = quarantine_state(vec![
            fake_entry("a", "replay"),
            fake_entry("b", "clock_skew"),
        ]);
        assert!(!handle_key_quarantine(key(KeyCode::Char('j')), &mut state));
        assert_eq!(state.quarantine_focus, 1);
        assert!(!handle_key_quarantine(key(KeyCode::Char('j')), &mut state));
        assert_eq!(state.quarantine_focus, 1, "clamps at last");
        assert!(!handle_key_quarantine(key(KeyCode::Char('k')), &mut state));
        assert_eq!(state.quarantine_focus, 0);
        assert!(!handle_key_quarantine(key(KeyCode::Char('k')), &mut state));
        assert_eq!(state.quarantine_focus, 0, "clamps at zero");
    }

    // QK2: first `d` sets confirm_discard; second `d` removes the
    // quarantine file. Since we use a fake in-memory path, the remove
    // will Err, which is ignored — what we test is that the state
    // transitions are correct (confirm cleared, focus unchanged).
    #[test]
    fn quarantine_d_first_sets_confirm_second_clears() {
        let mut state = quarantine_state(vec![fake_entry("lead", "signature_invalid")]);
        // First d → enter confirm mode.
        assert!(!handle_key_quarantine(key(KeyCode::Char('d')), &mut state));
        assert!(
            state.quarantine_confirm_discard,
            "confirm not set after first d"
        );
        // Second d → attempt delete (ignored) and clear confirm.
        assert!(!handle_key_quarantine(key(KeyCode::Char('d')), &mut state));
        assert!(
            !state.quarantine_confirm_discard,
            "confirm not cleared after second d"
        );
    }

    // QK3: any key other than d/y cancels confirm without deleting.
    #[test]
    fn quarantine_non_confirm_key_cancels_discard() {
        let mut state = quarantine_state(vec![fake_entry("lead", "replay")]);
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        assert!(state.quarantine_confirm_discard);
        handle_key_quarantine(key(KeyCode::Char('k')), &mut state);
        assert!(!state.quarantine_confirm_discard, "k should cancel confirm");
        assert_eq!(state.quarantine_focus, 0, "focus unchanged on cancel");
    }

    // QK4: `d` on an empty list is a no-op — confirm does not arm
    // when there is nothing to discard.
    #[test]
    fn quarantine_d_on_empty_list_is_noop() {
        let mut state = quarantine_state(Vec::new());
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        assert!(!state.quarantine_confirm_discard);
    }

    // QK5: `Q` returns to the panopticon and clears any pending confirm.
    #[test]
    fn quarantine_q_returns_to_panopticon_and_clears_confirm() {
        let mut state = quarantine_state(vec![fake_entry("lead", "replay")]);
        state.quarantine_confirm_discard = true;
        handle_key_quarantine(key(KeyCode::Char('Q')), &mut state);
        assert_eq!(state.screen, Screen::Panopticon);
        assert!(!state.quarantine_confirm_discard);
    }

    // QK6: `o` on a focused entry opens the compose surface and
    // pre-fills the input buffer with the entry's raw body.
    #[test]
    fn quarantine_o_opens_compose_with_prefilled_body() {
        let mut state = quarantine_state(vec![fake_entry("lead", "signature_invalid")]);
        handle_key_quarantine(key(KeyCode::Char('o')), &mut state);
        assert_eq!(state.screen, Screen::QuarantineCompose);
        assert_eq!(
            state.quarantine_compose_recipient, "lead",
            "recipient not set"
        );
        assert_eq!(
            state.input, "body for lead",
            "input not pre-filled with entry body"
        );
    }

    // QK7: Esc from the compose surface cancels without sending —
    // input is cleared and we return to Screen::Quarantine.
    #[test]
    fn quarantine_compose_esc_cancels() {
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let mut state = quarantine_state(vec![fake_entry("lead", "replay")]);
        state.screen = Screen::QuarantineCompose;
        state.quarantine_compose_recipient = "lead".to_owned();
        state.set_input("some draft text".to_owned());

        let was_exit = handle_key(
            key(KeyCode::Esc),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit);
        assert_eq!(state.screen, Screen::Quarantine);
        assert!(state.input.is_empty(), "input must be cleared on cancel");
        assert!(state.quarantine_compose_recipient.is_empty());
    }

    // QK8: discard-then-reload path — after a file is deleted the
    // reload should clamp focus to the new (shorter) list. The
    // reload_state tests cover this via RS3 (the focus-clamp in
    // reload_state). This test verifies the full round-trip through
    // the disk fixture: seed a real file, run handle_key_quarantine
    // twice (d, d), confirm file is gone.
    #[test]
    fn quarantine_discard_deletes_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let qdir = tmp
            .path()
            .join("agents")
            .join("lead")
            .join("inbox")
            .join("quarantine");
        std::fs::create_dir_all(&qdir).unwrap();
        let file_path = qdir.join("abc.signature_invalid");
        std::fs::write(&file_path, b"not json").unwrap();

        let mut state = AppState::default();
        state.screen = Screen::Quarantine;
        state.quarantine = crate::quarantine_view::read_snapshot(tmp.path());
        state.quarantine_focus = 0;
        assert_eq!(state.quarantine.entries.len(), 1, "seeded one entry");

        // First d: arm confirm.
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        assert!(state.quarantine_confirm_discard);
        // Second d: delete.
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        assert!(!state.quarantine_confirm_discard);
        assert!(!file_path.exists(), "quarantine file should be deleted");
    }

    // QK9: replay-ledger preservation — the TUI discard must not touch
    // `replay-ledger.jsonl`. The watcher's verify pipeline writes the
    // replay entry before making the quarantine decision, so the entry
    // is already durable when the file lands in quarantine/. Discarding
    // only removes the quarantine file; the ledger entry stays, blocking
    // any future re-delivery of the same (sender_id, message_id, nonce).
    //
    // This is the Phase 8 done-when integration-test requirement:
    // "The replay ledger entry for a discarded message prevents re-delivery
    // if the same message_id is submitted again."
    //
    // The full replay-pipeline assertion is in
    // `reeve-runtime::verify::tests::replay_second_call_yields_quarantine`.
    // This test covers the TUI side: discard must scope its delete to the
    // single quarantine file, leaving the ledger untouched.
    #[test]
    fn quarantine_discard_preserves_replay_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let qdir = tmp
            .path()
            .join("agents")
            .join("lead")
            .join("inbox")
            .join("quarantine");
        std::fs::create_dir_all(&qdir).unwrap();
        let qfile = qdir.join("abc.signature_invalid");
        std::fs::write(&qfile, b"not json").unwrap();

        // Simulate the runtime having written a replay-ledger entry when
        // the envelope first arrived.
        let ledger_path = tmp.path().join("replay-ledger.jsonl");
        let ledger_entry = r#"{"sender_id":"00000000-0000-7000-8000-000000000001","message_id":"00000000-0000-7000-8000-000000000002","nonce":"AAAAAAAAAAAAAAAAAAAAAA==","observed_at":"2026-01-01T00:00:00Z"}"#;
        std::fs::write(&ledger_path, format!("{ledger_entry}\n")).unwrap();

        let mut state = AppState::default();
        state.screen = Screen::Quarantine;
        state.quarantine = crate::quarantine_view::read_snapshot(tmp.path());
        state.quarantine_focus = 0;

        // Discard the quarantine file.
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);

        assert!(!qfile.exists(), "quarantine file must be deleted");
        assert!(ledger_path.exists(), "replay ledger must NOT be deleted");
        let ledger_after = std::fs::read_to_string(&ledger_path).unwrap();
        assert_eq!(
            ledger_after,
            format!("{ledger_entry}\n"),
            "replay ledger content must be unchanged after discard"
        );
    }

    // ── Model tab editor tests ─────────────────────────────────────────────

    fn state_in_model_tab() -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Inspect;
        state.inspect_tab = InspectTab::Model;
        state.inspect_agent_name = Some("lead".to_owned());
        state
    }

    // MT1: j/k navigate between threshold fields and clamp at bounds.
    #[test]
    fn model_tab_jk_navigate_fields() {
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let mut state = state_in_model_tab();
        assert_eq!(state.inspect_model_field, 0);

        handle_key_inspect(
            key(KeyCode::Char('j')),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert_eq!(state.inspect_model_field, 1);

        handle_key_inspect(
            key(KeyCode::Char('k')),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert_eq!(state.inspect_model_field, 0);

        // Clamp at 0.
        handle_key_inspect(
            key(KeyCode::Char('k')),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert_eq!(state.inspect_model_field, 0);
    }

    // MT2: Enter starts editing and pre-fills the input buffer with the
    // current value; Esc cancels without changing state.
    #[test]
    fn model_tab_enter_starts_editing_esc_cancels() {
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let mut state = state_in_model_tab();
        state.inspect_thresholds.cost_per_agent = Some(0.05);

        handle_key_inspect(
            key(KeyCode::Enter),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(state.inspect_model_editing);
        assert_eq!(state.input, "0.050000");

        handle_key_inspect(
            key(KeyCode::Esc),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!state.inspect_model_editing);
        assert!(state.input.is_empty());
        assert_eq!(state.inspect_thresholds.cost_per_agent, Some(0.05));
    }

    // MT3: invalid input (zero, negative, non-numeric) does not write the
    // profile — the existing threshold is preserved.
    #[test]
    fn model_tab_invalid_input_leaves_file_untouched() {
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();

        let dirs = AgentDirs::provision(data_dir, "lead").unwrap();
        let profile = CapabilityProfile {
            name: "lead".to_owned(),
            version: 1,
            enabled_categories: None,
            thresholds: Thresholds {
                cost_per_agent: Some(0.10),
                ..Default::default()
            },
        };
        reeve_runtime::capability::write_capability_profile(&dirs.profile_path(), &profile)
            .unwrap();

        let mut state = state_in_model_tab();
        state.inspect_agent_name = Some("lead".to_owned());
        state.inspect_model_field = 0; // cost_per_agent

        for bad_input in &["0", "-1", "abc", "NaN", "inf", "0.0"] {
            state.inspect_model_editing = true;
            state.set_input(bad_input.to_string());
            handle_key_inspect(
                key(KeyCode::Enter),
                &mut state,
                data_dir,
                &registry,
                &keystore,
            )
            .unwrap();
            let reloaded =
                reeve_runtime::capability::load_capability_profile(&dirs.profile_path()).unwrap();
            assert_eq!(
                reloaded.thresholds.cost_per_agent,
                Some(0.10),
                "input {bad_input:?} must not overwrite existing threshold"
            );
        }
    }

    // MT4: empty input clears (sets to None) and writes the file.
    #[test]
    fn model_tab_empty_input_clears_threshold() {
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();

        let dirs = AgentDirs::provision(data_dir, "lead").unwrap();
        let profile = CapabilityProfile {
            name: "lead".to_owned(),
            version: 1,
            enabled_categories: None,
            thresholds: Thresholds {
                cost_per_agent: Some(0.10),
                ..Default::default()
            },
        };
        reeve_runtime::capability::write_capability_profile(&dirs.profile_path(), &profile)
            .unwrap();

        let mut state = state_in_model_tab();
        state.inspect_agent_name = Some("lead".to_owned());
        state.inspect_model_field = 0;
        state.inspect_model_editing = true;
        state.set_input(String::new());

        handle_key_inspect(
            key(KeyCode::Enter),
            &mut state,
            data_dir,
            &registry,
            &keystore,
        )
        .unwrap();

        let reloaded =
            reeve_runtime::capability::load_capability_profile(&dirs.profile_path()).unwrap();
        assert_eq!(
            reloaded.thresholds.cost_per_agent, None,
            "empty input must clear the limit"
        );
        assert_eq!(state.inspect_thresholds.cost_per_agent, None);
    }
}
