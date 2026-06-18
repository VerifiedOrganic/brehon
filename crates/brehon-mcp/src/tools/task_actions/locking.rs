//! File-based locking for task and repository operations.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::paths::{brehon_root_dir, tasks_dir};

pub(super) const TASK_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const TASK_LOCK_RETRY: Duration = Duration::from_millis(10);

#[cfg(not(test))]
pub(super) const TASK_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);
#[cfg(test)]
pub(super) const TASK_LOCK_STALE_AFTER: Duration = Duration::from_secs(1);

// A live holder touches its lock every 10s (1/3 of the 30s stale window, 3x
// headroom even on coarse-mtime filesystems) so `clear_stale_lock` never sees a
// >=30s-old mtime for a live lock and therefore never force-clears it.
#[cfg(not(test))]
pub(super) const TASK_LOCK_HEARTBEAT: Duration = Duration::from_secs(10);
#[cfg(test)]
pub(super) const TASK_LOCK_HEARTBEAT: Duration = Duration::from_millis(100);

pub(crate) struct TaskLock {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for TaskLock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.heartbeat.take() {
            // Drop cannot `.await`; `abort` is non-blocking and the heartbeat
            // task can never outlive this guard.
            handle.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn task_lock_path(task_id: &str) -> Option<PathBuf> {
    tasks_dir().map(|dir| dir.join(format!(".{task_id}.lock")))
}

fn clear_stale_lock(path: &std::path::Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let Ok(age) = modified.elapsed() else {
        return;
    };
    if age >= TASK_LOCK_STALE_AFTER {
        let _ = std::fs::remove_file(path);
    }
}

/// Advance the mtime of an existing lock file.
///
/// Opens the file for writing (never `create_new`, so a removed lock is not
/// resurrected) and writes a small marker so the mtime advances even on
/// filesystems that treat a no-op truncate as metadata-neutral. The
/// `io::Result` lets the heartbeat detect a removed file.
fn touch_lock(path: &std::path::Path) -> std::io::Result<()> {
    const HEARTBEAT_MARKER: &[u8] = b"heartbeat\n";

    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(HEARTBEAT_MARKER)?;
    f.set_len(HEARTBEAT_MARKER.len() as u64)?;
    f.sync_all()?;
    Ok(())
}

/// Spawn a detached tokio task that renews `path`'s mtime every
/// `TASK_LOCK_HEARTBEAT` until `stop` is set. If the file disappears
/// (`NotFound`) the loop exits so we never resurrect a lock another holder may
/// now own.
fn spawn_heartbeat(path: PathBuf, stop: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TASK_LOCK_HEARTBEAT);
        // The first tick fires immediately; the file already exists by the time
        // this task runs, so touching on the first tick is harmless.
        loop {
            ticker.tick().await;
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let touch_path = path.clone();
            match tokio::task::spawn_blocking(move || touch_lock(&touch_path)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) if err.kind() == ErrorKind::NotFound => {
                    break;
                }
                Ok(Err(_)) => {}
                Err(_) => break,
            }
        }
    })
}

pub(crate) async fn acquire_task_lock(task_id: &str) -> Result<TaskLock, String> {
    let path = task_lock_path(task_id).ok_or_else(|| "No tasks dir".to_string())?;
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => {
                let stop = Arc::new(AtomicBool::new(false));
                let heartbeat = spawn_heartbeat(path.clone(), stop.clone());
                return Ok(TaskLock {
                    path,
                    stop,
                    heartbeat: Some(heartbeat),
                });
            }
            Err(err)
                if err.kind() == ErrorKind::AlreadyExists
                    && start.elapsed() < TASK_LOCK_TIMEOUT =>
            {
                clear_stale_lock(&path);
                tokio::time::sleep(TASK_LOCK_RETRY).await;
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                return Err(format!("Timed out waiting for task lock for {task_id}"));
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

pub(crate) fn acquire_task_lock_blocking(task_id: &str) -> Result<TaskLock, String> {
    let path = task_lock_path(task_id).ok_or_else(|| "No tasks dir".to_string())?;
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            // Blocking callers (migration) run outside a tokio runtime and hold
            // the lock only for a single fast write, far under the stale window,
            // so they need no heartbeat (spawning one here would panic with no
            // reactor running).
            Ok(_) => {
                return Ok(TaskLock {
                    path,
                    stop: Arc::new(AtomicBool::new(false)),
                    heartbeat: None,
                });
            }
            Err(err)
                if err.kind() == ErrorKind::AlreadyExists
                    && start.elapsed() < TASK_LOCK_TIMEOUT =>
            {
                clear_stale_lock(&path);
                std::thread::sleep(TASK_LOCK_RETRY);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                return Err(format!("Timed out waiting for task lock for {task_id}"));
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

pub(crate) struct RepoLock {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.heartbeat.take() {
            // Drop cannot `.await`; `abort` is non-blocking and the heartbeat
            // task can never outlive this guard.
            handle.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn repo_lock_path() -> Option<PathBuf> {
    let root = brehon_root_dir()?;
    let runtime_dir = root.join("runtime");
    std::fs::create_dir_all(&runtime_dir).ok()?;
    Some(runtime_dir.join(".repo.lock"))
}

pub(super) async fn acquire_repo_lock() -> Result<RepoLock, String> {
    let path = repo_lock_path().ok_or_else(|| "No brehon runtime dir".to_string())?;
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => {
                let stop = Arc::new(AtomicBool::new(false));
                let heartbeat = spawn_heartbeat(path.clone(), stop.clone());
                return Ok(RepoLock {
                    path,
                    stop,
                    heartbeat: Some(heartbeat),
                });
            }
            Err(err)
                if err.kind() == ErrorKind::AlreadyExists
                    && start.elapsed() < TASK_LOCK_TIMEOUT =>
            {
                clear_stale_lock(&path);
                tokio::time::sleep(TASK_LOCK_RETRY).await;
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                return Err("Timed out waiting for repository integration lock".to_string());
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ScopedEnv, TEST_ENV_LOCK};

    // The stale window is intentionally larger than the heartbeat so a live
    // holder always refreshes within one window.
    fn beyond_stale_window() -> Duration {
        TASK_LOCK_STALE_AFTER + Duration::from_millis(100)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_long_lock_is_not_reclaimed() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

        let lock = acquire_repo_lock().await.expect("first acquire succeeds");
        let lock_path = repo_lock_path().expect("repo lock path");
        assert!(lock_path.exists(), "lock file should exist while held");

        // Hold the live lock past the stale window. The heartbeat keeps its
        // mtime fresh, so a second acquirer must NOT force-clear it.
        tokio::time::sleep(beyond_stale_window()).await;

        let second = acquire_repo_lock().await;
        assert!(
            second.is_err(),
            "a live, heartbeating lock must not be reclaimed by a concurrent acquirer"
        );

        drop(lock);
        // Give Drop's remove_file a moment to settle the filesystem state.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !lock_path.exists(),
            "lock file should be removed once the guard is dropped"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dead_stale_lock_is_reclaimed() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let root = tempfile::tempdir().unwrap();
        let _env = ScopedEnv::set(&[("BREHON_ROOT", root.path().to_str().unwrap())]);

        // Simulate a dead holder: a lock file with no heartbeat keeping it
        // fresh.
        let lock_path = repo_lock_path().expect("repo lock path");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .expect("create orphan lock file");

        tokio::time::sleep(beyond_stale_window()).await;

        let reclaimed = acquire_repo_lock().await;
        assert!(
            reclaimed.is_ok(),
            "a stale lock from a dead holder must be reclaimable"
        );
    }
}
