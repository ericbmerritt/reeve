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

use tracing::{debug, info, warn};

use crate::agent_fs::{AgentDirs, RuntimeLayout};
use crate::agent_registry::{generate_or_load_keypair, AgentRecord, AgentRegistry, AgentStatus};
use crate::audit::AuditLog;
use crate::blacklist::BlacklistRegistry;
use crate::capability::{load_capability_profile, write_capability_profile, ProfileError};
use crate::config::{install_defaults, load_persona_config, load_team_config};
use crate::dispatcher::{MessageDispatcher, SendMessage};
use crate::identity_registry::{IdentityRegistry, StoredIdentity};
use crate::inbox::AgentInbox;
use crate::ledger::{DeliveryLedger, ReplayLedger};
use crate::model_resolution::SpawnSnapshot;
use crate::runtime_lock::{RuntimeLock, RuntimeLockError};
use crate::spawn_coordinator::{SpawnCoordinator, SpawnRequest};
use crate::supervisor::{HeartbeatActor, WatchInbox, WatcherActor};
use crate::system_registry::{SystemActorRecord, SystemRegistry};
use crate::tool::BlacklistHandle;
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

/// Send SIGTERM to the running daemon and wait up to `STOP_TIMEOUT` for it to exit.
///
/// Returns [`DaemonError::NoRuntime`] when no daemon is running. Returns
/// [`DaemonError::Signal`] when the kill command fails. Returns
/// [`DaemonError::Timeout`] if the daemon does not exit within `STOP_TIMEOUT`.
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
    adapters: &[Arc<dyn reeve_adapter::Adapter>],
) -> Result<(), DaemonError> {
    info!(pid = std::process::id(), "daemon starting");
    let _lock = acquire_lock(state_dir.clone())?;
    match crate::agent_fs::migrate_legacy_identities_nesting(data_dir) {
        Ok(moved) if !moved.is_empty() => {
            info!(?moved, "migrated legacy identities/ nesting to data root");
        }
        Ok(_) => {}
        Err(e) => {
            return Err(DaemonError::Resource {
                component: "data-root layout migration",
                source: Box::new(e),
            });
        }
    }
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
        adapters,
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

/// Open all persistent resources needed by the daemon. `data_dir` is the
/// reeve data root; identity TOMLs live in its `identities/` subdirectory,
/// the ledgers and audit log at the root itself.
fn open_resources(data_dir: &Path) -> Result<Resources, DaemonError> {
    let registry =
        IdentityRegistry::open(RuntimeLayout::new(data_dir).identities_root()).map_err(|e| {
            DaemonError::Resource {
                component: "identity registry",
                source: Box::new(e),
            }
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
    adapters: &[Arc<dyn reeve_adapter::Adapter>],
    agent_registry_path: PathBuf,
) -> Result<(), DaemonError> {
    // Prepare everything that can fail before entering the actix runtime.
    // Agent startup failures surface here with structured errors rather than
    // being swallowed inside block_on.
    let startup = prepare_agent_startup(
        data_dir,
        identity_registry,
        watcher,
        adapters,
        agent_registry_path,
    )?;

    #[cfg(unix)]
    {
        debug!("actix system starting");
        let mut launch_err: Option<DaemonError> = None;
        actix::System::new().block_on(async {
            // _dispatcher_addr keeps the actor alive for the duration of the
            // system.
            let _dispatcher_addr = match launch_actors(state_dir, startup).await {
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
            let _dispatcher_addr = match launch_actors(state_dir, startup).await {
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

    // Unlike an ordinary spawned agent, the estate coordinator has no
    // `AgentRegistry` status to go stale on shutdown — it lives in
    // `SystemRegistry`, which carries no lifecycle field. Nothing to mark
    // here.

    Ok(())
}

/// Pre-computed inputs for [`launch_actors`]; produced by the fallible
/// [`prepare_agent_startup`] step that runs before the actix system starts.
///
/// No agent identity is minted here — only the estate coordinator's
/// system-actor identity (see [`crate::system_registry`]). The default
/// team's members (including whichever one holds the lead role) are minted
/// inside [`launch_actors`] through the ordinary spawn-coordinator path —
/// see `crate::estate::form_team` — once that path exists to mint through;
/// there is no bespoke lead-agent bootstrap left to precompute.
struct AgentStartup {
    /// All adapters available to the daemon; used by the subagent resume path
    /// to match each subagent's snapshotted `adapter_id`, and by the spawn
    /// coordinator to resolve a persona's preferred model at mint time.
    adapters: Vec<Arc<dyn reeve_adapter::Adapter>>,
    agent_registry_path: PathBuf,
    watcher: Arc<Watcher>,
    data_dir: PathBuf,
    identity_registry: Arc<IdentityRegistry>,
    /// The operator's identity, threaded to the spawn coordinator so it can
    /// distinguish operator-sourced from peer-sourced spawns, and used as
    /// the `sender_id` when the daemon forms the default team on first boot.
    operator_id: reeve_types::IdentityId,
    /// Team-config byte cap on caller-supplied `system_prompt` at spawn.
    max_system_prompt_bytes: usize,
    /// The estate coordinator's durable identity.
    estate_id: reeve_types::IdentityId,
    /// Inbox handle for the estate coordinator's provisioned maildir.
    estate_inbox: AgentInbox,
}

/// Look up a named system actor in the system registry, verifying the
/// on-disk keypair against the identity registry; bootstrap a fresh identity
/// and register both records when the name is unseen. Used for the estate
/// coordinator's identity — the one identity the daemon itself owns and
/// bootstraps directly, rather than through the ordinary spawn path every
/// agent (including the default team's lead) mints through. System actors
/// are deliberately not `AgentRegistry` entries: no persona, no lifecycle
/// status, no incarnation — see [`crate::system_registry`].
#[expect(
    clippy::too_many_arguments,
    reason = "the function threads two registries plus the identity fields; \
              bundling into a struct trades clarity for indirection at its \
              one call site"
)]
fn ensure_named_system_identity(
    name: &str,
    identity_display_name: &str,
    inbox_dir: PathBuf,
    keypair: &reeve_types::Keypair,
    system_registry: &mut SystemRegistry,
    identity_registry: &IdentityRegistry,
    operator_id: reeve_types::IdentityId,
) -> Result<reeve_types::IdentityId, DaemonError> {
    if let Some(record) = system_registry.lookup(name) {
        let id = record.identity_id;
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
                        "agent_id found in system registry but no entry in identity registry",
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
        debug!(identity_id = %id, name, "reusing existing identity");
        Ok(id)
    } else {
        let identity =
            reeve_types::Identity::new_system(identity_display_name.to_owned(), operator_id)
                .map_err(|e| DaemonError::Resource {
                    component: "system identity",
                    source: Box::new(e),
                })?;
        let system_id = identity.identity_id;
        let public_key = *keypair.public();
        let key_record = reeve_types::KeyRecord::new(system_id, public_key).map_err(|e| {
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
        system_registry
            .register(SystemActorRecord {
                name: name.to_owned(),
                identity_id: system_id,
                inbox_dir,
            })
            .map_err(|e| DaemonError::Resource {
                component: "system registry register",
                source: Box::new(e),
            })?;
        debug!(identity_id = %system_id, name, "registered new identity");
        Ok(system_id)
    }
}

/// Fallible preparation: load configs, provision directories, resolve the
/// model. The default team's members are minted later, inside
/// [`launch_actors`], through the ordinary spawn-coordinator path.
///
/// None of this requires the actix runtime to be running, so errors can be
/// propagated normally.
fn prepare_agent_startup(
    data_dir: &Path,
    identity_registry: &Arc<IdentityRegistry>,
    watcher: Arc<Watcher>,
    adapters: &[Arc<dyn reeve_adapter::Adapter>],
    agent_registry_path: PathBuf,
) -> Result<AgentStartup, DaemonError> {
    // 1. Install default configs if they do not already exist.
    install_defaults(data_dir).map_err(|e| DaemonError::Resource {
        component: "config defaults",
        source: Box::new(e),
    })?;
    debug!("default configs ready");

    // 2. Load the default team config, needed only for its system-prompt
    // byte cap here — member enumeration happens inside `form_team` itself
    // when `launch_actors` forms the team.
    let layout = RuntimeLayout::new(data_dir);
    let team_path = layout.team_config_path("default");
    let team = load_team_config(&team_path).map_err(|e| DaemonError::Resource {
        component: "team config",
        source: Box::new(e),
    })?;

    let mut system_registry =
        SystemRegistry::open(RuntimeLayout::new(data_dir).system_registry_path()).map_err(|e| {
            DaemonError::Resource {
                component: "system registry",
                source: Box::new(e),
            }
        })?;

    // The operator identity anchors the estate coordinator's `created_by`
    // and is the `sender_id` `launch_actors` forms the default team with.
    // The reeve-cli first-run flow refuses to start the daemon without an
    // enrolled operator, so a miss here means the identity registry was
    // tampered with between enrollment and daemon start.
    let operator_id = {
        let all_identities = identity_registry
            .list()
            .map_err(|e| DaemonError::Resource {
                component: "identity registry list",
                source: Box::new(e),
            })?;
        all_identities
            .iter()
            .find(|s| s.identity().identity_type == reeve_types::IdentityType::Operator)
            .map(|s| s.identity().identity_id)
            .ok_or_else(|| DaemonError::Resource {
                component: "operator lookup",
                source: Box::<dyn std::error::Error + Send + Sync>::from(
                    "no operator identity enrolled; run `reeve identity enroll` before starting the daemon",
                ),
            })?
    };

    // The estate coordinator gets a provisioned inbox and durable identity
    // like an agent, but is registered in `SystemRegistry`, not
    // `AgentRegistry` — it is not model-backed, has no persona, and never
    // has an incarnation. `launch_actors` starts the coordinator actor on
    // this inbox directly instead of resuming it through the ordinary
    // agent resume pass.
    let estate_dirs =
        AgentDirs::provision(data_dir, crate::estate::ESTATE_AGENT_NAME).map_err(|e| {
            DaemonError::Resource {
                component: "estate dirs",
                source: Box::new(e),
            }
        })?;
    let estate_keypair =
        generate_or_load_keypair(&estate_dirs.identity_key_path()).map_err(|e| {
            DaemonError::Resource {
                component: "estate keypair",
                source: Box::new(e),
            }
        })?;
    let estate_id = ensure_named_system_identity(
        crate::estate::ESTATE_AGENT_NAME,
        crate::estate::ESTATE_AGENT_NAME,
        estate_dirs.inbox_root(),
        &estate_keypair,
        &mut system_registry,
        identity_registry,
        operator_id,
    )?;
    let estate_inbox = AgentInbox::from_path(estate_dirs.inbox_root());

    Ok(AgentStartup {
        adapters: adapters.to_vec(),
        agent_registry_path,
        watcher,
        data_dir: data_dir.to_path_buf(),
        identity_registry: Arc::clone(identity_registry),
        operator_id,
        max_system_prompt_bytes: team.max_system_prompt_bytes(),
        estate_id,
        estate_inbox,
    })
}

/// Start all supervised actors inside the running actix system.
///
/// Must be called from within an actix `block_on` context.
#[expect(
    clippy::too_many_lines,
    reason = "linear actor wiring sequence: heartbeat, watcher, dispatcher, \
              spawn coordinator, estate coordinator, persisted-subagent \
              resume, and (first boot only) default-team formation. Each \
              step depends on outputs of the previous; splitting just to \
              chase a line budget would obscure the order."
)]
async fn launch_actors(
    state_dir: PathBuf,
    startup: AgentStartup,
) -> Result<actix::Addr<MessageDispatcher>, DaemonError> {
    let AgentStartup {
        adapters,
        agent_registry_path,
        watcher,
        data_dir,
        identity_registry,
        operator_id,
        max_system_prompt_bytes,
        estate_id,
        estate_inbox,
    } = startup;

    // HeartbeatActor: touches the heartbeat file every second.
    actix::Supervisor::start(move |_| HeartbeatActor::new(state_dir));

    let watcher_for_coord = Arc::clone(&watcher);

    // Create the blacklist handle before starting the WatcherActor so the
    // actor can reload it on every 250ms poll without needing a message send.
    let blacklist_path_for_watcher = RuntimeLayout::new(&data_dir).blacklist_path();
    let initial_blacklist = match BlacklistRegistry::load_from_path(&blacklist_path_for_watcher) {
        Ok(r) => {
            debug!(path = %blacklist_path_for_watcher.display(), entries = r.len(), "loaded blacklist");
            r
        }
        Err(crate::blacklist::BlacklistError::Io { ref source, .. })
            if source.kind() == io::ErrorKind::NotFound =>
        {
            debug!("no blacklist.toml at startup; starting with empty blacklist");
            BlacklistRegistry::empty()
        }
        Err(err) => {
            warn!(err = %err, "failed to load blacklist.toml at startup; starting empty");
            BlacklistRegistry::empty()
        }
    };
    let blacklist_handle: BlacklistHandle = Arc::new(std::sync::RwLock::new(initial_blacklist));

    let audit_shared: Arc<AuditLog> = Arc::new(
        AuditLog::open(data_dir.clone()).unwrap_or_else(|e| panic!("cannot open audit log: {e}")),
    );
    let audit_for_watcher = Arc::clone(&audit_shared);
    let blacklist_handle_for_watcher = Arc::clone(&blacklist_handle);
    let watcher_addr = actix::Supervisor::start(move |_| {
        WatcherActor::new(Arc::clone(&watcher)).with_blacklist(
            blacklist_path_for_watcher,
            blacklist_handle_for_watcher,
            audit_for_watcher,
        )
    });

    let control_routes = crate::agent::ControlRoutes::default();

    // The dispatcher re-opens the agent registry on every dispatch so it
    // stays in lockstep with records the spawn coordinator persists at
    // runtime; pre-flighting an open here only validates the path is well
    // formed and surfaces config errors at startup rather than at first send.
    AgentRegistry::open(agent_registry_path.clone()).map_err(|e| DaemonError::Resource {
        component: "message dispatcher registry",
        source: Box::new(e),
    })?;
    let identity_registry_for_dispatcher = Arc::clone(&identity_registry);
    let dispatcher_registry_path = agent_registry_path.clone();
    let dispatcher_addr = actix::Supervisor::start(move |_| {
        MessageDispatcher::new(
            dispatcher_registry_path.clone(),
            Arc::clone(&identity_registry_for_dispatcher),
        )
    });

    // Keep clones for the subagent re-launch pass below; the spawn
    // coordinator consumes its own handles.
    let data_dir_for_resume = data_dir.clone();
    let agent_registry_path_for_resume = agent_registry_path.clone();
    let identity_registry_for_resume = Arc::clone(&identity_registry);
    let adapters_for_resume = adapters.clone();
    let watcher_for_resume = Arc::clone(&watcher_for_coord);
    let watcher_addr_for_resume = watcher_addr.clone();
    let dispatcher_recipient_for_resume = dispatcher_addr.clone().recipient();

    let spawn_coordinator = SpawnCoordinator::new(
        data_dir,
        agent_registry_path,
        identity_registry,
        adapters.clone(),
        Arc::clone(&audit_shared),
        watcher_for_coord,
        watcher_addr.clone().recipient(),
        dispatcher_addr.clone().recipient(),
        Some(Arc::clone(&blacklist_handle)),
        operator_id,
        max_system_prompt_bytes,
    )
    .with_control_routes(control_routes.clone());
    let coord_addr = actix::Supervisor::start(move |_| spawn_coordinator);
    let coord_recipient_for_resume = coord_addr.clone();

    // EstateCoordinator: operator-tier organizational operations arriving as
    // signed envelopes on the reserved `estate` inbox. Started after the
    // spawn coordinator because team formation mints members through it.
    let layout_for_estate = RuntimeLayout::new(&data_dir_for_resume);
    let engagements = crate::engagement::EngagementRegistry::open(
        layout_for_estate.engagements_root(),
    )
    .map_err(|e| DaemonError::Resource {
        component: "engagement registry",
        source: Box::new(e),
    })?;
    let teams = crate::team::TeamRegistry::open(layout_for_estate.rosters_root()).map_err(|e| {
        DaemonError::Resource {
            component: "team registry",
            source: Box::new(e),
        }
    })?;
    let estate_team_ops = crate::estate::EstateOpsDeps {
        spawner: coord_addr.clone().recipient(),
        teams,
        engagements: engagements.clone(),
        control_routes: control_routes.clone(),
        agent_registry_path: agent_registry_path_for_resume.clone(),
        data_dir: data_dir_for_resume.clone(),
        identity_registry: Arc::clone(&identity_registry_for_resume),
        adapters: adapters_for_resume.clone(),
        watcher: Arc::clone(&watcher_for_resume),
        inbox_starter: watcher_addr_for_resume.clone().recipient(),
        dispatcher: dispatcher_recipient_for_resume.clone(),
        blacklist: Some(Arc::clone(&blacklist_handle)),
    };

    // Re-launch any agent the previous daemon left in the registry, before
    // forming the default team below: on a fresh install the registry is
    // empty (nothing to resume, no-op); on every later boot this must run
    // first so the default team's already-formed members are resumed here
    // rather than raced by a second, freshly-minted instance from the
    // form-team call that follows.
    let resume_inbox_starter = watcher_addr_for_resume.recipient();
    resume_persisted_subagents(
        &data_dir_for_resume,
        &agent_registry_path_for_resume,
        &identity_registry_for_resume,
        &adapters_for_resume,
        Some(&blacklist_handle),
        &audit_shared,
        &watcher_for_resume,
        &resume_inbox_starter,
        &dispatcher_recipient_for_resume,
        Some(&coord_recipient_for_resume.recipient()),
        Some(&control_routes),
    );

    // Form the default team from teams/default.toml on first boot. Refuses
    // (audited `name_taken`, not an error) and is a no-op on every later
    // boot once the roster already exists — the roster's existence is the
    // sole gate, so this is safe to call unconditionally on every start.
    // Members mint through the ordinary spawn-coordinator path with no
    // special-casing for the lead role; `reeve team form` (operator-
    // triggered) goes through the identical `form_team` call.
    crate::estate::form_team(
        &estate_team_ops,
        &audit_shared,
        operator_id,
        "default",
        "default",
        time::OffsetDateTime::now_utc(),
    )
    .await;

    let estate_audit = Arc::clone(&audit_shared);
    let estate_addr = actix::Supervisor::start(move |_| {
        crate::estate::EstateCoordinator::new(operator_id, engagements, estate_audit)
            .with_team_ops(estate_team_ops)
    });
    watcher_addr.do_send(WatchInbox {
        agent_id: estate_id,
        inbox: estate_inbox,
        on_quarantine: None,
        recipient: estate_addr.recipient(),
    });

    Ok(dispatcher_addr)
}

/// Re-launch every agent in the registry. The estate coordinator is never a
/// member of this registry (see [`crate::system_registry`]), so it never
/// needs excluding here. Called once on daemon start, before the default
/// team is (re-)formed —
/// see `launch_actors`'s ordering comment. Each record is treated as a
/// best-effort resume: per-agent failures (missing snapshot, persona
/// removed, adapter id mismatch, keypair drift) are logged and skipped
/// rather than failing the whole daemon — one corrupt persisted agent
/// must not prevent the rest of the estate from coming up.
///
/// Symmetry with the spawn-time path: this performs steps L–P of the
/// spawn coordinator (build tools, construct Agent, supervise, register
/// route, watch inbox) without redoing A–K (validation, mint identity,
/// write snapshot) which already ran when the operator spawned the agent.
/// Why a subagent could not be re-launched during the daemon-restart resume
/// pass. `ProfileMissing` is distinguished because it has a defined operator
/// recovery (write the persona profile, restart) and is recorded as the
/// agent's `stopped_reason`; every other failure collapses to `Other`.
enum ResumeError {
    /// Neither the agent's profile snapshot nor the persona profile it would
    /// be synthesized from exists. The agent is left `Stopped` with
    /// `stopped_reason = "profile_missing"`.
    ProfileMissing,
    /// Any other resume failure (open dirs, key mismatch, parse, adapter, …).
    Other(String),
}

impl std::fmt::Display for ResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProfileMissing => f.write_str(
                "agent profile.toml missing and persona profile.toml absent; \
                 cannot synthesize a capability profile",
            ),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl From<String> for ResumeError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the function wires together every piece of daemon state a \
              subagent's lifecycle touches; bundling into a struct trades \
              clarity for indirection at the one call site"
)]
fn resume_persisted_subagents(
    data_dir: &Path,
    agent_registry_path: &Path,
    identity_registry: &Arc<IdentityRegistry>,
    adapters: &[Arc<dyn reeve_adapter::Adapter>],
    blacklist: Option<&BlacklistHandle>,
    audit: &Arc<AuditLog>,
    watcher: &Arc<Watcher>,
    inbox_starter: &actix::Recipient<WatchInbox>,
    dispatcher: &actix::Recipient<SendMessage>,
    coordinator: Option<&actix::Recipient<SpawnRequest>>,
    control_routes: Option<&crate::agent::ControlRoutes>,
) {
    let registry = match AgentRegistry::open(agent_registry_path.to_path_buf()) {
        Ok(r) => r,
        Err(err) => {
            warn!(err = %err, "resume: failed to open agent registry; skipping subagent resume");
            return;
        }
    };

    for record in registry.list() {
        // Retirement is the deliberate end of an identity: never resumed,
        // unlike Stopped (which is retried each boot in case the failure
        // was transient).
        if matches!(record.status, AgentStatus::Retired) {
            continue;
        }
        if let Err(err) = resume_one_subagent(
            data_dir,
            agent_registry_path,
            identity_registry,
            adapters,
            blacklist,
            audit,
            watcher,
            inbox_starter,
            dispatcher,
            coordinator.cloned(),
            control_routes,
            record,
        ) {
            warn!(
                agent_name = %record.name,
                err = %err,
                "resume: failed to re-launch subagent; marking stopped"
            );
            // Mark the record stopped so the panopticon shows an accurate
            // status and the operator doesn't send messages to a dead inbox.
            // A missing profile gets a named reason the operator can act on;
            // other failures stop without a specific reason.
            if let Ok(mut reg) = AgentRegistry::open(agent_registry_path.to_path_buf()) {
                let _ = match err {
                    ResumeError::ProfileMissing => {
                        reg.update_stopped_with_reason(record.name.as_str(), "profile_missing")
                    }
                    ResumeError::Other(_) => {
                        reg.update_status(record.name.as_str(), AgentStatus::Stopped)
                    }
                };
            }
        }
    }
}

/// Single-agent resume. Returns a string error description on failure; the
/// caller logs the error context and moves on. Symmetric with the steps
/// `SpawnCoordinator::handle` performs from `build_subagent_tools` onward.
#[expect(
    clippy::too_many_arguments,
    reason = "subagent resume needs every collaborator the spawn-time path \
              uses minus the parts already done at spawn (identity mint, \
              registry write); bundling into a context struct trades \
              clarity for indirection at the only call site"
)]
#[expect(
    clippy::too_many_lines,
    reason = "linear sequence of guards: each step depends on the previous; \
              splitting on line count would fragment the error-handling chain"
)]
fn resume_one_subagent(
    data_dir: &Path,
    agent_registry_path: &Path,
    identity_registry: &Arc<IdentityRegistry>,
    adapters: &[Arc<dyn reeve_adapter::Adapter>],
    blacklist: Option<&BlacklistHandle>,
    audit: &Arc<AuditLog>,
    watcher: &Arc<Watcher>,
    inbox_starter: &actix::Recipient<WatchInbox>,
    dispatcher: &actix::Recipient<SendMessage>,
    coordinator: Option<actix::Recipient<SpawnRequest>>,
    control_routes: Option<&crate::agent::ControlRoutes>,
    record: &AgentRecord,
) -> Result<(), ResumeError> {
    let dirs = AgentDirs::open(data_dir, record.name.as_str())
        .map_err(|e| format!("open agent dirs: {e}"))?;

    let keypair = generate_or_load_keypair(&dirs.identity_key_path())
        .map_err(|e| format!("load keypair: {e}"))?;

    // Verify the identity registry still has this agent and the on-disk key
    // matches what was registered at spawn time. If they don't match the
    // recipient verification on every envelope would fail anyway.
    let stored = identity_registry
        .lookup(record.identity_id)
        .map_err(|e| format!("identity lookup: {e}"))?
        .ok_or_else(|| "identity registry has no entry for this agent".to_owned())?;
    let stored_key = &stored
        .key_records()
        .first()
        .ok_or_else(|| "stored identity has no key records".to_owned())?
        .public_key;
    if keypair.public() != stored_key {
        return Err(ResumeError::Other(
            "on-disk identity.key does not match stored public key for this agent".to_owned(),
        ));
    }

    // Load the snapshot to recover the resolved adapter id and the composed
    // system prompt the agent was originally spawned with.
    let snapshot_text = std::fs::read_to_string(dirs.agent_toml_path())
        .map_err(|e| format!("read agent.toml: {e}"))?;
    let snapshot: SpawnSnapshot =
        toml::from_str(&snapshot_text).map_err(|e| format!("parse agent.toml: {e}"))?;

    // Find the adapter matching this agent's snapshot. A mismatch means the
    // daemon was reconfigured without the adapter that spawned this agent, or
    // the snapshot was written by a different deployment. Skip rather than
    // fail the whole resume pass.
    let adapter = adapters
        .iter()
        .find(|a| a.id() == snapshot.adapter_id)
        .ok_or_else(|| {
            let available: Vec<&str> = adapters.iter().map(|a| a.id()).collect();
            format!(
                "no running adapter matches snapshot adapter_id '{}'; available: [{}]",
                snapshot.adapter_id,
                available.join(", ")
            )
        })?;

    // System prompt fallback: snapshots written before the field existed
    // serialize with system_prompt == "". Use the persona's base prompt in
    // that case so an upgrade does not blank the agent out.
    let system_prompt = if snapshot.system_prompt.is_empty() {
        let persona_name = record
            .persona_name
            .as_deref()
            .unwrap_or(&snapshot.persona_name);
        let persona_path = RuntimeLayout::new(data_dir).persona_config_path(persona_name);
        load_persona_config(&persona_path)
            .map_err(|e| format!("load persona for prompt fallback: {e}"))?
            .system_prompt
    } else {
        snapshot.system_prompt.clone()
    };

    // Recover the agent's capability profile. Prefer the immutable snapshot
    // written at spawn. If it's absent (e.g. an agent that predates the
    // per-agent snapshot), synthesize one from the persona's current profile
    // — the single documented exception to "snapshot at spawn time" — and
    // persist it so later restarts read a stable snapshot. If the persona
    // profile is also absent, refuse the resume rather than running unenforced
    // (no permissive fallback); the caller stops the agent with
    // `stopped_reason = "profile_missing"`.
    let profile = match load_capability_profile(&dirs.profile_path()) {
        Ok(p) => Some(Arc::new(p)),
        // Only a genuinely absent snapshot synthesizes from the persona (the
        // one documented upgrade exception). A present-but-unreadable snapshot
        // — parse error, bad permissions, unsupported version, symlink — is
        // corruption we must not paper over by overwriting it: that would hide
        // the corruption and could silently widen or alter enforcement if the
        // persona profile has since diverged. Fail the resume instead.
        Err(ProfileError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            let persona_name = record
                .persona_name
                .as_deref()
                .unwrap_or(&snapshot.persona_name);
            let persona_profile_path =
                RuntimeLayout::new(data_dir).persona_profile_path(persona_name);
            let synthesized = match load_capability_profile(&persona_profile_path) {
                Ok(p) => p,
                Err(ProfileError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    return Err(ResumeError::ProfileMissing);
                }
                Err(err) => {
                    return Err(ResumeError::Other(format!(
                        "load persona profile for synthesis: {err}"
                    )));
                }
            };
            write_capability_profile(&dirs.profile_path(), &synthesized)
                .map_err(|e| format!("write synthesized profile snapshot: {e}"))?;
            warn!(
                agent_name = %record.name,
                persona = %persona_name,
                "resume: agent profile.toml missing; synthesized snapshot from persona profile"
            );
            Some(Arc::new(synthesized))
        }
        Err(err) => {
            return Err(ResumeError::Other(format!(
                "load agent profile snapshot: {err}"
            )));
        }
    };
    crate::spawn_coordinator::launch_incarnation(
        Arc::clone(adapter),
        &dirs,
        snapshot,
        system_prompt,
        record.identity_id,
        keypair,
        profile,
        data_dir,
        record.name.as_str(),
        agent_registry_path,
        watcher,
        control_routes,
        coordinator,
        dispatcher,
        blacklist,
        inbox_starter,
        audit,
    )
    .map_err(ResumeError::Other)?;

    tracing::info!(
        agent_name = %record.name,
        identity_id = %record.identity_id,
        "resume: re-launched persisted subagent"
    );
    Ok(())
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
    use crate::agent_fs::{AgentDirs, RuntimeLayout};
    use crate::agent_registry::tests::registry_path_for_data_dir;
    use crate::agent_registry::{
        generate_or_load_keypair, AgentRecord, AgentRegistry, AgentStatus, ValidatedAgentName,
    };
    use crate::audit::AuditLog;
    use crate::identity_registry::StoredIdentity;
    use crate::model_resolution::{write_spawn_snapshot, SpawnSnapshot};
    use crate::test_support::{build_registries, enroll_test_operator, MockAdapter};

    use super::resume_persisted_subagents;

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
        enroll_test_operator(&identity_registry);
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        let result = prepare_agent_startup(
            &data_dir,
            &identity_registry,
            watcher,
            std::slice::from_ref(&adapter),
            agent_registry_path,
        );
        assert!(
            result.is_ok(),
            "prepare_agent_startup should succeed with defaults; got: {:?}",
            result.err().map(|e| e.to_string())
        );
    }

    // A3: prepare_agent_startup produces the same estate_id across two calls
    // against the same data directory (durable identity). The default
    // team's lead-role member is no longer minted here — it goes through
    // the ordinary spawn path inside `launch_actors` — so this only covers
    // the one identity `prepare_agent_startup` still bootstraps directly.
    #[test]
    fn prepare_agent_startup_uses_stable_estate_identity_across_calls() {
        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        let (identity_registry1, watcher1, _) = build_registries(&data_dir);
        enroll_test_operator(&identity_registry1);
        let first = prepare_agent_startup(
            &data_dir,
            &identity_registry1,
            watcher1,
            std::slice::from_ref(&adapter),
            registry_path_for_data_dir(&data_dir),
        )
        .expect("first prepare_agent_startup should succeed");

        let (identity_registry2, watcher2, _) = build_registries(&data_dir);
        let second = prepare_agent_startup(
            &data_dir,
            &identity_registry2,
            watcher2,
            std::slice::from_ref(&adapter),
            registry_path_for_data_dir(&data_dir),
        )
        .expect("second prepare_agent_startup should succeed");

        assert_eq!(
            first.estate_id, second.estate_id,
            "estate coordinator identity must be stable across restarts"
        );

        // The identity registry must contain exactly one System-typed entry
        // after the second call — the estate coordinator, with no
        // duplication on the second bootstrap. (The other entry is the test
        // operator enrolled before the first call.)
        let stored = identity_registry2
            .lookup(second.estate_id)
            .expect("identity registry lookup must not fail")
            .expect("identity registry must contain the estate_id after second call");
        assert_eq!(
            stored.identity().identity_id,
            second.estate_id,
            "stored identity id must match estate_id"
        );
        assert_eq!(
            stored.identity().identity_type,
            reeve_types::IdentityType::System,
            "estate coordinator identity must be System-typed, not Agent"
        );
        let all = identity_registry2
            .list()
            .expect("identity registry list must not fail");
        let system_actors: Vec<_> = all
            .iter()
            .filter(|s| s.identity().identity_type == reeve_types::IdentityType::System)
            .collect();
        assert_eq!(
            system_actors.len(),
            1,
            "identity registry must contain exactly one System-typed entry \
             (estate), not duplicated; got: {system_actors:?}"
        );
    }

    // A3b: regression for the crash where `estate` appeared in
    // `AgentRegistry` (no `agent.toml`, so any chat-style submit against it
    // hit a raw IO NotFound). Estate must be resolvable via `SystemRegistry`
    // and absent from `AgentRegistry` entirely — not filtered out, just
    // never there.
    #[test]
    fn estate_is_registered_as_system_actor_not_agent_registry_entry() {
        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        enroll_test_operator(&identity_registry);
        let startup = prepare_agent_startup(
            &data_dir,
            &identity_registry,
            watcher,
            std::slice::from_ref(&adapter),
            agent_registry_path.clone(),
        )
        .expect("prepare_agent_startup should succeed");

        let system_registry = crate::system_registry::SystemRegistry::open(
            RuntimeLayout::new(&data_dir).system_registry_path(),
        )
        .unwrap();
        let record = system_registry
            .lookup(crate::estate::ESTATE_AGENT_NAME)
            .expect("estate must be registered in SystemRegistry");
        assert_eq!(record.identity_id, startup.estate_id);

        let agent_registry = AgentRegistry::open(agent_registry_path).unwrap();
        assert!(
            agent_registry
                .lookup(crate::estate::ESTATE_AGENT_NAME)
                .is_none(),
            "estate must not appear in AgentRegistry — that was the root cause \
             of the chat-submit crash against a non-existent agent.toml"
        );
    }

    // A4: When the on-disk keypair does not match the identity-registry public
    // key, prepare_agent_startup must return a Resource("keypair mismatch")
    // error. Exercises the mismatch branch at the keypair-verification step,
    // via the estate identity — the one identity this function still
    // bootstraps directly through `ensure_named_system_identity`.
    #[test]
    fn prepare_agent_startup_rejects_mismatched_keypair() {
        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        // First call: establishes identity and writes the estate keypair file.
        let (identity_registry1, watcher1, _) = build_registries(&data_dir);
        enroll_test_operator(&identity_registry1);
        prepare_agent_startup(
            &data_dir,
            &identity_registry1,
            watcher1,
            std::slice::from_ref(&adapter),
            registry_path_for_data_dir(&data_dir),
        )
        .expect("first call should succeed");

        // Overwrite the estate keypair file with a freshly generated keypair
        // so the on-disk key no longer matches the identity-registry entry.
        let estate_dirs = AgentDirs::open(&data_dir, crate::estate::ESTATE_AGENT_NAME)
            .expect("estate dirs must exist after first call");
        let new_keypair = reeve_types::Keypair::generate();
        let key_path = estate_dirs.identity_key_path();
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
            std::slice::from_ref(&adapter),
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

    // ── resume_persisted_subagents tests ──────────────────────────────────────

    // R1: After a subagent's on-disk fixture is set up, resume_persisted_subagents
    // re-launches it: the watcher's routing table picks up the agent's
    // identity_id. Without this, the lead's send_message to a subagent that
    // survived a daemon restart would drop the envelope into an unwatched
    // inbox/new/ and surface nothing in the logs.
    #[test]
    #[cfg(unix)]
    fn resume_persisted_subagents_registers_route_for_persisted_subagent() {
        use crate::test_support::{NullDispatcher, NullInboxStarter};
        use actix::Actor as _;

        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let operator_id = enroll_test_operator(&identity_registry);
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        // Provision a persisted "worker" subagent as if a prior daemon spawn
        // had completed: identity in registry, agent record, agent.toml
        // (SpawnSnapshot), keypair file, persona config + profile. The persona
        // profile lets the resume path recover a capability profile (the agent
        // snapshot below omits one, so resume synthesizes from the persona).
        crate::test_support::write_persona_config(&data_dir, "worker", "claude-opus-4-7");
        crate::test_support::write_full_access_persona_profile(&data_dir, "worker");
        let worker_dirs = AgentDirs::provision(&data_dir, "worker").unwrap();
        let worker_keypair = generate_or_load_keypair(&worker_dirs.identity_key_path()).unwrap();
        let worker_id = reeve_types::IdentityId::new().unwrap();

        // Identity registry entry
        {
            let mut identity =
                reeve_types::Identity::new_agent("worker".to_owned(), operator_id).unwrap();
            identity.identity_id = worker_id;
            let key_record =
                reeve_types::KeyRecord::new(worker_id, *worker_keypair.public()).unwrap();
            let stored = StoredIdentity::new(identity, key_record).unwrap();
            identity_registry.write(&stored).unwrap();
        }

        // Snapshot on disk
        let snapshot = SpawnSnapshot {
            persona_name: "worker".to_owned(),
            persona_version: 1,
            adapter_id: adapter.id().to_owned(),
            agent_id: worker_id.to_string(),
            system_prompt: "You are a worker. Reply with 'ack' to any inbound.".to_owned(),
            system_prompt_source: None,
            engagement_name: None,
            working_root: None,
        };
        write_spawn_snapshot(&worker_dirs, &snapshot).unwrap();

        // Agent registry record
        {
            let mut agent_registry = AgentRegistry::open(agent_registry_path.clone()).unwrap();
            agent_registry
                .register(AgentRecord {
                    name: ValidatedAgentName::new("worker").unwrap(),
                    identity_id: worker_id,
                    inbox_dir: worker_dirs.inbox_root(),
                    persona_name: Some("worker".to_owned()),
                    spawned_at: time::OffsetDateTime::now_utc(),
                    status: AgentStatus::Running,
                    stopped_reason: None,
                })
                .unwrap();
        }

        // Resume runs inside an actix system because it starts an actor
        // and sends messages to recipients.
        let watcher_for_assert = Arc::clone(&watcher);
        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();
            let dispatcher = NullDispatcher.start().recipient();

            let test_audit =
                Arc::new(AuditLog::open(data_dir.clone()).expect("open audit log in test"));
            resume_persisted_subagents(
                &data_dir,
                &agent_registry_path,
                &identity_registry,
                std::slice::from_ref(&adapter),
                None,
                &test_audit,
                &watcher,
                &inbox_starter,
                &dispatcher,
                None,
                None,
            );

            // resume runs synchronously: by the time it returns the worker
            // actor has been started and watcher.register_route has run.
            assert!(
                watcher.has_route(worker_id),
                "watcher must have a route for the resumed subagent; \
                 send_message would otherwise land in an unwatched inbox"
            );

            actix::System::current().stop();
        });

        // Belt-and-suspenders: same assertion on the Arc held outside the
        // System block, so a regression that registers on an inner clone
        // shows up.
        assert!(watcher_for_assert.has_route(worker_id));

        // The agent had no profile snapshot, so the resume synthesized one
        // from the persona profile and persisted it for future restarts.
        assert!(
            worker_dirs.profile_path().exists(),
            "resume should synthesize agents/worker/profile.toml from the persona profile"
        );
    }

    // R1a: the default team's lead-role member — an ordinary "lead"-persona
    // agent since phase 2, with no bespoke bootstrap of its own — resumes
    // successfully across a daemon restart using nothing but what
    // `install_defaults` ships. Regression test for a live bug found while
    // smoke-testing the phase-2 boot redesign: before `install_defaults`
    // shipped `personas/lead/profile.toml`, a lead-role member minted
    // through the ordinary spawn path got no capability-profile snapshot at
    // mint time (spawn only writes one when a persona profile resolved) and
    // then failed to resume on every subsequent boot with
    // `ProfileMissing` — silently breaking the walking-skeleton demo after
    // the very first restart.
    #[test]
    #[cfg(unix)]
    #[expect(
        clippy::too_many_lines,
        reason = "linear fixture setup mirroring R1's shape (identity, snapshot, \
                  registry record) plus the resume call and both assertions; \
                  splitting fragments a single coherent scenario"
    )]
    fn resume_succeeds_for_lead_persona_agent_after_install_defaults() {
        use crate::test_support::{NullDispatcher, NullInboxStarter};
        use actix::Actor as _;

        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let operator_id = enroll_test_operator(&identity_registry);
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        // The one thing a fresh install does before anything can be minted.
        crate::config::install_defaults(&data_dir).unwrap();

        // Provision a "default-lead" record the same way a first-boot
        // `form_team` mint would have, but — matching R1's setup — with no
        // agent-level profile.toml of its own, so this exercises the
        // synthesize-from-persona-profile fallback specifically.
        let lead_dirs = AgentDirs::provision(&data_dir, "default-lead").unwrap();
        let lead_keypair = generate_or_load_keypair(&lead_dirs.identity_key_path()).unwrap();
        let lead_id = reeve_types::IdentityId::new().unwrap();
        {
            let mut identity =
                reeve_types::Identity::new_agent("default-lead".to_owned(), operator_id).unwrap();
            identity.identity_id = lead_id;
            let key_record = reeve_types::KeyRecord::new(lead_id, *lead_keypair.public()).unwrap();
            let stored = StoredIdentity::new(identity, key_record).unwrap();
            identity_registry.write(&stored).unwrap();
        }
        let snapshot = SpawnSnapshot {
            persona_name: "lead".to_owned(),
            persona_version: 1,
            adapter_id: adapter.id().to_owned(),
            agent_id: lead_id.to_string(),
            system_prompt: "You are a helpful AI assistant.".to_owned(),
            system_prompt_source: None,
            engagement_name: None,
            working_root: None,
        };
        write_spawn_snapshot(&lead_dirs, &snapshot).unwrap();
        {
            let mut agent_registry = AgentRegistry::open(agent_registry_path.clone()).unwrap();
            agent_registry
                .register(AgentRecord {
                    name: ValidatedAgentName::new("default-lead").unwrap(),
                    identity_id: lead_id,
                    inbox_dir: lead_dirs.inbox_root(),
                    persona_name: Some("lead".to_owned()),
                    spawned_at: time::OffsetDateTime::now_utc(),
                    status: AgentStatus::Running,
                    stopped_reason: None,
                })
                .unwrap();
        }

        let agent_registry_path_for_assert = agent_registry_path.clone();
        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();
            let dispatcher = NullDispatcher.start().recipient();
            let test_audit =
                Arc::new(AuditLog::open(data_dir.clone()).expect("open audit log in test"));
            resume_persisted_subagents(
                &data_dir,
                &agent_registry_path,
                &identity_registry,
                std::slice::from_ref(&adapter),
                None,
                &test_audit,
                &watcher,
                &inbox_starter,
                &dispatcher,
                None,
                None,
            );

            assert!(
                watcher.has_route(lead_id),
                "the default team's lead role must resume across a restart \
                 using only what install_defaults ships — got ProfileMissing \
                 (no route registered) instead"
            );

            actix::System::current().stop();
        });

        let record = AgentRegistry::open(agent_registry_path_for_assert)
            .unwrap()
            .lookup("default-lead")
            .unwrap()
            .clone();
        assert_eq!(
            record.status,
            AgentStatus::Running,
            "resume must not have marked default-lead Stopped(profile_missing); \
             got stopped_reason={:?}",
            record.stopped_reason
        );
    }

    // R1b: when neither the agent's profile snapshot nor the persona profile
    // exists, the resume refuses (no permissive fallback): the agent is not
    // routable and is left Stopped with stopped_reason = "profile_missing".
    #[test]
    #[cfg(unix)]
    fn resume_marks_profile_missing_when_no_profile_anywhere() {
        use crate::test_support::{NullDispatcher, NullInboxStarter};
        use actix::Actor as _;

        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let operator_id = enroll_test_operator(&identity_registry);
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        // Provision identity, keypair, snapshot, and record — but NO agent
        // profile.toml and NO persona profile.toml.
        let worker_dirs = AgentDirs::provision(&data_dir, "worker").unwrap();
        let worker_keypair = generate_or_load_keypair(&worker_dirs.identity_key_path()).unwrap();
        let worker_id = reeve_types::IdentityId::new().unwrap();
        {
            let mut identity =
                reeve_types::Identity::new_agent("worker".to_owned(), operator_id).unwrap();
            identity.identity_id = worker_id;
            let key_record =
                reeve_types::KeyRecord::new(worker_id, *worker_keypair.public()).unwrap();
            let stored = StoredIdentity::new(identity, key_record).unwrap();
            identity_registry.write(&stored).unwrap();
        }
        let snapshot = SpawnSnapshot {
            persona_name: "worker".to_owned(),
            persona_version: 1,
            adapter_id: adapter.id().to_owned(),
            agent_id: worker_id.to_string(),
            system_prompt: "You are a worker.".to_owned(),
            system_prompt_source: None,
            engagement_name: None,
            working_root: None,
        };
        write_spawn_snapshot(&worker_dirs, &snapshot).unwrap();
        {
            let mut agent_registry = AgentRegistry::open(agent_registry_path.clone()).unwrap();
            agent_registry
                .register(AgentRecord {
                    name: ValidatedAgentName::new("worker").unwrap(),
                    identity_id: worker_id,
                    inbox_dir: worker_dirs.inbox_root(),
                    persona_name: Some("worker".to_owned()),
                    spawned_at: time::OffsetDateTime::now_utc(),
                    status: AgentStatus::Running,
                    stopped_reason: None,
                })
                .unwrap();
        }

        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();
            let dispatcher = NullDispatcher.start().recipient();
            let test_audit =
                Arc::new(AuditLog::open(data_dir.clone()).expect("open audit log in test"));
            resume_persisted_subagents(
                &data_dir,
                &agent_registry_path,
                &identity_registry,
                std::slice::from_ref(&adapter),
                None,
                &test_audit,
                &watcher,
                &inbox_starter,
                &dispatcher,
                None,
                None,
            );

            assert!(
                !watcher.has_route(worker_id),
                "an agent with no recoverable profile must not be routable"
            );

            let reg = AgentRegistry::open(agent_registry_path.clone()).unwrap();
            let rec = reg.lookup("worker").expect("record present");
            assert_eq!(rec.status, AgentStatus::Stopped);
            assert_eq!(rec.stopped_reason.as_deref(), Some("profile_missing"));

            actix::System::current().stop();
        });
    }

    // R1c: a present-but-corrupt agent profile.toml is NOT synthesized over.
    // Resume fails and leaves the corrupt snapshot untouched, surfacing the
    // corruption rather than silently replacing it — even though a valid
    // persona profile exists that synthesis could otherwise have used.
    #[test]
    #[cfg(unix)]
    fn resume_does_not_synthesize_over_corrupt_agent_profile() {
        use crate::test_support::{NullDispatcher, NullInboxStarter};
        use actix::Actor as _;

        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let operator_id = enroll_test_operator(&identity_registry);
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        // A valid persona profile exists (synthesis could succeed) ...
        crate::test_support::write_full_access_persona_profile(&data_dir, "worker");
        let worker_dirs = AgentDirs::provision(&data_dir, "worker").unwrap();
        // ... but the agent's own profile snapshot is present and corrupt.
        let corrupt = "this is not valid profile toml ===";
        fs::write(worker_dirs.profile_path(), corrupt).unwrap();

        let worker_keypair = generate_or_load_keypair(&worker_dirs.identity_key_path()).unwrap();
        let worker_id = reeve_types::IdentityId::new().unwrap();
        {
            let mut identity =
                reeve_types::Identity::new_agent("worker".to_owned(), operator_id).unwrap();
            identity.identity_id = worker_id;
            let key_record =
                reeve_types::KeyRecord::new(worker_id, *worker_keypair.public()).unwrap();
            let stored = StoredIdentity::new(identity, key_record).unwrap();
            identity_registry.write(&stored).unwrap();
        }
        let snapshot = SpawnSnapshot {
            persona_name: "worker".to_owned(),
            persona_version: 1,
            adapter_id: adapter.id().to_owned(),
            agent_id: worker_id.to_string(),
            system_prompt: "You are a worker.".to_owned(),
            system_prompt_source: None,
            engagement_name: None,
            working_root: None,
        };
        write_spawn_snapshot(&worker_dirs, &snapshot).unwrap();
        {
            let mut agent_registry = AgentRegistry::open(agent_registry_path.clone()).unwrap();
            agent_registry
                .register(AgentRecord {
                    name: ValidatedAgentName::new("worker").unwrap(),
                    identity_id: worker_id,
                    inbox_dir: worker_dirs.inbox_root(),
                    persona_name: Some("worker".to_owned()),
                    spawned_at: time::OffsetDateTime::now_utc(),
                    status: AgentStatus::Running,
                    stopped_reason: None,
                })
                .unwrap();
        }

        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();
            let dispatcher = NullDispatcher.start().recipient();
            let test_audit =
                Arc::new(AuditLog::open(data_dir.clone()).expect("open audit log in test"));
            resume_persisted_subagents(
                &data_dir,
                &agent_registry_path,
                &identity_registry,
                std::slice::from_ref(&adapter),
                None,
                &test_audit,
                &watcher,
                &inbox_starter,
                &dispatcher,
                None,
                None,
            );

            assert!(
                !watcher.has_route(worker_id),
                "an agent with a corrupt profile snapshot must not be routable"
            );
            // The corrupt snapshot is preserved, not overwritten by synthesis.
            let on_disk = fs::read_to_string(worker_dirs.profile_path()).unwrap();
            assert_eq!(
                on_disk, corrupt,
                "a corrupt profile.toml must not be silently overwritten"
            );

            actix::System::current().stop();
        });
    }

    // R2: A persisted subagent whose snapshot adapter_id does not match the
    // running daemon's adapter is skipped (logged at warn). The watcher's
    // routing table must NOT contain a route for it — the alternative would
    // be a route to an actor that immediately fails on the first model call.
    #[test]
    #[cfg(unix)]
    fn resume_persisted_subagents_skips_subagent_with_adapter_drift() {
        use crate::test_support::{NullDispatcher, NullInboxStarter};
        use actix::Actor as _;

        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let operator_id = enroll_test_operator(&identity_registry);
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));

        crate::test_support::write_persona_config(&data_dir, "worker", "claude-opus-4-7");
        let worker_dirs = AgentDirs::provision(&data_dir, "worker").unwrap();
        let worker_keypair = generate_or_load_keypair(&worker_dirs.identity_key_path()).unwrap();
        let worker_id = reeve_types::IdentityId::new().unwrap();

        let mut identity =
            reeve_types::Identity::new_agent("worker".to_owned(), operator_id).unwrap();
        identity.identity_id = worker_id;
        let key_record = reeve_types::KeyRecord::new(worker_id, *worker_keypair.public()).unwrap();
        let stored = StoredIdentity::new(identity, key_record).unwrap();
        identity_registry.write(&stored).unwrap();

        // Snapshot with a DIFFERENT adapter than the daemon will run with.
        let snapshot = SpawnSnapshot {
            persona_name: "worker".to_owned(),
            persona_version: 1,
            adapter_id: "claude-opus-4-7@some-other-route".to_owned(),
            agent_id: worker_id.to_string(),
            system_prompt: String::from("ignored"),
            system_prompt_source: None,
            engagement_name: None,
            working_root: None,
        };
        write_spawn_snapshot(&worker_dirs, &snapshot).unwrap();

        let mut agent_registry = AgentRegistry::open(agent_registry_path.clone()).unwrap();
        agent_registry
            .register(AgentRecord {
                name: ValidatedAgentName::new("worker").unwrap(),
                identity_id: worker_id,
                inbox_dir: worker_dirs.inbox_root(),
                persona_name: Some("worker".to_owned()),
                spawned_at: time::OffsetDateTime::now_utc(),
                status: AgentStatus::Running,
                stopped_reason: None,
            })
            .unwrap();

        let watcher_for_assert = Arc::clone(&watcher);
        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();
            let dispatcher = NullDispatcher.start().recipient();

            let test_audit =
                Arc::new(AuditLog::open(data_dir.clone()).expect("open audit log in test"));
            resume_persisted_subagents(
                &data_dir,
                &agent_registry_path,
                &identity_registry,
                std::slice::from_ref(&adapter),
                None,
                &test_audit,
                &watcher,
                &inbox_starter,
                &dispatcher,
                None,
                None,
            );

            actix::System::current().stop();
        });

        assert!(
            !watcher_for_assert.has_route(worker_id),
            "watcher must not register a route for a subagent whose snapshot \
             adapter_id differs from the running daemon's adapter"
        );
    }
}
