//! Configuration validation.
//!
//! Validates config for:
//! - Missing agent refs in roles
//! - Invalid review thresholds
//! - Circular worker/reviewer references
//! - Contradictory concurrency limits
//! - Unsupported terminal mode requests

mod runtime_policy;

use std::{collections::HashSet, sync::LazyLock};

use brehon_adapter_sdk::harness::{
    builtin_cli_from_launcher_shape, HarnessControlPlane, HarnessTransport, SupervisorCli,
};
use brehon_types::{
    BrehonConfig, PermissionProfile, ResearchPermissions, RoleKind, RuntimeTerminalHostKind,
    RuntimeTerminalHostPaneOwnership,
};

use runtime_policy::validate_runtime_policy;

const SUPPORTED_RUNTIME_WORKFLOWS: &[&str] = &["rate_limit.quarantine_recommendation"];
static VALID_PERMISSION_PROFILES: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| PermissionProfile::names().collect());
static VALID_ROLE_KINDS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| RoleKind::profile_defaults_keys().collect());

/// A warning produced during config validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationWarning {
    pub kind: ValidationWarningKind,
    pub message: String,
    pub is_fatal: bool,
}

impl ValidationWarning {
    pub fn new(kind: ValidationWarningKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            is_fatal: kind.is_fatal(),
        }
    }

    pub fn non_fatal(kind: ValidationWarningKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            is_fatal: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationWarningKind {
    MissingAgentRef,
    InvalidReviewThreshold,
    CircularWorkerReviewer,
    ContradictoryConcurrency,
    UnsupportedTerminalMode,
    AgentCommandNotFound,
    ReviewPolicyConflict,
    ReviewPanelConflict,
    ReviewPanelInvalid,
    MissingRequiredStructure,
    PromptPolicyConflict,
    InvalidRetentionConfig,
    InvalidContextConfig,
    RuntimeWorkflowConflict,
    RuntimeTerminalHostConflict,
    RuntimePolicyConflict,
    SupervisorTerminalContract,
    LauncherCapabilityConflict,
    RoutingPolicyConflict,
    AdvisorPolicyConflict,
    ResearchPolicyConflict,
    ProfilePolicyConflict,
    InvalidWorktreeRoot,
}

impl ValidationWarningKind {
    pub const fn is_fatal(self) -> bool {
        matches!(
            self,
            ValidationWarningKind::ReviewPanelInvalid
                | ValidationWarningKind::MissingRequiredStructure
                | ValidationWarningKind::RuntimeWorkflowConflict
                | ValidationWarningKind::RuntimeTerminalHostConflict
                | ValidationWarningKind::RuntimePolicyConflict
                | ValidationWarningKind::SupervisorTerminalContract
                | ValidationWarningKind::LauncherCapabilityConflict
                | ValidationWarningKind::InvalidContextConfig
                | ValidationWarningKind::ResearchPolicyConflict
                | ValidationWarningKind::ProfilePolicyConflict
                | ValidationWarningKind::InvalidWorktreeRoot
        )
    }
}

impl std::fmt::Display for ValidationWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::fmt::Display for ValidationWarningKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationWarningKind::MissingAgentRef => write!(f, "Missing agent ref"),
            ValidationWarningKind::InvalidReviewThreshold => write!(f, "Invalid review threshold"),
            ValidationWarningKind::CircularWorkerReviewer => {
                write!(f, "Circular worker/reviewer ref")
            }
            ValidationWarningKind::ContradictoryConcurrency => {
                write!(f, "Contradictory concurrency")
            }
            ValidationWarningKind::UnsupportedTerminalMode => {
                write!(f, "Unsupported terminal mode")
            }
            ValidationWarningKind::AgentCommandNotFound => write!(f, "Agent command not found"),
            ValidationWarningKind::ReviewPolicyConflict => write!(f, "Review policy conflict"),
            ValidationWarningKind::ReviewPanelConflict => write!(f, "Review panel conflict"),
            ValidationWarningKind::ReviewPanelInvalid => write!(f, "Review panel invalid"),
            ValidationWarningKind::MissingRequiredStructure => {
                write!(f, "Missing required structure")
            }
            ValidationWarningKind::PromptPolicyConflict => write!(f, "Prompt policy conflict"),
            ValidationWarningKind::InvalidRetentionConfig => {
                write!(f, "Invalid retention config")
            }
            ValidationWarningKind::InvalidContextConfig => {
                write!(f, "Invalid context config")
            }
            ValidationWarningKind::RuntimeWorkflowConflict => {
                write!(f, "Runtime workflow conflict")
            }
            ValidationWarningKind::RuntimeTerminalHostConflict => {
                write!(f, "Runtime terminal host conflict")
            }
            ValidationWarningKind::RuntimePolicyConflict => {
                write!(f, "Runtime policy conflict")
            }
            ValidationWarningKind::SupervisorTerminalContract => {
                write!(f, "Supervisor terminal contract")
            }
            ValidationWarningKind::LauncherCapabilityConflict => {
                write!(f, "Launcher capability conflict")
            }
            ValidationWarningKind::RoutingPolicyConflict => write!(f, "Routing policy conflict"),
            ValidationWarningKind::AdvisorPolicyConflict => write!(f, "Advisor policy conflict"),
            ValidationWarningKind::ResearchPolicyConflict => write!(f, "Research policy conflict"),
            ValidationWarningKind::ProfilePolicyConflict => write!(f, "Profile policy conflict"),
            ValidationWarningKind::InvalidWorktreeRoot => write!(f, "Invalid worktree root"),
        }
    }
}

/// Validate configuration and return all warnings.
pub fn validate(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();

    warnings.extend(validate_structure(config));
    warnings.extend(validate_agent_refs(config));
    warnings.extend(validate_routing_policy(config));
    warnings.extend(validate_advisors(config));
    warnings.extend(validate_research(config));
    warnings.extend(validate_review_thresholds(config));
    warnings.extend(validate_review_panels(config));
    warnings.extend(validate_prompt_policy(config));
    warnings.extend(validate_launcher_capability_overrides(config));
    warnings.extend(validate_runtime_workflows(config));
    warnings.extend(validate_runtime_policy(config));
    warnings.extend(validate_runtime_terminal_host(config));
    warnings.extend(validate_supervisor_terminal_contract(config));
    warnings.extend(validate_circular_references(config));
    warnings.extend(validate_concurrency(config));
    warnings.extend(validate_terminal_mode(config));
    warnings.extend(validate_retention(config));
    warnings.extend(validate_context(config));
    warnings.extend(validate_profiles(config));
    warnings.extend(validate_worktree_root(config));

    warnings
}

fn validate_routing_policy(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let worker_lanes: HashSet<_> = config
        .roles
        .workers
        .iter()
        .map(|worker| worker.lane.as_str())
        .collect();

    for (label, lane) in [
        (
            "routing.default_worker_lane",
            config.routing.default_worker_lane.as_deref(),
        ),
        (
            "routing.escalation_lane",
            config.routing.escalation_lane.as_deref(),
        ),
        (
            "routing.escalation.from_lane",
            config.routing.escalation.from_lane.as_deref(),
        ),
        (
            "routing.escalation.to_lane",
            config.routing.escalation.to_lane.as_deref(),
        ),
    ] {
        if let Some(lane) = lane.filter(|lane| !lane.trim().is_empty()) {
            if !worker_lanes.contains(lane) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::RoutingPolicyConflict,
                    format!(
                        "{label} references worker lane '{lane}', but no worker pool uses that lane"
                    ),
                ));
            }
        }
    }

    let mut seen_rule_ids = HashSet::new();
    for (idx, rule) in config.routing.rules.iter().enumerate() {
        if !rule.id.trim().is_empty() && !seen_rule_ids.insert(rule.id.as_str()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RoutingPolicyConflict,
                format!("routing.rules[{idx}] duplicates rule id '{}'", rule.id),
            ));
        }
        if rule.policy.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RoutingPolicyConflict,
                format!("routing.rules[{idx}] has no policy fields to apply"),
            ));
        }
        let criteria = &rule.criteria;
        if !criteria.default
            && criteria.task_type.is_none()
            && criteria.priority.is_none()
            && criteria.work_class.is_none()
            && criteria.work_classes.is_empty()
            && criteria.title_any.is_empty()
            && criteria.text_any.is_empty()
        {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RoutingPolicyConflict,
                format!("routing.rules[{idx}] has no match criteria; set match.default=true for a catch-all rule"),
            ));
        }
        if let Some(preferred_lane) = rule
            .policy
            .get("preferred_lane")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
        {
            if !worker_lanes.contains(preferred_lane) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::RoutingPolicyConflict,
                    format!(
                        "routing.rules[{idx}].policy.preferred_lane references worker lane '{preferred_lane}', but no worker pool uses that lane"
                    ),
                ));
            }
        }
    }

    warnings
}

fn validate_advisors(config: &BrehonConfig) -> Vec<ValidationWarning> {
    if config.advisors.is_default() {
        return Vec::new();
    }

    let mut warnings = Vec::new();
    let mut pool_lanes = HashSet::new();
    let mut room_ids = HashSet::new();

    if config.advisors.enabled && config.advisors.pools.is_empty() {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::AdvisorPolicyConflict,
            "advisors.enabled is true but no advisors.pools are configured",
        ));
    }

    if config.advisors.enabled && config.advisors.rooms.is_empty() {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::AdvisorPolicyConflict,
            "advisors.enabled is true but no advisors.rooms are configured",
        ));
    }

    for (idx, pool) in config.advisors.pools.iter().enumerate() {
        let lane = pool.lane.trim();
        if lane.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::AdvisorPolicyConflict,
                format!("advisors.pools[{idx}].lane must not be empty"),
            ));
            continue;
        }
        if !pool_lanes.insert(lane.to_string()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::AdvisorPolicyConflict,
                format!("advisors.pools[{idx}] duplicates lane '{lane}'"),
            ));
        }
        if pool.min > pool.max {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::AdvisorPolicyConflict,
                format!(
                    "advisors.pools[{idx}] has min={} greater than max={}",
                    pool.min, pool.max
                ),
            ));
        }
        if !config.lanes.contains_key(lane) && !config.launchers.contains_key(lane) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::AdvisorPolicyConflict,
                format!(
                    "advisors.pools[{idx}].lane references '{lane}', but no lane or launcher uses that name"
                ),
            ));
        }
    }

    for (idx, room) in config.advisors.rooms.iter().enumerate() {
        let id = room.id.trim();
        if id.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::AdvisorPolicyConflict,
                format!("advisors.rooms[{idx}].id must not be empty"),
            ));
        } else if !room_ids.insert(id.to_string()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::AdvisorPolicyConflict,
                format!("advisors.rooms[{idx}] duplicates id '{id}'"),
            ));
        }
        if room.participants.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::AdvisorPolicyConflict,
                format!("advisors.rooms[{idx}] has no participants"),
            ));
        }
        for participant in &room.participants {
            let participant = participant.trim();
            if participant.is_empty() {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::AdvisorPolicyConflict,
                    format!("advisors.rooms[{idx}] has an empty participant"),
                ));
            } else if !pool_lanes.contains(participant) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::AdvisorPolicyConflict,
                    format!(
                        "advisors.rooms[{idx}] references participant '{participant}', but no advisors.pools lane uses that name"
                    ),
                ));
            }
        }
        for doc in &room.context.docs {
            let doc = doc.trim();
            if doc.starts_with('/') || doc.contains("..") {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::AdvisorPolicyConflict,
                    format!(
                        "advisors.rooms[{idx}].context.docs should be repository-relative without '..': '{doc}'"
                    ),
                ));
            }
        }
    }

    for (idx, pool) in config.advisors.pools.iter().enumerate() {
        for room_id in &pool.rooms {
            let room_id = room_id.trim();
            if !room_id.is_empty() && !room_ids.contains(room_id) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::AdvisorPolicyConflict,
                    format!(
                        "advisors.pools[{idx}].rooms references '{room_id}', but no advisors.rooms id matches"
                    ),
                ));
            }
        }
    }

    warnings
}

