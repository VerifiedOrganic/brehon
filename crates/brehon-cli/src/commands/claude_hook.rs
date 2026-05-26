//! `brehon claude-hook` — Claude Code `PreToolUse` hook.
//!
//! Wired up from `.claude/settings.local.json` at worktree setup. Reads the
//! tool-call JSON Claude Code emits on stdin and decides whether to allow or
//! block the call before the model can run it.
//!
//! ## Why this exists
//!
//! The worker startup prompt tells agents "stay in your worktree, never
//! checkout main." Strong instruction-followers (Claude itself, Kimi K2)
//! honor that. Weaker ones (Minimax M2 through the Claude harness, some
//! open-weight models) treat the rule as a suggestion and routinely:
//!
//! - `git checkout main`, edit files, then `git checkout worker-branch` and
//!   call `task action=complete` → empty commit, work stranded on main.
//! - `cd ..` to compare or read, then accidentally write outside the worktree.
//! - `git reset --hard main` to "fix" something, blowing away worker progress.
//!
//! Git pre-commit hooks (installed by `ensure_protected_branch_hooks`) catch
//! the *commit* step, but by then the damage is done. This hook fires before
//! the model can even run the offending command.
//!
//! ## Protocol
//!
//! Claude Code passes JSON on stdin shaped roughly like:
//!
//! ```json
//! {"tool_name": "Bash", "tool_input": {"command": "git checkout main"}}
//! ```
//!
//! Exit 0 = allow. Exit 2 = block (Claude surfaces the message we print to
//! stderr to the model). Anything else is treated as "non-blocking error."
//!
//! Reference: <https://docs.claude.com/en/docs/claude-code/hooks>

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

/// Branches workers must never switch onto, reset against, or restore from.
/// `merge_target` is added dynamically per-task via the BREHON_MERGE_TARGET
/// env var so epic subtasks targeting `epic/foo` are protected too.
const ALWAYS_PROTECTED: &[&str] = &["main", "master", "develop", "trunk", "HEAD"];

/// Path (relative to the worktree containing this hook) that Brehon writes
/// while a `brehon run` is active. When this file is absent the hook is a
/// no-op — that way the user's normal Claude Code usage outside Brehon
/// sessions is undisturbed, even if the hook config remained installed
/// after a crash.
const ACTIVE_MARKER_RELATIVE: &str = ".brehon/runtime/claude-hook-active";

/// Entry point. Reads stdin, applies policy, writes a decision.
pub fn execute() -> ExitCode {
    // Defense in depth: if the marker is missing, Brehon isn't running and
    // this hook should fall through. Cleanup also removes the hook config
    // from settings.local.json, so we shouldn't normally reach this branch
    // without an active session — but if we do (crashed cleanup, stale
    // config in a checked-in settings file, etc.), we must not block the
    // user.
    if !marker_present() {
        return ExitCode::SUCCESS;
    }

    let mut buf = String::new();
    if io::stdin().read_to_string(&mut buf).is_err() {
        // No stdin / unreadable — fail open so we don't break Claude Code
        // when the hook is invoked outside its normal protocol.
        return ExitCode::SUCCESS;
    }

    let payload: Value = match serde_json::from_str(&buf) {
        Ok(v) => v,
        Err(_) => return ExitCode::SUCCESS,
    };

    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_input = payload.get("tool_input").cloned().unwrap_or(Value::Null);

    let decision = evaluate(tool_name, &tool_input, &PolicyContext::from_env());

    match decision {
        Decision::Allow => ExitCode::SUCCESS,
        Decision::Block(reason) => {
            // Exit code 2 + message on stderr is the documented "block and
            // tell the model why" path.
            eprintln!("Brehon worktree-guard denied this call: {reason}");
            ExitCode::from(2)
        }
    }
}

