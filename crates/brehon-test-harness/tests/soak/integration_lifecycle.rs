use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::MutexGuard;
use std::time::{Duration, Instant};

use brehon_mcp::server::ContentBlock;
use brehon_mcp::tools::factory::FactoryTool;
use brehon_mcp::tools::task_actions::TaskActionsTool;
use brehon_mcp::tools::verification::VerificationTool;
use brehon_mcp::tools::Tool;
use brehon_mux::{
    AgentAdapter, AgentPaneMaterialization, Mux, MuxConfig, PromptQueueEntry, SessionScopedQueue,
    SupervisorCli,
};
use brehon_ports::AgentGateway;
use brehon_test_harness::TEST_ENV_LOCK;
use brehon_tui::RuntimeAutomationHarness;
use brehon_types::config::{ReviewConfig, ReviewPanelMode};
use brehon_types::review::ReviewPolicy;
use brehon_types::sanitize_runtime_key;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const SUPERVISOR_NAME: &str = "claude-supervisor";
const RUNTIME_SESSION_NAME: &str = "soak-runtime";
const RUNTIME_ADVANCE: Duration = Duration::from_secs(31);
const GATEWAY_LOG_DIR_ENV: &str = "BREHON_SOAK_GATEWAY_LOG_DIR";

struct ScopedBrehonEnv {
    saved: Vec<(OsString, Option<OsString>)>,
}

impl ScopedBrehonEnv {
    // This test needs repo-local `OsString` values plus a full BREHON_*
    // prefix scrub between actors so worker/reviewer/supervisor context does
    // not leak across tool calls. `brehon_test_harness::test_env::ScopedEnv`
    // only accepts `&str` pairs and restores an explicit key list, so the
    // soak keeps this narrower helper local for now.
    fn set(vars: Vec<(&'static str, OsString)>) -> Self {
        let mut saved = Vec::new();
        let mut tracked_keys = Vec::new();

        for (key, value) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BREHON_") {
                tracked_keys.push(key.clone());
                saved.push((key, Some(value)));
            }
        }

        for (key, _) in &vars {
            let key = OsString::from(key);
            if !tracked_keys.iter().any(|existing| existing == &key) {
                tracked_keys.push(key.clone());
                saved.push((key, None));
            }
        }

        for key in &tracked_keys {
            std::env::remove_var(key);
        }
        for (key, value) in vars {
            std::env::set_var(key, value);
        }

        Self { saved }
    }
}

impl Drop for ScopedBrehonEnv {
    fn drop(&mut self) {
        let current_keys: Vec<OsString> = std::env::vars_os()
            .filter_map(|(key, _)| key.to_string_lossy().starts_with("BREHON_").then_some(key))
            .collect();
        for key in current_keys {
            std::env::remove_var(key);
        }
        for (key, value) in &self.saved {
            if let Some(value) = value {
                std::env::set_var(key, value);
            }
        }
    }
}

struct ScopedEnvVar {
    key: OsString,
    saved: Option<OsString>,
}

impl ScopedEnvVar {
    fn set(key: &str, value: OsString) -> Self {
        let key_os = OsString::from(key);
        let saved = std::env::var_os(key);
        std::env::set_var(key, &value);
        Self { key: key_os, saved }
    }