fn validate_research(config: &BrehonConfig) -> Vec<ValidationWarning> {
    if config.research.is_default() {
        return Vec::new();
    }

    let mut warnings = Vec::new();
    let mut pool_ids = HashSet::new();
    let mut route_ids = HashSet::new();
    let mut roles_with_pool = HashSet::new();

    if config.research.enabled && config.research.pools.is_empty() {
        warnings.push(ValidationWarning::non_fatal(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.enabled is true but no research.pools are configured",
        ));
    }
    if config.research.enabled && config.research.routes.is_empty() {
        warnings.push(ValidationWarning::non_fatal(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.enabled is true but no research.routes are configured",
        ));
    }

    let artifact_root = config.research.artifact_root.trim();
    if artifact_root.is_empty() {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.artifact_root must not be empty",
        ));
    } else if !config.research.unsafe_allow_external_artifact_root {
        let normalized = artifact_root.replace('\\', "/");
        let escapes_runtime = normalized.starts_with('/')
            || normalized.contains("/../")
            || normalized.starts_with("../")
            || normalized.ends_with("/..")
            || !(normalized == ".brehon/runtime" || normalized.starts_with(".brehon/runtime/"));
        if escapes_runtime {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!(
                    "research.artifact_root '{artifact_root}' must stay under .brehon/runtime unless research.unsafe_allow_external_artifact_root=true"
                ),
            ));
        }
    }

    if config.research.defaults.max_parallel_jobs == 0 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.defaults.max_parallel_jobs must be greater than 0",
        ));
    }
    if config.research.defaults.job_timeout_secs == 0 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.defaults.job_timeout_secs must be greater than 0",
        ));
    }
    if config.research.defaults.max_artifact_bytes == 0 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.defaults.max_artifact_bytes must be greater than 0",
        ));
    }
    if config.research.worker_requests.max_requests_per_task == 0 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.worker_requests.max_requests_per_task must be greater than 0",
        ));
    }
    if config.research.worker_requests.max_cost_units_per_task == 0 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.worker_requests.max_cost_units_per_task must be greater than 0",
        ));
    }
    if config.research.worker_requests.max_cost_units_per_request == 0 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ResearchPolicyConflict,
            "research.worker_requests.max_cost_units_per_request must be greater than 0",
        ));
    }

    for (idx, pool) in config.research.pools.iter().enumerate() {
        let id = pool.id.trim();
        if id.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.pools[{idx}].id must not be empty"),
            ));
        } else if !pool_ids.insert(id.to_string()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.pools[{idx}] duplicates id '{id}'"),
            ));
        }
        let lane = pool.lane.trim();
        if lane.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.pools[{idx}].lane must not be empty"),
            ));
        } else if !config.has_lane(lane) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!(
                    "research.pools[{idx}].lane references '{lane}', but no lane or launcher uses that name"
                ),
            ));
        }
        let role = pool.role.trim();
        if role.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.pools[{idx}].role must not be empty"),
            ));
        } else {
            roles_with_pool.insert(role.to_string());
        }
        if pool.min > pool.max {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!(
                    "research.pools[{idx}] has min={} greater than max={}",
                    pool.min, pool.max
                ),
            ));
        }
        if pool.max == 0 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.pools[{idx}].max must be greater than 0"),
            ));
        }
        if pool.cost_units == 0 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.pools[{idx}].cost_units must be greater than 0"),
            ));
        }
        if !matches!(
            pool.permissions,
            ResearchPermissions::ReadOnly | ResearchPermissions::Sandboxed
        ) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.pools[{idx}].permissions is unsupported"),
            ));
        }
        if pool.instruction_profile.is_none() {
            warnings.push(ValidationWarning::non_fatal(
                ValidationWarningKind::ResearchPolicyConflict,
                format!(
                    "research.pools[{idx}] has no instruction_profile; ensure lane '{lane}' is research-specific"
                ),
            ));
        }
    }

    for role in &config.research.worker_requests.allowed_roles {
        let role = role.trim();
        if role.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                "research.worker_requests.allowed_roles contains an empty role",
            ));
        } else if !roles_with_pool.contains(role) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!(
                    "research.worker_requests.allowed_roles references '{role}', but no research pool provides that role"
                ),
            ));
        }
    }

    for (route_idx, route) in config.research.routes.iter().enumerate() {
        let id = route.id.trim();
        if id.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.routes[{route_idx}].id must not be empty"),
            ));
        } else if !route_ids.insert(id.to_string()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.routes[{route_idx}] duplicates id '{id}'"),
            ));
        }
        if route.required.is_some() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.routes[{route_idx}] sets required, but research routes cannot block task progress"),
            ));
        }
        if !route.criteria.extra.is_empty() {
            let keys = route
                .criteria
                .extra
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.routes[{route_idx}].match has unknown key(s): {keys}"),
            ));
        }
        if !route.criteria.has_any_matcher() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!(
                    "research.routes[{route_idx}] has no match criteria; set match.default=true for a catch-all route"
                ),
            ));
        }
        if route.jobs.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.routes[{route_idx}] has no jobs"),
            ));
        }
        if route.max_jobs_per_task == Some(0) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ResearchPolicyConflict,
                format!("research.routes[{route_idx}].max_jobs_per_task must be greater than 0"),
            ));
        }
        let mut job_ids = HashSet::new();
        for (job_idx, job) in route.jobs.iter().enumerate() {
            let job_id = job.id.trim();
            if job_id.is_empty() {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ResearchPolicyConflict,
                    format!("research.routes[{route_idx}].jobs[{job_idx}].id must not be empty"),
                ));
            } else if !job_ids.insert(job_id.to_string()) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ResearchPolicyConflict,
                    format!(
                        "research.routes[{route_idx}].jobs[{job_idx}] duplicates id '{job_id}'"
                    ),
                ));
            }
            if !pool_ids.contains(job.pool.trim()) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ResearchPolicyConflict,
                    format!(
                        "research.routes[{route_idx}].jobs[{job_idx}].pool references missing pool '{}'",
                        job.pool
                    ),
                ));
            }
            if job.prompt_template.trim().is_empty() {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ResearchPolicyConflict,
                    format!(
                        "research.routes[{route_idx}].jobs[{job_idx}].prompt_template must not be empty"
                    ),
                ));
            }
        }
        for (job_idx, job) in route.jobs.iter().enumerate() {
            for dependency in &job.depends_on {
                if !job_ids.contains(dependency.trim()) {
                    warnings.push(ValidationWarning::new(
                        ValidationWarningKind::ResearchPolicyConflict,
                        format!(
                            "research.routes[{route_idx}].jobs[{job_idx}].depends_on references missing job '{dependency}'"
                        ),
                    ));
                }
            }
        }
    }

    warnings
}

fn validate_context(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let retrieval = config.context.retrieval;

    if retrieval.default_limit == 0 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidContextConfig,
            "context.retrieval.default_limit must be greater than 0",
        ));
    }
    if retrieval.max_limit == 0 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidContextConfig,
            "context.retrieval.max_limit must be greater than 0",
        ));
    }
    if retrieval.default_limit > retrieval.max_limit {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidContextConfig,
            "context.retrieval.default_limit cannot exceed context.retrieval.max_limit",
        ));
    }
    if retrieval.snippet_chars < 32 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidContextConfig,
            "context.retrieval.snippet_chars must be at least 32",
        ));
    }

    let compression = config.context.compression;
    if compression.enabled && !compression.store_raw {
        warnings.push(ValidationWarning::non_fatal(
            ValidationWarningKind::InvalidContextConfig,
            "context.compression.store_raw=false discards raw memory/rule prose; raw retrieval can only return the stored compact form",
        ));
    }
    if compression.enabled
        && !compression.compact_memories
        && !compression.compact_rules
        && !compression.compact_tasks
    {
        warnings.push(ValidationWarning::non_fatal(
            ValidationWarningKind::InvalidContextConfig,
            "context.compression.enabled=true has no effect because all compact_* toggles are false",
        ));
    }

    warnings
}

fn validate_profiles(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();

    for (role_kind, _profile) in &config.profiles.defaults {
        if !VALID_ROLE_KINDS.contains(role_kind.as_str()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ProfilePolicyConflict,
                format!("profiles.defaults contains unknown role kind '{role_kind}'"),
            ));
        }
    }

    for (profile_name, spec) in &config.profiles.specs {
        if !VALID_PERMISSION_PROFILES.contains(profile_name.as_str()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ProfilePolicyConflict,
                format!("profiles.specs contains unknown profile name '{profile_name}'"),
            ));
        }
        if spec.unsafe_marker && profile_name != "unsafe" {
            warnings.push(ValidationWarning::non_fatal(
                ValidationWarningKind::ProfilePolicyConflict,
                format!(
                    "profiles.specs['{profile_name}'] has unsafe_marker=true but profile name is not 'unsafe'"
                ),
            ));
        }
        if !spec.unsafe_marker && profile_name == "unsafe" {
            warnings.push(ValidationWarning::non_fatal(
                ValidationWarningKind::ProfilePolicyConflict,
                "profiles.specs['unsafe'] should have unsafe_marker=true",
            ));
        }
    }

    // Cross-validate: every profile referenced in defaults must have a spec entry.
    for (role_kind, profile) in &config.profiles.defaults {
        let profile_name = profile.as_str();
        if !config.profiles.specs.contains_key(profile_name) {
            warnings.push(ValidationWarning::non_fatal(
                ValidationWarningKind::ProfilePolicyConflict,
                format!(
                    "profiles.defaults['{role_kind}'] references profile '{profile_name}', but no profiles.specs entry exists for it"
                ),
            ));
        }
    }

    // Cross-validate: launcher profile overrides must have a spec entry.
    for (launcher_name, launcher) in &config.launchers {
        if let Some(profile) = launcher.profile {
            let profile_name = profile.as_str();
            if !config.profiles.specs.contains_key(profile_name) {
                warnings.push(ValidationWarning::non_fatal(
                    ValidationWarningKind::ProfilePolicyConflict,
                    format!(
                        "launcher '{launcher_name}' references profile '{profile_name}', but no profiles.specs entry exists for it"
                    ),
                ));
            }
        }
    }

    // Cross-validate: lane profile overrides must have a spec entry.
    for (lane_name, lane) in &config.lanes {
        if let Some(profile) = lane.profile {
            let profile_name = profile.as_str();
            if !config.profiles.specs.contains_key(profile_name) {
                warnings.push(ValidationWarning::non_fatal(
                    ValidationWarningKind::ProfilePolicyConflict,
                    format!(
                        "lane '{lane_name}' references profile '{profile_name}', but no profiles.specs entry exists for it"
                    ),
                ));
            }
        }
    }

    warnings
}

fn validate_worktree_root(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let Some(root) = config.orchestration.worktree_root.as_deref() else {
        return warnings;
    };

    if root.trim().is_empty() {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidWorktreeRoot,
            "orchestration.worktree_root must not be empty",
        ));
        return warnings;
    }

    let normalized = root.replace('\\', "/");
    if normalized.contains("/../")
        || normalized.starts_with("../")
        || normalized.ends_with("/..")
        || normalized == ".."
    {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidWorktreeRoot,
            format!("orchestration.worktree_root '{root}' contains path traversal ('..')"),
        ));
    }

    if root.contains('\0') {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidWorktreeRoot,
            format!("orchestration.worktree_root '{root}' contains invalid null bytes"),
        ));
    }

    let path = std::path::Path::new(root);
    if !path.is_absolute() {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidWorktreeRoot,
            format!("orchestration.worktree_root '{root}' must be an absolute path"),
        ));
    }

    warnings
}

