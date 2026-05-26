//! Per-agent inspect screen — Phase 7.
//!
//! Read-only drill-in from the panopticon. Title bar with the agent's
//! name, persona, model, status, and cost; five tabs across the top
//! (Thread / Tools / Model / Decisions / Memory). Only the Thread tab is
//! populated in this ladder — the other four show a "not yet available"
//! placeholder so the operator can see the layout but doesn't mistake
//! the empty body for a render bug. `Tab`/`Shift+Tab` and `1-5` cycle
//! tabs; `h`/`Esc` returns to the panopticon (the keymap is in
//! `crate::app::handle_key_inspect`).
//!
//! Aesthetic continuity with [`crate::ui`], [`crate::ui_panopticon`], and
//! [`crate::ui_quarantine`] is intentional — same `reeve · …` title
//! prefix, same `NO_COLOR` fallback contract, same footer-hint shape.
//! When Tools/Model/Decisions/Memory ship in later ladders they drop
//! into this module and inherit the chrome.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::state::{AgentStatus, AppState, InspectTab};
use crate::ui_common::no_color;

/// Render the inspect screen into `frame`.
pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let width = area.width;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Length(1), // tab bar
            Constraint::Length(1), // separator
            Constraint::Min(1),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    frame.render_widget(Paragraph::new(build_title_bar(state)), chunks[0]);
    frame.render_widget(Paragraph::new(build_tab_bar(state.inspect_tab)), chunks[1]);
    frame.render_widget(Paragraph::new(build_separator_line(width)), chunks[2]);
    frame.render_widget(
        Paragraph::new(build_body(state, chunks[3].width)),
        chunks[3],
    );
    frame.render_widget(Paragraph::new(build_footer()), chunks[4]);
}

/// Title bar: `reeve · {name} ({persona}) ─── {model} · {sigil} {status} · ${cost}`.
///
/// The agent name shown is `inspect_agent_name` when set; if absent
/// (defensive — should not happen because inspect is only reachable from
/// `Enter` on a panopticon row which always sets it), falls back to
/// `chat_agent_name`. The persona/model/status/cost fields are whatever
/// [`crate::app::reload_state`] populated for the active agent.
fn build_title_bar(state: &AppState) -> Line<'static> {
    let name = state
        .inspect_agent_name
        .as_deref()
        .unwrap_or(&state.chat_agent_name);
    let sigil = status_sigil(&state.status);
    let status = status_text(&state.status);

    let prefix_style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::Blue)
    };
    let prefix = Span::styled("reeve \u{00B7} ".to_owned(), prefix_style);

    let agent_part = Span::raw(format!(
        "{} ({}) \u{2500}\u{2500}\u{2500} {} \u{00B7} ",
        name, state.persona_name, state.model_id,
    ));

    let sigil_style = status_color(&state.status)
        .map(|c| Style::default().fg(c))
        .unwrap_or_default();
    let sigil_span = Span::styled(sigil.to_owned(), sigil_style);

    let status_span = Span::raw(format!(" {status} \u{00B7} ${:.2}", state.cost_usd));

    Line::from(vec![prefix, agent_part, sigil_span, status_span])
}

/// Tab bar — five tabs in fixed order, active tab styled bold (and
/// reverse-video when colour is on). Inactive tabs use the dim modifier
/// so the active tab is the visual anchor.
fn build_tab_bar(active: InspectTab) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, tab) in InspectTab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  ".to_owned()));
        }
        let label = tab.label().to_owned();
        let style = if *tab == active {
            if no_color() {
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Cyan)
            }
        } else if no_color() {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

/// A horizontal rule the full width of the inspect area, matching the
/// other screens' separator shape.
fn build_separator_line(width: u16) -> Line<'static> {
    Line::from("\u{2500}".repeat(usize::from(width)))
}

/// Body content for the active tab. The Thread tab renders the
/// conversation journal; the other four render a "not yet available"
/// placeholder so the layout is the same shape regardless of which tab
/// the operator landed on.
fn build_body(state: &AppState, width: u16) -> Vec<Line<'static>> {
    match state.inspect_tab {
        InspectTab::Thread => build_thread_body(state, width),
        InspectTab::Tools => stub_body("Tools"),
        InspectTab::Model => stub_body("Model"),
        InspectTab::Decisions => stub_body("Decisions"),
        InspectTab::Memory => stub_body("Memory"),
    }
}