/// Check for the active marker by walking up from the current directory.
/// Claude Code launches the hook from the worker's worktree, so the marker
/// lives at `<worktree-or-project>/.brehon/runtime/claude-hook-active`.
fn marker_present() -> bool {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut dir: &Path = &cwd;
    loop {
        if dir.join(ACTIVE_MARKER_RELATIVE).exists() {
            return true;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Allow,
    Block(String),
}

struct PolicyContext {
    /// Absolute path to the worker's worktree (BREHON_WORKSPACE_ROOT).
    worktree_root: Option<PathBuf>,
    /// Current directory of the hook process. Relative tool paths are unsafe
    /// unless this is inside the assigned worktree.
    current_dir: Option<PathBuf>,
    /// Shared repository root. Used to produce precise denial messages when
    /// a file tool tries to mutate the protected checkout.
    project_root: Option<PathBuf>,
    /// Agent role for role-specific exceptions, such as supervisor repairs.
    agent_role: Option<String>,
    /// Brehon runtime root. Used to identify Brehon-owned integration worktrees.
    brehon_root: Option<PathBuf>,
    /// Extra protected branch from the task's merge_target, if any.
    merge_target: Option<String>,
}

impl PolicyContext {
    fn from_env() -> Self {
        let brehon_root = std::env::var("BREHON_ROOT").ok().map(PathBuf::from);
        let project_root = std::env::var("BREHON_PROJECT_ROOT")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                brehon_root
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            });
        Self {
            worktree_root: std::env::var("BREHON_WORKSPACE_ROOT")
                .ok()
                .map(PathBuf::from),
            current_dir: std::env::current_dir().ok(),
            project_root,
            agent_role: std::env::var("BREHON_AGENT_ROLE")
                .ok()
                .filter(|s| !s.is_empty()),
            brehon_root,
            merge_target: std::env::var("BREHON_MERGE_TARGET")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }

    fn protected_branches(&self) -> Vec<&str> {
        let mut out: Vec<&str> = ALWAYS_PROTECTED.to_vec();
        if let Some(target) = self.merge_target.as_deref() {
            out.push(target);
        }
        out
    }
}

fn evaluate(tool_name: &str, tool_input: &Value, ctx: &PolicyContext) -> Decision {
    match tool_name {
        "Bash" => evaluate_bash(tool_input, ctx),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            evaluate_mutating_file_tool(tool_name, tool_input, ctx)
        }
        "Task" => evaluate_task_tool(tool_input, ctx),
        _ => Decision::Allow,
    }
}

fn evaluate_task_tool(_tool_input: &Value, _ctx: &PolicyContext) -> Decision {
    Decision::Block(
        "Claude Task/subagent execution is disabled inside Brehon runs because it creates unmanaged Claude worktrees outside Brehon's assigned worktree pool. Continue in this pane, or use Brehon task/research/review tools instead."
            .to_string(),
    )
}

fn evaluate_bash(tool_input: &Value, ctx: &PolicyContext) -> Decision {
    if let Decision::Block(reason) = check_hook_cwd_inside_allowed_root(ctx) {
        return Decision::Block(reason);
    }

    let cmd = match tool_input.get("command").and_then(Value::as_str) {
        Some(c) => c,
        None => return Decision::Allow,
    };

    // Split on `&&`, `||`, `;`, and `|` so a single Bash call can't smuggle a
    // forbidden subcommand past us by chaining. We don't try to fully parse
    // bash — that's a losing battle — we just check each segment in isolation.
    for segment in split_segments(cmd) {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Decision::Block(reason) = check_git_branch_change(trimmed, ctx) {
            return Decision::Block(reason);
        }
        if let Decision::Block(reason) = check_cd_outside_worktree(trimmed, ctx) {
            return Decision::Block(reason);
        }
        if let Decision::Block(reason) = check_bash_file_write_outside_worktree(trimmed, ctx) {
            return Decision::Block(reason);
        }
    }

    Decision::Allow
}

fn evaluate_mutating_file_tool(
    tool_name: &str,
    tool_input: &Value,
    ctx: &PolicyContext,
) -> Decision {
    let paths = mutating_tool_paths(tool_input);
    if paths.is_empty() {
        return Decision::Block(format!(
            "`{tool_name}` did not include a recognized file path. Brehon fails closed for mutating Claude tools during isolated runs."
        ));
    }

    for (key, path) in paths {
        if let Decision::Block(reason) = validate_mutating_path(tool_name, key, &path, ctx) {
            return Decision::Block(reason);
        }
    }
    Decision::Allow
}

fn mutating_tool_paths(tool_input: &Value) -> Vec<(&'static str, String)> {
    ["file_path", "notebook_path", "path"]
        .into_iter()
        .filter_map(|key| {
            tool_input
                .get(key)
                .and_then(Value::as_str)
                .map(|value| (key, value.to_string()))
        })
        .collect()
}

