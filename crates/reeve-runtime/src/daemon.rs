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

use tracing::{debug, info};

use crate::agent::Agent;
use crate::agent_fs::AgentDirs;
use crate::agent_registry::{AgentRecord, AgentRegistry, AgentStatus, ValidatedAgentName};
use crate::audit::AuditLog;
use crate::config::{install_defaults, load_persona_config, load_team_config};
use crate::dispatcher::MessageDispatcher;
use crate::identity_registry::{IdentityRegistry, StoredIdentity};
use crate::inbox::AgentInbox;
use crate::ledger::{DeliveryLedger, ReplayLedger};
use crate::model_resolution::{resolve_model, write_spawn_snapshot};
use crate::runtime_lock::{RuntimeLock, RuntimeLockError};
use crate::spawn_coordinator::SpawnCoordinator;
use crate::supervisor::{HeartbeatActor, WatchInbox, WatcherActor};
use crate::watcher::Watcher;

/// Filename of the PID file inside the state directory.
const PID_FILENAME: &str = "runtime.pid";

/// Heartbeat staleness threshold: 2× the 1-second tick in [`HeartbeatActor`].
const STALE_THRESHOLD: Duration = Duration::from_secs(2);

/// How long [`daemon_spawn`] waits for the daemon to write its PID file.
///
/// First run is slower: the daemon installs default configs, opens multiple
/// file-based stores, resolves the model adapter, and starts the actix
/// supervisor tree before writing the heartbeat file. 15 s is generous but
/// avoids false "start failed" errors on cold or resource-constrained machines.
const SPAWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);

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
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
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

// ── heartbeat_fresh ───────────────────────────────────────────────────────────

