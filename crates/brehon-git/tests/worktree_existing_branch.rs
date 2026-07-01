use std::path::{Path, PathBuf};

use brehon_git::{BranchOps, WorktreeOps};
use git2::Repository;
use tempfile::TempDir;

fn setup_test_repo() -> (TempDir, Repository) {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let repo = Repository::init(temp_dir.path()).expect("failed to init repo");

    std::fs::write(temp_dir.path().join(".gitignore"), "target/\n.brehon/\n")
        .expect("failed to write gitignore");
    let sig = git2::Signature::now("Test", "test@example.com").expect("failed to create sig");
    let mut index = repo.index().expect("failed to get index");
    index
        .add_path(Path::new(".gitignore"))
        .expect("failed to add gitignore");
    index.write().expect("failed to write index");
    let oid = index.write_tree().expect("failed to write tree");
    let tree = repo.find_tree(oid).expect("failed to find tree");
    let commit = repo
        .commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
        .expect("failed to create initial commit");
    repo.reference("refs/heads/main", commit, true, "create main branch")
        .expect("failed to create main ref");
    repo.set_head("refs/heads/main")
        .expect("failed to set main head");
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .expect("failed to checkout main");
    drop(tree);

    (temp_dir, repo)
}

fn test_worktree_path(temp_dir: &TempDir, name: &str) -> PathBuf {
    let root = temp_dir.path().join(".brehon").join("worktrees");
    std::fs::create_dir_all(&root).expect("failed to create worktree root");
    root.join(name)
}

fn commit_paths(repo: &Repository, paths: &[&str], message: &str) {
    let sig = git2::Signature::now("Test", "test@example.com").expect("failed to create sig");
    let mut index = repo.index().expect("failed to get index");
    for path in paths {
        index.add_path(Path::new(path)).expect("failed to add path");
    }
    index.write().expect("failed to write index");
    let oid = index.write_tree().expect("failed to write tree");
    let tree = repo.find_tree(oid).expect("failed to find tree");
    let parent = repo
        .head()
        .expect("failed to read head")
        .peel_to_commit()
        .expect("failed to peel head");
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
        .expect("failed to create commit");
}

#[test]
fn create_worktree_checks_out_existing_branch_when_head_is_elsewhere() {
    let (temp_dir, repo) = setup_test_repo();
    let branch_name = "epic/test-feature";
    let branch_ops = BranchOps::new(&repo);
    branch_ops
        .create_branch(branch_name, Some("main"))
        .expect("failed to create branch");

    repo.set_head(&format!("refs/heads/{branch_name}"))
        .expect("failed to switch to test branch");
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .expect("failed to check out test branch");
    std::fs::create_dir_all(temp_dir.path().join("src")).expect("failed to create src");
    std::fs::write(temp_dir.path().join("src/conflict.txt"), "epic branch\n")
        .expect("failed to write branch file");
    commit_paths(&repo, &["src/conflict.txt"], "branch change");

    repo.set_head("refs/heads/main")
        .expect("failed to return to main");
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .expect("failed to check out main");

    let ops = WorktreeOps::new(&repo);
    let worktree_path = test_worktree_path(&temp_dir, "existing-branch-from-main");
    let result = ops.create_worktree(branch_name, &worktree_path);

    assert!(result.is_ok(), "should reuse existing branch: {result:?}");
    let worktree_repo = Repository::open(&worktree_path).expect("open worktree");
    let head = worktree_repo
        .head()
        .expect("worktree head")
        .shorthand()
        .map(str::to_string);
    assert_eq!(head.as_deref(), Some(branch_name));
    assert_eq!(
        std::fs::read_to_string(worktree_path.join("src/conflict.txt")).expect("read branch file"),
        "epic branch\n"
    );
}
