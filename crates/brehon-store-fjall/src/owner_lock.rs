use std::collections::HashMap;
#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::store::StoreError;

const OWNER_LOCK_SUFFIX: &str = "owner.lock";

static STORE_OWNER_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<StoreOwnerLockInner>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub(crate) struct StoreOwnerLock {
    _inner: Arc<StoreOwnerLockInner>,
}

struct StoreOwnerLockInner {
    #[cfg(unix)]
    file: File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl Drop for StoreOwnerLockInner {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl StoreOwnerLock {
    pub(crate) fn acquire(db_path: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(db_path)?;
        let lock_path = owner_lock_path(db_path);
        let key = stable_owner_lock_key(&lock_path);
        let mut locks = STORE_OWNER_LOCKS
            .lock()
            .map_err(|_| StoreError::Storage("Fjall owner lock registry is poisoned".into()))?;
        if let Some(existing) = locks.get(&key).and_then(Weak::upgrade) {
            return Ok(Self { _inner: existing });
        }

        let inner = Arc::new(StoreOwnerLockInner::acquire(&lock_path)?);
        locks.insert(key, Arc::downgrade(&inner));
        Ok(Self { _inner: inner })
    }
}

impl StoreOwnerLockInner {
    fn acquire(lock_path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(lock_path)?;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == ErrorKind::WouldBlock {
                    return Err(StoreError::Storage(format!(
                        "Fjall event store is already open by another process at {}; use runtime-file MCP backing or the owning Brehon process",
                        lock_path.display()
                    )));
                }
                return Err(StoreError::Io(err));
            }
            Ok(Self { file })
        }
        #[cfg(not(unix))]
        {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(lock_path)
            {
                Ok(_) => Ok(Self {
                    path: lock_path.to_path_buf(),
                }),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    Err(StoreError::Storage(format!(
                        "Fjall event store is already open by another process at {}; use runtime-file MCP backing or the owning Brehon process",
                        lock_path.display()
                    )))
                }
                Err(err) => Err(StoreError::Io(err)),
            }
        }
    }
}

fn owner_lock_path(db_path: &Path) -> PathBuf {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("fjall");
    parent.join(format!(".{name}.{OWNER_LOCK_SUFFIX}"))
}

fn stable_owner_lock_key(lock_path: &Path) -> PathBuf {
    if let Some(parent) = lock_path
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
    {
        if let Some(file_name) = lock_path.file_name() {
            return parent.join(file_name);
        }
    }
    std::fs::canonicalize(lock_path).unwrap_or_else(|_| lock_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FjallEventStore;
    use std::process::Command;
    use tempfile::tempdir;

    const FJALL_LOCK_PROBE_PATH_ENV: &str = "BREHON_FJALL_LOCK_PROBE_PATH";

    #[test]
    fn owner_lock_rejects_cross_process_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db");
        let _store = FjallEventStore::new(&path).unwrap();

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "owner_lock::tests::fjall_owner_lock_child_probe",
            ])
            .env(FJALL_LOCK_PROBE_PATH_ENV, &path)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child probe failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore]
    fn fjall_owner_lock_child_probe() {
        let Some(path) = std::env::var_os(FJALL_LOCK_PROBE_PATH_ENV).map(PathBuf::from) else {
            return;
        };

        match FjallEventStore::new(&path) {
            Ok(_) => panic!("child process unexpectedly opened store while parent held owner lock"),
            Err(err) => assert!(
                err.to_string().contains("already open by another process"),
                "unexpected owner lock error: {err}"
            ),
        }
    }
}