fn validate_mutating_path(
    tool_name: &str,
    key: &str,
    raw_path: &str,
    ctx: &PolicyContext,
) -> Decision {
    let Some(path) = normalize_candidate_path(raw_path, ctx) else {
        return Decision::Block(format!(
            "`{tool_name}` cannot mutate `{raw_path}` because Brehon could not resolve `{key}` inside the agent worktree."
        ));
    };

    if path_allowed_for_mutation(&path, ctx) {
        return Decision::Allow;
    }

    let shared_root = ctx.project_root.as_deref().map(lexical_normalize);
    if shared_root
        .as_ref()
        .is_some_and(|root| path.starts_with(root))
    {
        return Decision::Block(format!(
            "`{tool_name}` attempted to mutate `{}` under the shared repo root `{}`. \
             Write only inside your assigned worktree.",
            path.display(),
            shared_root.unwrap().display()
        ));
    }

    let worktree = ctx
        .worktree_root
        .as_deref()
        .map(|path| lexical_normalize(path).display().to_string())
        .unwrap_or_else(|| "<missing BREHON_WORKSPACE_ROOT>".to_string());
    Decision::Block(format!(
        "`{tool_name}` attempted to mutate `{}`, outside the assigned worktree (`{worktree}`).",
        path.display()
    ))
}

fn normalize_candidate_path(raw_path: &str, ctx: &PolicyContext) -> Option<PathBuf> {
    let cleaned = clean_shell_path_token(raw_path)?;
    if cleaned.contains('$') || cleaned.contains('`') || cleaned.is_empty() {
        return None;
    }
    let path = Path::new(cleaned);
    if path.is_absolute() {
        Some(lexical_normalize(path))
    } else {
        let base = ctx
            .current_dir
            .as_deref()
            .or(ctx.worktree_root.as_deref())?;
        Some(lexical_normalize(&base.join(path)))
    }
}

fn clean_shell_path_token(token: &str) -> Option<&str> {
    let cleaned = token
        .trim()
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\'']);
    if cleaned.is_empty()
        || cleaned == "-"
        || cleaned == "/dev/null"
        || cleaned.starts_with('&')
        || cleaned.contains(">&")
    {
        return None;
    }
    Some(cleaned)
}

fn path_allowed_for_mutation(path: &Path, ctx: &PolicyContext) -> bool {
    if ctx
        .worktree_root
        .as_deref()
        .map(|worktree| path.starts_with(lexical_normalize(worktree)))
        .unwrap_or(false)
    {
        return true;
    }
    is_supervisor_integration_worktree(path, ctx)
}

fn check_hook_cwd_inside_allowed_root(ctx: &PolicyContext) -> Decision {
    let Some(current_dir) = ctx.current_dir.as_deref() else {
        return Decision::Allow;
    };
    let current_dir = lexical_normalize(current_dir);
    if path_allowed_for_mutation(&current_dir, ctx) {
        return Decision::Allow;
    }
    Decision::Block(format!(
        "Claude hook is executing from `{}`, outside the assigned worktree. \
         This indicates the agent process was launched or moved outside containment; Brehon fails closed.",
        current_dir.display()
    ))
}

/// Block `git checkout <protected>`, `git switch <protected>`,
/// `git reset --hard <protected>`, and `git restore --source=<protected>`.
fn check_git_branch_change(segment: &str, ctx: &PolicyContext) -> Decision {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    if tokens.len() < 3 || tokens[0] != "git" {
        return Decision::Allow;
    }
    let subcommand = tokens[1];

    let mentions_protected = |args: &[&str]| -> Option<String> { protected_token_in(args, ctx) };

    let block = |branch: String, reason: &str| -> Decision {
        Decision::Block(format!(
            "{reason} (`{branch}` is on the protected list). Stay on your dedicated worker \
             branch — every shell command runs in your worktree, and switching off the worker \
             branch causes silent empty commits at completion."
        ))
    };

    match subcommand {
        "checkout" | "switch" => {
            if let Some(branch) = mentions_protected(&tokens[2..]) {
                return block(branch, &format!("`git {subcommand}` to a protected branch"));
            }
        }
        "reset" if tokens.contains(&"--hard") => {
            // Block `git reset --hard <protected>` (destroys worker branch state).
            if let Some(branch) = mentions_protected(&tokens[2..]) {
                return block(branch, "`git reset --hard` against a protected branch");
            }
        }
        "restore" => {
            // Block `git restore --source=<protected> ...` (pulls protected
            // tree into the worker branch's working tree).
            for token in &tokens[2..] {
                // Bare `--source <ref>` is two tokens; follow-up completeness
                // can handle that variant if needed.
                let source = token
                    .strip_prefix("--source=")
                    .or_else(|| token.strip_prefix("-s="));
                if let Some(src) = source {
                    if ctx.protected_branches().contains(&src) {
                        return block(
                            src.to_string(),
                            "`git restore --source=` from a protected branch",
                        );
                    }
                }
            }
        }
        _ => {}
    }

    Decision::Allow
}

