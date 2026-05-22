//! Lead chat screen renderer for the Reeve TUI.
//!
//! [`draw`] translates an [`AppState`] snapshot into ratatui widgets. Every
//! function in this module is pure: no I/O, no side effects, only layout
//! arithmetic and widget construction. The caller owns terminal state.
//!
//! Layout (top to bottom):
//! 1. Title bar — 1 row: agent identity, model, and status.
//! 2. Conversation pane — fills available height.
//! 3. Separator — 1 row.
//! 4. Input line — 1 row.
//! 5. Footer — 1 row.
//!
//! Color contract (from `specs/reeve-tui-design.md`):
//! - `AgentStatus::Working` → Yellow fg on title sigil.
//! - `AgentStatus::Crashed` → Red fg on title sigil.
//! - `AgentStatus::Idle` → no color.
//! - `AgentStatus::Unknown` → no color.
//! - `NO_COLOR=1` → all color styling suppressed; sigils carry full meaning.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::state::{AgentStatus, AppState};

/// Return true when `NO_COLOR` is set in the environment (any value).
///
/// Called once per draw pass. The `std::env` call is cheap at this cadence.
fn no_color() -> bool {
    std::env::var("NO_COLOR").is_ok()
}

/// Status sigil for a given [`AgentStatus`].
///
/// Sigils carry full meaning under `NO_COLOR=1` (no color needed separately).
fn status_sigil(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "○",
        AgentStatus::Working => "●",
        AgentStatus::Crashed => "✗",
        AgentStatus::Unknown => "?",
    }
}

/// Foreground color for the status sigil. Returns `None` under `NO_COLOR`.
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

/// Human-readable status text following the sigil in the title bar.
fn status_text(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Crashed => "crashed",
        AgentStatus::Unknown => "unknown",
    }
}

/// Format: `reeve · lead ({persona}) ─── {model} · {sigil} {status}`
fn build_title_bar(state: &AppState) -> Line<'static> {
    let sigil = status_sigil(&state.status);
    let status = status_text(&state.status);

    let prefix_style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::Blue)
    };
    let prefix = Span::styled("reeve · ".to_owned(), prefix_style);

    let lead_part = Span::raw(format!(
        "lead ({}) \u{2500}\u{2500}\u{2500} {} \u{00B7} ",
        state.persona_name, state.model_id,
    ));

    let sigil_style = status_color(&state.status)
        .map(|c| Style::default().fg(c))
        .unwrap_or_default();
    let sigil_span = Span::styled(sigil.to_owned(), sigil_style);

    let status_span = Span::raw(format!(" {status}"));

    Line::from(vec![prefix, lead_part, sigil_span, status_span])
}

/// Format an optional timestamp as `HH:MM`, or return an empty string.
fn format_timestamp(ts: Option<time::OffsetDateTime>) -> String {
    ts.map(|t| format!("{:02}:{:02}", t.hour(), t.minute()))
        .unwrap_or_default()
}

/// Build the conversation lines from all entries in state.
///
/// Each entry renders as two parts:
/// 1. Speaker line: `{label} · {timestamp_or_blank}`
/// 2. Text line:   `  {text}`
fn build_conversation_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    for entry in &state.conversation {
        let label = entry.speaker_label(&state.persona_name, state.operator_id);
        let ts = format_timestamp(entry.timestamp);

        let speaker_line = if ts.is_empty() {
            Line::from(label)
        } else {
            Line::from(format!("{label} \u{00B7} {ts}"))
        };
        lines.push(speaker_line);

        let text_line = Line::from(format!("  {}", entry.text));
        lines.push(text_line);

        lines.push(Line::from(""));
    }

    lines
}

fn build_separator(width: u16) -> Line<'static> {
    let bar: String = "\u{2500}".repeat(usize::from(width));
    Line::from(bar)
}

/// Format: `> {text}_` (underscore simulates cursor at end).
fn build_input_line(state: &AppState) -> Line<'static> {
    Line::from(format!("> {}_", state.input))
}

/// Format: `Tab panopticon · ? help · /search · q quit ─── ${cost:.4} USD`
fn build_footer(state: &AppState) -> Line<'static> {
    let nav =
        "Tab panopticon \u{00B7} ? help \u{00B7} /search \u{00B7} q quit \u{2500}\u{2500}\u{2500} ";
    let cost = format!("${:.4} USD", state.cost_usd);
    Line::from(format!("{nav}{cost}"))
}

/// Render the lead chat screen into `frame`.
///
/// Layout: title (1) | conversation (flex) | separator (1) | input (1) | footer (1).
pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(0),    // conversation (flex)
            Constraint::Length(1), // separator
            Constraint::Length(1), // input
            Constraint::Length(1), // footer
        ])
        .split(area);

    let title_line = build_title_bar(state);
    let title_widget = Paragraph::new(Text::from(vec![title_line]));
    frame.render_widget(title_widget, chunks[0]);

    let conv_lines = build_conversation_lines(state);
    let conv_widget = render_conversation(conv_lines, chunks[1]);
    frame.render_widget(conv_widget, chunks[1]);

    let sep_line = build_separator(chunks[2].width);
    let sep_widget = Paragraph::new(Text::from(vec![sep_line]));
    frame.render_widget(sep_widget, chunks[2]);

    let input_line = build_input_line(state);
    let input_widget = Paragraph::new(Text::from(vec![input_line]));
    frame.render_widget(input_widget, chunks[3]);

    let footer_line = build_footer(state);
    let footer_widget = Paragraph::new(Text::from(vec![footer_line]));
    frame.render_widget(footer_widget, chunks[4]);
}

/// Wrap conversation lines in a scrolled-to-bottom `Paragraph`.
///
/// If the conversation is taller than the available area, scroll so the
/// most recent entries are visible.
fn render_conversation(
    lines: Vec<Line<'static>>,
    area: ratatui::layout::Rect,
) -> Paragraph<'static> {
    let total = lines.len();
    let visible = usize::from(area.height);
    let scroll = if total > visible {
        u16::try_from(total - visible).unwrap_or(u16::MAX)
    } else {
        0
    };
    Paragraph::new(Text::from(lines)).scroll((scroll, 0))
}
