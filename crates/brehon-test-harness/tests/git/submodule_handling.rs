use brehon_ports::{GitOperations, PortError};
use brehon_test_harness::FakeGitOperations;
use std::collections::HashMap;
use std::path::Path;

#[tokio::test]
async fn worktree_includes_submodule_references() {
    let git = FakeGitOperations::new();

    git.create_branch("feature-with-submodule");

    let mut files = HashMap::new();
    files.insert(
        ".gitmodules".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "[submodule \"vendor/lib\"]\npath = vendor/lib\nurl = https://github.com/example/lib.git".to_string(),
            lines_added: 3,
            lines_removed: 0,
        },
    );
    files.insert(
        "vendor/lib".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "submodule reference".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-with-submodule", files);

    let path = Path::new("/tmp/worktree-with-submodule");
    let result = git.create_worktree("feature-with-submodule", path).await;
    assert!(result.is_ok());

    let result = git.remove_worktree(path).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn submodule_path_detection() {
    let git = FakeGitOperations::new();
    git.create_branch("has-submodules");

    let mut files = HashMap::new();
    files.insert(
        ".gitmodules".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "[submodule \"dep1\"]\npath = external/dep1\nurl = https://example.com/dep1"
                .to_string(),
            lines_added: 2,
            lines_removed: 0,
        },
    );
    files.insert(
        "external/dep1".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "".to_string(),
            lines_added: 0,
            lines_removed: 0,
        },
    );
    git.set_branch_files("has-submodules", files);

    let diff = git.diff("has-submodules", "main").await.unwrap();
    assert!(diff.files.iter().any(|f| f.path == ".gitmodules"));
}

#[tokio::test]
async fn multiple_submodules() {
    let git = FakeGitOperations::new();
    git.create_branch("multi-submodules");

    let mut files = HashMap::new();
    files.insert(
        ".gitmodules".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "[submodule \"lib-a\"]\npath = vendor/a\nurl = https://example.com/a\n[submodule \"lib-b\"]\npath = vendor/b\nurl = https://example.com/b".to_string(),
            lines_added: 5,
            lines_removed: 0,
        },
    );
    files.insert(
        "vendor/a".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "submodule a".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    files.insert(
        "vendor/b".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "submodule b".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("multi-submodules", files);

    let diff = git.diff("multi-submodules", "main").await.unwrap();
    assert_eq!(diff.files.len(), 3);
}

#[tokio::test]
async fn worktree_creation_validates_branch() {
    let git = FakeGitOperations::new();

    let result = git
        .create_worktree("nonexistent-branch", Path::new("/tmp/any"))
        .await;
    assert!(result.is_err());
    match result {
        Err(PortError::Git(msg)) => {
            assert!(msg.contains("does not exist"));
        }
        _ => panic!("Expected Git error for nonexistent branch"),
    }
}

#[tokio::test]
async fn submodule_conflict_detection() {
    let git = FakeGitOperations::new();
    git.create_branch("submodule-change");

    let mut files = HashMap::new();
    files.insert(
        ".gitmodules".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "[submodule \"updated\"]\npath = lib\nurl = https://new-url.com/lib"
                .to_string(),
            lines_added: 2,
            lines_removed: 1,
        },
    );
    git.set_branch_files("submodule-change", files);

    git.add_conflict_files("submodule-change", vec![".gitmodules".to_string()]);

    let result = git.merge("submodule-change").await.unwrap();
    match result {
        brehon_ports::MergeResult::Conflict { files } => {
            assert!(files.contains(&".gitmodules".to_string()));
        }
        brehon_ports::MergeResult::Success => {}
    }
}

#[tokio::test]
async fn clean_worktree_removal() {
    let git = FakeGitOperations::new();
    git.create_branch("test-branch");

    let path = Path::new("/tmp/clean-worktree");
    git.create_worktree("test-branch", path).await.unwrap();

    let result = git.remove_worktree(path).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn branch_switching_preserves_submodules() {
    let git = FakeGitOperations::new();

    git.create_branch("with-submodule");

    let mut files = HashMap::new();
    files.insert(
        ".gitmodules".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "[submodule \"shared\"]".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("with-submodule", files);

    git.checkout("with-submodule").await.unwrap();

    let current = git.current_branch().await.unwrap();
    assert_eq!(current, "with-submodule");
}
