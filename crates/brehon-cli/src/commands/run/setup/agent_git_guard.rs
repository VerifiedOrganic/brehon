use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Directory placed at the front of launched agent PATHs. It contains a `git`
/// shim that denies any Git invocation whose resolved repository is the shared
/// project root.
const AGENT_GIT_GUARD_BIN_RELATIVE: &str = ".brehon/runtime/git-guard/bin";

pub(crate) fn ensure_agent_git_worktree_guard(cwd: &Path) -> Result<PathBuf> {
    let guard_bin = cwd.join(AGENT_GIT_GUARD_BIN_RELATIVE);
    std::fs::create_dir_all(&guard_bin).with_context(|| {
        format!(
            "Failed to create agent git guard directory '{}'",
            guard_bin.display()
        )
    })?;

    let real_git = resolve_real_git_excluding(&guard_bin)?;
    let project_root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let script_path = guard_bin.join("git");
    std::fs::write(
        &script_path,
        agent_git_worktree_guard_script(&real_git, &project_root),
    )
    .with_context(|| {
        format!(
            "Failed to write agent git guard script '{}'",
            script_path.display()
        )
    })?;

    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&script_path)
            .with_context(|| {
                format!(
                    "Failed to read agent git guard script metadata '{}'",
                    script_path.display()
                )
            })?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script_path, permissions).with_context(|| {
            format!(
                "Failed to mark agent git guard script executable '{}'",
                script_path.display()
            )
        })?;
    }

    tracing::info!(
        guard_bin = %guard_bin.display(),
        real_git = %real_git.display(),
        project_root = %project_root.display(),
        "Installed agent git shared-root guard"
    );
    Ok(guard_bin)
}

