//! Panopticon screen renderer.
//!
//! Translates a [`PanopticonSnapshot`] into ratatui widgets. Pure: no I/O,
//! no side effects. Aesthetic continuity with [`crate::ui`] (the lead chat
//! screen) is intentional — both screens share the same `Yellow`/`Cyan`/`Magenta`
//! palette, the same `NO_COLOR` fallback contract, and the same `reeve · …`
//! title prefix in blue.
//!
//! Layout (top to bottom):
//! 1. Title bar — 1 row.
//! 2. Pending decisions section — 1 row header (always rendered; "none" in
//!    Phase 6).
//! 3. Agents section — header + lead row + `─ workers ─` separator + non-lead
//!    rows.
//! 4. Recent events — header + rows.
//! 5. Queues strip — 1 row.
//! 6. Footer — 1 row.
//!
//! Phase 6 design choices (Saskia's review):
//! - No MODEL column — the chat-screen title bar already shows model per
//!   agent; in the panopticon it degrades to visual noise.
//! - No standalone ACTIVITY column — folded into the status sigil cell as
//!   `● 0:12` (working with time-in-state), `○` idle, `!` waiting (Phase
//!   6 never hits this — pending decisions stay empty), `?` unknown, `✓`
//!   clean stopped, `✗` crashed stopped.
//! - Sort within running: status priority then oldest first; lead pinned at
//!   the top with a `─ workers ─` separator before the rest.
//! - Stopped rows rendered DIM so they recede from glance attention.
//! - Operator-authored events surface as a single `you` row (the data
//!   layer's [`Source::Operator`] variant, rendered as [`OPERATOR_LABEL`]);
//!   coloured Yellow to match the operator's speaker hue in the chat
//!   screen.
//! - Pending decisions section is *rendered* even when empty (`── none ──`)
//!   so the layout's structural ribs stay constant when the first card
//!   arrives.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use time::{Duration, OffsetDateTime};

use crate::panopticon::{
    AgentRow, EventKind, PanopticonSnapshot, QueueCounts, RecentEvent, Source,
};
use crate::state::AgentStatus;
use crate::ui_common::{format_time_hhmm, no_color};

// ── Layout constants ────────────────────────────────────────────────────────
//
// All in display-cell units (columns for widths, rows for the row constants).
// Sized for an 80-column terminal at four agents + six event rows + the
// pending-decisions / queues / footer chrome — the wireframe budget. Tuning
// notes inline.

/// AGENT column width. Long enough for `worker-<8-hex>` (15 chars) plus two
/// chars of headroom before truncation kicks in.
const AGENT_COL_WIDTH: usize = 18;
/// PERSONA column width. Personas are short names (`lead`, `worker`,
/// `reviewer`, …); 10 chars covers every persona shipped so far.
const PERSONA_COL_WIDTH: usize = 10;
/// ELAPSED column width. Covers `2h 04m` (6 chars) with one char of
/// breathing room.
const ELAPSED_COL_WIDTH: usize = 8;
/// Status cell width. Working agents render as `● 0:12` (six display
/// chars at minute scale, seven at `● 12:30`); idle/stopped agents
/// render as a single sigil. Pad to 7 so ELAPSED and COST stay aligned
/// across mixed running/idle/stopped rows. `pad_right` truncates beyond
/// this, but the suffix format caps below the bound by construction.
const STATUS_COL_WIDTH: usize = 7;
/// Event-kind column width. Covers the longest kind currently shipped
/// (`system`, 6 chars) without truncating; future taxonomy additions
/// (`tool`, `flag`, `model`, `spawn`, `exit`) all fit.
const EVENT_KIND_COL_WIDTH: usize = 6;

/// Minimum row budget the events panel insists on, no matter how few events
/// exist. Six is Saskia's floor: "events at 3 rows is broken — you can't
/// scan history at three rows."
const EVENT_PANEL_MIN_ROWS: u16 = 6;