/// Returns `true` if the daemon heartbeat file is fresh (written within 2 seconds).
///
/// Reads `{state_dir}/runtime/heartbeat` mtime via `symlink_metadata`. A symlink
/// at the path is treated as absent. Returns `false` on any error.
pub fn heartbeat_fresh(state_dir: &Path) -> bool {
    let path = state_dir.join("runtime").join("heartbeat");
    matches!(heartbeat_age(&path), Some(age) if age <= STALE_THRESHOLD)
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
    // Daemon log: stderr goes to $XDG_STATE_HOME/reeve/daemon.log so errors are
    // visible without attaching to the process. Falls back to /dev/null on any
    // file-open failure so a log permission issue never blocks startup.
    cmd.stderr(daemon_log_stdio());

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

/// Return a `Stdio` that appends daemon stderr to `<state_dir>/daemon.log`.
///
/// Falls back to `/dev/null` so a log-file permission problem never prevents
/// the daemon from starting.
fn daemon_log_stdio() -> std::process::Stdio {
    // Attempt to resolve the state directory; if that fails, fall back quietly.
    let Ok(state_dir) = crate::runtime_lock::default_state_dir() else {
        return std::process::Stdio::null();
    };
    let log_path = state_dir.join("daemon.log");
    // ensure_directory creates the dir if absent; ignore errors here.
    let _ = crate::fs_util::ensure_directory(&state_dir, 0o700);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null())
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

/// Send SIGTERM to the running daemon and wait up to [`STOP_TIMEOUT`] for it to exit.
///
/// Returns [`DaemonError::NoRuntime`] when no daemon is running. Returns
/// [`DaemonError::Signal`] when the kill command fails. Returns
/// [`DaemonError::Timeout`] if the daemon does not exit within [`STOP_TIMEOUT`].
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
    match wait_for_exit(pid, STOP_TIMEOUT) {
        Ok(()) => Ok(()),
        Err(DaemonError::Timeout { .. }) => {
            // SIGTERM was ignored (stalled actor loop, broken signal handler).
            // Escalate to SIGKILL so `daemon stop` always terminates the process.
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            wait_for_exit(pid, Duration::from_secs(2))
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<(), DaemonError> {
    let status = std::process::Command::new("kill")
        .args(["-15", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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
///
/// The `adapter` is built by the CLI layer (which has access to the platform
/// keychain) and passed in here so the runtime crate does not need to reopen
/// the keychain itself.
pub fn daemon_run(
    state_dir: PathBuf,
    data_dir: &Path,
    adapter: &Arc<dyn reeve_adapter::Adapter>,
) -> Result<(), DaemonError> {
    info!(pid = std::process::id(), "daemon starting");
    let _lock = acquire_lock(state_dir.clone())?;
    let (registry, replay, delivery, audit) = open_resources(data_dir)?;
    let agent_registry_path =
        AgentRegistry::default_registry_path().map_err(|e| DaemonError::Resource {
            component: "agent registry path",
            source: Box::new(e),
        })?;
    let watcher = Arc::new(Watcher::new(
        &registry,
        &replay,
        delivery,
        audit,
        agent_registry_path.clone(),
    ));

    run_actor_system(
        state_dir,
        data_dir,
        &registry,
        watcher,
        adapter,
        agent_registry_path,
    )?;
    info!("daemon stopping cleanly");
    Ok(())
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
#[expect(
    clippy::too_many_arguments,
    reason = "each parameter is a distinct resource with no natural grouping peer"
)]
fn run_actor_system(
    state_dir: PathBuf,
    data_dir: &Path,
    identity_registry: &Arc<IdentityRegistry>,
    watcher: Arc<Watcher>,
    adapter: &Arc<dyn reeve_adapter::Adapter>,
    agent_registry_path: PathBuf,
) -> Result<(), DaemonError> {
    // Prepare everything that can fail before entering the actix runtime.
    // Agent startup failures surface here with structured errors rather than
    // being swallowed inside block_on.
    let startup = prepare_agent_startup(
        data_dir,
        identity_registry,
        watcher,
        adapter,
        agent_registry_path,
    )?;
    let agent_registry_path = startup.agent_registry_path.clone();

    #[cfg(unix)]
    {
        debug!("actix system starting");
        let mut launch_err: Option<DaemonError> = None;
        actix::System::new().block_on(async {
            // _dispatcher_addr keeps the actor alive for the duration of the
            // system.
            let _dispatcher_addr = match launch_actors(state_dir, startup) {
                Ok(addr) => addr,
                Err(e) => {
                    launch_err = Some(e);
                    actix::System::current().stop();
                    return;
                }
            };
            // Set up SIGTERM handler inside block_on — tokio::signal requires
            // an active runtime, which actix provides only once block_on starts.
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let Ok(mut signal_stream) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                // If signal registration fails, run without graceful shutdown.
                std::future::pending::<()>().await;
                return;
            };
            let system = actix::System::current();
            tokio::spawn(async move {
                signal_stream.recv().await;
                info!("SIGTERM received, shutting down");
                system.stop();
                let _ = shutdown_tx.send(());
            });
            let _ = shutdown_rx.await;
        });
        if let Some(e) = launch_err {
            return Err(e);
        }
    }

    #[cfg(not(unix))]
    {
        debug!("actix system starting");
        let mut launch_err: Option<DaemonError> = None;
        actix::System::new().block_on(async {
            // _dispatcher_addr keeps the actor alive for the duration of the
            // system.
            let _dispatcher_addr = match launch_actors(state_dir, startup) {
                Ok(addr) => addr,
                Err(e) => {
                    launch_err = Some(e);
                    actix::System::current().stop();
                    return;
                }
            };
            // No signal API on this platform. This future never resolves; the
            // process must be killed externally. Non-Unix is not a supported
            // deployment target.
            std::future::pending::<()>().await;
        });
        if let Some(e) = launch_err {
            return Err(e);
        }
    }

    // On clean shutdown, mark the lead agent as Stopped. Failure here is
    // non-fatal: the registry will show Running at next startup and be
    // corrected then.
    if let Ok(mut registry) = AgentRegistry::open(agent_registry_path) {
        if let Err(err) = registry.update_status("lead", AgentStatus::Stopped) {
            tracing::warn!(err = %err, "failed to mark lead agent as Stopped on shutdown");
        }
    }

    Ok(())
}

/// Pre-computed inputs for [`launch_actors`]; produced by the fallible
/// [`prepare_agent_startup`] step that runs before the actix system starts.
///
/// The [`Agent`] value itself is constructed inside [`launch_actors`] so it
/// can hold [`actix::Recipient`]s to tool actors that only exist once the
/// actix runtime is up.
struct AgentStartup {
    adapter: Arc<dyn reeve_adapter::Adapter>,
    dirs: AgentDirs,
    snapshot: crate::model_resolution::SpawnSnapshot,
    system_prompt: String,
    inbox: AgentInbox,
    agent_id: reeve_types::IdentityId,
    keypair: reeve_types::Keypair,
    agent_registry_path: PathBuf,
    watcher: Arc<Watcher>,
    data_dir: PathBuf,
    identity_registry: Arc<IdentityRegistry>,
}

/// Fallible preparation: load configs, provision directories, resolve the
/// model, write the spawn snapshot, and construct the lead agent value.
///
/// None of this requires the actix runtime to be running, so errors can be
/// propagated normally.
#[expect(
    clippy::too_many_lines,
    reason = "splitting would obscure the linear dependency chain across startup steps"
)]
fn prepare_agent_startup(
    data_dir: &Path,
    identity_registry: &Arc<IdentityRegistry>,
    watcher: Arc<Watcher>,
    adapter: &Arc<dyn reeve_adapter::Adapter>,
    agent_registry_path: PathBuf,
) -> Result<AgentStartup, DaemonError> {
    // 1. Install default configs if they do not already exist.
    install_defaults(data_dir).map_err(|e| DaemonError::Resource {
        component: "config defaults",
        source: Box::new(e),
    })?;
    debug!("default configs ready");

    // 2. Load the default team config.
    let team_path = data_dir.join("teams").join("default.toml");
    let team = load_team_config(&team_path).map_err(|e| DaemonError::Resource {
        component: "team config",
        source: Box::new(e),
    })?;
    debug!(lead_role = %team.lead_role, "loaded team config");

    // 3. Locate the lead member entry.
    let lead_member = team
        .members
        .iter()
        .find(|m| m.role_label == team.lead_role)
        .ok_or_else(|| DaemonError::Resource {
            component: "lead member",
            source: Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "team config has no member with role_label '{}'",
                team.lead_role
            )),
        })?;

    // 4. Load persona config for the lead member.
    let persona_path = data_dir
        .join("personas")
        .join(&lead_member.persona_name)
        .join("config.toml");
    let persona_config = load_persona_config(&persona_path).map_err(|e| DaemonError::Resource {
        component: "persona config",
        source: Box::new(e),
    })?;
    debug!(name = %lead_member.persona_name, "loaded persona config");

    // 5. Provision the lead agent directory tree.
    let dirs = AgentDirs::provision(data_dir, "lead").map_err(|e| DaemonError::Resource {
        component: "agent dirs",
        source: Box::new(e),
    })?;

    // 6. On first run mint a new identity and register it; on restart reuse the stored identity_id.
    let keypair = crate::generate_or_load_keypair(&dirs.identity_key_path()).map_err(|e| {
        DaemonError::Resource {
            component: "agent keypair",
            source: Box::new(e),
        }
    })?;

    let mut agent_registry =
        AgentRegistry::open(agent_registry_path.clone()).map_err(|e| DaemonError::Resource {
            component: "agent registry",
            source: Box::new(e),
        })?;

    let agent_id = if let Some(record) = agent_registry.lookup("lead") {
        let id = record.identity_id;
        agent_registry
            .update_status("lead", AgentStatus::Running)
            .map_err(|e| DaemonError::Resource {
                component: "agent registry update",
                source: Box::new(e),
            })?;
        // Halt if the key file was replaced without updating the registry —
        // proceeding would produce envelopes that fail signature verification
        // at every counterparty.
        match identity_registry.lookup(id) {
            Ok(Some(stored)) => {
                let key_records = stored.key_records();
                let stored_key = &key_records
                    .first()
                    .ok_or_else(|| DaemonError::Resource {
                        component: "identity registry lookup",
                        source: Box::new(io::Error::other(
                            "stored identity has no key records; registry may be corrupt",
                        )),
                    })?
                    .public_key;
                if keypair.public() != stored_key {
                    return Err(DaemonError::Resource {
                        component: "keypair mismatch",
                        source: Box::new(io::Error::other(
                            "on-disk identity.key does not match stored public key; \
                             restore the correct key file or re-register the agent",
                        )),
                    });
                }
            }
            Ok(None) => {
                return Err(DaemonError::Resource {
                    component: "identity registry lookup",
                    source: Box::new(io::Error::other(
                        "agent_id found in agent registry but no entry in identity registry",
                    )),
                });
            }
            Err(err) => {
                return Err(DaemonError::Resource {
                    component: "identity registry lookup",
                    source: Box::new(err),
                });
            }
        }
        debug!(agent_id = %id, "reusing existing lead identity");
        id
    } else {
        // Bootstrap: no operator identity exists yet. Use new_operator so
        // created_by is None — there is no prior identity to reference, and
        // a dangling foreign key would corrupt the registry.
        let identity = reeve_types::Identity::new_operator(lead_member.persona_name.clone())
            .map_err(|e| DaemonError::Resource {
                component: "agent identity",
                source: Box::new(e),
            })?;
        let agent_id = identity.identity_id;
        let public_key = *keypair.public();
        let key_record = reeve_types::KeyRecord::new(agent_id, public_key).map_err(|e| {
            DaemonError::Resource {
                component: "key record",
                source: Box::new(e),
            }
        })?;
        let stored =
            StoredIdentity::new(identity, key_record).map_err(|e| DaemonError::Resource {
                component: "stored identity",
                source: Box::new(e),
            })?;
        identity_registry
            .write(&stored)
            .map_err(|e| DaemonError::Resource {
                component: "identity registry write",
                source: Box::new(e),
            })?;
        let lead_name = ValidatedAgentName::new("lead").map_err(|e| DaemonError::Resource {
            component: "lead agent name",
            source: Box::new(e),
        })?;
        agent_registry
            .register(AgentRecord {
                name: lead_name,
                identity_id: agent_id,
                inbox_dir: dirs.inbox_root(),
                persona_name: Some(lead_member.persona_name.clone()),
                spawned_at: time::OffsetDateTime::now_utc(),
                status: AgentStatus::Running,
            })
            .map_err(|e| DaemonError::Resource {
                component: "agent registry register",
                source: Box::new(e),
            })?;
        debug!(agent_id = %agent_id, "registered new lead identity");
        agent_id
    };

    // 7. Resolve the model adapter against this persona's preferences.
    let snapshot = resolve_model(&persona_config, &[adapter.as_ref()], agent_id).map_err(|e| {
        DaemonError::Resource {
            component: "model resolution",
            source: Box::new(e),
        }
    })?;
    debug!(adapter_id = %snapshot.adapter_id, "resolved adapter");

    // 8. Write the spawn snapshot to disk (includes agent_id for TUI signing).
    write_spawn_snapshot(&dirs, &snapshot).map_err(|e| DaemonError::Resource {
        component: "spawn snapshot",
        source: Box::new(e),
    })?;
    debug!(agent_id = %agent_id, "wrote spawn snapshot");

    // 9. Build the inbox handle pointing to the lead's provisioned inbox.
    let inbox = AgentInbox::from_path(dirs.inbox_root());

    let system_prompt = persona_config.system_prompt.clone();

    Ok(AgentStartup {
        adapter: Arc::clone(adapter),
        dirs,
        snapshot,
        system_prompt,
        inbox,
        agent_id,
        keypair,
        agent_registry_path,
        watcher,
        data_dir: data_dir.to_path_buf(),
        identity_registry: Arc::clone(identity_registry),
    })
}

