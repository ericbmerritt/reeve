//! Application state for the Reeve TUI.
//!
//! `AppState` holds everything the renderer needs: conversation history,
//! agent status, cumulative cost, identity context, and the current input
//! buffer. All fields are plain Rust values — no I/O happens here. Reads
//! from disk are in `crate::reader`; the watcher loop calls back into
//! those readers and replaces the state.

use reeve_types::IdentityId;
use time::OffsetDateTime;

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

// ── AppState ──────────────────────────────────────────────────────────────────

/// Full application state, refreshed by the filesystem watcher.
///
/// The watcher loop (in `crate::watcher`) calls `crate::reader` functions on
/// every filesystem event and replaces the fields here. The renderer reads
/// this struct in the draw pass; it never touches the filesystem directly.
#[derive(Debug, Clone)]
pub struct AppState {
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
            conversation: Vec::new(),
            status: AgentStatus::Unknown,
            cost_usd: 0.0,
            persona_name: String::from("lead"),
            model_id: String::from("unknown"),
            operator_id: None,
            scroll_offset: 0,
            input: String::new(),
            cursor_pos: 0,
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
