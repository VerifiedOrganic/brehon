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

    info!("  notifications:");
    info!("    enabled: {}", config.notifications.enabled);
    info!(
        "    telegram_enabled: {}",
        config.notifications.providers.telegram.enabled
    );
    info!(
        "    subscriptions: {}",
        config.notifications.subscriptions.len()
    );

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
    if key == "profiles" {
        return profiles(project_path, config_override);
    }

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
            info!(
                "  alert_threshold_percent: {}",
                config.budget.alert_threshold_percent
            );
            info!("  enforcement: {:?}", config.budget.enforcement);
            info!(
                "  max_wall_clock_minutes: {:?}",
                config.budget.max_wall_clock_minutes
            );
        }

        "notifications" => {
            let telegram = &config.notifications.providers.telegram;
            info!("notifications:");
            info!("  enabled: {}", config.notifications.enabled);
            info!("  providers:");
            info!("    telegram:");
            info!("      enabled: {}", telegram.enabled);
            info!("      bot_token_env: {}", telegram.bot_token_env);
            info!("      chat_id_env: {}", telegram.chat_id_env);
            info!("      send_timeout_secs: {}", telegram.send_timeout_secs);
            info!("  subscriptions: {:?}", config.notifications.subscriptions);
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
            info!("    min_tokens: {}", config.context.compression.min_tokens);
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
            info!(
                "    prompt_contexts: {:?}",
                config.context.compression.prompt_contexts
            );
            info!(
                "    never_compress: {:?}",
                config.context.compression.never_compress
            );
            info!(
                "    headroom: {} {:?} timeout_ms={}",
                config.context.compression.headroom.command,
                config.context.compression.headroom.args,
                config.context.compression.headroom.timeout_ms
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
            anyhow::bail!("Unknown config key: {}. Valid keys: version, launchers, lanes, roles, review, supervisor, orchestration, runtime, budget, notifications, tui, context, security, profiles", key);
        }
    }

    Ok(())
}

fn collect_profile_entries<'a>(
    config: &'a brehon_types::BrehonConfig,
) -> Vec<(
    String,
    &'static str,
    brehon_types::EffectivePermissionProfile<'a>,
)> {
    let mut entries: Vec<(
        String,
        &'static str,
        brehon_types::EffectivePermissionProfile<'a>,
    )> = Vec::new();

    // Supervisor
    let sup_lane = &config.roles.supervisor.name;
    let sup_eff = config.effective_permission_profile(
        brehon_types::PermissionProfileRole::Supervisor,
        Some(sup_lane),
        None,
    );
    entries.push((sup_lane.clone(), "supervisor", sup_eff));

    // Workers
    for worker in &config.roles.workers {
        let eff = config.effective_permission_profile(
            brehon_types::PermissionProfileRole::Worker,
            Some(&worker.lane),
            None,
        );
        entries.push((worker.lane.clone(), "worker", eff));
    }

    // Reviewers
    for reviewer in &config.roles.reviewers {
        let eff = config.effective_permission_profile(
            brehon_types::PermissionProfileRole::Reviewer,
            Some(&reviewer.lane),
            None,
        );
        entries.push((reviewer.lane.clone(), "reviewer", eff));
    }

    // Advisor pools
    if config.advisors.enabled {
        for pool in &config.advisors.pools {
            let eff = config.effective_permission_profile(
                brehon_types::PermissionProfileRole::Advisor,
                Some(&pool.lane),
                None,
            );
            entries.push((pool.lane.clone(), "advisor", eff));
        }
    }

    // Research pools
    if config.research.enabled {
        for pool in &config.research.pools {
            let eff = config.effective_permission_profile(
                brehon_types::PermissionProfileRole::Research,
                Some(&pool.lane),
                None,
            );
            entries.push((pool.lane.clone(), "research", eff));
        }
    }

    entries
}

