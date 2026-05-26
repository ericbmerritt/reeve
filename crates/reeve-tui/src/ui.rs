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
//! - Conversation roles: operator (you) → Yellow; lead persona → Cyan; other
//!   agents → Magenta; system → `DarkGray`. The three speaker hues sit on
//!   different points of the color wheel so adjacent entries from different
//!   speakers are scannable at a glance. Body rows use the same hue with the
//!   `DIM` modifier so the role color does not dominate the pane.
//! - `NO_COLOR=1` → all color styling suppressed; sigils and speaker labels
//!   carry full meaning.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{AgentStatus, AppState, ConversationEntry, EntryKind};

use crate::ui_common::no_color;

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

/// Format: `reeve · {agent} ({persona}) ─── {model} · {sigil} {status}`
///
/// The agent slot is the role name from `state.chat_agent_name` — `"lead"`
/// for the lead, the worker's registered name (e.g. `"worker-2e28aff5"`)
/// when the operator is in a worker's chat after Enter-ing its panopticon
/// row. Hardcoding `"lead"` here would mislabel every non-lead chat: the
/// body would be the worker's, but the title would still claim lead.
fn build_title_bar(state: &AppState) -> Line<'static> {
    let sigil = status_sigil(&state.status);
    let status = status_text(&state.status);

    let prefix_style = if no_color() {
        Style::default()
    } else {
        Style::default().fg(Color::Blue)
    };
    let prefix = Span::styled("reeve · ".to_owned(), prefix_style);

    let agent_part = Span::raw(format!(
        "{} ({}) \u{2500}\u{2500}\u{2500} {} \u{00B7} ",
        state.chat_agent_name, state.persona_name, state.model_id,
    ));

    let sigil_style = status_color(&state.status)
        .map(|c| Style::default().fg(c))
        .unwrap_or_default();
    let sigil_span = Span::styled(sigil.to_owned(), sigil_style);

    let status_span = Span::raw(format!(" {status}"));

    let mut spans = vec![prefix, agent_part, sigil_span, status_span];
    if !state.is_at_bottom() {
        // Surface scroll position so the operator knows new entries are
        // arriving above their viewport and how to get back. End is the
        // shortest path; PageDown is documented in the footer.
        let scroll_style = if no_color() {
            Style::default()
        } else {
            Style::default().fg(Color::Cyan)
        };
        spans.push(Span::styled(
            format!(
                " \u{00B7} \u{2191} scrolled {} (End to bottom)",
                state.scroll_offset
            ),
            scroll_style,
        ));
    }
    Line::from(spans)
}

/// Color used for the speaker tag and body indent of a conversation entry.
///
/// - Operator (you) → yellow; the warmest hue, so operator input pops against
///   the agent palette.
/// - Lead persona (Outbound) → cyan.
/// - Other agents (Inbound from non-operator) → magenta.
/// - System annotations → dim gray.
///
/// Yellow / Cyan / Magenta sit at ~120° apart on the color wheel, so adjacent
/// entries from different speakers stay visually separable. Yellow also
/// double-duties as the `Working` status sigil in the title bar — different
/// region, so the reuse is intentional rather than confusing.
///
/// `NO_COLOR=1` collapses every role to default style; the textual speaker
/// label still distinguishes them.
fn role_style(entry: &ConversationEntry, operator_id: Option<reeve_types::IdentityId>) -> Style {
    if no_color() {
        return Style::default();
    }
    match entry.kind {
        EntryKind::Outbound => Style::default().fg(Color::Cyan),
        EntryKind::System => Style::default().fg(Color::DarkGray),
        EntryKind::Inbound => match entry.sender_id {
            Some(id) if Some(id) == operator_id => Style::default().fg(Color::Yellow),
            _ => Style::default().fg(Color::Magenta),
        },
    }
}

/// Two-space indent prepended to each body row of a conversation entry,
/// hanging under the speaker tag. The renderer also pre-wraps long body
/// lines so continuation rows keep this indent — ratatui's
/// `Wrap { trim: false }` preserves it on the first row but drops it on
/// continuations.
const BODY_INDENT: &str = "  ";

