use super::*;
use serde_json::json;
use std::ffi::OsString;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn ctx_with(worktree: &str, merge_target: Option<&str>) -> PolicyContext {
    PolicyContext {
        worktree_root: Some(PathBuf::from(worktree)),
        current_dir: Some(PathBuf::from(worktree)),
        project_root: None,
        agent_role: None,
        brehon_root: None,
        worktree_root_base: None,
        merge_target: merge_target.map(str::to_string),
    }
}

fn ctx_with_project(worktree: &str, project_root: &str) -> PolicyContext {
    PolicyContext {
        worktree_root: Some(PathBuf::from(worktree)),
        current_dir: Some(PathBuf::from(worktree)),
        project_root: Some(PathBuf::from(project_root)),
        agent_role: None,
        brehon_root: Some(PathBuf::from(format!("{project_root}/.brehon"))),
        worktree_root_base: None,
        merge_target: None,
    }
}

fn supervisor_ctx(worktree: &str, brehon_root: &str) -> PolicyContext {
    PolicyContext {
        worktree_root: Some(PathBuf::from(worktree)),
        current_dir: Some(PathBuf::from(worktree)),
        project_root: Path::new(brehon_root).parent().map(Path::to_path_buf),
        agent_role: Some("supervisor".to_string()),
        brehon_root: Some(PathBuf::from(brehon_root)),
        worktree_root_base: None,
        merge_target: None,
    }
}

fn bash(cmd: &str) -> Value {
    json!({ "command": cmd })
}

#[test]
fn marker_present_uses_brehon_root_env_for_external_worktrees() {
    let _guard = ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let brehon_root = temp.path().join(".brehon");
    let marker = brehon_root.join("runtime").join("claude-hook-active");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "active\n").unwrap();
    let _env = EnvVarGuard::set("BREHON_ROOT", &brehon_root);

    assert!(marker_present());
}

