//! PTY management using portable-pty
//!
//! Provides a wrapper around portable-pty with:
//! - Async read/write operations
//! - Raw byte output (terminal parsing done by ghostty_vt)
//! - Resize support

mod agents;
pub(crate) mod config;
mod core;
mod dump;
pub(crate) mod filesystem;
pub(crate) mod prompts;

pub use config::{PtyConfig, TeamsSpawnConfig};
pub use core::{Pty, PtyEvent, format_cursor_position_report};
pub use prompts::{
    build_advisor_startup_prompt, build_research_startup_prompt, build_reviewer_startup_prompt,
    build_supervisor_startup_prompt, build_worker_startup_prompt,
};

#[cfg(test)]
mod tests {
    use crate::pty::agents::copilot::{
        desired_copilot_mcp_config, prepare_local_copilot_runtime_with_global_config,
    };
    use crate::pty::agents::current_brehon_exe;
    use crate::pty::agents::gemini::{
        gemini_builtin_skill_names_for_role, prepare_local_gemini_home,
    };
    use crate::pty::agents::kimi::{
        desired_kimi_mcp_config, prepare_local_kimi_runtime_with_global_share,
    };
    use crate::pty::agents::opencode::{
        OPENCODE_MCP_TIMEOUT_MS, prepare_local_opencode_runtime,
        prepare_local_opencode_runtime_with_global_config,
    };
    use crate::pty::core::{descendant_process_ids_from_snapshot, kill_child};
    use crate::pty::*;
    use portable_pty::{Child, ChildKiller, ExitStatus};
    use std::collections::VecDeque;
    use std::io;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex, OnceLock};

    fn fresh_temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn setup_fake_linked_worktree(prefix: &str) -> (PathBuf, PathBuf) {
        let workdir = fresh_temp_dir(prefix);
        let gitdir = fresh_temp_dir(&format!("{prefix}-gitdir"));
        std::fs::write(
            workdir.join(".git"),
            format!("gitdir: {}\n", gitdir.to_string_lossy()),
        )
        .unwrap();
        (workdir, gitdir)
    }

    fn set_brehon_session_name_for_test(session_name: &str) -> impl Drop {
        struct Guard {
            _lock: std::sync::MutexGuard<'static, ()>,
            previous: Option<String>,
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                if let Some(previous) = self.previous.take() {
                    unsafe {
                        std::env::set_var("BREHON_SESSION_NAME", previous);
                    }
                } else {
                    unsafe {
                        std::env::remove_var("BREHON_SESSION_NAME");
                    }
                }
            }
        }

        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        let mutex = LOCK.get_or_init(|| StdMutex::new(()));
        let lock = mutex.lock().expect("session env lock poisoned");
        let previous = std::env::var("BREHON_SESSION_NAME").ok();
        unsafe {
            std::env::set_var("BREHON_SESSION_NAME", session_name);
        }
        Guard {
            _lock: lock,
            previous,
        }
    }

    #[derive(Clone, Debug)]
    enum TryWaitResult {
        Running,
        Exited(u32),
    }

    #[derive(Debug)]
    struct FakeChildState {
        try_wait_results: VecDeque<TryWaitResult>,
        kill_calls: usize,
        wait_calls: usize,
    }

    #[derive(Clone, Debug)]
    struct FakeChild {
        state: Arc<StdMutex<FakeChildState>>,
    }

    impl FakeChild {
        fn new(try_wait_results: Vec<TryWaitResult>) -> (Self, Arc<StdMutex<FakeChildState>>) {
            let state = Arc::new(StdMutex::new(FakeChildState {
                try_wait_results: try_wait_results.into(),
                kill_calls: 0,
                wait_calls: 0,
            }));
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl ChildKiller for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            self.state.lock().expect("state lock poisoned").kill_calls += 1;
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl Child for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            let mut state = self.state.lock().expect("state lock poisoned");
            let next = state
                .try_wait_results
                .pop_front()
                .unwrap_or(TryWaitResult::Running);
            match next {
                TryWaitResult::Running => Ok(None),
                TryWaitResult::Exited(code) => Ok(Some(ExitStatus::with_exit_code(code))),
            }
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            self.state.lock().expect("state lock poisoned").wait_calls += 1;
            Ok(ExitStatus::with_signal("SIGKILL"))
        }

        fn process_id(&self) -> Option<u32> {
            Some(42)
        }
    }

    #[test]
    fn test_kill_child_reaps_after_kill() {
        struct Case {
            name: &'static str,
            try_wait_results: Vec<TryWaitResult>,
            expected_kill_calls: usize,
            expected_wait_calls: usize,
        }

        let cases = [
            Case {
                name: "running child is killed then reaped",
                try_wait_results: vec![TryWaitResult::Running],
                expected_kill_calls: 1,
                expected_wait_calls: 1,
            },
            Case {
                name: "already exited child is not killed or reaped again",
                try_wait_results: vec![TryWaitResult::Exited(0)],
                expected_kill_calls: 0,
                expected_wait_calls: 0,
            },
        ];

        for case in cases {
            let (mut child, state) = FakeChild::new(case.try_wait_results);
            kill_child(&mut child).expect(case.name);
            let state = state.lock().expect("state lock poisoned");
            assert_eq!(state.kill_calls, case.expected_kill_calls, "{}", case.name);
            assert_eq!(state.wait_calls, case.expected_wait_calls, "{}", case.name);
        }
    }

    #[test]
    fn test_descendant_process_ids_from_snapshot_collects_entire_subtree() {
        let snapshot = "\
100 1
110 100
120 100
121 120
130 121
140 999
";

        let descendants = descendant_process_ids_from_snapshot(snapshot, 100);
        assert_eq!(descendants.len(), 4);
        assert!(descendants.contains(&110));
        assert!(descendants.contains(&120));
        assert!(descendants.contains(&121));
        assert!(descendants.contains(&130));
        assert!(!descendants.contains(&140));
    }

    #[tokio::test]
    async fn test_pty_config_default() {
        let config = PtyConfig::default();
        assert_eq!(config.command, "bash");
        assert_eq!(config.rows, 24);
        assert_eq!(config.cols, 80);
    }

    #[tokio::test]
    async fn test_pty_config_claude() {
        let config = PtyConfig::claude(
            "test-agent",
            "worker",
            None,
            None,
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(config.command, "claude");
        assert!(
            config
                .args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "test-agent")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_ROLE" && v == "worker")
        );
        // No BREHON_ROOT when not provided
        assert!(!config.env.iter().any(|(k, _)| k == "BREHON_ROOT"));
    }

    #[tokio::test]
    async fn test_pty_config_claude_with_brehon_root() {
        let brehon_root = PathBuf::from("/home/user/project/.brehon");
        let config = PtyConfig::claude(
            "test-agent",
            "worker",
            None,
            None,
            PathBuf::from("/tmp"),
            Some(&brehon_root),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_ROOT" && v == "/home/user/project/.brehon")
        );
    }

    #[tokio::test]
    async fn test_pty_config_claude_with_supervisor() {
        let config = PtyConfig::claude(
            "test-worker",
            "worker",
            None,
            None,
            PathBuf::from("/tmp"),
            None,
            Some("test-supervisor"),
            None,
            None,
            None,
            None,
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "test-supervisor")
        );
    }

    #[tokio::test]
    async fn test_pty_config_sets_clone_path() {
        let config = PtyConfig::claude(
            "test-worker",
            "worker",
            None,
            None,
            PathBuf::from("/tmp/worktree"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_CLONE_PATH" && v == "/tmp/worktree")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_WORKSPACE_ROOT" && v == "/tmp/worktree")
        );
    }

    #[tokio::test]
    async fn test_pty_config_claude_with_model() {
        let config = PtyConfig::claude(
            "test-agent",
            "worker",
            None,
            None,
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            Some("claude-opus-4-6"),
            None,
            None,
        );
        assert!(config.args.contains(&"--model".to_string()));
        assert!(config.args.contains(&"claude-opus-4-6".to_string()));
    }

    #[tokio::test]
    async fn test_pty_config_claude_with_reasoning_effort_override() {
        let config = PtyConfig::claude(
            "test-agent",
            "worker",
            None,
            None,
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            Some("high"),
        );
        assert!(config.args.contains(&"--effort".to_string()));
        assert!(config.args.contains(&"high".to_string()));
        assert!(!config.args.contains(&"medium".to_string()));
    }

    #[tokio::test]
    async fn test_pty_config_claude_without_model() {
        let config = PtyConfig::claude(
            "test-agent",
            "worker",
            None,
            None,
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!config.args.contains(&"--model".to_string()));
    }

    #[tokio::test]
    async fn test_pty_config_claude_preserves_agent_type_alias() {
        let config = PtyConfig::claude(
            "test-reviewer",
            "reviewer",
            Some("claude-reviewer"),
            None,
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "claude-reviewer")
        );
    }

    #[tokio::test]
    async fn test_pty_config_codex_with_model() {
        let workdir = fresh_temp_dir("brehon-codex-home");
        let config = PtyConfig::codex(
            "test-agent",
            "supervisor",
            workdir.clone(),
            None,
            None,
            None,
            Some("gpt-5.3-codex"),
            None,
            None,
        );
        assert!(config.args.contains(&"--model".to_string()));
        assert!(config.args.contains(&"gpt-5.3-codex".to_string()));
        let codex_home = workdir.join(".brehon/factory-runtime/codex/home");
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| { k == "CODEX_HOME" && v == codex_home.to_string_lossy().as_ref() })
        );
        let codex_config = std::fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(codex_config.contains("[mcp_servers.brehon]"));
        assert!(codex_config.contains("[mcp_servers.brehon.env]"));
        assert!(codex_config.contains("BREHON_AGENT_NAME = \"test-agent\""));
        assert!(codex_config.contains("BREHON_AGENT_ROLE = \"supervisor\""));
        assert!(codex_config.contains("BREHON_AGENT_TYPE = \"codex\""));
        assert!(!codex_config.contains("[mcp_servers.pantheon]"));
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_codex_remote_worker() {
        let workdir = fresh_temp_dir("brehon-codex-remote");
        let config = PtyConfig::codex_remote(
            "test-agent",
            "worker",
            workdir.clone(),
            None,
            Some("supervisor"),
            Some("codex"),
            Some("gpt-5.4"),
            None,
            "ws://127.0.0.1:4242",
            None,
        );
        assert_eq!(config.command, "sh");
        assert_eq!(config.args.first().map(String::as_str), Some("-c"));
        let script = config.args.get(1).expect("wrapper script");
        assert!(script.contains("remote-ready"));
        assert!(script.contains("--remote"));
        assert!(script.contains("ws://127.0.0.1:4242"));
        assert!(script.contains("trust_level"));
        assert!(script.contains("--model"));
        assert!(script.contains("gpt-5.4"));
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_claude_with_teams() {
        let teams = TeamsSpawnConfig {
            team_name: "test-team".to_string(),
            agent_id: "worker-1@test-team".to_string(),
            agent_name: "worker-1".to_string(),
            agent_color: "blue".to_string(),
            agent_type: "general-purpose".to_string(),
            parent_session_id: Some("lead-session-123".to_string()),
        };
        let config = PtyConfig::claude(
            "worker-1",
            "worker",
            None,
            None,
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            Some(&teams),
            None,
        );
        assert!(config.args.contains(&"--team-name".to_string()));
        assert!(config.args.contains(&"test-team".to_string()));
        assert!(config.args.contains(&"--agent-id".to_string()));
        assert!(config.args.contains(&"worker-1@test-team".to_string()));
        assert!(config.args.contains(&"--agent-name".to_string()));
        assert!(config.args.contains(&"--agent-color".to_string()));
        assert!(config.args.contains(&"--teammate-mode".to_string()));
        assert!(config.args.contains(&"tmux".to_string()));
        assert!(config.args.contains(&"--parent-session-id".to_string()));
        assert!(config.args.contains(&"lead-session-123".to_string()));
        // Workers should NOT have --session-id
        assert!(!config.args.contains(&"--session-id".to_string()));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS" && v == "1")
        );
    }

    #[tokio::test]
    async fn test_pty_config_claude_with_teams_lead() {
        let teams = TeamsSpawnConfig {
            team_name: "test-team".to_string(),
            agent_id: "supervisor@test-team".to_string(),
            agent_name: "supervisor".to_string(),
            agent_color: "green".to_string(),
            agent_type: "team-lead".to_string(),
            parent_session_id: None,
        };
        let config = PtyConfig::claude(
            "supervisor",
            "supervisor",
            None,
            None,
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            Some(&teams),
            None,
        );
        // Lead also gets --teammate-mode so it polls its inbox
        assert!(config.args.contains(&"--teammate-mode".to_string()));
        assert!(config.args.contains(&"tmux".to_string()));
        // No --parent-session-id for the lead
        assert!(!config.args.contains(&"--parent-session-id".to_string()));
    }

    #[tokio::test]
    async fn test_pty_config_gemini_worker() {
        let workdir = fresh_temp_dir("brehon-gemini-worker");
        let config = PtyConfig::gemini(
            "test-worker",
            "worker",
            workdir.clone(),
            None,
            Some("test-supervisor"),
            None,
            Some("pro"),
            None,
            None,
        );
        assert_eq!(config.command, "gemini");
        assert!(config.args.contains(&"--approval-mode".to_string()));
        assert!(config.args.contains(&"--sandbox".to_string()));
        assert!(config.args.contains(&"false".to_string()));
        assert!(config.args.contains(&"--model".to_string()));
        assert!(config.args.contains(&"pro".to_string()));
        assert!(config.args.contains(&"-i".to_string()));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "test-worker")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "GEMINI_SANDBOX" && v == "false")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "test-supervisor")
        );
        assert!(config.env.iter().any(|(k, v)| {
            k == "HOME"
                && v == workdir
                    .join(".brehon/factory-runtime/gemini/home")
                    .to_string_lossy()
                    .as_ref()
        }));
        let settings_path =
            workdir.join(".brehon/factory-runtime/gemini/home/.gemini/settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["mcpServers"],
            serde_json::json!({
                "brehon": {
                    "command": current_brehon_exe(),
                    "args": ["serve"],
                    "trust": true,
                }
            })
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_gemini_supervisor() {
        let workdir = fresh_temp_dir("brehon-gemini-supervisor");
        let config = PtyConfig::gemini(
            "supervisor",
            "supervisor",
            workdir.clone(),
            None,
            None,
            Some("gemini"),
            None,
            None,
            None,
        );
        assert_eq!(config.command, "gemini");
        assert!(config.args.contains(&"--approval-mode".to_string()));
        assert!(config.args.contains(&"-i".to_string()));
        assert!(config.args.iter().any(|arg| {
            arg.contains("action=session_start name=supervisor agent_type=supervisor")
        }));
        // Should not contain --model when not specified
        assert!(!config.args.contains(&"--model".to_string()));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_FACTORY_WORKER_CLI" && v == "gemini")
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_gemini_acp_reviewer_strips_embedded_startup_prompt() {
        let workdir = fresh_temp_dir("brehon-gemini-acp-reviewer");
        let config = PtyConfig::gemini_acp(
            "reviewer-1",
            "reviewer",
            workdir.clone(),
            None,
            None,
            None,
            Some("pro"),
            Some("medium"),
        );
        assert_eq!(config.command, "gemini");
        assert_eq!(config.args.first().map(String::as_str), Some("--acp"));
        assert!(config.args.contains(&"--model".to_string()));
        assert!(config.args.contains(&"pro".to_string()));
        assert!(!config.args.contains(&"--thinking-budget".to_string()));
        let settings_path =
            workdir.join(".brehon/factory-runtime/gemini/home/.gemini/settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "MEDIUM"
        );
        assert!(!config.args.contains(&"-i".to_string()));
        assert!(
            !config
                .args
                .iter()
                .any(|arg| arg.contains("reviewer=reviewer-1"))
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_gemini_with_reasoning_effort() {
        let workdir = fresh_temp_dir("brehon-gemini-effort");
        let config = PtyConfig::gemini(
            "gemini-worker",
            "worker",
            workdir.clone(),
            None,
            Some("supervisor"),
            None,
            None,
            None,
            Some("medium"),
        );
        assert!(!config.args.contains(&"--thinking-budget".to_string()));
        let settings_path =
            workdir.join(".brehon/factory-runtime/gemini/home/.gemini/settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "MEDIUM"
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_gemini_without_reasoning_effort() {
        let workdir = fresh_temp_dir("brehon-gemini-no-effort");
        let config = PtyConfig::gemini(
            "gemini-worker",
            "worker",
            workdir.clone(),
            None,
            Some("supervisor"),
            None,
            None,
            None,
            None,
        );
        assert!(!config.args.contains(&"--thinking-budget".to_string()));
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_opencode_worker() {
        let workdir = fresh_temp_dir("brehon-opencode-worker");
        let config = PtyConfig::opencode(
            "oc-worker",
            "worker",
            workdir.clone(),
            None,
            Some("test-supervisor"),
            None,
            Some("openai/gpt-4.1"),
            Some("high"),
            None,
        );
        assert_eq!(config.command, "opencode");
        assert!(config.args.contains(&"-m".to_string()));
        assert!(config.args.contains(&"openai/gpt-4.1".to_string()));
        assert!(!config.args.contains(&"--variant".to_string()));
        assert!(!config.args.contains(&"high".to_string()));
        assert!(config.args.contains(&"--prompt".to_string()));
        assert!(
            config
                .args
                .iter()
                .any(|arg| {
                    arg.contains(
                        "Do NOT proactively call `mcp_brehon_agent action=session_start` or `mcp_brehon_agent action=whoami` during idle startup"
                    )
                })
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "oc-worker")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "test-supervisor")
        );
        assert!(config.env.iter().any(|(k, v)| {
            k == "XDG_CONFIG_HOME"
                && v == workdir
                    .join(".brehon/factory-runtime/opencode/xdg")
                    .to_string_lossy()
                    .as_ref()
        }));
        let local_config_path =
            workdir.join(".brehon/factory-runtime/opencode/xdg/opencode/opencode.json");
        let local_config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(local_config_path).unwrap()).unwrap();
        assert_eq!(
            local_config["mcp"],
            serde_json::json!({
                "brehon": {
                    "type": "local",
                    "command": [current_brehon_exe(), "serve"],
                    "enabled": true,
                }
            })
        );
        assert_eq!(local_config["model"], "openai/gpt-4.1");
        assert_eq!(local_config["agent"]["build"]["model"], "openai/gpt-4.1");
        assert_eq!(
            local_config["provider"]["openai"]["models"]["gpt-4.1"]["options"]["reasoningEffort"],
            "high"
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_opencode_supervisor() {
        let workdir = fresh_temp_dir("brehon-opencode-supervisor");
        let config = PtyConfig::opencode(
            "supervisor",
            "supervisor",
            workdir.clone(),
            None,
            None,
            Some("opencode"),
            None,
            None,
            None,
        );
        assert_eq!(config.command, "opencode");
        assert!(!config.args.contains(&"-m".to_string()));
        assert!(config.args.contains(&"--prompt".to_string()));
        assert!(
            config
                .args
                .iter()
                .any(|arg| arg
                    .contains("action=session_start name=supervisor agent_type=supervisor"))
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_FACTORY_WORKER_CLI" && v == "opencode")
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_opencode_server_backed_worker() {
        let workdir = fresh_temp_dir("brehon-opencode-server-backed");
        let config = PtyConfig::opencode_server_backed(
            "oc-worker",
            "worker",
            workdir.clone(),
            None,
            Some("test-supervisor"),
            None,
            Some("google/gemini-3.1-pro-preview"),
            Some("medium"),
            43123,
            None,
        );
        assert_eq!(config.command, "zsh");
        assert_eq!(config.args.first().map(String::as_str), Some("-lc"));
        let script = config.args.get(1).expect("attach script should be present");
        assert!(script.contains("opencode attach"));
        assert!(script.contains("http://127.0.0.1:43123"));
        assert!(script.contains("--session"));
        assert!(script.contains("session-id"));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "test-supervisor")
        );
        assert!(config.env.iter().any(|(k, _)| k == "XDG_STATE_HOME"));
        assert!(config.env.iter().any(|(k, _)| k == "XDG_DATA_HOME"));
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_opencode_server_backed_supervisor() {
        let workdir = fresh_temp_dir("brehon-opencode-supervisor-server-backed");
        let config = PtyConfig::opencode_server_backed(
            "supervisor",
            "supervisor",
            workdir.clone(),
            None,
            None,
            Some("opencode"),
            Some("google/gemini-3.1-pro-preview"),
            None,
            43124,
            None,
        );
        assert_eq!(config.command, "zsh");
        assert_eq!(config.args.first().map(String::as_str), Some("-lc"));
        let script = config.args.get(1).expect("attach script should be present");
        assert!(script.contains("opencode attach"));
        assert!(script.contains("http://127.0.0.1:43124"));
        assert!(script.contains("--session"));
        assert!(script.contains("session-id"));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_FACTORY_WORKER_CLI" && v == "opencode")
        );
        assert!(config.env.iter().any(|(k, _)| k == "XDG_STATE_HOME"));
        assert!(config.env.iter().any(|(k, _)| k == "XDG_DATA_HOME"));
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_opencode_headless_server_worker() {
        let workdir = fresh_temp_dir("brehon-opencode-headless-server");
        let config = PtyConfig::opencode_headless_server(
            "oc-worker",
            "worker",
            workdir.clone(),
            None,
            Some("test-supervisor"),
            None,
            Some("google/gemini-3.1-pro-preview"),
            Some("medium"),
            43125,
            None,
        );
        assert_eq!(config.command, "opencode");
        assert_eq!(
            config.args,
            vec![
                "serve".to_string(),
                "--hostname".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "43125".to_string(),
            ]
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "test-supervisor")
        );
        assert!(config.env.iter().any(|(k, _)| k == "XDG_STATE_HOME"));
        assert!(config.env.iter().any(|(k, _)| k == "XDG_DATA_HOME"));
        assert!(
            config.env.iter().any(|(k, v)| {
                k == "BREHON_OPENCODE_SERVER_URL" && v == "http://127.0.0.1:43125"
            })
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_opencode_acp_worker() {
        let workdir = fresh_temp_dir("brehon-opencode-acp");
        let config = PtyConfig::opencode_acp(
            "oc-worker",
            "worker",
            workdir.clone(),
            None,
            Some("test-supervisor"),
            None,
            Some("google/gemini-3.1-pro-preview"),
            Some("medium"),
        );
        assert_eq!(config.command, "opencode");
        assert_eq!(config.args.first().map(String::as_str), Some("acp"));
        assert!(!config.args.contains(&"-m".to_string()));
        assert!(config.args.contains(&"acp".to_string()));
        assert!(config.args.contains(&"--cwd".to_string()));
        assert!(!config.args.contains(&"--hostname".to_string()));
        assert!(!config.args.contains(&"--port".to_string()));
        assert!(!config.env.iter().any(|(k, _)| k == "HOME"));
        assert!(config.env.iter().any(|(k, v)| {
            k == "XDG_CONFIG_HOME"
                && v == workdir
                    .join(".brehon/factory-runtime/opencode/xdg")
                    .to_string_lossy()
                    .as_ref()
        }));
        assert!(
            config
                .env
                .iter()
                .any(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_MODEL" && v == "google/gemini-3.1-pro-preview")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_REASONING_EFFORT" && v == "medium")
        );
        assert!(
            !config
                .args
                .iter()
                .any(|arg| arg.contains("Brehon worker startup"))
        );
        let local_config_path =
            workdir.join(".brehon/factory-runtime/opencode/xdg/opencode/opencode.json");
        let local_config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(local_config_path).unwrap()).unwrap();
        assert_eq!(
            local_config["mcp"],
            serde_json::json!({
                "brehon": {
                    "type": "local",
                    "command": [current_brehon_exe(), "serve"],
                    "enabled": true,
                }
            })
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_custom_acp_worker() {
        let workdir = fresh_temp_dir("brehon-custom-acp");
        let brehon_root = workdir.join(".brehon");
        let config = PtyConfig::custom_acp(
            "custom-worker",
            "worker",
            "goose",
            &["acp".to_string(), "--stdio".to_string()],
            Some("goose-worker"),
            workdir.clone(),
            Some(&brehon_root),
            Some("supervisor"),
            Some("goose"),
        );

        assert_eq!(config.command, "goose");
        assert_eq!(config.args, vec!["acp".to_string(), "--stdio".to_string()]);
        assert_eq!(config.cwd, Some(workdir.clone()));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "custom-worker")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_ROLE" && v == "worker")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "goose-worker")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "supervisor")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_FACTORY_WORKER_CLI" && v == "goose")
        );
        assert!(
            config.env.iter().any(|(k, v)| {
                k == "BREHON_ROOT" && v == brehon_root.to_string_lossy().as_ref()
            })
        );

        let _ = std::fs::remove_dir_all(workdir);
    }

    #[test]
    fn test_prepare_local_opencode_runtime_writes_local_mcp_block() {
        let temp_dir = fresh_temp_dir("brehon-opencode-runtime");
        let (xdg_root, content) =
            prepare_local_opencode_runtime(&temp_dir, None, "/tmp/brehon", None, None).unwrap();
        let config_path = xdg_root.join("opencode/opencode.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
        assert_eq!(
            config["mcp"],
            serde_json::json!({
                "brehon": {
                    "type": "local",
                    "command": ["/tmp/brehon", "serve"],
                    "enabled": true,
                }
            })
        );
        assert_eq!(
            config["experimental"]["mcp_timeout"],
            OPENCODE_MCP_TIMEOUT_MS
        );
        let content_json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(content_json["mcp"], config["mcp"]);
        assert_eq!(
            content_json["experimental"]["mcp_timeout"],
            OPENCODE_MCP_TIMEOUT_MS
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_prepare_local_opencode_runtime_preserves_provider_config_and_plugins() {
        let temp_dir = fresh_temp_dir("brehon-opencode-runtime");
        let global_root = fresh_temp_dir("brehon-opencode-global");
        let global_config_dir = global_root.join("opencode");
        std::fs::create_dir_all(&global_config_dir).unwrap();
        std::fs::create_dir_all(global_config_dir.join("command")).unwrap();
        std::fs::create_dir_all(global_config_dir.join("agents")).unwrap();
        std::fs::write(
            global_config_dir.join("opencode.json"),
            r#"{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["opencode-gemini-auth@latest"],
  "provider": {
    "google": {
      "models": {
        "gemini-3.1-pro-preview": { "name": "Gemini 3.1 Pro Preview" }
      }
    }
  }
}"#,
        )
        .unwrap();
        std::fs::write(
            global_config_dir.join("command/custom.md"),
            "This should not leak into Brehon workers.\n",
        )
        .unwrap();
        std::fs::write(
            global_config_dir.join("agents/helper.md"),
            "This should not leak into Brehon workers.\n",
        )
        .unwrap();

        let (xdg_root, _) = prepare_local_opencode_runtime_with_global_config(
            &temp_dir,
            "/tmp/brehon",
            Some(&global_config_dir),
            None,
            None,
            None,
        )
        .unwrap();
        let config_path = xdg_root.join("opencode/opencode.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();

        assert_eq!(
            config["plugin"],
            serde_json::json!(["opencode-gemini-auth@latest"])
        );
        assert_eq!(
            config["provider"]["google"]["models"]["gemini-3.1-pro-preview"]["name"],
            "Gemini 3.1 Pro Preview"
        );
        assert_eq!(
            config["mcp"],
            serde_json::json!({
                "brehon": {
                    "type": "local",
                    "command": ["/tmp/brehon", "serve"],
                    "enabled": true,
                }
            })
        );
        assert_eq!(
            config["experimental"]["mcp_timeout"],
            OPENCODE_MCP_TIMEOUT_MS
        );
        assert!(
            !xdg_root.join("opencode/command/custom.md").exists(),
            "global slash commands must not be copied into Brehon workers"
        );
        assert!(
            !xdg_root.join("opencode/agents/helper.md").exists(),
            "global agents must not be copied into Brehon workers"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::remove_dir_all(&global_root);
    }

    #[test]
    fn test_prepare_local_opencode_runtime_seeds_global_auth_state() {
        let temp_dir = fresh_temp_dir("brehon-opencode-runtime");
        let global_config_root = fresh_temp_dir("brehon-opencode-global-config");
        let global_data_root = fresh_temp_dir("brehon-opencode-global-data");
        let global_config_dir = global_config_root.join("opencode");
        let global_data_dir = global_data_root.join("opencode");
        std::fs::create_dir_all(&global_config_dir).unwrap();
        std::fs::create_dir_all(&global_data_dir).unwrap();
        std::fs::write(global_config_dir.join("opencode.json"), "{}").unwrap();
        std::fs::write(
            global_data_dir.join("auth.json"),
            r#"{"google":{"type":"oauth","token":"secret"}}"#,
        )
        .unwrap();

        let previous_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", &global_data_root);
        }

        let result = prepare_local_opencode_runtime_with_global_config(
            &temp_dir,
            "/tmp/brehon",
            Some(&global_config_dir),
            None,
            None,
            None,
        )
        .unwrap();

        match previous_xdg_data_home {
            Some(value) => unsafe {
                std::env::set_var("XDG_DATA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("XDG_DATA_HOME");
            },
        }

        let (xdg_root, _) = result;
        let auth_path = xdg_root.join("data/opencode/auth.json");
        assert!(auth_path.exists(), "expected auth.json to be mirrored");
        let auth_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(auth_path).unwrap()).unwrap();
        assert_eq!(auth_json["google"]["type"], "oauth");

        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::remove_dir_all(&global_config_root);
        let _ = std::fs::remove_dir_all(&global_data_root);
    }

    #[test]
    fn test_prepare_local_opencode_runtime_applies_model_and_reasoning_to_config() {
        let temp_dir = fresh_temp_dir("brehon-opencode-runtime");
        let (xdg_root, _) = prepare_local_opencode_runtime(
            &temp_dir,
            None,
            "/tmp/brehon",
            Some("google/gemini-3.1-pro-preview"),
            Some("high"),
        )
        .unwrap();
        let config_path = xdg_root.join("opencode/opencode.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();

        assert_eq!(config["model"], "google/gemini-3.1-pro-preview");
        assert_eq!(
            config["agent"]["build"]["model"],
            "google/gemini-3.1-pro-preview"
        );
        assert_eq!(
            config["provider"]["google"]["models"]["gemini-3.1-pro-preview"]["options"]["reasoningEffort"],
            "high"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_prepare_local_opencode_runtime_maps_reasoning_to_build_variant_when_model_supports_variants()
     {
        let temp_dir = fresh_temp_dir("brehon-opencode-runtime");
        let global_root = fresh_temp_dir("brehon-opencode-global");
        let global_config_dir = global_root.join("opencode");
        std::fs::create_dir_all(&global_config_dir).unwrap();
        std::fs::write(
            global_config_dir.join("opencode.json"),
            r#"{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "kimi-for-coding-oauth": {
      "models": {
        "kimi-for-coding": {
          "name": "k2.6",
          "reasoning": true,
          "options": {},
          "variants": {
            "off": { "reasoning_effort": "off" },
            "auto": { "reasoning_effort": "auto" },
            "low": { "reasoning_effort": "low" },
            "medium": { "reasoning_effort": "medium" },
            "high": { "reasoning_effort": "high" }
          }
        }
      }
    }
  }
}"#,
        )
        .unwrap();

        let (xdg_root, _) = prepare_local_opencode_runtime_with_global_config(
            &temp_dir,
            "/tmp/brehon",
            Some(&global_config_dir),
            None,
            Some("kimi-for-coding-oauth/kimi-for-coding"),
            Some("high"),
        )
        .unwrap();
        let config_path = xdg_root.join("opencode/opencode.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();

        assert_eq!(config["model"], "kimi-for-coding-oauth/kimi-for-coding");
        assert_eq!(
            config["agent"]["build"]["model"],
            "kimi-for-coding-oauth/kimi-for-coding"
        );
        assert_eq!(config["agent"]["build"]["variant"], "high");
        assert!(
            config["provider"]["kimi-for-coding-oauth"]["models"]["kimi-for-coding"]["options"]
                .get("reasoningEffort")
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::remove_dir_all(&global_root);
    }

    #[test]
    fn test_prepare_local_opencode_runtime_limits_factory_permissions_to_worktree_and_gitdir() {
        let project_root = fresh_temp_dir("brehon-opencode-project");
        let worker_cwd = project_root.join(".brehon/worktrees/opencode-1");
        let gitdir = project_root.join(".git/worktrees/opencode-1");
        std::fs::create_dir_all(&worker_cwd).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(
            worker_cwd.join(".git"),
            format!("gitdir: {}\n", gitdir.to_string_lossy()),
        )
        .unwrap();

        let (xdg_root, _) = prepare_local_opencode_runtime(
            &worker_cwd,
            Some(&project_root),
            "/tmp/brehon",
            None,
            None,
        )
        .unwrap();
        let config_path = xdg_root.join("opencode/opencode.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();

        let canonical_project_root = std::fs::canonicalize(&project_root).unwrap();
        let canonical_worker_cwd = std::fs::canonicalize(&worker_cwd).unwrap();
        let canonical_gitdir = std::fs::canonicalize(&gitdir).unwrap();
        let worker_pattern = format!("{}/*", canonical_worker_cwd.to_string_lossy());
        let gitdir_pattern = format!("{}/*", canonical_gitdir.to_string_lossy());
        assert_eq!(
            config["permission"]["external_directory"]["*"], "deny",
            "non-interactive factory workers must fail closed outside the worktree"
        );
        assert_eq!(
            config["permission"]["doom_loop"], "deny",
            "ACP workers cannot answer doom_loop permission prompts"
        );
        assert_eq!(
            config["permission"]["read"]["*.env"], "deny",
            "ACP workers cannot answer env-read permission prompts"
        );
        assert_eq!(
            config["permission"]["read"]["*.env.*"], "deny",
            "ACP workers cannot answer env-read permission prompts"
        );
        assert_eq!(
            config["permission"]["external_directory"][&worker_pattern],
            "allow"
        );
        assert_eq!(config["permission"]["read"][&worker_pattern], "allow");
        assert_eq!(
            config["permission"]["external_directory"][&gitdir_pattern],
            "allow"
        );
        assert_eq!(config["permission"]["read"][&gitdir_pattern], "allow");
        let project_pattern = format!("{}/*", canonical_project_root.to_string_lossy());
        assert!(
            config["permission"]["external_directory"][&project_pattern].is_null(),
            "shared project root must not be trusted for OpenCode workers"
        );
        assert!(
            config["permission"]["read"][&project_pattern].is_null(),
            "shared project root must not be readable for OpenCode workers"
        );

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn test_prepare_local_gemini_home_writes_trusted_folder_and_settings() {
        let temp_dir = fresh_temp_dir("brehon-gemini-home");
        let (home_root, trusted_folders_path) =
            prepare_local_gemini_home(&temp_dir, "/tmp/brehon", "worker", Some("high")).unwrap();
        let settings_path = home_root.join(".gemini/settings.json");
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["mcpServers"],
            serde_json::json!({
                "brehon": {
                    "command": "/tmp/brehon",
                    "args": ["serve"],
                    "trust": true,
                }
            })
        );
        assert_eq!(
            settings["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        let trusted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(trusted_folders_path).unwrap()).unwrap();
        let canonical_temp_dir = std::fs::canonicalize(&temp_dir).unwrap();
        assert_eq!(
            trusted[canonical_temp_dir.to_string_lossy().as_ref()],
            "TRUST_FOLDER"
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_prepare_local_gemini_home_copies_role_scoped_skills() {
        let temp_dir = fresh_temp_dir("brehon-gemini-skills");
        let source_root = temp_dir.join(".gemini/extensions/maestro/skills");
        std::fs::create_dir_all(source_root.join("brehon-discovery")).unwrap();
        std::fs::write(
            source_root.join("brehon-discovery/SKILL.md"),
            "supervisor skill",
        )
        .unwrap();
        std::fs::create_dir_all(source_root.join("brehon-supervisor-checklist")).unwrap();
        std::fs::write(
            source_root.join("brehon-supervisor-checklist/SKILL.md"),
            "supervisor checklist",
        )
        .unwrap();
        std::fs::create_dir_all(source_root.join("brehon-worker")).unwrap();
        std::fs::write(source_root.join("brehon-worker/SKILL.md"), "worker skill").unwrap();

        let (home_root, _) =
            prepare_local_gemini_home(&temp_dir, "/tmp/brehon", "supervisor", None).unwrap();
        let runtime_skills = home_root.join(".gemini/extensions/maestro/skills");
        assert!(runtime_skills.join("brehon-discovery/SKILL.md").exists());
        assert!(
            runtime_skills
                .join("brehon-supervisor-checklist/SKILL.md")
                .exists()
        );
        assert!(!runtime_skills.join("brehon-worker/SKILL.md").exists());

        let (home_root, _) =
            prepare_local_gemini_home(&temp_dir, "/tmp/brehon", "worker", None).unwrap();
        let runtime_skills = home_root.join(".gemini/extensions/maestro/skills");
        assert!(!runtime_skills.join("brehon-discovery/SKILL.md").exists());
        assert!(
            !runtime_skills
                .join("brehon-supervisor-checklist/SKILL.md")
                .exists()
        );
        assert!(runtime_skills.join("brehon-worker/SKILL.md").exists());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_gemini_builtin_skill_names_are_brehon_namespaced() {
        for role in ["supervisor", "worker", "reviewer"] {
            for skill_name in gemini_builtin_skill_names_for_role(role) {
                assert!(
                    skill_name.starts_with("brehon-"),
                    "Gemini role skill '{skill_name}' for {role} must use the brehon-* namespace"
                );
            }
        }
        assert!(
            gemini_builtin_skill_names_for_role("supervisor")
                .contains(&"brehon-supervisor-checklist"),
            "Gemini supervisors must receive the built-in Brehon checklist skill"
        );
    }

    #[test]
    fn test_prepare_local_kimi_runtime_seeds_auth_and_writes_runtime_config() {
        let temp_dir = fresh_temp_dir("brehon-kimi-runtime");
        let brehon_root = temp_dir.join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();
        let global_root = fresh_temp_dir("brehon-kimi-global");
        std::fs::create_dir_all(global_root.join("credentials")).unwrap();
        std::fs::write(global_root.join("device_id"), "device-1").unwrap();
        std::fs::write(global_root.join("credentials/oauth--kimi-code.json"), "{}").unwrap();
        std::fs::write(
            global_root.join("config.toml"),
            r#"
default_model = "kimi-code/kimi-for-coding"
default_thinking = false
default_yolo = false

[models."kimi-code/kimi-for-coding"]
provider = "managed:kimi-code"
model = "kimi-for-coding"
max_context_size = 262144
capabilities = ["thinking", "image_in", "video_in"]

[providers."managed:kimi-code"]
type = "kimi"
base_url = "https://api.kimi.com/coding/v1"
api_key = ""

[providers."managed:kimi-code".oauth]
storage = "file"
key = "oauth/kimi-code"
"#,
        )
        .unwrap();

        let (share_dir, model_override) = prepare_local_kimi_runtime_with_global_share(
            &temp_dir,
            "/tmp/brehon",
            Some(&brehon_root),
            Some(&global_root),
            Some("kimi-for-coding"),
            Some("high"),
        )
        .unwrap();

        assert_eq!(model_override.as_deref(), Some("kimi-for-coding"));
        assert!(share_dir.join("device_id").exists());
        assert!(share_dir.join("credentials/oauth--kimi-code.json").exists());

        let config_text = std::fs::read_to_string(share_dir.join("config.toml")).unwrap();
        assert!(config_text.contains(r#"default_model = "kimi-code/kimi-for-coding""#));
        assert!(config_text.contains("default_thinking = true"));
        assert!(config_text.contains("default_yolo = true"));

        let mcp_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(share_dir.join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            mcp_json,
            desired_kimi_mcp_config(
                "/tmp/brehon",
                &temp_dir,
                Some(&brehon_root),
                None,
                None,
                None,
                None
            )
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::remove_dir_all(&global_root);
    }

    #[tokio::test]
    async fn test_pty_config_junie_worker() {
        let config = PtyConfig::junie(
            "junie-worker",
            "worker",
            PathBuf::from("/tmp"),
            None,
            Some("test-supervisor"),
            None,
            Some("anthropic-claude-3.5-sonnet"),
            None,
        );
        assert_eq!(config.command, "junie");
        assert!(config.args.contains(&"--brave".to_string()));
        assert!(
            config
                .args
                .contains(&"--model=anthropic-claude-3.5-sonnet".to_string())
        );
        assert!(config.args.contains(&"--task".to_string()));
        assert!(config.args.contains(&"--output-format".to_string()));
        assert!(config.args.contains(&"json".to_string()));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "junie-worker")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "test-supervisor")
        );
    }

    #[tokio::test]
    async fn test_pty_config_kimi_acp_worker() {
        let workdir = fresh_temp_dir("brehon-kimi-acp-worker");
        let brehon_root = workdir.join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();
        let _session_guard = set_brehon_session_name_for_test("brehon-test-session");
        let config = PtyConfig::kimi_acp(
            "kimi-worker",
            "worker",
            workdir.clone(),
            Some(&brehon_root),
            Some("test-supervisor"),
            None,
            Some("kimi-for-coding"),
            Some("high"),
        );

        assert_eq!(config.command, "kimi");
        assert_eq!(config.args, vec!["acp".to_string()]);
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "test-supervisor")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "KIMI_CLI_NO_AUTO_UPDATE" && v == "true")
        );
        assert!(config.env.iter().any(|(k, _)| k == "KIMI_SHARE_DIR"));
        assert!(
            config
                .env
                .iter()
                .any(|(k, _)| k == "BREHON_ACP_MCP_SERVERS_JSON")
        );

        let runtime_config = workdir.join(".brehon/factory-runtime/kimi/share/config.toml");
        let config_text = std::fs::read_to_string(runtime_config).unwrap();
        assert!(config_text.contains(r#"default_yolo = true"#));

        let runtime_mcp = workdir.join(".brehon/factory-runtime/kimi/share/mcp.json");
        let mcp_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(runtime_mcp).unwrap()).unwrap();
        assert_eq!(
            mcp_json,
            desired_kimi_mcp_config(
                &current_brehon_exe(),
                &workdir,
                Some(&brehon_root),
                Some("kimi-worker"),
                Some("worker"),
                Some("test-supervisor"),
                None,
            )
        );
        let env = &mcp_json["mcpServers"]["brehon"]["env"];
        assert_eq!(env["BREHON_AGENT_NAME"], "kimi-worker");
        assert_eq!(env["BREHON_AGENT_ROLE"], "worker");
        assert_eq!(
            env["BREHON_ROOT"],
            brehon_root.to_string_lossy().to_string()
        );
        assert_eq!(
            env["BREHON_WORKSPACE_ROOT"],
            workdir.to_string_lossy().to_string()
        );
        assert_eq!(env["BREHON_SESSION_NAME"], "brehon-test-session");

        let acp_servers = config
            .env
            .iter()
            .find(|(k, _)| k == "BREHON_ACP_MCP_SERVERS_JSON")
            .map(|(_, v)| v)
            .expect("acp mcp servers env");
        let parsed: serde_json::Value = serde_json::from_str(acp_servers).unwrap();
        assert_eq!(
            parsed[0]["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "BREHON_AGENT_NAME")
                .unwrap()["value"],
            "kimi-worker"
        );
        assert_eq!(
            parsed[0]["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "BREHON_AGENT_ROLE")
                .unwrap()["value"],
            "worker"
        );
        assert_eq!(
            parsed[0]["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "BREHON_ROOT")
                .unwrap()["value"],
            brehon_root.to_string_lossy().to_string()
        );
        assert_eq!(
            parsed[0]["env"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "BREHON_SESSION_NAME")
                .unwrap()["value"],
            "brehon-test-session"
        );

        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_kimi_supervisor() {
        let workdir = fresh_temp_dir("brehon-kimi-supervisor");
        let config = PtyConfig::kimi(
            "supervisor",
            "supervisor",
            workdir.clone(),
            None,
            None,
            Some("kimi"),
            Some("kimi-for-coding"),
            Some("off"),
        );

        assert_eq!(config.command, "kimi");
        assert!(config.args.contains(&"--work-dir".to_string()));
        assert!(config.args.contains(&"--yolo".to_string()));
        assert!(config.args.contains(&"--no-thinking".to_string()));
        assert!(config.args.contains(&"--prompt".to_string()));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_FACTORY_WORKER_CLI" && v == "kimi")
        );

        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_junie_supervisor() {
        let config = PtyConfig::junie(
            "supervisor",
            "supervisor",
            PathBuf::from("/tmp"),
            None,
            None,
            Some("junie"),
            None,
            None,
        );
        assert_eq!(config.command, "junie");
        assert!(config.args.contains(&"--brave".to_string()));
        assert!(config.args.contains(&"--task".to_string()));
        assert!(config.args.iter().any(|arg| {
            arg.contains("action=session_start name=supervisor agent_type=supervisor")
        }));
        assert!(!config.args.iter().any(|a| a.starts_with("--model=")));
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_FACTORY_WORKER_CLI" && v == "junie")
        );
    }

    #[tokio::test]
    async fn test_pty_config_copilot_acp_worker() {
        let workdir = fresh_temp_dir("brehon-copilot-acp-worker");
        let config = PtyConfig::copilot_acp(
            "copilot-worker",
            "worker",
            workdir.clone(),
            None,
            Some("test-supervisor"),
            None,
            Some("claude-sonnet-4.6"),
            Some("high"),
        );

        assert!(matches!(config.command.as_str(), "copilot" | "gh"));
        assert!(config.args.contains(&"--acp".to_string()));
        assert!(config.args.contains(&"--stdio".to_string()));
        assert!(config.args.contains(&"--allow-all".to_string()));
        assert!(config.args.contains(&"--no-ask-user".to_string()));
        assert!(config.args.contains(&"--no-auto-update".to_string()));
        assert!(config.args.contains(&"--config-dir".to_string()));
        assert!(config.args.contains(&"--model".to_string()));
        assert!(config.args.contains(&"claude-sonnet-4.6".to_string()));
        assert!(
            !config
                .args
                .iter()
                .any(|arg| arg.contains("Brehon worker startup"))
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "copilot-worker")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "test-supervisor")
        );
        assert!(config.env.iter().any(|(k, _)| k == "COPILOT_HOME"));
        assert!(config.env.iter().any(|(k, _)| k == "COPILOT_CACHE_HOME"));

        let runtime_config = workdir.join(".brehon/factory-runtime/copilot/home/config.json");
        let config_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(runtime_config).unwrap()).unwrap();
        assert_eq!(config_json["model"], "claude-sonnet-4.6");
        assert_eq!(config_json["effortLevel"], "high");

        let runtime_mcp = workdir.join(".brehon/factory-runtime/copilot/home/mcp-config.json");
        let mcp_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(runtime_mcp).unwrap()).unwrap();
        assert_eq!(mcp_json, desired_copilot_mcp_config(&current_brehon_exe()));
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_copilot_acp_supervisor() {
        let workdir = fresh_temp_dir("brehon-copilot-acp-supervisor");
        let config = PtyConfig::copilot_acp(
            "supervisor",
            "supervisor",
            workdir.clone(),
            None,
            None,
            Some("copilot"),
            None,
            None,
        );
        assert!(matches!(config.command.as_str(), "copilot" | "gh"));
        assert!(config.args.contains(&"--acp".to_string()));
        assert!(config.args.contains(&"--stdio".to_string()));
        assert!(config.args.contains(&"--allow-all".to_string()));
        assert!(!config.args.contains(&"--model".to_string()));
        assert!(
            !config
                .args
                .iter()
                .any(|arg| arg
                    .contains("action=session_start name=supervisor agent_type=supervisor"))
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_FACTORY_WORKER_CLI" && v == "copilot")
        );
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_opencode_with_brehon_root() {
        let brehon_root = PathBuf::from("/home/user/project/.brehon");
        let config = PtyConfig::opencode(
            "oc-worker",
            "worker",
            PathBuf::from("/tmp"),
            Some(&brehon_root),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_ROOT" && v == "/home/user/project/.brehon")
        );
    }

    #[tokio::test]
    async fn test_pty_config_copilot_acp_with_model() {
        let workdir = fresh_temp_dir("brehon-copilot-acp-model");
        let config = PtyConfig::copilot_acp(
            "copilot-worker",
            "worker",
            workdir.clone(),
            None,
            None,
            None,
            Some("gpt-4.1"),
            None,
        );
        assert!(config.args.contains(&"--model".to_string()));
        assert!(config.args.contains(&"gpt-4.1".to_string()));
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[tokio::test]
    async fn test_pty_config_copilot_acp_without_model() {
        let workdir = fresh_temp_dir("brehon-copilot-acp-no-model");
        let config = PtyConfig::copilot_acp(
            "copilot-worker",
            "worker",
            workdir.clone(),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!config.args.contains(&"--model".to_string()));
        let _ = std::fs::remove_dir_all(workdir);
    }

    #[test]
    fn test_prepare_local_copilot_runtime_preserves_login_but_drops_hooks_and_plugins() {
        let temp_dir = fresh_temp_dir("brehon-copilot-runtime");
        let global_root = fresh_temp_dir("brehon-copilot-global");
        let global_config_path = global_root.join("config.json");
        std::fs::write(
            &global_config_path,
            r#"{
  "last_logged_in_user": {
    "host": "https://github.com",
    "login": "octocat"
  },
  "logged_in_users": [
    {
      "host": "https://github.com",
      "login": "octocat"
    }
  ],
  "hooks": {
    "preToolUse": []
  },
  "enabledPlugins": {
    "playground": true
  },
  "trusted_folders": ["/tmp/elsewhere"]
}"#,
        )
        .unwrap();

        let (config_dir, cache_dir) = prepare_local_copilot_runtime_with_global_config(
            &temp_dir,
            "/tmp/brehon",
            Some(&global_config_path),
            Some("gpt-5"),
            Some("medium"),
        )
        .unwrap();

        assert_eq!(
            config_dir,
            temp_dir.join(".brehon/factory-runtime/copilot/home")
        );
        assert_eq!(
            cache_dir,
            temp_dir.join(".brehon/factory-runtime/copilot/cache")
        );

        let config_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(config_json["last_logged_in_user"]["login"], "octocat");
        assert_eq!(config_json["logged_in_users"][0]["login"], "octocat");
        assert!(config_json.get("hooks").is_none());
        assert!(config_json.get("enabledPlugins").is_none());
        assert!(config_json.get("trusted_folders").is_none());
        assert_eq!(config_json["disableAllHooks"], true);
        assert_eq!(config_json["model"], "gpt-5");
        assert_eq!(config_json["effortLevel"], "medium");

        let mcp_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(config_dir.join("mcp-config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(mcp_json, desired_copilot_mcp_config("/tmp/brehon"));

        let _ = std::fs::remove_dir_all(temp_dir);
        let _ = std::fs::remove_dir_all(global_root);
    }

    #[test]
    fn test_worker_startup_prompt_includes_commit_and_worktree_rules() {
        let prompt = build_worker_startup_prompt(
            "worker-1",
            "clear-elk-58",
            "mcp__brehon__agent",
            "mcp__brehon__task",
            None,
        );

        assert!(prompt.contains("Use Brehon MCP tools for task/state coordination"));
        assert!(prompt.contains("action=checkpoint id=<task> message="));
        assert!(prompt.contains("action=progress id=<task> percent=<n>"));
        assert!(prompt.contains("action=complete id=<task>"));
        assert!(prompt.contains("moves the task to `review_ready`"));
        assert!(prompt.contains("status=blocked"));
        assert!(
            prompt.contains("Do NOT proactively call `mcp__brehon__agent action=session_start`")
        );
        assert!(prompt.contains("call `mcp__brehon__task action=mine` at most once"));
        assert!(prompt.contains("emit at most one short readiness line"));
    }

    #[test]
    fn test_supervisor_startup_prompt_forbids_ready_ack_and_builtin_messaging() {
        let prompt = build_supervisor_startup_prompt(
            "supervisor-1",
            "mcp__brehon__agent",
            "mcp__brehon__task",
            None,
        );

        assert!(prompt.contains("Do NOT send readiness acknowledgements"));
        assert!(prompt.contains("Do NOT use built-in messaging tools like `SendMessage`"));
        assert!(prompt.contains("reply with one short status line and stop"));
        assert!(prompt.contains("After any action that may change the frontier"));
        assert!(prompt.contains("Call these silently, without narrating each step"));
        assert!(prompt.contains("Do not narrate MCP bootstrap/tool calls"));
        assert!(prompt.contains("mcp__brehon__search_skills query=\"\""));
        assert!(prompt.contains("mcp__brehon__search_rules query=\"\""));
        assert!(
            prompt
                .contains("Do NOT call `mcp__brehon__factory action=worker_status` during startup")
        );
    }

    #[test]
    fn test_reviewer_startup_prompt_limits_bootstrap_narration() {
        let prompt = build_reviewer_startup_prompt(
            "reviewer-1",
            "mcp__brehon__agent",
            "mcp__brehon__verification",
            None,
        );

        assert!(
            prompt.contains("Do NOT proactively discover, reconnect, or call Brehon MCP tools")
        );
        assert!(prompt.contains("emit at most one short readiness line"));
        assert!(prompt.contains("Do not narrate MCP bootstrap/tool calls"));
        assert!(prompt.contains("instead of polling, sleeping, or running shell commands"));
        assert!(!prompt.contains("Call these silently, without narrating each step"));
        assert!(!prompt.contains("action=session_start name="));
        assert!(!prompt.contains(" ; mcp__brehon__agent action=whoami"));
    }

    #[test]
    fn test_codex_worker_config_does_not_embed_startup_prompt() {
        let config = PtyConfig::codex(
            "worker-1",
            "worker",
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );

        assert!(
            !config
                .args
                .iter()
                .any(|arg| arg.contains("Brehon worker startup")),
            "Codex worker bootstrap should come from the shared prompt queue"
        );
    }

    #[test]
    fn test_codex_supervisor_config_embeds_startup_prompt() {
        let config = PtyConfig::codex(
            "supervisor",
            "supervisor",
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );

        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains("Factory supervisor startup")),
            "Codex supervisor bootstrap should be embedded in the interactive PTY session"
        );
        assert!(
            !config.args.contains(&"--no-alt-screen".to_string()),
            "Codex supervisors should keep the native interactive screen"
        );
    }

    #[test]
    fn test_codex_acp_config_uses_app_server() {
        let config = PtyConfig::codex_acp(
            "reviewer-1",
            "reviewer",
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            Some("gpt-5.4"),
            None,
        );

        assert_eq!(config.command, "codex");
        assert!(config.args.contains(&"app-server".to_string()));
        assert!(
            config
                .args
                .contains(&"--dangerously-bypass-approvals-and-sandbox".to_string())
        );
        assert!(
            config
                .args
                .contains(&"shell_environment_policy.inherit=all".to_string())
        );
        assert!(
            config
                .args
                .contains(&"sandbox_permissions=[\"disk-full-read-access\"]".to_string())
        );
        assert!(
            !config.args.contains(&"--ask-for-approval".to_string()),
            "bypass mode must not also pass explicit approval policy"
        );
        assert!(
            !config.args.contains(&"--sandbox".to_string()),
            "bypass mode must not also pass an explicit sandbox mode"
        );
        assert!(
            !config
                .args
                .iter()
                .any(|arg| arg.contains("Brehon reviewer startup"))
        );
    }

    #[test]
    fn test_codex_acp_config_sets_model_reasoning_effort() {
        let config = PtyConfig::codex_acp(
            "reviewer-1",
            "reviewer",
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            Some("gpt-5.4"),
            Some("xhigh"),
        );

        assert!(
            config
                .args
                .contains(&"model_reasoning_effort=\"xhigh\"".to_string())
        );
    }

    #[test]
    fn test_codex_config_trusts_linked_worktree_gitdir() {
        let (workdir, gitdir) = setup_fake_linked_worktree("brehon-codex-linked");
        let config = PtyConfig::codex(
            "worker-1",
            "worker",
            workdir.clone(),
            None,
            None,
            None,
            Some("gpt-5.3-codex"),
            None,
            None,
        );

        let codex_home = workdir.join(".brehon/factory-runtime/codex/home");
        let codex_config = std::fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(codex_config.contains(workdir.to_string_lossy().as_ref()));
        assert!(codex_config.contains(gitdir.to_string_lossy().as_ref()));
        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains(gitdir.to_string_lossy().as_ref())),
            "Codex launch args should trust the linked worktree admin dir"
        );

        let _ = std::fs::remove_dir_all(workdir);
        let _ = std::fs::remove_dir_all(gitdir);
    }

    #[test]
    fn test_codex_acp_config_trusts_linked_worktree_gitdir() {
        let (workdir, gitdir) = setup_fake_linked_worktree("brehon-codex-acp-linked");
        let config = PtyConfig::codex_acp(
            "worker-1",
            "worker",
            workdir.clone(),
            None,
            None,
            None,
            Some("gpt-5.3-codex"),
            None,
        );

        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains(gitdir.to_string_lossy().as_ref())),
            "Codex ACP launch args should trust the linked worktree admin dir"
        );

        let _ = std::fs::remove_dir_all(workdir);
        let _ = std::fs::remove_dir_all(gitdir);
    }

    #[test]
    fn test_custom_codex_acp_config_keeps_codex_bootstrap() {
        let cwd = fresh_temp_dir("brehon-custom-codex-acp");
        let brehon_root = cwd.join(".brehon");
        let instructions_dir = brehon_root.join("instructions");
        std::fs::create_dir_all(&instructions_dir).unwrap();
        std::fs::write(
            instructions_dir.join("codex-worker-instructions.md"),
            "worker instructions\n",
        )
        .unwrap();

        let config = PtyConfig::custom_codex_acp(
            "worker-1",
            "worker",
            cwd.clone(),
            Some("codex-ollama-worker"),
            Some(&brehon_root),
            Some("supervisor"),
            Some("codex-ollama-worker"),
            None,
            &[
                "-c".to_string(),
                "model=\"glm-5.1:cloud\"".to_string(),
                "-c".to_string(),
                "model_provider=\"ollama_cloud\"".to_string(),
                "app-server".to_string(),
            ],
        );

        assert_eq!(config.command, "codex");
        assert!(config.args.contains(&"app-server".to_string()));
        assert!(
            config
                .args
                .contains(&"model_provider=\"ollama_cloud\"".to_string())
        );
        assert!(config.args.contains(&"model=\"glm-5.1:cloud\"".to_string()));
        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains("model_instructions_file=")),
            "custom Codex ACP launch should still include Brehon worker instructions"
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "codex-ollama-worker")
        );
        assert!(
            config.env.iter().any(|(k, _)| k == "CODEX_HOME"),
            "custom Codex ACP launch should still provision CODEX_HOME"
        );

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn test_codex_reviewer_uses_reviewer_instructions_file() {
        let temp = fresh_temp_dir("brehon-codex-reviewer-instructions");
        let instructions_dir = temp.join("instructions");
        std::fs::create_dir_all(&instructions_dir).unwrap();
        let reviewer_path = instructions_dir.join("codex-reviewer-instructions.md");
        std::fs::write(&reviewer_path, "# reviewer\n").unwrap();

        let config = PtyConfig::codex(
            "reviewer-codex-1",
            "reviewer",
            PathBuf::from("/tmp"),
            Some(&temp),
            None,
            None,
            None,
            None,
            None,
        );

        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains("codex-reviewer-instructions.md")),
            "Codex reviewers should load reviewer-specific instructions"
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn test_codex_remote_does_not_resume_last_session() {
        let workdir = fresh_temp_dir("brehon-codex-remote");
        let ready_path = workdir.join(".brehon/factory-runtime/codex/remote-ready");
        std::fs::create_dir_all(ready_path.parent().expect("remote-ready parent")).unwrap();
        std::fs::write(&ready_path, "stale").unwrap();

        let config = PtyConfig::codex_remote(
            "reviewer-codex-1",
            "reviewer",
            workdir.clone(),
            None,
            None,
            None,
            Some("gpt-5.4"),
            None,
            "ws://127.0.0.1:43199",
            None,
        );

        assert_eq!(config.command, "sh");
        let script = config.args.join(" ");
        assert!(!script.contains("--last"));
        assert!(script.contains("--remote"));
        assert!(script.contains("ws://127.0.0.1:43199"));
        assert!(script.contains("codex remote session bootstrap failed"));
        assert!(!ready_path.exists(), "stale ready marker should be cleared");

        let _ = std::fs::remove_dir_all(workdir);
    }

    #[test]
    fn test_gemini_reviewer_config_embeds_startup_prompt() {
        let config = PtyConfig::gemini(
            "reviewer-gemini-1",
            "reviewer",
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );

        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains("Brehon reviewer startup"))
        );
        assert!(
            config
                .args
                .iter()
                .all(|arg| !arg.contains("action=session_start name="))
        );
        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains("mcp_brehon_verification"))
        );
        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains("action=submit_review"))
        );
        assert!(config.args.iter().any(|arg| arg.contains("review_id=")));
        assert!(config.args.iter().any(|arg| arg.contains("score=<1-10>")));
        assert!(config.args.iter().any(|arg| {
            arg.contains("Treat every file path in review prompts, findings, and task titles as repository-relative to that root")
        }));
    }

    #[test]
    fn test_opencode_reviewer_config_embeds_startup_prompt() {
        let config = PtyConfig::opencode(
            "reviewer-opencode-1",
            "reviewer",
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            Some("ollama-cloud/glm-5.1"),
            None,
            None,
        );

        assert!(config.args.contains(&"--prompt".to_string()));
        assert!(
            config
                .args
                .iter()
                .any(|arg| arg.contains("Brehon reviewer startup"))
        );
        assert!(
            config
                .args
                .iter()
                .all(|arg| !arg.contains("action=session_start name="))
        );
        assert!(config.args.iter().any(|arg| {
            arg.contains("Treat every file path in review prompts, findings, and task titles as repository-relative to that root")
        }));
    }
}