/// Build the conversation lines from all entries in state.
///
/// Each entry renders as two parts:
/// 1. Speaker line: `{label} · {timestamp_or_blank}`
/// 2. Text line:   `  {text}` (pre-wrapped against `width` so continuation
///    rows keep the two-space hang indent)
///
/// Both parts are colored by the entry's role (see `role_style`) so the
/// operator can scan who said what without reading the speaker tag. Body
/// rows are dimmed relative to the speaker tag to keep the role color from
/// dominating the pane.
///
/// Body wrapping happens here rather than via ratatui's `Wrap { trim: false }`
/// because that wrap mode preserves leading whitespace only on a line's
/// first visual row; continuation rows start at column 0 and lose the
/// indent. Pre-wrapping with explicit indent on every row keeps the
/// hang-indent stable through any number of wrap continuations.
///
/// When the agent is in `Working` status, appends a "thinking" indicator
/// with a clock-driven spinner so a long model call does not look like the
/// TUI is frozen.
fn build_conversation_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    for entry in &state.conversation {
        let label = entry.speaker_label(&state.persona_name, state.operator_id);
        let ts = crate::ui_common::format_time_hhmm_opt(entry.timestamp);
        let style = role_style(entry, state.operator_id);
        let body_style = if no_color() {
            Style::default()
        } else {
            style.add_modifier(ratatui::style::Modifier::DIM)
        };

        let speaker_text = if ts.is_empty() {
            label
        } else {
            format!("{label} \u{00B7} {ts}")
        };
        lines.push(Line::from(Span::styled(speaker_text, style)));

        // Preserve text line-breaks the agent put in the payload, then
        // pre-wrap each one against the chunk width with the body indent.
        for line in entry.text.lines() {
            for visual_row in wrap_body_line(line, width) {
                lines.push(Line::from(Span::styled(visual_row, body_style)));
            }
        }
        if entry.text.is_empty() {
            lines.push(Line::from(BODY_INDENT));
        }

        lines.push(Line::from(""));
    }

    if let Some(line) = thinking_indicator(state) {
        lines.push(line);
    }

    lines
}

/// Word-wrap `text` against `width`, prepending [`BODY_INDENT`] to each
/// visual row. Naive word-boundary wrap; words longer than the content
/// width fall onto a single row without splitting (rare in chat prose
/// and visually less harmful than mid-word splits in monospace).
fn wrap_body_line(text: &str, width: u16) -> Vec<String> {
    let indent_chars = BODY_INDENT.chars().count();
    // Clamp the content budget. At very narrow widths there's nothing
    // sensible to do; emit the text without wrapping rather than producing
    // an empty-content cascade.
    let content_width = usize::from(width).saturating_sub(indent_chars).max(1);

    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return vec![BODY_INDENT.to_owned()];
    }

    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in trimmed.split_whitespace() {
        let word_chars = word.chars().count();
        let projected = if current.is_empty() {
            word_chars
        } else {
            current.chars().count() + 1 + word_chars
        };
        if projected > content_width && !current.is_empty() {
            rows.push(format!("{BODY_INDENT}{current}"));
            current.clear();
            current.push_str(word);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        rows.push(format!("{BODY_INDENT}{current}"));
    }
    rows
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

/// Format: `PgUp/PgDn scroll · End bottom · q quit ─── ${cost:.2}`.
///
/// Cost format matches the panopticon's title-bar `${:.2}` — the screens
/// share a glance-level view of session spend, so the format follows. The
/// old `${:.4} USD` (four decimal places, with currency suffix) was an
/// outlier inherited from the walking-skeleton stub.
fn build_footer(state: &AppState) -> Line<'static> {
    let nav = "PgUp/PgDn scroll \u{00B7} End bottom \u{00B7} q quit \u{2500}\u{2500}\u{2500} ";
    let cost = format!("${:.2}", state.cost_usd);
    Line::from(format!("{nav}{cost}"))
}

/// Render the lead chat screen into `frame`.
///
/// Layout: title (1) | conversation (flex) | separator (1) | input (variable) | footer (1).
///
/// The input chunk grows to fit wrapped text up to half the screen height,
/// then stops growing (the conversation keeps at least half the area). The
/// height is computed by asking the same wrapped `Paragraph` we render how
/// many rows it would occupy at the current frame width — the only API
/// that accounts for `Wrap { trim: false }` and grapheme widths exactly.
pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();

    let input_line = build_input_line(state);
    let input_widget = Paragraph::new(Text::from(vec![input_line])).wrap(Wrap { trim: false });
    // Cap the input chunk at half the screen so the conversation keeps the
    // other half, but never go below 1 row — on a degenerate `area.height`
    // (1) the half-screen cap would round to 0, producing a zero-height
    // `Constraint::Length(0)` chunk and hiding the input entirely.
    let input_height_cap = (area.height / 2).max(1);
    let input_height = input_widget
        .line_count(area.width)
        .try_into()
        .unwrap_or(u16::MAX)
        .max(1)
        .min(input_height_cap);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // title bar
            Constraint::Min(0),               // conversation (flex)
            Constraint::Length(1),            // separator
            Constraint::Length(input_height), // input
            Constraint::Length(1),            // footer
        ])
        .split(area);

    let title_line = build_title_bar(state);
    let title_widget = Paragraph::new(Text::from(vec![title_line]));
    frame.render_widget(title_widget, chunks[0]);

    let conv_lines = build_conversation_lines(state, chunks[1].width);
    let conv_widget = render_conversation(&conv_lines, chunks[1], state.scroll_offset);
    frame.render_widget(conv_widget, chunks[1]);

    let sep_line = build_separator(chunks[2].width);
    let sep_widget = Paragraph::new(Text::from(vec![sep_line]));
    frame.render_widget(sep_widget, chunks[2]);

    frame.render_widget(input_widget, chunks[3]);

    let footer_line = build_footer(state);
    let footer_widget = Paragraph::new(Text::from(vec![footer_line]));
    frame.render_widget(footer_widget, chunks[4]);
}

