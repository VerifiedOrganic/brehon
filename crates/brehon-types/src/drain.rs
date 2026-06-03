//! Tracked drain of in-flight work for graceful shutdown.
//!
//! This module provides a process-global, sync-only tracker for in-flight work
//! items. It is used during graceful shutdown to wait for long-running operations
//! (git commits, review submissions, MCP calls) to complete before the process
//! terminates.
//!
//! Usage:
//! - Subsystems check [`is_draining()`] before starting new long-running work.
//! - Long-running operations create an [`InFlightGuard`] via [`in_flight_guard()`].
//! - The guard calls [`complete_in_flight()`] on drop, completing on every return path.
//! - The shutdown path calls [`in_flight_count()`] or [`in_flight_labels()`] to drain.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Process-global shutdown flag. Set to true when SIGINT or SIGTERM is received.
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// Process-global drain tracker.
static DRAIN_TRACKER: std::sync::OnceLock<DrainTracker> = std::sync::OnceLock::new();

fn global_tracker() -> &'static DrainTracker {
    DRAIN_TRACKER.get_or_init(DrainTracker::new)
}

/// Set the global shutdown/draining flag.
///
/// Called by signal handlers (SIGINT, SIGTERM) to indicate the process
/// is shutting down and no new long-running work should be started.
pub fn set_draining() {
    SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
}

/// Returns `true` if a shutdown signal has been received.
///
/// Long-running operations should check this before starting new work.
/// When draining, refuse new work but complete work already in progress.
pub fn is_draining() -> bool {
    SHUTDOWN_FLAG.load(Ordering::SeqCst)
}

/// Reset the draining flag. Intended for test helpers.
#[doc(hidden)]
pub fn reset_draining_for_test() {
    SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
}

/// Register an in-flight work item with a descriptive label.
///
/// Returns a key that must be passed to [`complete_in_flight()`] when done.
/// Prefer [`in_flight_guard()`] for RAII completion.
pub fn register_in_flight(label: &str) -> u64 {
    global_tracker().register(label)
}

/// Mark a previously registered in-flight work item as completed.
///
/// Completing an unknown key is a no-op (safe to call from `Drop` impls).
pub fn complete_in_flight(key: u64) {
    global_tracker().complete(key);
}

/// Create an RAII guard for an in-flight work item.
///
/// The guard calls `complete_in_flight()` on drop, ensuring completion on
/// every return path (including `?` propagation and panics via `Drop`).
///
/// # Example
///
/// ```ignore
/// if brehon_types::drain::is_draining() { return; }
/// let _guard = brehon_types::drain::in_flight_guard("git-commit");
/// do_git_commit()?;   // _guard completes on any return
/// ```
pub fn in_flight_guard(label: &str) -> InFlightGuard {
    InFlightGuard {
        key: register_in_flight(label),
    }
}

/// Returns the number of currently tracked in-flight work items.
pub fn in_flight_count() -> usize {
    global_tracker().count()
}

/// Returns a snapshot of current in-flight work labels.
///
/// Useful for logging which items are still pending at drain deadline.
pub fn in_flight_labels() -> Vec<String> {
    global_tracker().labels()
}

// ── RAII Guard ────────────────────────────────────────────────────────

/// RAII guard that calls [`complete_in_flight()`] on drop.
///
/// Created by [`in_flight_guard()`]. Drop happens on every return path,
/// so the tracked work is always marked complete.
pub struct InFlightGuard {
    key: u64,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        complete_in_flight(self.key);
    }
}

// ── DrainTracker (internal) ───────────────────────────────────────────

struct DrainTracker {
    items: std::sync::Mutex<DrainItems>,
    next_key: AtomicU64,
    /// Notification fired whenever an item completes, so async drain
    /// can wake immediately instead of relying on polling alone.
    completion_notify: std::sync::Mutex<Option<std::sync::Arc<std::sync::Condvar>>>,
}

struct DrainItems {
    entries: HashMap<u64, String>,
}

impl DrainTracker {
    fn new() -> Self {
        Self {
            items: std::sync::Mutex::new(DrainItems {
                entries: HashMap::new(),
            }),
            next_key: AtomicU64::new(1),
            completion_notify: std::sync::Mutex::new(None),
        }
    }

    fn register(&self, label: &str) -> u64 {
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        self.items
            .lock()
            .unwrap()
            .entries
            .insert(key, label.to_string());
        key
    }

    fn complete(&self, key: u64) {
        let removed = self.items.lock().unwrap().entries.remove(&key).is_some();
        if removed {
            // Wake any thread waiting in count_until_zero
            if let Some(cvar) = self.completion_notify.lock().unwrap().as_ref() {
                cvar.notify_all();
            }
        }
    }

