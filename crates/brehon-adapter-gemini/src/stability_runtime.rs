use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use brehon_types::{
    refresh_runtime_stability_counters, remove_session_stability_counters,
    write_session_stability_counters, StabilityCounters,
};

struct SessionSnapshotState {
    // Keep a single mutex around both version bookkeeping and snapshot I/O so
    // claim_* calls cannot race ahead of an in-flight write/remove. That means
    // async claimers may briefly wait behind file I/O, which is acceptable for
    // these low-frequency stability snapshots.
    inner: Mutex<SessionSnapshotLifecycle>,
}

#[derive(Default)]
struct SessionSnapshotLifecycle {
    latest_version: u64,
    closed: bool,
}

fn session_states() -> &'static Mutex<HashMap<String, Arc<SessionSnapshotState>>> {
    static SESSION_STATES: OnceLock<Mutex<HashMap<String, Arc<SessionSnapshotState>>>> =
        OnceLock::new();
    SESSION_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn session_state(session_key: &str) -> Arc<SessionSnapshotState> {
    let mut states = session_states()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    Arc::clone(states.entry(session_key.to_string()).or_insert_with(|| {
        Arc::new(SessionSnapshotState {
            inner: Mutex::new(SessionSnapshotLifecycle::default()),
        })
    }))
}

pub fn brehon_root_from_env() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

fn persist_session_snapshot_inner(session_key: &str, counters: StabilityCounters) {
    let Some(root) = brehon_root_from_env() else {
        return;
    };
    if write_session_stability_counters(&root, session_key, counters).is_ok() {
        let _ = refresh_runtime_stability_counters(&root);
    }
}

fn clear_session_snapshot_inner(session_key: &str) {
    let Some(root) = brehon_root_from_env() else {
        return;
    };
    if remove_session_stability_counters(&root, session_key).is_ok() {
        let _ = refresh_runtime_stability_counters(&root);
    }
}

fn claim_persist_session_snapshot(state: &Arc<SessionSnapshotState>) -> Option<u64> {
    let mut lifecycle = state.inner.lock().unwrap_or_else(|err| err.into_inner());
    if lifecycle.closed {
        return None;
    }
    lifecycle.latest_version = lifecycle.latest_version.saturating_add(1);
    Some(lifecycle.latest_version)
}

fn claim_clear_session_snapshot(state: &Arc<SessionSnapshotState>) -> u64 {
    let mut lifecycle = state.inner.lock().unwrap_or_else(|err| err.into_inner());
    lifecycle.closed = true;
    lifecycle.latest_version = lifecycle.latest_version.saturating_add(1);
    lifecycle.latest_version
}

fn run_persist_session_snapshot(
    session_key: &str,
    counters: StabilityCounters,
    state: &Arc<SessionSnapshotState>,
    version: u64,
) {
    let lifecycle = state.inner.lock().unwrap_or_else(|err| err.into_inner());
    if lifecycle.closed || lifecycle.latest_version != version {
        return;
    }
    // Hold the lifecycle lock through file I/O so the version check and write
    // stay serialized with later clear/persist claims for this session.
    persist_session_snapshot_inner(session_key, counters);
}

fn run_clear_session_snapshot(session_key: &str, state: &Arc<SessionSnapshotState>, version: u64) {
    let lifecycle = state.inner.lock().unwrap_or_else(|err| err.into_inner());
    if lifecycle.latest_version != version {
        return;
    }
    // Hold the lifecycle lock through file I/O so a claimed clear stays final
    // once it starts removing the on-disk snapshot.
    clear_session_snapshot_inner(session_key);
}

pub fn persist_session_snapshot(session_key: &str, counters: StabilityCounters) {
    let state = session_state(session_key);
    let Some(version) = claim_persist_session_snapshot(&state) else {
        return;
    };
    run_persist_session_snapshot(session_key, counters, &state, version);
}

pub fn clear_session_snapshot(session_key: &str) {
    let state = session_state(session_key);
    let version = claim_clear_session_snapshot(&state);
    run_clear_session_snapshot(session_key, &state, version);
}

pub fn schedule_persist_session_snapshot(session_key: String, counters: StabilityCounters) {
    let state = session_state(&session_key);
    let Some(version) = claim_persist_session_snapshot(&state) else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        run_persist_session_snapshot(&session_key, counters, &state, version);
    });
}

