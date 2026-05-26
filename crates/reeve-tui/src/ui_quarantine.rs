//! Quarantine review screen — Phase 8.
//!
//! Three vertical regions stacked under a shared title bar:
//!
//! 1. **List** (top): one row per quarantined envelope across every
//!    agent's `inbox/quarantine/` directory, with columns ARRIVED,
//!    RECIPIENT, SENDER, REASON.
//! 2. **Envelope details** (middle): metadata about the focused row.
//!    `message_id`, `sender_id`, `created_at`, and the raw reason
//!    token from the filename suffix.
//! 3. **Body** (bottom): the envelope's raw body as UTF-8 text. A
//!    `[non-UTF-8 body]` marker appears when the bytes didn't decode
//!    cleanly.
//!
//! The bottom row is a key-hint footer plus the discard confirmation
//! prompt when one is open.
//!
//! Aesthetic continuity with [`crate::ui`], [`crate::ui_panopticon`],
//! and [`crate::ui_inspect`] is intentional — same `reeve · …` prefix,
//! same `NO_COLOR` fallback contract, same footer-hint shape. The
//! Phase 6 stub used to live here; Phase 8 replaces it with the real
//! review UI.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::quarantine_view::{EnvelopeMeta, QuarantineEntry, QuarantineSnapshot};
use crate::state::AppState;
use crate::ui_common::{build_section_header, format_time_hhmm_opt, no_color, pad_right};

/// Column widths for the list section. Sums to 80 minus the
/// inter-column separators so an 80-wide terminal fits the canonical
/// layout without truncation. The renderer pads/truncates each cell to
/// the corresponding width before assembling the row.
const COL_ARRIVED: usize = 7;
const COL_RECIPIENT: usize = 16;
const COL_SENDER: usize = 16;
// REASON column fills the remainder; no fixed width.

/// Render the quarantine review screen into `frame`.
pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let width = area.width;
    let snap = &state.quarantine;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // list header
            Constraint::Min(3),    // list
            Constraint::Length(1), // envelope header sep
            Constraint::Length(6), // envelope details
            Constraint::Length(1), // body header sep
            Constraint::Min(2),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    frame.render_widget(Paragraph::new(build_title_bar(snap, width)), chunks[0]);
    frame.render_widget(Paragraph::new(build_list_header(width)), chunks[1]);
    frame.render_widget(
        Paragraph::new(build_list_rows(snap, state.quarantine_focus)),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(build_section_header("envelope", width)),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(build_envelope_details(focused(
            snap,
            state.quarantine_focus,
        ))),
        chunks[4],
    );
    frame.render_widget(
        Paragraph::new(build_section_header("body", width)),
        chunks[5],
    );
    frame.render_widget(
        Paragraph::new(build_body_pane(focused(snap, state.quarantine_focus))),
        chunks[6],
    );
    frame.render_widget(
        Paragraph::new(build_footer(state.quarantine_confirm_discard)),
        chunks[7],
    );
}

/// Title bar: `reeve · quarantine ─── N quarantined`.
fn build_title_bar(snap: &QuarantineSnapshot, width: u16) -> Line<'static> {
    let prefix_style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::Blue)
    };
    let prefix = Span::styled("reeve \u{00B7} quarantine ".to_owned(), prefix_style);
    let count = snap.entries.len();
    let suffix_text = if snap.truncated {
        format!("\u{2500}\u{2500}\u{2500} {count}+ quarantined (truncated)")
    } else {
        format!("\u{2500}\u{2500}\u{2500} {count} quarantined")
    };
    // Pad to terminal width so the rule extends to the right edge.
    // Subtract 1 for the separator space printed between suffix_text
    // and padding in the format string below (`"{suffix_text} {padding}"`).
    let pad_len = usize::from(width)
        .saturating_sub("reeve \u{00B7} quarantine ".chars().count())
        .saturating_sub(suffix_text.chars().count())
        .saturating_sub(1);
    let padding = "\u{2500}".repeat(pad_len);
    Line::from(vec![prefix, Span::raw(format!("{suffix_text} {padding}"))])
}

