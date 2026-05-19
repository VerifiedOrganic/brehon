use std::path::Path;

use anyhow::Result;
use tracing::info;

pub fn list(project_path: Option<&Path>, config_override: Option<&Path>) -> Result<()> {
    use brehon_config::load_config_with_override;

    let config = load_config_with_override(project_path, config_override)?;

    info!("Configuration values:");
    info!("  version: {}", config.version);

    info!("  launchers:");
    for (name, agent_config) in &config.launchers {
        match agent_config.adapter {
            brehon_types::agent::AdapterKind::OpenAiCompatible => info!(
                "    {}: base_url={:?} api_key_env={:?} headers={:?} {:?}",
                name,
                agent_config.base_url,
                agent_config.api_key_env,
                agent_config.headers,
                agent_config.adapter
            ),
            _ => info!(
                "    {}: {:?} {:?} {:?}",
                name, agent_config.command, agent_config.args, agent_config.adapter
            ),
        }
    }

    info!("  lanes:");
    for (name, lane_config) in &config.lanes {
        info!(
            "    {}: launcher={}, model={:?}",
            name, lane_config.launcher, lane_config.model
        );
    }

    info!("  roles:");
    info!("    supervisor: {:?}", config.roles.supervisor);
    for (i, worker_config) in config.roles.workers.iter().enumerate() {
        info!(
            "    worker[{}]: lane={}, min={}, max={}, assignment_mode={:?}, accepts={:?}",
            i,
            worker_config.lane,
            worker_config.min,
            worker_config.max,
            worker_config.assignment_mode,
            worker_config.accepts
        );
    }
    for (i, reviewer_config) in config.roles.reviewers.iter().enumerate() {
        info!(
            "    reviewer[{}]: lane={}, min={}, max={}",
            i, reviewer_config.lane, reviewer_config.min, reviewer_config.max
        );
    }

    info!("  review:");
    info!(
        "    policy: min_avg={}, min_ind={}, blocking={}, min_approvals={}",
        config.review.policy.min_average_score,
        config.review.policy.min_individual_score,
        config.review.policy.blocking_score,
        config.review.policy.min_approvals
    );
    info!("    timeout_minutes: {}", config.review.timeout_minutes);

    info!("  supervisor:");
    info!("    model: {:?}", config.supervisor.model);
    info!("    autonomy: {:?}", config.supervisor.autonomy);
    info!(
        "    heartbeat_minutes: {}",
        config.supervisor.heartbeat_minutes
    );

    info!("  orchestration:");
    info!(
        "    max_active_workers: {}",
        config.orchestration.max_active_workers
    );
    info!(
        "    worktree_isolation: {}",
        config.orchestration.worktree_isolation
    );
    info!(
        "    spawn_workers: {:?}",
        config.orchestration.spawn_workers
    );
    info!(
        "    drain_timeout_secs: {:?} (effective: {}s)",
        config.orchestration.drain_timeout_secs,
        config.orchestration.effective_drain_timeout_secs()
    );

    info!("  runtime:");
    info!(
        "    terminal_host: {:?}",
        config.runtime.terminal_host.effective_kind()
    );
    info!(
        "    enabled_workflows: {:?}",
        config.runtime.enabled_workflows
    );

    info!("  budget:");
    info!("    max_total_cost: {:?}", config.budget.max_total_cost);
    info!("    enforcement: {:?}", config.budget.enforcement);

    info!("  tui:");
    info!("    default_layout: {:?}", config.tui.default_layout);
    info!("    terminal_mode: {:?}", config.tui.terminal_mode);

    Ok(())
}

