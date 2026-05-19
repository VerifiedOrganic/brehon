//! Chaos test: Git operations under random delays and concurrent load.
//!
//! Tests that FakeGitOperations (and the patterns it models) remain safe
//! when multiple tasks race with rebase, merge, branch creation, and
//! worktree operations under injected chaos.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use brehon_ports::GitOperations;
use brehon_test_harness::{ChaosConfig, ChaosInjector, FakeGitOperations};

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chaos_git_concurrent_branch_creation() {
    let git = Arc::new(FakeGitOperations::new());
    let config = ChaosConfig::with_delays(Duration::from_millis(0), Duration::from_millis(10));
    let task_count = 20;
    let branches_per_task = 10;

    let mut handles = vec![];

    for task_id in 0..task_count {
        let git = Arc::clone(&git);
        let mut injector = ChaosInjector::new(config.clone());

        handles.push(tokio::spawn(async move {
            let mut created = 0;
            for i in 0..branches_per_task {
                injector.delay().await;

                let branch = format!("task-{}-branch-{}", task_id, i);
                git.create_branch(&branch);
                created += 1;
            }
            created
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;
    let total_created: usize = results.into_iter().map(|r| r.unwrap()).sum();

    assert_eq!(
        total_created,
        task_count * branches_per_task,
        "All branches should be created despite chaos"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chaos_git_rebase_merge_race() {
    let git = Arc::new(FakeGitOperations::new());
    let config = ChaosConfig::with_delays(Duration::from_millis(0), Duration::from_millis(5));

    // Pre-create branches
    for i in 0..10 {
        git.create_branch(&format!("feature/race-{}", i));
    }

    let mut handles = vec![];

    for i in 0..10 {
        let git = Arc::clone(&git);
        let mut injector = ChaosInjector::new(config.clone());
        let branch = format!("feature/race-{}", i);

        handles.push(tokio::spawn(async move {
            injector.delay().await;
            let rebase = git.rebase(&branch, "main").await;
            injector.delay().await;
            let merge = git.merge(&branch).await;
            (rebase.is_ok(), merge.is_ok())
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;
    let successes: usize = results
        .into_iter()
        .map(|r| {
            let (r_ok, m_ok) = r.unwrap();
            if r_ok && m_ok {
                1
            } else {
                0
            }
        })
        .sum();

    assert_eq!(successes, 10, "All rebase+merge pairs should complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chaos_git_conflict_then_success() {
    let git = Arc::new(FakeGitOperations::new());
    let config = ChaosConfig::with_delays(Duration::from_millis(0), Duration::from_millis(5));

    git.create_branch("feature/conflict-chaos");

    let mut files = HashMap::new();
    files.insert(
        "lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "pub fn add(a: i32, b: i32) -> i32 { a + b }".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature/conflict-chaos", files);
    git.set_rebase_conflict(vec!["lib.rs".to_string()]);

    let mut handles = vec![];

    for i in 0..5 {
        let git = Arc::clone(&git);
        let mut injector = ChaosInjector::new(config.clone());

        handles.push(tokio::spawn(async move {
            injector.delay().await;
            let result = git.rebase("feature/conflict-chaos", "main").await;
            (i, result)
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    // First concurrent call may get conflict, subsequent should succeed
    // because the conflict state is consumed
    let mut _conflicts = 0;
    let mut successes = 0;

    for result in results {
        let (_, rebase_result) = result.unwrap();
        match rebase_result {
            Ok(brehon_ports::RebaseResult::Conflict { .. }) => _conflicts += 1,
            Ok(brehon_ports::RebaseResult::Success) => successes += 1,
            Err(_) => {}
        }
    }

    assert!(
        successes >= 4,
        "Most rebases should succeed after conflict state is cleared, got {} successes",
        successes
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chaos_git_worktree_race() {
    let git = Arc::new(FakeGitOperations::new());
    let config = ChaosConfig::with_delays(Duration::from_millis(0), Duration::from_millis(5));

    for i in 0..10 {
        git.create_branch(&format!("feature/wt-{}", i));
    }

    let mut handles = vec![];

    for i in 0..10 {
        let git = Arc::clone(&git);
        let mut injector = ChaosInjector::new(config.clone());
        let branch = format!("feature/wt-{}", i);
        let path = std::path::PathBuf::from(format!("/tmp/chaos-wt-{}", i));

        handles.push(tokio::spawn(async move {
            injector.delay().await;
            let created = git.create_worktree(&branch, &path).await.is_ok();
            injector.delay().await;
            let removed = git.remove_worktree(&path).await.is_ok();
            (created, removed)
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;
    let total_ops: usize = results
        .into_iter()
        .map(|r| {
            let (c, r) = r.unwrap();
            if c && r {
                1
            } else {
                0
            }
        })
        .sum();

    assert_eq!(
        total_ops, 10,
        "All worktree create+remove pairs should complete"
    );
}