    fn prepend_path(prefix: &Path) -> Self {
        let mut paths = vec![prefix.to_path_buf()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        let joined = std::env::join_paths(paths).expect("join PATH entries");
        Self::set("PATH", joined)
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        if let Some(value) = &self.saved {
            std::env::set_var(&self.key, value);
        } else {
            std::env::remove_var(&self.key);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedReviewerReset {
    task_id: String,
    review_id: String,
    reviewer: String,
    #[serde(default)]
    requested_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedWorkerRecycle {
    task_id: String,
    worker: String,
    #[serde(default)]
    requested_at: Option<String>,
}

fn extract_text(result: &brehon_mcp::server::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn parse_result(result: &brehon_mcp::server::ToolResult) -> Value {
    let text = extract_text(result);
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse tool result as JSON: {err}\n{text}"))
}

fn run_git(workspace: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        workspace.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_git_workspace(workspace: &Path) {
    run_git(workspace, &["init", "-b", "main"]);
    run_git(workspace, &["config", "user.email", "test@example.com"]);
    run_git(workspace, &["config", "user.name", "Test User"]);
    run_git(workspace, &["config", "commit.gpgsign", "false"]);
    std::fs::write(workspace.join("README.md"), "seed\n").expect("write seed file");
    std::fs::write(workspace.join(".gitignore"), ".brehon/\n").expect("write gitignore");
    run_git(workspace, &["add", "README.md", ".gitignore"]);
    run_git(workspace, &["commit", "-m", "seed"]);
}

fn create_worktree(repo_root: &Path, relative: &str, branch: &str) -> PathBuf {
    let worktree = repo_root.join(relative);
    std::fs::create_dir_all(worktree.parent().expect("worktree parent")).expect("create parent");
    run_git(
        repo_root,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            worktree.to_str().expect("utf-8 worktree path"),
            "HEAD",
        ],
    );
    worktree
}

fn write_session(root: &Path, name: &str, role: &str, session_id: &str, agent_type: Option<&str>) {
    let sessions_dir = root.join("runtime").join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");
    let now = Utc::now().to_rfc3339();
    let mut entry = json!({
        "name": name,
        "role": role,
        "session_id": session_id,
        "registered_at": now,
        "last_seen_at": now,
    });
    if let Some(agent_type) = agent_type.filter(|value| !value.is_empty()) {
        entry["agent_type"] = Value::String(agent_type.to_string());
    }
    std::fs::write(
        sessions_dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(&entry).expect("serialize session"),
    )
    .expect("write session");
}

fn collect_queued_messages(root: &Path, target: Option<&str>) -> Vec<String> {
    let mut messages = Vec::new();
    let queue_dir = root.join("runtime").join("prompt-queue");

    fn walk(dir: &Path, target: Option<&str>, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                walk(&path, target, out);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.') || name.ends_with(".tmp"))
            {
                continue;
            }

            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<Value>(&content) else {
                continue;
            };
            let payload = parsed.get("entry").unwrap_or(&parsed);
            if let Some(target) = target {
                if payload.get("target").and_then(|value| value.as_str()) != Some(target) {
                    continue;
                }
            }
            if let Some(message) = payload.get("message").and_then(|value| value.as_str()) {
                out.push(message.to_string());
            }
        }
    }

    walk(&queue_dir, target, &mut messages);
    messages
}

fn assert_runtime_approval_state_clear(root: &Path, context: &str) {
    for path in [
        root.join("runtime").join("daemon").join("current.json"),
        root.join("runtime").join("daemon").join("approvals.json"),
        root.join("runtime").join("approvals.json"),
    ] {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed: Value = serde_json::from_str(&contents)
            .unwrap_or_else(|err| panic!("{context}: failed to parse {}: {err}", path.display()));

        let approvals = match path.file_name().and_then(|name| name.to_str()) {
            Some("current.json") => parsed
                .get("approvals")
                .and_then(|value| value.get("approvals"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            _ => parsed
                .get("approvals")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        };
        assert!(
            approvals.is_empty(),
            "{context}: runtime approval state should be empty in {}: {}",
            path.display(),
            serde_json::to_string_pretty(&parsed).expect("serialize approval state"),
        );

        if path.file_name().and_then(|name| name.to_str()) == Some("current.json") {
            let pending_approvals = parsed
                .get("metrics")
                .and_then(|value| value.get("pending_approvals"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            assert_eq!(
                pending_approvals,
                0,
                "{context}: runtime daemon metrics should report zero pending approvals in {}: {}",
                path.display(),
                serde_json::to_string_pretty(&parsed).expect("serialize daemon status"),
            );
        }
    }
}

fn assert_no_hidden_approval_prompts(root: &Path, context: &str) {
    let messages = collect_queued_messages(root, None)
        .join("\n")
        .to_ascii_lowercase();
    for needle in [
        "approval required",
        "needs approval",
        "terminal approval prompt",
        "manual input required",
    ] {
        assert!(
            !messages.contains(needle),
            "unexpected approval prompt marker '{needle}' in queued messages:\n{messages}"
        );
    }
    assert_runtime_approval_state_clear(root, context);
}

async fn assert_no_pending_gateway_requests(mux: &Mux, context: &str) {
    let counters = mux
        .gateway()
        .expect("gateway should exist for soak runtime")
        .stability_counters()
        .await;
    assert_eq!(
        counters.pending_requests, 0,
        "{context}: gateway should not retain pending requests: {counters:?}"
    );
    assert_eq!(
        counters.pending_prompt_waiters, 0,
        "{context}: gateway should not retain pending prompt waiters: {counters:?}"
    );
}

fn assert_shared_root_clean(workspace: &Path) {
    let status = run_git(workspace, &["status", "--short"]);
    assert!(
        status.is_empty(),
        "shared root should stay clean during soak:\n{status}"
    );
}

fn make_verification_tool() -> VerificationTool {
    let policy = ReviewPolicy {
        min_average_score: 7,
        min_individual_score: 6,
        blocking_score: 5,
        min_approvals: 2,
        require_blocking_feedback_resolution: true,
        max_review_rounds: 3,
    };
    let config = ReviewConfig {
        policy,
        timeout_minutes: 30,
        auto_assign: true,
        default_reviewers: vec!["gemini".to_string(), "kimi".to_string()],
        panel_mode: ReviewPanelMode::FullCouncil,
        ..ReviewConfig::default()
    };
    VerificationTool::new().with_config(config)
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn helper_log_path(root: &Path, agent: &str) -> PathBuf {
    root.join("runtime")
        .join("gateway-helper-logs")
        .join(format!("{agent}.log"))
}

fn wait_for_gateway_log_contains(root: &Path, agent: &str, needle: &str) {
    let path = helper_log_path(root, agent);
    for _ in 0..500 {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if content.contains(needle) {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    panic!("gateway helper log for {agent} never contained {needle:?}:\n{content}");
}

fn gateway_log_occurrence_count(root: &Path, agent: &str, needle: &str) -> usize {
    std::fs::read_to_string(helper_log_path(root, agent))
        .unwrap_or_default()
        .matches(needle)
        .count()
}

fn wait_for_gateway_log_occurrence_at_least(root: &Path, agent: &str, needle: &str, min: usize) {
    let path = helper_log_path(root, agent);
    for _ in 0..500 {
        let count = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .matches(needle)
            .count();
        if count >= min {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    panic!(
        "gateway helper log for {agent} never reached {min} occurrences of {needle:?}:\n{content}"
    );
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write helper script");
    let mut permissions = std::fs::metadata(path)
        .expect("helper metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod helper script");
}

fn pump_mux(mux: &mut Mux) {
    for _ in 0..64 {
        let (_, events) = mux.poll_batch();
        if events.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn advance_runtime(mux: &mut Mux) {
    let rt = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| {
        mux.tick_pane_state_machine_at(&rt, Instant::now() + RUNTIME_ADVANCE);
    });
    pump_mux(mux);
}

fn reviewer_reset_ack_path(root: &Path, request: &QueuedReviewerReset) -> PathBuf {
    root.join("runtime")
        .join("reviewer-reset-acks")
        .join(format!(
            "{}--{}--{}.json",
            sanitize_runtime_key(&request.task_id),
            sanitize_runtime_key(&request.review_id),
            sanitize_runtime_key(&request.reviewer)
        ))
}

fn worker_recycle_ack_path(root: &Path, request: &QueuedWorkerRecycle) -> PathBuf {
    root.join("runtime")
        .join("worker-recycle-acks")
        .join(format!(
            "{}--{}.json",
            sanitize_runtime_key(&request.task_id),
            sanitize_runtime_key(&request.worker)
        ))
}

struct RuntimeFixture {
    workspace: tempfile::TempDir,
    root: PathBuf,
    helper_bin_dir: PathBuf,
    supervisor_worktree: PathBuf,
    supervisor_branch: String,
    worker_worktrees: HashMap<String, PathBuf>,
    worker_branches: HashMap<String, String>,
    reviewer_worktrees: HashMap<String, PathBuf>,
    reviewer_branches: HashMap<String, String>,
}

impl RuntimeFixture {
    fn new() -> Self {
        assert!(
            python3_available(),
            "python3 is required for gateway_helper.py in the unattended lifecycle soak fixture"
        );
        let workspace = tempfile::tempdir().expect("tempdir");
        init_git_workspace(workspace.path());

        let root = workspace.path().join(".brehon");
        std::fs::create_dir_all(&root).expect("create brehon root");
        let runtime_daemon_dir = root.join("runtime").join("daemon");
        std::fs::create_dir_all(&runtime_daemon_dir).expect("create runtime daemon dir");
        std::fs::write(
            runtime_daemon_dir.join("approvals.json"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "session_id": RUNTIME_SESSION_NAME,
                "written_at_ms": Utc::now().timestamp_millis(),
                "approvals": [],
            }))
            .expect("serialize empty approval store"),
        )
        .expect("write empty approval store");
        let instructions_dir = root.join("instructions");
        std::fs::create_dir_all(&instructions_dir).expect("create instructions dir");
        for provider in ["claude", "codex", "gemini", "kimi"] {
            for role in ["worker", "reviewer", "supervisor", "advisor", "research"] {
                let file = format!("{provider}-{role}-instructions.md");
                std::fs::write(instructions_dir.join(&file), format!("{file}\n"))
                    .expect("write instruction file");
            }
        }

        let helper_bin_dir = root.join("runtime").join("cli-shims");
        let helper_log_dir = root.join("runtime").join("gateway-helper-logs");
        std::fs::create_dir_all(&helper_bin_dir).expect("create helper bin dir");
        std::fs::create_dir_all(&helper_log_dir).expect("create helper log dir");

        let helper_program = helper_bin_dir.join("gateway_helper.py");
        let helper_program_contents = r#"#!/usr/bin/env python3
import json
import os
import sys

PROTOCOL = sys.argv[1] if len(sys.argv) > 1 else "acp"
AGENT = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else os.environ.get("BREHON_AGENT_NAME", "unknown-agent")
LOG_DIR = sys.argv[3] if len(sys.argv) > 3 and sys.argv[3] else os.environ.get("__BREHON_SOAK_GATEWAY_LOG_DIR__")
LOG_PATH = os.path.join(LOG_DIR, f"{AGENT}.log") if LOG_DIR else None

def helper_log(kind, message):
    if not LOG_PATH:
        return
    os.makedirs(os.path.dirname(LOG_PATH), exist_ok=True)
    with open(LOG_PATH, "a", encoding="utf-8") as handle:
        handle.write(f"{kind}\t{message}\n")

def send(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()

def read_message():
    while True:
        line = sys.stdin.readline()
        if line == "":
            sys.exit(0)
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            continue

permission_count = 0
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = message.get("method")
    if not method:
        continue
    request_id = message.get("id")
    helper_log("method", method)

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"protocolVersion": 1, "agentCapabilities": {"content_block_types": ["text"], "session_config_options": ["mode", "model"], "permission_support": True, "terminal_support": False, "tool_call_streaming": "basic" if PROTOCOL == "gemini" else "full", "promptCapabilities": {"image": False, "audio": False, "embeddedContext": False}}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"sessionId": f"{AGENT}-{PROTOCOL}-session", "modes": {"availableModes": [{"id": "default"}, {"id": "yolo"}]}}})
    elif method in ("session/set_mode", "session/set_model", "shutdown"):
        send({"jsonrpc": "2.0", "id": request_id, "result": {}})
    elif method == "session/prompt":
        prompt = ""
        params = message.get("params") or {}
        blocks = params.get("prompt") or []
        if blocks:
            prompt = blocks[0].get("text", "")
        helper_log("prompt", prompt)
        if PROTOCOL == "gemini":
            permission_count += 1
            permission_id = f"permission-{AGENT}-{permission_count}"
            send({"jsonrpc": "2.0", "id": permission_id, "method": "session/request_permission", "params": {"options": [{"optionId": "allow_once", "kind": "allow_once"}, {"optionId": "cancel", "kind": "deny"}]}})
            while True:
                response = read_message()
                if response.get("id") == permission_id:
                    helper_log("permission", json.dumps(response, sort_keys=True))
                    break
        send({"jsonrpc": "2.0", "id": request_id, "result": {"response": f"{AGENT} handled prompt", "tokensUsed": 1, "stopReason": "stop"}})
    else:
        send({"jsonrpc": "2.0", "id": request_id, "result": {}})
"#
        .replace("__BREHON_SOAK_GATEWAY_LOG_DIR__", GATEWAY_LOG_DIR_ENV);
        write_executable(&helper_program, &helper_program_contents);
        let py_compile = Command::new("python3")
            .args([
                "-c",
                "import pathlib, sys; compile(pathlib.Path(sys.argv[1]).read_text(), sys.argv[1], 'exec')",
            ])
            .arg(&helper_program)
            .output()
            .expect("run py_compile for gateway helper");
        assert!(
            py_compile.status.success(),
            "gateway helper script must compile: {}",
            String::from_utf8_lossy(&py_compile.stderr)
        );

        for (command, protocol) in [("gemini", "gemini"), ("kimi", "acp")] {
            let script_path = helper_bin_dir.join(command);
            let contents = format!(
                "#!/bin/sh\nexec python3 -u {helper} {protocol} \"$BREHON_AGENT_NAME\" {log_dir} \"$@\"\n",
                helper = shell_single_quote(helper_program.to_str().expect("utf-8 helper path")),
                log_dir = shell_single_quote(helper_log_dir.to_str().expect("utf-8 helper log dir")),
            );
            write_executable(&script_path, &contents);
        }

        let supervisor_branch = "supervisor/claude-supervisor".to_string();
        let supervisor_worktree = create_worktree(
            workspace.path(),
            ".brehon/worktrees/supervisor/claude-supervisor",
            &supervisor_branch,
        );

        let workers = [
            ("worker-gemini", "worker/worker-gemini", "gemini"),
            ("worker-kimi", "worker/worker-kimi", "kimi"),
        ];
        let reviewers = [
            ("reviewer-gemini", "reviewer/reviewer-gemini", "gemini"),
            ("reviewer-kimi", "reviewer/reviewer-kimi", "kimi"),
        ];

        let mut worker_worktrees = HashMap::new();
        let mut worker_branches = HashMap::new();
        for (name, branch, agent_type) in workers {
            let path = create_worktree(
                workspace.path(),
                &format!(".brehon/worktrees/runs/soak/{name}"),
                branch,
            );
            worker_worktrees.insert(name.to_string(), path);
            worker_branches.insert(name.to_string(), branch.to_string());
            write_session(
                &root,
                name,
                "worker",
                &format!("{name}-session"),
                Some(agent_type),
            );
        }

        let mut reviewer_worktrees = HashMap::new();
        let mut reviewer_branches = HashMap::new();
        for (name, branch, agent_type) in reviewers {
            let path = create_worktree(
                workspace.path(),
                &format!(".brehon/worktrees/reviewer/{name}"),
                branch,
            );
            reviewer_worktrees.insert(name.to_string(), path);
            reviewer_branches.insert(name.to_string(), branch.to_string());
            write_session(
                &root,
                name,
                "reviewer",
                &format!("{name}-session"),
                Some(agent_type),
            );
        }

        write_session(
            &root,
            SUPERVISOR_NAME,
            "supervisor",
            "claude-supervisor-session",
            Some("kimi"),
        );

        Self {
            workspace,
            root,
            helper_bin_dir,
            supervisor_worktree,
            supervisor_branch,
            worker_worktrees,
            worker_branches,
            reviewer_worktrees,
            reviewer_branches,
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }

    fn path_guard(&self) -> ScopedEnvVar {
        ScopedEnvVar::prepend_path(&self.helper_bin_dir)
    }

    fn task_path(&self, task_id: &str) -> PathBuf {
        self.root
            .join("runtime")
            .join("tasks")
            .join(format!("{task_id}.json"))
    }

    fn write_pending_task(&self, task_id: &str, title: &str) {
        let tasks_dir = self.root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).expect("create tasks dir");
        let task = json!({
            "id": task_id,
            "task_id": task_id,
            "title": title,
            "description": format!("{title} unattended soak validation"),
            "status": "pending",
            "task_type": "task",
            "completion_mode": "close",
            "assignee": Value::Null,
        });
        std::fs::write(
            self.task_path(task_id),
            serde_json::to_string_pretty(&task).expect("serialize task"),
        )
        .expect("write task");
    }

    fn read_task(&self, task_id: &str) -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(self.task_path(task_id)).expect("read task json"),
        )
        .expect("parse task json")
    }

    fn actor_env(&self, actor: &str, role: &str) -> ScopedBrehonEnv {
        let (workspace_root, branch) = match role {
            "worker" => (
                self.worker_worktrees
                    .get(actor)
                    .expect("worker worktree")
                    .clone(),
                self.worker_branches
                    .get(actor)
                    .expect("worker branch")
                    .clone(),
            ),
            "reviewer" => (
                self.reviewer_worktrees
                    .get(actor)
                    .expect("reviewer worktree")
                    .clone(),
                self.reviewer_branches
                    .get(actor)
                    .expect("reviewer branch")
                    .clone(),
            ),
            "supervisor" => (
                self.supervisor_worktree.clone(),
                self.supervisor_branch.clone(),
            ),
            _ => (self.workspace_path().to_path_buf(), String::new()),
        };

        ScopedBrehonEnv::set(vec![
            ("BREHON_ROOT", self.root.clone().into_os_string()),
            ("BREHON_WORKSPACE_ROOT", workspace_root.into_os_string()),
            ("BREHON_PROJECT_ROOT", OsString::new()),
            ("BREHON_WORKTREE_BRANCH", OsString::from(branch)),
            ("BREHON_AGENT_NAME", OsString::from(actor)),
            ("BREHON_AGENT_ROLE", OsString::from(role)),
            ("BREHON_SUPERVISOR_NAME", OsString::from(SUPERVISOR_NAME)),
            ("BREHON_SESSION_NAME", OsString::from(RUNTIME_SESSION_NAME)),
        ])
    }

    fn build_mux(&self) -> Mux {
        let mut worker_cwds = HashMap::new();
        let mut worker_cli_map = HashMap::new();
        let mut worker_env_map = HashMap::new();
        for (name, path) in &self.worker_worktrees {
            worker_cwds.insert(name.clone(), path.clone());
        }
        worker_cli_map.insert(
            "worker-kimi".to_string(),
            AgentAdapter::BuiltIn(SupervisorCli::Kimi),
        );
        for name in ["worker-gemini", "worker-kimi"] {
            worker_env_map.insert(
                name.to_string(),
                vec![(
                    GATEWAY_LOG_DIR_ENV.to_string(),
                    self.root
                        .join("runtime")
                        .join("gateway-helper-logs")
                        .to_string_lossy()
                        .to_string(),
                )],
            );
        }

        let mut reviewer_cwds = HashMap::new();
        let mut reviewer_cli_map = HashMap::new();
        let mut reviewer_env_map = HashMap::new();
        for (name, path) in &self.reviewer_worktrees {
            reviewer_cwds.insert(name.clone(), path.clone());
        }
        reviewer_cli_map.insert(
            "reviewer-kimi".to_string(),
            AgentAdapter::BuiltIn(SupervisorCli::Kimi),
        );
        for name in ["reviewer-gemini", "reviewer-kimi"] {
            reviewer_env_map.insert(
                name.to_string(),
                vec![(
                    GATEWAY_LOG_DIR_ENV.to_string(),
                    self.root
                        .join("runtime")
                        .join("gateway-helper-logs")
                        .to_string_lossy()
                        .to_string(),
                )],
            );
        }

        let mut worker_names = self.worker_worktrees.keys().cloned().collect::<Vec<_>>();
        worker_names.sort();
        let mut reviewer_names = self.reviewer_worktrees.keys().cloned().collect::<Vec<_>>();
        reviewer_names.sort();

        Mux::factory(MuxConfig {
            cwd: self.workspace_path().to_path_buf(),
            session_name: Some(RUNTIME_SESSION_NAME.to_string()),
            brehon_root: Some(self.root.clone()),
            worktree_isolation: true,
            pane_materialization: AgentPaneMaterialization::Spawn,
            worker_cwds,
            supervisor_cwd: Some(self.supervisor_worktree.clone()),
            reviewer_cwds,
            workers: worker_names.len(),
            worker_names,
            supervisor_name: SUPERVISOR_NAME.to_string(),
            supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Kimi),
            supervisor_env: vec![(
                GATEWAY_LOG_DIR_ENV.to_string(),
                self.root
                    .join("runtime")
                    .join("gateway-helper-logs")
                    .to_string_lossy()
                    .to_string(),
            )],
            worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Gemini),
            worker_cli_map,
            worker_env_map,
            reviewer_names,
            reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Gemini),
            reviewer_cli_map,
            reviewer_env_map,
            include_director: false,
            rows: 24,
            cols: 120,
            ..Default::default()
        })
        .expect("create mux")
    }
}

fn reviewer_panel(result_json: &Value) -> Vec<String> {
    result_json["panel"]
        .as_array()
        .expect("panel array")
        .iter()
        .map(|value| value.as_str().expect("panel member string").to_string())
        .collect()
}

fn lock_env() -> MutexGuard<'static, ()> {
    TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_unattended_lifecycle_multiple_providers() {
    let _lock = lock_env();
    assert!(
        python3_available(),
        "python3 is required for gateway_helper.py in the unattended lifecycle soak"
    );
    let fixture = RuntimeFixture::new();
    let _path = fixture.path_guard();
    let _runtime_env = ScopedBrehonEnv::set(vec![
        ("BREHON_ROOT", fixture.root().as_os_str().to_os_string()),
        ("BREHON_SESSION_NAME", OsString::from(RUNTIME_SESSION_NAME)),
    ]);
    let factory = FactoryTool::new();
    let task_tool = TaskActionsTool;
    let verification = make_verification_tool();
    let mut runtime = RuntimeAutomationHarness::new(
        fixture.build_mux(),
        fixture.root().to_path_buf(),
        tokio::runtime::Handle::current(),
    )
    .expect("create runtime automation harness");
    advance_runtime(runtime.mux_mut());
    runtime.service_prompt_and_reset_queues();

    assert!(
        runtime
            .mux()
            .get("worker-gemini")
            .expect("gemini worker pane")
            .is_gateway_backed(),
        "gemini workers should use gateway-backed materialization"
    );
    assert!(
        runtime
            .mux()
            .get("worker-kimi")
            .expect("kimi worker pane")
            .is_gateway_backed(),
        "kimi workers should use gateway-backed materialization"
    );
    assert!(
        runtime
            .mux()
            .get("reviewer-gemini")
            .expect("gemini reviewer pane")
            .is_gateway_backed(),
        "gemini reviewers should use gateway-backed materialization"
    );
    assert!(
        runtime
            .mux()
            .get("reviewer-kimi")
            .expect("kimi reviewer pane")
            .is_gateway_backed(),
        "kimi reviewers should use gateway-backed materialization"
    );
    let workers = ["worker-gemini", "worker-kimi"];
    let gateway_reviewers = ["reviewer-gemini", "reviewer-kimi"];

    // Keep the default short for routine runs; set BREHON_SOAK_CYCLES to a
    // larger positive integer when debugging deeper real git/Fjall/worktree
    // soak behavior locally. This test already holds TEST_ENV_LOCK, so use the
    // unlocked helper instead of re-locking via crate::soak_cycles_locked(3).
    for cycle in 0..crate::soak_cycles(3) {
        let worker = workers[cycle % workers.len()];
        let task_id = format!("T-soak-{cycle:02}");
        fixture.write_pending_task(&task_id, &format!("Unattended soak cycle {cycle}"));

        {
            let _env = fixture.actor_env(SUPERVISOR_NAME, "supervisor");
            let assignment = factory
                .execute(json!({
                    "action": "assign_workers",
                    "task_id": task_id,
                    "worker": worker,
                }))
                .await
                .expect("assign worker");
            assert!(
                assignment.is_error.is_none(),
                "assign_workers failed for cycle {cycle}: {}",
                extract_text(&assignment)
            );
        }

        let assigned_task = fixture.read_task(&task_id);
        assert_eq!(assigned_task["status"], "assigned");
        assert_eq!(assigned_task["assignee"], worker);

        let queued_worker_messages = collect_queued_messages(fixture.root(), Some(worker));
        assert!(
            queued_worker_messages
                .iter()
                .any(|message| message.contains(&task_id)),
            "assignment for {task_id} should enqueue a real worker prompt for {worker}: {queued_worker_messages:#?}"
        );
        runtime.service_prompt_and_reset_queues();
        wait_for_gateway_log_contains(fixture.root(), worker, &task_id);
        let worker_session_before_close = runtime
            .mux()
            .get(worker)
            .and_then(|pane| pane.gateway_session_id().map(str::to_string));

        let worker_worktree = fixture
            .worker_worktrees
            .get(worker)
            .expect("worker worktree");
        std::fs::write(
            worker_worktree.join(format!("cycle-{cycle:02}.txt")),
            format!("worker {worker} completed soak cycle {cycle}\n"),
        )
        .expect("write worker change");

        {
            let _env = fixture.actor_env(worker, "worker");
            let complete = task_tool
                .execute(json!({
                    "action": "complete",
                    "id": task_id,
                    "notes": format!("cycle {cycle} implementation complete"),
                    "activity": "testing",
                }))
                .await
                .expect("worker complete");
            assert!(
                complete.is_error.is_none(),
                "complete failed for cycle {cycle}: {}",
                extract_text(&complete)
            );
            let complete_json = parse_result(&complete);
            assert_eq!(complete_json["task_status"], "review_ready");
        }

        let ready_task = fixture.read_task(&task_id);
        assert_eq!(ready_task["status"], "review_ready");
        assert_eq!(ready_task["review_owner"], worker);
        assert_eq!(ready_task["percent"], 100);
        assert!(ready_task["latest_commit"].is_string());

        let (review_id, panel) = {
            let _env = fixture.actor_env(SUPERVISOR_NAME, "supervisor");
            let request = verification
                .execute(json!({
                    "action": "request_review",
                    "task_id": task_id,
                    "requested_by": SUPERVISOR_NAME,
                    "title": format!("Cycle {cycle} unattended validation"),
                    "description": "Lifecycle soak round"
                }))
                .await
                .expect("request review");
            assert!(
                request.is_error.is_none(),
                "request_review failed for cycle {cycle}: {}",
                extract_text(&request)
            );
            let request_json = parse_result(&request);
            let review_id = request_json["review_id"]
                .as_str()
                .expect("review_id")
                .to_string();
            let panel = reviewer_panel(&request_json);
            assert_eq!(panel.len(), 2, "unexpected review panel for cycle {cycle}");
            (review_id, panel)
        };

        for reviewer in &panel {
            let queued_review_messages = collect_queued_messages(fixture.root(), Some(reviewer));
            assert!(
                queued_review_messages
                    .iter()
                    .any(|message| message.contains(&review_id)),
                "review request {review_id} should enqueue a real reviewer prompt for {reviewer}: {queued_review_messages:#?}"
            );
        }
        runtime.service_prompt_and_reset_queues();
        for reviewer in &panel {
            wait_for_gateway_log_contains(fixture.root(), reviewer, &review_id);
        }

        if cycle == 0 {
            let reviewer_to_reset = gateway_reviewers[0].to_string();
            assert!(panel.contains(&reviewer_to_reset));
            let reviewer_session_before_reset = runtime
                .mux()
                .get(&reviewer_to_reset)
                .and_then(|pane| pane.gateway_session_id().map(str::to_string))
                .expect("gateway reviewer session should be live before reset");
            runtime.refresh_dashboard_state();
            let review_context = runtime
                .mux()
                .get(&reviewer_to_reset)
                .expect("reviewer pane")
                .review_context()
                .cloned()
                .expect("expected review context before reset");
            assert_eq!(review_context.review_id, review_id);
            assert_eq!(review_context.task_id, task_id);
            assert_eq!(review_context.round, 1);
            assert_eq!(review_context.panel_total, panel.len());
            assert_eq!(review_context.panel_done, 0);

            let reviewer_reset_request = QueuedReviewerReset {
                task_id: task_id.clone(),
                review_id: review_id.clone(),
                reviewer: reviewer_to_reset.clone(),
                requested_at: Some(Utc::now().to_rfc3339()),
            };
            let reviewer_reset_prompt =
                format!("Brehon reviewer startup. You are reviewer '{reviewer_to_reset}'.");
            let reviewer_reset_prompt_count = gateway_log_occurrence_count(
                fixture.root(),
                &reviewer_to_reset,
                &reviewer_reset_prompt,
            );
            SessionScopedQueue::<QueuedReviewerReset>::new(
                RUNTIME_SESSION_NAME,
                fixture.root().join("runtime").join("reviewer-reset-queue"),
            )
            .enqueue(reviewer_reset_request.clone())
            .expect("enqueue reviewer reset");
            runtime.service_prompt_and_reset_queues();

            assert!(
                runtime
                    .mux()
                    .get(&reviewer_to_reset)
                    .expect("reviewer pane")
                    .review_context()
                    .is_none(),
                "review reset should clear reviewer context"
            );
            assert!(
                reviewer_reset_ack_path(fixture.root(), &reviewer_reset_request).exists(),
                "reviewer reset should persist an acknowledgement"
            );
            let live_sessions_after_reviewer_reset = AgentGateway::list_sessions(
                runtime
                    .mux()
                    .gateway()
                    .expect("gateway should exist after reviewer reset"),
            )
            .await
            .expect("list sessions after reviewer reset");
            assert!(
                !live_sessions_after_reviewer_reset
                    .iter()
                    .any(|session| session.session_id.as_str() == reviewer_session_before_reset),
                "old reviewer gateway session should be gone after reset"
            );
            wait_for_gateway_log_occurrence_at_least(
                fixture.root(),
                &reviewer_to_reset,
                &reviewer_reset_prompt,
                reviewer_reset_prompt_count + 1,
            );
            let reviewer_session_after_reset = runtime
                .mux()
                .get(&reviewer_to_reset)
                .and_then(|pane| pane.gateway_session_id().map(str::to_string))
                .expect("gateway reviewer session should exist after reviewer reset startup");
            assert_ne!(reviewer_session_after_reset, reviewer_session_before_reset);
            runtime.refresh_dashboard_state();
            let recovered_review_context = runtime
                .mux()
                .get(&reviewer_to_reset)
                .expect("reviewer pane after reset recovery")
                .review_context()
                .cloned()
                .expect("reviewer reset should restore active review context");
            assert_eq!(recovered_review_context.review_id, review_id);
            assert_eq!(recovered_review_context.task_id, task_id);
            assert_eq!(recovered_review_context.round, 1);
            assert_eq!(recovered_review_context.panel_total, panel.len());
            assert_eq!(recovered_review_context.panel_done, 0);
            assert_eq!(
                runtime.mux().pending_delayed_prompt_count(),
                0,
                "reviewer reset should drain delayed startup prompts"
            );
            assert_no_pending_gateway_requests(runtime.mux(), "after reviewer reset recovery")
                .await;

            runtime
                .trigger_supervisor_recovery_from_crash_output(
                    SUPERVISOR_NAME,
                    br#"<anonymous> (/bunfs/root/src/entrypoints/cli.js:577:98876)
TypeError: Cannot read properties of undefined"#,
                )
                .expect("trigger supervisor recovery");
            assert_eq!(
                runtime.mux().pending_delayed_prompt_count(),
                0,
                "supervisor recovery should drain delayed startup prompts"
            );
            assert!(
                !runtime
                    .mux()
                    .get(SUPERVISOR_NAME)
                    .expect("supervisor pane exists")
                    .has_exited(),
                "supervisor recovery should restore a live pane"
            );

            {
                let _env = fixture.actor_env(SUPERVISOR_NAME, "supervisor");
                let recovered = verification
                    .execute(json!({
                        "action": "review_status",
                        "task_id": task_id,
                    }))
                    .await
                    .expect("review status after supervisor reset");
                let recovered_json = parse_result(&recovered);
                assert_eq!(recovered_json["review_status"], "collecting");
                assert_eq!(recovered_json["review_id"], review_id);
                assert_eq!(recovered_json["round"], 1);
            }
        }

        for reviewer in &panel {
            let _env = fixture.actor_env(reviewer, "reviewer");
            let submit = verification
                .execute(json!({
                    "action": "submit_review",
                    "review_id": review_id,
                    "reviewer": reviewer,
                    "score": 8,
                    "verdict": "approved",
                    "summary": format!("{reviewer} approved cycle {cycle}"),
                }))
                .await
                .expect("submit review");
            assert!(
                submit.is_error.is_none(),
                "submit_review failed for cycle {cycle} reviewer {reviewer}: {}",
                extract_text(&submit)
            );
        }

        {
            let _env = fixture.actor_env(SUPERVISOR_NAME, "supervisor");
            let approved = verification
                .execute(json!({
                    "action": "review_status",
                    "task_id": task_id,
                }))
                .await
                .expect("final review status");
            let approved_json = parse_result(&approved);
            assert_eq!(approved_json["review_status"], "approved");
            assert_eq!(approved_json["progress"], "2/2");

            let close = task_tool
                .execute(json!({
                    "action": "close",
                    "id": task_id,
                    "agent_name": SUPERVISOR_NAME,
                    "role": "supervisor",
                    "supervisor": SUPERVISOR_NAME,
                }))
                .await
                .expect("close task");
            assert!(
                close.is_error.is_none(),
                "close failed for cycle {cycle}: {}",
                extract_text(&close)
            );
            let close_json = parse_result(&close);
            assert_eq!(
                close_json.get("action").and_then(Value::as_str),
                Some("closed"),
                "unexpected close payload for cycle {cycle}: {close_json}"
            );
            assert_eq!(close_json["worker_recycle_queued"], true);
        }

        let recycle_request = QueuedWorkerRecycle {
            task_id: task_id.clone(),
            worker: worker.to_string(),
            requested_at: None,
        };
        let reviewer_reset_queue = SessionScopedQueue::<QueuedReviewerReset>::new(
            RUNTIME_SESSION_NAME,
            fixture.root().join("runtime").join("reviewer-reset-queue"),
        );
        for reviewer in &panel {
            reviewer_reset_queue
                .enqueue(QueuedReviewerReset {
                    task_id: task_id.clone(),
                    review_id: review_id.clone(),
                    reviewer: reviewer.clone(),
                    requested_at: Some(Utc::now().to_rfc3339()),
                })
                .expect("enqueue reviewer reset after completed review");
        }
        runtime.service_prompt_and_reset_queues();
        assert!(
            worker_recycle_ack_path(fixture.root(), &recycle_request).exists(),
            "worker recycle should persist an acknowledgement"
        );
        let worker_recovery_prompt =
            format!("Recovered worker session continue after recycle {task_id}");
        SessionScopedQueue::<PromptQueueEntry>::new(
            RUNTIME_SESSION_NAME,
            fixture.root().join("runtime").join("prompt-queue"),
        )
        .enqueue(PromptQueueEntry::new(
            worker,
            Some(SUPERVISOR_NAME),
            &worker_recovery_prompt,
        ))
        .expect("enqueue worker recovery prompt");
        runtime.service_prompt_and_reset_queues();
        wait_for_gateway_log_contains(fixture.root(), worker, &worker_recovery_prompt);
        let recycled_worker_session = runtime
            .mux()
            .get(worker)
            .and_then(|pane| pane.gateway_session_id().map(str::to_string))
            .expect("gateway worker should exist after recovery prompt");
        let prior_worker_session = worker_session_before_close
            .expect("gateway worker should have had a session before recycle");
        assert_ne!(recycled_worker_session, prior_worker_session);

        let closed_task = fixture.read_task(&task_id);
        assert_eq!(closed_task["status"], "closed");
        assert_eq!(closed_task["closed_by"], SUPERVISOR_NAME);

        assert_shared_root_clean(fixture.workspace_path());
        assert_no_hidden_approval_prompts(fixture.root(), &format!("after cycle {cycle}"));
        assert_no_pending_gateway_requests(runtime.mux(), &format!("after cycle {cycle}")).await;
    }

    let live_before_shutdown = AgentGateway::list_sessions(
        runtime
            .mux()
            .gateway()
            .expect("gateway should exist before shutdown"),
    )
    .await
    .expect("list sessions before shutdown");
    assert!(
        !live_before_shutdown.is_empty(),
        "soak should leave active gateway sessions to validate shutdown cleanup"
    );

    runtime.shutdown_all().await;

    let live_after_shutdown = AgentGateway::list_sessions(
        runtime
            .mux()
            .gateway()
            .expect("gateway should remain addressable after shutdown"),
    )
    .await
    .expect("list sessions after shutdown");
    assert!(
        live_after_shutdown.is_empty(),
        "shutdown should terminate all live gateway sessions"
    );
    assert_shared_root_clean(fixture.workspace_path());
    assert_no_hidden_approval_prompts(fixture.root(), "after shutdown");
    assert_no_pending_gateway_requests(runtime.mux(), "after shutdown").await;
}
