//! Side-channel feedback cache writer.
//!
//! Writes a compact `FeedbackTaskSummary` per task to
//! `.brehon/runtime/feedback/{task_id}.json`. The TUI reads this cache so
//! it can render feedback evidence without depending on the durable
//! supervisor projection. The durable event stream is the source of
//! truth; this cache is best-effort.

use std::path::PathBuf;

use brehon_types::FeedbackTaskSummary;

/// Best-effort write of a compact feedback summary to the side-channel
/// cache file. Failures are logged at warn level and never propagate.
pub fn write_feedback_cache(task_id: &str, summary: &FeedbackTaskSummary) {
    let Some(dir) = feedback_cache_dir() else {
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            task_id,
            error = %err,
            "Failed to create feedback cache directory"
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
                    "Failed to write feedback cache file"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                task_id,
                error = %err,
                "Failed to serialize feedback summary for cache write"
            );
        }
    }
}

fn feedback_cache_dir() -> Option<PathBuf> {
    let root = std::env::var_os("BREHON_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))?;
    Some(root.join("runtime").join("feedback"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::FeedbackTriggerSummary;

    struct ScopedRoot {
        saved: Option<std::ffi::OsString>,
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
    fn cache_writer_round_trips_summary_via_disk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".brehon");
        std::fs::create_dir_all(&root).unwrap();
        let _scope = ScopedRoot::set(&root);

        let summary = FeedbackTaskSummary {
            active_triggers: vec![FeedbackTriggerSummary {
                trigger_id: "fb-cache-1".into(),
                kind: "reviewer_followup".into(),
                summary: "Open follow-up FUP-1".into(),
                created_at: "2026-05-16T00:00:00Z".into(),
            }],
            recent_decisions: Vec::new(),
            pending_clarifications: Vec::new(),
            escalations: Vec::new(),
            drain_active: false,
            safe_mode_active: false,
            updated_at: Some("2026-05-16T00:00:00Z".into()),
        };
        write_feedback_cache("T-cache", &summary);

        let path = root.join("runtime").join("feedback").join("T-cache.json");
        assert!(
            path.exists(),
            "cache file should exist at {}",
            path.display()
        );
        let parsed: FeedbackTaskSummary =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed, summary);
    }
}