fn format_fs_roots(roots: &[brehon_types::FsRootSpec]) -> String {
    if roots.is_empty() {
        "[]".to_string()
    } else {
        roots
            .iter()
            .map(|r| format!("{}(r={})", r.path, r.recursive))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn profiles(project_path: Option<&Path>, config_override: Option<&Path>) -> Result<()> {
    use brehon_config::load_config_with_override;

    let config = load_config_with_override(project_path, config_override)?;
    let entries = collect_profile_entries(&config);

    info!("Resolved permission profiles:");

    for (lane, role, eff) in entries {
        let spec = eff.spec;
        let backend = spec
            .map(|s| s.backend.to_string())
            .unwrap_or_else(|| "(default)".to_string());
        let network = spec
            .map(|s| s.network_class.to_string())
            .unwrap_or_else(|| "(default)".to_string());
        let credential = spec
            .map(|s| s.credential_class.to_string())
            .unwrap_or_else(|| "(default)".to_string());
        let env_policy = spec
            .map(|s| s.env_policy.to_string())
            .unwrap_or_else(|| "(default)".to_string());
        let unsafe_status = eff.profile == brehon_types::PermissionProfile::Unsafe
            || spec.map(|s| s.unsafe_marker).unwrap_or(false);
        let read_roots = spec
            .map(|s| format_fs_roots(&s.read_roots))
            .unwrap_or_else(|| "(default)".to_string());
        let write_roots = spec
            .map(|s| format_fs_roots(&s.write_roots))
            .unwrap_or_else(|| "(default)".to_string());
        let denied_roots = spec
            .map(|s| format_fs_roots(&s.denied_roots))
            .unwrap_or_else(|| "(default)".to_string());
        info!("  {} (role={}):", lane, role);
        info!(
            "    profile: {} (source={})",
            eff.profile.as_str(),
            eff.source
        );
        info!("    backend: {}", backend);
        info!("    network_class: {}", network);
        info!("    credential_exposure: {}", credential);
        info!("    env_policy: {}", env_policy);
        info!("    unsafe: {}", unsafe_status);
        info!("    read_roots: {}", read_roots);
        info!("    write_roots: {}", write_roots);
        info!("    denied_roots: {}", denied_roots);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    struct LogCaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCaptureWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogCaptureWriter(self.0.clone())
        }
    }

    impl io::Write for LogCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("log capture mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs_at(max_level: tracing::Level, run: impl FnOnce()) -> String {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_ansi(false)
            .with_writer(LogCapture(captured.clone()))
            .with_max_level(max_level)
            .finish();

        tracing::subscriber::with_default(subscriber, run);

        let bytes = captured.lock().expect("log capture mutex poisoned").clone();
        String::from_utf8(bytes).expect("captured logs should be utf-8")
    }

    fn capture_info_logs(run: impl FnOnce()) -> String {
        capture_logs_at(tracing::Level::INFO, run)
    }

    fn load_config(yaml: &str) -> brehon_types::BrehonConfig {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, yaml).expect("write config");
        brehon_config::load_config_with_override(None, Some(&path)).expect("load config")
    }

    fn runtime_profile_config() -> &'static str {
        r#"
version: 1
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: ""
    permissions: []
  workers:
    - lane: codex-worker
      min: 1
      max: 3
  reviewers:
    - lane: claude-reviewer
      min: 1
      max: 2
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 1
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: []
  panel_mode: full_council
  lease_mode: exclusive
  panels: []
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: Wait
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  alert_threshold_percent: 80
  enforcement: Soft
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
permissions:
  categories: {}
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: []
lanes:
  advisor-claude:
    launcher: claude
  cheap-worker:
    launcher: codex
advisors:
  enabled: true
  pools:
    - lane: advisor-claude
      min: 1
      max: 2
research:
  enabled: true
  pools:
    - id: spec-research
      lane: cheap-worker
      role: normative_requirements
      min: 0
      max: 2
profiles:
  defaults:
    supervisor: operator
    worker: workspace
    reviewer: reviewer
  specs:
    observe:
      backend: os_default
      network_class: denied
      credential_class: none
      env_policy: minimal
      unsafe_marker: false
    workspace:
      backend: os_default
      network_class: model_only
      credential_class: env_allowlist
      env_policy: minimal
      unsafe_marker: false
    reviewer:
      backend: os_default
      network_class: model_only
      credential_class: env_allowlist
      env_policy: minimal
      unsafe_marker: false
    operator:
      backend: os_default
      network_class: model_only
      credential_class: env_allowlist
      env_policy: minimal
      unsafe_marker: false
"#
    }

    fn phase_gate_profile_config() -> &'static str {
        r#"
