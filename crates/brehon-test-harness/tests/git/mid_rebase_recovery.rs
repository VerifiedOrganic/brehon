use brehon_ports::{GitOperations, RebaseResult};
use brehon_test_harness::FakeGitOperations;
use std::collections::HashMap;

#[tokio::test]
async fn detect_mid_rebase_state() {
    let git = FakeGitOperations::new();
    git.create_branch("rebase-in-progress");

    let mut files = HashMap::new();
    files.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "conflicting change".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("rebase-in-progress", files.clone());

    git.set_rebase_conflict(vec!["src/lib.rs".to_string()]);

    let result = git.rebase("rebase-in-progress", "main").await.unwrap();
    assert!(matches!(result, RebaseResult::Conflict { .. }));
}

#[tokio::test]
async fn mid_rebase_returns_conflict_files() {
    let git = FakeGitOperations::new();
    git.create_branch("feature-rebase");

    let conflict_files = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
    git.set_rebase_conflict(conflict_files.clone());

    let result = git.rebase("feature-rebase", "main").await.unwrap();
    match result {
        RebaseResult::Conflict { files, entries, .. } => {
            assert_eq!(files.len(), 2);
            assert!(files.contains(&"src/lib.rs".to_string()));
            assert!(files.contains(&"src/main.rs".to_string()));
            assert_eq!(entries.len(), 2);
        }
        RebaseResult::Success => panic!("Expected conflict in mid-rebase"),
    }
}

#[tokio::test]
async fn clean_rebase_after_conflict_resolution() {
    let git = FakeGitOperations::new();
    git.create_branch("resolved-branch");

    let result = git.rebase("resolved-branch", "main").await.unwrap();
    assert!(matches!(result, RebaseResult::Success));

    let branch = git.current_branch().await.unwrap();
    assert_eq!(branch, "resolved-branch");
}

#[tokio::test]
async fn rebase_state_resets_after_conflict() {
    let git = FakeGitOperations::new();
    git.create_branch("first-rebase");
    git.create_branch("second-rebase");

    git.set_rebase_conflict(vec!["src/conflict.rs".to_string()]);

    let first_result = git.rebase("first-rebase", "main").await.unwrap();
    assert!(matches!(first_result, RebaseResult::Conflict { .. }));

    let second_result = git.rebase("second-rebase", "main").await.unwrap();
    assert!(matches!(second_result, RebaseResult::Success));
}

#[tokio::test]
async fn multiple_sequential_rebases() {
    let git = FakeGitOperations::new();

    git.create_branch("branch-a");
    git.create_branch("branch-b");
    git.create_branch("branch-c");

    let rebase_a = git.rebase("branch-a", "main").await.unwrap();
    assert!(matches!(rebase_a, RebaseResult::Success));

    let rebase_b = git.rebase("branch-b", "main").await.unwrap();
    assert!(matches!(rebase_b, RebaseResult::Success));

    git.set_rebase_conflict(vec!["src/file.rs".to_string()]);
    let rebase_c = git.rebase("branch-c", "main").await.unwrap();
    assert!(matches!(rebase_c, RebaseResult::Conflict { .. }));
}

#[tokio::test]
async fn rebase_conflict_with_file_content_diff() {
    let git = FakeGitOperations::new();
    git.create_branch("conflict-branch");

    let mut main_files = HashMap::new();
    main_files.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "fn original() {}".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("main", main_files);

    let mut files = HashMap::new();
    files.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "fn different() {}".to_string(),
            lines_added: 5,
            lines_removed: 3,
        },
    );
    git.set_branch_files("conflict-branch", files);

    let conflicts = git.has_conflicts("conflict-branch", "main").await.unwrap();
    assert!(!conflicts.is_empty());
    assert!(conflicts.contains(&"src/lib.rs".to_string()));
}

#[tokio::test]
async fn rebase_state_cleared_on_success() {
    let git = FakeGitOperations::new();
    git.create_branch("clean-branch");

    git.set_rebase_conflict(vec!["temporary.rs".to_string()]);

    let result = git.rebase("clean-branch", "main").await.unwrap();
    assert!(matches!(result, RebaseResult::Conflict { .. }));

    git.create_branch("next-branch");
    let clean_result = git.rebase("next-branch", "main").await.unwrap();
    assert!(matches!(clean_result, RebaseResult::Success));
}
