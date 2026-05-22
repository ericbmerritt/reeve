//! `reeve daemon` subcommand group: start, stop, and status for the runtime
//! daemon, plus the hidden `run-internal` entry point called by `daemon start`.
//!
//! All four subcommands are thin wrappers: argument parsing and dispatch only.
//! Lifecycle logic lives entirely in [`reeve_runtime::daemon`].

use std::io::{self, Write};
use std::sync::Arc;

use clap::Subcommand;
use reeve_runtime::runtime_lock::{default_log_path, default_state_dir};
use reeve_runtime::{
    daemon_run, daemon_spawn, daemon_status, daemon_stop, keychain::labels, DaemonError,
    DaemonStatus, IdentityRegistry, KeychainError, OperatorSecretStore,
};
use secrecy::ExposeSecret as _;

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
    /// Print recent daemon log lines, optionally streaming new lines as they
    /// arrive. The log lives at `<state_dir>/daemon.log` and captures the
    /// tracing-subscriber output plus any pre-subscriber stderr from
    /// daemon startup.
    Logs {
        /// Stream new lines as the daemon writes them (like `tail -F`).
        #[arg(short, long)]
        follow: bool,
        /// Number of trailing lines to print before following (default 50).
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
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
        DaemonSubcommand::Logs { follow, lines } => cmd_logs(*follow, *lines),
        DaemonSubcommand::RunInternal => cmd_run_internal(),
    }
}

// ── start ─────────────────────────────────────────────────────────────────────

fn cmd_start() -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = default_state_dir()?;
    // Pre-fetch the API key in the foreground so the macOS Keychain dialog
    // (if any) appears here rather than in the background daemon process.
    preload_adapter_key()?;
    let result = daemon_spawn(&state_dir);
    std::env::remove_var("REEVE_ADAPTER_KEY");
    handle_spawn_result(result, &mut io::stdout().lock())
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

// ── logs ──────────────────────────────────────────────────────────────────────

/// Print `daemon.log` to stdout. Delegates to `tail(1)` (POSIX) because pure
/// Rust follow-mode is non-trivial and the operator-facing tool is allowed to
/// depend on a tool every supported dev OS ships with.
fn cmd_logs(follow: bool, lines: usize) -> Result<(), Box<dyn std::error::Error>> {
    let log_path = default_log_path()?;
    if !log_path.exists() {
        writeln!(
            io::stderr().lock(),
            "log file does not exist yet: {}",
            log_path.display()
        )?;
        writeln!(
            io::stderr().lock(),
            "(start the daemon with `reeve daemon start` to create it)"
        )?;
        return Ok(());
    }

    let mut cmd = std::process::Command::new("tail");
    cmd.arg("-n").arg(lines.to_string());
    if follow {
        // -F follows by name; survives the future log-rotation rename.
        cmd.arg("-F");
    }
    cmd.arg(&log_path);

    let status = cmd.status()?;
    if !status.success() {
        return Err(format!("tail exited non-zero: {status}").into());
    }
    Ok(())
}

// ── routing helpers (pure, testable) ─────────────────────────────────────────

fn handle_spawn_result(
    result: Result<(), DaemonError>,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    match result {
        Ok(()) => {
            writeln!(out, "daemon started")?;
            write_log_hint(out)?;
            Ok(())
        }
        Err(DaemonError::AlreadyRunning { pid: Some(pid) }) => {
            writeln!(out, "already running, PID {pid}")?;
            write_log_hint(out)?;
            Ok(())
        }
        Err(DaemonError::AlreadyRunning { pid: None }) => {
            writeln!(out, "already running")?;
            write_log_hint(out)?;
            Ok(())
        }
        Err(
            err @ (DaemonError::Lock(_)
            | DaemonError::Io { .. }
            | DaemonError::Resource { .. }
            | DaemonError::Timeout { .. }
            | DaemonError::Signal { .. }
            | DaemonError::NoRuntime),
        ) => {
            // Timeout and Resource errors during startup almost always have a
            // root cause in the daemon log; surface the path even on the error
            // path so operators do not need to remember where it lives.
            let _ = write_log_hint(&mut io::stderr().lock());
            Err(err.into())
        }
    }
}

/// Emit a `log: <path>` hint line. Best-effort: a path-resolution failure
/// here must not mask the primary status the caller just printed.
fn write_log_hint(out: &mut dyn Write) -> io::Result<()> {
    if let Ok(path) = default_log_path() {
        writeln!(
            out,
            "log: {} (run `reeve daemon logs -f` to tail)",
            path.display()
        )?;
    }
    Ok(())
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
            write_log_hint(out)?;
        }
        DaemonStatus::Stale { pid } => {
            writeln!(
                out,
                "stale, PID {pid} (process alive but heartbeat stopped — run: daemon stop)"
            )?;
            // Heartbeat-stopped daemons almost always tell you *why* in their
            // log; surface the path so the operator doesn't have to remember.
            write_log_hint(out)?;
        }
        DaemonStatus::NotRunning => {
            writeln!(out, "not running")?;
        }
    }
    Ok(())
}

// ── run-internal ──────────────────────────────────────────────────────────────

