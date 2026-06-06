use super::*;
use std::path::Path;

fn isolated_git_command(path: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command.current_dir(path);
    for key in [
        "BREHON_ALLOW_PROTECTED_BRANCH_COMMIT",
        "BREHON_PROTECTED_BRANCH_BYPASS_TOKEN",
        "BREHON_PROTECTED_BRANCH_BYPASS_DIR",
        "BREHON_PROTECTED_BRANCHES",
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn run_git(path: &Path, args: &[&str]) -> String {
    let output = isolated_git_command(path)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_git_repo(path: &Path) {
    run_git(path, &["init", "-b", "main"]);
    run_git(path, &["config", "user.email", "brehon@example.invalid"]);
    run_git(path, &["config", "user.name", "Brehon Test"]);
    std::fs::write(path.join("README.md"), "seed\n").unwrap();
    std::fs::write(
        path.join(".gitignore"),
        ".brehon/\n.claude/settings.local.json\n",
    )
    .unwrap();
    run_git(path, &["add", "README.md", ".gitignore"]);
    run_git(path, &["commit", "-m", "seed"]);
}

#[test]
fn test_protected_branch_hooks_allow_default_branch_without_active_run() {
    let temp = tempfile::tempdir().unwrap();
    init_git_repo(temp.path());
    ensure_protected_branch_hooks(temp.path(), "main").unwrap();

    std::fs::write(temp.path().join("allowed.txt"), "allowed\n").unwrap();
    run_git(temp.path(), &["add", "allowed.txt"]);
    let output = isolated_git_command(temp.path())
        .args(["commit", "-m", "allowed on inactive main"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "commit on inactive main should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_ensure_protected_branch_hooks_blocks_default_branch_commit() {
    let temp = tempfile::tempdir().unwrap();
    init_git_repo(temp.path());
    let initial_main = run_git(temp.path(), &["rev-parse", "main"]);
    ensure_protected_branch_hooks(temp.path(), "main").unwrap();
    let _activation = activate_protected_branch_guard(temp.path(), "test-session").unwrap();

    std::fs::write(temp.path().join("blocked.txt"), "blocked\n").unwrap();
    run_git(temp.path(), &["add", "blocked.txt"]);
    let blocked = isolated_git_command(temp.path())
        .args(["commit", "-m", "blocked on main"])
        .output()
        .unwrap();
    assert!(
        !blocked.status.success(),
        "commit on main unexpectedly succeeded: {}",
        String::from_utf8_lossy(&blocked.stdout)
    );
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        stderr.contains("Brehon protected branch guard"),
        "stderr: {stderr}"
    );

    run_git(temp.path(), &["checkout", "-b", "feature/protected-hook"]);
    run_git(temp.path(), &["commit", "-m", "feature branch allowed"]);
    let blocked_ref_update = isolated_git_command(temp.path())
        .args(["update-ref", "refs/heads/main", "HEAD"])
        .output()
        .unwrap();
    assert!(
        !blocked_ref_update.status.success(),
        "direct main ref update unexpectedly succeeded"
    );
    let ref_stderr = String::from_utf8_lossy(&blocked_ref_update.stderr);
    assert!(
        ref_stderr.contains("protected branch"),
        "stderr: {ref_stderr}"
    );
    assert_eq!(run_git(temp.path(), &["rev-parse", "main"]), initial_main);

    run_git(temp.path(), &["checkout", "main"]);
    std::fs::write(temp.path().join("repair.txt"), "repair\n").unwrap();
    run_git(temp.path(), &["add", "repair.txt"]);
    let env_only_repair = isolated_git_command(temp.path())
        .args(["commit", "-m", "deliberate repair"])
        .env("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT", "1")
        .env("BREHON_PROTECTED_BRANCH_BYPASS_TOKEN", "missing-lease")
        .env("BREHON_PROTECTED_BRANCH_BYPASS_DIR", temp.path())
        .output()
        .unwrap();
    assert!(
        !env_only_repair.status.success(),
        "generic repair env unexpectedly bypassed hook: {}",
        String::from_utf8_lossy(&env_only_repair.stdout)
    );
    let env_only_stderr = String::from_utf8_lossy(&env_only_repair.stderr);
    assert!(
        env_only_stderr.contains("Brehon protected branch guard"),
        "stderr: {env_only_stderr}"
    );

    let bypass_token = format!("test-{}", std::process::id());
    let bypass_dir = git_common_dir(temp.path())
        .unwrap()
        .join("brehon")
        .join(BREHON_PROTECTED_BRANCH_BYPASS_DIR);
    std::fs::create_dir_all(&bypass_dir).unwrap();
    std::fs::write(
        bypass_dir.join(&bypass_token),
        format!("pid={}\n", std::process::id()),
    )
    .unwrap();
    let leased_repair = isolated_git_command(temp.path())
        .args(["commit", "-m", "deliberate repair"])
        .env("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT", "1")
        .env("BREHON_PROTECTED_BRANCH_BYPASS_TOKEN", &bypass_token)
        .output()
        .unwrap();
    assert!(
        leased_repair.status.success(),
        "leased repair bypass should succeed: {}",
        String::from_utf8_lossy(&leased_repair.stderr)
    );
}

#[test]
fn test_protected_branch_guard_activation_drop_disarms_hooks() {
    let temp = tempfile::tempdir().unwrap();
    init_git_repo(temp.path());
    ensure_protected_branch_hooks(temp.path(), "main").unwrap();
    let activation = activate_protected_branch_guard(temp.path(), "test-session").unwrap();
    drop(activation);

    std::fs::write(temp.path().join("after-shutdown.txt"), "allowed\n").unwrap();
    run_git(temp.path(), &["add", "after-shutdown.txt"]);
    let output = isolated_git_command(temp.path())
        .args(["commit", "-m", "allowed after shutdown"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "commit after guard activation drop should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_ensure_protected_branch_hooks_preserves_existing_hook_body() {
    let temp = tempfile::tempdir().unwrap();
    init_git_repo(temp.path());
    let hook_path = temp.path().join(".git").join("hooks").join("pre-commit");
    std::fs::write(
        &hook_path,
        "#!/bin/sh\necho existing hook ran >&2\nexit 42\n",
    )
    .unwrap();

    ensure_protected_branch_hooks(temp.path(), "main").unwrap();
    run_git(temp.path(), &["checkout", "-b", "feature/existing-hook"]);
    std::fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();
    run_git(temp.path(), &["add", "feature.txt"]);

    let output = isolated_git_command(temp.path())
        .args(["commit", "-m", "feature"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("existing hook ran"), "stderr: {stderr}");
    assert!(
        !stderr.contains("Brehon protected branch guard"),
        "feature branch should reach the preserved hook body: {stderr}"
    );
}

#[test]
fn test_remove_protected_branch_hooks_preserves_existing_hook_body() {
    let temp = tempfile::tempdir().unwrap();
    init_git_repo(temp.path());
    let hooks_dir = temp.path().join(".git").join("hooks");
    let hook_path = hooks_dir.join("pre-commit");
    std::fs::write(
        &hook_path,
        "#!/bin/sh\necho existing hook ran >&2\nexit 42\n",
    )
    .unwrap();

    ensure_protected_branch_hooks(temp.path(), "main").unwrap();
    let marker_path = protected_branch_guard_marker_path(temp.path()).unwrap();
    let _activation = activate_protected_branch_guard(temp.path(), "test-session").unwrap();
    assert!(marker_path.exists());

    let removed = remove_protected_branch_hooks(temp.path()).unwrap();
    assert!(removed.iter().any(|path| path.ends_with("pre-commit")));
    assert!(!hooks_dir.join("commit-msg").exists());
    assert!(!marker_path.exists());

    let contents = std::fs::read_to_string(&hook_path).unwrap();
    assert!(!contents.contains(BREHON_PROTECTED_BRANCH_GUARD_BEGIN));
    assert!(contents.contains("existing hook ran"));

    run_git(temp.path(), &["checkout", "-b", "feature/cleaned-hook"]);
    std::fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();
    run_git(temp.path(), &["add", "feature.txt"]);
    let output = isolated_git_command(temp.path())
        .args(["commit", "-m", "feature"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("existing hook ran"), "stderr: {stderr}");
    assert!(!stderr.contains("Brehon protected branch guard"));
}
