//! 80×24 + `NO_COLOR=1` smoke for the Phase 6 TUI screens.
//!
//! Renders the chat, panopticon, and quarantine-stub screens into a
//! `ratatui::backend::TestBackend` sized 80×24 with `NO_COLOR=1` set, and
//! prints each buffer to stdout so the output can be eyeballed for
//! overflow, clipping, missing chrome, or unreadable layouts.
//!
//! Programmatic counterpart to the Phase 6 "fully readable" done-when
//! criterion. Run with:
//!
//! ```text
//! cargo run --example smoke_80x24_nocolor -p reeve-tui
//! ```

// The workspace clippy gate is strict on `unwrap_used`, `print_stdout`,
// and `assigning_clones` to keep library/runtime code honest. This
// example file is the inverse environment: setup failures *should*
// panic loudly so the smoke is obviously broken, and printing rendered
// buffers to stdout is the entire purpose of the binary. Allowing the
// lints at the file level (rather than tagging every call site) keeps
// the example readable.
#![expect(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::assigning_clones,
    reason = "example binary: setup must panic loudly on failure, and \
              printing rendered frames to stdout is the binary's purpose. \
              The strict workspace lints exist for library/runtime code."
)]

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use time::{Duration, OffsetDateTime};

use reeve_tui::panopticon::{
    AgentRow, EventKind, PanopticonSnapshot, QueueCounts, RecentEvent, Source,
};
use reeve_tui::state::{AgentStatus, AppState, ConversationEntry, EntryKind, Screen};

fn main() {
    std::env::set_var("NO_COLOR", "1");

    // The renderer reads `OffsetDateTime::now_utc()` to compute
    // time-in-state for the panopticon's working sigil. Anchor the
    // fixture's `state_changed_at` values to the same clock so the
    // smoke output reflects how the screen reads in a live run rather
    // than a 2-year-old timestamp.
    let now = OffsetDateTime::now_utc();
    let chat_state = sample_chat_state(now);
    let panopticon_snap = sample_panopticon(now);

    print_screen("Chat (80×24, NO_COLOR=1)", |frame| {
        reeve_tui::ui::draw(frame, &chat_state);
    });
    print_screen("Panopticon (80×24, NO_COLOR=1)", |frame| {
        reeve_tui::ui_panopticon::draw(frame, &panopticon_snap, 1);
    });
    // The quarantine screen now reads the full AppState (entries
    // list, focused index, confirm state). Use the chat fixture as
    // a base and graft an empty quarantine snapshot — the smoke just
    // verifies the layout is renderable on an 80x24 terminal.
    print_screen("Quarantine (80×24, NO_COLOR=1)", |frame| {
        reeve_tui::ui_quarantine::draw(frame, &chat_state);
    });
}

fn print_screen<F: FnOnce(&mut ratatui::Frame<'_>)>(title: &str, draw: F) {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(draw).unwrap();
    let buffer = terminal.backend().buffer();
    let lines: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect();
    println!("===== {title} =====");
    let border = "+".to_owned() + &"-".repeat(80) + "+";
    println!("{border}");
    for line in &lines {
        println!("|{line}|");
    }
    println!("{border}");
    println!();
}

fn sample_chat_state(now: OffsetDateTime) -> AppState {
    let mut state = AppState::default();
    state.screen = Screen::Chat;
    state.persona_name = "lead".to_owned();
    state.model_id = "claude-opus-4-7".to_owned();
    state.status = AgentStatus::Working;
    state.cost_usd = 0.31;
    let operator = reeve_types::IdentityId::new().unwrap();
    state.operator_id = Some(operator);
    state.conversation = vec![
        ConversationEntry {
            kind: EntryKind::Inbound,
            text: "refactor the deeds module so person and place share a base".to_owned(),
            timestamp: Some(now - Duration::minutes(4)),
            sender_id: Some(operator),
        },
        ConversationEntry {
            kind: EntryKind::Outbound,
            text: "I'll start by reading the current shape. Two passes: list the shared fields, then plan the trait."
                .to_owned(),
            timestamp: Some(now - Duration::minutes(3)),
            sender_id: None,
        },
        ConversationEntry {
            kind: EntryKind::System,
            text: "spawn worker-2e28aff5 (persona=worker)".to_owned(),
            timestamp: Some(now - Duration::minutes(2)),
            sender_id: None,
        },
    ];
    state.set_input("plan the trait surface before writing it".to_owned());
    state
}

fn sample_panopticon(now: OffsetDateTime) -> PanopticonSnapshot {
    PanopticonSnapshot {
        agents: vec![
            AgentRow {
                name: "lead".to_owned(),
                persona_name: Some("lead".to_owned()),
                status: AgentStatus::Working,
                is_running: true,
                is_ghost: false,
                cost_usd: 0.31,
                elapsed: Duration::seconds(7440),
                state_changed_at: Some(now - Duration::seconds(12)),
            },
            AgentRow {
                name: "worker-2e28aff5".to_owned(),
                persona_name: Some("worker".to_owned()),
                status: AgentStatus::Idle,
                is_running: true,
                is_ghost: false,
                cost_usd: 0.04,
                elapsed: Duration::seconds(200),
                state_changed_at: None,
            },
            AgentRow {
                name: "schema-7f3a09b1".to_owned(),
                persona_name: Some("reviewer".to_owned()),
                status: AgentStatus::Idle,
                is_running: false,
                is_ghost: false,
                cost_usd: 0.05,
                elapsed: Duration::seconds(840),
                state_changed_at: None,
            },
        ],
        recent_events: vec![
            RecentEvent {
                timestamp: now - Duration::seconds(12),
                source: Source::Agent("lead".to_owned()),
                kind: EventKind::System,
                summary: "model call · 500 out / 12k in · $0.003".to_owned(),
            },
            RecentEvent {
                timestamp: now - Duration::seconds(120),
                source: Source::Operator,
                kind: EventKind::Msg,
                summary: "refactor the deeds module".to_owned(),
            },
            RecentEvent {
                timestamp: now - Duration::seconds(180),
                source: Source::Agent("worker-2e28aff5".to_owned()),
                kind: EventKind::Msg,
                summary: "Done — pushed branch deeds-refactor".to_owned(),
            },
        ],
        queue_counts: QueueCounts {
            memory: 0,
            config: 0,
            quarantine: 1,
            cost_ok: true,
        },
        total_cost_usd: 0.40,
        session_elapsed: Some(Duration::seconds(7440)),
    }
}
