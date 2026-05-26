//! Application state for the Reeve TUI.
//!
//! `AppState` holds everything the renderer needs: conversation history,
//! agent status, cumulative cost, identity context, and the current input
//! buffer. All fields are plain Rust values — no I/O happens here. Reads
//! from disk are in `crate::reader`; the watcher loop calls back into
//! those readers and replaces the state.

use reeve_types::IdentityId;
use time::OffsetDateTime;

use crate::panopticon::PanopticonSnapshot;

// ── EntryKind ─────────────────────────────────────────────────────────────────

/// Directionality and role of a conversation entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// A message received from an external sender (inbound from the inbox).
    Inbound,
    /// A message produced by the agent and sent to a recipient.
    Outbound,
    /// A system-level annotation: startup, shutdown, error, model call, etc.
    System,
}

// ── ConversationEntry ─────────────────────────────────────────────────────────

/// A single display-ready entry in the lead agent's conversation history.
///
/// Parsed from `agents/lead/log/conversation.jsonl` by `crate::reader`. The
/// renderer uses `kind` and `text`; `timestamp` is shown in the speaker tag
/// when present. `sender_id` is set for inbound entries written by the
/// post-attribution runtime and `None` for legacy entries (or for variants
/// where the sender concept does not apply).
#[derive(Debug, Clone)]
pub struct ConversationEntry {
    pub kind: EntryKind,
    pub text: String,
    pub timestamp: Option<OffsetDateTime>,
    pub sender_id: Option<IdentityId>,
}

impl ConversationEntry {
    /// Resolve the display label for this entry given the lead persona name
    /// and the operator's identity id. Routing is:
    /// - `Outbound` → the persona name (the lead is talking)
    /// - `System` → `"system"`
    /// - `Inbound` with `sender_id == operator_id` → `"you"` (operator typed)
    /// - `Inbound` with another `sender_id` → that sender's id, truncated to
    ///   the leading UUID segment so the tag stays scannable
    /// - `Inbound` with no `sender_id` (legacy journals) → `"unknown"`
    pub fn speaker_label(&self, persona_name: &str, operator_id: Option<IdentityId>) -> String {
        match self.kind {
            EntryKind::Outbound => persona_name.to_owned(),
            EntryKind::System => "system".to_owned(),
            EntryKind::Inbound => match self.sender_id {
                Some(id) if Some(id) == operator_id => "you".to_owned(),
                Some(id) => short_id(id),
                None => "unknown".to_owned(),
            },
        }
    }
}

/// First UUID segment (8 hex chars) for a tag-friendly sender label. Full ids
/// are long; this keeps the speaker bar scannable while still being unique
/// enough to disambiguate within a single conversation.
fn short_id(id: IdentityId) -> String {
    let s = id.to_string();
    s.split('-').next().unwrap_or(&s).to_owned()
}

// ── AgentStatus ───────────────────────────────────────────────────────────────

/// Observed runtime status of the lead agent.
///
/// Parsed from `agents/lead/status` by `crate::reader::read_status`. The TUI
/// maps this to a status sigil and color in the title bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Agent is idle, waiting for input.
    Idle,
    /// Agent is processing (model call in flight or tool execution running).
    Working,
    /// The agent has crashed or the status file contains `"error"` / `"crashed"`.
    Crashed,
    /// Status file absent, unreadable, or contains an unrecognised token.
    Unknown,
}

// ── Screen ────────────────────────────────────────────────────────────────────

