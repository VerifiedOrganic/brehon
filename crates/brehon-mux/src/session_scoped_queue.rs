use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Result;

pub type EntryId = String;

pub type ScopedEntry<T> = Result<StoredScopedEntry<T>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredScopedEntry<T> {
    pub id: EntryId,
    pub session_name: String,
    pub entry: T,
}

pub struct SessionScopedQueue<T> {
    session_name: String,
    dir: PathBuf,
    _marker: PhantomData<T>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedEntry<T> {
    session_name: String,
    entry: T,
}

impl<T: Serialize + DeserializeOwned> SessionScopedQueue<T> {
    pub fn new(session_name: &str, dir: PathBuf) -> Self {
        Self {
            session_name: session_name.trim().to_string(),
            dir,
            _marker: PhantomData,
        }
    }

    pub fn enqueue(&self, entry: T) -> Result<EntryId> {
        std::fs::create_dir_all(&self.dir)?;

        let entry_id = format!(
            "{:020}-{}",
            chrono::Utc::now().timestamp_millis(),
            uuid::Uuid::new_v4()
        );
        let file_name = format!("{entry_id}.entry");
        let final_path = self.dir.join(&file_name);
        let temp_path = self.dir.join(format!(".{file_name}.tmp"));

        let payload = PersistedEntry {
            session_name: self.session_name.clone(),
            entry,
        };
        let encoded = serde_json::to_vec(&payload)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        std::fs::write(&temp_path, encoded)?;
        std::fs::rename(&temp_path, &final_path)?;
        Ok(entry_id)
    }

    pub fn drain(&self) -> impl Iterator<Item = ScopedEntry<T>> {
        let mut drained = Vec::new();
        let mut skipped_foreign = 0usize;

        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return drained.into_iter();
            }
            Err(err) => {
                drained.push(Err(err.into()));
                return drained.into_iter();
            }
        };

        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();

        for path in paths {
            if !path.is_file() {
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }

            match self.read_entry(&path) {
                Ok(payload) => {
                    if payload.session_name != self.session_name {
                        skipped_foreign = skipped_foreign.saturating_add(1);
                        continue;
                    }

                    if let Err(err) = std::fs::remove_file(&path) {
                        drained.push(Err(err.into()));
                        continue;
                    }

                    drained.push(Ok(StoredScopedEntry {
                        id: entry_id_from_path(&path),
                        session_name: payload.session_name,
                        entry: payload.entry,
                    }));
                }
                Err(err) => {
                    let move_result = self.move_to_dead_letter(&path);
                    drained.push(match move_result {
                        Ok(()) => Err(err),
                        Err(move_err) => Err(move_err),
                    });
                }
            }
        }

        if skipped_foreign > 0 {
            tracing::debug!(
                skipped_foreign,
                session = %self.session_name,
                "left foreign session queue entries untouched"
            );
        }

        drained.into_iter()
    }

    fn read_entry(&self, path: &Path) -> Result<PersistedEntry<T>> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err).into())
    }

    fn dead_letter_dir(&self) -> PathBuf {
        self.dir.join("dead-letter")
    }

    fn move_to_dead_letter(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }

        let dead_letter_dir = self.dead_letter_dir();
        std::fs::create_dir_all(&dead_letter_dir)?;

        let original_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("entry");
        let dead_letter_name = format!(
            "{:020}-{}",
            chrono::Utc::now().timestamp_millis(),
            original_name
        );
        let target = dead_letter_dir.join(dead_letter_name);
        std::fs::rename(path, target)?;
        Ok(())
    }
}

fn entry_id_from_path(path: &Path) -> EntryId {
    path.file_stem()
        .and_then(|value| value.to_str())
        .or_else(|| path.file_name().and_then(|value| value.to_str()))
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct TestPayload {
        message: String,
    }

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create temp test dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn enqueue_and_drain_round_trip() {
        let temp = TestDir::new("session-scoped-queue-roundtrip");
        let queue = SessionScopedQueue::new("session-a", temp.path().to_path_buf());

        let id = queue
            .enqueue(TestPayload {
                message: "hello".to_string(),
            })
            .expect("enqueue entry");

        let drained: Vec<_> = queue.drain().collect();
        assert_eq!(drained.len(), 1);

        let entry = drained[0].as_ref().expect("drain result should be ok");
        assert_eq!(entry.id, id);
        assert_eq!(entry.session_name, "session-a");
        assert_eq!(
            entry.entry,
            TestPayload {
                message: "hello".to_string(),
            }
        );
    }

    #[test]
    fn drain_leaves_cross_session_entries_for_their_owner() {
        let temp = TestDir::new("session-scoped-queue-cross-session");
        let queue_a = SessionScopedQueue::new("session-a", temp.path().to_path_buf());
        let queue_b = SessionScopedQueue::new("session-b", temp.path().to_path_buf());

        queue_a
            .enqueue(TestPayload {
                message: "from-a".to_string(),
            })
            .expect("enqueue session-a entry");
        queue_b
            .enqueue(TestPayload {
                message: "from-b".to_string(),
            })
            .expect("enqueue session-b entry");

        let drained_a: Vec<_> = queue_a.drain().collect();
        assert_eq!(drained_a.len(), 1);
        let entry = drained_a[0]
            .as_ref()
            .expect("session-a result should be ok");
        assert_eq!(entry.session_name, "session-a");
        assert_eq!(entry.entry.message, "from-a");

        let dead_letter_dir = temp.path().join("dead-letter");
        assert!(
            !dead_letter_dir.exists(),
            "foreign session entries should not be dead-lettered"
        );

        let drained_b: Vec<_> = queue_b.drain().collect();
        assert_eq!(drained_b.len(), 1);
        let entry = drained_b[0]
            .as_ref()
            .expect("session-b result should be ok");
        assert_eq!(entry.session_name, "session-b");
        assert_eq!(entry.entry.message, "from-b");
    }

    #[test]
    fn drain_moves_corrupt_files_to_dead_letter_and_returns_error() {
        let temp = TestDir::new("session-scoped-queue-corrupt");
        let queue = SessionScopedQueue::<TestPayload>::new("session-a", temp.path().to_path_buf());

        let corrupt_path = temp.path().join("corrupt.entry");
        std::fs::write(&corrupt_path, "{not valid json").expect("write corrupt entry");

        let drained: Vec<_> = queue.drain().collect();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].is_err(), "corrupt file should produce Err");

        assert!(
            !corrupt_path.exists(),
            "corrupt file should be moved out of active queue dir"
        );

        let dead_letter_dir = temp.path().join("dead-letter");
        let dead_letter_count = std::fs::read_dir(dead_letter_dir)
            .expect("dead-letter dir exists")
            .flatten()
            .count();
        assert_eq!(dead_letter_count, 1, "corrupt file should be dead-lettered");
    }
}