/// Column header line above the list.
fn build_list_header(width: u16) -> Line<'static> {
    let header = format!(
        "  {arrived:<aw$}  {recipient:<rw$}  {sender:<sw$}  REASON",
        arrived = "ARRIVED",
        aw = COL_ARRIVED,
        recipient = "RECIPIENT",
        rw = COL_RECIPIENT,
        sender = "SENDER",
        sw = COL_SENDER,
    );
    // Trim or pad to the visible width so the underline-style header
    // matches the panopticon's column-header treatment.
    let header = pad_right(&header, usize::from(width));
    let style = if no_color() {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::DarkGray)
    };
    Line::from(Span::styled(header, style))
}

/// Render the list rows. Each entry gets one line; the focused row is
/// prefixed with `▶ ` and styled with the active row colour.
fn build_list_rows(snap: &QuarantineSnapshot, focus: usize) -> Vec<Line<'static>> {
    if snap.entries.is_empty() {
        let dim = if no_color() {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        return vec![
            Line::from(""),
            Line::from(Span::styled("  no quarantined messages".to_owned(), dim)),
        ];
    }
    snap.entries
        .iter()
        .enumerate()
        .map(|(i, entry)| build_list_row(entry, i == focus))
        .collect()
}

fn build_list_row(entry: &QuarantineEntry, focused: bool) -> Line<'static> {
    let cursor = if focused { "\u{25B6} " } else { "  " };
    let arrived = format_time_hhmm_opt(entry.arrived);
    let arrived_cell = pad_right(&arrived, COL_ARRIVED);
    let recipient_cell = pad_right(&entry.recipient, COL_RECIPIENT);
    let sender_cell = pad_right(&sender_label(&entry.meta), COL_SENDER);
    let text = format!(
        "{cursor}{arrived_cell}  {recipient_cell}  {sender_cell}  {reason}",
        reason = entry.reason,
    );
    let style = if focused && !no_color() {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::Cyan)
    } else {
        Style::default()
    };
    Line::from(Span::styled(text, style))
}

/// Sender label for the list row. Truncates the full `UUIDv7` to its
/// leading 8 hex characters so the column stays scannable; the full
/// id is shown in the envelope details pane below. `"unknown"` is
/// reserved for parse-failure entries where no `sender_id` was
/// extractable.
fn sender_label(meta: &EnvelopeMeta) -> String {
    match meta {
        EnvelopeMeta::Parsed { sender_id, .. } => short_id(*sender_id),
        EnvelopeMeta::ParseFailure { .. } => "unknown".to_owned(),
    }
}

fn short_id(id: reeve_types::IdentityId) -> String {
    let s = id.to_string();
    s.split('-').next().unwrap_or(&s).to_owned()
}

/// Envelope-details body. Six lines: blank, `message_id`, `sender_id`,
/// `recipient_id`, `created_at`, verification. Parse-failure entries
/// surface a single explanatory line so the operator can still
/// discard the file.
fn build_envelope_details(entry: Option<&QuarantineEntry>) -> Vec<Line<'static>> {
    let dim = if no_color() {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let Some(entry) = entry else {
        return vec![
            Line::from(""),
            Line::from(Span::styled("  (no entry selected)".to_owned(), dim)),
        ];
    };
    match &entry.meta {
        EnvelopeMeta::Parsed {
            message_id,
            sender_id,
            recipient_id,
            created_at,
        } => vec![
            Line::from(""),
            Line::from(format!("  message_id    {message_id}")),
            Line::from(format!("  sender_id     {sender_id}")),
            Line::from(format!("  recipient_id  {recipient_id}")),
            Line::from(format!("  created_at    {}", format_rfc_short(*created_at))),
            Line::from(format!("  reason        {}", entry.reason)),
        ],
        EnvelopeMeta::ParseFailure { filename } => vec![
            Line::from(""),
            Line::from(format!("  filename      {filename}")),
            Line::from(format!("  reason        {}", entry.reason)),
            Line::from(Span::styled(
                "  parse_failure: the envelope JSON could not be decoded.".to_owned(),
                dim,
            )),
            Line::from(""),
            Line::from(""),
        ],
    }
}

