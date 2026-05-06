//! Application state for the Reeve TUI.
//!
//! `AppState` holds everything the renderer needs: conversation history,
//! agent status, cumulative cost, identity context, and the current input
//! buffer. All fields are plain Rust values — no I/O happens here. Reads
//! from disk are in `crate::reader`; the watcher loop calls back into
//! those readers and replaces the state.

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

impl EntryKind {
    /// Display label for this entry kind, given the persona name.
    pub fn speaker_label<'a>(&self, persona_name: &'a str) -> &'a str {
        match self {
            Self::Inbound => persona_name,
            Self::Outbound => "you",
            Self::System => "system",
        }
    }
}

// ── ConversationEntry ─────────────────────────────────────────────────────────

/// A single display-ready entry in the lead agent's conversation history.
///
/// Parsed from `agents/lead/log/conversation.jsonl` by `crate::reader`. The
/// renderer uses `kind` and `text`; `timestamp` is shown in the speaker tag
/// when present. Use [`EntryKind::speaker_label`] to obtain the display label.
#[derive(Debug, Clone)]
pub struct ConversationEntry {
    pub kind: EntryKind,
    pub text: String,
    pub timestamp: Option<OffsetDateTime>,
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
            input: String::new(),
            cursor_pos: 0,
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