/// Rows the layout consumes outside the agents and events panels (title bar,
/// pending-decisions header, agents header, events header, queues strip,
/// footer — six fixed-length-1 chunks). Used to compute the agents-region
/// cap so a long agent list cannot starve the events panel.
const NON_AGENT_FIXED_ROWS: u16 = 6;

// ── Title bar ────────────────────────────────────────────────────────────────

/// Build the title-bar line: `reeve · panopticon ─── $X.XX · Hh ────`.
///
/// Per the design review, the `N agents` count is intentionally omitted —
/// the agent table is right there to be counted.
fn build_title_bar(snap: &PanopticonSnapshot) -> Line<'static> {
    let prefix_style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::Blue)
    };
    let prefix = Span::styled("reeve \u{00B7} panopticon ".to_owned(), prefix_style);

    let cost = format!("${:.2}", snap.total_cost_usd);
    let elapsed = snap
        .session_elapsed
        .map(format_elapsed_short)
        .unwrap_or_default();
    let suffix = if elapsed.is_empty() {
        format!("\u{2500}\u{2500}\u{2500} {cost}")
    } else {
        format!("\u{2500}\u{2500}\u{2500} {cost} \u{00B7} {elapsed}")
    };

    Line::from(vec![prefix, Span::raw(suffix)])
}

// ── Section headers ─────────────────────────────────────────────────────────

/// Local thin wrapper around [`crate::ui_common::build_section_header`]
/// so existing call sites keep their unqualified name. The shared
/// helper was lifted to `ui_common` when the quarantine screen needed
/// the same `─ label ──────────` shape.
fn build_section_header(label: &str, width: u16) -> Line<'static> {
    crate::ui_common::build_section_header(label, width)
}

/// Build the pending-decisions header — explicit `── none ──` in Phase 6
/// because the panel is rendered-but-empty. Saskia: "the operator builds
/// muscle memory for where the panel is, and the queue counters gain a
/// referent."
fn build_pending_header(count: usize, width: u16) -> Line<'static> {
    let label = if count == 0 {
        "\u{2500} pending decisions \u{2500}\u{2500}\u{2500}\u{2500} none ".to_owned()
    } else {
        format!("\u{2500} pending decisions \u{2500}\u{2500}\u{2500}\u{2500} {count} ")
    };
    let pad = usize::from(width).saturating_sub(label.chars().count());
    let rule: String = "\u{2500}".repeat(pad);
    Line::from(format!("{label}{rule}"))
}

// ── Status sigil ────────────────────────────────────────────────────────────

/// Render the status cell for an agent row. Combines the sigil with a
/// time-in-state suffix when the agent is `Working` (so the operator can
/// tell a stuck call from a fresh one without reading two columns).
///
/// The returned span is padded to [`STATUS_COL_WIDTH`] so the ELAPSED and
/// COST columns stay aligned across rows whose status cell would
/// otherwise differ in width — `● 0:12` (six display chars) versus `○`
/// (one) versus `✓` (one). Without padding, the columns following the
/// status cell shift between rows and the table reads ragged.
///
/// Stopped agents get the stopped sigils: `✗` for `Crashed`, `✓` otherwise.
fn build_status_cell(row: &AgentRow, now: OffsetDateTime) -> Span<'static> {
    let (text, color) = status_cell_text(row, now);
    let padded = pad_right(&text, STATUS_COL_WIDTH);
    styled_span(padded, color)
}

/// Bare text + colour for the status cell, without padding. Split out so
/// the cell builder can apply [`STATUS_COL_WIDTH`] padding uniformly and
/// tests can pin the raw content without caring about column alignment.
fn status_cell_text(row: &AgentRow, now: OffsetDateTime) -> (String, Option<Color>) {
    if !row.is_running {
        return match row.status {
            AgentStatus::Crashed => ("\u{2717}".to_owned(), Some(Color::Red)), // ✗
            AgentStatus::Idle | AgentStatus::Working | AgentStatus::Unknown => {
                ("\u{2713}".to_owned(), Some(Color::Green)) // ✓
            }
        };
    }
    match row.status {
        AgentStatus::Working => {
            // ● followed by time-in-state (e.g. `● 0:12`) when we have an
            // anchor; bare `●` when the status file mtime is unavailable.
            let suffix = row
                .state_changed_at
                .map(|t| format!(" {}", format_short_duration(now - t)))
                .unwrap_or_default();
            (format!("\u{25CF}{suffix}"), Some(Color::Yellow))
        }
        AgentStatus::Idle => ("\u{25CB}".to_owned(), None), // ○
        AgentStatus::Crashed => ("\u{2717}".to_owned(), Some(Color::Red)), // ✗
        AgentStatus::Unknown => ("?".to_owned(), None),
    }
}