/// Start all supervised actors inside the running actix system.
///
/// Must be called from within an actix `block_on` context.
fn launch_actors(
    state_dir: PathBuf,
    startup: AgentStartup,
) -> Result<actix::Addr<MessageDispatcher>, DaemonError> {
    use actix::Actor as _;

    let AgentStartup {
        adapter,
        dirs,
        snapshot,
        system_prompt,
        inbox,
        agent_id,
        keypair,
        agent_registry_path,
        watcher,
        data_dir,
        identity_registry,
    } = startup;

    // HeartbeatActor: touches the heartbeat file every second.
    actix::Supervisor::start(move |_| HeartbeatActor::new(state_dir));

    let watcher_for_coord = Arc::clone(&watcher);
    let watcher_addr = actix::Supervisor::start(move |_| WatcherActor::new(Arc::clone(&watcher)));

    // Opened as a fresh registry snapshot — keys and inbox_dir paths for the
    // registered agents are stable after this point in the current ladder.
    let dispatcher_registry =
        AgentRegistry::open(agent_registry_path.clone()).map_err(|e| DaemonError::Resource {
            component: "message dispatcher registry",
            source: Box::new(e),
        })?;
    let identity_registry_for_dispatcher = Arc::clone(&identity_registry);
    let dispatcher_addr = actix::Supervisor::start(move |_| {
        MessageDispatcher::new(
            Arc::new(dispatcher_registry),
            identity_registry_for_dispatcher,
        )
    });

    let spawn_coordinator = SpawnCoordinator::new(
        data_dir,
        agent_registry_path,
        identity_registry,
        Arc::clone(&adapter),
        watcher_for_coord,
        watcher_addr.clone().recipient(),
    );
    let coord_addr = actix::Supervisor::start(move |_| spawn_coordinator);

    let spawn_agent_tool = crate::tool::SpawnAgentTool::new(coord_addr.recipient());
    let tools: Vec<(
        reeve_adapter::Tool,
        actix::Recipient<crate::tool::InvokeTool>,
    )> = vec![(
        crate::tool::SpawnAgentTool::descriptor(),
        spawn_agent_tool.start().recipient(),
    )];

    // Agent: processes inbound envelopes via the model adapter and the tool
    // execution loop.
    let lead_agent = Agent::new(
        adapter,
        &dirs,
        snapshot,
        system_prompt,
        agent_id,
        keypair,
        tools,
    )
    .map_err(|e| DaemonError::Resource {
        component: "lead agent",
        source: Box::new(e),
    })?;
    let lead_addr = actix::Supervisor::start(move |_| lead_agent);

    let lead_addr_clone = lead_addr.clone();
    watcher_addr.do_send(WatchInbox {
        agent_id,
        inbox,
        on_quarantine: Some(Box::new(move |reason| {
            lead_addr.do_send(crate::agent::QuarantineEvent { reason });
        })),
        recipient: lead_addr_clone.recipient(),
    });

    Ok(dispatcher_addr)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    #[cfg(unix)]
    use super::wait_for_exit;
    use super::{confirm_started, daemon_status, prepare_agent_startup, DaemonError, DaemonStatus};
    use crate::agent_registry::tests::registry_path_for_data_dir;
    use crate::test_support::{build_registries, MockAdapter};

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
    // Marked #[ignore] because SPAWN_CONFIRM_TIMEOUT is 15s and this test
    // waits the full duration. Run explicitly with `cargo test -- --ignored`
    // when verifying timeout behaviour.
    #[test]
    #[ignore = "SPAWN_CONFIRM_TIMEOUT is 15s; run with --ignored when verifying timeout behaviour"]
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

    // ── prepare_agent_startup tests ───────────────────────────────────────────

    // A1: prepare_agent_startup succeeds with default configs and a matching adapter.
    #[test]
    fn prepare_agent_startup_succeeds_with_defaults_and_matching_adapter() {
        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let agent_registry_path = registry_path_for_data_dir(&data_dir);
        let (identity_registry, watcher, _) = build_registries(&data_dir);
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        let result = prepare_agent_startup(
            &data_dir,
            &identity_registry,
            watcher,
            &adapter,
            agent_registry_path,
        );
        assert!(
            result.is_ok(),
            "prepare_agent_startup should succeed with defaults; got: {:?}",
            result.err().map(|e| e.to_string())
        );
    }

    // A2: prepare_agent_startup returns Resource error when no adapter matches.
    #[test]
    fn prepare_agent_startup_fails_with_no_matching_adapter() {
        struct WrongAdapter;

        #[async_trait::async_trait]
        impl reeve_adapter::Adapter for WrongAdapter {
            fn id(&self) -> &'static str {
                "other-model@route"
            }

            fn capabilities(&self) -> reeve_adapter::Capabilities {
                reeve_adapter::Capabilities::new()
            }

            async fn call(
                &self,
                _: &[reeve_adapter::Message],
                _: &[reeve_adapter::Tool],
                _: &reeve_adapter::Params,
            ) -> Result<reeve_adapter::Response, reeve_adapter::AdapterError> {
                Err(reeve_adapter::AdapterError::BadRequest {
                    message: String::from("mock"),
                })
            }
        }

        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let agent_registry_path = registry_path_for_data_dir(&data_dir);
        let (identity_registry, watcher, _) = build_registries(&data_dir);
        let adapter: Arc<dyn reeve_adapter::Adapter> = Arc::new(WrongAdapter);

        let result = prepare_agent_startup(
            &data_dir,
            &identity_registry,
            watcher,
            &adapter,
            agent_registry_path,
        );
        let is_model_resolution_err = matches!(
            result,
            Err(DaemonError::Resource {
                component: "model resolution",
                ..
            })
        );
        assert!(
            is_model_resolution_err,
            "expected Resource model resolution error"
        );
    }

    // A3: prepare_agent_startup produces the same agent_id and keypair across two
    // calls against the same data directory (durable identity).
    #[test]
    fn prepare_agent_startup_uses_stable_identity_across_calls() {
        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        let (identity_registry1, watcher1, _) = build_registries(&data_dir);
        let first = prepare_agent_startup(
            &data_dir,
            &identity_registry1,
            watcher1,
            &adapter,
            registry_path_for_data_dir(&data_dir),
        )
        .expect("first prepare_agent_startup should succeed");
        let first_id = first.agent_id;
        let first_public = *first.keypair.public();

        let (identity_registry2, watcher2, _) = build_registries(&data_dir);
        let second = prepare_agent_startup(
            &data_dir,
            &identity_registry2,
            watcher2,
            &adapter,
            registry_path_for_data_dir(&data_dir),
        )
        .expect("second prepare_agent_startup should succeed");

        assert_eq!(
            first_id, second.agent_id,
            "agent_id must be stable across restarts"
        );
        assert_eq!(
            first_public,
            *second.keypair.public(),
            "keypair public key must be stable across restarts"
        );

        // The identity registry must contain exactly one entry for agent_id
        // after the second call — no duplication.
        let stored = identity_registry2
            .lookup(second.agent_id)
            .expect("identity registry lookup must not fail")
            .expect("identity registry must contain the agent_id after second call");
        assert_eq!(
            stored.identity().identity_id,
            second.agent_id,
            "stored identity id must match agent_id"
        );
        let all = identity_registry2
            .list()
            .expect("identity registry list must not fail");
        assert_eq!(
            all.len(),
            1,
            "identity registry must have exactly one entry, not duplicated; got: {all:?}"
        );

        // The agent registry must show the lead record with status Running
        // after the second call.
        let agent_registry =
            crate::agent_registry::AgentRegistry::open(registry_path_for_data_dir(&data_dir))
                .unwrap();
        let record = agent_registry
            .lookup("lead")
            .expect("lead record must be present in agent registry after second call");
        assert_eq!(
            record.status,
            crate::agent_registry::AgentStatus::Running,
            "lead agent status must be Running after second prepare_agent_startup"
        );
    }

    // A4: When the on-disk keypair does not match the identity-registry public key,
    // prepare_agent_startup must return a Resource("keypair mismatch") error.
    // This exercises the mismatch branch at the keypair-verification step.
    #[test]
    fn prepare_agent_startup_rejects_mismatched_keypair() {
        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        // First call: establishes identity and writes the keypair file.
        let (identity_registry1, watcher1, _) = build_registries(&data_dir);
        let first = prepare_agent_startup(
            &data_dir,
            &identity_registry1,
            watcher1,
            &adapter,
            registry_path_for_data_dir(&data_dir),
        )
        .expect("first call should succeed");

        // Overwrite the keypair file with a freshly generated keypair so that
        // the on-disk key no longer matches the identity-registry entry.
        let new_keypair = reeve_types::Keypair::generate();
        let key_path = first.dirs.identity_key_path();
        let seed = new_keypair.private().to_seed_bytes();
        // Direct write simulates out-of-band key replacement (e.g. manual restore
        // from backup) without going through generate_or_load_keypair.
        fs::write(&key_path, seed.as_ref()).expect("overwrite keypair file");

        // Second call: must detect the mismatch and return an error.
        let (identity_registry2, watcher2, _) = build_registries(&data_dir);
        let result = prepare_agent_startup(
            &data_dir,
            &identity_registry2,
            watcher2,
            &adapter,
            registry_path_for_data_dir(&data_dir),
        );
        assert!(
            matches!(
                result,
                Err(DaemonError::Resource {
                    component: "keypair mismatch",
                    ..
                })
            ),
            "mismatched keypair must produce Resource(keypair mismatch); got: {:?}",
            result.err().map(|e| e.to_string())
        );
    }
}
