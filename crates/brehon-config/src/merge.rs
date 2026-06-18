//! Configuration merging logic.
//!
//! Config layers are merged with specific semantics:
//! - Scalars: overlay overrides base
//! - Arrays: overlay replaces base (no deep merge)
//! - Maps: overlay merges into base recursively
//! - Option fields: overlay wins if set

use std::collections::HashMap;

use brehon_types::{
    AdvisorConfig, AgentConnectionConfig, BrehonConfig, BudgetConfig, ContextConfig,
    EscalationConfig, LaneConfig, NotificationConfig, NudgeConfig, OrchestrationConfig,
    PermissionsConfig, PromptFragmentConfig, PromptPolicyConfig, ResearchConfig, RetentionConfig,
    ReviewConfig, ReviewerPoolConfig, RolesConfig, RuntimeConfig, SecurityConfig,
    StaleDetectionConfig, StuckDetectionConfig, SupervisorConfig, TuiConfig, WorkerPoolConfig,
};

/// Merge two configs, with `overlay` taking precedence over `base`.
pub fn merge_configs(base: BrehonConfig, overlay: BrehonConfig) -> BrehonConfig {
    BrehonConfig {
        version: base.version,
        launchers: merge_agents(base.launchers, overlay.launchers),
        lanes: merge_lanes(base.lanes, overlay.lanes),
        prompt_fragments: merge_prompt_fragments(base.prompt_fragments, overlay.prompt_fragments),
        prompt_policy: merge_prompt_policy(base.prompt_policy, overlay.prompt_policy),
        roles: merge_roles(base.roles, overlay.roles),
        routing: if overlay.routing.is_default() {
            base.routing
        } else {
            overlay.routing
        },
        advisors: merge_advisors(base.advisors, overlay.advisors),
        research: merge_research(base.research, overlay.research),
        review: merge_review(base.review, overlay.review),
        supervisor: merge_supervisor(base.supervisor, overlay.supervisor),
        orchestration: merge_orchestration(base.orchestration, overlay.orchestration),
        runtime: merge_runtime(base.runtime, overlay.runtime),
        budget: merge_budget(base.budget, overlay.budget),
        tui: merge_tui(base.tui, overlay.tui),
        escalation: merge_escalation(base.escalation, overlay.escalation),
        context: merge_context(base.context, overlay.context),
        permissions: merge_permissions(base.permissions, overlay.permissions),
        profiles: merge_profiles(base.profiles, overlay.profiles),
        retention: merge_retention(base.retention, overlay.retention),
        security: merge_security(base.security, overlay.security),
    }
}

fn merge_agents(
    base: HashMap<String, AgentConnectionConfig>,
    overlay: HashMap<String, AgentConnectionConfig>,
) -> HashMap<String, AgentConnectionConfig> {
    let mut merged = base;
    for (key, value) in overlay {
        merged.insert(key, value);
    }
    merged
}

fn merge_lanes(
    base: HashMap<String, LaneConfig>,
    overlay: HashMap<String, LaneConfig>,
) -> HashMap<String, LaneConfig> {
    let mut merged = base;
    for (key, value) in overlay {
        merged.insert(key, value);
    }
    merged
}

fn merge_prompt_fragments(
    base: HashMap<String, PromptFragmentConfig>,
    overlay: HashMap<String, PromptFragmentConfig>,
) -> HashMap<String, PromptFragmentConfig> {
    let mut merged = base;
    for (key, value) in overlay {
        merged.insert(key, value);
    }
    merged
}

fn merge_prompt_policy(
    base: PromptPolicyConfig,
    overlay: PromptPolicyConfig,
) -> PromptPolicyConfig {
    PromptPolicyConfig {
        enabled: if overlay.enabled.is_empty() {
            base.enabled
        } else {
            overlay.enabled
        },
    }
}

