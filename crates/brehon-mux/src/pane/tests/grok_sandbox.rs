use crate::harness::{
    AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane, HarnessTransport,
    PromptInjectionStrategy,
};
use crate::pane::Pane;

fn grok_adapter(name: &str) -> AgentAdapter {
    AgentAdapter::Custom(CustomAgentConfig {
        name: name.to_string(),
        command: Some("grok".to_string()),
        args: vec![
            "agent".to_string(),
            "--always-approve".to_string(),
            "stdio".to_string(),
        ],
        base_url: None,
        max_concurrency: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: true,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
            tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    })
}

#[test]
fn test_grok_acp_workers_use_distinct_sandbox_profiles_per_cwd() {
    let temp = tempfile::tempdir().expect("temp project");
    let project_root = temp.path().join("repo");
    let brehon_root = project_root.join(".brehon");
    let git_dir = project_root.join(".git");
    let worktree_a = temp.path().join("worktree-a");
    let worktree_b = temp.path().join("worktree-b");
    let worktree_git_a = git_dir.join("worktrees").join("worktree-a");
    let worktree_git_b = git_dir.join("worktrees").join("worktree-b");
    let grok_config_dir = temp.path().join("grok-config");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    std::fs::create_dir_all(&worktree_a).expect("create worktree a");
    std::fs::create_dir_all(&worktree_b).expect("create worktree b");
    std::fs::create_dir_all(&worktree_git_a).expect("create worktree git a");
    std::fs::create_dir_all(&worktree_git_b).expect("create worktree git b");
    std::fs::write(
        worktree_a.join(".git"),
        format!("gitdir: {}\n", worktree_git_a.display()),
    )
    .expect("write worktree a gitfile");
    std::fs::write(
        worktree_b.join(".git"),
        format!("gitdir: {}\n", worktree_git_b.display()),
    )
    .expect("write worktree b gitfile");

    let launcher_env = [(
        "GROK_BREHON_SANDBOX_CONFIG_DIR".to_string(),
        grok_config_dir.to_string_lossy().to_string(),
    )];
    let pane_a = Pane::worker_with_agent_type(
        "worker-a",
        worktree_a,
        Some("session-a"),
        Some(&brehon_root),
        "supervisor",
        &grok_adapter("grok-worker"),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
        &launcher_env,
        None,
    )
    .expect("create grok worker a pane");
    let pane_b = Pane::worker_with_agent_type(
        "worker-b",
        worktree_b,
        Some("session-a"),
        Some(&brehon_root),
        "supervisor",
        &grok_adapter("grok-worker"),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
        &launcher_env,
        None,
    )
    .expect("create grok worker b pane");

    let config_a = pane_a
        .gateway_spawn_config()
        .expect("gateway config a should exist");
    let config_b = pane_b
        .gateway_spawn_config()
        .expect("gateway config b should exist");
    let sandbox_profile_a = config_a
        .args
        .windows(2)
        .find_map(|window| (window[0] == "--sandbox").then_some(window[1].as_str()))
        .expect("grok sandbox profile a");
    let sandbox_profile_b = config_b
        .args
        .windows(2)
        .find_map(|window| (window[0] == "--sandbox").then_some(window[1].as_str()))
        .expect("grok sandbox profile b");

    assert_ne!(sandbox_profile_a, sandbox_profile_b);
    assert!(sandbox_profile_a.starts_with("brehon-repo-"));
    assert!(sandbox_profile_b.starts_with("brehon-repo-"));

    let sandbox_config =
        std::fs::read_to_string(grok_config_dir.join("sandbox.toml")).expect("sandbox config");
    assert!(sandbox_config.contains(&format!("[profiles.\"{sandbox_profile_a}\"]")));
    assert!(sandbox_config.contains(&format!("[profiles.\"{sandbox_profile_b}\"]")));
    assert!(sandbox_config.contains(&worktree_git_a.to_string_lossy().to_string()));
    assert!(sandbox_config.contains(&worktree_git_b.to_string_lossy().to_string()));
}