pub fn schedule_clear_session_snapshot(session_key: String) {
    let state = session_state(&session_key);
    let version = claim_clear_session_snapshot(&state);
    tokio::task::spawn_blocking(move || {
        run_clear_session_snapshot(&session_key, &state, version);
    });
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::future::Future;

    use super::*;

    fn test_env_lock() -> &'static Mutex<()> {
        static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        saved: Option<OsString>,
    }

    impl EnvVarGuard {
        fn preserve(key: &'static str) -> Self {
            Self {
                key,
                saved: std::env::var_os(key),
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // These test helpers serialize process-environment mutations through
            // test_env_lock(). If this crate adopts Rust 2024+ (where
            // set_var/remove_var are unsafe), that lock is the safety invariant
            // to cite.
            if let Some(value) = &self.saved {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    /// Caller must hold test_env_lock().
    /// Use run_with_brehon_root_test for the lock-acquiring variant.
    fn run_with_brehon_root_test_locked<F>(test: F)
    where
        F: Future<Output = ()>,
    {
        let _brehon_root_guard = EnvVarGuard::preserve("BREHON_ROOT");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(test);
    }

    fn run_with_brehon_root_test<F>(test: F)
    where
        F: Future<Output = ()>,
    {
        let _guard = test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        run_with_brehon_root_test_locked(test);
    }

    /// Caller must hold test_env_lock().
    /// Use run_with_brehon_root_sync_test for the lock-acquiring variant.
    fn run_with_brehon_root_sync_test_locked<F>(test: F)
    where
        F: FnOnce(),
    {
        let _brehon_root_guard = EnvVarGuard::preserve("BREHON_ROOT");
        test();
    }

    fn run_with_brehon_root_sync_test<F>(test: F)
    where
        F: FnOnce(),
    {
        let _guard = test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        run_with_brehon_root_sync_test_locked(test);
    }

    #[test]
    fn clear_wins_over_queued_persist_and_future_persists() {
        run_with_brehon_root_test(async {
            let root = tempfile::tempdir().unwrap();
            std::env::set_var("BREHON_ROOT", root.path());

            let session_key = "session-test-clear-queued".to_string();
            schedule_persist_session_snapshot(
                session_key.clone(),
                StabilityCounters {
                    pending_requests: 3,
                    ..Default::default()
                },
            );
            schedule_clear_session_snapshot(session_key.clone());
            schedule_persist_session_snapshot(
                session_key.clone(),
                StabilityCounters {
                    pending_requests: 99,
                    ..Default::default()
                },
            );

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let snapshot = root
                .path()
                .join("runtime")
                .join("stability-sessions")
                .join("session-test-clear-queued.json");
            assert!(!snapshot.exists(), "clear should prevent stale persists");
        });
    }

    #[test]
    fn direct_clear_also_blocks_queued_and_future_persists() {
        run_with_brehon_root_test(async {
            let root = tempfile::tempdir().unwrap();
            std::env::set_var("BREHON_ROOT", root.path());

            let session_key = "session-test-direct-clear".to_string();
            schedule_persist_session_snapshot(
                session_key.clone(),
                StabilityCounters {
                    pending_requests: 1,
                    ..Default::default()
                },
            );
            clear_session_snapshot(&session_key);
            persist_session_snapshot(
                &session_key,
                StabilityCounters {
                    pending_requests: 7,
                    ..Default::default()
                },
            );

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let snapshot = root
                .path()
                .join("runtime")
                .join("stability-sessions")
                .join("session-test-direct-clear.json");
            assert!(!snapshot.exists(), "direct clear should remain final");
        });
    }

    #[test]
    fn newest_persist_wins_over_older_queued_persist() {
        run_with_brehon_root_test(async {
            let root = tempfile::tempdir().unwrap();
            std::env::set_var("BREHON_ROOT", root.path());

            let session_key = "session-test-newest-persist".to_string();
            schedule_persist_session_snapshot(
                session_key.clone(),
                StabilityCounters {
                    pending_requests: 2,
                    ..Default::default()
                },
            );
            schedule_persist_session_snapshot(
                session_key.clone(),
                StabilityCounters {
                    pending_requests: 8,
                    ..Default::default()
                },
            );

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let snapshot = brehon_types::load_runtime_stability_counters(
                &root
                    .path()
                    .join("runtime")
                    .join("stability-sessions")
                    .join("session-test-newest-persist.json"),
            )
            .expect("snapshot should exist");
            assert_eq!(snapshot.pending_requests, 8);
        });
    }

    #[test]
    fn clear_remains_final_when_persist_arrives_after_close_claim() {
        run_with_brehon_root_sync_test(|| {
            let root = tempfile::tempdir().unwrap();
            std::env::set_var("BREHON_ROOT", root.path());

            let session_key = "session-test-clear-final".to_string();
            persist_session_snapshot(
                &session_key,
                StabilityCounters {
                    pending_requests: 4,
                    ..Default::default()
                },
            );
            let state = session_state(&session_key);
            let clear_version = claim_clear_session_snapshot(&state);
            assert!(claim_persist_session_snapshot(&state).is_none());
            run_clear_session_snapshot(&session_key, &state, clear_version);

            let snapshot = root
                .path()
                .join("runtime")
                .join("stability-sessions")
                .join("session-test-clear-final.json");
            assert!(
                !snapshot.exists(),
                "clear should stay final after close claim"
            );
        });
    }

    #[test]
    fn async_test_helper_restores_brehon_root_after_panic() {
        let _guard = test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _restore_original = EnvVarGuard::preserve("BREHON_ROOT");
        std::env::set_var("BREHON_ROOT", "/tmp/brehon-root-original");

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with_brehon_root_test_locked(async {
                std::env::set_var("BREHON_ROOT", "/tmp/brehon-root-should-be-restored");
                panic!("expected panic to verify BREHON_ROOT cleanup");
            });
        }));

        assert!(panic.is_err(), "helper should propagate panics");
        assert_eq!(
            std::env::var_os("BREHON_ROOT"),
            Some(OsString::from("/tmp/brehon-root-original"))
        );
    }

    #[test]
    fn sync_test_helper_restores_brehon_root_after_panic() {
        let _guard = test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _restore_original = EnvVarGuard::preserve("BREHON_ROOT");
        std::env::set_var("BREHON_ROOT", "/tmp/brehon-root-original");

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with_brehon_root_sync_test_locked(|| {
                std::env::set_var("BREHON_ROOT", "/tmp/brehon-root-should-be-restored");
                panic!("expected panic to verify BREHON_ROOT cleanup");
            });
        }));

        assert!(panic.is_err(), "helper should propagate panics");
        assert_eq!(
            std::env::var_os("BREHON_ROOT"),
            Some(OsString::from("/tmp/brehon-root-original"))
        );
    }
}