/// Build a `Span` honouring `NO_COLOR`. Wraps the fg-color application so
/// every styled span in this module collapses to default style under
/// monochrome — the sigil and the textual context still carry the meaning.
fn styled_span(text: String, fg: Option<Color>) -> Span<'static> {
    if no_color() {
        Span::raw(text)
    } else {
        match fg {
            Some(c) => Span::styled(text, Style::default().fg(c)),
            None => Span::raw(text),
        }
    }
}

// ── Time formatting ─────────────────────────────────────────────────────────

/// Format a duration for the title-bar's session-elapsed suffix: `2h`, `42m`,
/// or `9s`. Single-unit, no zero-padding — the title bar is glance-only.
fn format_elapsed_short(d: Duration) -> String {
    let secs = d.whole_seconds().max(0);
    if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Format a duration for the working-sigil suffix: `0:12`, `1:04`,
/// `12:30`, `2h04`. Used to show time-in-state alongside `●`.
fn format_short_duration(d: Duration) -> String {
    let secs = d.whole_seconds().max(0);
    if secs >= 3600 {
        format!("{}h{:02}", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Format a duration for the agent table's ELAPSED column: `2h 04m`,
/// `42m`, `9s`. Two-unit when hours present.
fn format_elapsed_table(d: Duration) -> String {
    let secs = d.whole_seconds().max(0);
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

// ── Agent table ─────────────────────────────────────────────────────────────

/// Build the agent table rows. Lead pinned at row one (with the focus
/// cursor coloured Cyan to match the lead persona in the chat screen);
/// non-lead rows preceded by a `─ workers ─` separator when present.
/// Stopped rows render DIM so they fade from glance attention.
fn build_agent_rows(
    snap: &PanopticonSnapshot,
    focus: usize,
    width: u16,
    now: OffsetDateTime,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    // `sort_key` pins lead at index 0 when present; the renderer relies on
    // that invariant here. If the sort ever changes, this condition needs
    // to find the lead by name instead.
    let has_lead = snap.agents.first().is_some_and(|a| a.name == "lead");
    let has_non_lead = snap.agents.iter().any(|a| a.name != "lead");
    let separator_idx = if has_lead && has_non_lead {
        Some(1)
    } else {
        None
    };

    for (idx, agent) in snap.agents.iter().enumerate() {
        if Some(idx) == separator_idx {
            lines.push(build_workers_separator(width));
        }
        lines.push(build_agent_row(agent, focus == idx, now));
    }
    lines
}

/// `─ workers ───────────────`. Inserted between the lead row and the rest
/// so the operator's eye knows the structural break.
fn build_workers_separator(width: u16) -> Line<'static> {
    let lead = "\u{2500} workers ";
    let pad = usize::from(width).saturating_sub(lead.chars().count());
    let rule: String = "\u{2500}".repeat(pad);
    let style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Line::from(Span::styled(format!("{lead}{rule}"), style))
}

/// Build a single agent table row:
/// `▶ name  persona   status_cell   elapsed   $cost`.
///
/// The focused row's `▶` cursor is coloured Cyan to match the lead persona
/// in the chat screen — visual identity carries when the operator presses
/// Enter and the chat opens.
fn build_agent_row(agent: &AgentRow, focused: bool, now: OffsetDateTime) -> Line<'static> {
    let cursor = if focused { "\u{25B6} " } else { "  " };
    let cursor_span = if no_color() {
        Span::raw(cursor.to_owned())
    } else {
        Span::styled(cursor.to_owned(), Style::default().fg(Color::Cyan))
    };

    let name = pad_right(&agent.name, AGENT_COL_WIDTH);
    let persona = pad_right(
        agent.persona_name.as_deref().unwrap_or("-"),
        PERSONA_COL_WIDTH,
    );
    let elapsed = pad_right(&format_elapsed_table(agent.elapsed), ELAPSED_COL_WIDTH);
    let cost = format!("${:>6.2}", agent.cost_usd);

    let mut spans = vec![
        cursor_span,
        Span::raw(format!("{name} {persona} ")),
        build_status_cell(agent, now),
        Span::raw(format!(" {elapsed} {cost}")),
    ];

    if !agent.is_running {
        // Saskia: stopped agents collapse visually into a quieter block.
        // DIM the whole row so the lead-of-attention stays on running rows.
        if !no_color() {
            spans = spans
                .into_iter()
                .map(|s| {
                    let style = s.style.add_modifier(Modifier::DIM);
                    Span::styled(s.content, style)
                })
                .collect();
        }
    }

    Line::from(spans)
}

/// Truncate or right-pad a column value to `width` display chars. Naive on
/// graphemes — agent names and persona names are ASCII-ish in practice; if
/// that changes the renderer needs `unicode-width`.
fn pad_right(s: &str, width: usize) -> String {
    let actual = s.chars().count();
    if actual >= width {
        s.chars().take(width).collect()
    } else {
        let mut out = s.to_owned();
        out.extend(std::iter::repeat_n(' ', width - actual));
        out
    }
}

// ── Recent events ───────────────────────────────────────────────────────────

/// Build one recent-event row: `HH:MM  source       kind    summary`.
fn build_event_row(event: &RecentEvent, width: u16) -> Line<'static> {
    let time = format_time_hhmm(event.timestamp);
    let source = pad_right(event.source.label(), AGENT_COL_WIDTH);
    let source_color = source_color_for(&event.source);
    let kind_text = pad_right(event.kind.tag(), EVENT_KIND_COL_WIDTH);
    let kind_color = event_kind_color(event.kind);

    let prefix = format!("{time}  ");
    let summary_budget = usize::from(width)
        .saturating_sub(prefix.len() + AGENT_COL_WIDTH + 1 + EVENT_KIND_COL_WIDTH + 1);
    let summary = truncate(&event.summary, summary_budget);

    let summary_style = if event.kind == EventKind::System && !no_color() {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };

    Line::from(vec![
        Span::raw(prefix),
        styled_span(source, Some(source_color)),
        Span::raw(" "),
        styled_span(kind_text, Some(kind_color)),
        Span::raw(" "),
        Span::styled(summary, summary_style),
    ])
}

/// Pick a fg colour for the event source: `Yellow` for the operator
/// (matches the operator's speaker hue in the chat screen), `Cyan` for the
/// lead, `Magenta` for any other agent. Total — every event has a source
/// and every source has a colour.
fn source_color_for(source: &Source) -> Color {
    match source {
        Source::Operator => Color::Yellow,
        Source::Agent(name) if name == "lead" => Color::Cyan,
        Source::Agent(_) => Color::Magenta,
    }
}

/// Pick a fg colour for the event kind tag. `Msg` `Cyan`, `System`
/// `DarkGray`. Exhaustive match — adding a new [`EventKind`] variant is a
/// compile error here, by design (so a new taxonomy entry can't silently
/// render uncoloured).
fn event_kind_color(kind: EventKind) -> Color {
    match kind {
        EventKind::Msg => Color::Cyan,
        EventKind::System => Color::DarkGray,
    }
}

/// Truncate a string to at most `max_chars` display chars, appending an
/// ellipsis when truncation actually fires.
fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_owned();
    }
    if max_chars == 1 {
        return "\u{2026}".to_owned();
    }
    let prefix: String = s.chars().take(max_chars - 1).collect();
    format!("{prefix}\u{2026}")
}

