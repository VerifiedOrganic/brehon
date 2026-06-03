use brehon_ports::GitOperations;
use brehon_test_harness::FakeGitOperations;
use std::collections::HashMap;
use std::path::Path;

#[tokio::test]
async fn detect_uncommitted_changes_on_startup() {
    let git = FakeGitOperations::new();
    git.create_branch("feature");

    let mut dirty_files = HashMap::new();
    dirty_files.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "modified content".to_string(),
            lines_added: 10,
            lines_removed: 5,
        },
    );
    git.set_branch_files("feature", dirty_files);

    let diff = git.diff("feature", "main").await.unwrap();
    assert!(!diff.files.is_empty());
}

#[tokio::test]
async fn dirty_worktree_preserves_changes() {
    let git = FakeGitOperations::new();
    git.create_branch("work-branch");

    let mut files = HashMap::new();
    files.insert(
        "src/main.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "fn main() {}".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("work-branch", files.clone());

    let diff = git.diff("work-branch", "main").await.unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "src/main.rs");
    assert_eq!(diff.files[0].additions, 1);
}

#[tokio::test]
async fn worktree_creation_with_dirty_state() {
    let git = FakeGitOperations::new();
    git.create_branch("dirty-branch");

    let path = Path::new("/tmp/test-worktree");

    let result = git.create_worktree("dirty-branch", path).await;
    assert!(result.is_ok());

    let result = git.remove_worktree(path).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn multiple_dirty_files_detection() {
    let git = FakeGitOperations::new();
    git.create_branch("multi-dirty");

    let mut files = HashMap::new();
    files.insert(
        "src/a.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "a".to_string(),
            lines_added: 5,
            lines_removed: 2,
        },
    );
    files.insert(
        "src/b.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "b".to_string(),
            lines_added: 3,
            lines_removed: 1,
        },
    );
    files.insert(
        "src/c.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "c".to_string(),
            lines_added: 10,
            lines_removed: 0,
        },
    );
    git.set_branch_files("multi-dirty", files);

    let diff = git.diff("multi-dirty", "main").await.unwrap();
    assert_eq!(diff.files.len(), 3);
}

#[tokio::test]
async fn clean_worktree_no_changes() {
    let git = FakeGitOperations::new();
    git.create_branch("clean-branch");

    let diff = git.diff("clean-branch", "main").await.unwrap();
    assert!(diff.files.is_empty());
}

#[tokio::test]
async fn worktree_already_exists_error() {
    let git = FakeGitOperations::new();
    git.create_branch("feature");

    let path = Path::new("/tmp/existing-worktree");

    git.create_worktree("feature", path).await.unwrap();

    let result = git.create_worktree("feature", path).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn worktree_removal_nonexistent_error() {
    let git = FakeGitOperations::new();

    let result = git.remove_worktree(Path::new("/nonexistent/path")).await;
    assert!(result.is_err());
}