fn check_bash_file_write_outside_worktree(segment: &str, ctx: &PolicyContext) -> Decision {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    if tokens.is_empty() {
        return Decision::Allow;
    }

    for (index, token) in tokens.iter().enumerate() {
        if let Some(target) = redirection_target(&tokens, index, token) {
            if let Decision::Block(reason) =
                validate_mutating_path("Bash redirection", "target", target, ctx)
            {
                return Decision::Block(reason);
            }
        }
    }

    let command_index = first_command_token(&tokens);
    let Some(command_index) = command_index else {
        return Decision::Allow;
    };
    let command = tokens[command_index]
        .rsplit('/')
        .next()
        .unwrap_or(tokens[command_index]);
    let args = &tokens[command_index + 1..];

    match command {
        "tee" => validate_all_non_option_paths("Bash tee", args, ctx),
        "touch" | "mkdir" | "rm" | "rmdir" | "truncate" | "chmod" | "chown" | "chgrp" => {
            validate_all_non_option_paths(&format!("Bash {command}"), args, ctx)
        }
        "cp" | "install" => validate_last_non_option_path(&format!("Bash {command}"), args, ctx),
        "mv" => validate_all_non_option_paths("Bash mv", args, ctx),
        _ => Decision::Allow,
    }
}

fn redirection_target<'a>(tokens: &'a [&'a str], index: usize, token: &'a str) -> Option<&'a str> {
    const REDIRECTS: &[&str] = &[">", ">>", ">|", "1>", "1>>", "2>", "2>>", "&>"];
    let target = if REDIRECTS.contains(&token) {
        tokens.get(index + 1).copied()
    } else {
        REDIRECTS
            .iter()
            .find_map(|prefix| token.strip_prefix(prefix).filter(|rest| !rest.is_empty()))
    };
    target.filter(|target| clean_shell_path_token(target).is_some())
}

