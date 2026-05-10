//! Actix supervisor tree for the Reeve runtime daemon.
//!
//! Wires two supervised actors:
//!
//! - [`HeartbeatActor`]: touches `<state_dir>/runtime/heartbeat` every second
//!   so external monitors can detect a stalled daemon without polling the PID
//!   file.
//! - [`WatcherActor`]: a supervised mailbox that holds the [`Watcher`] handle.
//!   Incoming [`WatchInbox`] messages immediately start watching the inbox
//!   using [`Watcher::run`] in a `spawn_blocking` thread.
//!
//! Both actors are started via [`actix::Supervisor::start`] by the daemon
//! runner. This module provides only the actor types and their message contracts
//! — no combined supervisor struct is introduced here.

use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, ActorContext, AsyncContext, Context, Handler, Message, Supervised};
use reeve_types::IdentityId;
use time;
use tracing::{debug, info, warn};

use crate::fs_util::{apply_file_mode_options, ensure_directory, set_nofollow};
use crate::inbox::AgentInbox;
use crate::watcher::Watcher;

/// Mode for the `runtime/` subdirectory inside the state directory on Unix.
const RUNTIME_SUBDIR_MODE: u32 = 0o700;

/// Mode for the heartbeat file on Unix. Readable only by the runtime user.
const HEARTBEAT_FILE_MODE: u32 = 0o600;

/// Name of the subdirectory inside the state directory that holds runtime files.
const RUNTIME_SUBDIR: &str = "runtime";

/// Interval between heartbeat file updates.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// How often [`WatcherActor`] runs `cur/` rotation housekeeping.
const ROTATION_INTERVAL: Duration = Duration::from_mins(5);

/// Files older than this are moved from `cur/` to `archive/` on each rotation.
///
/// `cur/` is an in-flight buffer; the conversation journal is the durable
/// record. 24 hours gives the operator a comfortable window to inspect recent
/// deliveries in place while bounding directory growth.
const CUR_RETENTION: time::Duration = time::Duration::hours(24);

// ── Messages ──────────────────────────────────────────────────────────────────

/// Internal clock tick for [`HeartbeatActor`]. Not public: the tick is
/// self-scheduled; callers have no reason to inject one.
struct Tick;

impl Message for Tick {
    type Result = ();
}

/// Instruct [`WatcherActor`] to register an agent inbox for watching.
///
/// The message is accepted and queued; active watching begins when the watcher
/// loop is wired in.
pub struct WatchInbox {
    /// Identity of the agent whose inbox should be watched.
    pub agent_id: IdentityId,
    /// Handle to the agent's inbox directory layout.
    pub inbox: AgentInbox,
    /// Called with the quarantine reason string whenever the watcher pipeline
    /// rejects an envelope to `quarantine/`. Use this to surface transport
    /// failures in the agent's conversation thread. `None` disables the hook.
    pub on_quarantine: Option<Box<dyn Fn(String) + Send + 'static>>,
    /// The actor to which verified envelopes are dispatched after they are
    /// moved to `cur/`. Non-optional: a [`WatchInbox`] message without a live
    /// dispatch target is a programming error. This differs from
    /// `on_quarantine`, which is an optional notification hook with different
    /// semantics.
    ///
    /// The [`WatcherActor`] handler registers this recipient with
    /// [`Watcher::register_route`] and performs a one-shot `cur/` scan for
    /// crash-recovery before starting the `new/` watcher loop.
    pub recipient: actix::Recipient<crate::agent::ProcessInbound>,
}

impl Message for WatchInbox {
    type Result = ();
}

// ── HeartbeatActor ────────────────────────────────────────────────────────────

/// Supervised actor that touches the heartbeat file every second.
///
/// The heartbeat file lives at `<state_dir>/runtime/heartbeat`. It is created
/// on first tick; the `runtime/` subdirectory is created in [`started`]. Each
/// tick overwrites the file with the current Unix timestamp as a decimal string
/// and calls `sync_data()` before rescheduling. If a write fails the actor
/// stops and the supervisor restarts it.
///
/// The state directory itself is created by [`RuntimeLock::acquire`]; only the
/// `runtime/` subdirectory is created here.
///
/// [`started`]: actix::Actor::started
/// [`RuntimeLock::acquire`]: crate::runtime_lock::RuntimeLock::acquire
pub struct HeartbeatActor {
    /// Absolute path to the heartbeat file.
    heartbeat_path: PathBuf,
}

impl HeartbeatActor {
    /// Construct a heartbeat actor whose file lives at
    /// `<state_dir>/runtime/heartbeat`.
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            heartbeat_path: state_dir.into().join(RUNTIME_SUBDIR).join("heartbeat"),
        }
    }

    /// Create the `runtime/` directory that holds the heartbeat file.
    ///
    /// Returns `true` on success. Returns `false` and calls `ctx.stop()` if
    /// the parent directory cannot be determined or `ensure_directory` fails —
    /// the supervisor will restart the actor and retry.
    fn ensure_runtime_dir(&mut self, ctx: &mut Context<Self>) -> bool {
        let runtime_dir = if let Some(dir) = self.heartbeat_path.parent() {
            dir.to_path_buf()
        } else {
            ctx.stop();
            return false;
        };
        if ensure_directory(&runtime_dir, RUNTIME_SUBDIR_MODE).is_err() {
            ctx.stop();
            return false;
        }
        true
    }
}