fn launcher_transport_override(
    launcher: &brehon_types::AgentConnectionConfig,
) -> Result<Option<HarnessTransport>, String> {
    launcher
        .transport_str()
        .map(str::parse::<HarnessTransport>)
        .transpose()
}

fn launcher_control_plane_override(
    launcher: &brehon_types::AgentConnectionConfig,
) -> Result<Option<HarnessControlPlane>, String> {
    launcher
        .control_plane_str()
        .map(str::parse::<HarnessControlPlane>)
        .transpose()
}

fn launcher_effective_capabilities(
    launcher: &brehon_types::AgentConnectionConfig,
) -> Option<(HarnessTransport, HarnessControlPlane)> {
    let builtin = builtin_cli_from_launcher(launcher);
    let mut transport = builtin
        .map(|cli| cli.capabilities().transport)
        .or_else(|| launcher_transport_override(launcher).ok().flatten())?;
    let mut control_plane = builtin
        .map(|cli| cli.capabilities().preferred_control_plane)
        .or_else(|| launcher_control_plane_override(launcher).ok().flatten())?;

    if let Ok(Some(cp_override)) = launcher_control_plane_override(launcher) {
        control_plane = cp_override;
        transport = cp_override.canonical_transport();
    } else if let Ok(Some(transport_override)) = launcher_transport_override(launcher) {
        if transport_override.supports_control_plane(control_plane) {
            transport = transport_override;
        }
    }

    Some((transport, control_plane))
}

fn validate_launcher_capability_overrides(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    for (name, launcher) in &config.launchers {
        if let Some(transport) = launcher.transport_str() {
            if transport.parse::<HarnessTransport>().is_err() {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::LauncherCapabilityConflict,
                    format!("launcher '{name}' has unsupported transport override '{transport}'"),
                ));
            }
        }
        if let Some(control_plane) = launcher.control_plane_str() {
            if control_plane.parse::<HarnessControlPlane>().is_err() {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::LauncherCapabilityConflict,
                    format!(
                        "launcher '{name}' has unsupported control_plane override '{control_plane}'"
                    ),
                ));
            }
        }

        let transport_override = launcher_transport_override(launcher).ok().flatten();
        let control_plane_override = launcher_control_plane_override(launcher).ok().flatten();
        if let (Some(transport), Some(control_plane)) = (transport_override, control_plane_override)
        {
            if !transport.supports_control_plane(control_plane) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::LauncherCapabilityConflict,
                    format!(
                        "launcher '{name}' has incompatible transport/control_plane overrides: transport='{}' cannot carry control_plane='{}'",
                        transport, control_plane
                    ),
                ));
            }
        }

        if control_plane_override.is_none() {
            if let Some(transport) = transport_override {
                if let Some((_, control_plane)) = launcher_effective_capabilities(launcher) {
                    if !transport.supports_control_plane(control_plane) {
                        warnings.push(ValidationWarning::new(
                            ValidationWarningKind::LauncherCapabilityConflict,
                            format!(
                                "launcher '{name}' transport override '{}' conflicts with effective control_plane '{}'; specify a compatible control_plane override too",
                                transport, control_plane
                            ),
                        ));
                    }
                }
            }
        }

        if let Some(cli) = builtin_cli_from_launcher(launcher) {
            if let Some((transport, control_plane)) = launcher_effective_capabilities(launcher) {
                if !cli.supports_transport_control_plane(transport, control_plane) {
                    warnings.push(ValidationWarning::new(
                        ValidationWarningKind::LauncherCapabilityConflict,
                        format!(
                            "launcher '{name}' requests built-in '{}' with unsupported transport/control_plane overrides: transport='{}' control_plane='{}'",
                            cli.as_str(),
                            transport,
                            control_plane
                        ),
                    ));
                }
            }
        }

        if launcher_uses_deepseek_anthropic_through_claude(launcher) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::LauncherCapabilityConflict,
                format!(
                    "launcher '{name}' routes Claude Code through DeepSeek's Anthropic-compatible endpoint; DeepSeek rejects Claude Code system-role messages in this mode. Use a NativeAgent/OpenAI-compatible DeepSeek launcher instead."
                ),
            ));
        }
    }
    warnings
}

fn launcher_uses_deepseek_anthropic_through_claude(
    launcher: &brehon_types::AgentConnectionConfig,
) -> bool {
    let command_is_claude = launcher
        .command
        .as_deref()
        .map(str::trim)
        .is_some_and(|command| command.eq_ignore_ascii_case("claude"));
    if !command_is_claude {
        return false;
    }

    launcher
        .env
        .get("ANTHROPIC_BASE_URL")
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| value.contains("deepseek") && value.contains("/anthropic"))
}

fn validate_runtime_workflows(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    for workflow_id in &config.runtime.enabled_workflows {
        let workflow_id = workflow_id.trim();
        if workflow_id.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimeWorkflowConflict,
                "runtime.enabled_workflows contains an empty workflow id",
            ));
            continue;
        }
        if !seen.insert(workflow_id.to_string()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimeWorkflowConflict,
                format!("runtime.enabled_workflows lists '{workflow_id}' more than once"),
            ));
        }
        if !SUPPORTED_RUNTIME_WORKFLOWS.contains(&workflow_id) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimeWorkflowConflict,
                format!("runtime.enabled_workflows enables unsupported workflow '{workflow_id}'"),
            ));
        }
    }
    warnings
}

fn validate_runtime_terminal_host(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let host = &config.runtime.terminal_host;
    match host.effective_kind() {
        RuntimeTerminalHostKind::Embedded | RuntimeTerminalHostKind::Headless => {}
        RuntimeTerminalHostKind::Web | RuntimeTerminalHostKind::NativeGui => {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimeTerminalHostConflict,
                format!(
                    "runtime.terminal_host.kind {:?} is not wired into brehon run; use embedded until host promotion is complete",
                    host.effective_kind()
                ),
            ));
        }
    }

    if host.effective_pane_ownership() == RuntimeTerminalHostPaneOwnership::Host
        && !matches!(host.effective_kind(), RuntimeTerminalHostKind::Headless)
    {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::RuntimeTerminalHostConflict,
            "runtime.terminal_host.pane_ownership=host requires runtime.terminal_host.kind=headless",
        ));
    }

    warnings
}

fn validate_supervisor_terminal_contract(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let supervisor_lane = config.roles.supervisor.name.as_str();
    let Some(launcher_name) = config.lane_launcher_name(supervisor_lane) else {
        return warnings;
    };
    let Some(launcher) = config.launchers.get(launcher_name) else {
        return warnings;
    };

    if supervisor_launcher_supports_pty(launcher) {
        return warnings;
    }

    warnings.push(ValidationWarning::new(
        ValidationWarningKind::SupervisorTerminalContract,
        format!(
            "Supervisor lane '{}' uses launcher '{}' ({:?}) but supervisors must be interactive PTY-backed; use a built-in supervisor lane or configure a command-backed PTY launcher such as adapter=PtyHooks. Gateway-only ACP/API launchers may still be used for workers and reviewers.",
            supervisor_lane, launcher_name, launcher.adapter
        ),
    ));

    warnings
}

fn supervisor_launcher_supports_pty(launcher: &brehon_types::AgentConnectionConfig) -> bool {
    use brehon_types::agent::AdapterKind;

    if launcher_invokes_builtin_supervisor(launcher) {
        return true;
    }

    if launcher_control_plane_override(launcher).ok().flatten()
        == Some(HarnessControlPlane::AcpSidecar)
    {
        return launcher_transport_override(launcher).ok().flatten()
            == Some(HarnessTransport::InteractivePty)
            && (launcher.adapter == AdapterKind::NativeAgent
                || launcher
                    .command_str()
                    .is_some_and(|command| !command.trim().is_empty()));
    }

    match launcher.adapter {
        AdapterKind::PtyHooks | AdapterKind::Mock | AdapterKind::Junie | AdapterKind::Agy => {
            launcher
                .command_str()
                .is_some_and(|command| !command.trim().is_empty())
        }
        AdapterKind::NativeAgent => true,
        AdapterKind::Acp => launcher_invokes_builtin_supervisor(launcher),
        AdapterKind::OpenAiCompatible
        | AdapterKind::Codex
        | AdapterKind::Kimi
        | AdapterKind::Copilot => false,
    }
}

fn launcher_invokes_builtin_supervisor(launcher: &brehon_types::AgentConnectionConfig) -> bool {
    builtin_cli_from_launcher(launcher).is_some()
}

fn validate_structure(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();

    if config.lanes.is_empty() && config.launchers.is_empty() {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::MissingRequiredStructure,
            "Config must define at least one lane".to_string(),
        ));
    }

    if config.roles.workers.is_empty() {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::MissingRequiredStructure,
            "Config must define at least one worker pool".to_string(),
        ));
    }

    if config.roles.reviewers.is_empty() {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::MissingRequiredStructure,
            "Config must define at least one reviewer pool".to_string(),
        ));
    }

    warnings
}

fn validate_prompt_policy(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let mut seen_enabled = HashSet::new();

    for fragment_id in &config.prompt_policy.enabled {
        if !seen_enabled.insert(fragment_id.as_str()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::PromptPolicyConflict,
                format!(
                    "Prompt policy enables fragment '{}' more than once",
                    fragment_id
                ),
            ));
        }
        if !config.prompt_fragments.contains_key(fragment_id) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::PromptPolicyConflict,
                format!("Prompt policy enables unknown fragment '{}'", fragment_id),
            ));
        }
    }

    for (fragment_id, fragment) in &config.prompt_fragments {
        if fragment.text.trim().is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::PromptPolicyConflict,
                format!("Prompt fragment '{}' has empty text", fragment_id),
            ));
        }
    }

    warnings
}