fn first_command_token(tokens: &[&str]) -> Option<usize> {
    let mut index = 0usize;
    while index < tokens.len() {
        let token = tokens[index];
        if token == "env" || token == "command" || token == "builtin" || token == "sudo" {
            index += 1;
            continue;
        }
        if token.contains('=') {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

fn validate_all_non_option_paths(tool_name: &str, args: &[&str], ctx: &PolicyContext) -> Decision {
    for arg in non_option_path_args(args) {
        if let Decision::Block(reason) = validate_mutating_path(tool_name, "path", arg, ctx) {
            return Decision::Block(reason);
        }
    }
    Decision::Allow
}

fn validate_last_non_option_path(tool_name: &str, args: &[&str], ctx: &PolicyContext) -> Decision {
    if let Some(arg) = non_option_path_args(args).last() {
        validate_mutating_path(tool_name, "path", arg, ctx)
    } else {
        Decision::Allow
    }
}

fn non_option_path_args<'a>(args: &'a [&'a str]) -> Vec<&'a str> {
    args.iter()
        .copied()
        .filter(|arg| !arg.starts_with('-'))
        .filter(|arg| !arg.contains('='))
        .collect()
}

/// Find the first token that is, or contains a path component matching, a
/// protected branch. Handles `main`, `origin/main`, `refs/heads/main`, etc.
fn protected_token_in(tokens: &[&str], ctx: &PolicyContext) -> Option<String> {
    let protected = ctx.protected_branches();
    for token in tokens {
        if token.starts_with('-') {
            continue;
        }
        // Trailing `--` or path-separator-only tokens are not branch args.
        if token.is_empty() || *token == "--" {
            continue;
        }
        let stripped = token
            .trim_start_matches("refs/heads/")
            .trim_start_matches("refs/remotes/")
            .trim_start_matches("origin/")
            .trim_start_matches("upstream/");
        if protected.contains(&stripped) {
            return Some((*token).to_string());
        }
        if protected.contains(token) {
            return Some((*token).to_string());
        }
    }
    None
}

/// Block `cd <path>` where the resolved path leaves the worktree.
///
/// Heuristic: we only block when we can confidently resolve the destination
/// to an absolute path that escapes BREHON_WORKSPACE_ROOT. If we can't tell
/// (e.g. `cd "$VAR"`, shell substitution), we allow — the worker protocol
/// already tells the model not to do this, and false-positive blocks erode
/// trust in the guard.
fn check_cd_outside_worktree(segment: &str, ctx: &PolicyContext) -> Decision {
    let worktree = match ctx.worktree_root.as_deref() {
        Some(p) => p,
        None => return Decision::Allow,
    };
    let stripped = segment
        .trim_start_matches("builtin ")
        .trim_start_matches("command ");
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    if tokens.is_empty() || tokens[0] != "cd" {
        return Decision::Allow;
    }
    // `cd` with no argument goes to $HOME — outside the worktree.
    if tokens.len() == 1 {
        return Decision::Block(format!(
            "bare `cd` goes to $HOME, outside the worktree ({}). Stay in the worktree.",
            worktree.display()
        ));
    }
    // Skip option-style tokens (-P, -L, --).
    let target_token = match tokens.iter().skip(1).find(|t| !t.starts_with('-')) {
        Some(t) => *t,
        None => return Decision::Allow,
    };

    // Skip cases we can't safely resolve.
    if target_token.contains('$') || target_token.contains('`') || target_token.contains('~') {
        return Decision::Allow;
    }

    // Strip surrounding quotes.
    let cleaned = target_token
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\'']);

    let candidate = if Path::new(cleaned).is_absolute() {
        PathBuf::from(cleaned)
    } else {
        worktree.join(cleaned)
    };

    // Normalize without filesystem access (lexical resolution) so the hook
    // doesn't need the directory to exist.
    let normalized = lexical_normalize(&candidate);

    if !normalized.starts_with(worktree) && !is_supervisor_integration_worktree(&normalized, ctx) {
        return Decision::Block(format!(
            "`cd {target_token}` resolves to `{}`, outside the worktree (`{}`). \
             Stay in the worktree.",
            normalized.display(),
            worktree.display()
        ));
    }
    Decision::Allow
}

fn is_supervisor_integration_worktree(path: &Path, ctx: &PolicyContext) -> bool {
    if ctx.agent_role.as_deref() != Some("supervisor") {
        return false;
    }
    let Some(brehon_root) = ctx.brehon_root.as_deref() else {
        return false;
    };

    let worktrees_root = lexical_normalize(&brehon_root.join("worktrees"));
    let integration_roots = [
        worktrees_root.join("epic"),
        worktrees_root.join("initiative"),
    ];
    integration_roots
        .iter()
        .any(|integration_root| path.starts_with(integration_root))
}

/// Resolve `.` and `..` components without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Split a bash command on top-level `&&`, `||`, `;`, and pipe characters.
/// This is intentionally naive — we don't honor quoting, but for the purpose
/// of spotting `git checkout main` smuggled into `something && git checkout main`
/// it's good enough.
fn split_segments(cmd: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = cmd.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let two = if i + 1 < bytes.len() {
            &bytes[i..i + 2]
        } else {
            &bytes[i..i + 1]
        };
        if two == b"&&" || two == b"||" {
            out.push(&cmd[start..i]);
            i += 2;
            start = i;
            continue;
        }
        let b = bytes[i];
        if b == b';' || b == b'|' || b == b'\n' {
            out.push(&cmd[start..i]);
            i += 1;
            start = i;
            continue;
        }
        i += 1;
    }
    if start < bytes.len() {
        out.push(&cmd[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_with(worktree: &str, merge_target: Option<&str>) -> PolicyContext {
        PolicyContext {
            worktree_root: Some(PathBuf::from(worktree)),
            current_dir: Some(PathBuf::from(worktree)),
            project_root: None,
            agent_role: None,
            brehon_root: None,
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
            merge_target: None,
        }
    }

    fn bash(cmd: &str) -> Value {
        json!({ "command": cmd })
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
            merge_target: None,
        };
        let decision = evaluate("Bash", &bash("cd /repo/.brehon/worktrees/epic/T-123"), &ctx);
        assert!(matches!(decision, Decision::Block(_)));
    }

    #[test]
    fn allows_cd_with_shell_variable() {
        // We can't resolve $VAR safely, so we let it through. The worker
        // prompt still tells the model not to do this.
        let decision = evaluate("Bash", &bash("cd $HOME"), &ctx_with("/work", None));
        assert_eq!(decision, Decision::Allow);
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
            merge_target: None,
        };
        assert_eq!(evaluate("Bash", &bash("cd .."), &ctx), Decision::Allow);
        assert!(matches!(
            evaluate("Bash", &bash("git checkout main"), &ctx),
            Decision::Block(_)
        ));
    }
}