/// Which screen the operator is currently looking at.
///
/// The TUI is multi-screen as of Phase 6: the chat screen is the per-agent
/// typing surface; the panopticon is the global overview. Phase 7 adds the
/// per-agent inspect screen — a read-only drill-in from the panopticon.
/// `Tab` cycles between chat and panopticon; the inspect screen is entered
/// by `Enter` on a panopticon row and exited with `h`/`Esc`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Screen {
    /// Chat conversation pane with input — primary work surface. The agent
    /// targeted by chat is `AppState::chat_agent_name`, set at TUI launch
    /// from `reeve attach <name>` or session memory. It does not change
    /// mid-session: per-agent chat for arbitrary agents is intentionally
    /// only reachable via the CLI escape hatch, per spec.
    #[default]
    Chat,
    /// Global panopticon: agent table, recent events, queue counts.
    Panopticon,
    /// Per-agent inspect — read-only drill-in from the panopticon.
    /// Renders five tabs across the top (Thread/Tools/Model/Decisions/
    /// Memory); only Thread is populated in this ladder. The targeted
    /// agent is `AppState::inspect_agent_name`, set by `Enter` on the
    /// focused panopticon row.
    Inspect,
    /// Quarantine review. Phase 6 ships a stub renderer that surfaces the
    /// per-agent quarantine count from the panopticon snapshot and a note
    /// that the full review UI lands in Phase 8. The Screen variant exists
    /// so the panopticon's `Q` keybinding has a real target and Phase 8
    /// can fill in the renderer without touching the dispatch path.
    Quarantine,
}

// ── InspectTab ────────────────────────────────────────────────────────────────

/// Which tab is active in the per-agent inspect screen.
///
/// Phase 7 ships only the Thread tab as functional content; the other four
/// render a "not yet available" placeholder. They exist as enum variants
/// so the keymap, tab-cycling math, and renderer dispatch are shaped for
/// the full inspect layout from the start — later phases fill in the
/// stubs without touching the cycle/dispatch logic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InspectTab {
    /// Conversation journal with speaker attribution. Default.
    #[default]
    Thread,
    /// Tool invocations log. Stub in ladder 2.
    Tools,
    /// Model API calls log. Stub in ladder 2.
    Model,
    /// Authority decisions log. Stub in ladder 2.
    Decisions,
    /// Memory references log. Stub in ladder 2.
    Memory,
}

impl InspectTab {
    /// All tabs in display order — drives both the tab-bar render and
    /// numeric `1-5` jump bindings. The ordinal of a variant in this
    /// array matches its `1-5` shortcut (0-indexed internally).
    pub const ALL: [Self; 5] = [
        Self::Thread,
        Self::Tools,
        Self::Model,
        Self::Decisions,
        Self::Memory,
    ];

