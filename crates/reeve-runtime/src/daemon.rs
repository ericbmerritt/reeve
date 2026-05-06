//! Library API for starting, stopping, and querying the runtime daemon process.
//!
//! This module provides three entry points:
//!
//! - [`daemon_status`] — infallible check: is the daemon alive, stale, or absent?
//! - [`daemon_spawn`] — fork a background daemon process and confirm it started.
//! - [`daemon_stop`] — send SIGTERM to the running daemon and wait for exit.
//! - [`daemon_run`] — the inner daemon loop, called by the spawned process itself.
//!
//! Process liveness is checked without `unsafe` by shelling out to `kill -0`.
//! The `daemon_run` function starts the actix supervisor tree (heartbeat and
//! watcher actors) and blocks until SIGTERM arrives or the system is stopped.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::audit::AuditLog;
use crate::identity_registry::IdentityRegistry;
use crate::ledger::{DeliveryLedger, ReplayLedger};
use crate::runtime_lock::{RuntimeLock, RuntimeLockError};
use crate::supervisor::{HeartbeatActor, WatcherActor};
use crate::watcher::Watcher;

/// Filename of the PID file inside the state directory.
const PID_FILENAME: &str = "runtime.pid";

/// Heartbeat staleness threshold: 2× the 1-second tick in [`HeartbeatActor`].
const STALE_THRESHOLD: Duration = Duration::from_secs(2);

/// How long [`daemon_spawn`] waits for the daemon to write its PID file.
const SPAWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

/// Poll interval for [`confirm_started`] when waiting for the daemon to write
/// its PID file.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Poll interval used when waiting for a process to exit in [`daemon_stop`].
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long [`daemon_stop`] waits for the daemon to exit after SIGTERM.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

// ── DaemonStatus ─────────────────────────────────────────────────────────────

/// Observed liveness state of the daemon process.
#[derive(Debug)]
pub enum DaemonStatus {
    /// PID file exists, process is alive, heartbeat mtime is within 2 seconds.
    Alive {
        /// PID from the PID file.
        pid: u32,
        /// Age of the heartbeat file at time of check.
        heartbeat_age: Box<Duration>,
    },
    /// PID file exists and process is alive, but heartbeat mtime is older than
    /// 2 seconds — the process may be stalled.
    Stale {
        /// PID from the PID file.
        pid: u32,
    },
    /// No PID file, or PID file present but the process at that PID is gone.
    NotRunning,
}

// ── DaemonError ──────────────────────────────────────────────────────────────

/// Errors produced by daemon lifecycle operations.
#[derive(Debug)]
pub enum DaemonError {
    /// A daemon is already running. `pid` is `None` when the PID file could not
    /// be read.
    AlreadyRunning { pid: Option<u32> },
    /// Failed to acquire the runtime lock.
    Lock(RuntimeLockError),
    /// Underlying I/O error, including resource-open failures.
    Io { path: PathBuf, source: io::Error },
    /// Failed to open a required runtime resource.
    Resource {
        /// Human-readable component name (e.g., `"identity registry"`).
        component: &'static str,
        /// Underlying error.
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// An operation timed out.
    Timeout {
        /// Operation that timed out (e.g., `"daemon start"`, `"daemon stop"`).
        op: &'static str,
    },
    /// Sending a signal to the daemon process failed.
    Signal { pid: u32, source: io::Error },
    /// No daemon is running; cannot stop or signal.
    NoRuntime,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { pid: Some(pid) } => {
                write!(f, "daemon is already running, PID {pid}")
            }
            Self::AlreadyRunning { pid: None } => {
                f.write_str("daemon is already running (PID unknown)")
            }
            Self::Lock(source) => write!(f, "daemon lock: {source}"),
            Self::Io { path, source } => {
                write!(f, "daemon IO at {}: {source}", path.display())
            }
            Self::Resource { component, source } => {
                write!(f, "failed to open {component}: {source}")
            }
            Self::Timeout { op } => write!(f, "{op} timed out"),
            Self::Signal { pid, source } => {
                write!(f, "daemon signal to PID {pid}: {source}")
            }
            Self::NoRuntime => f.write_str("no daemon is running"),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lock(source) => Some(source),
            Self::Io { source, .. } | Self::Signal { source, .. } => Some(source),
            Self::Resource { source, .. } => Some(source.as_ref()),
            Self::AlreadyRunning { .. } | Self::Timeout { .. } | Self::NoRuntime => None,
        }
    }
}