pub fn describe(
    project_path: Option<&Path>,
    config_override: Option<&Path>,
    key: &str,
) -> Result<()> {
    use brehon_config::load_config_with_override;

    let config = load_config_with_override(project_path, config_override)?;

    match key {
        "version" => info!("version: {}", config.version),

        "launchers" | "agents" => {
            info!("launchers:");
            for (name, agent_config) in &config.launchers {
                info!("  {}:", name);
                info!("    adapter: {:?}", agent_config.adapter);
                if let Some(command) = agent_config.command_str() {
                    info!("    command: {}", command);
                }
                if !agent_config.args.is_empty() {
                    info!("    args: {:?}", agent_config.args);
                }
                if let Some(base_url) = agent_config.base_url_str() {
                    info!("    base_url: {}", base_url);
                }
                if let Some(api_key_env) = agent_config.api_key_env_str() {
                    info!("    api_key_env: {}", api_key_env);
                }
                if !agent_config.headers.is_empty() {
                    info!("    headers: {:?}", agent_config.headers);
                }
            }
        }

        "lanes" => {
            info!("lanes:");
            for (name, lane_config) in &config.lanes {
                info!("  {}:", name);
                info!("    launcher: {}", lane_config.launcher);
                info!("    model: {:?}", lane_config.model);
                info!("    system_prompt: {:?}", lane_config.system_prompt);
            }
        }

        "roles" => {
            info!("roles:");
            info!("  supervisor:");
            info!("    name: {:?}", config.roles.supervisor.name);
            info!("    description: {:?}", config.roles.supervisor.description);
            info!("  workers:");
            for (i, w) in config.roles.workers.iter().enumerate() {
                info!(
                    "    [{}]: lane={}, min={}, max={}, assignment_mode={:?}, accepts={:?}",
                    i, w.lane, w.min, w.max, w.assignment_mode, w.accepts
                );
            }
            info!("  reviewers:");
            for (i, r) in config.roles.reviewers.iter().enumerate() {
                info!("    [{}]: lane={}, min={}, max={}", i, r.lane, r.min, r.max);
            }
        }

        "review" => {
            info!("review:");
            info!("  policy:");
            info!(
                "    min_average_score: {}",
                config.review.policy.min_average_score
            );
            info!(
                "    min_individual_score: {}",
                config.review.policy.min_individual_score
            );
            info!(
                "    blocking_score: {}",
                config.review.policy.blocking_score
            );
            info!("    min_approvals: {}", config.review.policy.min_approvals);
            info!("  timeout_minutes: {}", config.review.timeout_minutes);
            info!("  auto_assign: {}", config.review.auto_assign);
        }

        "supervisor" => {
            info!("supervisor:");
            info!("  model: {:?}", config.supervisor.model);
            info!("  autonomy: {:?}", config.supervisor.autonomy);
            info!(
                "  heartbeat_minutes: {}",
                config.supervisor.heartbeat_minutes
            );
            info!("  stuck_detection: {:?}", config.supervisor.stuck_detection);
        }

        "orchestration" => {
            info!("orchestration:");
            info!(
                "  max_active_workers: {}",
                config.orchestration.max_active_workers
            );
            info!(
                "  worktree_isolation: {}",
                config.orchestration.worktree_isolation
            );
            info!("  branch_prefix: {}", config.orchestration.branch_prefix);
            info!("  spawn_workers: {:?}", config.orchestration.spawn_workers);
        }

        "runtime" => {
            info!("runtime:");
            info!(
                "  enabled_workflows: {:?}",
                config.runtime.enabled_workflows
            );
            info!("  terminal_host:");
            info!(
                "    kind: {:?}",
                config.runtime.terminal_host.effective_kind()
            );
            info!(
                "    preview_pane: {}",
                config.runtime.terminal_host.preview_pane_enabled()
            );
            info!(
                "    pane_ownership: {:?}",
                config.runtime.terminal_host.effective_pane_ownership()
            );
            info!("    external_hosts: none");
        }

        "budget" => {
            info!("budget:");
            info!("  max_total_cost: {:?}", config.budget.max_total_cost);
            info!("  max_cost_per_task: {:?}", config.budget.max_cost_per_task);
            info!(
                "  max_tokens_per_agent: {:?}",
                config.budget.max_tokens_per_agent
            );
            info!("  enforcement: {:?}", config.budget.enforcement);
        }

        "tui" => {
            info!("tui:");
            info!("  default_layout: {:?}", config.tui.default_layout);
            info!("  terminal_mode: {:?}", config.tui.terminal_mode);
            info!("  notifications: {:?}", config.tui.notifications);
        }

        "context" => {
            info!("context:");
            info!("  db_path: {}", config.context.db_path);
            info!("  search_index_path: {}", config.context.search_index_path);
            info!("  memory_ttl_days: {:?}", config.context.memory_ttl_days);
            info!("  max_memories: {}", config.context.max_memories);
            info!("  agents_md: {:?}", config.context.agents_md);
            info!("  retrieval:");
            info!(
                "    default_limit: {}",
                config.context.retrieval.default_limit
            );
            info!("    max_limit: {}", config.context.retrieval.max_limit);
            info!(
                "    snippet_chars: {}",
                config.context.retrieval.snippet_chars
            );
            info!("  compression:");
            info!("    enabled: {}", config.context.compression.enabled);
            info!("    mode: {:?}", config.context.compression.mode);
            info!("    store_raw: {}", config.context.compression.store_raw);
            info!(
                "    compact_memories: {}",
                config.context.compression.compact_memories
            );
            info!(
                "    compact_rules: {}",
                config.context.compression.compact_rules
            );
            info!(
                "    compact_tasks: {}",
                config.context.compression.compact_tasks
            );
        }

        "security" => {
            info!("security:");
            info!("  sandbox_profile: {:?}", config.security.sandbox_profile);
            info!(
                "  persist_transcripts: {}",
                config.security.persist_transcripts
            );
        }

        _ => {
            anyhow::bail!("Unknown config key: {}. Valid keys: version, launchers, lanes, roles, review, supervisor, orchestration, runtime, budget, tui, context, security", key);
        }
    }

    Ok(())
}