fn cmd_run_internal() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging. REEVE_LOG controls the filter
    // (default: debug for reeve crates, warn for everything else).
    //
    // The fmt subscriber's default writer is stdout. The daemon spawner only
    // redirects stderr to <state_dir>/daemon.log (see daemon_log_stdio),
    // leaving stdout pointed at /dev/null. Without with_writer(io::stderr)
    // every tracing call would be silently discarded and the only entries
    // in daemon.log would be pre-subscriber `Error:` prints from `?`
    // propagation at startup. Cost us several hours; pin the writer
    // explicitly.
    let filter = std::env::var("REEVE_LOG").unwrap_or_else(|_| "reeve=debug,warn".to_owned());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(true)
        .with_thread_ids(false)
        .with_writer(io::stderr)
        .init();

    let state_dir = default_state_dir()?;
    let data_dir = IdentityRegistry::default_data_dir()?;
    let adapter = build_adapter_for_daemon()?;
    daemon_run(state_dir, &data_dir, &adapter).map_err(Into::into)
}

/// Fetch the Anthropic API key from the keychain and stash it in
/// `REEVE_ADAPTER_KEY` so the spawned daemon subprocess can read it without
/// triggering a second Keychain dialog.
fn preload_adapter_key() -> Result<(), Box<dyn std::error::Error>> {
    let store = crate::keychain::open_platform_secretstore()?;
    let secret = store
        .retrieve_secret(labels::ANTHROPIC_API_KEY)
        .map_err(|e| format!("keychain: {e}"))?;
    std::env::set_var("REEVE_ADAPTER_KEY", secret.expose_secret());
    Ok(())
}

/// Retrieve the Anthropic API key from the platform keychain and construct
/// the `ClaudeOpus47` adapter.
///
/// Mirrors the key-loading path used by `reeve adapter test`
/// (`adapter::retrieve_api_key`). The CLI layer owns keychain access;
/// `reeve-runtime` receives an already-constructed adapter.
fn build_adapter_for_daemon() -> Result<Arc<dyn reeve_adapter::Adapter>, Box<dyn std::error::Error>>
{
    // If the parent process pre-loaded the key (to avoid a macOS Keychain
    // dialog in a background process), use it directly.
    if let Ok(key_str) = std::env::var("REEVE_ADAPTER_KEY") {
        let secret = secrecy::SecretString::from(key_str);
        return Ok(Arc::new(reeve_adapter::ClaudeOpus47::new(secret)));
    }
    // Fall back to the keychain — reached when the daemon is started
    // independently (e.g. `reeve daemon start`).
    let store = crate::keychain::open_platform_secretstore()?;
    let secret = store
        .retrieve_secret(labels::ANTHROPIC_API_KEY)
        .map_err(keychain_error_for_daemon)?;
    let adapter = reeve_adapter::ClaudeOpus47::new(secret);
    Ok(Arc::new(adapter))
}

/// Map a [`KeychainError`] from daemon adapter loading to a user-facing error.
///
/// `SecretNotFound` gets a tailored hint; all other variants surface their
/// `Display` representation. Platform-specific arms are unreachable on the
/// opposing platform; the final wildcard handles `#[non_exhaustive]` future
/// additions.
fn keychain_error_for_daemon(err: KeychainError) -> Box<dyn std::error::Error> {
    match err {
        KeychainError::SecretNotFound { .. } => "No Anthropic API key in keychain. \
             Run `reeve adapter set-key` to add one."
            .into(),
        KeychainError::NotFound { .. }
        | KeychainError::InvalidSecretEncoding { .. }
        | KeychainError::InvalidSeedLength { .. } => Box::new(err),
        #[cfg(target_os = "macos")]
        KeychainError::MacOsKeychain { .. } | KeychainError::MacOsKeychainForLabel { .. } => {
            Box::new(err)
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        KeychainError::DuplicateEntry { .. }
        | KeychainError::SecretService { .. }
        | KeychainError::SecretServiceUnavailable { .. }
        | KeychainError::SecretServiceForLabel { .. } => Box::new(err),
        _ => Box::new(err),
    }
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
        assert!(output.contains("not running"), "got: {output:?}");

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

    // D1: keychain_error_for_daemon maps SecretNotFound to a hint message.
    #[test]
    fn keychain_error_secret_not_found_maps_to_hint() {
        use reeve_runtime::KeychainError;

        use super::keychain_error_for_daemon;

        let err = KeychainError::SecretNotFound {
            label: reeve_runtime::keychain::labels::ANTHROPIC_API_KEY.to_owned(),
        };
        let mapped = keychain_error_for_daemon(err);
        let msg = mapped.to_string();
        assert!(
            msg.contains("No Anthropic API key in keychain"),
            "expected hint message; got: {msg}"
        );
        assert!(
            msg.contains("reeve adapter set-key"),
            "expected set-key hint; got: {msg}"
        );
    }

    // D2: keychain_error_for_daemon maps InvalidSecretEncoding to Box<dyn Error>.
    #[test]
    fn keychain_error_other_variants_surface_display() {
        use reeve_runtime::KeychainError;

        use super::keychain_error_for_daemon;

        let err = KeychainError::InvalidSecretEncoding {
            label: String::from("test"),
        };
        let mapped = keychain_error_for_daemon(err);
        let msg = mapped.to_string();
        assert!(!msg.is_empty(), "error message must not be empty");
    }
}