version: 1
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: ""
    permissions: []
  workers:
    - lane: codex-worker
      min: 1
      max: 3
    - lane: codex-unsafe
      min: 0
      max: 1
  reviewers:
    - lane: claude-reviewer
      min: 1
      max: 2
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 1
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: []
  panel_mode: full_council
  lease_mode: exclusive
  panels: []
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: Wait
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  alert_threshold_percent: 80
  enforcement: Soft
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
permissions:
  categories: {}
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: []
lanes:
  advisor-claude:
    launcher: claude
  claude-reviewer:
    launcher: claude
  codex-unsafe:
    launcher: codex
    profile: unsafe
  codex-worker:
    launcher: codex
  research-readonly:
    launcher: codex
  research-dependency:
    launcher: codex
    profile: dependency
advisors:
  enabled: true
  pools:
    - lane: advisor-claude
      min: 1
      max: 2
research:
  enabled: true
  pools:
    - id: readonly-research
      lane: research-readonly
      role: normative_requirements
      min: 0
      max: 2
    - id: dependency-research
      lane: research-dependency
      role: code_map
      min: 0
      max: 2
profiles:
  defaults:
    supervisor: operator
    worker: workspace
    reviewer: reviewer
  specs:
    observe:
      backend: os_default
      network_class: denied
      credential_class: none
      env_policy: minimal
      unsafe_marker: false
    dependency:
      backend: os_default
      network_class: allowlisted
      credential_class: env_allowlist
      env_policy: minimal
      unsafe_marker: false
    workspace:
      backend: os_default
      network_class: model_only
      credential_class: env_allowlist
      env_policy: minimal
      unsafe_marker: false
    reviewer:
      backend: os_default
      network_class: model_only
      credential_class: env_allowlist
      env_policy: minimal
      unsafe_marker: false
    operator:
      backend: os_default
      network_class: model_only
      credential_class: env_allowlist
      env_policy: minimal
      unsafe_marker: false
    unsafe:
      backend: none
      network_class: unrestricted
      credential_class: unrestricted
      env_policy: inherit
      unsafe_marker: true
