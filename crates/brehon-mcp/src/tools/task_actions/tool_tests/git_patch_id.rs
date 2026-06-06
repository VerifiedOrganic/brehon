use super::*;

#[test]
fn test_git_patch_id_handles_root_commit_without_commit_header() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let workspace = tempfile::tempdir().unwrap();

    run_git(workspace.path(), &["init", "-b", "main"]);
    run_git(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    run_git(workspace.path(), &["config", "user.name", "Test User"]);
    std::fs::write(workspace.path().join("root.txt"), "root delta\n").unwrap();
    run_git(workspace.path(), &["add", "root.txt"]);
    run_git(workspace.path(), &["commit", "-m", "root delta"]);
    let root_commit = run_git(workspace.path(), &["rev-parse", "HEAD"]);

    assert!(git_patch_id(workspace.path(), &root_commit)
        .unwrap()
        .is_some());
}