fn merge_roles(base: RolesConfig, overlay: RolesConfig) -> RolesConfig {
    RolesConfig {
        supervisor: overlay.supervisor,
        workers: merge_worker_pools(base.workers, overlay.workers),
        reviewers: merge_reviewer_pools(base.reviewers, overlay.reviewers),
    }
}

fn merge_worker_pools(
    base: Vec<WorkerPoolConfig>,
    overlay: Vec<WorkerPoolConfig>,
) -> Vec<WorkerPoolConfig> {
    if overlay.is_empty() {
        return base;
    }
    overlay
}

fn merge_reviewer_pools(
    base: Vec<ReviewerPoolConfig>,
    overlay: Vec<ReviewerPoolConfig>,
) -> Vec<ReviewerPoolConfig> {
    if overlay.is_empty() {
        return base;
    }
    overlay
}

fn merge_advisors(base: AdvisorConfig, overlay: AdvisorConfig) -> AdvisorConfig {
    if overlay.is_default() {
        base
    } else {
        overlay
    }
}

fn merge_research(base: ResearchConfig, overlay: ResearchConfig) -> ResearchConfig {
    if overlay.is_default() {
        base
    } else {
        overlay
    }
}

fn merge_review(base: ReviewConfig, overlay: ReviewConfig) -> ReviewConfig {
    ReviewConfig {
        policy: overlay.policy,
        timeout_minutes: overlay.timeout_minutes,
        auto_assign: overlay.auto_assign,
        default_reviewers: if overlay.default_reviewers.is_empty() {
            base.default_reviewers
        } else {
            overlay.default_reviewers
        },
        panel_mode: overlay.panel_mode,
        lease_mode: overlay.lease_mode,
        panels: if overlay.panels.is_empty() {
            base.panels
        } else {
            overlay.panels
        },
        max_diff_tokens: overlay.max_diff_tokens,
        chunk_strategy: overlay.chunk_strategy,
        stale_detection: merge_stale_detection(base.stale_detection, overlay.stale_detection),
    }
}

fn merge_stale_detection(
    base: StaleDetectionConfig,
    overlay: StaleDetectionConfig,
) -> StaleDetectionConfig {
    StaleDetectionConfig {
        enabled: overlay.enabled,
        ignore_files: if overlay.ignore_files.is_empty() {
            base.ignore_files
        } else {
            overlay.ignore_files
        },
        strategy: overlay.strategy,
    }
}

fn merge_supervisor(base: SupervisorConfig, overlay: SupervisorConfig) -> SupervisorConfig {
    SupervisorConfig {
        model: overlay.model.or(base.model),
        reasoning_effort: overlay.reasoning_effort.or(base.reasoning_effort),
        autonomy: overlay.autonomy,
        heartbeat_minutes: overlay.heartbeat_minutes,
        stuck_detection: merge_stuck_detection(base.stuck_detection, overlay.stuck_detection),
        nudge: merge_nudge(base.nudge, overlay.nudge),
    }
}

fn merge_stuck_detection(
    _base: StuckDetectionConfig,
    overlay: StuckDetectionConfig,
) -> StuckDetectionConfig {
    StuckDetectionConfig {
        time_threshold_minutes: overlay.time_threshold_minutes,
        operation_aware: overlay.operation_aware,
        pattern_detection: overlay.pattern_detection,
    }
}

fn merge_nudge(_base: NudgeConfig, overlay: NudgeConfig) -> NudgeConfig {
    NudgeConfig {
        soft_after_minutes: overlay.soft_after_minutes,
        guidance_after_minutes: overlay.guidance_after_minutes,
    }
}