"#
    }

    #[test]
    fn collect_profile_entries_includes_all_runtime_roles() {
        let config = load_config(runtime_profile_config());
        let entries = collect_profile_entries(&config);

        let lanes: Vec<_> = entries.iter().map(|(lane, _, _)| lane.as_str()).collect();
        assert!(
            lanes.contains(&"claude-supervisor"),
            "supervisor missing: {:?}",
            lanes
        );
        assert!(
            lanes.contains(&"codex-worker"),
            "worker missing: {:?}",
            lanes
        );
        assert!(
            lanes.contains(&"claude-reviewer"),
            "reviewer missing: {:?}",
            lanes
        );
        assert!(
            lanes.contains(&"advisor-claude"),
            "advisor missing: {:?}",
            lanes
        );
        assert!(
            lanes.contains(&"cheap-worker"),
            "research missing: {:?}",
            lanes
        );

        let roles: Vec<_> = entries.iter().map(|(_, role, _)| *role).collect();
        assert!(roles.contains(&"supervisor"));
        assert!(roles.contains(&"worker"));
        assert!(roles.contains(&"reviewer"));
        assert!(roles.contains(&"advisor"));
        assert!(roles.contains(&"research"));

        let sup = entries
            .iter()
            .find(|(_, role, _)| *role == "supervisor")
            .unwrap();
        assert_eq!(sup.2.profile.as_str(), "operator");
        assert_eq!(sup.2.spec.unwrap().backend.to_string(), "OS Default");
        assert_eq!(sup.2.spec.unwrap().network_class.to_string(), "Model Only");
        assert_eq!(
            sup.2.spec.unwrap().credential_class.to_string(),
            "Env Allowlist"
        );
        assert!(!sup.2.spec.unwrap().unsafe_marker);

        let advisor = entries
            .iter()
            .find(|(_, role, _)| *role == "advisor")
            .expect("advisor entry should exist");
        assert_eq!(advisor.2.profile.as_str(), "observe");
        assert_eq!(
            advisor.2.source,
            brehon_types::EffectivePermissionProfileSource::BuiltInRoleDefault
        );
        assert_eq!(
            advisor
                .2
                .spec
                .expect("advisor should resolve observe spec")
                .backend,
            brehon_types::SandboxBackend::OsDefault
        );
        assert_eq!(
            advisor
                .2
                .spec
                .expect("advisor should resolve observe spec")
                .network_class,
            brehon_types::NetworkClass::Denied
        );
        assert!(
            !advisor
                .2
                .spec
                .expect("advisor should resolve observe spec")
                .unsafe_marker
        );

        let research = entries
            .iter()
            .find(|(_, role, _)| *role == "research")
            .expect("research entry should exist");
        assert_eq!(research.2.profile.as_str(), "observe");
        assert_eq!(
            research.2.source,
            brehon_types::EffectivePermissionProfileSource::BuiltInRoleDefault
        );
        assert_eq!(
            research
                .2
                .spec
                .expect("research should resolve observe spec")
                .credential_class
                .to_string(),
            "None"
        );
    }

    #[test]
    fn collect_profile_entries_omits_disabled_advisor_and_research_pools() {
        let yaml = r#"
version: 1
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: ""
    permissions: []
  workers:
    - lane: codex-worker
      min: 1
      max: 3
  reviewers:
    - lane: claude-reviewer
      min: 1
      max: 2
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 1
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: []
  panel_mode: full_council
  lease_mode: exclusive
  panels: []
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: Wait
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  alert_threshold_percent: 80
  enforcement: Soft
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
permissions:
  categories: {}
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: []
lanes:
  advisor-claude:
    launcher: claude
  cheap-worker:
    launcher: codex
advisors:
  enabled: false
  pools:
    - lane: advisor-claude
      min: 1
      max: 2
research:
  enabled: false
  pools:
    - id: spec-research
      lane: cheap-worker
      role: normative_requirements
      min: 0
      max: 2
"#;
        let config = load_config(yaml);
        let entries = collect_profile_entries(&config);

        let lanes: Vec<_> = entries.iter().map(|(lane, _, _)| lane.as_str()).collect();
        assert!(lanes.contains(&"claude-supervisor"));
        assert!(lanes.contains(&"codex-worker"));
        assert!(lanes.contains(&"claude-reviewer"));
        assert!(
            !lanes.contains(&"advisor-claude"),
            "disabled advisor should be omitted"
        );
        assert!(
            !lanes.contains(&"cheap-worker"),
            "disabled research should be omitted"
        );
    }

    #[test]
    fn describe_profiles_dispatches_without_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, runtime_profile_config()).expect("write config");

        let output = capture_info_logs(|| {
            describe(None, Some(&path), "profiles").expect("describe profiles should render");
        });

        assert!(output.contains("Resolved permission profiles:"));
        assert!(output.contains("profile: observe (source=Built-in Role Default)"));
        assert!(output.contains("env_policy: Minimal"));
        assert!(
            !output.contains("(default)"),
            "describe profiles should delegate to concrete profile output: {output}"
        );
    }

    #[test]
    fn profiles_command_runs_without_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, runtime_profile_config()).expect("write config");

        let output = capture_info_logs(|| {
            profiles(None, Some(&path)).expect("profiles should render");
        });

        assert!(output.contains("advisor-claude (role=advisor):"));
        assert!(output.contains("cheap-worker (role=research):"));
        assert_eq!(
            output
                .matches("profile: observe (source=Built-in Role Default)")
                .count(),
            2
        );
        assert_eq!(output.matches("backend: OS Default").count(), 5);
        assert_eq!(output.matches("network_class: Model Only").count(), 3);
        assert_eq!(output.matches("network_class: Denied").count(), 2);
        assert_eq!(
            output.matches("credential_exposure: Env Allowlist").count(),
            3
        );
        assert_eq!(output.matches("credential_exposure: None").count(), 2);
        assert_eq!(output.matches("env_policy: Minimal").count(), 5);
        assert_eq!(output.matches("unsafe: false").count(), 5);
        assert_eq!(output.matches("read_roots: []").count(), 5);
        assert_eq!(output.matches("write_roots: []").count(), 5);
        assert_eq!(output.matches("denied_roots: []").count(), 5);
        assert!(
            !output.contains("(default)"),
            "all runtime roles should resolve concrete profile specs: {output}"
        );
    }

    #[test]
    fn profiles_command_reports_unsafe_without_spec_entry() {
        let yaml =
            runtime_profile_config().replacen("    worker: workspace", "    worker: unsafe", 1);
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, yaml).expect("write config");

        let output = capture_info_logs(|| {
            profiles(None, Some(&path)).expect("profiles should render");
        });

        assert!(output.contains("codex-worker (role=worker):"));
        assert!(output.contains("profile: unsafe (source=Config Role Default)"));
        assert!(output.contains("backend: (default)"));
        assert!(output.contains("network_class: (default)"));
        assert!(output.contains("credential_exposure: (default)"));
        assert!(output.contains("env_policy: (default)"));
        assert!(output.contains("read_roots: (default)"));
        assert!(output.contains("write_roots: (default)"));
        assert!(output.contains("denied_roots: (default)"));
        assert!(output.contains("unsafe: true"));
    }

    #[test]
    fn profiles_command_phase_gate_resolved_profiles_are_stable_and_visible() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, phase_gate_profile_config()).expect("write config");

        let output = capture_info_logs(|| {
            profiles(None, Some(&path)).expect("profiles should render");
        });

        assert!(output.contains("claude-supervisor (role=supervisor):"));
        assert!(output.contains("profile: operator (source=Config Role Default)"));
        assert!(output.contains("codex-worker (role=worker):"));
        assert!(output.contains("profile: workspace (source=Config Role Default)"));
        assert!(
            output.contains(
                "codex-unsafe (role=worker):\n INFO     profile: unsafe (source=Lane Override)"
            ),
            "codex-unsafe lane should resolve to unsafe profile via Lane Override"
        );
        assert!(output.contains("claude-reviewer (role=reviewer):"));
        assert!(output.contains("profile: reviewer (source=Config Role Default)"));
        assert!(output.contains("advisor-claude (role=advisor):"));
        assert!(output.contains("research-readonly (role=research):"));
        assert!(output.contains("research-dependency (role=research):"));
        assert_eq!(
            output
                .matches("profile: observe (source=Built-in Role Default)")
                .count(),
            2
        );
        assert_eq!(
            output
                .matches("profile: dependency (source=Lane Override)")
                .count(),
            1
        );
        assert_eq!(
            output
                .matches("profile: unsafe (source=Lane Override)")
                .count(),
            1
        );
        assert_eq!(output.matches("unsafe: true").count(), 1);
        assert_eq!(output.matches("unsafe: false").count(), 6);
        assert!(
            !output.contains("(default)"),
            "phase gate output should use concrete specs for every resolved profile: {output}"
        );
    }

    #[test]
    fn profiles_command_renders_non_empty_filesystem_roots() {
        let yaml = r#"
version: 1
launchers:
  claude:
    adapter: Acp
    command: claude
    args: []
lanes:
  claude-supervisor:
    launcher: claude
  codex-worker:
    launcher: claude
  claude-reviewer:
    launcher: claude
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: test
    permissions: []
  workers:
    - lane: codex-worker
      min: 1
      max: 1
  reviewers:
    - lane: claude-reviewer
      min: 1
      max: 1
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 1
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: [claude-reviewer]
  panel_mode: full_council
  panels:
    - id: primary
      reviewers: [claude-reviewer]
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: Wait
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  alert_threshold_percent: 80
  enforcement: Soft
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
permissions:
  categories: {}
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: []
profiles:
  defaults:
    worker: workspace
  specs:
    workspace:
      backend: os_default
      network_class: model_only
      credential_class: env_allowlist
      env_policy: minimal
      unsafe_marker: false
      read_roots:
        - path: src
          recursive: true
        - path: docs
          recursive: false
      write_roots:
        - path: target
          recursive: true
      denied_roots:
        - path: .env
          recursive: false
"#;
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, yaml).expect("write config");

        let output = capture_info_logs(|| {
            profiles(None, Some(&path)).expect("profiles should render");
        });

        assert!(
            output.contains("read_roots: src(r=true), docs(r=false)"),
            "expected non-empty read_roots with mixed recursive flags, got: {output}"
        );
        assert!(
            output.contains("write_roots: target(r=true)"),
            "expected non-empty write_roots, got: {output}"
        );
        assert!(
            output.contains("denied_roots: .env(r=false)"),
            "expected non-empty denied_roots with recursive=false, got: {output}"
        );
    }
}
