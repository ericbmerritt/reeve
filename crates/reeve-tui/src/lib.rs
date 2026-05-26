//! Reeve TUI.
//!
//! ratatui frontend, filesystem reader / watcher, signed-envelope writer.
//! Talks to the runtime through the filesystem only — no socket, no RPC.
//! See `specs/reeve-tui-design.md` and `specs/reeve-tui-screens.md` for
//! design and layout.

pub mod app;
pub mod panopticon;
pub mod quarantine_view;
pub mod reader;
pub mod session;
pub mod state;
pub mod submit;
pub mod ui;
pub mod ui_common;
pub mod ui_inspect;
pub mod ui_panopticon;
pub mod ui_quarantine;
pub mod watcher;

pub use panopticon::{
    read_snapshot as read_panopticon_snapshot, AgentRow, EventKind, PanopticonSnapshot,
    QueueCounts, RecentEvent, Source, OPERATOR_LABEL,
};
pub use reader::{heartbeat_fresh, read_conversation, read_cost, read_status};
pub use state::{AgentStatus, AppState, ConversationEntry, EntryKind};