fn merge_orchestration(
    base: OrchestrationConfig,
    overlay: OrchestrationConfig,
) -> OrchestrationConfig {
    OrchestrationConfig {
        max_active_workers: overlay.max_active_workers,
        worktree_isolation: overlay.worktree_isolation,
        branch_prefix: overlay.branch_prefix,
        auto_cleanup_worktrees: overlay.auto_cleanup_worktrees,
        worker_idle_behavior: overlay.worker_idle_behavior,
        allow_mutating_idle_work: overlay.allow_mutating_idle_work,
        self_improve_tasks: if overlay.self_improve_tasks.is_empty() {
            base.self_improve_tasks
        } else {
            overlay.self_improve_tasks
        },
        spawn_workers: overlay.spawn_workers.or(base.spawn_workers),
        drain_timeout_secs: overlay.drain_timeout_secs.or(base.drain_timeout_secs),
        worktree_root: overlay.worktree_root.or(base.worktree_root),
        cargo_target_root: overlay.cargo_target_root.or(base.cargo_target_root),
        worktree_cleanup: overlay.worktree_cleanup,
    }
}

fn merge_runtime(base: RuntimeConfig, overlay: RuntimeConfig) -> RuntimeConfig {
    RuntimeConfig {
        enabled_workflows: if overlay.enabled_workflows.is_empty() {
            base.enabled_workflows
        } else {
            overlay.enabled_workflows
        },
        terminal_host: merge_runtime_terminal_host(base.terminal_host, overlay.terminal_host),
        retry: overlay.retry,
        continuation: overlay.continuation,
    }
}

fn merge_runtime_terminal_host(
    base: brehon_types::RuntimeTerminalHostConfig,
    overlay: brehon_types::RuntimeTerminalHostConfig,
) -> brehon_types::RuntimeTerminalHostConfig {
    brehon_types::RuntimeTerminalHostConfig {
        kind: overlay.kind.or(base.kind),
        preview_pane: overlay.preview_pane.or(base.preview_pane),
        pane_ownership: overlay.pane_ownership.or(base.pane_ownership),
    }
}

fn merge_budget(base: BudgetConfig, overlay: BudgetConfig) -> BudgetConfig {
    BudgetConfig {
        max_total_cost: overlay.max_total_cost.or(base.max_total_cost),
        max_cost_per_task: overlay.max_cost_per_task.or(base.max_cost_per_task),
        max_tokens_per_agent: overlay.max_tokens_per_agent.or(base.max_tokens_per_agent),
        alert_threshold_percent: overlay.alert_threshold_percent,
        enforcement: overlay.enforcement,
        max_wall_clock_minutes: overlay
            .max_wall_clock_minutes
            .or(base.max_wall_clock_minutes),
    }
}

fn merge_tui(base: TuiConfig, overlay: TuiConfig) -> TuiConfig {
    TuiConfig {
        default_layout: overlay.default_layout,
        terminal_mode: overlay.terminal_mode,
        notifications: merge_notifications(base.notifications, overlay.notifications),
        keybindings: if overlay.keybindings.is_empty() {
            base.keybindings
        } else {
            overlay.keybindings
        },
    }
}

fn merge_notifications(
    _base: NotificationConfig,
    overlay: NotificationConfig,
) -> NotificationConfig {
    NotificationConfig {
        toast_duration_seconds: overlay.toast_duration_seconds,
        flash_tabs: overlay.flash_tabs,
        modal_on_critical: overlay.modal_on_critical,
    }
}

fn merge_escalation(_base: EscalationConfig, overlay: EscalationConfig) -> EscalationConfig {
    EscalationConfig {
        human_in_loop: overlay.human_in_loop,
        notify_via: overlay.notify_via,
        escalation_timeout_minutes: overlay.escalation_timeout_minutes,
    }
}

fn merge_context(base: ContextConfig, overlay: ContextConfig) -> ContextConfig {
    ContextConfig {
        db_path: if overlay.db_path.is_empty() {
            base.db_path
        } else {
            overlay.db_path
        },
        search_index_path: if overlay.search_index_path.is_empty() {
            base.search_index_path
        } else {
            overlay.search_index_path
        },
        memory_ttl_days: overlay.memory_ttl_days.or(base.memory_ttl_days),
        max_memories: overlay.max_memories,
        agents_md: overlay.agents_md,
        retrieval: overlay.retrieval,
        compression: overlay.compression,
    }
}

