//! Compact proof bundle summary helpers used by MCP tools.
//!
//! The summary DTO itself lives in `brehon-types` so the TUI can consume it
//! without taking a dependency on this crate. This module re-exports it and
//! adds an IO helper that mirrors the latest summary to a side-channel
//! cache file the TUI reads.

use std::path::PathBuf;

#[allow(unused_imports)]
pub use brehon_types::{ProofSummary, PROOF_SUMMARY_LIST_CAP};

/// Best-effort write of a compact summary to a side-channel cache file
/// `.brehon/runtime/proof/{task_id}.json`. The TUI reads this cache so it
/// can render proof evidence without depending on the durable proof store.
/// Failures are logged at warn level and never propagate — the durable
/// event projection is the source of truth.
pub fn write_proof_cache(task_id: &str, summary: &ProofSummary) {
    let Some(dir) = proof_cache_dir() else {
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            task_id,
            error = %err,
            "Failed to create proof cache directory"
        );
        return;
    }
    let path = dir.join(format!("{task_id}.json"));
    match serde_json::to_string_pretty(summary) {
        Ok(payload) => {
            if let Err(err) = std::fs::write(&path, payload) {
                tracing::warn!(
                    task_id,
                    path = %path.display(),
                    error = %err,
                    "Failed to write proof cache file"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                task_id,
                error = %err,
                "Failed to serialize proof summary for cache write"
            );
        }
    }
}

fn proof_cache_dir() -> Option<PathBuf> {
    let root = std::env::var_os("BREHON_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))?;
    Some(root.join("runtime").join("proof"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    struct ScopedRoot {
        saved: Option<OsString>,
    }
    impl ScopedRoot {
        fn set(value: &std::path::Path) -> Self {
            let saved = std::env::var_os("BREHON_ROOT");
            std::env::set_var("BREHON_ROOT", value);
            Self { saved }
        }
    }
    impl Drop for ScopedRoot {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var("BREHON_ROOT", value),
                None => std::env::remove_var("BREHON_ROOT"),
            }
        }
    }

    #[test]
    fn write_proof_cache_writes_summary_to_runtime_proof_dir() {
        let _lock = crate::tools::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".brehon");
        std::fs::create_dir_all(&root).unwrap();
        let _scope = ScopedRoot::set(&root);

        let mut summary = ProofSummary::absent();
        summary.proof_bundle_id = Some("proof-T-write".to_string());
        summary.status = "incomplete".to_string();
        summary.absent = false;
        summary.command_count = 3;
        summary.commits = vec!["abc".to_string()];

        write_proof_cache("T-write", &summary);

        let path = root.join("runtime").join("proof").join("T-write.json");
        assert!(
            path.exists(),
            "cache file should exist at {}",
            path.display()
        );
        let parsed: ProofSummary =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed, summary);
    }
}
