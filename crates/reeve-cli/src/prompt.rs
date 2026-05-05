//! Shared stdin prompt helper.
//!
//! Centralises the stderr-prompt → stdin-read → trim → empty-guard pattern
//! used by both the `set-key` and `enroll` commands.

use std::io::{self, BufRead, Write};

/// Print `prompt` to stderr (no trailing newline), flush, then read one line
/// from stdin. The trimmed result is returned. An empty (or whitespace-only)
/// line returns `Err(empty_error)`.
///
/// Lock ordering: stderr is locked, flushed, and **dropped** before stdin is
/// locked, avoiding potential deadlocks on platforms where the two share a
/// mutex.
pub(crate) fn prompt_one_line(
    prompt: &str,
    empty_error: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    {
        let stderr = io::stderr();
        let mut stderr = stderr.lock();
        write!(stderr, "{prompt}")?;
        stderr.flush()?;
    }
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(empty_error.into());
    }
    Ok(trimmed.to_owned())
}
