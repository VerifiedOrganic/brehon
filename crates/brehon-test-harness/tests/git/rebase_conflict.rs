use brehon_ports::{GitOperations, RebaseFallbackStrategy, RebaseResult};
use brehon_test_harness::FakeGitOperations;
use std::collections::HashMap;

#[tokio::test]
async fn rebase_conflict_detection() {
    let git = FakeGitOperations::new();

    git.create_branch("feature-a");
    git.create_branch("feature-b");

    let mut files_a = HashMap::new();
    files_a.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "fn x() { 1 }".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-a", files_a);

    let mut files_b = HashMap::new();
    files_b.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "fn x() { 2 }".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-b", files_b.clone());

    git.set_rebase_conflict(vec!["src/lib.rs".to_string()]);

    let result = git.rebase("feature-b", "main").await.unwrap();
    match result {
        RebaseResult::Conflict { files, entries, .. } => {
            assert!(!files.is_empty());
            assert!(files.contains(&"src/lib.rs".to_string()));
            assert!(entries.iter().any(|e| e.path == "src/lib.rs"));
        }
        RebaseResult::Success => panic!("Expected conflict, got success"),
    }
}

#[tokio::test]
async fn rebase_conflict_files_listed_correctly() {
    let git = FakeGitOperations::new();

    git.create_branch("feature-a");
    git.create_branch("feature-b");

    let mut files_a = HashMap::new();
    files_a.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "fn foo() {}".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    files_a.insert(
        "src/bar.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "fn bar() {}".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-a", files_a);

    let mut files_b = HashMap::new();
    files_b.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "fn foo() { modified }".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-b", files_b);

    git.set_rebase_conflict(vec!["src/lib.rs".to_string(), "src/bar.rs".to_string()]);

    let result = git.rebase("feature-b", "main").await.unwrap();
    match result {
        RebaseResult::Conflict { files, entries, .. } => {
            assert_eq!(files.len(), 2);
            assert!(files.contains(&"src/lib.rs".to_string()));
            assert!(files.contains(&"src/bar.rs".to_string()));
            assert_eq!(entries.len(), 2);
        }
        RebaseResult::Success => panic!("Expected conflict"),
    }
}

#[tokio::test]
async fn rebase_on_nonexistent_branch_returns_error() {
    let git = FakeGitOperations::new();

    let result = git.rebase("nonexistent", "main").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn rebase_onto_nonexistent_branch_returns_error() {
    let git = FakeGitOperations::new();
    git.create_branch("feature");

    let result = git.rebase("feature", "nonexistent").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn rebase_then_merge_combined_workflow() {
    let git = FakeGitOperations::new();

    git.create_branch("feature-a");
    git.create_branch("feature-b");

    git.set_rebase_conflict(vec!["src/lib.rs".to_string()]);

    let rebase_result = git.rebase("feature-a", "main").await.unwrap();
    assert!(matches!(rebase_result, RebaseResult::Conflict { .. }));

    git.set_rebase_conflict(vec!["src/other.rs".to_string()]);

    let second_result = git.rebase("feature-b", "main").await.unwrap();
    assert!(matches!(second_result, RebaseResult::Conflict { .. }));
}

#[tokio::test]
async fn rebase_conflict_includes_fallback_strategy() {
    let git = FakeGitOperations::new();
    git.create_branch("feature");

    git.set_rebase_conflict(vec!["src/lib.rs".to_string()]);

    let result = git.rebase("feature", "main").await.unwrap();
    match result {
        RebaseResult::Conflict {
            fallback_attempted, ..
        } => {
            assert!(!matches!(fallback_attempted, RebaseFallbackStrategy::None));
        }
        RebaseResult::Success => panic!("Expected conflict"),
    }
}
