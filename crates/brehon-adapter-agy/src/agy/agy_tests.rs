use super::*;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn test_env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn set_test_env(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> EnvVarGuard {
    let previous = std::env::var(key).ok();
    std::env::set_var(key, value);
    EnvVarGuard { key, previous }
}

fn agy_params(role: &str, allow_privileged_mode: bool) -> AgySpawnParams {
    AgySpawnParams {
        name: format!("agy-{role}"),
        role: role.to_string(),
        cwd: PathBuf::from("/tmp"),
        brehon_root: None,
        supervisor_name: Some("supervisor".to_string()),
        factory_worker_cli: None,
        model: None,
        allow_privileged_mode,
    }
}

fn assert_only_supported_agy_flags(config: &AgySessionConfig) {
    // Keep this list aligned with `agy --help` for flags Brehon may generate.
    // This catches stale removed flags such as `--mcp-location` before panes
    // boot into help text instead of an agent session.
    const SUPPORTED_GENERATED_FLAGS: &[&str] = &[
        "--dangerously-skip-permissions",
        "--model",
        "--prompt-interactive",
    ];

    for arg in &config.args {
        if !arg.starts_with("--") {
            continue;
        }
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        assert!(
            SUPPORTED_GENERATED_FLAGS.contains(&flag),
            "generated unsupported agy flag {flag}; args={:?}",
            config.args
        );
    }
}

#[test]
fn agy_session_config_from_params_worker() {
    let params = agy_params("worker", false);
    let config = AgySessionConfig::from_params(&params);
    assert_eq!(config.command, "agy");
    assert!(!config.args.contains(&"--mcp-location".to_string()));
    assert!(config.args.contains(&"--prompt-interactive".to_string()));
    let prompt = config
        .args
        .iter()
        .find(|arg| arg.contains("Brehon worker startup"))
        .expect("worker startup prompt");
    assert!(prompt.contains("mcp_brehon_agent"));
    assert!(prompt.contains("mcp_brehon_task"));
    assert!(!prompt.contains("Antigravity MCP usage for this Brehon session"));
    assert!(!prompt.contains(".antigravitycli/brehon_mcp_client.py"));
    assert!(!prompt.contains("python3"));
    assert!(prompt.contains("You are worker 'agy-worker'"));
    assert!(prompt.contains("target=supervisor"));
    assert!(config
        .env
        .iter()
        .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "agy-worker"));
    assert!(config
        .env
        .iter()
        .any(|(k, v)| k == "BREHON_AGENT_ROLE" && v == "worker"));
    assert!(!config
        .args
        .contains(&"--dangerously-skip-permissions".to_string()));
}

#[test]
fn agy_session_config_unsafe_profile_enables_skip_permissions() {
    let params = agy_params("worker", true);
    let config = AgySessionConfig::from_params(&params);
    assert_eq!(config.args[0], "--dangerously-skip-permissions");
}

#[test]
fn agy_session_config_emits_only_supported_cli_flags() {
    for role in ["worker", "supervisor", "reviewer"] {
        for allow_privileged_mode in [false, true] {
            let config = AgySessionConfig::from_params(&agy_params(role, allow_privileged_mode));
            assert_only_supported_agy_flags(&config);
        }
    }
}

#[test]
fn agy_mcp_config_merges_brehon_server_in_workspace_agents_config() {
    let test_root =
        std::env::temp_dir().join(format!("brehon-agy-mcp-test-{}", uuid::Uuid::new_v4()));
    let workspace = test_root.join("workspace");
    let config_path = workspace.join(AGY_PROJECT_MCP_CONFIG_PATH);
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        r#"{"mcpServers":{"agora":{"command":"agora","args":["serve"]},"other":{"command":"other","args":["serve"]},"brehon":{"command":"/old/brehon","args":["serve"],"cwd":"/old/worktree","env":{"BREHON_SESSION_NAME":"brehon-session","BREHON_WORKTREE_ROOT":"/external/worktrees","BREHON_SUPERVISOR_NAME":"claude-supervisor"}}}}"#,
    )
    .unwrap();

    let env = vec![
        ("BREHON_AGENT_NAME".to_string(), "agy-worker".to_string()),
        ("BREHON_AGENT_ROLE".to_string(), "worker".to_string()),
        (
            "BREHON_ROOT".to_string(),
            workspace.join(".brehon").to_string_lossy().to_string(),
        ),
        ("PATH".to_string(), "/usr/bin".to_string()),
    ];
    configure_project_mcp_config(&workspace, "/tmp/brehon", &env);

    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
    assert_eq!(
        config["mcpServers"]["brehon"]["command"],
        serde_json::json!("/tmp/brehon")
    );
    assert_eq!(
        config["mcpServers"]["brehon"]["args"],
        serde_json::json!(["serve"])
    );
    assert_eq!(
        config["mcpServers"]["brehon"]["cwd"],
        serde_json::Value::String(workspace.to_string_lossy().to_string())
    );
    assert_eq!(
        config["mcpServers"]["brehon"]["env"]["BREHON_AGENT_NAME"],
        "agy-worker"
    );
    assert_eq!(
        config["mcpServers"]["brehon"]["env"]["BREHON_AGENT_ROLE"],
        "worker"
    );
    assert_eq!(
        config["mcpServers"]["brehon"]["env"]["BREHON_ROOT"],
        workspace.join(".brehon").to_string_lossy().to_string()
    );
    assert_eq!(
        config["mcpServers"]["brehon"]["env"]["BREHON_SESSION_NAME"],
        "brehon-session"
    );
    assert_eq!(
        config["mcpServers"]["brehon"]["env"]["BREHON_WORKTREE_ROOT"],
        "/external/worktrees"
    );
    assert_eq!(
        config["mcpServers"]["brehon"]["env"]["BREHON_SUPERVISOR_NAME"],
        "claude-supervisor"
    );
    assert_eq!(config["mcpServers"]["other"]["command"], "other");
    assert!(config["mcpServers"].get("agora").is_none());
    assert!(!workspace.join(".mcp.json").exists());

    let _ = std::fs::remove_dir_all(test_root);
}