/// Thread tab body: conversation entries with speaker label and timestamp.
/// Matches the chat-screen rendering except no input pane is shown — the
/// inspect screen is read-only.
fn build_thread_body(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in &state.conversation {
        let label = entry.speaker_label(&state.persona_name, state.operator_id);
        let ts = crate::ui_common::format_time_hhmm_opt(entry.timestamp);
        let speaker_text = if ts.is_empty() {
            label
        } else {
            format!("{label} \u{00B7} {ts}")
        };
        lines.push(Line::from(Span::styled(
            speaker_text,
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for line in entry.text.lines() {
            for visual_row in wrap_body_line(line, width) {
                lines.push(Line::from(visual_row));
            }
        }
        lines.push(Line::from(""));
    }
    if lines.is_empty() {
        let dim = if no_color() {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  no conversation entries yet".to_owned(),
            dim,
        )));
    }
    lines
}

/// Two-space hang indent under each speaker line, then word-wrap at the
/// inspect body's content width. Mirrors the chat screen's wrap shape so
/// the two views read the same.
fn wrap_body_line(text: &str, width: u16) -> Vec<String> {
    const BODY_INDENT: &str = "  ";
    let content_width = usize::from(width).saturating_sub(BODY_INDENT.len()).max(1);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= content_width {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(format!("{BODY_INDENT}{current}"));
            current.clear();
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(format!("{BODY_INDENT}{current}"));
    }
    if out.is_empty() {
        out.push(BODY_INDENT.to_owned());
    }
    out
}

/// Placeholder body for stub tabs. The operator should know what they're
/// looking at and that the absence of data is by design, not a bug.
fn stub_body(tab_name: &str) -> Vec<Line<'static>> {
    let dim = if no_color() {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {tab_name} — not yet available."),
            dim,
        )),
        Line::from(Span::styled(
            "  This tab's content lands in a later ladder. The Thread".to_owned(),
            dim,
        )),
        Line::from(Span::styled(
            "  tab (key `1`) has the per-agent conversation history.".to_owned(),
            dim,
        )),
    ]
}

/// Footer: navigation keys. `h` is the vim-flavoured back; `Esc` does the
/// same thing for muscle memory consistency with the rest of the TUI.
fn build_footer() -> Line<'static> {
    Line::from(
        "h back \u{00B7} Tab next \u{00B7} Shift+Tab prev \u{00B7} 1-5 jump \u{00B7} q quit"
            .to_owned(),
    )
}

// ── Status sigil / colour / text ──────────────────────────────────────────────
//
// Duplicated from `crate::ui` rather than imported. The chat module's
// helpers are crate-private, and lifting them into `ui_common` would
// pull `AgentStatus` rendering across three screens with no observable
// shared behaviour beyond the symbol table. Keeping the three small
// match arms local means each screen can diverge (e.g., add a sigil
// variant) without touching the others.

fn status_sigil(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "\u{25CB}",    // ○
        AgentStatus::Working => "\u{25CF}", // ●
        AgentStatus::Crashed => "!",
        AgentStatus::Unknown => "?",
    }
}

fn status_color(status: &AgentStatus) -> Option<Color> {
    if no_color() {
        return None;
    }
    match status {
        AgentStatus::Working => Some(Color::Yellow),
        AgentStatus::Crashed => Some(Color::Red),
        AgentStatus::Idle | AgentStatus::Unknown => None,
    }
}

