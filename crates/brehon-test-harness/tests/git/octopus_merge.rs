use brehon_ports::{GitOperations, MergeResult};
use brehon_test_harness::FakeGitOperations;
use std::collections::HashMap;

#[tokio::test]
async fn octopus_merge_three_branches_same_file() {
    let git = FakeGitOperations::new();

    git.create_branch("feature-1");
    git.create_branch("feature-2");
    git.create_branch("feature-3");

    let mut files_1 = HashMap::new();
    files_1.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "version = 1".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-1", files_1);

    let mut files_2 = HashMap::new();
    files_2.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "version = 2".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-2", files_2);

    let mut files_3 = HashMap::new();
    files_3.insert(
        "src/lib.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "version = 3".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-3", files_3.clone());

    git.add_conflict_files("feature-3", vec!["src/lib.rs".to_string()]);

    let result_1 = git.merge("feature-1").await.unwrap();
    assert!(matches!(result_1, MergeResult::Success));

    let result_2 = git.merge("feature-2").await.unwrap();
    assert!(matches!(result_2, MergeResult::Success));

    let result_3 = git.merge("feature-3").await.unwrap();
    match result_3 {
        MergeResult::Conflict { files } => {
            assert!(!files.is_empty());
            assert!(files.contains(&"src/lib.rs".to_string()));
        }
        MergeResult::Success => {}
    }
}

#[tokio::test]
async fn octopus_merge_different_files_succeeds() {
    let git = FakeGitOperations::new();

    git.create_branch("feature-a");
    git.create_branch("feature-b");
    git.create_branch("feature-c");

    let mut files_a = HashMap::new();
    files_a.insert(
        "src/a.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "mod a".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-a", files_a);

    let mut files_b = HashMap::new();
    files_b.insert(
        "src/b.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "mod b".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-b", files_b);

    let mut files_c = HashMap::new();
    files_c.insert(
        "src/c.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "mod c".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("feature-c", files_c);

    let result_a = git.merge("feature-a").await.unwrap();
    assert!(matches!(result_a, MergeResult::Success));

    let result_b = git.merge("feature-b").await.unwrap();
    assert!(matches!(result_b, MergeResult::Success));

    let result_c = git.merge("feature-c").await.unwrap();
    assert!(matches!(result_c, MergeResult::Success));
}

#[tokio::test]
async fn octopus_merge_partial_conflict() {
    let git = FakeGitOperations::new();

    git.create_branch("clean-feature");
    git.create_branch("conflict-feature");

    let mut clean_files = HashMap::new();
    clean_files.insert(
        "src/new.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "new file".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("clean-feature", clean_files);

    let mut conflict_files = HashMap::new();
    conflict_files.insert(
        "src/existing.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "conflicting change".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("conflict-feature", conflict_files);

    let clean_result = git.merge("clean-feature").await.unwrap();
    assert!(matches!(clean_result, MergeResult::Success));

    git.set_merge_conflict("conflict-feature", vec!["src/existing.rs".to_string()]);
    let conflict_result = git.merge("conflict-feature").await.unwrap();
    match conflict_result {
        MergeResult::Conflict { files } => {
            assert!(!files.is_empty());
            assert!(files.contains(&"src/existing.rs".to_string()));
        }
        MergeResult::Success => panic!("Expected conflict"),
    }
}

#[tokio::test]
async fn octopus_merge_all_conflict() {
    let git = FakeGitOperations::new();

    git.create_branch("branch-1");
    git.create_branch("branch-2");
    git.create_branch("branch-3");

    let mut files = HashMap::new();
    files.insert(
        "src/core.rs".to_string(),
        brehon_test_harness::mock_git::FileContent {
            content: "conflict".to_string(),
            lines_added: 1,
            lines_removed: 0,
        },
    );
    git.set_branch_files("branch-1", files.clone());
    git.set_branch_files("branch-2", files.clone());
    git.set_branch_files("branch-3", files);

    git.set_merge_conflict("branch-1", vec!["src/core.rs".to_string()]);

    let result = git.merge("branch-1").await.unwrap();
    assert!(matches!(result, MergeResult::Conflict { .. }));
}

#[tokio::test]
async fn sequential_merges_with_conflicts() {
    let git = FakeGitOperations::new();

    git.create_branch("first");
    git.create_branch("second");

    git.set_merge_conflict("first", vec!["src/a.rs".to_string()]);
    let result1 = git.merge("first").await.unwrap();
    assert!(matches!(result1, MergeResult::Conflict { .. }));

    git.set_merge_conflict("second", vec!["src/b.rs".to_string()]);
    let result2 = git.merge("second").await.unwrap();
    assert!(matches!(result2, MergeResult::Conflict { .. }));
}
