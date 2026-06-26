//! Helpers shared between the chat ([`crate::ui`]) and panopticon
//! ([`crate::ui_panopticon`]) renderers.
//!
//! Anything that both screens compute the same way and that has nothing
//! screen-specific to say lives here. Today: the `NO_COLOR` predicate and a
//! `HH:MM` timestamp formatter — both copied verbatim before this extract
//! and a CLAUDE.md design-defaults violation (the rule: "Search adjacent
//! files in the same crate before writing helpers"). New shared helpers
//! land here rather than being inlined a third time.

use time::OffsetDateTime;

/// Return true when `NO_COLOR` is set in the environment (any value).
///
/// Called once per draw pass. The `std::env` call is cheap at this cadence.
#[must_use]
pub fn no_color() -> bool {
    std::env::var("NO_COLOR").is_ok()
}

/// Format a timestamp as `HH:MM`. Used by both screens' dense-row contexts
/// (chat speaker tags, panopticon events stream) where seconds add noise
/// without information.
#[must_use]
pub fn format_time_hhmm(ts: OffsetDateTime) -> String {
    format!("{:02}:{:02}", ts.hour(), ts.minute())
}

/// Format an optional timestamp as `HH:MM`, or return an empty string.
/// Conversation entries from legacy journals (pre-attribution) have no
/// timestamp; the speaker tag elides the `· HH:MM` suffix entirely
/// rather than rendering `· `.
#[must_use]
pub fn format_time_hhmm_opt(ts: Option<OffsetDateTime>) -> String {
    ts.map(format_time_hhmm).unwrap_or_default()
}

/// Build a section header line: `─ label ──────────` extended to the
/// renderer's current `width`. Shared by the panopticon and quarantine
/// screens; the visual rhythm is intentional so the two screens read
/// alike.
///
/// Naive on graphemes — `label` is ASCII in practice. If a future
/// caller needs wide characters in the label the renderer should pull
/// in `unicode-width`.
#[must_use]
pub fn build_section_header(label: &str, width: u16) -> ratatui::text::Line<'static> {
    let lead = format!("\u{2500} {label} ");
    let pad = usize::from(width).saturating_sub(lead.chars().count());
    let rule: String = "\u{2500}".repeat(pad);
    ratatui::text::Line::from(format!("{lead}{rule}"))
}

/// Truncate or right-pad a column value to exactly `width` display
/// chars. Agent names, persona names, and reason tokens — the only
/// values this is called with — are ASCII-safe; if a future caller
/// passes wide characters the renderer needs `unicode-width`.
#[must_use]
pub fn pad_right(s: &str, width: usize) -> String {
    let actual = s.chars().count();
    if actual >= width {
        s.chars().take(width).collect()
    } else {
        let mut out = s.to_owned();
        out.extend(std::iter::repeat_n(' ', width - actual));
        out
    }
}

/// Truncate a string to at most `max_chars` display chars, appending an
/// ellipsis when truncation actually fires. Shared by the dense single-line
/// rows in the panopticon and inspect screens.
#[must_use]
pub fn truncate(s: &str, max_chars: usize) -> String {
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
