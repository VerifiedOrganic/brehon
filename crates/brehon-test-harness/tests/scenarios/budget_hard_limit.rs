//! Test: the LIVE token-accounting path that the budget kill-switch reads.
//!
//! The Wave-1 budget gate (`brehon-tui` `run::budget::evaluate_budget`, unit-
//! and integration-tested in that crate against the real tick + dispatch seam)
//! enforces against the **live per-task token rollup** persisted by
//! `record_task_token_usage` and read back by `read_run_total_tokens`. The
//! previous version of this scenario hand-appended a `SystemDraining` event and
//! asserted it existed — it drove no gate and proved nothing.
//!
//! This version drives the *real* production accounting path end to end:
//! - tokens are recorded through `record_task_token_usage` (the exact function
//!   the live ACP/codex/gemini/opencode sessions call), and
//! - the run total is read through `read_run_total_tokens` (the exact function
//!   the gate's `read_spend_snapshot` calls),
//!
//! then asserts the run-total spend signal crosses a Hard cap — i.e. the signal
//! the kill-switch fires on is real, monotonic, and survives restart, unlike
//! the `tokens_used: 0` `ResponseReceived` path.

use brehon_types::{read_run_total_tokens, read_task_token_usage, record_task_token_usage};

/// Write a task JSON the rollup can update (id, type, optional parent).
fn write_task(brehon_root: &std::path::Path, id: &str, task_type: &str, parent: Option<&str>) {
    let tasks = brehon_root.join("runtime").join("tasks");
    std::fs::create_dir_all(&tasks).unwrap();
    let mut task = serde_json::json!({
        "task_id": id,
        "task_type": task_type,
        "status": "in_progress",
    });
    if let Some(parent) = parent {
        task["parent_id"] = serde_json::Value::String(parent.to_string());
    }
    std::fs::write(
        tasks.join(format!("{id}.json")),
        serde_json::to_string_pretty(&task).unwrap(),
    )
    .unwrap();
}

#[test]
fn live_rollup_crosses_hard_token_cap() {
    let brehon_root = tempfile::tempdir().unwrap();
    let root = brehon_root.path();

    // A realistic hierarchy: initiative > epic > task. The rollup must
    // accumulate into ancestors so the run-total is read off the initiative.
    write_task(root, "I-1", "initiative", None);
    write_task(root, "E-1", "epic", Some("I-1"));
    write_task(root, "T-1", "task", Some("E-1"));

    let hard_cap: u64 = 100_000;

    // Below the cap after the first chunk of real spend.
    let updated = record_task_token_usage(root, "T-1", 40_000).unwrap();
    assert_eq!(updated, vec!["T-1", "E-1", "I-1"], "rollup hits ancestors");
    assert!(
        read_run_total_tokens(root).unwrap() < hard_cap,
        "still under the Hard cap"
    );

    // More real spend pushes the run-total over the Hard cap.
    record_task_token_usage(root, "T-1", 70_000).unwrap();
    let run_total = read_run_total_tokens(root).unwrap();
    assert_eq!(
        run_total, 110_000,
        "run-total is the initiative rollup, not a triple count of every row"
    );
    assert!(
        run_total >= hard_cap,
        "the live spend signal the kill-switch reads has crossed the Hard cap"
    );

    // The per-task reader (used by the gate to distinguish unknown from zero)
    // sees the same monotonic value, and a missing task reads as unknown.
    assert_eq!(read_task_token_usage(root, "I-1").unwrap(), Some(110_000));
    assert_eq!(read_task_token_usage(root, "missing").unwrap(), None);
}

#[test]
fn unreadable_rollup_is_distinguishable_from_zero_for_fail_closed() {
    // The gate fails closed when spend is unknown under a Hard cap. The reader
    // must therefore report a genuinely empty run as a real zero (Ok(0)) and a
    // never-written task as unknown (Ok(None)) — not conflate the two.
    let brehon_root = tempfile::tempdir().unwrap();
    let root = brehon_root.path();

    // No tasks dir at all: a legitimately empty run reads as zero spend.
    assert_eq!(read_run_total_tokens(root).unwrap(), 0);

    // A task that was never charged reads as "unknown", which the gate treats
    // as fail-closed material under a Hard cap.
    write_task(root, "T-1", "task", None);
    assert_eq!(read_task_token_usage(root, "T-1").unwrap(), None);
}