fn resolve_real_git_excluding(guard_bin: &Path) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if paths_match(&dir, guard_bin) {
            continue;
        }
        let candidate = dir.join("git");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    let candidate = which::which("git").context("Failed to locate git on PATH")?;
    if paths_match(
        candidate.parent().unwrap_or_else(|| Path::new("")),
        guard_bin,
    ) {
        anyhow::bail!(
            "Resolved git to the Brehon guard itself at '{}'; cannot install recursive git guard",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn paths_match(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn agent_git_worktree_guard_script(real_git: &Path, project_root: &Path) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

real_git={real_git}
project_root={project_root}
effective_cwd="${{PWD:-.}}"
args=("$@")
idx=0

while (( idx < ${{#args[@]}} )); do
  arg="${{args[$idx]}}"
  case "$arg" in
    -C)
      next=$((idx + 1))
      if (( next >= ${{#args[@]}} )); then
        break
      fi
      value="${{args[$next]}}"
      if [[ "$value" = /* ]]; then
        effective_cwd="$value"
      else
        effective_cwd="$effective_cwd/$value"
      fi
      idx=$((idx + 2))
      ;;
    -C?*)
      value="${{arg#-C}}"
      if [[ "$value" = /* ]]; then
        effective_cwd="$value"
      else
        effective_cwd="$effective_cwd/$value"
      fi
      idx=$((idx + 1))
      ;;
    -c|--git-dir|--work-tree|--namespace|--config-env)
      idx=$((idx + 2))
      ;;
    --git-dir=*|--work-tree=*|--namespace=*|--config-env=*)
      idx=$((idx + 1))
      ;;
    --help|--version|-v)
      exec "$real_git" "${{args[@]}}"
      ;;
    --)
      break
      ;;
    -*)
      idx=$((idx + 1))
      ;;
    *)
      break
      ;;
  esac
done
subcommand="${{args[$idx]:-}}"

repo_root="$("$real_git" -C "$effective_cwd" rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -n "$repo_root" ]]; then
  repo_root="$(cd "$repo_root" 2>/dev/null && pwd -P || printf '%s\n' "$repo_root")"
fi

brehon_git_guard_bypass_valid=0
brehon_bypass_token="${{BREHON_PROTECTED_BRANCH_BYPASS_TOKEN:-}}"
brehon_bypass_dir="${{BREHON_PROTECTED_BRANCH_BYPASS_DIR:-}}"
case "$brehon_bypass_token" in *[!A-Za-z0-9_.-]*|"") brehon_bypass_token="" ;; esac
if [[ "${{BREHON_ALLOW_PROTECTED_BRANCH_COMMIT:-}}" == "1" && -n "$brehon_bypass_token" && -n "$brehon_bypass_dir" ]]; then
  brehon_bypass_dir="${{brehon_bypass_dir%/}}"
  brehon_bypass_path="$brehon_bypass_dir/$brehon_bypass_token"
  if [[ -f "$brehon_bypass_path" ]]; then
    brehon_bypass_pid=""
    while IFS= read -r brehon_bypass_line; do
      case "$brehon_bypass_line" in
        pid=*)
          brehon_bypass_pid="${{brehon_bypass_line#pid=}}"
          break
          ;;
      esac
    done < "$brehon_bypass_path" || true
    case "$brehon_bypass_pid" in
      ""|*[!0-9]*) ;;
      *) kill -0 "$brehon_bypass_pid" 2>/dev/null && brehon_git_guard_bypass_valid=1 ;;
    esac
  fi
fi

if [[ "$repo_root" == "$project_root" && -n "$subcommand" ]]; then
  case "$subcommand" in
    add|am|apply|checkout|cherry-pick|clean|commit|merge|mv|pull|rebase|reset|restore|revert|rm|stash|switch|update-index)
      if [[ "$brehon_git_guard_bypass_valid" != "1" ]]; then
        echo "Brehon git guard denied mutating git command '$subcommand' inside protected shared repo root: $project_root" >&2
        echo "Use the assigned isolated worktree for mutating Git operations during this run." >&2
        exit 2
      fi
      ;;
  esac
fi

exec "$real_git" "${{args[@]}}"
"#,
        real_git = shell_quote(real_git),
        project_root = shell_quote(project_root),
    )
}

#[cfg(test)]
mod tests {
    use super::super::{git_common_dir, BREHON_PROTECTED_BRANCH_BYPASS_DIR};
    use super::*;

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
    fn test_agent_git_worktree_guard_allows_shared_root_reads_and_denies_mutations() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        let worktree_path = temp.path().join("agent-worktree");
        let worktree_arg = worktree_path.to_string_lossy().into_owned();
        run_git(
            temp.path(),
            &["worktree", "add", "-b", "agent/test", &worktree_arg],
        );

        let guard_bin = ensure_agent_git_worktree_guard(temp.path()).unwrap();
        let guarded_path = std::env::join_paths(std::iter::once(guard_bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();

        let allowed_root_read = std::process::Command::new("git")
            .env("PATH", &guarded_path)
            .current_dir(temp.path())
            .arg("status")
            .output()
            .unwrap();

        assert!(
            allowed_root_read.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&allowed_root_read.stdout),
            String::from_utf8_lossy(&allowed_root_read.stderr)
        );

        let allowed_root_worktree_list = std::process::Command::new("git")
            .env("PATH", &guarded_path)
            .current_dir(&worktree_path)
            .args([
                "-C",
                temp.path().to_string_lossy().as_ref(),
                "worktree",
                "list",
            ])
            .output()
            .unwrap();

        assert!(
            allowed_root_worktree_list.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&allowed_root_worktree_list.stdout),
            String::from_utf8_lossy(&allowed_root_worktree_list.stderr)
        );

        let denied = std::process::Command::new("git")
            .env("PATH", &guarded_path)
            .current_dir(temp.path())
            .args(["checkout", "HEAD", "--", "README.md"])
            .output()
            .unwrap();

        assert_eq!(denied.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&denied.stderr)
                .contains("Brehon git guard denied mutating git command 'checkout'"),
            "stderr: {}",
            String::from_utf8_lossy(&denied.stderr)
        );

        let allowed_root_read_with_c = std::process::Command::new("git")
            .env("PATH", &guarded_path)
            .current_dir(&worktree_path)
            .args(["-C", temp.path().to_string_lossy().as_ref(), "status"])
            .output()
            .unwrap();

        assert!(
            allowed_root_read_with_c.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&allowed_root_read_with_c.stdout),
            String::from_utf8_lossy(&allowed_root_read_with_c.stderr)
        );

        let denied_with_c = std::process::Command::new("git")
            .env("PATH", &guarded_path)
            .current_dir(&worktree_path)
            .args([
                "-C",
                temp.path().to_string_lossy().as_ref(),
                "checkout",
                "HEAD",
                "--",
                "README.md",
            ])
            .output()
            .unwrap();

        assert_eq!(denied_with_c.status.code(), Some(2));

        let allowed = std::process::Command::new("git")
            .env("PATH", &guarded_path)
            .current_dir(&worktree_path)
            .args(["status", "--short"])
            .output()
            .unwrap();

        assert!(
            allowed.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&allowed.stdout),
            String::from_utf8_lossy(&allowed.stderr)
        );
    }

    #[test]
    fn test_agent_git_worktree_guard_allows_leased_protected_branch_mutation() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        let guard_bin = ensure_agent_git_worktree_guard(temp.path()).unwrap();
        let guarded_path = std::env::join_paths(std::iter::once(guard_bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();

        let env_only = std::process::Command::new("git")
            .env("PATH", &guarded_path)
            .env("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT", "1")
            .env("BREHON_PROTECTED_BRANCH_BYPASS_TOKEN", "missing-lease")
            .env("BREHON_PROTECTED_BRANCH_BYPASS_DIR", temp.path())
            .current_dir(temp.path())
            .args(["checkout", "HEAD", "--", "README.md"])
            .output()
            .unwrap();
        assert_eq!(env_only.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&env_only.stderr)
                .contains("Brehon git guard denied mutating git command 'checkout'"),
            "stderr: {}",
            String::from_utf8_lossy(&env_only.stderr)
        );

        let bypass_token = format!("guard-test-{}", std::process::id());
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

        let leased = std::process::Command::new("git")
            .env("PATH", &guarded_path)
            .env("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT", "1")
            .env("BREHON_PROTECTED_BRANCH_BYPASS_TOKEN", &bypass_token)
            .env("BREHON_PROTECTED_BRANCH_BYPASS_DIR", &bypass_dir)
            .current_dir(temp.path())
            .args(["checkout", "HEAD", "--", "README.md"])
            .output()
            .unwrap();
        assert!(
            leased.status.success(),
            "leased guard bypass should allow Brehon-controlled mutation\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&leased.stdout),
            String::from_utf8_lossy(&leased.stderr)
        );
    }
}