// ── daemon_status ─────────────────────────────────────────────────────────────

/// Check whether the daemon is running. Infallible: any I/O error is treated as
/// [`DaemonStatus::NotRunning`].
///
/// Returns [`DaemonStatus::Alive`] when the PID file exists, the process is
/// alive, and the heartbeat mtime is within 2 seconds. Returns
/// [`DaemonStatus::Stale`] when the process is alive but the heartbeat is old
/// or absent. Returns [`DaemonStatus::NotRunning`] otherwise.
pub fn daemon_status(state_dir: &Path) -> DaemonStatus {
    let pid_path = state_dir.join(PID_FILENAME);
    let Some(pid) = crate::runtime_lock::read_pid_file(&pid_path) else {
        return DaemonStatus::NotRunning;
    };

    if !process_alive(pid) {
        return DaemonStatus::NotRunning;
    }

    let heartbeat_path = state_dir.join("runtime").join("heartbeat");
    match heartbeat_age(&heartbeat_path) {
        Some(age) if age <= STALE_THRESHOLD => DaemonStatus::Alive {
            pid,
            heartbeat_age: Box::new(age),
        },
        _ => DaemonStatus::Stale { pid },
    }
}

/// Check process liveness by shelling out to `kill -0` to stay within
/// `unsafe_code = deny`. Exit status 0 means the process exists.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Compute the age of the heartbeat file at `path` using its mtime.
///
/// Returns `None` when the file does not exist or cannot be stat'd. The mtime
/// is compared against `SystemTime::now()`; negative ages (future mtime) are
/// treated as `Duration::ZERO`.
fn heartbeat_age(path: &Path) -> Option<Duration> {
    // Use symlink_metadata so a symlink placed at the heartbeat path is not
    // followed silently. The heartbeat file is written by HeartbeatActor with
    // O_NOFOLLOW; a symlink here is unexpected and we treat it as absent.
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    let mtime = metadata.modified().ok()?;
    let now = SystemTime::now();
    Some(now.duration_since(mtime).unwrap_or(Duration::ZERO))
}

// ── daemon_spawn ──────────────────────────────────────────────────────────────

/// Spawn the daemon as a detached background process.
///
/// Checks that no daemon is already running, then re-executes the current
/// binary with `daemon run-internal`. The child process detaches into its own
/// process group so it survives terminal close. After spawning, polls
/// [`daemon_status`] up to `SPAWN_CONFIRM_TIMEOUT` to confirm the daemon wrote
/// its PID file.
pub fn daemon_spawn(state_dir: &Path) -> Result<(), DaemonError> {
    match daemon_status(state_dir) {
        DaemonStatus::Alive { pid, .. } | DaemonStatus::Stale { pid } => {
            return Err(DaemonError::AlreadyRunning { pid: Some(pid) });
        }
        DaemonStatus::NotRunning => {}
    }

    let exe = std::env::current_exe().map_err(|source| DaemonError::Io {
        path: PathBuf::from("<current_exe>"),
        source,
    })?;

    spawn_detached(&exe)?;
    confirm_started(state_dir)
}

/// Perform the actual `Command::spawn` call, detaching the child.
fn spawn_detached(exe: &Path) -> Result<(), DaemonError> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "run-internal"]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // process_group(0) creates a new process group, preventing SIGHUP on
        // terminal close.
        cmd.process_group(0);
    }

    cmd.spawn().map_err(|source| DaemonError::Io {
        path: exe.to_path_buf(),
        source,
    })?;
    // Child handle is intentionally dropped; daemon is detached.
    Ok(())
}