#[test]
fn agy_trust_updates_legacy_and_cli_settings() {
    let test_root =
        std::env::temp_dir().join(format!("brehon-agy-trust-test-{}", uuid::Uuid::new_v4()));
    let home = test_root.join("home");
    let project = test_root.join("project");
    let worktree = project.join(".brehon/worktrees/runs/test/worker-1");
    std::fs::create_dir_all(&worktree).unwrap();
    let settings_path = home.join(".gemini/antigravity-cli/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(
        &settings_path,
        r#"{"colorScheme":"dark","trustedWorkspaces":["/already/trusted"]}"#,
    )
    .unwrap();

    let paths = trusted_workspace_paths(&worktree, Some(&project.join(".brehon")));
    trust_folders_in_home(&home, &paths);
    let worktree_key = paths[0].to_string_lossy();
    let project_key = paths[1].to_string_lossy();

    let trusted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".gemini/trustedFolders.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        trusted
            .get(worktree_key.as_ref())
            .and_then(serde_json::Value::as_str),
        Some("TRUST_FOLDER")
    );
    assert_eq!(
        trusted
            .get(project_key.as_ref())
            .and_then(serde_json::Value::as_str),
        Some("TRUST_FOLDER")
    );

    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
    let workspaces = settings["trustedWorkspaces"].as_array().unwrap();
    assert!(workspaces
        .iter()
        .any(|value| value.as_str() == Some(worktree_key.as_ref())));
    assert!(workspaces
        .iter()
        .any(|value| value.as_str() == Some(project_key.as_ref())));

    let _ = std::fs::remove_dir_all(test_root);
}

#[test]
fn agy_adapter_kind_is_agy() {
    let adapter = AgyAdapter::new(AgyConfig {
        command: "agy".to_string(),
        args: vec![],
        env: vec![],
    });
    assert_eq!(adapter.kind(), brehon_types::AdapterKind::Agy);
}

#[test]
fn test_agy_preflight_checks_complete() {
    let _env_lock = test_env_lock();
    let test_root =
        std::env::temp_dir().join(format!("brehon-preflight-test-{}", uuid::Uuid::new_v4()));
    let workspace = test_root.join("workspace");
    let fake_home = test_root.join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&fake_home).unwrap();

    let _home = set_test_env("HOME", &fake_home);
    let _force_preflight = set_test_env("BREHON_FORCE_PREFLIGHT", "1");

    let res = run_preflight_checks(&workspace, "/bin/echo", None);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("MCP config file"));

    let mcp_config_path = workspace.join(AGY_PROJECT_MCP_CONFIG_PATH);
    std::fs::create_dir_all(mcp_config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &mcp_config_path,
        r#"{"mcpServers":{"brehon":{"command":"brehon","args":["serve"]}}}"#,
    )
    .unwrap();

    let res = run_preflight_checks(&workspace, "/bin/echo", None);
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Preflight trust check failed"));

    let trust_path = fake_home.join(".gemini/trustedFolders.json");
    std::fs::create_dir_all(trust_path.parent().unwrap()).unwrap();
    let canonical_workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace.clone());
    let trust_json = serde_json::json!({
        canonical_workspace.to_string_lossy().to_string(): "TRUST_FOLDER"
    });
    std::fs::write(&trust_path, serde_json::to_string(&trust_json).unwrap()).unwrap();

    let res = run_preflight_checks(&workspace, "/bin/echo", None);
    assert!(res.is_ok(), "Preflight checks should pass: {:?}", res);

    let _ = std::fs::remove_dir_all(test_root);
}
