//! Notify-based filesystem watcher for the lead agent directory.
//!
//! `watch_agent_dir` starts a recursive watcher on `agents/lead/` and calls
//! `on_change` at most once per 250 ms window — a debounce that prevents the
//! renderer from reloading state on every individual file event during a burst
//! write (e.g., a model call that updates status, cost, and conversation in
//! rapid succession).
//!
//! The watcher is dropped when the returned [`notify::RecommendedWatcher`] is
//! dropped, which also terminates the debounce thread.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher as _};

/// Debounce window: accumulate events, then fire the callback once.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

/// Watch `agent_dir` for any file changes and call `on_change` at most once
/// per 250 ms.
///
/// Returns the watcher handle. The caller must keep it alive for as long as
/// watching should continue; dropping it stops the watcher and terminates
/// the debounce thread.
///
/// # Errors
///
/// Returns a `notify::Error` when the watcher cannot be created or the watch
/// path cannot be registered (e.g., `agent_dir` does not exist).
pub fn watch_agent_dir(
    agent_dir: &Path,
    on_change: impl Fn() + Send + 'static,
) -> Result<notify::RecommendedWatcher, notify::Error> {
    let (tx, rx) = mpsc::channel::<()>();

    // Spawn the debounce thread before creating the watcher. If watcher
    // creation fails the thread exits immediately when `rx` is dropped.
    std::thread::spawn(move || debounce_loop(&rx, on_change));

    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        // Notify the debounce thread on any successful event. Errors (e.g.,
        // path gone) are silently ignored: the TUI will show stale state
        // until the next fresh event arrives.
        if result.is_ok() {
            // Ignore send errors: the debounce thread may have exited if the
            // caller dropped the watcher handle already.
            let _ = tx.send(());
        }
    })?;

    watcher.watch(agent_dir, RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Receive filesystem events and call `on_change` at most once per
/// `DEBOUNCE_WINDOW`.
///
/// The loop exits when `rx` is closed (i.e., the watcher is dropped).
pub(crate) fn debounce_loop(rx: &mpsc::Receiver<()>, on_change: impl Fn()) {
    loop {
        // Block until the first event in a new window.
        match rx.recv() {
            Ok(()) => {}
            Err(_) => return, // sender dropped; watcher was stopped.
        }

        // Drain any events that arrive within the debounce window.
        let deadline = std::time::Instant::now() + DEBOUNCE_WINDOW;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Watcher dropped during the drain window; fire once more
                    // for any buffered events, then exit.
                    on_change();
                    return;
                }
            }
        }

        on_change();
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use super::*;

    // W1: a single event fires the callback exactly once.
    #[test]
    fn single_event_fires_callback_once() {
        let (tx, rx) = mpsc::channel::<()>();
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = Arc::clone(&count);
        let handle = std::thread::spawn(move || {
            debounce_loop(&rx, move || {
                count_clone.fetch_add(1, Ordering::SeqCst);
            });
        });
        tx.send(()).unwrap();
        drop(tx); // disconnect so the thread exits
        handle.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    // W2: multiple events arriving within the debounce window fire the callback once.
    #[test]
    fn multiple_events_in_window_fire_once() {
        let (tx, rx) = mpsc::channel::<()>();
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = Arc::clone(&count);
        let handle = std::thread::spawn(move || {
            debounce_loop(&rx, move || {
                count_clone.fetch_add(1, Ordering::SeqCst);
            });
        });
        // Send 5 events in rapid succession
        for _ in 0..5 {
            tx.send(()).unwrap();
        }
        drop(tx);
        handle.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    // W3: disconnecting the sender during the drain window fires the callback exactly once.
    #[test]
    fn disconnect_during_drain_fires_once() {
        // Use a sync_channel with capacity 0: send() blocks until recv() is called.
        // This gives us a deterministic way to know the debounce thread has received
        // the event and entered the drain loop before we disconnect the sender.
        let (tx, rx) = mpsc::sync_channel::<()>(0);
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = Arc::clone(&count);
        let handle = std::thread::spawn(move || {
            debounce_loop(&rx, move || {
                count_clone.fetch_add(1, Ordering::SeqCst);
            });
        });
        // send() blocks until the debounce thread's recv() completes, so when
        // this returns the thread is inside the drain loop.
        tx.send(()).unwrap();
        // Disconnect the sender while the thread is in the drain loop.
        drop(tx);
        handle.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