/// Format a timestamp as `YYYY-MM-DD HH:MM:SSZ` — enough resolution
/// for the operator to correlate with logs, no more.
fn format_rfc_short(ts: time::OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        ts.year(),
        u8::from(ts.month()),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second(),
    )
}

/// Body pane: raw envelope body as text, plus a `[non-UTF-8 body]`
/// marker if the original bytes didn't decode cleanly.
fn build_body_pane(entry: Option<&QuarantineEntry>) -> Vec<Line<'static>> {
    let Some(entry) = entry else {
        return vec![Line::from("")];
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    if entry.body_lossy {
        let warn = if no_color() {
            Style::default().add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(Color::Yellow)
        };
        lines.push(Line::from(Span::styled(
            "  [non-UTF-8 body — lossy conversion]".to_owned(),
            warn,
        )));
    }
    if entry.raw_body.is_empty() {
        let dim = if no_color() {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        lines.push(Line::from(Span::styled("  (empty body)".to_owned(), dim)));
    } else {
        for line in entry.raw_body.lines() {
            lines.push(Line::from(format!("  {line}")));
        }
    }
    lines
}

/// Footer line. When a discard confirmation is open the prompt
/// replaces the help text so the operator's eye lands on the
/// pending action.
fn build_footer(confirm_discard: bool) -> Line<'static> {
    if confirm_discard {
        let warn = if no_color() {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Red)
        };
        Line::from(Span::styled(
            "discard this entry? press d or y to confirm, any other key to cancel".to_owned(),
            warn,
        ))
    } else {
        Line::from(
            "d discard \u{00B7} o convert \u{00B7} j/k navigate \u{00B7} Tab back \u{00B7} q quit"
                .to_owned(),
        )
    }
}

/// Return the focused entry, or `None` when the focus index is out
/// of range (empty list, or stale focus during a transient refresh).
fn focused(snap: &QuarantineSnapshot, focus: usize) -> Option<&QuarantineEntry> {
    snap.entries.get(focus)
}