fn validate_review_panels(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let reviewer_agents: HashSet<_> = config
        .roles
        .reviewers
        .iter()
        .map(|reviewer| reviewer.lane.as_str())
        .collect();
    let worker_agents: HashSet<_> = config
        .roles
        .workers
        .iter()
        .map(|worker| worker.lane.as_str())
        .collect();
    let supervisor_agent = config.roles.supervisor.name.as_str();
    let mut seen_panel_ids = HashSet::new();
    let mut required_slots_per_agent = std::collections::HashMap::<String, u32>::new();

    for panel in &config.review.panels {
        let panel_id = panel.id.trim();
        if panel_id.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ReviewPanelInvalid,
                "Review panel id must not be empty",
            ));
            continue;
        }

        if !seen_panel_ids.insert(panel_id.to_string()) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ReviewPanelInvalid,
                format!("Duplicate review panel id '{}'", panel_id),
            ));
        }

        if panel.reviewers.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ReviewPanelInvalid,
                format!("Review panel '{}' has no reviewers", panel_id),
            ));
        }

        let mut seen_reviewers_in_panel = HashSet::new();
        for reviewer in &panel.reviewers {
            if !reviewer_agents.contains(reviewer.as_str()) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ReviewPanelInvalid,
                    format!(
                        "Review panel '{}' references reviewer lane '{}' which is not configured under roles.reviewers",
                        panel_id, reviewer
                    ),
                ));
            }
            if !seen_reviewers_in_panel.insert(reviewer.as_str()) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ReviewPanelConflict,
                    format!(
                        "Review panel '{}' lists reviewer '{}' more than once",
                        panel_id, reviewer
                    ),
                ));
            }
            if reviewer == supervisor_agent {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ReviewPanelConflict,
                    format!(
                        "Review panel '{}' includes supervisor '{}'. This blurs coordinator/reviewer separation by default.",
                        panel_id, reviewer
                    ),
                ));
            }
            if worker_agents.contains(reviewer.as_str()) {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ReviewPanelConflict,
                    format!(
                        "Review panel '{}' includes worker '{}'. This overlaps worker and reviewer roles by default.",
                        panel_id, reviewer
                    ),
                ));
            }
            *required_slots_per_agent
                .entry(reviewer.clone())
                .or_insert(0) += 1;
        }
    }

    for reviewer in &config.roles.reviewers {
        if let Some(required_slots) = required_slots_per_agent.get(&reviewer.lane) {
            if *required_slots > reviewer.min {
                warnings.push(ValidationWarning::new(
                    ValidationWarningKind::ReviewPanelConflict,
                    format!(
                        "Review panels require {} reviewer slot(s) for '{}', but roles.reviewers guarantees only min={}. Concurrent panel capacity is not guaranteed.",
                        required_slots, reviewer.lane, reviewer.min
                    ),
                ));
            }
        }
    }

    if config.review.lease_mode == brehon_types::config::ReviewLeaseMode::ShareAfterSubmit {
        let incompatible: Vec<String> = config
            .roles
            .reviewers
            .iter()
            .filter(|reviewer| !reviewer_lane_supports_shared_reset(config, &reviewer.lane))
            .map(|reviewer| reviewer.lane.clone())
            .collect();
        if !incompatible.is_empty() {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ReviewPanelConflict,
                format!(
                    "review.lease_mode=share_after_submit requires reset-capable reviewers. Incompatible reviewer lane(s): {}",
                    incompatible.join(", ")
                ),
            ));
        }
    }

    warnings
}

/// Try to resolve a launcher configuration to a built-in [`SupervisorCli`]
/// so that its canonical [`HarnessCapabilities`] can be used instead of
/// hardcoded `AdapterKind` branches.
fn builtin_cli_from_launcher(
    launcher: &brehon_types::AgentConnectionConfig,
) -> Option<SupervisorCli> {
    builtin_cli_from_launcher_shape(launcher.adapter, launcher.command_str(), &launcher.args)
}

fn launcher_requests_unsupported_builtin_one_shot(
    launcher: &brehon_types::AgentConnectionConfig,
    transport: Option<HarnessTransport>,
    control_plane: Option<HarnessControlPlane>,
) -> bool {
    let requests_one_shot = transport.is_some_and(HarnessTransport::is_one_shot)
        || control_plane.is_some_and(HarnessControlPlane::is_one_shot);
    requests_one_shot
        && builtin_cli_from_launcher(launcher).is_some_and(|cli| !cli.capabilities().one_shot)
}

fn reviewer_launcher_uses_junie_one_shot_contract(
    launcher: &brehon_types::AgentConnectionConfig,
) -> bool {
    use brehon_types::agent::AdapterKind;

    if launcher.adapter == AdapterKind::Junie {
        return true;
    }

    if launcher.adapter != AdapterKind::Acp {
        return false;
    }

    let command = launcher.command_str().unwrap_or_default();
    let command_basename = std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command);
    command_basename == "junie"
        && (launcher.args.is_empty() || launcher.args.iter().any(|arg| arg == "--task"))
}

fn reviewer_lane_supports_shared_reset(config: &BrehonConfig, lane: &str) -> bool {
    let Some(launcher) = config.lane_launcher(lane) else {
        return false;
    };

    // Junie reviewer sessions always use `--task` one-shot execution today,
    // even when the launcher shape otherwise looks like a reusable PTY lane.
    // Until Junie exposes a real reusable reviewer contract, shared-reset
    // reviewers must reject these lanes.
    if reviewer_launcher_uses_junie_one_shot_contract(launcher) {
        return false;
    }

    let transport_override = launcher_transport_override(launcher).ok().flatten();
    let control_plane_override = launcher_control_plane_override(launcher).ok().flatten();
    if let (Some(transport), Some(control_plane)) = (transport_override, control_plane_override) {
        if !transport.supports_control_plane(control_plane) {
            return false;
        }
    }

    if control_plane_override.is_none() {
        if let Some(transport) = transport_override {
            if let Some((_, control_plane)) = launcher_effective_capabilities(launcher) {
                if !transport.supports_control_plane(control_plane) {
                    return false;
                }
            }
        }
    }

    if let Some((transport, control_plane)) = launcher_effective_capabilities(launcher) {
        if launcher_requests_unsupported_builtin_one_shot(
            launcher,
            Some(transport),
            Some(control_plane),
        ) {
            return false;
        }
        return control_plane.needs_post_spawn_prompt()
            || (transport.is_pty()
                && launcher
                    .command_str()
                    .is_some_and(|command| !command.trim().is_empty()));
    }

    // Fall back to AdapterKind defaults for non-built-in adapters.
    match launcher.adapter {
        brehon_types::agent::AdapterKind::OpenAiCompatible => true,
        brehon_types::agent::AdapterKind::Mock => true,
        brehon_types::agent::AdapterKind::PtyHooks => true,
        brehon_types::agent::AdapterKind::NativeAgent => true,
        brehon_types::agent::AdapterKind::Acp => true,
        brehon_types::agent::AdapterKind::Agy => launcher
            .command_str()
            .is_some_and(|command| !command.trim().is_empty()),
        // Built-in adapters with dedicated AdapterKind variants (Codex, Kimi,
        // Junie, Copilot) are resolved via `builtin_cli_from_launcher` above
        // and never reach this fallback. New variants must be explicitly wired
        // into either `builtin_cli_from_launcher` or this match to opt in.
        _ => {
            tracing::debug!(
                adapter_kind = ?launcher.adapter,
                "unrecognized AdapterKind variant reached shared_reset fallback; defaulting to false"
            );
            false
        }
    }
}

fn validate_agent_refs(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let defined_launchers: HashSet<_> = config.launchers.keys().cloned().collect();
    let defined_lanes: HashSet<_> = config
        .lanes
        .keys()
        .cloned()
        .chain(config.launchers.keys().cloned())
        .collect();

    for (lane_name, lane) in &config.lanes {
        if !defined_launchers.contains(&lane.launcher) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::MissingAgentRef,
                format!(
                    "Lane '{}' references undefined launcher '{}'",
                    lane_name, lane.launcher
                ),
            ));
        }
    }

    let supervisor_agent = &config.roles.supervisor.name;
    if !defined_lanes.contains(supervisor_agent) {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::MissingAgentRef,
            format!(
                "Supervisor role references undefined lane '{}'",
                supervisor_agent
            ),
        ));
    }

    for (idx, worker) in config.roles.workers.iter().enumerate() {
        if !defined_lanes.contains(&worker.lane) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::MissingAgentRef,
                format!(
                    "Worker pool {} references undefined lane '{}'",
                    idx, worker.lane
                ),
            ));
        }
    }

    for (idx, reviewer) in config.roles.reviewers.iter().enumerate() {
        if !defined_lanes.contains(&reviewer.lane) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::MissingAgentRef,
                format!(
                    "Reviewer pool {} references undefined lane '{}'",
                    idx, reviewer.lane
                ),
            ));
        }
    }

    for reviewer in &config.review.default_reviewers {
        if !defined_lanes.contains(reviewer) {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::MissingAgentRef,
                format!("Default reviewer '{}' is not defined in lanes", reviewer),
            ));
        }
    }

    warnings
}

fn validate_review_thresholds(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let policy = &config.review.policy;

    if policy.blocking_score >= policy.min_individual_score {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidReviewThreshold,
            format!(
                "blocking_score ({}) should be less than min_individual_score ({})",
                policy.blocking_score, policy.min_individual_score
            ),
        ));
    }

    if policy.min_individual_score > policy.min_average_score {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidReviewThreshold,
            format!(
                "min_individual_score ({}) should not exceed min_average_score ({})",
                policy.min_individual_score, policy.min_average_score
            ),
        ));
    }

    if policy.min_approvals < 1 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidReviewThreshold,
            "min_approvals must be at least 1",
        ));
    }

    if policy.max_review_rounds < 1 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidReviewThreshold,
            "max_review_rounds must be at least 1",
        ));
    }

    warnings
}

fn validate_circular_references(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();

    let worker_agents: HashSet<_> = config
        .roles
        .workers
        .iter()
        .map(|w| w.lane.as_str())
        .collect();
    let reviewer_agents: HashSet<_> = config
        .roles
        .reviewers
        .iter()
        .map(|r| r.lane.as_str())
        .collect();

    let overlap: Vec<_> = worker_agents.intersection(&reviewer_agents).collect();

    for agent in overlap {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::CircularWorkerReviewer,
            format!(
                "Lane '{}' is defined in both worker and reviewer pools, which may cause conflicts",
                agent
            ),
        ));
    }

    warnings
}

fn validate_concurrency(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();

    let total_min_workers: u32 = config.roles.workers.iter().map(|w| w.min).sum();
    let total_max_workers: u32 = config.roles.workers.iter().map(|w| w.max).sum();
    let generic_max_workers: u32 = config
        .roles
        .workers
        .iter()
        .filter(|w| w.assignment_mode != brehon_types::config::WorkerAssignmentMode::Reserved)
        .map(|w| w.max)
        .sum();
    let total_min_reviewers: u32 = config.roles.reviewers.iter().map(|r| r.min).sum();

    if let Some(worker_count) = config.orchestration.spawn_workers {
        if worker_count == 0 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ContradictoryConcurrency,
                "orchestration.spawn_workers is 0; use null or a positive value",
            ));
        }
        if worker_count > generic_max_workers {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ContradictoryConcurrency,
                format!(
                    "orchestration.spawn_workers ({}) exceeds generic worker max ({})",
                    worker_count, generic_max_workers
                ),
            ));
        }
    }

    for (idx, worker) in config.roles.workers.iter().enumerate() {
        if worker.min > worker.max {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ContradictoryConcurrency,
                format!(
                    "Worker pool {} has min ({}) > max ({})",
                    idx, worker.min, worker.max
                ),
            ));
        }
        if worker.assignment_mode == brehon_types::config::WorkerAssignmentMode::Reserved
            && worker.accepts.is_empty()
        {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ContradictoryConcurrency,
                format!(
                    "Worker pool {} is reserved but has no accepted work classes",
                    idx
                ),
            ));
        }
        if worker.assignment_mode == brehon_types::config::WorkerAssignmentMode::Normal
            && !worker.accepts.is_empty()
        {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ContradictoryConcurrency,
                format!(
                    "Worker pool {} has accepts entries but assignment_mode is normal",
                    idx
                ),
            ));
        }
    }

    for (idx, reviewer) in config.roles.reviewers.iter().enumerate() {
        if reviewer.min > reviewer.max {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::ContradictoryConcurrency,
                format!(
                    "Reviewer pool {} has min ({}) > max ({})",
                    idx, reviewer.min, reviewer.max
                ),
            ));
        }
    }

    if total_min_workers > config.orchestration.max_active_workers {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ContradictoryConcurrency,
            format!(
                "Total min workers ({}) exceeds max_active_workers ({})",
                total_min_workers, config.orchestration.max_active_workers
            ),
        ));
    }

    if config.budget.max_total_cost.is_some() && total_max_workers > 10 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ContradictoryConcurrency,
            format!(
                "High max worker count ({}) with budget limit may cause resource contention",
                total_max_workers
            ),
        ));
    }

    let expected_concurrent = total_min_workers + total_min_reviewers;
    if expected_concurrent > 50 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::ContradictoryConcurrency,
            format!(
                "Expected concurrent agents ({}) may exceed system limits",
                expected_concurrent
            ),
        ));
    }

    warnings
}

