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
use ratatui::widgets::{Paragraph, Wrap};
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
/// 2. Text line:   `  {text}` (possibly multiple visual rows after wrapping)
///
/// When the agent is in `Working` status, appends a "thinking" indicator
/// with a clock-driven spinner so a long model call does not look like the
/// TUI is frozen.
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

        // Preserve text line-breaks the agent put in the payload; wrapping is
        // handled by the Paragraph widget at render time.
        for line in entry.text.lines() {
            lines.push(Line::from(format!("  {line}")));
        }
        if entry.text.is_empty() {
            lines.push(Line::from("  "));
        }

        lines.push(Line::from(""));
    }

    if let Some(line) = thinking_indicator(state) {
        lines.push(line);
    }

    lines
}

/// 10-phase Braille dot spinner. Phase advances ~8 frames per second based on
/// the wall clock so the indicator animates without needing tick state in
/// `AppState`; every `draw` call recomputes from `Instant::now`.
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// When the lead agent is `Working`, return a styled "thinking" line for the
/// conversation pane. Returns `None` in every other state so an idle agent
/// shows nothing extra below the last conversation entry.
fn thinking_indicator(state: &AppState) -> Option<Line<'static>> {
    if state.status != AgentStatus::Working {
        return None;
    }
    // ~125ms per frame. SystemTime is fine here: the spinner is an animation,
    // not a security boundary, and a clock step would just resync the phase.
    let millis_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let frames_len = u128::try_from(SPINNER_FRAMES.len()).unwrap_or(u128::MAX);
    let phase = usize::try_from((millis_since_epoch / 125) % frames_len).unwrap_or(0);
    let frame = SPINNER_FRAMES[phase];
    let style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::Yellow)
    };
    Some(Line::from(vec![
        Span::styled(format!("  {frame} "), style),
        Span::styled("thinking…".to_owned(), style),
    ]))
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
/// Word-wraps long entries so the model's responses (which can easily exceed
/// 200 columns) stay visible — without wrap, a single overflowing line would
/// render as a single chopped-off row. `trim: false` preserves the two-space
/// text indent and any leading whitespace inside model responses.
///
/// Scroll calculation approximates the post-wrap row count via
/// [`count_wrapped_rows`]: ratatui's exact `Paragraph::line_count` API is
/// behind an unstable feature in 0.29, and we'd rather not opt in to a
/// future-breaking signature for a UI nicety. The ceiling-divide
/// approximation overcounts by at most one row per logical line, which
/// errs on the side of "scroll just a bit too far" rather than clipping
/// recent entries off the bottom.
fn render_conversation(
    lines: Vec<Line<'static>>,
    area: ratatui::layout::Rect,
) -> Paragraph<'static> {
    let total = count_wrapped_rows(&lines, area.width);
    let visible = usize::from(area.height);
    let scroll = total
        .saturating_sub(visible)
        .try_into()
        .unwrap_or(u16::MAX);
    Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

/// Approximate the number of terminal rows that `lines` will occupy after
/// word-wrap at `width` columns. Empty lines count as one row each. The
/// approximation is the ceiling-divide of each line's visual width by the
/// render width; it overcounts by at most one row per line versus ratatui's
/// real wrap implementation (which is word-aware), and never undercounts,
/// so the auto-scroll target never clips the latest entry off the bottom.
fn count_wrapped_rows(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| {
            let len = line.width();
            if len == 0 {
                1
            } else {
                len.div_ceil(width)
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    // U1: count_wrapped_rows returns 1 per empty line and ceil-divide for
    // overflowing lines. Regression guard for the auto-scroll calculation.
    #[test]
    fn count_wrapped_rows_handles_empty_and_overflow() {
        let lines = vec![
            Line::from(""),                            // 1 row
            Line::from("short"),                       // 1 row (5 chars)
            Line::from("x".repeat(80)),                // 4 rows at width=20
            Line::from("y".repeat(21)),                // 2 rows at width=20
        ];
        assert_eq!(count_wrapped_rows(&lines, 20), 1 + 1 + 4 + 2);
    }

    // U2: zero render width is treated as width=1 so we never divide by zero
    // and the function still terminates with a sensible upper bound.
    #[test]
    fn count_wrapped_rows_zero_width_is_safe() {
        let lines = vec![Line::from("hello")];
        // width treated as 1, so 5 chars → 5 rows
        assert_eq!(count_wrapped_rows(&lines, 0), 5);
    }

    // U3: thinking_indicator is None unless the agent is Working — idle / crashed
    // / unknown agents should not show the spinner.
    #[test]
    fn thinking_indicator_only_when_working() {
        let mut state = AppState::default();
        state.status = AgentStatus::Idle;
        assert!(thinking_indicator(&state).is_none());
        state.status = AgentStatus::Crashed;
        assert!(thinking_indicator(&state).is_none());
        state.status = AgentStatus::Unknown;
        assert!(thinking_indicator(&state).is_none());
        state.status = AgentStatus::Working;
        assert!(thinking_indicator(&state).is_some());
    }
}
