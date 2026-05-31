use std::collections::HashMap;

use crate::*;

fn test_config() -> BrehonConfig {
    BrehonConfig {
        version: 1,
        launchers: HashMap::from([
            (
                "codex".into(),
                AgentConnectionConfig {
                    adapter: AdapterKind::Acp,
                    command: Some("codex".into()),
                    args: vec!["app-server".into()],
                    provider: None,
                    transport: None,
                    control_plane: None,
                    base_url: None,
                    api_key_env: None,
                    permission_mode: None,
                    profile: None,
                    max_parallel_tool_calls: None,
                    assistant_message_passthrough_fields: Vec::new(),
                    reasoning_effort_param: None,
                    extra_body: None,
                    env: HashMap::new(),
                    headers: HashMap::new(),
                },
            ),
            (
                "custom-launcher".into(),
                AgentConnectionConfig {
                    adapter: AdapterKind::Acp,
                    command: Some("custom-agent".into()),
                    args: Vec::new(),
                    provider: None,
                    transport: None,
                    control_plane: None,
                    base_url: None,
                    api_key_env: None,
                    permission_mode: None,
                    profile: None,
                    max_parallel_tool_calls: None,
                    assistant_message_passthrough_fields: Vec::new(),
                    reasoning_effort_param: None,
                    extra_body: None,
                    env: HashMap::new(),
                    headers: HashMap::new(),
                },
            ),
        ]),
        lanes: HashMap::from([(
            "codex-worker".into(),
            LaneConfig {
                launcher: "codex".into(),
                model: None,
                reasoning_effort: Some("high".into()),
                system_prompt: None,
                profile: None,
            },
        )]),
        prompt_fragments: HashMap::new(),
        prompt_policy: PromptPolicyConfig::default(),
        roles: RolesConfig {
            supervisor: RoleDefinition {
                name: "supervisor".into(),
                kind: RoleKind::Supervisor,
                description: String::new(),
                permissions: Vec::new(),
                system_prompt: None,
            },
            workers: Vec::new(),
            reviewers: Vec::new(),
        },
        routing: RoutingConfig::default(),
        advisors: AdvisorConfig::default(),
        research: ResearchConfig::default(),
        review: ReviewConfig::default(),
        supervisor: SupervisorConfig {
            model: None,
            reasoning_effort: None,
            autonomy: AutonomyLevel::Guided,
            heartbeat_minutes: 15,
            stuck_detection: StuckDetectionConfig {
                time_threshold_minutes: 10,
                operation_aware: true,
                pattern_detection: true,
            },
            nudge: NudgeConfig {
                soft_after_minutes: 5,
                guidance_after_minutes: 10,
            },
        },
        orchestration: OrchestrationConfig {
            max_active_workers: 1,
            worktree_isolation: true,
            branch_prefix: "brehon/".into(),
            auto_cleanup_worktrees: true,
            worker_idle_behavior: WorkerIdleBehavior::Wait,
            allow_mutating_idle_work: false,
            self_improve_tasks: Vec::new(),
            spawn_workers: None,
            drain_timeout_secs: None,
            worktree_root: None,
            worktree_cleanup: WorktreeCleanupConfig::default(),
        },
        runtime: RuntimeConfig::default(),
        budget: BudgetConfig {
            max_total_cost: None,
            max_cost_per_task: None,
            max_tokens_per_agent: None,
            alert_threshold_percent: 80,
            enforcement: BudgetEnforcement::Soft,
        },
        tui: TuiConfig {
            default_layout: LayoutPreset::Balanced,
            terminal_mode: TerminalMode::Auto,
            notifications: NotificationConfig {
                toast_duration_seconds: 5,
                flash_tabs: true,
                modal_on_critical: true,
            },
            keybindings: "default".into(),
        },
        escalation: EscalationConfig {
            human_in_loop: true,
            notify_via: NotifyMethod::Terminal,
            escalation_timeout_minutes: 15,
        },
        context: ContextConfig {
            db_path: ".brehon/brehon.db".into(),
            search_index_path: ".brehon/indexes/tantivy".into(),
            memory_ttl_days: None,
            max_memories: 10_000,
            agents_md: AgentsMdMode::Auto,
            retrieval: ContextRetrievalConfig::default(),
            compression: ContextCompressionConfig::default(),
        },
        permissions: PermissionsConfig::default(),
        profiles: ProfilesConfig::default(),
        retention: RetentionConfig::default(),
        security: SecurityConfig {
            sandbox_profile: SandboxProfile::OsDefault,
            persist_transcripts: true,
            redact_patterns: Vec::new(),
            env_allowlist: Vec::new(),
        },
    }
}