impl Actor for HeartbeatActor {
    type Context = Context<Self>;

    /// Create `<state_dir>/runtime/` and schedule the first tick.
    fn started(&mut self, ctx: &mut Context<Self>) {
        if self.ensure_runtime_dir(ctx) {
            debug!(path = %self.heartbeat_path.display(), "heartbeat actor started");
            ctx.notify_later(Tick, HEARTBEAT_INTERVAL);
        }
    }
}

impl Supervised for HeartbeatActor {
    /// Re-create the runtime directory and reschedule the tick after restart.
    ///
    /// Without re-running directory creation, a failed `started` (dir creation
    /// error) would cause `restarting` to schedule a tick, `touch_heartbeat`
    /// to fail (no dir), the actor to stop again, and so on indefinitely.
    fn restarting(&mut self, ctx: &mut Context<Self>) {
        warn!("heartbeat actor restarting");
        if self.ensure_runtime_dir(ctx) {
            ctx.notify_later(Tick, HEARTBEAT_INTERVAL);
        }
    }
}

impl Handler<Tick> for HeartbeatActor {
    type Result = ();

    fn handle(&mut self, _msg: Tick, ctx: &mut Context<Self>) {
        if let Err(err) = touch_heartbeat(&self.heartbeat_path) {
            warn!(err = %err, "heartbeat write failed, stopping for supervisor restart");
            ctx.stop();
            return;
        }
        ctx.notify_later(Tick, HEARTBEAT_INTERVAL);
    }
}

/// Write the current Unix timestamp (seconds) to `path` and fsync.
///
/// Creates the file with mode `0o600` on Unix. Uses truncating write so the
/// file stays small and reflects only the most recent tick. The data content
/// (decimal seconds) is machine-readable but not security-sensitive; it exists
/// solely to update the mtime for monitors that call `stat(2)`.
fn touch_heartbeat(path: &std::path::Path) -> io::Result<()> {
    use std::io::Write;
    use std::time::SystemTime;

    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut options = File::options();
    options.write(true).create(true).truncate(true);
    apply_file_mode_options(&mut options, HEARTBEAT_FILE_MODE);
    set_nofollow(&mut options);
    let mut file = options.open(path)?;

    write!(file, "{secs}")?;
    file.sync_data()
}

// ── WatcherActor ──────────────────────────────────────────────────────────────

/// Supervised actor that holds the [`Watcher`] handle for the inbox pipeline.
///
/// Accepts [`WatchInbox`] messages and immediately starts watching the inbox
/// using [`Watcher::run`] in a blocking thread via
/// [`tokio::task::spawn_blocking`]. The blocking thread is kept off the async
/// executor so it does not stall other tasks.
///
/// [`Watcher::run`]: crate::watcher::Watcher::run
pub struct WatcherActor {
    watcher: Arc<Watcher>,
    /// Inboxes registered via [`WatchInbox`]; iterated on each rotation tick.
    inboxes: Vec<AgentInbox>,
}

impl WatcherActor {
    /// Construct a watcher actor from an existing [`Watcher`] handle.
    pub fn new(watcher: Arc<Watcher>) -> Self {
        Self {
            watcher,
            inboxes: Vec::new(),
        }
    }

    /// Return the shared watcher handle.
    ///
    /// Call this to drive the inbox watch loop from outside the actor's
    /// message-handler context.
    pub fn watcher(&self) -> &Arc<Watcher> {
        &self.watcher
    }
}

impl Actor for WatcherActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Context<Self>) {
        ctx.run_interval(ROTATION_INTERVAL, |actor, _ctx| {
            let now = time::OffsetDateTime::now_utc();
            for inbox in &actor.inboxes {
                match actor.watcher.rotate_cur(inbox, CUR_RETENTION, now) {
                    Ok(outcome) if outcome.archived > 0 => {
                        debug!(archived = outcome.archived, "rotated cur/ to archive/");
                    }
                    Ok(_) => {}
                    Err(e) => warn!(err = %e, "cur/ rotation error"),
                }
            }
        });
    }
}

impl Supervised for WatcherActor {
    // No additional wiring needed on restart; inboxes persists on the
    // actor value across restarts because `restarting` receives `&mut self`.
}

impl Handler<WatchInbox> for WatcherActor {
    type Result = ();

