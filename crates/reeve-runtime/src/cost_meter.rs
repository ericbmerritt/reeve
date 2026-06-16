//! Session cost aggregation for the cost-threshold authority check.
//!
//! [`session_cost_usd`] walks the `agents/` directory under the data root and
//! sums each agent's persisted cost file to produce the total session spend.
//! This is the brainstem-tier accumulator the per-session threshold check reads
//! before each adapter call.
//!
//! The cost files are written by each agent after every successful model call
//! (see `Agent::handle_response`). The read is opportunistic: missing or
//! unreadable cost files contribute 0.0 rather than failing the threshold check
//! — an agent that has never made a model call has spent nothing.

use std::path::Path;

/// Return the total session cost in USD across all agents under `data_dir`.
///
/// Reads `<data_dir>/agents/<name>/cost` for every subdirectory. Entries that
/// are missing, unreadable, or unparseable contribute `0.0`. Never panics.
///
/// **Approximation:** cost files are written after each successful adapter
/// call completes. An agent's in-flight spend for the *current* turn is not
/// visible here until `handle_response` flushes it to disk. Concurrent agents
/// each reading this total therefore see a floor, not the live session sum.
pub fn session_cost_usd(data_dir: &Path) -> f64 {
    let agents_dir = data_dir.join("agents");
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return 0.0;
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let cost_path = entry.path().join("cost");
            let text = std::fs::read_to_string(&cost_path).ok()?;
            text.trim().parse::<f64>().ok()
        })
        .sum()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_cost(base: &Path, agent: &str, usd: f64) {
        let dir = base.join("agents").join(agent);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cost"), format!("{usd:.6}")).unwrap();
    }

    // CM1: empty agents directory returns 0.0
    #[test]
    fn empty_agents_dir_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("agents")).unwrap();
        let total = session_cost_usd(tmp.path());
        assert!(
            (total - 0.0).abs() < f64::EPSILON,
            "empty agents/ must sum to 0.0; got {total}"
        );
    }

    // CM2: missing agents directory returns 0.0 (no panic)
    #[test]
    fn missing_agents_dir_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let total = session_cost_usd(tmp.path());
        assert!(
            (total - 0.0).abs() < f64::EPSILON,
            "missing agents/ must return 0.0; got {total}"
        );
    }

    // CM3: sums multiple agent cost files
    #[test]
    fn sums_multiple_agents() {
        let tmp = tempfile::tempdir().unwrap();
        write_cost(tmp.path(), "lead", 0.01);
        write_cost(tmp.path(), "worker-abc", 0.005);
        write_cost(tmp.path(), "worker-def", 0.003);
        let total = session_cost_usd(tmp.path());
        assert!((total - 0.018).abs() < 1e-9, "expected 0.018; got {total}");
    }

    // CM4: agent with missing cost file contributes 0.0
    #[test]
    fn missing_cost_file_contributes_zero() {
        let tmp = tempfile::tempdir().unwrap();
        write_cost(tmp.path(), "lead", 0.05);
        // worker has a directory but no cost file
        fs::create_dir_all(tmp.path().join("agents").join("worker")).unwrap();
        let total = session_cost_usd(tmp.path());
        assert!(
            (total - 0.05).abs() < 1e-9,
            "missing cost file must contribute 0; got {total}"
        );
    }

    // CM5: unparseable cost file is skipped
    #[test]
    fn unparseable_cost_file_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_cost(tmp.path(), "lead", 0.02);
        let bad_dir = tmp.path().join("agents").join("broken");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("cost"), "not-a-number").unwrap();
        let total = session_cost_usd(tmp.path());
        assert!(
            (total - 0.02).abs() < 1e-9,
            "unparseable file must be skipped; got {total}"
        );
    }
}