fn status_text(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Crashed => "crashed",
        AgentStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AgentStatus, AppState, ConversationEntry, EntryKind, Screen};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn state_for_inspect(name: &str) -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Inspect;
        state.inspect_agent_name = Some(name.to_owned());
        state.inspect_tab = InspectTab::Thread;
        state.persona_name = "worker".to_owned();
        state.model_id = "claude-opus-4-7".to_owned();
        state.status = AgentStatus::Idle;
        state.cost_usd = 0.42;
        state.conversation = vec![
            ConversationEntry {
                kind: EntryKind::System,
                text: "agent started".to_owned(),
                timestamp: None,
                sender_id: None,
            },
            ConversationEntry {
                kind: EntryKind::Outbound,
                text: "Acknowledged. Reviewing your request now.".to_owned(),
                timestamp: None,
                sender_id: None,
            },
        ];
        state
    }

    fn render(state: &AppState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, state)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // I1: title bar includes the inspect agent's name, persona, model,
    // and cost — the four fields the wireframe calls out. The status
    // sigil and text are also present so the operator can see at a
    // glance whether the agent is still doing work.
    #[test]
    fn title_bar_includes_agent_metadata() {
        let state = state_for_inspect("worker-2e28aff5");
        let rendered = render(&state, 100, 10);
        assert!(
            rendered.contains("worker-2e28aff5"),
            "name missing: {rendered}"
        );
        assert!(rendered.contains("(worker)"), "persona missing: {rendered}");
        assert!(
            rendered.contains("claude-opus-4-7"),
            "model missing: {rendered}"
        );
        assert!(rendered.contains("$0.42"), "cost missing: {rendered}");
        assert!(rendered.contains("idle"), "status text missing: {rendered}");
    }

    // I2: tab bar shows all five labels in display order.
    #[test]
    fn tab_bar_shows_all_five_tabs() {
        let state = state_for_inspect("worker-x");
        let rendered = render(&state, 100, 10);
        for label in ["THREAD", "TOOLS", "MODEL", "DECISIONS", "MEMORY"] {
            assert!(rendered.contains(label), "tab {label} missing: {rendered}");
        }
    }

    // I3: Thread tab renders conversation entries from state.conversation.
    // This is the load-bearing read path for the inspect screen.
    #[test]
    fn thread_tab_renders_conversation_entries() {
        let state = state_for_inspect("worker-x");
        let rendered = render(&state, 80, 24);
        assert!(
            rendered.contains("agent started"),
            "system entry missing: {rendered}"
        );
        assert!(
            rendered.contains("Reviewing your request"),
            "outbound entry missing: {rendered}"
        );
    }

    // I4: switching to a stub tab renders the placeholder body — no
    // panic, no empty pane, no "Tools" rendering as if it were the
    // Thread tab. This is the spec's "tab cycling does not panic or
    // corrupt render state" criterion.
    #[test]
    fn tools_tab_renders_stub_placeholder() {
        let mut state = state_for_inspect("worker-x");
        state.inspect_tab = InspectTab::Tools;
        let rendered = render(&state, 80, 24);
        assert!(
            rendered.contains("not yet available"),
            "stub placeholder missing: {rendered}"
        );
        assert!(
            rendered.contains("Tools"),
            "stub tab name missing: {rendered}"
        );
        // Conversation entries should NOT leak through — Tools is a
        // separate tab body.
        assert!(
            !rendered.contains("Reviewing your request"),
            "Thread content leaked into Tools tab: {rendered}"
        );
    }

    // I5: all four stub tabs render their placeholder without panicking
    // at 80x24. Catches enum-arm divergence early.
    #[test]
    fn all_stub_tabs_render_without_panic() {
        for tab in [
            InspectTab::Tools,
            InspectTab::Model,
            InspectTab::Decisions,
            InspectTab::Memory,
        ] {
            let mut state = state_for_inspect("worker-x");
            state.inspect_tab = tab;
            let _ = render(&state, 80, 24);
        }
    }

    // I6: 80x24 NO_COLOR smoke. Same shape as the panopticon and
    // quarantine smoke — proves the inspect screen is readable on the
    // smallest terminal we support.
    #[test]
    fn draw_renders_at_80x24() {
        // No-color is opt-in here so the test is deterministic regardless
        // of the developer's environment.
        std::env::set_var("NO_COLOR", "1");
        let state = state_for_inspect("worker-x");
        let rendered = render(&state, 80, 24);
        std::env::remove_var("NO_COLOR");
        assert!(rendered.contains("THREAD"), "tab bar missing");
        assert!(rendered.contains("h back"), "footer missing");
        assert!(rendered.contains("worker-x"), "title bar missing");
    }
}
