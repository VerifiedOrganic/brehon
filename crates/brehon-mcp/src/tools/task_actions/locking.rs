//! File-based locking for task and repository operations.

use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::paths::{brehon_root_dir, tasks_dir};

pub(super) const TASK_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const TASK_LOCK_RETRY: Duration = Duration::from_millis(10);
pub(super) const TASK_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

pub(crate) struct TaskLock {
    path: PathBuf,
}

impl Drop for TaskLock {
    fn drop(&mut self) {
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

pub(crate) async fn acquire_task_lock(task_id: &str) -> Result<TaskLock, String> {
    let path = task_lock_path(task_id).ok_or_else(|| "No tasks dir".to_string())?;
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(TaskLock { path }),
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
            Ok(_) => return Ok(TaskLock { path }),
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
}

impl Drop for RepoLock {
    fn drop(&mut self) {
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
            Ok(_) => return Ok(RepoLock { path }),
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
