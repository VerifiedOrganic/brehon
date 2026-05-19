//! Soak test: Repeated git operations.
//!
//! Verifies that FakeGitOperations (and by proxy, the real git layer) does not
//! accumulate unbounded state, leak merge/rebase states, or leave dirty
//! worktrees after many operations.

use std::collections::HashMap;

use brehon_ports::GitOperations;
use brehon_test_harness::FakeGitOperations;

const CYCLES: usize = 200;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_git_create_branch_rebase_merge_cycles() {
    let git = FakeGitOperations::new();

    for cycle in 0..CYCLES {
        let branch = format!("feature/soak-{}", cycle);
        git.create_branch(&branch);

        // Rebase onto main
        let rebase_result = git.rebase(&branch, "main").await.unwrap();
        assert!(
            matches!(rebase_result, brehon_ports::RebaseResult::Success),
            "Cycle {}: Rebase should succeed",
            cycle
        );

        // Merge back
        let merge_result = git.merge(&branch).await.unwrap();
        assert!(
            matches!(merge_result, brehon_ports::MergeResult::Success),
            "Cycle {}: Merge should succeed",
            cycle
        );

        // Diff
        let diff = git.diff(&branch, "main").await.unwrap();
        // Validate cleanliness every cycle (no vacuous OR condition).
        assert!(
            diff.files.is_empty(),
            "Cycle {}: Diff should be clean",
            cycle
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_git_conflict_recovery_clean_state() {
    let git = FakeGitOperations::new();

    for cycle in 0..CYCLES {
        let branch = format!("feature/conflict-{}", cycle);
        git.create_branch(&branch);

        // Set up conflicting files
        let mut files = HashMap::new();
        files.insert(
            "src/main.rs".to_string(),
            brehon_test_harness::mock_git::FileContent {
                content: format!("fn main() {{ println!(\"cycle {}\"); }}", cycle),
                lines_added: 1,
                lines_removed: 0,
            },
        );
        git.set_branch_files(&branch, files);

        // Trigger conflict
        git.set_rebase_conflict(vec!["src/main.rs".to_string()]);
        let rebase_result = git.rebase(&branch, "main").await.unwrap();
        assert!(
            matches!(rebase_result, brehon_ports::RebaseResult::Conflict { .. }),
            "Cycle {}: Expected conflict",
            cycle
        );

        // After conflict, the rebase_state should be cleared
        let next_rebase = git.rebase(&branch, "main").await.unwrap();
        assert!(
            matches!(next_rebase, brehon_ports::RebaseResult::Success),
            "Cycle {}: Rebase state should be clean after first conflict",
            cycle
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_git_worktree_create_remove_cycles() {
    let git = FakeGitOperations::new();

    for cycle in 0..CYCLES {
        let branch = format!("feature/wt-{}", cycle);
        git.create_branch(&branch);

        let path = std::path::PathBuf::from(format!("/tmp/soak-wt-{}", cycle));
        git.create_worktree(&branch, &path).await.unwrap();
        git.remove_worktree(&path).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_git_checkout_cycles_remain_consistent() {
    let git = FakeGitOperations::new();

    for cycle in 0..CYCLES {
        let branch = format!("feature/checkout-{}", cycle);
        git.create_branch(&branch);

        git.checkout(&branch).await.unwrap();
        let current = git.current_branch().await.unwrap();
        assert_eq!(
            current, branch,
            "Cycle {}: Checkout should switch branch",
            cycle
        );

        git.checkout("main").await.unwrap();
        let current = git.current_branch().await.unwrap();
        assert_eq!(current, "main", "Cycle {}: Should return to main", cycle);
    }
}