/// Poll `daemon_status` until the daemon is `Alive` or `SPAWN_CONFIRM_TIMEOUT`
/// elapses.
pub(crate) fn confirm_started(state_dir: &Path) -> Result<(), DaemonError> {
    let deadline = std::time::Instant::now() + SPAWN_CONFIRM_TIMEOUT;
    loop {
        match daemon_status(state_dir) {
            DaemonStatus::Alive { .. } => return Ok(()),
            DaemonStatus::Stale { .. } | DaemonStatus::NotRunning => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(DaemonError::Timeout { op: "daemon start" });
        }
        std::thread::sleep(SPAWN_POLL_INTERVAL);
    }
}

// ── daemon_stop ───────────────────────────────────────────────────────────────

/// Send SIGTERM to the running daemon and wait up to 5 seconds for it to exit.
///
/// Returns [`DaemonError::NoRuntime`] when no daemon is running. Returns
/// [`DaemonError::Signal`] when the kill command fails. Returns
/// [`DaemonError::Timeout`] if the daemon does not exit within 5 seconds.
pub fn daemon_stop(state_dir: &Path) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        daemon_stop_unix(state_dir)
    }
    #[cfg(not(unix))]
    {
        let _ = state_dir;
        Err(DaemonError::NoRuntime)
    }
}

#[cfg(unix)]
fn daemon_stop_unix(state_dir: &Path) -> Result<(), DaemonError> {
    let pid_path = state_dir.join(PID_FILENAME);
    let Some(pid) = crate::runtime_lock::read_pid_file(&pid_path) else {
        return Err(DaemonError::NoRuntime);
    };

    if !process_alive(pid) {
        return Err(DaemonError::NoRuntime);
    }

    send_sigterm(pid)?;
    wait_for_exit(pid, STOP_TIMEOUT)
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<(), DaemonError> {
    let status = std::process::Command::new("kill")
        .args(["-15", &pid.to_string()])
        .status()
        .map_err(|source| DaemonError::Signal { pid, source })?;

    if status.success() {
        Ok(())
    } else {
        Err(DaemonError::Signal {
            pid,
            source: io::Error::other(format!("kill -15 exited with {status}")),
        })
    }
}

#[cfg(unix)]
pub(crate) fn wait_for_exit(pid: u32, timeout: Duration) -> Result<(), DaemonError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(DaemonError::Timeout { op: "daemon stop" });
        }
        std::thread::sleep(STOP_POLL_INTERVAL);
    }
}

// ── daemon_run ────────────────────────────────────────────────────────────────

/// Opened runtime resources, bundled for the `open_resources` return.
/// Private: immediately destructured at the call site in `daemon_run`.
type Resources = (
    Arc<IdentityRegistry>,
    Arc<ReplayLedger>,
    Arc<DeliveryLedger>,
    Arc<AuditLog>,
);

/// Inner daemon loop. Called by the spawned child process via `daemon run-internal`.
///
/// Acquires the runtime lock, opens all persistent resources, starts the actix
/// supervisor tree, and blocks until SIGTERM arrives (Unix) or the system is
/// stopped. On return the lock is dropped and the PID file is removed.
pub fn daemon_run(state_dir: PathBuf, data_dir: &Path) -> Result<(), DaemonError> {
    let _lock = acquire_lock(state_dir.clone())?;
    let (registry, replay, delivery, audit) = open_resources(data_dir)?;
    let watcher = Arc::new(Watcher::new(&registry, &replay, delivery, audit));

    run_actor_system(state_dir, watcher)
}

