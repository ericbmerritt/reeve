//! `reeve daemon` subcommand group: start, stop, and status for the runtime
//! daemon, plus the hidden `run-internal` entry point called by `daemon start`.
//!
//! All four subcommands are thin wrappers: argument parsing and dispatch only.
//! Lifecycle logic lives entirely in [`reeve_runtime::daemon`].

use std::io::{self, Write};

use clap::Subcommand;
use reeve_runtime::runtime_lock::default_state_dir;
use reeve_runtime::{
    daemon_run, daemon_spawn, daemon_status, daemon_stop, DaemonError, DaemonStatus,
    IdentityRegistry,
};

// ── Subcommand definitions ────────────────────────────────────────────────────

/// Subcommands under `reeve daemon`.
#[derive(Subcommand, Debug)]
pub(crate) enum DaemonSubcommand {
    /// Start the runtime daemon in the background.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Print the current daemon status.
    Status,
    /// Run the daemon in the foreground (internal; called by `daemon start`).
    #[command(hide = true, name = "run-internal")]
    RunInternal,
}

// ── Public dispatch ───────────────────────────────────────────────────────────

/// Dispatch `reeve daemon` subcommands.
///
/// Called from `main` after subcommand parsing. Errors propagate back to
/// `main` and are printed via the standard `Box<dyn Error>` chain before exit.
pub(crate) fn dispatch(cmd: &DaemonSubcommand) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        DaemonSubcommand::Start => cmd_start(),
        DaemonSubcommand::Stop => cmd_stop(),
        DaemonSubcommand::Status => cmd_status(),
        DaemonSubcommand::RunInternal => cmd_run_internal(),
    }
}

// ── start ─────────────────────────────────────────────────────────────────────

fn cmd_start() -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = default_state_dir()?;
    handle_spawn_result(daemon_spawn(&state_dir), &mut io::stdout().lock())
}

// ── stop ──────────────────────────────────────────────────────────────────────

fn cmd_stop() -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = default_state_dir()?;
    handle_stop_result(daemon_stop(&state_dir), &mut io::stdout().lock())
}

// ── status ────────────────────────────────────────────────────────────────────

fn cmd_status() -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = default_state_dir()?;
    format_status(daemon_status(&state_dir), &mut io::stdout().lock())
}

// ── routing helpers (pure, testable) ─────────────────────────────────────────

fn handle_spawn_result(
    result: Result<(), DaemonError>,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    match result {
        Ok(()) => {
            writeln!(out, "daemon started")?;
            Ok(())
        }
        Err(DaemonError::AlreadyRunning { pid: Some(pid) }) => {
            writeln!(out, "already running, PID {pid}")?;
            Ok(())
        }
        Err(DaemonError::AlreadyRunning { pid: None }) => {
            writeln!(out, "already running")?;
            Ok(())
        }
        Err(
            err @ (DaemonError::Lock(_)
            | DaemonError::Io { .. }
            | DaemonError::Resource { .. }
            | DaemonError::Timeout { .. }
            | DaemonError::Signal { .. }
            | DaemonError::NoRuntime),
        ) => Err(err.into()),
    }
}

fn handle_stop_result(
    result: Result<(), DaemonError>,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    match result {
        Ok(()) => {
            writeln!(out, "daemon stopped")?;
            Ok(())
        }
        Err(DaemonError::NoRuntime) => {
            writeln!(out, "no runtime")?;
            Ok(())
        }
        Err(
            err @ (DaemonError::AlreadyRunning { .. }
            | DaemonError::Lock(_)
            | DaemonError::Io { .. }
            | DaemonError::Resource { .. }
            | DaemonError::Timeout { .. }
            | DaemonError::Signal { .. }),
        ) => Err(err.into()),
    }
}

fn format_status(
    status: DaemonStatus,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    match status {
        DaemonStatus::Alive { pid, heartbeat_age } => {
            let age = heartbeat_age.as_secs_f64();
            writeln!(out, "alive, PID {pid}, heartbeat {age:.1}s ago")?;
        }
        DaemonStatus::Stale { pid } => {
            writeln!(out, "stale, PID {pid} (heartbeat old or absent)")?;
        }
        DaemonStatus::NotRunning => {
            writeln!(out, "no runtime")?;
        }
    }
    Ok(())
}

// ── run-internal ──────────────────────────────────────────────────────────────

fn cmd_run_internal() -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = default_state_dir()?;
    let data_dir = IdentityRegistry::default_data_dir()?;
    daemon_run(state_dir, &data_dir).map_err(Into::into)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reeve_runtime::DaemonError;

    use super::{format_status, handle_spawn_result, handle_stop_result, DaemonStatus};

    #[test]
    fn handle_spawn_result_already_running_with_pid() {
        let mut buf = Vec::<u8>::new();
        let result =
            handle_spawn_result(Err(DaemonError::AlreadyRunning { pid: Some(42) }), &mut buf);
        assert!(result.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains("already running, PID 42"),
            "got: {output:?}"
        );
    }

    #[test]
    fn handle_spawn_result_already_running_without_pid() {
        let mut buf = Vec::<u8>::new();
        let result = handle_spawn_result(Err(DaemonError::AlreadyRunning { pid: None }), &mut buf);
        assert!(result.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("already running"), "got: {output:?}");
    }

    #[test]
    fn handle_stop_result_no_runtime() {
        let mut buf = Vec::<u8>::new();
        let result = handle_stop_result(Err(DaemonError::NoRuntime), &mut buf);
        assert!(result.is_ok());
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("no runtime"), "got: {output:?}");
    }

    #[test]
    fn format_status_all_variants() {
        // NotRunning
        let mut buf = Vec::<u8>::new();
        format_status(DaemonStatus::NotRunning, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("no runtime"), "got: {output:?}");

        // Stale
        let mut buf = Vec::<u8>::new();
        format_status(DaemonStatus::Stale { pid: 99 }, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("stale"), "got: {output:?}");
        assert!(output.contains("99"), "got: {output:?}");

        // Alive
        let mut buf = Vec::<u8>::new();
        format_status(
            DaemonStatus::Alive {
                pid: 1,
                heartbeat_age: Box::new(Duration::from_millis(500)),
            },
            &mut buf,
        )
        .unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("alive"), "got: {output:?}");
        assert!(output.contains('1'), "got: {output:?}");
    }
}