fn merge_permissions(base: PermissionsConfig, overlay: PermissionsConfig) -> PermissionsConfig {
    let mut merged = base.categories;
    for (key, value) in overlay.categories {
        merged.insert(key, value);
    }
    PermissionsConfig { categories: merged }
}

fn merge_profiles(
    base: brehon_types::ProfilesConfig,
    overlay: brehon_types::ProfilesConfig,
) -> brehon_types::ProfilesConfig {
    let mut defaults = base.defaults;
    for (key, value) in overlay.defaults {
        defaults.insert(key, value);
    }
    let mut specs = base.specs;
    for (key, value) in overlay.specs {
        specs.insert(key, value);
    }
    brehon_types::ProfilesConfig { defaults, specs }
}

fn merge_retention(base: RetentionConfig, overlay: RetentionConfig) -> RetentionConfig {
    RetentionConfig {
        max_events: overlay.max_events.or(base.max_events),
        idempotency_ttl_hours: overlay.idempotency_ttl_hours.or(base.idempotency_ttl_hours),
        max_completed_tasks: if overlay.max_completed_tasks != 0 {
            overlay.max_completed_tasks
        } else {
            base.max_completed_tasks
        },
        max_assignment_history: if overlay.max_assignment_history != 0 {
            overlay.max_assignment_history
        } else {
            base.max_assignment_history
        },
        max_tasks: if overlay.max_tasks != 0 {
            overlay.max_tasks
        } else {
            base.max_tasks
        },
        sweep_interval_secs: if overlay.sweep_interval_secs != 0 {
            overlay.sweep_interval_secs
        } else {
            base.sweep_interval_secs
        },
    }
}

