use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::TempDir;

use super::*;

#[test]
fn paths_equal_for_import_check_resolves_relative_against_project_root() {
    let dir = TempDir::new().unwrap();
    let project_root = dir.path();
    fs::write(project_root.join("plan.json"), "{}").unwrap();

    assert!(paths_equal_for_import_check(
        project_root,
        Path::new("plan.json"),
        Path::new("plan.json")
    ));
    assert!(paths_equal_for_import_check(
        project_root,
        Path::new("./plan.json"),
        Path::new("plan.json")
    ));
    assert!(paths_equal_for_import_check(
        project_root,
        Path::new("plan.json"),
        &project_root.join("plan.json")
    ));
    assert!(!paths_equal_for_import_check(
        project_root,
        Path::new("plan.json"),
        Path::new("other.json")
    ));
}

#[test]
fn paths_equal_for_import_check_falls_back_when_file_removed() {
    let dir = TempDir::new().unwrap();
    let project_root = dir.path();
    assert!(paths_equal_for_import_check(
        project_root,
        Path::new("plan.json"),
        Path::new("plan.json")
    ));
    assert!(!paths_equal_for_import_check(
        project_root,
        Path::new("plan.json"),
        Path::new("other.json")
    ));
}

#[test]
fn paths_equal_for_import_check_falls_back_with_redundant_components() {
    let dir = TempDir::new().unwrap();
    let project_root = dir.path();
    assert!(paths_equal_for_import_check(
        project_root,
        Path::new("nonexistent/../plan.json"),
        Path::new("plan.json")
    ));
    assert!(!paths_equal_for_import_check(
        project_root,
        Path::new("nonexistent/../plan.json"),
        Path::new("other.json")
    ));
    assert!(paths_equal_for_import_check(
        project_root,
        Path::new("./plan.json"),
        Path::new("plan.json")
    ));
}

#[test]
fn paths_equal_for_import_check_mixed_canonicalize_fallback() {
    let dir = TempDir::new().unwrap();
    let project_root = dir.path().canonicalize().unwrap();
    fs::write(project_root.join("plan.json"), "{}").unwrap();
    assert!(paths_equal_for_import_check(
        &project_root,
        Path::new("plan.json"),
        Path::new("nonexistent/../plan.json")
    ));
    assert!(!paths_equal_for_import_check(
        &project_root,
        Path::new("plan.json"),
        Path::new("nonexistent/../other.json")
    ));
}

#[test]
fn find_prior_import_of_source_file_missing_dir_returns_none() {
    let dir = TempDir::new().unwrap();
    let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
    assert!(result.is_none());
}

#[test]
fn find_prior_import_of_source_file_empty_dir_returns_none() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".brehon").join("runtime").join("tasks")).unwrap();
    let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
    assert!(result.is_none());
}

#[test]
fn find_prior_import_of_source_file_skips_malformed_json() {
    let dir = TempDir::new().unwrap();
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(tasks_dir.join("T-bad.json"), "not json").unwrap();
    fs::write(
        tasks_dir.join("T-good.json"),
        r#"{"task_id":"T-good","plan_import":{"source_file":"plan.json"}}"#,
    )
    .unwrap();
    let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
    assert_eq!(
        result,
        Some(("T-good".to_string(), "plan.json".to_string()))
    );
}

#[test]
fn find_prior_import_of_source_file_skips_task_without_plan_import() {
    let dir = TempDir::new().unwrap();
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("T-no-import.json"),
        r#"{"task_id":"T-no-import"}"#,
    )
    .unwrap();
    let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn git_commit_is_ancestor_fails_for_invalid_ref() {
    let dir = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    fs::write(dir.path().join("a"), "a\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let err = git_commit_is_ancestor(dir.path(), "deadbeef")
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("deadbeef"),
        "expected deadbeef error, got: {message}"
    );
}

#[tokio::test]
async fn git_commit_is_ancestor_returns_false_for_real_non_ancestor() {
    let dir = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    fs::write(dir.path().join("a"), "a\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "first"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path())
        .output()
        .unwrap();
    fs::write(dir.path().join("b"), "b\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "b"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "second"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let second_commit = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    std::process::Command::new("git")
        .args(["checkout", "HEAD~1"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!git_commit_is_ancestor(dir.path(), &second_commit)
        .await
        .unwrap());
}

#[tokio::test]
async fn git_commit_is_ancestor_returns_true_for_actual_ancestor() {
    let dir = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    fs::write(dir.path().join("a"), "a\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "first"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path())
        .output()
        .unwrap();
    fs::write(dir.path().join("b"), "b\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "b"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "second"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(git_commit_is_ancestor(dir.path(), "HEAD~1").await.unwrap());
}

#[test]
fn find_prior_import_fail_closed_when_legacy_basename_matches() {
    let dir = TempDir::new().unwrap();
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("T-legacy.json"),
        r#"{"task_id":"T-legacy","plan_import":{"source_file":"../plan.json"}}"#,
    )
    .unwrap();
    let source_path = dir.path().join("plan.json");
    let result = find_prior_import_of_source_file(dir.path(), &source_path).unwrap();
    assert!(
        result.is_some(),
        "legacy path ../plan.json with matching basename should be treated as duplicate"
    );
}

#[test]
fn find_prior_import_fail_closed_when_external_legacy_basename_matches() {
    let dir = TempDir::new().unwrap();
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("T-external.json"),
        r#"{"task_id":"T-external","plan_import":{"source_file":"../outside/plan.json"}}"#,
    )
    .unwrap();
    let source_path = dir.path().join("outside").join("plan.json");
    let result = find_prior_import_of_source_file(dir.path(), &source_path).unwrap();
    assert!(
        result.is_some(),
        "legacy path ../outside/plan.json with matching basename should be treated as duplicate"
    );
}

#[test]
fn find_prior_import_skips_legacy_relative_source_file_when_basename_differs() {
    let dir = TempDir::new().unwrap();
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("T-legacy.json"),
        r#"{"task_id":"T-legacy","plan_import":{"source_file":"../plan-a.json"}}"#,
    )
    .unwrap();
    let source_path = dir.path().join("plan-b.json");
    let result = find_prior_import_of_source_file(dir.path(), &source_path).unwrap();
    assert!(
        result.is_none(),
        "legacy path ../plan-a.json with differing basename should be skipped"
    );
}

#[test]
fn find_prior_import_warns_on_unreadable_task_file() {
    let dir = TempDir::new().unwrap();
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("T-good.json"),
        r#"{"task_id":"T-good","plan_import":{"source_file":"plan.json"}}"#,
    )
    .unwrap();
    fs::write(
        tasks_dir.join("T-unreadable.json"),
        r#"{"task_id":"T-unreadable","plan_import":{"source_file":"plan.json"}}"#,
    )
    .unwrap();
    let mut perms = fs::metadata(tasks_dir.join("T-unreadable.json"))
        .unwrap()
        .permissions();
    perms.set_mode(0o000);
    fs::set_permissions(tasks_dir.join("T-unreadable.json"), perms).unwrap();

    let result = find_prior_import_of_source_file(dir.path(), Path::new("plan.json")).unwrap();
    assert_eq!(
        result,
        Some(("T-good".to_string(), "plan.json".to_string()))
    );

    let mut perms = fs::metadata(tasks_dir.join("T-unreadable.json"))
        .unwrap()
        .permissions();
    perms.set_mode(0o644);
    fs::set_permissions(tasks_dir.join("T-unreadable.json"), perms).unwrap();
}