#[test]
fn lane_reasoning_effort_prefers_override_then_lane_default() {
    let config = test_config();

    assert_eq!(
        config.lane_reasoning_effort("codex-worker", None),
        Some("high")
    );
    assert_eq!(
        config.lane_reasoning_effort("codex-worker", Some("xhigh")),
        Some("xhigh")
    );
}

#[test]
fn effective_permission_profile_uses_expected_builtin_defaults() {
    let config = test_config();

    let cases = [
        (
            PermissionProfileRole::Supervisor,
            PermissionProfile::Operator,
        ),
        (PermissionProfileRole::Worker, PermissionProfile::Workspace),
        (PermissionProfileRole::Reviewer, PermissionProfile::Reviewer),
        (PermissionProfileRole::Advisor, PermissionProfile::Observe),
        (PermissionProfileRole::Research, PermissionProfile::Observe),
        (
            PermissionProfileRole::Integrator,
            PermissionProfile::Integrator,
        ),
    ];

    for (role, expected) in cases {
        let effective = config.effective_permission_profile(role, None, None);
        assert_eq!(effective.profile, expected, "{role:?}");
        assert_eq!(
            effective.source,
            EffectivePermissionProfileSource::BuiltInRoleDefault,
            "{role:?}"
        );
        assert!(effective.spec.is_none(), "{role:?}");
    }
}

#[test]
fn effective_permission_profile_uses_configured_role_default_when_present() {
    let mut config = test_config();
    config
        .profiles
        .defaults
        .insert("worker".into(), PermissionProfile::Dependency);

    let effective = config.effective_permission_profile(PermissionProfileRole::Worker, None, None);
    assert_eq!(effective.profile, PermissionProfile::Dependency);
    assert_eq!(
        effective.source,
        EffectivePermissionProfileSource::ConfigRoleDefault
    );
}

#[test]
fn effective_permission_profile_prefers_lane_then_launcher_then_role_default() {
    let mut config = test_config();
    config.launchers.get_mut("codex").unwrap().profile = Some(PermissionProfile::Dependency);

    let launcher_only = config.effective_permission_profile(
        PermissionProfileRole::Worker,
        Some("codex-worker"),
        None,
    );
    assert_eq!(launcher_only.profile, PermissionProfile::Dependency);
    assert_eq!(
        launcher_only.source,
        EffectivePermissionProfileSource::Launcher
    );

    config.lanes.get_mut("codex-worker").unwrap().profile = Some(PermissionProfile::Workspace);

    let lane_override = config.effective_permission_profile(
        PermissionProfileRole::Worker,
        Some("codex-worker"),
        None,
    );
    assert_eq!(lane_override.profile, PermissionProfile::Workspace);
    assert_eq!(lane_override.source, EffectivePermissionProfileSource::Lane);
}

#[test]
fn effective_permission_profile_resolves_custom_launcher_profile() {
    let mut config = test_config();
    config.launchers.get_mut("custom-launcher").unwrap().profile =
        Some(PermissionProfile::Dependency);

    let effective = config.effective_permission_profile(
        PermissionProfileRole::Custom,
        Some("custom-launcher"),
        None,
    );
    assert_eq!(effective.profile, PermissionProfile::Dependency);
    assert_eq!(effective.source, EffectivePermissionProfileSource::Launcher);
}