    /// Display label for the tab bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Thread => "THREAD",
            Self::Tools => "TOOLS",
            Self::Model => "MODEL",
            Self::Decisions => "DECISIONS",
            Self::Memory => "MEMORY",
        }
    }

    /// Index of this tab in `ALL` — used for both render highlighting
    /// and cycle arithmetic. Kept as a method rather than a `usize`
    /// field so adding a tab is one match arm, not a renumbering.
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// Next tab in cycle order, wrapping back to Thread after Memory.
    pub fn next(self) -> Self {
        let i = self.index();
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// Previous tab in cycle order, wrapping from Thread back to Memory.
    pub fn prev(self) -> Self {
        let i = self.index();
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

// ── AppState ──────────────────────────────────────────────────────────────────

/// Full application state, refreshed by the filesystem watcher.
///
/// The watcher loop (in `crate::watcher`) calls `crate::reader` functions on
/// every filesystem event and replaces the fields here. The renderer reads
/// this struct in the draw pass; it never touches the filesystem directly.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Role name (from the agent registry) of the agent currently shown in
    /// the chat screen. Drives which `AgentDirs` the chat reload reads,
    /// which inbox `submit_message` writes to, and what's recorded in
    /// `session.toml` on exit. Defaults to `"lead"` for backward
    /// compatibility with the single-screen TUI.
    pub chat_agent_name: String,
    pub conversation: Vec<ConversationEntry>,
    pub status: AgentStatus,
    pub cost_usd: f64,
    pub persona_name: String,
    pub model_id: String,
    /// Operator identity used to resolve the `"you"` label on inbound entries.
    /// Populated at startup from the identity registry; left `None` until the
    /// registry lookup completes (renderer falls back to a short-id label).
    pub operator_id: Option<IdentityId>,
    /// Number of rows the operator has scrolled up from the bottom of the
    /// conversation pane. `0` means the view is anchored to the most recent
    /// entry (default behaviour: new inbound/outbound entries auto-scroll into
    /// view). Any positive value pins the view at that distance from the
    /// bottom; new entries arrive *above* the visible area until the user
    /// scrolls back down or hits End. The renderer clamps reads against the
    /// actual visible content, so an over-large value is harmless.
    pub scroll_offset: u16,
    pub(crate) input: String,
    cursor_pos: usize, // private: always a valid byte boundary in input
    /// Which screen the renderer should draw on the next frame.
    pub screen: Screen,
    /// Panopticon view-model, refreshed from disk by the watcher loop.
    /// Renders empty when not yet read.
    pub panopticon: PanopticonSnapshot,
    /// Cursor position in the panopticon's agent table. `j`/`k` adjust it;
    /// the renderer clamps against the actual agent count, so any value is
    /// safe.
    pub panopticon_focus: usize,
    /// Role name (from the agent registry) of the agent currently being
    /// drilled into via the per-agent inspect screen. Set when the
    /// operator presses `Enter` on a panopticon row; `None` until that
    /// happens and after `h`/`Esc` returns to the panopticon (the value
    /// is preserved across the return so re-entering inspect for the
    /// same agent costs no extra disk reads).
    ///
    /// Inspect lookups and inspect reloads use this name; chat lookups
    /// continue to use `chat_agent_name`. The two are independent so the
    /// operator can chat with one agent and inspect another without
    /// either screen's data being clobbered.
    pub inspect_agent_name: Option<String>,
    /// Active tab on the inspect screen. `Tab`/`Shift+Tab` and `1-5`
    /// cycle this; the renderer dispatches body content based on it.
    pub inspect_tab: InspectTab,
}

impl AppState {
    /// Replace input content; cursor moves to end.
    pub fn set_input(&mut self, s: String) {
        self.cursor_pos = s.len();
        self.input = s;
    }

    /// Current cursor byte position (always valid within input).
    pub fn cursor_pos(&self) -> usize {
        self.cursor_pos
    }

    /// Move cursor by `delta` characters (not bytes), clamped to bounds.
    /// Returns the new cursor position.
    pub fn move_cursor(&mut self, delta: isize) -> usize {
        if delta == 0 {
            return self.cursor_pos;
        }
        // Walk character boundaries in the correct direction
        let chars: Vec<(usize, char)> = self.input.char_indices().collect();
        if chars.is_empty() {
            return 0;
        }
        // Find current char index (the first char boundary at or after cursor_pos).
        let current_char_idx = chars
            .iter()
            .position(|(pos, _)| *pos >= self.cursor_pos)
            .unwrap_or(chars.len());
        // Clamp arithmetic in signed space, then convert back.
        let new_char_idx = (current_char_idx.cast_signed().saturating_add(delta))
            .max(0)
            .min(chars.len().cast_signed())
            .cast_unsigned();
        self.cursor_pos = if new_char_idx >= chars.len() {
            self.input.len()
        } else {
            chars[new_char_idx].0
        };
        self.cursor_pos
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            chat_agent_name: String::from("lead"),
            conversation: Vec::new(),
            status: AgentStatus::Unknown,
            cost_usd: 0.0,
            persona_name: String::from("lead"),
            model_id: String::from("unknown"),
            operator_id: None,
            scroll_offset: 0,
            input: String::new(),
            cursor_pos: 0,
            screen: Screen::Chat,
            panopticon: PanopticonSnapshot::default(),
            panopticon_focus: 0,
            inspect_agent_name: None,
            inspect_tab: InspectTab::Thread,
        }
    }
}

impl AppState {
    /// Scroll the conversation pane up by `rows`. The new offset is bounded
    /// only above; if the requested offset is larger than the visible content,
    /// the renderer clamps it on its side.
    pub fn scroll_up(&mut self, rows: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(rows);
    }

    /// Scroll the conversation pane down by `rows`. Saturates at 0 (the
    /// bottom). Once the offset reaches 0 auto-scroll-on-new-entries resumes.
    pub fn scroll_down(&mut self, rows: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
    }

    /// Anchor the view at the most recent entry. Re-enables auto-scroll.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// `true` when the conversation view is anchored to the latest entry.
    pub fn is_at_bottom(&self) -> bool {
        self.scroll_offset == 0
    }