/// Acquire the runtime lock, mapping `RuntimeLockError` into `DaemonError`.
fn acquire_lock(state_dir: PathBuf) -> Result<RuntimeLock, DaemonError> {
    RuntimeLock::acquire(state_dir).map_err(|e| match e {
        RuntimeLockError::AlreadyRunning { pid } => DaemonError::AlreadyRunning { pid },
        other @ (RuntimeLockError::Io { .. }
        | RuntimeLockError::MissingHome
        | RuntimeLockError::RelativeStateDir { .. }) => DaemonError::Lock(other),
    })
}

/// Open all persistent resources needed by the daemon.
fn open_resources(data_dir: &Path) -> Result<Resources, DaemonError> {
    let registry =
        IdentityRegistry::open(data_dir.to_path_buf()).map_err(|e| DaemonError::Resource {
            component: "identity registry",
            source: Box::new(e),
        })?;
    let replay = ReplayLedger::open(data_dir.to_path_buf()).map_err(|e| DaemonError::Resource {
        component: "replay ledger",
        source: Box::new(e),
    })?;
    let delivery =
        DeliveryLedger::open(data_dir.to_path_buf()).map_err(|e| DaemonError::Resource {
            component: "delivery ledger",
            source: Box::new(e),
        })?;
    let audit = AuditLog::open(data_dir.to_path_buf()).map_err(|e| DaemonError::Resource {
        component: "audit log",
        source: Box::new(e),
    })?;
    Ok((
        Arc::new(registry),
        Arc::new(replay),
        Arc::new(delivery),
        Arc::new(audit),
    ))
}

/// Start the actix system, launch supervised actors, and block until shutdown.
fn run_actor_system(state_dir: PathBuf, watcher: Arc<Watcher>) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let mut signal_stream = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .map_err(|source| DaemonError::Resource {
            component: "SIGTERM handler",
            source: Box::new(source),
        })?;

        actix::System::new().block_on(async move {
            start_actors(&state_dir, watcher);
            let system = actix::System::current();
            tokio::spawn(async move {
                signal_stream.recv().await;
                system.stop();
                // Ignore send error: the block_on future may have already resolved.
                let _ = shutdown_tx.send(());
            });
            // Await shutdown notification. Resolves when SIGTERM fires or
            // system.stop() is called from another path.
            let _ = shutdown_rx.await;
        });
    }

    #[cfg(not(unix))]
    {
        actix::System::new().block_on(async move {
            start_actors(&state_dir, watcher);
            // No signal API on this platform. This future never resolves; the
            // process must be killed externally. Non-Unix is not a supported
            // deployment target.
            std::future::pending::<()>().await;
        });
    }

    Ok(())
}