fn validate_terminal_mode(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();

    use brehon_types::TerminalMode;

    if config.tui.terminal_mode == TerminalMode::Interactive {
        let supervisors_require_terminal = true;
        if supervisors_require_terminal {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::UnsupportedTerminalMode,
                "Interactive terminal mode requires ACP-capable agents. \
                          Fallback to transcript mode will be used for unsupported agents.",
            ));
        }
    }

    warnings
}

fn validate_retention(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();

    if let Some(max_events) = config.retention.max_events {
        if max_events < 1000 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::InvalidRetentionConfig,
                format!(
                    "max_events ({}) is very low; consider at least 1000 to avoid data loss",
                    max_events
                ),
            ));
        }
    }

    if let Some(ttl) = config.retention.idempotency_ttl_hours {
        if ttl < 1 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::InvalidRetentionConfig,
                "idempotency_ttl_hours must be at least 1".to_string(),
            ));
        }
    }

    if config.retention.max_completed_tasks != 0 && config.retention.max_completed_tasks < 100 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidRetentionConfig,
            format!(
                "max_completed_tasks ({}) is very low; consider at least 100",
                config.retention.max_completed_tasks
            ),
        ));
    }

    if config.retention.max_assignment_history != 0 && config.retention.max_assignment_history < 10
    {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidRetentionConfig,
            format!(
                "max_assignment_history ({}) is very low; consider at least 10",
                config.retention.max_assignment_history
            ),
        ));
    }

    if config.retention.max_tasks != 0 && config.retention.max_tasks < 100 {
        warnings.push(ValidationWarning::new(
            ValidationWarningKind::InvalidRetentionConfig,
            format!(
                "max_tasks ({}) is very low; consider at least 100",
                config.retention.max_tasks
            ),
        ));
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{
        AdapterKind, AgentConnectionConfig, AgentsMdMode, AutonomyLevel, BudgetConfig,
        BudgetEnforcement, ChunkStrategy, ContextConfig, CredentialClass, EnvPolicy,
        EscalationConfig, LaneConfig, LayoutPreset, ModelConfig, NetworkClass, NotificationConfig,
        NotifyMethod, NudgeConfig, OrchestrationConfig, PermissionProfile, PermissionsConfig,
        ProfilesConfig, ResearchConfig, ResearchJobTemplateConfig, ResearchPoolConfig,
        ResearchRouteConfig, ResearchRouteMatchConfig, RetentionConfig, ReviewConfig,
        ReviewerPoolConfig, RoleDefinition, RoleKind, RolesConfig, RoutingConfig, RuntimeConfig,
        RuntimeTerminalHostKind, SandboxBackend, SandboxProfile, SandboxSpec, SecurityConfig,
        StaleDetectionConfig, StaleStrategy, StuckDetectionConfig, SupervisorConfig, TerminalMode,
        TuiConfig, WorkerIdleBehavior, WorkerPoolConfig,
    };
    use std::collections::HashMap;

    fn launcher_with_details(
        adapter: AdapterKind,
        command: Option<&str>,
        args: &[&str],
        transport: Option<&str>,
        control_plane: Option<&str>,
    ) -> AgentConnectionConfig {
        AgentConnectionConfig {
            adapter,
            command: command.map(|s| s.into()),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            provider: None,
            transport: transport.map(|value| value.to_string()),
            control_plane: control_plane.map(|value| value.to_string()),
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
        }
    }

    fn launcher(adapter: AdapterKind, command: Option<&str>) -> AgentConnectionConfig {
        launcher_with_details(adapter, command, &[], None, None)
    }

    fn minimal_valid_config() -> BrehonConfig {
        let mut launchers = HashMap::new();
        launchers.insert(
            "claude-code".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::Acp,
                command: Some("claude".into()),
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
        launchers.insert(
            "codex".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::Acp,
                command: Some("codex".into()),
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

        BrehonConfig {
            version: 1,
            launchers,
            lanes: HashMap::from([
                (
                    "claude-code".to_string(),
                    LaneConfig {
                        launcher: "claude-code".to_string(),
                        model: None,
                        reasoning_effort: None,
                        system_prompt: None,
                        profile: None,
                    },
                ),
                (
                    "codex".to_string(),
                    LaneConfig {
                        launcher: "codex".to_string(),
                        model: None,
                        reasoning_effort: None,
                        system_prompt: None,
                        profile: None,
                    },
                ),
            ]),
            prompt_fragments: HashMap::new(),
            prompt_policy: brehon_types::PromptPolicyConfig::default(),
            roles: RolesConfig {
                supervisor: RoleDefinition {
                    name: "claude-code".into(),
                    kind: RoleKind::Supervisor,
                    description: "Supervisor".into(),
                    permissions: vec![],
                    system_prompt: None,
                },
                workers: vec![WorkerPoolConfig {
                    lane: "claude-code".into(),
                    model: Some(ModelConfig {
                        provider: "anthropic".into(),
                        name: "claude-sonnet-4-6".into(),
                    }),
                    reasoning_effort: None,
                    assignment_mode: brehon_types::config::WorkerAssignmentMode::Normal,
                    accepts: Vec::new(),
                    min: 1,
                    max: 3,
                }],
                reviewers: vec![ReviewerPoolConfig {
                    lane: "codex".into(),
                    model: Some(ModelConfig {
                        provider: "openai".into(),
                        name: "gpt-5.3-codex".into(),
                    }),
                    reasoning_effort: None,
                    system_prompt: None,
                    min: 1,
                    max: 2,
                }],
            },
            routing: RoutingConfig::default(),
            advisors: brehon_types::AdvisorConfig::default(),
            research: ResearchConfig::default(),
            review: ReviewConfig {
                policy: brehon_types::ReviewPolicy::default(),
                timeout_minutes: 30,
                auto_assign: true,
                default_reviewers: vec!["codex".into()],
                panel_mode: brehon_types::ReviewPanelMode::FullCouncil,
                lease_mode: brehon_types::config::ReviewLeaseMode::Exclusive,
                panels: vec![brehon_types::ReviewPanelConfig {
                    id: "primary".into(),
                    reviewers: vec!["codex".into()],
                }],
                max_diff_tokens: 8000,
                chunk_strategy: ChunkStrategy::ByDirectory,
                stale_detection: StaleDetectionConfig {
                    enabled: true,
                    ignore_files: vec![],
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
                self_improve_tasks: vec![],
                spawn_workers: None,
                drain_timeout_secs: None,
                worktree_root: None,
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
                max_memories: 10000,
                agents_md: AgentsMdMode::Auto,
                retrieval: brehon_types::config::ContextRetrievalConfig::default(),
                compression: brehon_types::config::ContextCompressionConfig::default(),
            },
            permissions: PermissionsConfig {
                categories: HashMap::new(),
            },
            profiles: ProfilesConfig::default(),
            retention: RetentionConfig::default(),
            security: SecurityConfig {
                sandbox_profile: SandboxProfile::OsDefault,
                persist_transcripts: true,
                redact_patterns: vec![],
                env_allowlist: vec![],
            },
        }
    }

    #[test]
    fn valid_config_has_no_warnings() {
        let config = minimal_valid_config();
        let warnings = validate(&config);
        assert!(
            warnings.is_empty(),
            "Expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn routing_policy_validates_worker_lane_references() {
        let mut config = minimal_valid_config();
        config.routing = serde_yaml::from_str(
            r#"
default_worker_lane: missing-worker
rules:
  - id: release-risk
    match:
      text_any:
        - release
    policy:
      preferred_lane: also-missing
"#,
        )
        .expect("routing config");

        let warnings = validate(&config);
        let routing_warnings: Vec<_> = warnings
            .iter()
            .filter(|warning| warning.kind == ValidationWarningKind::RoutingPolicyConflict)
            .collect();
        assert_eq!(routing_warnings.len(), 2, "{warnings:?}");
        assert!(
            routing_warnings.iter().all(|warning| !warning.is_fatal),
            "{routing_warnings:?}"
        );
    }

    #[test]
    fn research_policy_accepts_valid_opt_in_config() {
        let mut config = minimal_valid_config();
        config.research.enabled = true;
        config.research.pools = vec![ResearchPoolConfig {
            id: "specs".into(),
            lane: "claude-code".into(),
            instruction_profile: Some("cite primary sources".into()),
            role: "normative_requirements".into(),
            min: 0,
            max: 2,
            cost_units: 1,
            permissions: ResearchPermissions::ReadOnly,
            output_schema: brehon_types::ResearchOutputSchema::SpecBrief,
        }];
        config.research.routes = vec![ResearchRouteConfig {
            id: "protocol-specs".into(),
            criteria: ResearchRouteMatchConfig {
                text_any: vec!["PFCP".into()],
                ..ResearchRouteMatchConfig::default()
            },
            jobs: vec![ResearchJobTemplateConfig {
                pool: "specs".into(),
                id: "normative".into(),
                depends_on: Vec::new(),
                prompt_template: "Summarize {{task_id}}".into(),
            }],
            ..ResearchRouteConfig::default()
        }];

        let warnings = validate(&config);
        assert!(
            !warnings
                .iter()
                .any(|warning| warning.kind == ValidationWarningKind::ResearchPolicyConflict),
            "unexpected research warnings: {warnings:?}"
        );
    }

    #[test]
    fn research_policy_rejects_blocking_routes_and_unknown_match_keys() {
        let mut config = minimal_valid_config();
        config.research.enabled = true;
        config.research.pools = vec![ResearchPoolConfig {
            id: "specs".into(),
            lane: "claude-code".into(),
            instruction_profile: Some("cite primary sources".into()),
            role: "normative_requirements".into(),
            min: 0,
            max: 1,
            cost_units: 1,
            permissions: ResearchPermissions::ReadOnly,
            output_schema: brehon_types::ResearchOutputSchema::SpecBrief,
        }];
        let mut criteria = ResearchRouteMatchConfig {
            text_any: vec!["PFCP".into()],
            ..ResearchRouteMatchConfig::default()
        };
        criteria
            .extra
            .insert("domain_magic".into(), serde_json::json!(true));
        config.research.routes = vec![ResearchRouteConfig {
            id: "bad".into(),
            required: Some(true),
            criteria,
            jobs: vec![ResearchJobTemplateConfig {
                pool: "missing".into(),
                id: "normative".into(),
                depends_on: Vec::new(),
                prompt_template: "Summarize".into(),
            }],
            ..ResearchRouteConfig::default()
        }];

        let warnings = validate(&config);
        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ResearchPolicyConflict
                && warning.message.contains("cannot block task progress")
                && warning.is_fatal
        }));
        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ResearchPolicyConflict
                && warning.message.contains("unknown key")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ResearchPolicyConflict
                && warning.message.contains("missing pool")
        }));
    }

    #[test]
    fn advisor_policy_validates_rooms_and_pools() {
        let mut config = minimal_valid_config();
        config.advisors = serde_yaml::from_str(
            r#"
enabled: true
pools:
  - lane: missing-advisor
    min: 2
    max: 1
rooms:
  - id: release-war-room
    participants:
      - missing-advisor
      - not-a-pool
    context:
      docs:
        - ../outside.md
"#,
        )
        .expect("advisor config");

        let warnings = validate(&config);
        let advisor_warnings: Vec<_> = warnings
            .iter()
            .filter(|warning| warning.kind == ValidationWarningKind::AdvisorPolicyConflict)
            .collect();
        assert_eq!(advisor_warnings.len(), 4, "{advisor_warnings:?}");
        assert!(
            advisor_warnings.iter().all(|warning| !warning.is_fatal),
            "{advisor_warnings:?}"
        );
    }

    #[test]
    fn context_retrieval_limits_are_validated() {
        let mut config = minimal_valid_config();
        config.context.retrieval.default_limit = 25;
        config.context.retrieval.max_limit = 10;

        let warnings = validate(&config);
        assert!(
            warnings
                .iter()
                .any(|w| { w.kind == ValidationWarningKind::InvalidContextConfig && w.is_fatal }),
            "Expected fatal invalid-context warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn context_compression_store_raw_false_is_warned_when_enabled() {
        let mut config = minimal_valid_config();
        config.context.compression.enabled = true;
        config.context.compression.store_raw = false;

        let warnings = validate(&config);
        assert!(
            warnings
                .iter()
                .any(|w| w.kind == ValidationWarningKind::InvalidContextConfig && !w.is_fatal),
            "Expected non-fatal invalid-context warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn context_compression_enabled_with_no_targets_is_warned() {
        let mut config = minimal_valid_config();
        config.context.compression.enabled = true;
        config.context.compression.compact_memories = false;
        config.context.compression.compact_rules = false;
        config.context.compression.compact_tasks = false;

        let warnings = validate(&config);
        assert!(
            warnings.iter().any(|w| {
                w.kind == ValidationWarningKind::InvalidContextConfig
                    && !w.is_fatal
                    && w.message.contains("has no effect")
            }),
            "Expected no-effect compression warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn empty_lanes_is_fatal() {
        let mut config = minimal_valid_config();
        config.lanes.clear();
        config.launchers.clear();

        let warnings = validate(&config);
        assert!(
            warnings.iter().any(|w| {
                w.kind == ValidationWarningKind::MissingRequiredStructure && w.is_fatal
            }),
            "Expected fatal missing-structure warning for empty lanes, got: {:?}",
            warnings
        );
    }

    #[test]
    fn empty_workers_is_fatal() {
        let mut config = minimal_valid_config();
        config.roles.workers.clear();

        let warnings = validate(&config);
        assert!(
            warnings.iter().any(|w| {
                w.kind == ValidationWarningKind::MissingRequiredStructure && w.is_fatal
            }),
            "Expected fatal missing-structure warning for empty worker pools, got: {:?}",
            warnings
        );
    }

    #[test]
    fn empty_reviewers_is_fatal() {
        let mut config = minimal_valid_config();
        config.roles.reviewers.clear();

        let warnings = validate(&config);
        assert!(
            warnings.iter().any(|w| {
                w.kind == ValidationWarningKind::MissingRequiredStructure && w.is_fatal
            }),
            "Expected fatal missing-structure warning for empty reviewer pools, got: {:?}",
            warnings
        );
    }

    #[test]
    fn missing_agent_in_supervisor() {
        let mut config = minimal_valid_config();
        config.roles.supervisor.name = "undefined-agent".into();

        let warnings = validate(&config);
        assert!(!warnings.is_empty());
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::MissingAgentRef));
    }

    #[test]
    fn supervisor_terminal_contract_rejects_gateway_only_acp_launcher() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "goose".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::Acp,
                command: Some("goose".into()),
                args: vec!["acp".into()],
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
        config.lanes.insert(
            "goose-supervisor".into(),
            LaneConfig {
                launcher: "goose".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.supervisor.name = "goose-supervisor".into();

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::SupervisorTerminalContract
                && warning.is_fatal
                && warning.message.contains("Gateway-only ACP/API launchers")
        }));
    }

    #[test]
    fn supervisor_terminal_contract_rejects_openai_launcher() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "openai".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::OpenAiCompatible,
                command: None,
                args: Vec::new(),
                provider: None,
                transport: None,
                control_plane: None,
                base_url: Some("https://api.openai.example/v1".into()),
                api_key_env: Some("OPENAI_API_KEY".into()),
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
        config.lanes.insert(
            "api-supervisor".into(),
            LaneConfig {
                launcher: "openai".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.supervisor.name = "api-supervisor".into();

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::SupervisorTerminalContract
                && warning.is_fatal
                && warning.message.contains("interactive PTY-backed")
        }));
    }

    #[test]
    fn supervisor_terminal_contract_accepts_custom_pty_launcher() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "goose-pty".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::PtyHooks,
                command: Some("goose".into()),
                args: vec!["--interactive".into()],
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
        config.lanes.insert(
            "goose-supervisor".into(),
            LaneConfig {
                launcher: "goose-pty".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.supervisor.name = "goose-supervisor".into();

        let warnings = validate(&config);

        assert!(!warnings
            .iter()
            .any(|warning| warning.kind == ValidationWarningKind::SupervisorTerminalContract));
    }

    #[test]
    fn supervisor_terminal_contract_accepts_acp_sidecar_launcher() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "native-supervisor".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::Acp,
                command: Some("brehon-native-agent".into()),
                args: vec!["--supervised".into()],
                provider: None,
                transport: Some("interactive_pty".into()),
                control_plane: Some("acp_sidecar".into()),
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
        config.lanes.insert(
            "native-supervisor".into(),
            LaneConfig {
                launcher: "native-supervisor".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.supervisor.name = "native-supervisor".into();

        let warnings = validate(&config);

        assert!(!warnings
            .iter()
            .any(|warning| warning.kind == ValidationWarningKind::SupervisorTerminalContract));
    }

    #[test]
    fn launcher_capability_validation_rejects_unknown_control_plane() {
        let mut config = minimal_valid_config();
        config.launchers.get_mut("codex").unwrap().control_plane = Some("wat".into());

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::LauncherCapabilityConflict && warning.is_fatal
        }));
    }

    #[test]
    fn launcher_capability_validation_rejects_incompatible_override_pair() {
        let mut config = minimal_valid_config();
        let launcher = config.launchers.get_mut("codex").unwrap();
        launcher.transport = Some("app_server".into());
        launcher.control_plane = Some("pty_injection".into());

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::LauncherCapabilityConflict
                && warning
                    .message
                    .contains("incompatible transport/control_plane overrides")
        }));
    }

    #[test]
    fn launcher_capability_validation_rejects_transport_only_conflict_with_builtin_shape() {
        let mut config = minimal_valid_config();
        let launcher = config.launchers.get_mut("claude-code").unwrap();
        launcher.transport = Some("interactive_pty".into());
        launcher.control_plane = None;

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::LauncherCapabilityConflict
                && warning
                    .message
                    .contains("specify a compatible control_plane override too")
        }));
    }

    #[test]
    fn launcher_capability_validation_rejects_unsupported_builtin_gateway_override() {
        let mut config = minimal_valid_config();
        let launcher = config.launchers.get_mut("claude-code").unwrap();
        launcher.transport = Some("app_server".into());
        launcher.control_plane = Some("acp".into());

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::LauncherCapabilityConflict
                && warning.message.contains(
                    "requests built-in 'claude' with unsupported transport/control_plane overrides",
                )
        }));
    }

    #[test]
    fn launcher_capability_validation_rejects_unsupported_builtin_managed_api_override() {
        let mut config = minimal_valid_config();
        let launcher = config.launchers.get_mut("codex").unwrap();
        launcher.args = vec!["app-server".into()];
        launcher.transport = Some("managed_api".into());
        launcher.control_plane = Some("openai_compatible".into());

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::LauncherCapabilityConflict
                && warning.message.contains(
                    "requests built-in 'codex' with unsupported transport/control_plane overrides",
                )
        }));
    }

    #[test]
    fn launcher_capability_validation_rejects_deepseek_anthropic_via_claude() {
        let mut config = minimal_valid_config();
        let launcher = config.launchers.get_mut("claude-code").unwrap();
        launcher.env.insert(
            "ANTHROPIC_BASE_URL".into(),
            "https://api.deepseek.com/anthropic".into(),
        );

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::LauncherCapabilityConflict
                && warning.is_fatal
                && warning
                    .message
                    .contains("DeepSeek's Anthropic-compatible endpoint")
        }));
    }

    #[test]
    fn supervisor_terminal_contract_accepts_acp_junie_launcher() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "junie".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::Acp,
                command: Some("junie".into()),
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
        );
        config.lanes.insert(
            "junie-supervisor".into(),
            LaneConfig {
                launcher: "junie".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.supervisor.name = "junie-supervisor".into();

        let warnings = validate(&config);

        assert!(!warnings
            .iter()
            .any(|warning| warning.kind == ValidationWarningKind::SupervisorTerminalContract));
    }

    #[test]
    fn supervisor_terminal_contract_accepts_builtin_launcher_with_custom_lane_name() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "alias-claude".into(),
            AgentConnectionConfig {
                adapter: AdapterKind::Acp,
                command: Some("claude".into()),
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
        );
        config.lanes.insert(
            "safety-supervisor".into(),
            LaneConfig {
                launcher: "alias-claude".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.supervisor.name = "safety-supervisor".into();

        let warnings = validate(&config);

        assert!(!warnings
            .iter()
            .any(|warning| warning.kind == ValidationWarningKind::SupervisorTerminalContract));
    }

    #[test]
    fn missing_agent_in_worker_pool() {
        let mut config = minimal_valid_config();
        config.roles.workers.push(WorkerPoolConfig {
            lane: "missing-agent".into(),
            model: Some(ModelConfig {
                provider: "test".into(),
                name: "test".into(),
            }),
            reasoning_effort: None,
            assignment_mode: brehon_types::config::WorkerAssignmentMode::Normal,
            accepts: Vec::new(),
            min: 1,
            max: 2,
        });

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::MissingAgentRef));
    }

    #[test]
    fn review_panel_warns_on_duplicate_member_and_role_overlap() {
        let mut config = minimal_valid_config();
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["codex".into(), "codex".into(), "claude-code".into()],
        }];

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning
                    .message
                    .contains("lists reviewer 'codex' more than once")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning
                    .message
                    .contains("includes supervisor 'claude-code'")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("includes worker 'claude-code'")
        }));
    }

    #[test]
    fn missing_agent_in_reviewer_pool() {
        let mut config = minimal_valid_config();
        config.roles.reviewers.push(ReviewerPoolConfig {
            lane: "missing-reviewer".into(),
            model: Some(ModelConfig {
                provider: "test".into(),
                name: "test".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 2,
        });

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::MissingAgentRef));
    }

    #[test]
    fn share_after_submit_allows_claude_reviewers() {
        let mut config = minimal_valid_config();
        config.roles.reviewers.push(ReviewerPoolConfig {
            lane: "claude-code".into(),
            model: Some(ModelConfig {
                provider: "anthropic".into(),
                name: "claude-opus-4-6".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        });
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["claude-code".into(), "codex".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["claude-code".into(), "codex".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn share_after_submit_allows_agy_reviewers() {
        let mut config = minimal_valid_config();
        config
            .launchers
            .insert("agy".into(), launcher(AdapterKind::Agy, Some("agy")));
        config.lanes.insert(
            "agy-reviewer".into(),
            LaneConfig {
                launcher: "agy".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "agy-reviewer".into(),
            model: Some(ModelConfig {
                provider: "google".into(),
                name: "antigravity-2.0".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["agy-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["agy-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn share_after_submit_allows_acp_agy_reviewers_with_one_shot_override() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "agy".into(),
            launcher_with_details(
                AdapterKind::Acp,
                Some("agy"),
                &["--prompt-interactive"],
                None,
                Some("one_shot"),
            ),
        );
        config.lanes.insert(
            "agy-reviewer".into(),
            LaneConfig {
                launcher: "agy".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "agy-reviewer".into(),
            model: Some(ModelConfig {
                provider: "google".into(),
                name: "antigravity-2.0".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["agy-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["agy-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn prompt_policy_warns_on_unknown_enabled_fragment() {
        let mut config = minimal_valid_config();
        config.prompt_policy.enabled = vec!["architecture.hexagonal".into()];

        let warnings = validate(&config);
        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::PromptPolicyConflict
                && warning
                    .message
                    .contains("enables unknown fragment 'architecture.hexagonal'")
        }));
    }

    #[test]
    fn runtime_workflow_validation_accepts_supported_workflow() {
        let mut config = minimal_valid_config();
        config.runtime.enabled_workflows = vec!["rate_limit.quarantine_recommendation".to_string()];

        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|warning| warning.kind == ValidationWarningKind::RuntimeWorkflowConflict));
    }

    #[test]
    fn runtime_workflow_validation_rejects_unknown_workflow() {
        let mut config = minimal_valid_config();
        config.runtime.enabled_workflows = vec!["unknown.workflow".to_string()];

        let warnings = validate(&config);
        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::RuntimeWorkflowConflict
                && warning.is_fatal
                && warning
                    .message
                    .contains("unsupported workflow 'unknown.workflow'")
        }));
    }

    #[test]
    fn runtime_terminal_host_validation_accepts_default_embedded() {
        let config = minimal_valid_config();

        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|warning| warning.kind == ValidationWarningKind::RuntimeTerminalHostConflict));
    }

    #[test]
    fn runtime_terminal_host_validation_accepts_headless_host_selection() {
        let mut config = minimal_valid_config();
        config.runtime.terminal_host.kind = Some(RuntimeTerminalHostKind::Headless);

        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|warning| warning.kind == ValidationWarningKind::RuntimeTerminalHostConflict));
    }

    #[test]
    fn runtime_terminal_host_validation_rejects_unwired_host_selection() {
        let mut config = minimal_valid_config();
        config.runtime.terminal_host.kind = Some(RuntimeTerminalHostKind::Web);

        let warnings = validate(&config);
        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::RuntimeTerminalHostConflict
                && warning.is_fatal
                && warning.message.contains("not wired into brehon run")
        }));
    }

    #[test]
    fn runtime_terminal_host_validation_rejects_host_ownership_without_host_adapter() {
        let mut config = minimal_valid_config();
        config.runtime.terminal_host.kind = Some(RuntimeTerminalHostKind::Embedded);
        config.runtime.terminal_host.pane_ownership = Some(RuntimeTerminalHostPaneOwnership::Host);

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::RuntimeTerminalHostConflict
                && warning.is_fatal
                && warning.message.contains("pane_ownership=host requires")
        }));
    }

    #[test]
    fn runtime_terminal_host_validation_accepts_promoted_host_pane_ownership() {
        let mut config = minimal_valid_config();
        config.runtime.terminal_host.kind = Some(RuntimeTerminalHostKind::Headless);
        config.runtime.terminal_host.pane_ownership = Some(RuntimeTerminalHostPaneOwnership::Host);

        let warnings = validate(&config);

        assert!(!warnings
            .iter()
            .any(|warning| warning.kind == ValidationWarningKind::RuntimeTerminalHostConflict));
    }

    #[test]
    fn invalid_threshold_blocking_score() {
        let mut config = minimal_valid_config();
        config.review.policy.blocking_score = 7;
        config.review.policy.min_individual_score = 5;

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::InvalidReviewThreshold));
    }

    #[test]
    fn invalid_threshold_min_exceeds_avg() {
        let mut config = minimal_valid_config();
        config.review.policy.min_individual_score = 8;
        config.review.policy.min_average_score = 6;

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::InvalidReviewThreshold));
    }

    #[test]
    fn circular_worker_reviewer_reference() {
        let mut config = minimal_valid_config();
        config.roles.reviewers.push(ReviewerPoolConfig {
            lane: "claude-code".into(),
            model: Some(ModelConfig {
                provider: "anthropic".into(),
                name: "claude-sonnet-4-6".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 2,
        });

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::CircularWorkerReviewer));
    }

    #[test]
    fn min_exceeds_max_in_worker_pool() {
        let mut config = minimal_valid_config();
        config.roles.workers[0].min = 5;
        config.roles.workers[0].max = 2;

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::ContradictoryConcurrency));
    }

    #[test]
    fn total_min_workers_exceeds_parallelism() {
        let mut config = minimal_valid_config();
        config.roles.workers[0].min = 10;
        config.orchestration.max_active_workers = 3;

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::ContradictoryConcurrency));
    }

    #[test]
    fn interactive_terminal_mode_warning() {
        let mut config = minimal_valid_config();
        config.tui.terminal_mode = TerminalMode::Interactive;

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::UnsupportedTerminalMode));
    }

    #[test]
    fn missing_default_reviewer() {
        let mut config = minimal_valid_config();
        config
            .review
            .default_reviewers
            .push("undefined-agent".into());

        let warnings = validate(&config);
        assert!(warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::MissingAgentRef));
    }

    #[test]
    fn profile_policy_rejects_unknown_spec_key() {
        let mut config = minimal_valid_config();
        config.profiles.specs.insert(
            "unknown_profile".into(),
            SandboxSpec {
                backend: SandboxBackend::None,
                read_roots: Vec::new(),
                write_roots: Vec::new(),
                denied_roots: Vec::new(),
                network_class: NetworkClass::Denied,
                credential_class: CredentialClass::None,
                env_policy: EnvPolicy::Inherit,
                unsafe_marker: false,
            },
        );

        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::ProfilePolicyConflict
                && w.is_fatal
                && w.message.contains("unknown profile name 'unknown_profile'")
        }));
    }

    #[test]
    fn profile_policy_warns_when_unsafe_marker_on_non_unsafe_profile() {
        let mut config = minimal_valid_config();
        config.profiles.specs.insert(
            "operator".into(),
            SandboxSpec {
                backend: SandboxBackend::OsDefault,
                read_roots: Vec::new(),
                write_roots: Vec::new(),
                denied_roots: Vec::new(),
                network_class: NetworkClass::ModelOnly,
                credential_class: CredentialClass::EnvAllowlist,
                env_policy: EnvPolicy::Minimal,
                unsafe_marker: true,
            },
        );

        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::ProfilePolicyConflict
                && !w.is_fatal
                && w.message
                    .contains("unsafe_marker=true but profile name is not 'unsafe'")
        }));
    }

    #[test]
    fn profile_policy_warns_when_unsafe_profile_lacks_unsafe_marker() {
        let mut config = minimal_valid_config();
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
                unsafe_marker: false,
            },
        );

        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::ProfilePolicyConflict
                && !w.is_fatal
                && w.message
                    .contains("profiles.specs['unsafe'] should have unsafe_marker=true")
        }));
    }

    #[test]
    fn profile_policy_accepts_unsafe_profile_with_unsafe_marker() {
        let mut config = minimal_valid_config();
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

        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::ProfilePolicyConflict));
    }

    #[test]
    fn profile_policy_rejects_unknown_defaults_key() {
        let mut config = minimal_valid_config();
        config
            .profiles
            .defaults
            .insert("reviewre".into(), PermissionProfile::Reviewer);

        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::ProfilePolicyConflict
                && w.is_fatal
                && w.message.contains("unknown role kind 'reviewre'")
        }));
    }

    #[test]
    fn profile_policy_rejects_non_role_kind_defaults_key() {
        let mut config = minimal_valid_config();
        config
            .profiles
            .defaults
            .insert("advisor".into(), PermissionProfile::Reviewer);

        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::ProfilePolicyConflict
                && w.is_fatal
                && w.message.contains("unknown role kind 'advisor'")
        }));
    }

    #[test]
    fn profile_policy_accepts_custom_defaults_key() {
        let mut config = minimal_valid_config();
        config
            .profiles
            .defaults
            .insert("custom".into(), PermissionProfile::Reviewer);
        config.profiles.specs.insert(
            "reviewer".into(),
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

        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::ProfilePolicyConflict));
    }

    #[test]
    fn profile_policy_accepts_valid_profiles_in_launchers_and_lanes() {
        let mut config = minimal_valid_config();
        config.launchers.get_mut("codex").unwrap().profile = Some(PermissionProfile::Workspace);
        config.lanes.get_mut("codex").unwrap().profile = Some(PermissionProfile::Dependency);
        config.profiles.specs.insert(
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
        config.profiles.specs.insert(
            "dependency".into(),
            SandboxSpec {
                backend: SandboxBackend::OsDefault,
                read_roots: Vec::new(),
                write_roots: Vec::new(),
                denied_roots: Vec::new(),
                network_class: NetworkClass::Allowlisted,
                credential_class: CredentialClass::EnvAllowlist,
                env_policy: EnvPolicy::Minimal,
                unsafe_marker: false,
            },
        );

        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::ProfilePolicyConflict));
    }

    #[test]
    fn profile_policy_no_warnings_for_empty_profiles() {
        let config = minimal_valid_config();

        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::ProfilePolicyConflict));
    }

    #[test]
    fn profile_policy_warns_when_default_references_missing_spec() {
        let mut config = minimal_valid_config();
        config
            .profiles
            .defaults
            .insert("worker".into(), PermissionProfile::Workspace);

        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::ProfilePolicyConflict
                && !w.is_fatal
                && w.message
                    .contains("profiles.defaults['worker'] references profile 'workspace'")
        }));
    }

    #[test]
    fn profile_policy_warns_when_launcher_references_missing_spec() {
        let mut config = minimal_valid_config();
        config.launchers.get_mut("codex").unwrap().profile = Some(PermissionProfile::Workspace);

        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::ProfilePolicyConflict
                && !w.is_fatal
                && w.message
                    .contains("launcher 'codex' references profile 'workspace'")
        }));
    }

    #[test]
    fn profile_policy_warns_when_lane_references_missing_spec() {
        let mut config = minimal_valid_config();
        config.lanes.get_mut("codex").unwrap().profile = Some(PermissionProfile::Dependency);

        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::ProfilePolicyConflict
                && !w.is_fatal
                && w.message
                    .contains("lane 'codex' references profile 'dependency'")
        }));
    }

    #[test]
    fn share_after_submit_allows_kimi_reviewers() {
        let mut config = minimal_valid_config();
        config
            .launchers
            .insert("kimi".into(), launcher(AdapterKind::Kimi, Some("kimi")));
        config.lanes.insert(
            "kimi-reviewer".into(),
            LaneConfig {
                launcher: "kimi".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "kimi-reviewer".into(),
            model: Some(ModelConfig {
                provider: "moonshot".into(),
                name: "kimi-k2".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["kimi-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["kimi-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn share_after_submit_acp_form_allows_kimi_reviewers() {
        let mut config = minimal_valid_config();
        config
            .launchers
            .insert("kimi".into(), launcher(AdapterKind::Acp, Some("kimi")));
        config.launchers.get_mut("kimi").unwrap().args = vec!["acp".into()];
        config.lanes.insert(
            "kimi-reviewer".into(),
            LaneConfig {
                launcher: "kimi".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "kimi-reviewer".into(),
            model: Some(ModelConfig {
                provider: "moonshot".into(),
                name: "kimi-k2".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["kimi-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["kimi-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn share_after_submit_allows_gemini_reviewers() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "gemini".into(),
            launcher_with_details(AdapterKind::Acp, Some("gemini"), &["--acp"], None, None),
        );
        config.lanes.insert(
            "gemini-reviewer".into(),
            LaneConfig {
                launcher: "gemini".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "gemini-reviewer".into(),
            model: Some(ModelConfig {
                provider: "google".into(),
                name: "gemini-2.5-pro".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["gemini-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["gemini-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn share_after_submit_allows_gemini_reviewers_with_pty_control_plane_override() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "gemini".into(),
            launcher_with_details(
                AdapterKind::Acp,
                Some("gemini"),
                &["--acp"],
                None,
                Some("pty_injection"),
            ),
        );
        config.lanes.insert(
            "gemini-reviewer".into(),
            LaneConfig {
                launcher: "gemini".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "gemini-reviewer".into(),
            model: Some(ModelConfig {
                provider: "google".into(),
                name: "gemini-2.5-pro".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["gemini-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["gemini-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn share_after_submit_rejects_junie_reviewers() {
        let mut config = minimal_valid_config();
        config
            .launchers
            .insert("junie".into(), launcher(AdapterKind::Junie, Some("junie")));
        config.lanes.insert(
            "junie-reviewer".into(),
            LaneConfig {
                launcher: "junie".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "junie-reviewer".into(),
            model: Some(ModelConfig {
                provider: "jetbrains".into(),
                name: "junie-pro".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["junie-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["junie-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
                && warning.message.contains("junie-reviewer")
        }));
    }

    #[test]
    fn share_after_submit_rejects_acp_form_junie_reviewers() {
        let mut config = minimal_valid_config();
        config
            .launchers
            .insert("junie".into(), launcher(AdapterKind::Acp, Some("junie")));
        config.lanes.insert(
            "junie-reviewer".into(),
            LaneConfig {
                launcher: "junie".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "junie-reviewer".into(),
            model: Some(ModelConfig {
                provider: "jetbrains".into(),
                name: "junie-pro".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["junie-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["junie-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
                && warning.message.contains("junie-reviewer")
        }));
    }

    #[test]
    fn share_after_submit_rejects_junie_reviewers_without_command() {
        let mut config = minimal_valid_config();
        config
            .launchers
            .insert("junie".into(), launcher(AdapterKind::Junie, None));
        config.lanes.insert(
            "junie-reviewer".into(),
            LaneConfig {
                launcher: "junie".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "junie-reviewer".into(),
            model: Some(ModelConfig {
                provider: "jetbrains".into(),
                name: "junie-pro".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["junie-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["junie-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
                && warning.message.contains("junie-reviewer")
        }));
    }

    #[test]
    fn share_after_submit_rejects_acp_form_junie_with_task_args() {
        let mut config = minimal_valid_config();
        let mut junie = launcher(AdapterKind::Acp, Some("junie"));
        junie.args = vec!["--task".into()];
        config.launchers.insert("junie".into(), junie);
        config.lanes.insert(
            "junie-reviewer".into(),
            LaneConfig {
                launcher: "junie".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "junie-reviewer".into(),
            model: Some(ModelConfig {
                provider: "jetbrains".into(),
                name: "junie-pro".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["junie-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["junie-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
                && warning.message.contains("junie-reviewer")
        }));
    }

    #[test]
    fn share_after_submit_allows_copilot_reviewers() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "copilot".into(),
            launcher(AdapterKind::Copilot, Some("copilot")),
        );
        config.lanes.insert(
            "copilot-reviewer".into(),
            LaneConfig {
                launcher: "copilot".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "copilot-reviewer".into(),
            model: Some(ModelConfig {
                provider: "github".into(),
                name: "copilot-latest".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["copilot-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["copilot-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn share_after_submit_acp_form_allows_copilot_reviewers() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "copilot".into(),
            launcher(AdapterKind::Acp, Some("copilot")),
        );
        config.lanes.insert(
            "copilot-reviewer".into(),
            LaneConfig {
                launcher: "copilot".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "copilot-reviewer".into(),
            model: Some(ModelConfig {
                provider: "github".into(),
                name: "copilot-latest".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["copilot-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["copilot-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn share_after_submit_allows_opencode_reviewers() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "opencode".into(),
            launcher(AdapterKind::Acp, Some("opencode")),
        );
        config.lanes.insert(
            "opencode-reviewer".into(),
            LaneConfig {
                launcher: "opencode".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "opencode-reviewer".into(),
            model: Some(ModelConfig {
                provider: "opencode".into(),
                name: "opencode-latest".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["opencode-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["opencode-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn capability_override_acp_allows_shared_reset() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "custom-gateway".into(),
            launcher_with_details(
                AdapterKind::Acp,
                Some("custom-gateway"),
                &[],
                None,
                Some("acp"),
            ),
        );
        config.lanes.insert(
            "custom-gateway-reviewer".into(),
            LaneConfig {
                launcher: "custom-gateway".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "custom-gateway-reviewer".into(),
            model: Some(ModelConfig {
                provider: "custom".into(),
                name: "custom-model".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["custom-gateway-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["custom-gateway-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn capability_override_pty_requires_command_for_shared_reset() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "custom-pty".into(),
            launcher_with_details(
                AdapterKind::Acp,
                None,
                &[],
                Some("interactive_pty"),
                Some("pty_injection"),
            ),
        );
        config.lanes.insert(
            "custom-pty-reviewer".into(),
            LaneConfig {
                launcher: "custom-pty".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "custom-pty-reviewer".into(),
            model: Some(ModelConfig {
                provider: "custom".into(),
                name: "custom-model".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["custom-pty-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["custom-pty-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
                && warning.message.contains("custom-pty-reviewer")
        }));
    }

    #[test]
    fn capability_override_pty_with_incompatible_transport_rejects_shared_reset() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "custom-pty-bad-transport".into(),
            launcher_with_details(
                AdapterKind::Acp,
                Some("custom-pty-agent"),
                &[],
                Some("app_server"),
                Some("pty_injection"),
            ),
        );
        config.lanes.insert(
            "custom-pty-bad-transport-reviewer".into(),
            LaneConfig {
                launcher: "custom-pty-bad-transport".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "custom-pty-bad-transport-reviewer".into(),
            model: Some(ModelConfig {
                provider: "custom".into(),
                name: "custom-model".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["custom-pty-bad-transport-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["custom-pty-bad-transport-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
                && warning
                    .message
                    .contains("custom-pty-bad-transport-reviewer")
        }));
    }

    #[test]
    fn capability_override_pty_with_command_allows_shared_reset() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "custom-pty-cmd".into(),
            launcher_with_details(
                AdapterKind::Acp,
                Some("custom-pty-agent"),
                &[],
                Some("interactive_pty"),
                Some("pty_injection"),
            ),
        );
        config.lanes.insert(
            "custom-pty-cmd-reviewer".into(),
            LaneConfig {
                launcher: "custom-pty-cmd".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "custom-pty-cmd-reviewer".into(),
            model: Some(ModelConfig {
                provider: "custom".into(),
                name: "custom-model".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["custom-pty-cmd-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["custom-pty-cmd-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn builtin_one_shot_override_rejects_shared_reset() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "codex".into(),
            launcher_with_details(
                AdapterKind::Acp,
                Some("codex"),
                &["app-server"],
                None,
                Some("one_shot"),
            ),
        );
        config.lanes.insert(
            "codex-reviewer".into(),
            LaneConfig {
                launcher: "codex".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "codex-reviewer".into(),
            model: Some(ModelConfig {
                provider: "openai".into(),
                name: "codex".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["codex-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["codex-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
                && warning.message.contains("codex-reviewer")
        }));
    }

    #[test]
    fn custom_codex_app_server_one_shot_override_allows_shared_reset() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "codex".into(),
            launcher_with_details(
                AdapterKind::Acp,
                Some("codex"),
                &["app-server", "--flag"],
                None,
                Some("one_shot"),
            ),
        );
        config.lanes.insert(
            "codex-reviewer".into(),
            LaneConfig {
                launcher: "codex".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "codex-reviewer".into(),
            model: Some(ModelConfig {
                provider: "openai".into(),
                name: "codex".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["codex-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["codex-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn capability_override_native_hooks_allows_shared_reset() {
        let mut config = minimal_valid_config();
        config.launchers.insert(
            "custom-native".into(),
            launcher_with_details(
                AdapterKind::NativeAgent,
                Some("custom-native-agent"),
                &[],
                Some("native_hooks"),
                Some("native_hooks"),
            ),
        );
        config.lanes.insert(
            "custom-native-reviewer".into(),
            LaneConfig {
                launcher: "custom-native".into(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );
        config.roles.reviewers = vec![ReviewerPoolConfig {
            lane: "custom-native-reviewer".into(),
            model: Some(ModelConfig {
                provider: "custom".into(),
                name: "custom-model".into(),
            }),
            reasoning_effort: None,
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.lease_mode = brehon_types::config::ReviewLeaseMode::ShareAfterSubmit;
        config.review.default_reviewers = vec!["custom-native-reviewer".into()];
        config.review.panels = vec![brehon_types::ReviewPanelConfig {
            id: "primary".into(),
            reviewers: vec!["custom-native-reviewer".into()],
        }];

        let warnings = validate(&config);

        assert!(!warnings.iter().any(|warning| {
            warning.kind == ValidationWarningKind::ReviewPanelConflict
                && warning.message.contains("share_after_submit")
        }));
    }

    #[test]
    fn worktree_root_validation_accepts_none() {
        let config = minimal_valid_config();
        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::InvalidWorktreeRoot));
    }

    #[test]
    fn worktree_root_validation_accepts_valid_absolute_path() {
        let mut config = minimal_valid_config();
        config.orchestration.worktree_root = Some("/tmp/brehon-worktrees".into());
        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::InvalidWorktreeRoot));
    }

    #[test]
    fn worktree_root_validation_rejects_relative_path() {
        let mut config = minimal_valid_config();
        config.orchestration.worktree_root = Some(".brehon/worktrees".into());
        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::InvalidWorktreeRoot
                && w.is_fatal
                && w.message.contains("must be an absolute path")
        }));
    }

    #[test]
    fn worktree_root_validation_rejects_empty_string() {
        let mut config = minimal_valid_config();
        config.orchestration.worktree_root = Some("".into());
        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::InvalidWorktreeRoot
                && w.is_fatal
                && w.message.contains("must not be empty")
        }));
    }

    #[test]
    fn worktree_root_validation_rejects_path_traversal() {
        let mut config = minimal_valid_config();
        config.orchestration.worktree_root = Some("../outside".into());
        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::InvalidWorktreeRoot
                && w.is_fatal
                && w.message.contains("path traversal")
        }));
    }

    #[test]
    fn worktree_root_validation_rejects_embedded_traversal() {
        let mut config = minimal_valid_config();
        config.orchestration.worktree_root = Some("/safe/../unsafe".into());
        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::InvalidWorktreeRoot
                && w.is_fatal
                && w.message.contains("path traversal")
        }));
    }

    #[test]
    fn worktree_root_validation_rejects_null_bytes() {
        let mut config = minimal_valid_config();
        config.orchestration.worktree_root = Some("/tmp/brehon\0worktrees".into());
        let warnings = validate(&config);
        assert!(warnings.iter().any(|w| {
            w.kind == ValidationWarningKind::InvalidWorktreeRoot
                && w.is_fatal
                && w.message.contains("null bytes")
        }));
    }

    #[test]
    fn worktree_root_validation_accepts_dotdot_as_path_component_prefix() {
        let mut config = minimal_valid_config();
        config.orchestration.worktree_root = Some("/tmp/..cache/build".into());
        let warnings = validate(&config);
        assert!(!warnings
            .iter()
            .any(|w| w.kind == ValidationWarningKind::InvalidWorktreeRoot));
    }
}