/// Render the quarantine compose surface into `frame`.
///
/// Presents a two-row header (title + separator), a single-line input
/// prompt, an editor area (the compose buffer, pre-filled with the
/// quarantined body), and a footer with the submit/cancel hints. The
/// operator can type freely; `Enter` submits, `Esc`/`Tab` cancel.
pub fn draw_compose(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let width = area.width;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Length(1), // separator
            Constraint::Length(1), // prompt label
            Constraint::Min(3),    // compose body
            Constraint::Length(1), // footer
        ])
        .split(area);

    // Title bar.
    let prefix_style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::Blue)
    };
    let recipient = &state.quarantine_compose_recipient;
    let title = Line::from(vec![
        Span::styled(
            "reeve \u{00B7} quarantine \u{00B7} compose ".to_owned(),
            prefix_style,
        ),
        Span::raw(format!("\u{2500}\u{2500}\u{2500} to: {recipient}")),
    ]);
    frame.render_widget(Paragraph::new(title), chunks[0]);
    frame.render_widget(
        Paragraph::new(build_section_header("message", width)),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from("  compose new message:")),
        chunks[2],
    );

    let body_text = format!("  {}", state.input);
    frame.render_widget(Paragraph::new(body_text), chunks[3]);

    // Footer.
    frame.render_widget(
        Paragraph::new(Line::from(
            "Enter send \u{00B7} Esc cancel \u{00B7} Backspace delete".to_owned(),
        )),
        chunks[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quarantine_view::{EnvelopeMeta, QuarantineEntry, QuarantineSnapshot};
    use crate::state::{AppState, Screen};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use time::OffsetDateTime;

    fn entry_for(recipient: &str, reason: &str, body: &str) -> QuarantineEntry {
        QuarantineEntry {
            path: PathBuf::from(format!("/x/{recipient}/{reason}")),
            recipient: recipient.to_owned(),
            arrived: Some(
                OffsetDateTime::from_unix_timestamp(1_716_700_000).expect("fixture timestamp"),
            ),
            reason: reason.to_owned(),
            meta: EnvelopeMeta::ParseFailure {
                filename: format!("{recipient}-stem.{reason}"),
            },
            raw_body: body.to_owned(),
            body_lossy: false,
        }
    }

    fn state_with_entries(entries: Vec<QuarantineEntry>, focus: usize) -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Quarantine;
        state.quarantine = QuarantineSnapshot {
            entries,
            truncated: false,
        };
        state.quarantine_focus = focus;
        state
    }

    fn render(state: &AppState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, state)).expect("draw");
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

    // Q1: empty list renders the explicit "no quarantined messages"
    // line rather than an empty pane.
    #[test]
    fn empty_list_renders_explicit_message() {
        std::env::set_var("NO_COLOR", "1");
        let state = state_with_entries(Vec::new(), 0);
        let rendered = render(&state, 80, 24);
        std::env::remove_var("NO_COLOR");
        assert!(
            rendered.contains("no quarantined messages"),
            "empty list message missing: {rendered}"
        );
    }

    // Q2: a single entry shows up with reason, recipient, body — the
    // load-bearing review-screen content. Done-when 1.
    #[test]
    fn single_entry_shows_metadata_and_body() {
        std::env::set_var("NO_COLOR", "1");
        let state = state_with_entries(
            vec![entry_for(
                "lead",
                "signature_invalid",
                "hello world payload",
            )],
            0,
        );
        let rendered = render(&state, 80, 24);
        std::env::remove_var("NO_COLOR");
        assert!(rendered.contains("lead"), "recipient missing: {rendered}");
        assert!(
            rendered.contains("signature_invalid"),
            "reason missing: {rendered}"
        );
        assert!(
            rendered.contains("hello world payload"),
            "body missing: {rendered}"
        );
    }

    // Q3: focused row gets the ▶ cursor; other rows don't.
    #[test]
    fn cursor_marks_focused_row_only() {
        std::env::set_var("NO_COLOR", "1");
        let state = state_with_entries(
            vec![
                entry_for("lead", "replay", "first"),
                entry_for("worker-x", "clock_skew", "second"),
            ],
            1,
        );
        let rendered = render(&state, 80, 24);
        std::env::remove_var("NO_COLOR");
        // The ▶ should appear once.
        let cursor_count = rendered.matches('\u{25B6}').count();
        assert_eq!(
            cursor_count, 1,
            "expected exactly one ▶ cursor; got {cursor_count}: {rendered}"
        );
    }

    // Q4: discard confirmation replaces the help footer with the
    // prompt the operator must answer.
    #[test]
    fn confirm_discard_replaces_footer_with_prompt() {
        std::env::set_var("NO_COLOR", "1");
        let mut state = state_with_entries(vec![entry_for("lead", "replay", "x")], 0);
        state.quarantine_confirm_discard = true;
        let rendered = render(&state, 80, 24);
        std::env::remove_var("NO_COLOR");
        assert!(
            rendered.contains("discard this entry?"),
            "confirm prompt missing: {rendered}"
        );
        // The standard footer hint should be hidden while confirming.
        assert!(
            !rendered.contains("d discard"),
            "default footer leaked through confirm: {rendered}"
        );
    }

    // Q5: 80x24 NO_COLOR smoke — title, list, body, footer all on
    // screen. Same shape as the panopticon and inspect smokes.
    #[test]
    fn draw_renders_at_80x24() {
        std::env::set_var("NO_COLOR", "1");
        let state = state_with_entries(
            vec![entry_for("lead", "clock_skew", "test message body")],
            0,
        );
        let rendered = render(&state, 80, 24);
        std::env::remove_var("NO_COLOR");
        assert!(rendered.contains("quarantine"), "title missing");
        assert!(rendered.contains("REASON"), "list header missing");
        assert!(rendered.contains("Tab back"), "footer missing");
    }
}