#[test]
fn blocks_git_checkout_main() {
    let decision = evaluate("Bash", &bash("git checkout main"), &ctx_with("/work", None));
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_git_switch_master() {
    let decision = evaluate("Bash", &bash("git switch master"), &ctx_with("/work", None));
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_git_reset_hard_main() {
    let decision = evaluate(
        "Bash",
        &bash("git reset --hard origin/main"),
        &ctx_with("/work", None),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_restore_source_main() {
    let decision = evaluate(
        "Bash",
        &bash("git restore --source=main src/foo.rs"),
        &ctx_with("/work", None),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_merge_target_when_set() {
    let decision = evaluate(
        "Bash",
        &bash("git checkout epic/auth"),
        &ctx_with("/work", Some("epic/auth")),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn allows_checkout_worker_branch() {
    let decision = evaluate(
        "Bash",
        &bash("git checkout brehon/worker-1"),
        &ctx_with("/work", None),
    );
    assert_eq!(decision, Decision::Allow);
}

#[test]
fn blocks_smuggled_protected_checkout_after_and() {
    // Combined commands are split before the policy runs.
    let decision = evaluate(
        "Bash",
        &bash("ls && git checkout main"),
        &ctx_with("/work", None),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_cd_to_parent_outside_worktree() {
    let decision = evaluate("Bash", &bash("cd .."), &ctx_with("/work/sub", None));
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_bare_cd() {
    let decision = evaluate("Bash", &bash("cd"), &ctx_with("/work", None));
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn allows_cd_inside_worktree() {
    let decision = evaluate("Bash", &bash("cd src/foo"), &ctx_with("/work", None));
    assert_eq!(decision, Decision::Allow);
}

#[test]
fn supervisor_can_cd_to_integration_worktree() {
    let ctx = supervisor_ctx(
        "/repo/.brehon/worktrees/runs/session/supervisor/claude-supervisor",
        "/repo/.brehon",
    );
    let decision = evaluate("Bash", &bash("cd /repo/.brehon/worktrees/epic/T-123"), &ctx);
    assert_eq!(decision, Decision::Allow);

    let decision = evaluate(
        "Bash",
        &bash("cd /repo/.brehon/worktrees/initiative/T-init"),
        &ctx,
    );
    assert_eq!(decision, Decision::Allow);
}

#[test]
fn supervisor_can_cd_to_external_integration_worktree() {
    let ctx = PolicyContext {
        worktree_root: Some(PathBuf::from(
            "/external/brehon/worktrees/repo-123/runs/session/supervisor/claude-supervisor",
        )),
        current_dir: Some(PathBuf::from(
            "/external/brehon/worktrees/repo-123/runs/session/supervisor/claude-supervisor",
        )),
        project_root: Some(PathBuf::from("/repo")),
        agent_role: Some("supervisor".to_string()),
        brehon_root: Some(PathBuf::from("/repo/.brehon")),
        worktree_root_base: Some(PathBuf::from("/external/brehon/worktrees/repo-123")),
        merge_target: None,
    };

    let decision = evaluate(
        "Bash",
        &bash("cd /external/brehon/worktrees/repo-123/initiative/T-init"),
        &ctx,
    );
    assert_eq!(decision, Decision::Allow);

    let decision = evaluate(
        "Bash",
        &bash("cd /external/brehon/worktrees/repo-123/runs/session/worker-1"),
        &ctx,
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn supervisor_cannot_cd_to_worker_worktree() {
    let ctx = supervisor_ctx(
        "/repo/.brehon/worktrees/runs/session/supervisor/claude-supervisor",
        "/repo/.brehon",
    );
    let decision = evaluate(
        "Bash",
        &bash("cd /repo/.brehon/worktrees/runs/session/worker-1"),
        &ctx,
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn worker_cannot_cd_to_integration_worktree() {
    let ctx = PolicyContext {
        worktree_root: Some(PathBuf::from(
            "/repo/.brehon/worktrees/runs/session/worker-1",
        )),
        current_dir: Some(PathBuf::from(
            "/repo/.brehon/worktrees/runs/session/worker-1",
        )),
        project_root: Some(PathBuf::from("/repo")),
        agent_role: Some("worker".to_string()),
        brehon_root: Some(PathBuf::from("/repo/.brehon")),
        worktree_root_base: None,
        merge_target: None,
    };
    let decision = evaluate("Bash", &bash("cd /repo/.brehon/worktrees/epic/T-123"), &ctx);
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_cd_with_shell_variable() {
    let decision = evaluate("Bash", &bash("cd $HOME"), &ctx_with("/work", None));
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_pushd_to_shared_root() {
    let decision = evaluate(
        "Bash",
        &bash("pushd /repo"),
        &ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo"),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_bash_project_root_env_reference() {
    let decision = evaluate(
        "Bash",
        &bash("cd \"$BREHON_PROJECT_ROOT\" && sed -i 's/a/b/' src/lib.rs"),
        &ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo"),
    );
    assert!(matches!(decision, Decision::Block(reason) if reason.contains("BREHON_PROJECT_ROOT")));
}

#[test]
fn blocks_bash_literal_shared_root_reference() {
    let decision = evaluate(
        "Bash",
        &bash("python3 - <<'PY'\nopen('/repo/src/lib.rs', 'w').write('oops')\nPY"),
        &ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo"),
    );
    assert!(matches!(decision, Decision::Block(reason) if reason.contains("shared repo root")));
}

#[test]
fn allows_unrelated_bash() {
    let decision = evaluate(
        "Bash",
        &bash("cargo build --release"),
        &ctx_with("/work", None),
    );
    assert_eq!(decision, Decision::Allow);
}

#[test]
fn blocks_task_tool_to_prevent_unmanaged_claude_worktrees() {
    let decision = evaluate(
        "Task",
        &json!({
            "description": "review implementation",
            "prompt": "Inspect the repository and report findings."
        }),
        &ctx_with("/work", None),
    );

    assert!(
        matches!(decision, Decision::Block(reason) if reason.contains("unmanaged Claude worktrees"))
    );
}

#[test]
fn allows_file_tool_inside_worktree() {
    let decision = evaluate(
        "Edit",
        &json!({ "file_path": "/work/src/lib.rs", "old_string": "a", "new_string": "b" }),
        &ctx_with("/work", None),
    );
    assert_eq!(decision, Decision::Allow);
}

#[test]
fn blocks_file_tool_in_shared_root() {
    let decision = evaluate(
        "Write",
        &json!({ "file_path": "/repo/src/lib.rs", "content": "oops" }),
        &ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo"),
    );
    assert!(matches!(decision, Decision::Block(reason) if reason.contains("shared repo root")));
}

#[test]
fn blocks_multi_edit_in_shared_root() {
    let decision = evaluate(
        "MultiEdit",
        &json!({ "file_path": "/repo/crates/brehon-types/src/config.rs", "edits": [] }),
        &ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo"),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_notebook_edit_outside_worktree() {
    let decision = evaluate(
        "NotebookEdit",
        &json!({ "notebook_path": "/tmp/analysis.ipynb", "new_source": "x" }),
        &ctx_with("/work", None),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_mutating_file_tool_when_worktree_root_missing() {
    let ctx = PolicyContext {
        worktree_root: None,
        current_dir: None,
        project_root: Some(PathBuf::from("/repo")),
        agent_role: Some("worker".to_string()),
        brehon_root: Some(PathBuf::from("/repo/.brehon")),
        worktree_root_base: None,
        merge_target: None,
    };
    let decision = evaluate("Write", &json!({ "file_path": "src/lib.rs" }), &ctx);
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_relative_file_tool_when_hook_cwd_escaped_worktree() {
    let mut ctx = ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo");
    ctx.current_dir = Some(PathBuf::from("/repo"));
    let decision = evaluate(
        "Write",
        &json!({ "file_path": "src/lib.rs", "content": "oops" }),
        &ctx,
    );
    assert!(matches!(decision, Decision::Block(reason) if reason.contains("shared repo root")));
}

#[test]
fn blocks_bash_redirection_to_shared_root() {
    let decision = evaluate(
        "Bash",
        &bash("cat <<EOF > /repo/src/lib.rs"),
        &ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo"),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn allows_bash_redirection_to_dev_null() {
    let decision = evaluate(
        "Bash",
        &bash("cargo test 2>/dev/null"),
        &ctx_with("/work", None),
    );
    assert_eq!(decision, Decision::Allow);
}

#[test]
fn blocks_bash_tee_to_shared_root() {
    let decision = evaluate(
        "Bash",
        &bash("printf hi | tee /repo/src/lib.rs"),
        &ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo"),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_bash_sed_in_place_to_shared_root() {
    let decision = evaluate(
        "Bash",
        &bash("sed -i 's/a/b/' /repo/src/lib.rs"),
        &ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo"),
    );
    assert!(matches!(decision, Decision::Block(_)));
}

#[test]
fn blocks_bash_when_hook_cwd_escaped_worktree() {
    let mut ctx = ctx_with_project("/repo/.brehon/worktrees/runs/session/worker-1", "/repo");
    ctx.current_dir = Some(PathBuf::from("/repo"));
    let decision = evaluate("Bash", &bash("cargo test"), &ctx);
    assert!(
        matches!(decision, Decision::Block(reason) if reason.contains("outside the assigned worktree"))
    );
}

#[test]
fn allows_when_no_worktree_root_set() {
    // Without BREHON_WORKSPACE_ROOT we can't resolve `cd` safely, so we
    // fall back to allow for `cd` calls but still block git branch
    // changes (those don't need the worktree root).
    let ctx = PolicyContext {
        worktree_root: None,
        current_dir: None,
        project_root: None,
        agent_role: None,
        brehon_root: None,
        worktree_root_base: None,
        merge_target: None,
    };
    assert_eq!(evaluate("Bash", &bash("cd .."), &ctx), Decision::Allow);
    assert!(matches!(
        evaluate("Bash", &bash("git checkout main"), &ctx),
        Decision::Block(_)
    ));
}
