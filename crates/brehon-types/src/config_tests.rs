use std::collections::HashMap;

use crate::*;

#[test]
fn lane_reasoning_effort_prefers_override_then_lane_default() {
    let config = BrehonConfig {
        version: 1,
        launchers: HashMap::new(),
        lanes: HashMap::from([(
            "codex-worker".into(),
            LaneConfig {
                launcher: "codex".into(),
                model: None,
                reasoning_effort: Some("high".into()),
                system_prompt: None,
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
        retention: RetentionConfig::default(),
        security: SecurityConfig {
            sandbox_profile: SandboxProfile::OsDefault,
            persist_transcripts: true,
            redact_patterns: Vec::new(),
            env_allowlist: Vec::new(),
        },
    };

    assert_eq!(
        config.lane_reasoning_effort("codex-worker", None),
        Some("high")
    );
    assert_eq!(
        config.lane_reasoning_effort("codex-worker", Some("xhigh")),
        Some("xhigh")
    );
}