    /// Start watching the inbox in a blocking thread and register the inbox
    /// for periodic `cur/` rotation.
    ///
    /// [`Watcher::run`] is a blocking loop; `spawn_blocking` keeps it off the
    /// async executor. Errors in the watcher loop are logged and the loop
    /// exits; the supervisor does not restart the watcher thread automatically.
    fn handle(&mut self, msg: WatchInbox, _ctx: &mut Context<Self>) {
        let watcher = Arc::clone(&self.watcher);
        let agent_id = msg.agent_id;
        let inbox = msg.inbox.clone();
        let recipient = msg.recipient.clone();
        self.inboxes.push(msg.inbox.clone());
        let on_quarantine = msg.on_quarantine;

        // Route must be registered before watcher.run starts. The run
        // loop calls handle_deliver → dispatch_envelope, which consults
        // the routing table; a message arriving before register_route is
        // called is silently dropped.
        watcher.register_route(agent_id, recipient.clone());

        // scan_cur_and_dispatch reads the filesystem; move it off the actix
        // thread so it doesn't block the async executor.
        tokio::task::spawn_blocking({
            let inbox = inbox.clone();
            let watcher = Arc::clone(&watcher);
            move || {
                watcher.scan_cur_and_dispatch(&inbox, agent_id);
            }
        });

        info!(inbox_root = %inbox.root().display(), "watcher started for inbox");
        tokio::task::spawn_blocking(move || {
            let cb = on_quarantine.unwrap_or_else(|| Box::new(|_| {}));
            let _ = watcher.run(agent_id, &inbox, cb);
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use actix::{Actor, ActorContext, Context, Handler, Message, Supervised, Supervisor};
    use tempfile::tempdir;

    use super::HeartbeatActor;

    // ── S1: heartbeat actor touches the file ──────────────────────────────────

    /// Starting a supervised `HeartbeatActor` creates the heartbeat file within
    /// one tick period. The test polls for file existence every 100 ms up to a
    /// 5-second deadline rather than sleeping a fixed duration.
    #[test]
    fn heartbeat_actor_touches_file() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let heartbeat_path = state_dir.join("runtime").join("heartbeat");

        let state_dir_owned = state_dir.clone();
        actix::System::new().block_on(async move {
            let _addr = Supervisor::start(move |_| HeartbeatActor::new(state_dir_owned));

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if heartbeat_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "heartbeat file did not appear within 5 seconds"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            actix::System::current().stop();
        });

        assert!(
            state_dir.join("runtime").join("heartbeat").exists(),
            "heartbeat file must exist after one tick period",
        );
    }

    // ── S2: supervisor restarts failing actor; heartbeat keeps running ─────────

    /// A `PanicActor` that requests a stop on `TriggerStop` is restarted by the
    /// supervisor. The `restart_count` is incremented in `restarting`. A
    /// `HeartbeatActor` running in parallel is unaffected.
    ///
    /// Named `TriggerStop` rather than `TriggerPanic` because actix does not use
    /// `catch_unwind` — a real panic kills the tokio task and bypasses the
    /// supervisor's restart logic. `ctx.stop()` is the correct supervised-failure
    /// mechanism in actix 0.13.
    #[test]
    fn panic_actor_is_restarted_supervisor_keeps_running() {
        let tmp = tempdir().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let heartbeat_path = state_dir.join("runtime").join("heartbeat");
        let restart_count = Arc::new(AtomicU32::new(0));
        let restart_count_for_actor = Arc::clone(&restart_count);
        let restart_count_for_poll = Arc::clone(&restart_count);

        let state_dir_owned = state_dir.clone();
        actix::System::new().block_on(async move {
            let _heartbeat = Supervisor::start(move |_| HeartbeatActor::new(state_dir_owned));

            let addr_failing = Supervisor::start(move |_| PanicActor {
                restart_count: Arc::clone(&restart_count_for_actor),
            });

            // Poll for the heartbeat file before triggering the sibling failure,
            // so the assertion at the end is meaningful.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if heartbeat_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "heartbeat file did not appear within 5 seconds"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            addr_failing.do_send(TriggerStop);

            // Poll until the supervisor has restarted PanicActor at least once.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if restart_count_for_poll.load(Ordering::SeqCst) >= 1 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "supervisor did not restart PanicActor within 2 seconds"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        assert!(
            restart_count.load(Ordering::SeqCst) >= 1,
            "supervisor must have restarted PanicActor at least once",
        );
        assert!(
            state_dir.join("runtime").join("heartbeat").exists(),
            "heartbeat file must still exist after sibling actor failure",
        );
    }

    // ── Test-only actor types ─────────────────────────────────────────────────

    /// Test actor that calls `ctx.stop()` when it receives [`TriggerStop`],
    /// causing the supervisor to invoke `restarting` and restart it.
    struct PanicActor {
        restart_count: Arc<AtomicU32>,
    }

    impl Actor for PanicActor {
        type Context = Context<Self>;
    }

    impl Supervised for PanicActor {
        fn restarting(&mut self, _ctx: &mut Context<Self>) {
            self.restart_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct TriggerStop;

    impl Message for TriggerStop {
        type Result = ();
    }

    impl Handler<TriggerStop> for PanicActor {
        type Result = ();

        fn handle(&mut self, _msg: TriggerStop, ctx: &mut Context<Self>) {
            ctx.stop();
        }
    }
}