// ── Queues strip ────────────────────────────────────────────────────────────

/// `m N·c N·Q N·$ ok`. Separators are bare `·` (no surrounding spaces) so
/// the strip stays compact under 80 cols even as counts grow. `$ ok` /
/// `$ TRIPPED` is dollar-sigil-style consistent with the rest of the row.
fn build_queues_strip(counts: QueueCounts) -> Line<'static> {
    let cost_label = if counts.cost_ok { "$ ok" } else { "$ TRIPPED" };
    let cost_color = if counts.cost_ok {
        Some(Color::Green)
    } else {
        Some(Color::Red)
    };
    Line::from(vec![
        Span::raw(format!(
            "m {}\u{00B7}c {}\u{00B7}Q {}\u{00B7}",
            counts.memory, counts.config, counts.quarantine
        )),
        styled_span(cost_label.to_owned(), cost_color),
    ])
}

// ── Footer ──────────────────────────────────────────────────────────────────

/// `j/k navigate · Enter open · Tab chat · Q quarantine · q quit`. Keeps
/// the same `·` separators as the queues strip for visual rhythm.
fn build_footer() -> Line<'static> {
    Line::from(
        "j/k navigate \u{00B7} Enter open \u{00B7} Tab chat \u{00B7} \
         Q quarantine \u{00B7} q quit"
            .to_owned(),
    )
}