/// Wrap conversation lines in a `Paragraph`, scrolled either to the bottom
/// (default) or `user_scroll` rows up from the bottom.
///
/// Word-wraps long entries so the model's responses (which can easily exceed
/// 200 columns) stay visible — without wrap, a single overflowing line would
/// render as a single chopped-off row. `trim: false` preserves the two-space
/// text indent and any leading whitespace inside model responses.
///
/// Scroll math:
/// - `bottom_scroll = total_rows - visible_rows`, clamped at 0. This is the
///   scroll offset needed to anchor the latest entry to the bottom.
/// - `user_scroll` is how many rows the operator has scrolled up via
///   `PageUp` / mouse wheel / `Shift-Up`. It's clamped to `[0, bottom_scroll]`
///   on the render side so over-scrolling is harmless.
/// - Effective scroll passed to ratatui is `bottom_scroll - clamped_user`.
///
/// `Paragraph::line_count` (behind ratatui's `unstable-rendered-line-info`
/// feature, see this crate's Cargo.toml) reports the exact post-wrap row
/// count. A prior ceiling-divide approximation undercounted because
/// `Wrap { trim: false }` preserves leading whitespace on continuation rows
/// — effectively narrowing the wrap width past column 0 — and the latest
/// entry ended up below the viewport. We accept the unstable-feature
/// pin for correctness here; the API surface is small and the build will
/// loudly fail if ratatui changes it.
fn render_conversation(
    lines: &[Line<'static>],
    area: ratatui::layout::Rect,
    user_scroll: u16,
) -> Paragraph<'static> {
    let paragraph = Paragraph::new(Text::from(lines.to_vec())).wrap(Wrap { trim: false });
    let total = paragraph.line_count(area.width);
    let visible = usize::from(area.height);
    let bottom_scroll: u16 = total.saturating_sub(visible).try_into().unwrap_or(u16::MAX);
    let effective = bottom_scroll.saturating_sub(user_scroll);
    paragraph.scroll((effective, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    // U_ROLE_STYLE: each role resolves to its documented color so the operator
    // can tell their own input apart from the lead, peers, and system at a
    // glance. NO_COLOR is honored elsewhere (no test here because it sets a
    // process-wide env var and would race with parallel tests).
    #[test]
    fn role_style_distinguishes_each_speaker() {
        let operator_id = reeve_types::IdentityId::new().unwrap();
        let other_agent_id = reeve_types::IdentityId::new().unwrap();

        let operator_entry = ConversationEntry {
            kind: EntryKind::Inbound,
            text: String::new(),
            timestamp: None,
            sender_id: Some(operator_id),
        };
        let lead_entry = ConversationEntry {
            kind: EntryKind::Outbound,
            text: String::new(),
            timestamp: None,
            sender_id: None,
        };
        let peer_entry = ConversationEntry {
            kind: EntryKind::Inbound,
            text: String::new(),
            timestamp: None,
            sender_id: Some(other_agent_id),
        };
        let system_entry = ConversationEntry {
            kind: EntryKind::System,
            text: String::new(),
            timestamp: None,
            sender_id: None,
        };

        let op = Some(operator_id);
        assert_eq!(role_style(&operator_entry, op).fg, Some(Color::Yellow));
        assert_eq!(role_style(&lead_entry, op).fg, Some(Color::Cyan));
        assert_eq!(role_style(&peer_entry, op).fg, Some(Color::Magenta));
        assert_eq!(role_style(&system_entry, op).fg, Some(Color::DarkGray));
    }

    // U_INPUT_WRAP: a long input string produces a multi-row wrapped paragraph,
    // and line_count() reflects that — so the layout constraint can grow the
    // input chunk.
    #[test]
    fn input_widget_line_count_grows_with_long_input() {
        let mut state = AppState::default();
        state.input = "abcdefghij ".repeat(20) + &"abcdefghij".repeat(5); // mix of spaces and a long run
        let line = build_input_line(&state);
        let paragraph = Paragraph::new(Text::from(vec![line])).wrap(Wrap { trim: false });
        // At width=40 a >250-char input must wrap to more than one row.
        let count_40 = paragraph.line_count(40);
        assert!(
            count_40 > 1,
            "expected >1 wrapped rows at width=40, got {count_40}"
        );
        // At width=1000 the same input fits on a single row.
        let count_1000 = paragraph.line_count(1000);
        assert_eq!(
            count_1000, 1,
            "expected 1 row at width=1000, got {count_1000}"
        );
    }

    // U_INPUT_WRAP_NO_SPACES: a no-space input (long path, URL, dense text) must
    // still wrap — ratatui's Wrap word-breaks on whitespace, so if it doesn't
    // also break mid-word the input would run off-screen even with our growing
    // chunk. This is the case the operator hit.
    #[test]
    fn input_widget_wraps_input_without_spaces() {
        let mut state = AppState::default();
        state.input = "a".repeat(300);
        let line = build_input_line(&state);
        let paragraph = Paragraph::new(Text::from(vec![line])).wrap(Wrap { trim: false });
        let count = paragraph.line_count(40);
        assert!(
            count > 1,
            "no-space input must wrap to multiple rows; got {count} rows at width=40"
        );
    }

    // U_INPUT_HEIGHT_TINY: on a degenerate terminal height (1 row) the
    // input chunk's half-screen cap would round to 0; the renderer must
    // floor the result at 1 so the input is never hidden.
    #[test]
    fn input_height_floors_at_one_on_tiny_terminals() {
        for area_height in [0_u16, 1, 2, 3] {
            let cap = (area_height / 2).max(1);
            assert!(
                cap >= 1,
                "input height cap must never be zero (area_height={area_height})"
            );
        }
    }

    // U_BODY_WRAP: a long body line wraps with the BODY_INDENT on every
    // visual row. The smoke at 80×24 caught the prior bug where the
    // second row of a wrapped Outbound entry started at column 1.
    #[test]
    fn wrap_body_line_keeps_indent_on_every_visual_row() {
        let text = "I'll start by reading the current shape. \
                    Two passes: list the shared fields, then plan the trait.";
        let rows = wrap_body_line(text, 40);
        assert!(rows.len() >= 2, "long line should wrap; got {rows:?}");
        for (i, row) in rows.iter().enumerate() {
            assert!(
                row.starts_with(BODY_INDENT),
                "row {i} missing body indent: {row:?}"
            );
            assert!(
                row.chars().count() <= 40,
                "row {i} exceeds width=40: {row:?}"
            );
        }
    }

    // U_BODY_WRAP_SHORT: short text fits in one visual row, still
    // carries the indent.
    #[test]
    fn wrap_body_line_short_text_is_single_row_with_indent() {
        let rows = wrap_body_line("ok", 80);
        assert_eq!(rows, vec!["  ok".to_owned()]);
    }

    // U_BODY_WRAP_EMPTY: empty text produces one indent-only row (so the
    // surrounding entry still occupies vertical space for the speaker
    // tag).
    #[test]
    fn wrap_body_line_empty_text_produces_indent_only_row() {
        assert_eq!(wrap_body_line("", 80), vec!["  ".to_owned()]);
        assert_eq!(wrap_body_line("   ", 80), vec!["  ".to_owned()]);
    }

    // U_FOOTER_COST: the chat footer uses the same `${:.2}` format the
    // panopticon title bar does — no four-decimal `$0.3100 USD` outlier.
    #[test]
    fn build_footer_uses_two_decimal_cost_no_currency_suffix() {
        let mut state = AppState::default();
        state.cost_usd = 0.31;
        let line = build_footer(&state);
        let rendered: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(
            rendered.contains("$0.31"),
            "footer should show $0.31 (2dp); got {rendered:?}"
        );
        assert!(
            !rendered.contains("$0.3100"),
            "footer should not show 4dp cost: {rendered:?}"
        );
        assert!(
            !rendered.contains("USD"),
            "footer should not include USD suffix: {rendered:?}"
        );
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

    // U4: scroll_up / scroll_down / scroll_to_bottom each move the offset in
    // the documented direction and saturate at their bounds.
    #[test]
    fn app_state_scroll_helpers() {
        let mut state = AppState::default();
        assert!(state.is_at_bottom());

        state.scroll_up(10);
        assert_eq!(state.scroll_offset, 10);
        assert!(!state.is_at_bottom());

        state.scroll_down(3);
        assert_eq!(state.scroll_offset, 7);

        // saturating downscroll never wraps under zero
        state.scroll_down(99);
        assert_eq!(state.scroll_offset, 0);
        assert!(state.is_at_bottom());

        // saturating up at u16::MAX never wraps over
        state.scroll_up(u16::MAX);
        state.scroll_up(1);
        assert_eq!(state.scroll_offset, u16::MAX);

        state.scroll_to_bottom();
        assert_eq!(state.scroll_offset, 0);
    }
}