/// Start the supervised `HeartbeatActor` and `WatcherActor`.
fn start_actors(state_dir: &Path, watcher: Arc<Watcher>) {
    let state_dir = state_dir.to_path_buf();
    actix::Supervisor::start(move |_| HeartbeatActor::new(state_dir));
    actix::Supervisor::start(move |_| WatcherActor::new(Arc::clone(&watcher)));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[cfg(unix)]
    use super::wait_for_exit;
    use super::{confirm_started, daemon_status, DaemonError, DaemonStatus};

    fn write_pid(state_dir: &Path, pid: u32) {
        fs::create_dir_all(state_dir).unwrap();
        fs::write(state_dir.join("runtime.pid"), format!("{pid}\n")).unwrap();
    }

    fn write_heartbeat(state_dir: &Path) {
        let runtime_dir = state_dir.join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();
        // Write current timestamp; file's mtime will be now.
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        fs::write(runtime_dir.join("heartbeat"), secs.to_string()).unwrap();
    }

    // D1: no PID file → NotRunning
    #[test]
    fn daemon_status_returns_not_running_when_no_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        assert!(
            matches!(daemon_status(&state_dir), DaemonStatus::NotRunning),
            "expected NotRunning when no PID file exists"
        );
    }

    // D2: PID file with a dead PID → NotRunning
    //
    // On most systems, PID 4_000_000 does not exist. If by extraordinary
    // coincidence it does, this test may spuriously fail; acceptable for a
    // test that must exercise the dead-PID branch without process scaffolding.
    #[test]
    fn daemon_status_returns_not_running_for_dead_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        write_pid(&state_dir, 4_000_000_u32);

        assert!(
            matches!(daemon_status(&state_dir), DaemonStatus::NotRunning),
            "expected NotRunning for a PID that does not exist"
        );
    }

    // D3: PID file with current PID and fresh heartbeat → Alive
    #[test]
    fn daemon_status_returns_alive_for_current_process() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        write_pid(&state_dir, std::process::id());
        write_heartbeat(&state_dir);

        match daemon_status(&state_dir) {
            DaemonStatus::Alive { pid, heartbeat_age } => {
                assert_eq!(pid, std::process::id());
                assert!(
                    *heartbeat_age < Duration::from_secs(2),
                    "heartbeat_age should be less than 2s, got {heartbeat_age:?}"
                );
            }
            other @ (DaemonStatus::Stale { .. } | DaemonStatus::NotRunning) => {
                panic!("expected Alive, got {other:?}")
            }
        }
    }

    // D4: PID file with current PID and no heartbeat → Stale
    #[test]
    fn daemon_status_returns_stale_for_current_process_with_no_heartbeat() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        write_pid(&state_dir, std::process::id());
        // No heartbeat file written; heartbeat_age returns None → Stale.

        assert!(
            matches!(daemon_status(&state_dir), DaemonStatus::Stale { .. }),
            "expected Stale when heartbeat file is absent"
        );
    }

    // D5: Display for all DaemonError variants contains expected substrings.
    #[test]
    fn daemon_error_display_all_variants() {
        let cases: &[(DaemonError, &str)] = &[
            (DaemonError::AlreadyRunning { pid: Some(42) }, "42"),
            (DaemonError::AlreadyRunning { pid: None }, "unknown"),
            (
                DaemonError::Io {
                    path: PathBuf::from("/tmp/test"),
                    source: io::Error::other("boom"),
                },
                "/tmp/test",
            ),
            (
                DaemonError::Resource {
                    component: "identity registry",
                    source: Box::new(io::Error::other("disk full")),
                },
                "identity registry",
            ),
            (DaemonError::Timeout { op: "daemon start" }, "timed out"),
            (
                DaemonError::Signal {
                    pid: 99,
                    source: io::Error::other("refused"),
                },
                "99",
            ),
            (DaemonError::NoRuntime, "no daemon"),
        ];

        for (err, needle) in cases {
            let rendered = err.to_string();
            assert!(
                rendered.contains(needle),
                "Display for {err:?} should contain {needle:?}, got: {rendered}"
            );
        }
    }

    // T1: confirm_started returns Timeout when no PID file is ever written.
    //
    // SPAWN_CONFIRM_TIMEOUT is 2s; this test will take up to 2s. It verifies
    // the loop terminates with the correct error variant.
    #[test]
    fn confirm_started_returns_timeout_when_daemon_does_not_start() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // No PID file; confirm_started will poll until SPAWN_CONFIRM_TIMEOUT.
        let result = confirm_started(&state_dir);
        assert!(
            matches!(result, Err(DaemonError::Timeout { .. })),
            "expected Timeout, got {result:?}",
        );
    }

    // T2: wait_for_exit returns Timeout when the process does not exit.
    //
    // Uses the current process's own PID (which will not exit). Passes a short
    // 300 ms timeout so the test completes quickly.
    #[cfg(unix)]
    #[test]
    fn wait_for_exit_returns_timeout_when_process_persists() {
        let pid = std::process::id();
        let result = wait_for_exit(pid, Duration::from_millis(300));
        assert!(
            matches!(result, Err(DaemonError::Timeout { .. })),
            "expected Timeout, got {result:?}",
        );
    }
}