#[test]
fn effective_permission_profile_agent_override_wins_and_returns_unsafe_spec() {
    let mut config = test_config();
    config.launchers.get_mut("codex").unwrap().profile = Some(PermissionProfile::Dependency);
    config.lanes.get_mut("codex-worker").unwrap().profile = Some(PermissionProfile::Workspace);
    config.profiles.specs.insert(
        "unsafe".into(),
        SandboxSpec {
            backend: SandboxBackend::None,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_class: NetworkClass::Unrestricted,
            credential_class: CredentialClass::Unrestricted,
            env_policy: EnvPolicy::Inherit,
            unsafe_marker: true,
        },
    );

    let effective = config.effective_permission_profile(
        PermissionProfileRole::Worker,
        Some("codex-worker"),
        Some(PermissionProfile::Unsafe),
    );
    assert_eq!(effective.profile, PermissionProfile::Unsafe);
    assert_eq!(
        effective.source,
        EffectivePermissionProfileSource::AgentOverride
    );
    assert_eq!(effective.spec, config.profiles.specs.get("unsafe"));
    assert!(effective.spec.unwrap().unsafe_marker);
}

#[test]
fn effective_permission_profile_custom_uses_builtin_observe_fallback() {
    let config = test_config();

    let effective = config.effective_permission_profile(PermissionProfileRole::Custom, None, None);
    assert_eq!(effective.profile, PermissionProfile::Observe);
    assert_eq!(
        effective.source,
        EffectivePermissionProfileSource::BuiltInRoleDefault
    );
}

#[test]
fn effective_permission_profile_custom_uses_configured_default() {
    let mut config = test_config();
    config
        .profiles
        .defaults
        .insert("custom".into(), PermissionProfile::Workspace);

    let effective = config.effective_permission_profile(PermissionProfileRole::Custom, None, None);
    assert_eq!(effective.profile, PermissionProfile::Workspace);
    assert_eq!(
        effective.source,
        EffectivePermissionProfileSource::ConfigRoleDefault
    );
}

#[test]
fn effective_permission_profile_lane_with_missing_launcher_does_not_fallback_to_same_named_launcher(
) {
    let mut config = test_config();
    // Add a legacy launcher whose name matches the lane we're about to create.
    config.launchers.insert(
        "orphan-lane".into(),
        AgentConnectionConfig {
            adapter: AdapterKind::Acp,
            command: Some("orphan-agent".into()),
            args: Vec::new(),
            provider: None,
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: Some(PermissionProfile::Operator),
            max_parallel_tool_calls: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: HashMap::new(),
            headers: HashMap::new(),
        },
    );
    // Add a lane that references a nonexistent launcher key.
    config.lanes.insert(
        "orphan-lane".into(),
        LaneConfig {
            launcher: "missing-launcher".into(),
            model: None,
            reasoning_effort: None,
            system_prompt: None,
            profile: None,
        },
    );

    let effective = config.effective_permission_profile(
        PermissionProfileRole::Worker,
        Some("orphan-lane"),
        None,
    );
    // Should fall through to role default, NOT pick up the "orphan-lane" launcher's Operator profile.
    assert_eq!(effective.profile, PermissionProfile::Workspace);
    assert_eq!(
        effective.source,
        EffectivePermissionProfileSource::BuiltInRoleDefault
    );
}

#[test]
fn effective_permission_profile_source_display_is_human_readable() {
    assert_eq!(
        EffectivePermissionProfileSource::AgentOverride.to_string(),
        "Agent Override"
    );
    assert_eq!(
        EffectivePermissionProfileSource::Lane.to_string(),
        "Lane Override"
    );
    assert_eq!(
        EffectivePermissionProfileSource::Launcher.to_string(),
        "Launcher Override"
    );
    assert_eq!(
        EffectivePermissionProfileSource::ConfigRoleDefault.to_string(),
        "Config Role Default"
    );
    assert_eq!(
        EffectivePermissionProfileSource::BuiltInRoleDefault.to_string(),
        "Built-in Role Default"
    );
}

#[test]
fn profiles_config_role_default_present_and_absent() {
    let mut profiles = ProfilesConfig::default();
    profiles
        .defaults
        .insert("worker".into(), PermissionProfile::Dependency);

    assert_eq!(
        profiles.role_default(PermissionProfileRole::Worker),
        Some(PermissionProfile::Dependency)
    );
    assert_eq!(
        profiles.role_default(PermissionProfileRole::Supervisor),
        None
    );
    assert_eq!(profiles.role_default(PermissionProfileRole::Advisor), None);
}

