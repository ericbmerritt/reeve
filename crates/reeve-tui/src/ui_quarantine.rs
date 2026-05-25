//! Quarantine review screen — Phase 6 stub.
//!
//! Phase 6 wires the `Q` keybinding from the panopticon to this screen so
//! the operator's muscle memory has somewhere to land. The full review UI
//! (per-message inspection, approve/release/discard actions) lands in
//! Phase 8 alongside the gatekeeper work; until then, this renderer
//! surfaces the count the panopticon already tracks and tells the operator
//! the review screen itself is not yet built.
//!
//! Aesthetic continuity with [`crate::ui`] and [`crate::ui_panopticon`] is
//! intentional — same `reeve · …` title prefix, same `NO_COLOR` fallback
//! contract, same footer-hint shape. When the Phase 8 renderer arrives it
//! drops into this module and inherits the chrome.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::panopticon::PanopticonSnapshot;
use crate::ui_common::no_color;

/// Render the quarantine review stub into `frame`.
pub fn draw(frame: &mut Frame<'_>, snap: &PanopticonSnapshot) {
    let area = frame.area();
    let width = area.width;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Length(1), // section header
            Constraint::Length(1), // count line
            Constraint::Min(1),    // placeholder body
            Constraint::Length(1), // footer
        ])
        .split(area);

    frame.render_widget(Paragraph::new(build_title_bar(snap)), chunks[0]);
    frame.render_widget(
        Paragraph::new(build_section_header("quarantine", width)),
        chunks[1],
    );
    frame.render_widget(Paragraph::new(build_count_line(snap)), chunks[2]);
    frame.render_widget(Paragraph::new(build_placeholder_body()), chunks[3]);
    frame.render_widget(Paragraph::new(build_footer()), chunks[4]);
}

/// Title bar matches the panopticon's: `reeve · quarantine ─── $X.XX`.
fn build_title_bar(snap: &PanopticonSnapshot) -> Line<'static> {
    let prefix_style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::Blue)
    };
    let prefix = Span::styled("reeve \u{00B7} quarantine ".to_owned(), prefix_style);
    let cost = format!("${:.2}", snap.total_cost_usd);
    let suffix = format!("\u{2500}\u{2500}\u{2500} {cost}");
    Line::from(vec![prefix, Span::raw(suffix)])
}

/// Section header `─ quarantine ─────`. Same shape as the panopticon's
/// section headers; intentionally shared visual rhythm.
fn build_section_header(label: &str, width: u16) -> Line<'static> {
    let lead = format!("\u{2500} {label} ");
    let pad = usize::from(width).saturating_sub(lead.chars().count());
    let rule: String = "\u{2500}".repeat(pad);
    Line::from(format!("{lead}{rule}"))
}

/// Count line — the actionable piece of information this screen can
/// truthfully show today.
fn build_count_line(snap: &PanopticonSnapshot) -> Line<'static> {
    let count = snap.queue_counts.quarantine;
    let text = match count {
        0 => "  no quarantined messages".to_owned(),
        1 => "  1 quarantined message across all agents".to_owned(),
        n => format!("  {n} quarantined messages across all agents"),
    };
    Line::from(text)
}

/// Placeholder body — explicit about what is and is not built yet so the
/// operator does not stare at an empty screen wondering what they missed.
fn build_placeholder_body() -> Vec<Line<'static>> {
    let dim = if no_color() {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    vec![
        Line::from(""),
        Line::from(Span::styled(
            "  The full quarantine review screen lands in Phase 8 alongside the".to_owned(),
            dim,
        )),
        Line::from(Span::styled(
            "  gatekeeper work. Each agent's `inbox/quarantine/` directory is".to_owned(),
            dim,
        )),
        Line::from(Span::styled(
            "  the source of truth in the meantime — `ls` and `cat` work.".to_owned(),
            dim,
        )),
    ]
}

/// Footer mirrors the panopticon's `·` separators and key hints.
fn build_footer() -> Line<'static> {
    Line::from("Esc back \u{00B7} Tab panopticon \u{00B7} Q close \u{00B7} q quit".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panopticon::{PanopticonSnapshot, QueueCounts};

    fn snap_with_quarantine(count: usize) -> PanopticonSnapshot {
        PanopticonSnapshot {
            queue_counts: QueueCounts {
                quarantine: count,
                ..QueueCounts::default()
            },
            ..PanopticonSnapshot::default()
        }
    }

    // Q1: zero quarantined messages renders as the explicit no-messages
    // line rather than `0`. Empty-state phrasing matters when the count
    // is the only operational data on the screen.
    #[test]
    fn count_line_zero_uses_no_messages_phrasing() {
        let line = build_count_line(&snap_with_quarantine(0));
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            text.contains("no quarantined"),
            "zero-count line should read 'no quarantined…'; got {text:?}"
        );
    }

    // Q2: a positive count is rendered with the number embedded so the
    // operator does not need to switch screens to know how many entries
    // are waiting.
    #[test]
    fn count_line_positive_count_embeds_number() {
        let line = build_count_line(&snap_with_quarantine(7));
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            text.contains('7'),
            "positive-count line must show the count; got {text:?}"
        );
    }

    // Q3: full draw at 24-row × 80-col puts the operator-facing message,
    // the count line, and the footer keymap on-screen. The stub does not
    // need anything more interactive than this until Phase 8.
    #[test]
    fn draw_renders_stub_at_80x24() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let snap = snap_with_quarantine(3);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &snap)).unwrap();

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
            rendered.contains("quarantine"),
            "title or header missing; got:\n{rendered}"
        );
        assert!(
            rendered.contains("Phase 8"),
            "operator-facing 'phase 8' note missing; got:\n{rendered}"
        );
        assert!(
            rendered.contains("Esc back"),
            "footer key hints missing; got:\n{rendered}"
        );
    }
}