    fn count(&self) -> usize {
        self.items.lock().unwrap().entries.len()
    }

    fn labels(&self) -> Vec<String> {
        self.items
            .lock()
            .unwrap()
            .entries
            .values()
            .cloned()
            .collect()
    }

    /// Block until count reaches zero, or timeout expires.
    /// Returns the remaining count (0 if all completed).
    fn wait_until_zero(&self, timeout: std::time::Duration) -> usize {
        let cvar = std::sync::Arc::new(std::sync::Condvar::new());
        *self.completion_notify.lock().unwrap() = Some(cvar.clone());

        let result = {
            let items = self.items.lock().unwrap();
            let (final_items, _) = cvar
                .wait_timeout_while(items, timeout, |items| !items.entries.is_empty())
                .unwrap();
            final_items.entries.len()
        };

        // Clear the condvar so we don't hold a stale reference
        *self.completion_notify.lock().unwrap() = None;
        result
    }
}

/// Block until all in-flight work completes, or timeout expires.
///
/// Returns a tuple of (remaining, total_at_start).
/// Uses a condvar for immediate wake-on-completion instead of pure polling.
pub fn drain_sync(timeout: std::time::Duration) -> (usize, usize) {
    let total = in_flight_count();
    if total == 0 {
        return (0, 0);
    }
    let remaining = global_tracker().wait_until_zero(timeout);
    (remaining, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single shared lock for all tests that touch the global tracker.
    /// Rust runs tests in parallel by default; without this lock, tests that
    /// call `register_in_flight` / `complete_in_flight` / `drain_sync` against
    /// the global OnceLock interfere with each other.
    static GLOBAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn register_and_complete() {
        let tracker = DrainTracker::new();
        assert_eq!(tracker.count(), 0);

        let k1 = tracker.register("git-commit");
        let k2 = tracker.register("review-submit");
        assert_eq!(tracker.count(), 2);

        tracker.complete(k1);
        assert_eq!(tracker.count(), 1);

        tracker.complete(k2);
        assert_eq!(tracker.count(), 0);
    }

    #[test]
    fn complete_unknown_key_is_noop() {
        let tracker = DrainTracker::new();
        tracker.complete(9999);
        assert_eq!(tracker.count(), 0);
    }

    #[test]
    fn keys_are_monotonically_increasing() {
        let tracker = DrainTracker::new();
        let k1 = tracker.register("a");
        let k2 = tracker.register("b");
        let k3 = tracker.register("c");
        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn in_flight_guard_completes_on_drop() {
        // Use a local tracker to avoid OnceLock pollution
        let tracker = DrainTracker::new();
        let k = tracker.register("test-work");
        assert_eq!(tracker.count(), 1);
        // Simulate what InFlightGuard::drop does
        tracker.complete(k);
        assert_eq!(tracker.count(), 0);
    }

    #[test]
    fn guard_completes_via_global_tracker() {
        let _lock = GLOBAL_TEST_LOCK.lock().unwrap();
        let before = in_flight_count();
        {
            let _g = in_flight_guard("global-guard-test");
            assert_eq!(in_flight_count(), before + 1);
        }
        assert_eq!(in_flight_count(), before);
    }

    #[test]
    fn labels_returns_current_entries() {
        let tracker = DrainTracker::new();
        tracker.register("alpha");
        tracker.register("beta");
        let mut labels = tracker.labels();
        labels.sort();
        assert_eq!(labels, vec!["alpha", "beta"]);
    }

    #[test]
    fn is_draining_roundtrip() {
        let _lock = GLOBAL_TEST_LOCK.lock().unwrap();
        reset_draining_for_test();
        assert!(!is_draining());
        set_draining();
        assert!(is_draining());
        reset_draining_for_test();
        assert!(!is_draining());
    }

    #[test]
    fn drain_sync_returns_zero_when_no_work() {
        let _lock = GLOBAL_TEST_LOCK.lock().unwrap();
        reset_draining_for_test();
        // Ensure no guards from other tests are held
        let (remaining, total) = drain_sync(std::time::Duration::from_millis(50));
        assert_eq!(remaining, 0);
        assert_eq!(total, 0);
    }

    #[test]
    fn drain_sync_times_out_with_stuck_work() {
        let _lock = GLOBAL_TEST_LOCK.lock().unwrap();
        let k = register_in_flight("stuck-drain-test");
        let (remaining, total) = drain_sync(std::time::Duration::from_millis(50));
        assert!(remaining >= 1);
        assert!(total >= 1);
        complete_in_flight(k);
    }
}