fn merge_security(base: SecurityConfig, overlay: SecurityConfig) -> SecurityConfig {
    SecurityConfig {
        sandbox_profile: overlay.sandbox_profile,
        persist_transcripts: overlay.persist_transcripts,
        redact_patterns: if overlay.redact_patterns.is_empty() {
            base.redact_patterns
        } else {
            overlay.redact_patterns
        },
        env_allowlist: if overlay.env_allowlist.is_empty() {
            base.env_allowlist
        } else {
            overlay.env_allowlist
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{
        AdapterKind, AgentsMdMode, AutonomyLevel, BudgetEnforcement, ChunkStrategy,
        CredentialClass, EnvPolicy, LayoutPreset, NetworkClass, NotifyMethod, Permission,
        PermissionProfile, RoleDefinition, RoleKind, SandboxBackend, SandboxProfile, SandboxSpec,
        StaleStrategy, TerminalMode, WorkerIdleBehavior,
    };
    use std::collections::HashMap;

    fn create_base_config() -> BrehonConfig {
        BrehonConfig {
            version: 1,
            launchers: HashMap::new(),
            lanes: HashMap::new(),
            prompt_fragments: HashMap::new(),
            prompt_policy: brehon_types::PromptPolicyConfig::default(),
            roles: RolesConfig {
                supervisor: RoleDefinition {
                    name: "supervisor".into(),
                    kind: RoleKind::Supervisor,
                    description: "Base supervisor".into(),
                    permissions: vec![Permission::CreateTasks],
                    system_prompt: None,
                },
                workers: vec![],
                reviewers: vec![],
            },
            routing: brehon_types::RoutingConfig::default(),
            advisors: AdvisorConfig::default(),
            research: ResearchConfig::default(),
            review: ReviewConfig {
                policy: brehon_types::ReviewPolicy::default(),
                timeout_minutes: 30,
                auto_assign: true,
                default_reviewers: vec!["gemini".into()],
                panel_mode: brehon_types::ReviewPanelMode::FullCouncil,
                lease_mode: brehon_types::config::ReviewLeaseMode::Exclusive,
                panels: vec![brehon_types::ReviewPanelConfig {
                    id: "primary".into(),
                    reviewers: vec!["gemini".into()],
                }],
                max_diff_tokens: 8000,
                chunk_strategy: ChunkStrategy::ByDirectory,
                stale_detection: StaleDetectionConfig {
                    enabled: true,
                    ignore_files: vec!["Cargo.lock".into()],
                    strategy: StaleStrategy::DeltaReview,
                },
            },
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
                max_active_workers: 3,
                worktree_isolation: true,
                branch_prefix: "brehon/".into(),
                auto_cleanup_worktrees: true,
                worker_idle_behavior: WorkerIdleBehavior::SelfImprove,
                allow_mutating_idle_work: false,
                self_improve_tasks: vec!["run_tests".into()],
                spawn_workers: None,
                drain_timeout_secs: None,
                worktree_root: None,
                cargo_target_root: None,
                worktree_cleanup: brehon_types::WorktreeCleanupConfig::default(),
            },
            runtime: RuntimeConfig::default(),
            budget: BudgetConfig {
                max_total_cost: None,
                max_cost_per_task: None,
                max_tokens_per_agent: None,
                alert_threshold_percent: 80,
                enforcement: BudgetEnforcement::Soft,
                max_wall_clock_minutes: None,
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
                max_memories: 10000,
                agents_md: AgentsMdMode::Auto,
                retrieval: brehon_types::config::ContextRetrievalConfig::default(),
                compression: brehon_types::config::ContextCompressionConfig::default(),
            },
            permissions: PermissionsConfig {
                categories: HashMap::new(),
            },
            profiles: brehon_types::ProfilesConfig::default(),
            retention: RetentionConfig::default(),
            security: SecurityConfig {
                sandbox_profile: SandboxProfile::OsDefault,
                persist_transcripts: true,
                redact_patterns: vec![],
                env_allowlist: vec!["PATH".into()],
            },
        }
    }

    #[test]
    fn merge_scalar_override() {
        let base = create_base_config();
        let mut overlay = create_base_config();
        overlay.supervisor.heartbeat_minutes = 30;

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.supervisor.heartbeat_minutes, 30);
    }

    #[test]
    fn merge_array_replace() {
        let base = create_base_config();
        let mut overlay = create_base_config();
        overlay.review.default_reviewers = vec!["claude-code".into(), "codex".into()];

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.review.default_reviewers.len(), 2);
        assert!(merged
            .review
            .default_reviewers
            .contains(&"claude-code".to_string()));
    }

    #[test]
    fn merge_map_entries() {
        let base = create_base_config();
        let mut overlay = create_base_config();
        overlay.launchers.insert(
            "new-agent".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::Acp,
                command: Some("new-agent".into()),
                args: vec![],
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
        );

        let merged = merge_configs(base, overlay);
        assert!(merged.launchers.contains_key("new-agent"));
    }

    #[test]
    fn merge_option_field() {
        let base = create_base_config();
        let mut overlay = create_base_config();
        overlay.budget.max_total_cost = Some(100.0);

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.budget.max_total_cost, Some(100.0));
    }

    #[test]
    fn merge_preserves_base_when_overlay_empty() {
        let base = create_base_config();
        let overlay = create_base_config();

        let merged = merge_configs(base.clone(), overlay);
        assert_eq!(
            merged.orchestration.self_improve_tasks,
            base.orchestration.self_improve_tasks
        );
    }

    #[test]
    fn merge_runtime_enabled_workflows_replace_when_overlay_sets_values() {
        let mut base = create_base_config();
        base.runtime.enabled_workflows = vec!["rate_limit.quarantine_recommendation".into()];
        let mut overlay = create_base_config();
        overlay.runtime.enabled_workflows = vec!["custom.workflow".into()];

        let merged = merge_configs(base, overlay);

        assert_eq!(merged.runtime.enabled_workflows, vec!["custom.workflow"]);
    }

    #[test]
    fn merge_runtime_enabled_workflows_inherit_when_overlay_empty() {
        let mut base = create_base_config();
        base.runtime.enabled_workflows = vec!["rate_limit.quarantine_recommendation".into()];
        let overlay = create_base_config();

        let merged = merge_configs(base, overlay);

        assert_eq!(
            merged.runtime.enabled_workflows,
            vec!["rate_limit.quarantine_recommendation"]
        );
    }

    #[test]
    fn merge_runtime_terminal_host_inherits_when_overlay_omits_selection() {
        let mut base = create_base_config();
        base.runtime.terminal_host.kind = Some(brehon_types::RuntimeTerminalHostKind::Headless);
        base.runtime.terminal_host.preview_pane = Some(true);
        base.runtime.terminal_host.pane_ownership =
            Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host);
        let overlay = create_base_config();

        let merged = merge_configs(base, overlay);

        assert_eq!(
            merged.runtime.terminal_host.kind,
            Some(brehon_types::RuntimeTerminalHostKind::Headless)
        );
        assert_eq!(merged.runtime.terminal_host.preview_pane, Some(true));
        assert_eq!(
            merged.runtime.terminal_host.pane_ownership,
            Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host)
        );
    }

    #[test]
    fn merge_runtime_terminal_host_overlay_replaces_selection_fields() {
        let mut base = create_base_config();
        base.runtime.terminal_host.kind = Some(brehon_types::RuntimeTerminalHostKind::Headless);
        base.runtime.terminal_host.preview_pane = Some(false);
        base.runtime.terminal_host.pane_ownership =
            Some(brehon_types::RuntimeTerminalHostPaneOwnership::Mux);
        let mut overlay = create_base_config();
        overlay.runtime.terminal_host.kind = Some(brehon_types::RuntimeTerminalHostKind::Embedded);
        overlay.runtime.terminal_host.preview_pane = Some(true);
        overlay.runtime.terminal_host.pane_ownership =
            Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host);

        let merged = merge_configs(base, overlay);

        assert_eq!(
            merged.runtime.terminal_host.kind,
            Some(brehon_types::RuntimeTerminalHostKind::Embedded)
        );
        assert_eq!(merged.runtime.terminal_host.preview_pane, Some(true));
        assert_eq!(
            merged.runtime.terminal_host.pane_ownership,
            Some(brehon_types::RuntimeTerminalHostPaneOwnership::Host)
        );
    }

    #[test]
    fn merge_nested_struct() {
        let base = create_base_config();
        let mut overlay = create_base_config();
        overlay.supervisor.stuck_detection.time_threshold_minutes = 20;

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.supervisor.stuck_detection.time_threshold_minutes, 20);
        assert!(merged.supervisor.stuck_detection.operation_aware);
    }

    #[test]
    fn merge_permissions_hashmap() {
        let mut base = create_base_config();
        base.permissions.categories.insert(
            "bash".into(),
            brehon_types::PermissionCategory::Simple(brehon_types::PermissionValue::Ask),
        );

        let mut overlay = create_base_config();
        overlay.permissions.categories.insert(
            "bash".into(),
            brehon_types::PermissionCategory::Simple(brehon_types::PermissionValue::Allow),
        );

        let merged = merge_configs(base, overlay);
        assert_eq!(
            merged.permissions.categories.get("bash"),
            Some(&brehon_types::PermissionCategory::Simple(
                brehon_types::PermissionValue::Allow
            ))
        );
    }

    #[test]
    fn merge_profiles_overlay_replaces_existing_profile_spec() {
        let mut base = create_base_config();
        base.profiles
            .defaults
            .insert("worker".into(), PermissionProfile::Workspace);
        base.profiles.specs.insert(
            "workspace".into(),
            SandboxSpec {
                backend: SandboxBackend::OsDefault,
                read_roots: Vec::new(),
                write_roots: Vec::new(),
                denied_roots: Vec::new(),
                network_class: NetworkClass::ModelOnly,
                credential_class: CredentialClass::EnvAllowlist,
                env_policy: EnvPolicy::Minimal,
                unsafe_marker: false,
            },
        );

        let mut overlay = create_base_config();
        overlay
            .profiles
            .defaults
            .insert("worker".into(), PermissionProfile::Dependency);
        overlay.profiles.specs.insert(
            "workspace".into(),
            SandboxSpec {
                backend: SandboxBackend::Bubblewrap,
                read_roots: Vec::new(),
                write_roots: Vec::new(),
                denied_roots: Vec::new(),
                network_class: NetworkClass::Allowlisted,
                credential_class: CredentialClass::KeychainRead,
                env_policy: EnvPolicy::Strict,
                unsafe_marker: false,
            },
        );

        let merged = merge_configs(base, overlay);

        assert_eq!(
            merged.profiles.defaults.get("worker"),
            Some(&PermissionProfile::Dependency)
        );
        assert_eq!(
            merged.profiles.specs.get("workspace"),
            Some(&SandboxSpec {
                backend: SandboxBackend::Bubblewrap,
                read_roots: Vec::new(),
                write_roots: Vec::new(),
                denied_roots: Vec::new(),
                network_class: NetworkClass::Allowlisted,
                credential_class: CredentialClass::KeychainRead,
                env_policy: EnvPolicy::Strict,
                unsafe_marker: false,
            })
        );
    }

    #[test]
    fn merge_drain_timeout_secs_overlay_none_falls_through_to_base() {
        let mut base = create_base_config();
        base.orchestration.drain_timeout_secs = Some(60);

        let overlay = create_base_config(); // drain_timeout_secs: None

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.orchestration.drain_timeout_secs, Some(60));
        assert_eq!(merged.orchestration.effective_drain_timeout_secs(), 60);
    }

    #[test]
    fn merge_drain_timeout_secs_overlay_overrides_base() {
        let mut base = create_base_config();
        base.orchestration.drain_timeout_secs = Some(60);

        let mut overlay = create_base_config();
        overlay.orchestration.drain_timeout_secs = Some(10);

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.orchestration.drain_timeout_secs, Some(10));
        assert_eq!(merged.orchestration.effective_drain_timeout_secs(), 10);
    }

    #[test]
    fn merge_drain_timeout_secs_both_none_falls_to_default() {
        let base = create_base_config(); // drain_timeout_secs: None
        let overlay = create_base_config(); // drain_timeout_secs: None

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.orchestration.drain_timeout_secs, None);
        assert_eq!(merged.orchestration.effective_drain_timeout_secs(), 30);
    }

    #[test]
    fn merge_advisors_default_overlay_preserves_base() {
        let mut base = create_base_config();
        base.advisors.enabled = true;
        base.advisors.rooms.push(brehon_types::AdvisorRoomConfig {
            id: "release-war-room".into(),
            title: None,
            turn_mode: None,
            participants: Vec::new(),
            context: brehon_types::AdvisorRoomContextConfig::default(),
        });

        let overlay = create_base_config();

        let merged = merge_configs(base, overlay);
        assert!(merged.advisors.enabled);
        assert_eq!(merged.advisors.rooms[0].id, "release-war-room");
    }

    #[test]
    fn merge_retention_sweep_interval_secs_uses_overlay_when_set() {
        let mut base = create_base_config();
        base.retention.sweep_interval_secs = 60;

        let mut overlay = create_base_config();
        overlay.retention.sweep_interval_secs = 15;

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.retention.sweep_interval_secs, 15);
    }

    #[test]
    fn merge_retention_sweep_interval_secs_falls_back_to_base_when_overlay_unset() {
        let mut base = create_base_config();
        base.retention.sweep_interval_secs = 60;

        let overlay = create_base_config(); // 0 sentinel means "unset"

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.retention.sweep_interval_secs, 60);
    }
}
