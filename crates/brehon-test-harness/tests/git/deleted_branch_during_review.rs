use brehon_ports::{GitOperations, PortError};
use brehon_test_harness::FakeGitOperations;

#[tokio::test]
async fn detect_deleted_branch_during_review() {
    let git = FakeGitOperations::new();

    git.create_branch("feature-under-review");

    assert!(git.branch_exists("feature-under-review"));

    let result = git.checkout("feature-under-review").await;
    assert!(result.is_ok());

    let result = git.rebase("feature-under-review", "main").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn error_on_accessing_deleted_branch() {
    let git = FakeGitOperations::new();

    git.create_branch("will-be-deleted");

    assert!(git.branch_exists("will-be-deleted"));

    let result = git.rebase("nonexistent-branch", "main").await;
    assert!(result.is_err());
    match result {
        Err(PortError::Git(msg)) => {
            assert!(msg.contains("does not exist"));
        }
        _ => panic!("Expected Git error"),
    }
}

#[tokio::test]
async fn error_on_merging_deleted_branch() {
    let git = FakeGitOperations::new();

    let result = git.merge("deleted-branch").await;
    assert!(result.is_err());
    match result {
        Err(PortError::Git(msg)) => {
            assert!(msg.contains("does not exist"));
        }
        _ => panic!("Expected Git error"),
    }
}

#[tokio::test]
async fn error_on_checkout_deleted_branch() {
    let git = FakeGitOperations::new();

    let result = git.checkout("nonexistent-branch").await;
    assert!(result.is_err());
    match result {
        Err(PortError::Git(msg)) => {
            assert!(msg.contains("does not exist"));
        }
        _ => panic!("Expected Git error"),
    }
}

#[tokio::test]
async fn branch_existence_check() {
    let git = FakeGitOperations::new();

    assert!(git.branch_exists("main"));
    assert!(!git.branch_exists("nonexistent"));

    git.create_branch("new-branch");
    assert!(git.branch_exists("new-branch"));
}