#[test]
fn profiles_config_spec_for_present_and_absent() {
    let mut profiles = ProfilesConfig::default();
    let spec = SandboxSpec {
        backend: SandboxBackend::None,
        read_roots: Vec::new(),
        write_roots: Vec::new(),
        denied_roots: Vec::new(),
        network_class: NetworkClass::Unrestricted,
        credential_class: CredentialClass::Unrestricted,
        env_policy: EnvPolicy::Inherit,
        unsafe_marker: false,
    };
    profiles.specs.insert("workspace".into(), spec.clone());

    assert_eq!(profiles.spec_for(PermissionProfile::Workspace), Some(&spec));
    assert_eq!(profiles.spec_for(PermissionProfile::Unsafe), None);
}

#[test]
fn effective_permission_profile_returns_spec_when_configured() {
    let mut config = test_config();
    config.profiles.specs.insert(
        "workspace".into(),
        SandboxSpec {
            backend: SandboxBackend::OsDefault,
            read_roots: vec![FsRootSpec {
                path: ".".into(),
                recursive: true,
            }],
            write_roots: vec![FsRootSpec {
                path: ".brehon/runtime".into(),
                recursive: true,
            }],
            denied_roots: vec![FsRootSpec {
                path: "/etc".into(),
                recursive: false,
            }],
            network_class: NetworkClass::Allowlisted,
            credential_class: CredentialClass::KeychainRead,
            env_policy: EnvPolicy::Strict,
            unsafe_marker: false,
        },
    );

    let eff = config.effective_permission_profile(PermissionProfileRole::Worker, None, None);
    assert_eq!(eff.profile, PermissionProfile::Workspace);
    assert!(eff.spec.is_some());
    let spec = eff.spec.unwrap();
    assert_eq!(spec.backend, SandboxBackend::OsDefault);
    assert_eq!(spec.network_class, NetworkClass::Allowlisted);
    assert!(!spec.unsafe_marker);

    // When profile is resolved but no spec exists, spec is None
    config.profiles.specs.clear();
    let eff = config.effective_permission_profile(PermissionProfileRole::Worker, None, None);
    assert_eq!(eff.profile, PermissionProfile::Workspace);
    assert!(eff.spec.is_none());
}

#[test]
fn permission_profile_serialization_roundtrip() {
    for profile in PermissionProfile::variants() {
        let json = serde_json::to_string(&profile).unwrap();
        let parsed: PermissionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, parsed, "roundtrip failed for {:?}", profile);
        assert_eq!(parsed.as_str(), profile.as_str());
    }
}

#[test]
fn permission_profile_rejects_unknown_variant() {
    let err = serde_json::from_str::<PermissionProfile>("\"not_a_profile\"").unwrap_err();
    assert!(err.to_string().contains("unknown variant"));
}

#[test]
fn profiles_config_defaults_to_empty() {
    let parsed: ProfilesConfig = serde_json::from_str("{}").unwrap();
    assert!(parsed.defaults.is_empty());
    assert!(parsed.specs.is_empty());
    assert!(parsed.is_default());
}

#[test]
fn sandbox_spec_roundtrip() {
    let spec = SandboxSpec {
        backend: SandboxBackend::Bubblewrap,
        read_roots: vec![FsRootSpec {
            path: ".".into(),
            recursive: true,
        }],
        write_roots: vec![FsRootSpec {
            path: ".brehon/runtime".into(),
            recursive: true,
        }],
        denied_roots: vec![FsRootSpec {
            path: "/etc".into(),
            recursive: false,
        }],
        network_class: NetworkClass::Allowlisted,
        credential_class: CredentialClass::KeychainRead,
        env_policy: EnvPolicy::Strict,
        unsafe_marker: true,
    };
    let json = serde_json::to_string(&spec).unwrap();
    let parsed: SandboxSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(spec, parsed);
}