    /// Flip between [`Screen::Chat`] and [`Screen::Panopticon`]. Resets the
    /// panopticon focus to the top row whenever the operator enters the
    /// panopticon so the cursor never lands on a row that has since
    /// scrolled out of view.
    ///
    /// `Screen::Quarantine` is treated as a peer of `Panopticon` for the
    /// purposes of this toggle — calling `toggle_screen` from quarantine
    /// returns to chat. The app-level keymap routes `Tab` from quarantine
    /// to panopticon directly rather than calling this helper, so this
    /// fallback is only reached if a new caller appears later.
    pub fn toggle_screen(&mut self) {
        self.screen = match self.screen {
            Screen::Chat => {
                self.panopticon_focus = 0;
                Screen::Panopticon
            }
            Screen::Panopticon | Screen::Quarantine | Screen::Inspect => Screen::Chat,
        };
    }

    /// Move the panopticon focus up one row, clamped at zero.
    pub fn panopticon_focus_up(&mut self) {
        self.panopticon_focus = self.panopticon_focus.saturating_sub(1);
    }

    /// Move the panopticon focus down one row, clamped against the agent
    /// table's length. Safe at empty tables (does nothing).
    pub fn panopticon_focus_down(&mut self) {
        let len = self.panopticon.agents.len();
        if len == 0 {
            return;
        }
        let max = len.saturating_sub(1);
        if self.panopticon_focus < max {
            self.panopticon_focus += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(s: &str) -> AppState {
        let mut st = AppState::default();
        st.set_input(s.to_owned());
        st
    }

    #[test]
    fn set_input_places_cursor_at_end() {
        let mut st = AppState::default();
        st.set_input("hello".to_owned());
        assert_eq!(st.cursor_pos(), 5);
    }

    #[test]
    fn move_cursor_on_empty_string_stays_at_zero() {
        let mut st = AppState::default();
        assert_eq!(st.move_cursor(-1), 0);
        assert_eq!(st.move_cursor(1), 0);
    }

    #[test]
    fn move_cursor_clamps_at_start() {
        let mut st = state_with("hello");
        // move left past start — should clamp at 0
        let pos = st.move_cursor(-100);
        assert_eq!(pos, 0);
        assert_eq!(st.cursor_pos(), 0);
    }

    #[test]
    fn move_cursor_clamps_at_end() {
        let mut st = state_with("hello");
        // cursor starts at end (5); move right past end — clamps at len
        let pos = st.move_cursor(100);
        assert_eq!(pos, 5); // "hello".len()
        assert_eq!(st.cursor_pos(), 5);
    }

    #[test]
    fn move_cursor_unicode_multibyte() {
        // 'é' is 2 bytes (U+00E9); string "héllo" has byte len 6
        let mut st = state_with("héllo");
        assert_eq!(st.cursor_pos(), 6); // set_input puts cursor at byte end

        // move left 1 char: 'o' is 1 byte, cursor should go to byte 5
        let pos = st.move_cursor(-1);
        assert_eq!(pos, 5, "one char left from end");

        // move left 3 more: skips 'l', 'l', 'é' (2 bytes) → cursor at byte 1 (after 'h')
        let pos = st.move_cursor(-3);
        assert_eq!(pos, 1, "three chars left from 'o'");

        // move left 1 more: skips 'h' → cursor at byte 0
        let pos = st.move_cursor(-1);
        assert_eq!(pos, 0, "one char left from 'é'");

        // move right 2: skips 'h' (1 byte), then 'é' (2 bytes) → cursor at byte 3
        let pos = st.move_cursor(2);
        assert_eq!(pos, 3, "two chars right from start");
    }

    #[test]
    fn move_cursor_ascii_step_by_step() {
        let mut st = state_with("abc");
        // cursor at 3
        st.move_cursor(-3); // cursor at 0
        assert_eq!(st.cursor_pos(), 0);
        st.move_cursor(1); // cursor at 1 ('b')
        assert_eq!(st.cursor_pos(), 1);
        st.move_cursor(1); // cursor at 2 ('c')
        assert_eq!(st.cursor_pos(), 2);
        st.move_cursor(1); // cursor at 3 (end)
        assert_eq!(st.cursor_pos(), 3);
    }
}