pub fn validate(project_path: Option<&Path>, config_override: Option<&Path>) -> Result<()> {
    use brehon_config::{load_config_with_override, validate};

    let config = load_config_with_override(project_path, config_override)?;

    let warnings = validate(&config);

    if warnings.is_empty() {
        info!("Configuration is valid with no warnings");
    } else {
        info!("Configuration is valid but has warnings:");
        for warning in &warnings {
            tracing::warn!("  {}", warning);
        }
    }

    for (name, agent_config) in &config.launchers {
        if let Some(command) = agent_config.command_str() {
            let cmd_path: Result<std::path::PathBuf, _> = which::which(command);
            if cmd_path.is_err() {
                tracing::warn!("Agent '{}' command '{}' not found on PATH", name, command);
            }
        } else if matches!(
            agent_config.adapter,
            brehon_types::agent::AdapterKind::Acp | brehon_types::agent::AdapterKind::Mock
        ) {
            tracing::warn!(
                "Launcher '{}' uses adapter {:?} but has no command configured",
                name,
                agent_config.adapter
            );
        }
        if matches!(
            agent_config.adapter,
            brehon_types::agent::AdapterKind::OpenAiCompatible
        ) && agent_config.base_url_str().is_none()
        {
            tracing::warn!(
                "Launcher '{}' uses OpenAiCompatible but has no base_url configured",
                name
            );
        }
    }

    for worker in &config.roles.workers {
        if !config.has_lane(&worker.lane) {
            tracing::warn!("Worker references unknown lane: {}", worker.lane);
        }
        if worker.min > worker.max {
            tracing::warn!("Worker pool '{}' has min > max", worker.lane);
        }
        if worker.assignment_mode == brehon_types::WorkerAssignmentMode::Normal
            && !worker.accepts.is_empty()
        {
            tracing::warn!(
                "Worker pool '{}' has accepts entries but assignment_mode is normal",
                worker.lane
            );
        }
    }

    for reviewer in &config.roles.reviewers {
        if !config.has_lane(&reviewer.lane) {
            tracing::warn!("Reviewer references unknown lane: {}", reviewer.lane);
        }
        if reviewer.min > reviewer.max {
            tracing::warn!("Reviewer pool '{}' has min > max", reviewer.lane);
        }
    }

    if config.review.policy.min_average_score < config.review.policy.blocking_score {
        tracing::warn!(
            "min_average_score ({}) is less than blocking_score ({}), approval threshold may be unreachable",
            config.review.policy.min_average_score,
            config.review.policy.blocking_score
        );
    }

    if config.review.policy.min_individual_score < config.review.policy.blocking_score {
        tracing::warn!(
            "min_individual_score ({}) is less than blocking_score ({}), individual blockers may not trigger correctly",
            config.review.policy.min_individual_score,
            config.review.policy.blocking_score
        );
    }

    Ok(())
}