// ── Top-level draw ──────────────────────────────────────────────────────────

/// Render the panopticon screen into `frame`.
///
/// `focus` is the index of the focused agent row in `snap.agents`. The
/// renderer clamps it on its own side, so an out-of-range index is harmless.
pub fn draw(frame: &mut Frame<'_>, snap: &PanopticonSnapshot, focus: usize) {
    let area = frame.area();
    let width = area.width;
    let now = OffsetDateTime::now_utc();
    let clamped_focus = focus.min(snap.agents.len().saturating_sub(1));

    let agent_rows = build_agent_rows(snap, clamped_focus, width, now);
    let event_rows: Vec<Line<'static>> = snap
        .recent_events
        .iter()
        .map(|e| build_event_row(e, width))
        .collect();

    // Region budgets:
    // - Agents region is content-sized BUT capped by the screen budget so
    //   a long agent list cannot push the events, queues, or footer off
    //   the bottom. The cap is computed from `area.height` minus the
    //   non-agent rows the layout owes (`NON_AGENT_FIXED_ROWS` headers /
    //   strip / footer) and the events floor (`EVENT_PANEL_MIN_ROWS`). A
    //   future stopped-agent collapse will keep `agent_rows.len()` low
    //   enough that this cap rarely bites; today it's the safety net.
    // - Events region uses `Min(EVENT_PANEL_MIN_ROWS)` to fill all
    //   remaining vertical space with a 6-row floor (Saskia: "events at 3
    //   rows is broken"). Critically *not* `Length(event_rows.len())`:
    //   a chatty conversation.jsonl produced a `Length(32)` request that
    //   ran the agents section off the bottom of a 24-row terminal and
    //   hid workers entirely — the same failure mode is now structurally
    //   prevented for agents too.
    let raw_agents_height = u16::try_from(agent_rows.len()).unwrap_or(u16::MAX).max(1);
    let agents_height_cap = area
        .height
        .saturating_sub(NON_AGENT_FIXED_ROWS + EVENT_PANEL_MIN_ROWS)
        .max(1);
    let agents_height = raw_agents_height.min(agents_height_cap);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                 // title bar
            Constraint::Length(1),                 // pending decisions header
            Constraint::Length(1),                 // agents section header
            Constraint::Length(agents_height),     // agent rows (capped)
            Constraint::Length(1),                 // recent events header
            Constraint::Min(EVENT_PANEL_MIN_ROWS), // events fill remaining
            Constraint::Length(1),                 // queues strip
            Constraint::Length(1),                 // footer
        ])
        .split(area);

    frame.render_widget(Paragraph::new(build_title_bar(snap)), chunks[0]);
    frame.render_widget(Paragraph::new(build_pending_header(0, width)), chunks[1]);
    frame.render_widget(
        Paragraph::new(build_section_header("agents", width)),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(Text::from(agent_rows)).wrap(Wrap { trim: false }),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(build_section_header("recent events", width)),
        chunks[4],
    );
    frame.render_widget(
        Paragraph::new(Text::from(event_rows)).wrap(Wrap { trim: false }),
        chunks[5],
    );
    frame.render_widget(
        Paragraph::new(build_queues_strip(snap.queue_counts)),
        chunks[6],
    );
    frame.render_widget(Paragraph::new(build_footer()), chunks[7]);
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panopticon::{build_snapshot, AgentInputs};

    // U1: title-bar elapsed format is single-unit and glanceable. The hour
    // path is the steady-state for a real session.
    #[test]
    fn format_elapsed_short_picks_largest_unit() {
        assert_eq!(format_elapsed_short(Duration::seconds(9)), "9s");
        assert_eq!(format_elapsed_short(Duration::seconds(60)), "1m");
        assert_eq!(format_elapsed_short(Duration::seconds(125)), "2m");
        assert_eq!(format_elapsed_short(Duration::seconds(3600)), "1h");
        assert_eq!(format_elapsed_short(Duration::seconds(7325)), "2h");
        // Negative durations clamp to zero rather than wrapping or panicking.
        assert_eq!(format_elapsed_short(Duration::seconds(-5)), "0s");
    }

    // U2: time-in-state for working agents reads `M:SS` under an hour and
    // `Hh MM` over. This is the format alongside the `●` sigil.
    #[test]
    fn format_short_duration_uses_clock_like_format() {
        assert_eq!(format_short_duration(Duration::seconds(12)), "0:12");
        assert_eq!(format_short_duration(Duration::seconds(64)), "1:04");
        assert_eq!(format_short_duration(Duration::seconds(750)), "12:30");
        assert_eq!(format_short_duration(Duration::seconds(7440)), "2h04");
    }

    // U3: the agent table's ELAPSED column carries two units once we cross
    // an hour boundary so a long session reads at a glance.
    #[test]
    fn format_elapsed_table_two_units_above_hour() {
        assert_eq!(format_elapsed_table(Duration::seconds(9)), "9s");
        assert_eq!(format_elapsed_table(Duration::seconds(125)), "2m");
        assert_eq!(format_elapsed_table(Duration::seconds(7440)), "2h 04m");
    }

    // U4: source colour mapping: operator yellow (matches the chat-screen
    // operator hue), lead cyan, peer agents magenta. NO_COLOR is honoured
    // by `styled_span`, not the mapping itself.
    #[test]
    fn source_color_for_distinguishes_operator_lead_and_peers() {
        assert_eq!(source_color_for(&Source::Operator), Color::Yellow);
        assert_eq!(
            source_color_for(&Source::Agent("lead".to_owned())),
            Color::Cyan
        );
        assert_eq!(
            source_color_for(&Source::Agent("worker-abc12345".to_owned())),
            Color::Magenta
        );
    }

    // U5: event kind colour mapping. Exhaustive over [`EventKind`]; a new
    // variant added to the data layer fails to compile here, by design.
    #[test]
    fn event_kind_color_msg_cyan_system_darkgray() {
        assert_eq!(event_kind_color(EventKind::Msg), Color::Cyan);
        assert_eq!(event_kind_color(EventKind::System), Color::DarkGray);
    }

    // U6: status cell for a working agent embeds time-in-state from
    // `state_changed_at`. Bare `●` when the anchor is unavailable.
    #[test]
    fn build_status_cell_working_includes_time_in_state() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
        let changed = OffsetDateTime::from_unix_timestamp(1_700_000_088).unwrap();
        let mut row = AgentRow {
            name: "lead".to_owned(),
            persona_name: Some("lead".to_owned()),
            status: AgentStatus::Working,
            is_running: true,
            cost_usd: 0.0,
            elapsed: Duration::seconds(100),
            state_changed_at: Some(changed),
        };
        let (text, _) = status_cell_text(&row, now);
        assert_eq!(text, "\u{25CF} 0:12");

        row.state_changed_at = None;
        let (text, _) = status_cell_text(&row, now);
        assert_eq!(text, "\u{25CF}");
    }

    // U6b: build_status_cell pads to STATUS_COL_WIDTH so mixed
    // running/idle/stopped rows keep ELAPSED and COST aligned. The
    // smoke at 80x24 caught this — `● 0:12` and `○` would otherwise
    // push downstream columns out of sync row-to-row.
    #[test]
    fn build_status_cell_pads_to_fixed_width() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
        let row = AgentRow {
            name: "lead".to_owned(),
            persona_name: Some("lead".to_owned()),
            status: AgentStatus::Idle,
            is_running: true,
            cost_usd: 0.0,
            elapsed: Duration::seconds(0),
            state_changed_at: None,
        };
        let cell = build_status_cell(&row, now);
        assert_eq!(
            cell.content.chars().count(),
            STATUS_COL_WIDTH,
            "status cell must always be STATUS_COL_WIDTH wide; got {:?}",
            cell.content
        );
    }

    // U7: stopped agents pick their sigil from status — `✗` for crashed,
    // `✓` for everything else.
    #[test]
    fn build_status_cell_stopped_uses_clean_or_crash_sigil() {
        let now = OffsetDateTime::now_utc();
        let mut row = AgentRow {
            name: "worker-old".to_owned(),
            persona_name: Some("worker".to_owned()),
            status: AgentStatus::Idle,
            is_running: false,
            cost_usd: 0.0,
            elapsed: Duration::seconds(10),
            state_changed_at: None,
        };
        let (text, _) = status_cell_text(&row, now);
        assert_eq!(text, "\u{2713}"); // ✓
        row.status = AgentStatus::Crashed;
        let (text, _) = status_cell_text(&row, now);
        assert_eq!(text, "\u{2717}"); // ✗
    }

    // U8: truncation appends an ellipsis only when truncation actually
    // fires; short inputs round-trip.
    #[test]
    fn truncate_only_ellipsizes_on_overflow() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w\u{2026}");
        assert_eq!(truncate("anything", 0), "");
        assert_eq!(truncate("anything", 1), "\u{2026}");
    }

    // U9: pending-decisions header is rendered even at zero, with explicit
    // `── none ──`. Phase 6 spec: panel rendered but empty.
    #[test]
    fn build_pending_header_renders_none_at_zero() {
        let line = build_pending_header(0, 80);
        // Span 0 is the only span; concatenate for visual inspection.
        let rendered: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            rendered.contains("pending decisions"),
            "header label missing: {rendered:?}"
        );
        assert!(
            rendered.contains("none"),
            "empty-state suffix missing: {rendered:?}"
        );
    }

    // U10: the agent-rows builder inserts a `─ workers ─` separator
    // between the lead row and the first non-lead row, but not when there
    // are no non-lead agents.
    #[test]
    fn agent_rows_insert_workers_separator_only_when_workers_present() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
        let only_lead = build_snapshot(
            &[AgentInputs {
                name: "lead".to_owned(),
                persona_name: Some("lead".to_owned()),
                status: AgentStatus::Idle,
                is_running: true,
                cost_usd: 0.0,
                spawned_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                state_changed_at: None,
                conversation_tail: Vec::new(),
            }],
            0,
            None,
            now,
        );
        let lines = build_agent_rows(&only_lead, 0, 80, now);
        let has_separator = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("workers")));
        assert!(!has_separator, "no separator when there are no workers");

        let with_workers = build_snapshot(
            &[
                AgentInputs {
                    name: "lead".to_owned(),
                    persona_name: Some("lead".to_owned()),
                    status: AgentStatus::Idle,
                    is_running: true,
                    cost_usd: 0.0,
                    spawned_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                    state_changed_at: None,
                    conversation_tail: Vec::new(),
                },
                AgentInputs {
                    name: "worker".to_owned(),
                    persona_name: Some("worker".to_owned()),
                    status: AgentStatus::Working,
                    is_running: true,
                    cost_usd: 0.0,
                    spawned_at: OffsetDateTime::from_unix_timestamp(1_700_000_050).unwrap(),
                    state_changed_at: None,
                    conversation_tail: Vec::new(),
                },
            ],
            0,
            None,
            now,
        );
        let lines = build_agent_rows(&with_workers, 0, 80, now);
        let has_separator = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("workers")));
        assert!(
            has_separator,
            "separator must appear when there is at least one worker"
        );
    }

    // U_AGENT_CAP: a registry with many agents must not push events,
    // queues, or footer off the bottom of a 24-row terminal. Earlier
    // regression: the events region's `Length(N)` request did exactly
    // that; the same failure mode is now structurally prevented for the
    // agents region too. Pin the bottom-of-screen rows (queues + footer)
    // and assert they survived rendering with 20 agents.
    #[test]
    fn draw_keeps_queues_and_footer_visible_with_many_agents() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let now = OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
        let inputs: Vec<AgentInputs> = (0..20)
            .map(|i| AgentInputs {
                name: format!("worker-{i:02}"),
                persona_name: Some("worker".to_owned()),
                status: AgentStatus::Idle,
                is_running: true,
                cost_usd: 0.0,
                spawned_at: OffsetDateTime::from_unix_timestamp(1_700_000_000 + i64::from(i))
                    .unwrap(),
                state_changed_at: None,
                conversation_tail: Vec::new(),
            })
            .collect();
        let snap = build_snapshot(&inputs, 0, None, now);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &snap, 0)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered: String = (0..buffer.area.height)
            .map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect();
                row
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("m 0"),
            "queues strip must remain on-screen with many agents; got:\n{rendered}"
        );
        assert!(
            rendered.contains("j/k navigate"),
            "footer must remain on-screen with many agents; got:\n{rendered}"
        );
    }

    // U11: a full draw at 24-row × 80-col must put a worker row on-screen
    // when one is registered AND the lead has dozens of recent events.
    // Earlier regression: the events panel's `Constraint::Length(N)` with
    // N=32 squeezed the agents region off-screen on a 24-row terminal, so
    // the snapshot reader returned both agents but only the lead row was
    // visible. Today the renderer caps events at `EVENT_PANEL_ROWS` so the
    // agents region keeps its constant top-of-screen budget regardless of
    // how chatty the journals get.
    #[test]
    fn draw_renders_worker_row_at_80x24() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let now = OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
        let many_events: Vec<crate::state::ConversationEntry> = (0..40)
            .map(|i| crate::state::ConversationEntry {
                kind: crate::state::EntryKind::Outbound,
                text: format!("event {i}"),
                timestamp: Some(
                    OffsetDateTime::from_unix_timestamp(1_700_000_000 + i64::from(i)).unwrap(),
                ),
                sender_id: None,
            })
            .collect();
        let snap = build_snapshot(
            &[
                AgentInputs {
                    name: "lead".to_owned(),
                    persona_name: Some("lead".to_owned()),
                    status: AgentStatus::Idle,
                    is_running: true,
                    cost_usd: 0.0,
                    spawned_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                    state_changed_at: None,
                    conversation_tail: many_events,
                },
                AgentInputs {
                    name: "worker-2e28aff5".to_owned(),
                    persona_name: Some("worker".to_owned()),
                    status: AgentStatus::Idle,
                    is_running: true,
                    cost_usd: 0.0,
                    spawned_at: OffsetDateTime::from_unix_timestamp(1_700_000_050).unwrap(),
                    state_changed_at: None,
                    conversation_tail: Vec::new(),
                },
            ],
            0,
            None,
            now,
        );

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &snap, 0)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered: String = (0..buffer.area.height)
            .map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect();
                row
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("lead"),
            "lead row must be on-screen; got:\n{rendered}"
        );
        assert!(
            rendered.contains("worker-2e28aff5"),
            "worker row must be on-screen; got:\n{rendered}"
        );
    }
}
