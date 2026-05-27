//! Configuration types for Brehon.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use strum::{EnumIter, IntoEnumIterator, IntoStaticStr};

use crate::agent::AdapterKind;
use crate::review::ReviewPolicy;
use crate::role::{ModelConfig, RoleDefinition, RoleKind};

/// Root configuration structure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrehonConfig {
    /// Configuration version.
    pub version: u32,
    /// Launcher definitions (how to connect to a runtime/provider CLI).
    #[serde(default, alias = "agents")]
    pub launchers: HashMap<String, AgentConnectionConfig>,
    /// Logical lane definitions (runtime identity + model/prompt defaults).
    #[serde(default)]
    pub lanes: HashMap<String, LaneConfig>,
    /// Named project-wide prompt fragments.
    #[serde(default)]
    pub prompt_fragments: HashMap<String, PromptFragmentConfig>,
    /// Project-wide prompt policy.
    #[serde(default)]
    pub prompt_policy: PromptPolicyConfig,
    /// Role definitions (what they do).
    pub roles: RolesConfig,
    /// Optional worker routing policy. Explicit task execution_policy still wins.
    #[serde(default, skip_serializing_if = "RoutingConfig::is_default")]
    pub routing: RoutingConfig,
    /// Optional multi-agent advisor rooms for read-only brainstorming.
    #[serde(default, skip_serializing_if = "AdvisorConfig::is_default")]
    pub advisors: AdvisorConfig,
    /// Optional read-only research agents for structured context gathering.
    #[serde(default, skip_serializing_if = "ResearchConfig::is_default")]
    pub research: ResearchConfig,
    /// Review configuration.
    pub review: ReviewConfig,
    /// Supervisor configuration.
    pub supervisor: SupervisorConfig,
    /// Orchestration configuration.
    pub orchestration: OrchestrationConfig,
    /// Runtime side-channel configuration.
    #[serde(default)]
    pub runtime: RuntimeConfig,
    /// Budget configuration.
    pub budget: BudgetConfig,
    /// TUI configuration.
    pub tui: TuiConfig,
    /// Escalation configuration.
    pub escalation: EscalationConfig,
    /// Context/memory configuration.
    pub context: ContextConfig,
    /// Permissions configuration.
    pub permissions: PermissionsConfig,
    /// Retention configuration.
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Security configuration.
    pub security: SecurityConfig,
    /// Permission profiles and sandbox specifications.
    #[serde(default, skip_serializing_if = "ProfilesConfig::is_default")]
    pub profiles: ProfilesConfig,
}

/// Runtime side-channel and daemon configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Workflow ids allowed to request commands. Empty keeps workflows dry-run.
    #[serde(default)]
    pub enabled_workflows: Vec<String>,
    /// Terminal host selection for runtime-host experiments.
    #[serde(default)]
    pub terminal_host: RuntimeTerminalHostConfig,
    /// Retry policy for failed durable runs.
    #[serde(default)]
    pub retry: RetryPolicyConfig,
    /// Continuation policy for bounded same-run prompting.
    #[serde(default)]
    pub continuation: ContinuationPolicyConfig,
}

/// Retry policy for durable run failures.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryPolicyConfig {
    /// Whether retry decisions are enabled.
    pub enabled: bool,
    /// Maximum attempts for one logical run lane.
    pub max_attempts: u32,
    /// Base retry delay in milliseconds.
    pub base_delay_ms: u64,
    /// Maximum retry delay in milliseconds.
    pub max_delay_ms: u64,
    /// Deterministic jitter budget in milliseconds for schedulers.
    pub jitter_ms: u64,
}

impl Default for RetryPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 2,
            base_delay_ms: 10_000,
            max_delay_ms: 60_000,
            jitter_ms: 250,
        }
    }
}

/// Continuation policy for bounded same-run prompting.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContinuationPolicyConfig {
    /// Whether same-run continuation decisions are enabled.
    pub enabled: bool,
    /// Maximum prompt turns within one durable run.
    pub max_turns_per_run: u32,
    /// Idle seconds before a continuation prompt is eligible.
    pub idle_prompt_after_secs: u64,
}

impl Default for ContinuationPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_turns_per_run: 5,
            idle_prompt_after_secs: 300,
        }
    }
}

/// Terminal host selection for runtime-host experiments.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeTerminalHostConfig {
    /// Selected host kind. Omitted means the production embedded mux/TUI path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RuntimeTerminalHostKind>,
    /// Spawn one host-owned shell pane for observability while agent panes stay
    /// mux-owned. Omitted keeps the preview disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_pane: Option<bool>,
    /// Owner of worker/reviewer/supervisor panes. Omitted keeps the current
    /// production mux-owned pane path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_ownership: Option<RuntimeTerminalHostPaneOwnership>,
}

impl RuntimeTerminalHostConfig {
    pub fn effective_kind(&self) -> RuntimeTerminalHostKind {
        self.kind.unwrap_or_default()
    }

    pub fn preview_pane_enabled(&self) -> bool {
        self.preview_pane.unwrap_or(false)
    }

    pub fn effective_pane_ownership(&self) -> RuntimeTerminalHostPaneOwnership {
        self.pane_ownership.unwrap_or_default()
    }
}

/// Runtime pane ownership mode for agent panes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTerminalHostPaneOwnership {
    /// Current mux-owned PTY path.
    #[default]
    Mux,
    /// Future promoted terminal-host-owned agent panes.
    Host,
}

/// Supported terminal host names.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTerminalHostKind {
    /// Current embedded mux/TUI path.
    #[default]
    Embedded,
    /// Transcript-only protocol harness.
    Headless,
    /// Browser-hosted terminal adapter.
    Web,
    /// Native GUI terminal adapter.
    NativeGui,
}

/// Agent connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConnectionConfig {
    /// Which adapter to use.
    pub adapter: AdapterKind,
    /// Command to invoke for subprocess-backed adapters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments for the command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Provider backend for first-class native runtimes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional harness transport override for custom launchers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Optional harness control-plane override for custom launchers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_plane: Option<String>,
    /// Base URL for direct OpenAI-compatible adapters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Environment variable containing the API key for direct adapters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Permission mode for the Brehon-native runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Explicit permission profile override for this launcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<PermissionProfile>,
    /// Maximum native tool calls to execute concurrently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_tool_calls: Option<usize>,
    /// Assistant message extension fields preserved across native tool-call subturns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assistant_message_passthrough_fields: Vec<String>,
    /// Request-body path where lane reasoning effort should be written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort_param: Option<String>,
    /// Extra JSON object merged into native OpenAI-compatible requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Value>,
    /// Extra environment variables for subprocess-backed launchers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Extra static headers for direct adapters.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

impl AgentConnectionConfig {
    pub fn command_str(&self) -> Option<&str> {
        self.command.as_deref()
    }

    pub fn base_url_str(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    pub fn provider_str(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn api_key_env_str(&self) -> Option<&str> {
        self.api_key_env.as_deref()
    }

    pub fn transport_str(&self) -> Option<&str> {
        self.transport.as_deref()
    }

    pub fn control_plane_str(&self) -> Option<&str> {
        self.control_plane.as_deref()
    }

    pub fn permission_mode_str(&self) -> Option<&str> {
        self.permission_mode.as_deref()
    }

    pub fn max_parallel_tool_calls(&self) -> Option<usize> {
        self.max_parallel_tool_calls
    }

    pub fn assistant_message_passthrough_fields(&self) -> &[String] {
        &self.assistant_message_passthrough_fields
    }

    pub fn reasoning_effort_param_str(&self) -> Option<&str> {
        self.reasoning_effort_param.as_deref()
    }
}

/// Logical lane configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaneConfig {
    /// Launcher to use for this lane.
    pub launcher: String,
    /// Optional default model for this lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    /// Optional default reasoning effort for this lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Optional default system prompt for this lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Optional permission profile override for this lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<PermissionProfile>,
}

/// Named project-wide prompt fragment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptFragmentConfig {
    /// Which runtime roles this fragment should apply to.
    #[serde(default)]
    pub applies_to: Vec<PromptTarget>,
    /// Lower priorities are applied first.
    #[serde(default = "default_prompt_fragment_priority")]
    pub priority: i32,
    /// Prompt text for this fragment.
    pub text: String,
}

/// Prompt policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PromptPolicyConfig {
    /// Enabled prompt fragment ids, in project policy order.
    #[serde(default)]
    pub enabled: Vec<String>,
}

/// Runtime role target for prompt fragments.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PromptTarget {
    /// Target the supervisor role.
    Supervisor,
    /// Target worker roles.
    Worker,
    /// Target reviewer roles.
    Reviewer,
    /// Target advisor roles.
    Advisor,
    /// Target research roles.
    Research,
    /// Target all roles.
    All,
}

impl PromptTarget {
    /// Parse a role name string into a `PromptTarget`, returning `None` for unrecognized roles.
    pub fn from_role_name(role: &str) -> Option<Self> {
        match role.trim() {
            "supervisor" => Some(Self::Supervisor),
            "worker" => Some(Self::Worker),
            "reviewer" => Some(Self::Reviewer),
            "advisor" => Some(Self::Advisor),
            "research" => Some(Self::Research),
            _ => None,
        }
    }
}

impl PromptFragmentConfig {
    /// Return `true` if this fragment should be included for the given role target.
    pub fn applies_to(&self, role: PromptTarget) -> bool {
        self.applies_to.is_empty()
            || self.applies_to.contains(&PromptTarget::All)
            || self.applies_to.contains(&role)
    }
}

fn default_prompt_fragment_priority() -> i32 {
    100
}

/// Roles configuration (supervisor, workers, reviewers).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RolesConfig {
    /// Supervisor role definition.
    pub supervisor: RoleDefinition,
    /// Worker pool definitions.
    pub workers: Vec<WorkerPoolConfig>,
    /// Reviewer pool definitions.
    pub reviewers: Vec<ReviewerPoolConfig>,
}

/// Worker pool configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerPoolConfig {
    /// Lane to use.
    #[serde(alias = "agent")]
    pub lane: String,
    /// Optional model override for this pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    /// Optional reasoning effort override for this pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Whether this pool participates in normal assignment or is reserved for
    /// tasks that explicitly target it.
    #[serde(default)]
    pub assignment_mode: WorkerAssignmentMode,
    /// Work classes accepted by a reserved pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepts: Vec<String>,
    /// Minimum instances.
    pub min: u32,
    /// Maximum instances.
    pub max: u32,
}

/// Assignment behavior for a worker pool.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkerAssignmentMode {
    /// Pool can receive ordinary worker assignments.
    #[default]
    Normal,
    /// Pool only receives tasks whose execution policy targets it.
    Reserved,
}

/// Optional config-level worker routing policy.
///
/// Routing rules produce an effective execution policy for tasks that do not
/// already carry an explicit `execution_policy`. This lets operators prefer
/// cheaper worker pools by default while reserving stronger models for risky
/// classes of work.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RoutingConfig {
    /// Optional fallback lane to prefer when no rule matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_worker_lane: Option<String>,
    /// Optional lane used by future escalation automation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_lane: Option<String>,
    /// Ordered routing rules. First match wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RoutingRuleConfig>,
    /// Optional escalation trigger configuration. Parsed now so configs can be
    /// shared before automated escalation uses it.
    #[serde(default, skip_serializing_if = "RoutingEscalationConfig::is_default")]
    pub escalation: RoutingEscalationConfig,
}

impl RoutingConfig {
    pub fn is_default(&self) -> bool {
        self.default_worker_lane.is_none()
            && self.escalation_lane.is_none()
            && self.rules.is_empty()
            && self.escalation.is_default()
    }
}

/// One ordered routing rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RoutingRuleConfig {
    /// Stable operator-facing identifier.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Match criteria for the task.
    #[serde(default, rename = "match")]
    pub criteria: RoutingRuleMatchConfig,
    /// Execution-policy fields to apply when this rule matches.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub policy: serde_json::Map<String, serde_json::Value>,
}

/// Match criteria for a routing rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingRuleMatchConfig {
    /// Matches every task. Useful for an explicit catch-all rule.
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,
    /// Match a single task type, such as `task`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    /// Match a single priority value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Match one work class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_class: Option<String>,
    /// Match any of these work classes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub work_classes: Vec<String>,
    /// Case-insensitive substrings matched against title only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_any: Vec<String>,
    /// Case-insensitive substrings matched against title, description, notes,
    /// acceptance criteria, test requirements, implementation notes, blockers,
    /// and imported source-gate text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_any: Vec<String>,
}

/// Future escalation automation knobs parsed from config.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingEscalationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_lane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_lane: Option<String>,
    #[serde(default, skip_serializing_if = "RoutingEscalationTriggers::is_default")]
    pub triggers: RoutingEscalationTriggers,
}

impl RoutingEscalationConfig {
    pub fn is_default(&self) -> bool {
        self.from_lane.is_none() && self.to_lane.is_none() && self.triggers.is_default()
    }
}

/// Future escalation trigger thresholds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingEscalationTriggers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_requested_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub integration_conflict: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dirty_main_detected: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewer_blocking_keywords: Vec<String>,
}

impl RoutingEscalationTriggers {
    pub fn is_default(&self) -> bool {
        self.changes_requested_rounds.is_none()
            && self.stalled_minutes.is_none()
            && !self.integration_conflict
            && !self.dirty_main_detected
            && self.reviewer_blocking_keywords.is_empty()
    }
}

/// Optional read-only advisor room configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AdvisorConfig {
    /// Enables advisor room orchestration. Omitted/false keeps the runtime inert.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    /// UI/runtime freshness target for advisor responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_timeout_secs: Option<u64>,
    /// Default room turn style when a room omits one.
    #[serde(default, skip_serializing_if = "AdvisorTurnMode::is_default")]
    pub default_turn_mode: AdvisorTurnMode,
    /// Advisor pools available for opt-in rooms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<AdvisorPoolConfig>,
    /// Named chat/brainstorming rooms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rooms: Vec<AdvisorRoomConfig>,
}

impl AdvisorConfig {
    pub fn is_default(&self) -> bool {
        !self.enabled
            && self.response_timeout_secs.is_none()
            && self.default_turn_mode.is_default()
            && self.pools.is_empty()
            && self.rooms.is_empty()
    }
}

/// Advisor interaction mode.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorTurnMode {
    /// Anyone may respond when useful.
    #[default]
    OpenChat,
    /// Ask participants in declared order.
    RoundRobin,
    /// Encourage dissent before synthesis.
    Debate,
    /// Produce one combined answer from the room.
    Synthesis,
    /// Watch context and speak only when explicitly addressed.
    Watch,
}

impl AdvisorTurnMode {
    pub fn is_default(&self) -> bool {
        matches!(self, Self::OpenChat)
    }
}

/// Advisor pool configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdvisorPoolConfig {
    /// Lane to use for this advisor pool.
    #[serde(alias = "agent")]
    pub lane: String,
    /// Optional model override for this pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    /// Optional reasoning effort override for this pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Optional extra advisor-specific system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Minimum instances.
    pub min: u32,
    /// Maximum instances.
    pub max: u32,
    /// Rooms this pool may join. Empty means any configured room may use it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rooms: Vec<String>,
    /// Advisor permission level.
    #[serde(default)]
    pub permissions: AdvisorPermissions,
}

/// Advisor room configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AdvisorRoomConfig {
    /// Stable room id.
    #[serde(default)]
    pub id: String,
    /// Optional display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional room-specific turn style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_mode: Option<AdvisorTurnMode>,
    /// Advisor pool lanes in this room.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<String>,
    /// Context this room should consider.
    #[serde(default, skip_serializing_if = "AdvisorRoomContextConfig::is_default")]
    pub context: AdvisorRoomContextConfig,
}

/// Advisor room context attachments.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AdvisorRoomContextConfig {
    /// Task filters or task ids represented as operator-provided YAML/JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<serde_json::Value>,
    /// Repository-relative docs to attach to this room.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub docs: Vec<String>,
}

impl AdvisorRoomContextConfig {
    pub fn is_default(&self) -> bool {
        self.tasks.is_empty() && self.docs.is_empty()
    }
}

/// Advisor permission profile.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorPermissions {
    /// Advisor may read context and post room messages, but not edit repo state.
    #[default]
    ReadOnly,
    /// Reserved for future sandboxed experiments.
    Sandboxed,
}

/// Optional read-only research agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResearchConfig {
    /// Enables research orchestration. Omitted/false keeps the runtime inert.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    /// Artifact root relative to the project root by default.
    #[serde(
        default = "default_research_artifact_root",
        skip_serializing_if = "is_default_research_artifact_root"
    )]
    pub artifact_root: String,
    /// Explicit opt-out of the default `.brehon/runtime` containment check.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unsafe_allow_external_artifact_root: bool,
    /// Project-level research defaults.
    #[serde(default, skip_serializing_if = "ResearchDefaultsConfig::is_default")]
    pub defaults: ResearchDefaultsConfig,
    /// Prompt attachment policy.
    #[serde(default, skip_serializing_if = "ResearchAttachConfig::is_default")]
    pub attach: ResearchAttachConfig,
    /// Worker-initiated request policy.
    #[serde(
        default,
        skip_serializing_if = "ResearchWorkerRequestsConfig::is_default"
    )]
    pub worker_requests: ResearchWorkerRequestsConfig,
    /// Research pools available to routes and requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<ResearchPoolConfig>,
    /// Ordered research routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<ResearchRouteConfig>,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            artifact_root: default_research_artifact_root(),
            unsafe_allow_external_artifact_root: false,
            defaults: ResearchDefaultsConfig::default(),
            attach: ResearchAttachConfig::default(),
            worker_requests: ResearchWorkerRequestsConfig::default(),
            pools: Vec::new(),
            routes: Vec::new(),
        }
    }
}

impl ResearchConfig {
    pub fn is_default(&self) -> bool {
        !self.enabled
            && is_default_research_artifact_root(&self.artifact_root)
            && !self.unsafe_allow_external_artifact_root
            && self.defaults.is_default()
            && self.attach.is_default()
            && self.worker_requests.is_default()
            && self.pools.is_empty()
            && self.routes.is_empty()
    }
}

fn default_research_artifact_root() -> String {
    ".brehon/runtime/research".to_string()
}

fn is_default_research_artifact_root(value: &str) -> bool {
    value == ".brehon/runtime/research"
}

/// Project-wide research defaults.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchDefaultsConfig {
    #[serde(default = "default_research_max_parallel_jobs")]
    pub max_parallel_jobs: u32,
    #[serde(default = "default_research_job_timeout_secs")]
    pub job_timeout_secs: u64,
    #[serde(default = "default_research_max_summary_tokens")]
    pub max_summary_tokens: u32,
    #[serde(default = "default_research_max_artifact_bytes")]
    pub max_artifact_bytes: u64,
    #[serde(default = "default_true")]
    pub require_citations: bool,
    #[serde(default)]
    pub permissions: ResearchPermissions,
}

impl Default for ResearchDefaultsConfig {
    fn default() -> Self {
        Self {
            max_parallel_jobs: default_research_max_parallel_jobs(),
            job_timeout_secs: default_research_job_timeout_secs(),
            max_summary_tokens: default_research_max_summary_tokens(),
            max_artifact_bytes: default_research_max_artifact_bytes(),
            require_citations: true,
            permissions: ResearchPermissions::ReadOnly,
        }
    }
}

impl ResearchDefaultsConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn default_research_max_parallel_jobs() -> u32 {
    4
}

fn default_research_job_timeout_secs() -> u64 {
    180
}

fn default_research_max_summary_tokens() -> u32 {
    1800
}

fn default_research_max_artifact_bytes() -> u64 {
    200_000
}

/// Research prompt attachment policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchAttachConfig {
    #[serde(default = "default_true")]
    pub on_task_assignment: bool,
    #[serde(default = "default_true")]
    pub on_review_request: bool,
    #[serde(default)]
    pub on_advisor_room_context: bool,
    #[serde(default = "default_true")]
    pub include_manifest: bool,
    #[serde(default = "default_true")]
    pub include_summaries: bool,
    #[serde(default = "default_research_max_attached_artifacts")]
    pub max_attached_artifacts: usize,
}

impl Default for ResearchAttachConfig {
    fn default() -> Self {
        Self {
            on_task_assignment: true,
            on_review_request: true,
            on_advisor_room_context: false,
            include_manifest: true,
            include_summaries: true,
            max_attached_artifacts: default_research_max_attached_artifacts(),
        }
    }
}

impl ResearchAttachConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn default_research_max_attached_artifacts() -> usize {
    6
}

/// Worker-initiated research request limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchWorkerRequestsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_research_max_requests_per_task")]
    pub max_requests_per_task: u32,
    #[serde(default = "default_research_max_cost_units_per_task")]
    pub max_cost_units_per_task: u32,
    #[serde(default = "default_research_max_cost_units_per_request")]
    pub max_cost_units_per_request: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_roles: Vec<String>,
}

impl Default for ResearchWorkerRequestsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_requests_per_task: default_research_max_requests_per_task(),
            max_cost_units_per_task: default_research_max_cost_units_per_task(),
            max_cost_units_per_request: default_research_max_cost_units_per_request(),
            allowed_roles: Vec::new(),
        }
    }
}

impl ResearchWorkerRequestsConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn default_research_max_requests_per_task() -> u32 {
    3
}

fn default_research_max_cost_units_per_task() -> u32 {
    6
}

fn default_research_max_cost_units_per_request() -> u32 {
    3
}

/// Research pool configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchPoolConfig {
    pub id: String,
    #[serde(alias = "agent")]
    pub lane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_profile: Option<String>,
    pub role: String,
    pub min: u32,
    pub max: u32,
    #[serde(default = "default_research_cost_units")]
    pub cost_units: u32,
    #[serde(default)]
    pub permissions: ResearchPermissions,
    #[serde(default)]
    pub output_schema: ResearchOutputSchema,
}

fn default_research_cost_units() -> u32 {
    1
}

/// Research permission profile.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResearchPermissions {
    /// Read context and write research artifacts only.
    #[default]
    ReadOnly,
    /// Reserved for future sandboxed experiments.
    Sandboxed,
}

/// Supported research output schema identifiers.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResearchOutputSchema {
    #[default]
    SpecBrief,
    CodeMap,
    TestMatrix,
    RiskBrief,
}

impl ResearchOutputSchema {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpecBrief => "spec_brief",
            Self::CodeMap => "code_map",
            Self::TestMatrix => "test_matrix",
            Self::RiskBrief => "risk_brief",
        }
    }
}

/// Research route configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResearchRouteConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub trigger: ResearchTrigger,
    #[serde(default, rename = "continue", skip_serializing_if = "is_false")]
    pub continue_: bool,
    #[serde(default)]
    pub timeout_policy: ResearchTimeoutPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_jobs_per_task: Option<u32>,
    #[serde(default, rename = "match")]
    pub criteria: ResearchRouteMatchConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<ResearchJobTemplateConfig>,
    /// Explicitly parsed so validation can reject the field. Research routes
    /// are advisory and cannot block task progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

impl Default for ResearchRouteConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: true,
            trigger: ResearchTrigger::default(),
            continue_: false,
            timeout_policy: ResearchTimeoutPolicy::default(),
            max_jobs_per_task: None,
            criteria: ResearchRouteMatchConfig::default(),
            jobs: Vec::new(),
            required: None,
        }
    }
}

/// Research trigger point.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResearchTrigger {
    #[default]
    BeforeAssignment,
    BeforeReview,
    Manual,
}

/// Non-blocking timeout behavior for prompt construction.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResearchTimeoutPolicy {
    #[default]
    ContinueWithWarning,
    SkipUnready,
}

/// Match criteria for a research route.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResearchRouteMatchConfig {
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_class: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub work_classes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_status_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_size_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths_all: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_plan_any: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ResearchRouteMatchConfig {
    pub fn has_any_matcher(&self) -> bool {
        self.default
            || self.task_type.is_some()
            || self.priority.is_some()
            || self.work_class.is_some()
            || !self.work_classes.is_empty()
            || !self.title_any.is_empty()
            || !self.text_any.is_empty()
            || !self.task_status_any.is_empty()
            || !self.task_size_any.is_empty()
            || !self.changed_paths_any.is_empty()
            || !self.changed_paths_all.is_empty()
            || !self.source_plan_any.is_empty()
    }
}

/// One job template emitted by a research route.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchJobTemplateConfig {
    pub pool: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_template: String,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

/// Reviewer pool configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewerPoolConfig {
    /// Lane to use.
    #[serde(alias = "agent")]
    pub lane: String,
    /// Optional model override for this pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    /// Optional reasoning effort override for this pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Optional system prompt override for reviewer sessions in this pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Minimum instances.
    pub min: u32,
    /// Maximum instances.
    pub max: u32,
}

/// Review configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewConfig {
    /// Scoring policy.
    pub policy: ReviewPolicy,
    /// Timeout in minutes.
    pub timeout_minutes: u32,
    /// Auto-assign reviewers.
    pub auto_assign: bool,
    /// Default reviewers.
    pub default_reviewers: Vec<String>,
    /// How a review panel is formed.
    pub panel_mode: ReviewPanelMode,
    /// Whether leased reviewer panels remain exclusive for the task lifetime or
    /// can release reviewers after they submit.
    #[serde(default)]
    pub lease_mode: ReviewLeaseMode,
    /// Explicit review panels. When configured, tasks lease these named panels
    /// instead of selecting ad-hoc reviewers from the live pool.
    #[serde(default)]
    pub panels: Vec<ReviewPanelConfig>,
    /// Maximum diff tokens before chunking.
    pub max_diff_tokens: u32,
    /// Chunking strategy.
    pub chunk_strategy: ChunkStrategy,
    /// Stale detection settings.
    pub stale_detection: StaleDetectionConfig,
}

/// Explicit review panel definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewPanelConfig {
    /// Stable panel identifier.
    pub id: String,
    /// Reviewer lane slots for this panel, in order.
    ///
    /// Each entry refers to a reviewer lane name (for example
    /// `claude-reviewer`, `codex-reviewer`, `gemini-reviewer`). When the panel
    /// is leased, Brehon binds each slot to a live reviewer session of that lane.
    pub reviewers: Vec<String>,
}

/// Review panel formation strategy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPanelMode {
    /// All eligible reviewers at review start become the council for that round.
    FullCouncil,
    /// A bounded panel is selected from the eligible reviewer pool.
    FixedSize,
}

/// Review panel lease strategy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLeaseMode {
    /// A task keeps its panel lease until it reaches a terminal state.
    #[default]
    Exclusive,
    /// Reviewers are released after they submit and can be reused once their
    /// sessions are hard-reset.
    ShareAfterSubmit,
}

/// Chunking strategy for large diffs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ChunkStrategy {
    /// Chunk by directory.
    ByDirectory,
    /// Chunk by file.
    ByFile,
    /// Don't chunk.
    None,
}

/// Stale detection configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StaleDetectionConfig {
    /// Whether stale detection is enabled.
    pub enabled: bool,
    /// Files to ignore.
    pub ignore_files: Vec<String>,
    /// Stale detection strategy.
    pub strategy: StaleStrategy,
}

/// Stale detection strategy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum StaleStrategy {
    /// Full re-review on stale.
    FullReview,
    /// Delta review only.
    DeltaReview,
    /// Just warn.
    Warn,
}

impl Default for StaleDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ignore_files: Vec::new(),
            strategy: StaleStrategy::DeltaReview,
        }
    }
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            policy: ReviewPolicy::default(),
            timeout_minutes: 30,
            auto_assign: true,
            default_reviewers: Vec::new(),
            panel_mode: ReviewPanelMode::FullCouncil,
            lease_mode: ReviewLeaseMode::Exclusive,
            panels: Vec::new(),
            max_diff_tokens: 8000,
            chunk_strategy: ChunkStrategy::ByDirectory,
            stale_detection: StaleDetectionConfig::default(),
        }
    }
}

/// Supervisor configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// Optional explicit model configuration for the supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    /// Optional explicit reasoning effort override for the supervisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Autonomy level.
    pub autonomy: AutonomyLevel,
    /// Heartbeat interval in minutes.
    pub heartbeat_minutes: u32,
    /// Stuck detection settings.
    pub stuck_detection: StuckDetectionConfig,
    /// Nudge configuration.
    pub nudge: NudgeConfig,
}

/// Autonomy level for supervisor AI.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AutonomyLevel {
    /// Full AI decision making.
    Full,
    /// Guided (default).
    Guided,
    /// Minimal (only on stuck/failure).
    Minimal,
}

/// Stuck detection configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StuckDetectionConfig {
    /// Time threshold in minutes.
    pub time_threshold_minutes: u32,
    /// Whether to be operation-aware.
    pub operation_aware: bool,
    /// Whether to use pattern detection.
    pub pattern_detection: bool,
}

/// Nudge configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NudgeConfig {
    /// Send soft nudge after this many minutes.
    pub soft_after_minutes: u32,
    /// Send guidance nudge after this many minutes.
    pub guidance_after_minutes: u32,
}

/// Orchestration configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestrationConfig {
    /// Maximum active worker tasks at once.
    #[serde(alias = "dispatch_parallelism")]
    pub max_active_workers: u32,
    /// Whether to use worktree isolation.
    pub worktree_isolation: bool,
    /// Branch prefix.
    pub branch_prefix: String,
    /// Auto-cleanup worktrees.
    pub auto_cleanup_worktrees: bool,
    /// Worker idle behavior.
    pub worker_idle_behavior: WorkerIdleBehavior,
    /// Allow mutating idle work.
    pub allow_mutating_idle_work: bool,
    /// Self-improvement tasks.
    pub self_improve_tasks: Vec<String>,
    /// Override total spawned workers. If unset, Brehon uses the sum of pool minimums.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "worker_count"
    )]
    pub spawn_workers: Option<u32>,
    /// Maximum seconds to wait for in-flight work to drain during shutdown.
    /// After this deadline, remaining work is terminated.
    /// Defaults to 30 when unset in all config layers.
    /// Uses `Option` so that config layer inheritance works correctly:
    /// an omitted field is `None` and falls through to the base config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_timeout_secs: Option<u64>,
    /// Explicit external worktree root. When unset, Brehon computes a cross-platform
    /// default under the user's data directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_root: Option<String>,
}

impl OrchestrationConfig {
    /// Returns the effective drain timeout in seconds.
    ///
    /// Falls back to 30 if no config layer set the value.
    pub fn effective_drain_timeout_secs(&self) -> u64 {
        self.drain_timeout_secs.unwrap_or(30)
    }

    /// Resolve the effective worktree root directory.
    ///
    /// If `worktree_root` is explicitly configured, returns that path
    ///
    /// The explicit override, when set, must be an absolute path.
    /// Validation rejects relative overrides.
    ///
    /// Otherwise computes a platform-appropriate default under the user's
    /// data directory, scoped by `repo_identity` to avoid collisions
    /// across different repositories.
    pub fn resolve_worktree_root(&self, _project_root: &std::path::Path, repo_identity: &str) -> std::path::PathBuf {
        if let Some(root) = &self.worktree_root {
            std::path::PathBuf::from(root)
        } else {
            default_worktree_root(repo_identity)
        }
    }

    /// Returns the legacy in-repo worktree root for reading existing runtime records.
    pub fn legacy_worktree_root(project_root: &std::path::Path) -> std::path::PathBuf {
        project_root.join(".brehon").join("worktrees")
    }
}

/// Returns the platform-specific data directory for Brehon, if available.
fn platform_data_dir() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "brehon").map(|d| d.data_dir().to_path_buf())
}

/// Compute the platform-default external worktree root.
///
/// macOS: `~/Library/Application Support/brehon/worktrees/<identity>/`
/// Linux: `$XDG_DATA_HOME/brehon/worktrees/<identity>/` or `~/.local/share/brehon/worktrees/<identity>/`
/// Fallback (no home dir): `<temp_dir>/brehon/worktrees/<identity>/`
pub fn default_worktree_root(repo_identity: &str) -> std::path::PathBuf {
    default_worktree_root_with_base(platform_data_dir(), repo_identity)
}

fn default_worktree_root_with_base(
    base: Option<std::path::PathBuf>,
    repo_identity: &str,
) -> std::path::PathBuf {
    let base = base.unwrap_or_else(|| std::env::temp_dir().join("brehon"));
    base.join("worktrees").join(sanitize_repo_identity(repo_identity))
}

/// Sanitize a repo identity string for use as a filesystem path component.
fn sanitize_repo_identity(identity: &str) -> String {
    let sanitized: String = identity
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized.to_string()
    }
}

impl BrehonConfig {
    /// Return true when a lane is explicitly configured or exists via legacy launcher-only config.
    pub fn has_lane(&self, lane: &str) -> bool {
        self.lanes.contains_key(lane) || self.launchers.contains_key(lane)
    }

    /// Resolve the launcher key for a lane, falling back to legacy launcher-only configs.
    pub fn lane_launcher_name(&self, lane: &str) -> Option<&str> {
        self.lanes
            .get(lane)
            .map(|cfg| cfg.launcher.as_str())
            .or_else(|| {
                self.launchers
                    .get_key_value(lane)
                    .map(|(name, _)| name.as_str())
            })
    }

    /// Resolve the launcher config for a lane.
    pub fn lane_launcher(&self, lane: &str) -> Option<&AgentConnectionConfig> {
        let launcher = self.lane_launcher_name(lane)?;
        self.launchers.get(launcher)
    }

    /// Resolve the model for a lane, allowing a pool/supervisor override to take precedence.
    pub fn lane_model<'a>(
        &'a self,
        lane: &str,
        override_model: Option<&'a ModelConfig>,
    ) -> Option<&'a ModelConfig> {
        override_model.or_else(|| self.lanes.get(lane).and_then(|cfg| cfg.model.as_ref()))
    }

    /// Resolve reasoning effort for a lane, allowing a pool/supervisor override to take precedence.
    pub fn lane_reasoning_effort<'a>(
        &'a self,
        lane: &str,
        override_effort: Option<&'a str>,
    ) -> Option<&'a str> {
        override_effort.or_else(|| {
            self.lanes
                .get(lane)
                .and_then(|cfg| cfg.reasoning_effort.as_deref())
        })
    }

    /// Resolve the system prompt for a lane, allowing a pool/role override to take precedence.
    pub fn lane_system_prompt<'a>(
        &'a self,
        lane: &str,
        override_prompt: Option<&'a str>,
    ) -> Option<&'a str> {
        override_prompt.or_else(|| {
            self.lanes
                .get(lane)
                .and_then(|cfg| cfg.system_prompt.as_deref())
        })
    }

    /// Resolve the effective permission profile for a runtime role, optional
    /// lane/launcher identity, and optional explicit per-agent override.
    ///
    /// Resolution order is deterministic:
    /// 1. per-agent override
    /// 2. lane-level `profile`
    /// 3. launcher-level `profile`
    /// 4. `profiles.defaults` for supported role kinds
    /// 5. built-in runtime-role fallback
    pub fn effective_permission_profile(
        &self,
        role: PermissionProfileRole,
        lane: Option<&str>,
        override_profile: Option<PermissionProfile>,
    ) -> EffectivePermissionProfile<'_> {
        let lane = lane.map(str::trim).filter(|value| !value.is_empty());
        let lane_cfg = lane.and_then(|lane_name| self.lanes.get(lane_name));

        let (profile, source) = if let Some(profile) = override_profile {
            (profile, EffectivePermissionProfileSource::AgentOverride)
        } else if let Some(profile) = lane_cfg.and_then(|cfg| cfg.profile) {
            (profile, EffectivePermissionProfileSource::Lane)
        } else if let Some(profile) = lane
            .and_then(|lane_name| self.lane_launcher(lane_name))
            .and_then(|cfg| cfg.profile)
        {
            (profile, EffectivePermissionProfileSource::Launcher)
        } else if let Some(profile) = self.profiles.role_default(role) {
            (profile, EffectivePermissionProfileSource::ConfigRoleDefault)
        } else {
            (
                role.default_profile(),
                EffectivePermissionProfileSource::BuiltInRoleDefault,
            )
        };

        EffectivePermissionProfile {
            profile,
            source,
            spec: self.profiles.spec_for(profile),
        }
    }

    /// Resolve enabled project-wide prompt fragments for a role into a single
    /// prompt block ordered by priority, then fragment id.
    pub fn project_prompt_for_role(&self, role: PromptTarget) -> Option<String> {
        let mut fragments: Vec<(&str, &PromptFragmentConfig)> = self
            .prompt_policy
            .enabled
            .iter()
            .filter_map(|id| {
                self.prompt_fragments
                    .get(id)
                    .map(|fragment| (id.as_str(), fragment))
            })
            .filter(|(_, fragment)| fragment.applies_to(role))
            .filter(|(_, fragment)| !fragment.text.trim().is_empty())
            .collect();

        if fragments.is_empty() {
            return None;
        }

        fragments.sort_by(|(left_id, left), (right_id, right)| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left_id.cmp(right_id))
        });

        let mut rendered = String::from(
            "Project-wide engineering policy. These constraints apply across this repository:\n",
        );
        for (id, fragment) in fragments {
            rendered.push_str(&format!("\n[{id}]\n{}\n", fragment.text.trim()));
        }
        Some(rendered.trim_end().to_string())
    }

    /// Convenience wrapper over [`Self::project_prompt_for_role`] that accepts a role name string.
    pub fn project_prompt_for_role_name(&self, role: &str) -> Option<String> {
        let role = PromptTarget::from_role_name(role)?;
        self.project_prompt_for_role(role)
    }
}

/// Worker idle behavior during review wait.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WorkerIdleBehavior {
    /// Wait for review.
    Wait,
    /// Perform self-improvement tasks.
    SelfImprove,
}

/// Budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetConfig {
    /// Maximum total cost (null = unlimited).
    pub max_total_cost: Option<f64>,
    /// Maximum cost per task.
    pub max_cost_per_task: Option<f64>,
    /// Maximum tokens per agent.
    pub max_tokens_per_agent: Option<u64>,
    /// Alert threshold percentage.
    pub alert_threshold_percent: u8,
    /// Enforcement mode.
    pub enforcement: BudgetEnforcement,
}

/// Budget enforcement mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum BudgetEnforcement {
    /// Stop work when limit hit.
    Hard,
    /// Warn but continue.
    Soft,
}

/// TUI configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TuiConfig {
    /// Default layout.
    pub default_layout: LayoutPreset,
    /// Terminal mode.
    pub terminal_mode: TerminalMode,
    /// Notification settings.
    pub notifications: NotificationConfig,
    /// Keybindings preset.
    pub keybindings: String,
}

/// Layout preset.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum LayoutPreset {
    /// Equal-weight panes for all views.
    Balanced,
    /// Single focused pane with minimized sidebars.
    Focus,
    /// Bare-minimum UI chrome.
    Minimal,
    /// Wider panes optimized for large terminals.
    Wide,
}

/// Terminal mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TerminalMode {
    /// Auto-detect from capabilities.
    Auto,
    /// Always use interactive terminal.
    Interactive,
    /// Always use transcript.
    Transcript,
}

/// Notification configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationConfig {
    /// Toast duration in seconds.
    pub toast_duration_seconds: u32,
    /// Flash tabs for notifications.
    pub flash_tabs: bool,
    /// Show modal for critical issues.
    pub modal_on_critical: bool,
}

/// Escalation configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EscalationConfig {
    /// Whether to escalate to human.
    pub human_in_loop: bool,
    /// Notification method.
    pub notify_via: NotifyMethod,
    /// Timeout before escalation.
    pub escalation_timeout_minutes: u32,
}

/// Notification method.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NotifyMethod {
    /// Notify via the terminal UI.
    Terminal,
    /// Notify via an external webhook.
    Webhook,
    /// No notification.
    None,
}

/// Context/memory configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextConfig {
    /// Database path.
    pub db_path: String,
    /// Search index path.
    pub search_index_path: String,
    /// Memory TTL in days (null = no expiry).
    pub memory_ttl_days: Option<u32>,
    /// Maximum memories.
    pub max_memories: u32,
    /// How to handle AGENTS.md.
    pub agents_md: AgentsMdMode,
    /// Token-efficient context retrieval controls.
    #[serde(default)]
    pub retrieval: ContextRetrievalConfig,
    /// Deterministic compression controls for model-facing context.
    #[serde(default)]
    pub compression: ContextCompressionConfig,
}

/// Token-efficient context retrieval controls.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ContextRetrievalConfig {
    /// Default result count for context search/list tools.
    #[serde(default = "default_context_retrieval_default_limit")]
    pub default_limit: usize,
    /// Maximum result count accepted by context tools.
    #[serde(default = "default_context_retrieval_max_limit")]
    pub max_limit: usize,
    /// Maximum model-facing snippet length in characters.
    #[serde(default = "default_context_retrieval_snippet_chars")]
    pub snippet_chars: usize,
}

impl Default for ContextRetrievalConfig {
    fn default() -> Self {
        Self {
            default_limit: default_context_retrieval_default_limit(),
            max_limit: default_context_retrieval_max_limit(),
            snippet_chars: default_context_retrieval_snippet_chars(),
        }
    }
}

fn default_context_retrieval_default_limit() -> usize {
    5
}

fn default_context_retrieval_max_limit() -> usize {
    20
}

fn default_context_retrieval_snippet_chars() -> usize {
    240
}

/// Deterministic compression controls for model-facing context.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ContextCompressionConfig {
    /// Enable deterministic compact context.
    #[serde(default)]
    pub enabled: bool,
    /// Compression algorithm/mode.
    #[serde(default)]
    pub mode: ContextCompressionMode,
    /// Preserve raw content alongside compact content.
    #[serde(default = "default_true")]
    pub store_raw: bool,
    /// Store and return compact memory text when compression is enabled.
    #[serde(default = "default_true")]
    pub compact_memories: bool,
    /// Store and return compact rule text when compression is enabled.
    #[serde(default = "default_true")]
    pub compact_rules: bool,
    /// Return compact task context when compression is enabled.
    #[serde(default = "default_true")]
    pub compact_tasks: bool,
}

impl Default for ContextCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ContextCompressionMode::DeterministicTerse,
            store_raw: true,
            compact_memories: true,
            compact_rules: true,
            compact_tasks: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Supported context compression mode.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompressionMode {
    /// Deterministic rule-based prose compaction.
    #[default]
    DeterministicTerse,
}

/// AGENTS.md handling mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AgentsMdMode {
    /// Auto-detect.
    Auto,
    /// Ignore.
    Ignore,
    /// Required.
    Required,
}

/// Permissions configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PermissionsConfig {
    /// Default permission for unknown actions.
    #[serde(flatten)]
    pub categories: HashMap<String, PermissionCategory>,
}

/// A permission category (e.g., "bash", "edit") or the default wildcard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PermissionCategory {
    /// A simple permission value (for wildcard entries).
    Simple(PermissionValue),
    /// A nested category with specific action permissions.
    Nested(HashMap<String, PermissionValue>),
}

/// Permission value.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PermissionValue {
    /// Permit the action without prompting.
    Allow,
    /// Block the action silently.
    Deny,
    /// Prompt the user before allowing.
    Ask,
}

/// Security configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityConfig {
    /// Sandbox profile.
    pub sandbox_profile: SandboxProfile,
    /// Persist transcripts.
    pub persist_transcripts: bool,
    /// Patterns to redact from logs.
    pub redact_patterns: Vec<String>,
    /// Environment variables to allow.
    pub env_allowlist: Vec<String>,
}

/// Legacy coarse-grained sandbox profile.
///
/// `ProfilesConfig`/`SandboxSpec` carries the newer per-profile sandbox model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum SandboxProfile {
    /// No sandboxing.
    None,
    /// Use the OS-provided default sandbox.
    OsDefault,
    /// Use a custom sandbox profile.
    Custom,
}

/// Named permission profile for agent sandboxing and capability restrictions.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, EnumIter, IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PermissionProfile {
    /// Read-only repo context; no shell writes; no network except model API.
    Observe,
    /// Read-only repo, review metadata, diffs, and artifacts; no file writes.
    Reviewer,
    /// Read/write only inside assigned worktree; local tests allowed.
    Workspace,
    /// Workspace plus allowlisted dependency/source-control egress.
    Dependency,
    /// Write only in integration worktree; controlled git/cherry-pick/test.
    Integrator,
    /// Can request elevated actions; high-risk actions are never auto-run.
    Operator,
    /// Current broad behavior; must be explicit, visible, and audited.
    Unsafe,
}

impl PermissionProfile {
    /// Iterate the canonical permission profiles recognized by the config model.
    pub fn variants() -> impl Iterator<Item = Self> {
        <Self as IntoEnumIterator>::iter()
    }

    /// Canonical serialized profile name used in config files.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Iterate the canonical serialized profile names used in config files.
    pub fn names() -> impl Iterator<Item = &'static str> {
        Self::variants().map(Self::as_str)
    }
}

/// Runtime role classification for permission-profile resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionProfileRole {
    /// Supervisor runtime.
    Supervisor,
    /// Worker runtime.
    Worker,
    /// Reviewer runtime.
    Reviewer,
    /// Advisor runtime.
    Advisor,
    /// Research runtime.
    Research,
    /// Integration/runtime-maintenance role.
    Integrator,
    /// Custom or otherwise unclassified runtime.
    Custom,
}

impl PermissionProfileRole {
    /// Return the `profiles.defaults` key for roles that participate in
    /// config-level default overrides.
    pub fn defaults_key(self) -> Option<&'static str> {
        match self {
            Self::Supervisor => Some(RoleKind::Supervisor.profile_defaults_key()),
            Self::Worker => Some(RoleKind::Worker.profile_defaults_key()),
            Self::Reviewer => Some(RoleKind::Reviewer.profile_defaults_key()),
            Self::Custom => Some(RoleKind::Custom.profile_defaults_key()),
            Self::Advisor | Self::Research | Self::Integrator => None,
        }
    }

    /// Built-in fallback profile when neither config nor per-agent overrides
    /// are present.
    pub fn default_profile(self) -> PermissionProfile {
        match self {
            Self::Supervisor => PermissionProfile::Operator,
            Self::Worker => PermissionProfile::Workspace,
            Self::Reviewer => PermissionProfile::Reviewer,
            Self::Advisor | Self::Research | Self::Custom => PermissionProfile::Observe,
            Self::Integrator => PermissionProfile::Integrator,
        }
    }
}

impl From<RoleKind> for PermissionProfileRole {
    fn from(value: RoleKind) -> Self {
        match value {
            RoleKind::Supervisor => Self::Supervisor,
            RoleKind::Worker => Self::Worker,
            RoleKind::Reviewer => Self::Reviewer,
            RoleKind::Custom => Self::Custom,
        }
    }
}

/// Sandbox backend selection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    /// No sandbox enforcement.
    #[serde(alias = "None")]
    None,
    /// Use the OS-provided default sandbox mechanism.
    #[serde(alias = "OsDefault")]
    OsDefault,
    /// Linux bubblewrap namespace sandbox.
    #[serde(alias = "Bubblewrap")]
    Bubblewrap,
    /// macOS seatbelt sandbox.
    #[serde(alias = "Seatbelt")]
    Seatbelt,
}

impl std::fmt::Display for SandboxBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "None"),
            Self::OsDefault => write!(f, "OS Default"),
            Self::Bubblewrap => write!(f, "Bubblewrap"),
            Self::Seatbelt => write!(f, "Seatbelt"),
        }
    }
}

/// Network access classification.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NetworkClass {
    /// No network access.
    Denied,
    /// Only provider model API required by runtime.
    ModelOnly,
    /// Explicit allowlist of endpoints.
    Allowlisted,
    /// No network restrictions.
    Unrestricted,
}

impl std::fmt::Display for NetworkClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => write!(f, "Denied"),
            Self::ModelOnly => write!(f, "Model Only"),
            Self::Allowlisted => write!(f, "Allowlisted"),
            Self::Unrestricted => write!(f, "Unrestricted"),
        }
    }
}

/// Credential access classification.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CredentialClass {
    /// No credential access.
    None,
    /// Only explicitly allowlisted environment variables.
    EnvAllowlist,
    /// Read access to OS keychain.
    KeychainRead,
    /// No credential restrictions.
    Unrestricted,
}

impl std::fmt::Display for CredentialClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "None"),
            Self::EnvAllowlist => write!(f, "Env Allowlist"),
            Self::KeychainRead => write!(f, "Keychain Read"),
            Self::Unrestricted => write!(f, "Unrestricted"),
        }
    }
}

/// Environment variable inheritance policy.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EnvPolicy {
    /// Inherit ambient environment (legacy behavior).
    #[default]
    Inherit,
    /// Minimal clean environment.
    Minimal,
    /// Strict environment with explicit allowlist only.
    Strict,
}

impl std::fmt::Display for EnvPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inherit => write!(f, "Inherit"),
            Self::Minimal => write!(f, "Minimal"),
            Self::Strict => write!(f, "Strict"),
        }
    }
}

/// Filesystem root specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct FsRootSpec {
    /// Repository-relative or absolute path.
    pub path: String,
    /// Whether to include subdirectories.
    #[serde(default = "default_true")]
    pub recursive: bool,
}

/// Concrete sandbox specification for a permission profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SandboxSpec {
    /// Sandbox backend to use.
    pub backend: SandboxBackend,
    /// Readable filesystem roots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_roots: Vec<FsRootSpec>,
    /// Writable filesystem roots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_roots: Vec<FsRootSpec>,
    /// Explicitly denied filesystem roots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_roots: Vec<FsRootSpec>,
    /// Network access level.
    pub network_class: NetworkClass,
    /// Credential access level.
    pub credential_class: CredentialClass,
    /// Environment variable policy.
    #[serde(default)]
    pub env_policy: EnvPolicy,
    /// Whether this spec represents the explicit unsafe profile.
    #[serde(default)]
    pub unsafe_marker: bool,
}

/// Permission profiles configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfilesConfig {
    /// Default profile assigned to each lowercase role kind key (`supervisor`,
    /// `worker`, `reviewer`, or `custom`) when no override is present.
    #[serde(default)]
    pub defaults: BTreeMap<String, PermissionProfile>,
    /// Per-profile sandbox specifications. Keys must be valid profile names.
    #[serde(default)]
    pub specs: BTreeMap<String, SandboxSpec>,
}

impl ProfilesConfig {
    pub fn is_default(&self) -> bool {
        self.defaults.is_empty() && self.specs.is_empty()
    }

    /// Resolve a config-level default profile for the given runtime role.
    pub fn role_default(&self, role: PermissionProfileRole) -> Option<PermissionProfile> {
        role.defaults_key()
            .and_then(|key| self.defaults.get(key).copied())
    }

    /// Resolve the configured sandbox spec for a profile, if present.
    pub fn spec_for(&self, profile: PermissionProfile) -> Option<&SandboxSpec> {
        self.specs.get(profile.as_str())
    }
}

/// Source of an effective permission-profile decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectivePermissionProfileSource {
    /// Explicit runtime/per-agent override.
    AgentOverride,
    /// Lane-level override.
    Lane,
    /// Launcher-level override.
    Launcher,
    /// Configured role default from `profiles.defaults`.
    ConfigRoleDefault,
    /// Built-in fallback for the runtime role.
    BuiltInRoleDefault,
}

impl std::fmt::Display for EffectivePermissionProfileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentOverride => write!(f, "Agent Override"),
            Self::Lane => write!(f, "Lane Override"),
            Self::Launcher => write!(f, "Launcher Override"),
            Self::ConfigRoleDefault => write!(f, "Config Role Default"),
            Self::BuiltInRoleDefault => write!(f, "Built-in Role Default"),
        }
    }
}

/// Deterministic effective permission-profile resolution result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectivePermissionProfile<'a> {
    /// Resolved profile name.
    pub profile: PermissionProfile,
    /// Where the resolved profile came from.
    pub source: EffectivePermissionProfileSource,
    /// Concrete sandbox spec for the profile when configured.
    pub spec: Option<&'a SandboxSpec>,
}

/// Retention and boundedness configuration.
///
/// `Default` returns zero/`None` for every field so merge logic can distinguish
/// "explicitly set" from "fall back to base/default". Serde deserialization
/// still applies the non-zero defaults via the per-field `serde(default = ...)`
/// attributes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionConfig {
    /// Maximum number of events to retain in the hot event log.
    /// When the log exceeds this count, oldest events are pruned
    /// after ensuring views are fully rebuilt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_events: Option<u64>,
    /// TTL for idempotency keys in hours.
    /// Keys older than this are swept during startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_ttl_hours: Option<u64>,
    /// Maximum completed tasks to track in-memory for dependency resolution.
    #[serde(default = "default_max_completed_tasks")]
    pub max_completed_tasks: u64,
    /// Maximum assignment history entries to retain per worker pool.
    #[serde(default = "default_max_assignment_history")]
    pub max_assignment_history: u64,
    /// Maximum tasks to retain in-memory in the task board.
    #[serde(default = "default_max_tasks")]
    pub max_tasks: u64,
    /// Minimum seconds between retention sweeps in the orchestrator.
    /// Defaults to 60 seconds.
    #[serde(default = "default_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
}

/// Canonical default retention sweep interval (seconds).
pub const DEFAULT_RETENTION_SWEEP_INTERVAL_SECS: u64 = 60;

fn default_max_completed_tasks() -> u64 {
    10_000
}

fn default_max_assignment_history() -> u64 {
    1_000
}

fn default_max_tasks() -> u64 {
    10_000
}

fn default_sweep_interval_secs() -> u64 {
    DEFAULT_RETENTION_SWEEP_INTERVAL_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn autonomy_level_serialization() {
        let level = AutonomyLevel::Guided;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, r#""Guided""#);
        let parsed: AutonomyLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AutonomyLevel::Guided);
    }

    #[test]
    fn budget_enforcement_roundtrip() {
        let modes = vec![BudgetEnforcement::Hard, BudgetEnforcement::Soft];
        for mode in modes {
            let json = serde_json::to_string(&mode).unwrap();
            let parsed: BudgetEnforcement = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, parsed);
        }
    }

    #[test]
    fn context_compression_defaults_to_disabled_when_omitted() {
        let parsed: ContextCompressionConfig = serde_json::from_str("{}").unwrap();

        assert!(!parsed.enabled);
        assert!(parsed.store_raw);
        assert!(parsed.compact_memories);
        assert!(parsed.compact_rules);
        assert!(parsed.compact_tasks);
    }

    #[test]
    fn layout_preset_variants() {
        let presets = vec![
            LayoutPreset::Balanced,
            LayoutPreset::Focus,
            LayoutPreset::Minimal,
            LayoutPreset::Wide,
        ];
        for preset in presets {
            let json = serde_json::to_string(&preset).unwrap();
            let parsed: LayoutPreset = serde_json::from_str(&json).unwrap();
            assert_eq!(preset, parsed);
        }
    }

    #[test]
    fn agent_connection_config() {
        let config = AgentConnectionConfig {
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
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AgentConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn runtime_terminal_host_config_roundtrip() {
        let config = RuntimeConfig {
            enabled_workflows: vec!["rate_limit.quarantine_recommendation".into()],
            terminal_host: RuntimeTerminalHostConfig {
                kind: Some(RuntimeTerminalHostKind::Headless),
                preview_pane: Some(true),
                pane_ownership: Some(RuntimeTerminalHostPaneOwnership::Host),
            },
            retry: RetryPolicyConfig::default(),
            continuation: ContinuationPolicyConfig::default(),
        };

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"kind\":\"headless\""));
        assert!(json.contains("\"preview_pane\":true"));
        assert!(json.contains("\"pane_ownership\":\"host\""));
        let parsed: RuntimeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, config);
        assert_eq!(
            parsed.terminal_host.effective_kind(),
            RuntimeTerminalHostKind::Headless
        );
        assert!(parsed.terminal_host.preview_pane_enabled());
        assert_eq!(
            parsed.terminal_host.effective_pane_ownership(),
            RuntimeTerminalHostPaneOwnership::Host
        );
    }

    #[test]
    fn runtime_terminal_host_defaults_to_embedded() {
        let config: RuntimeConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(
            config.terminal_host.effective_kind(),
            RuntimeTerminalHostKind::Embedded
        );
        assert!(!config.terminal_host.preview_pane_enabled());
        assert_eq!(
            config.terminal_host.effective_pane_ownership(),
            RuntimeTerminalHostPaneOwnership::Mux
        );
        assert_eq!(config.retry, RetryPolicyConfig::default());
        assert_eq!(config.continuation, ContinuationPolicyConfig::default());
    }

    #[test]
    fn advisor_config_roundtrip() {
        let config = AdvisorConfig {
            enabled: true,
            response_timeout_secs: Some(45),
            default_turn_mode: AdvisorTurnMode::Debate,
            pools: vec![AdvisorPoolConfig {
                lane: "kimi-advisor".into(),
                model: None,
                reasoning_effort: Some("medium".into()),
                system_prompt: Some("Be concise.".into()),
                min: 1,
                max: 3,
                rooms: vec!["release-war-room".into()],
                permissions: AdvisorPermissions::ReadOnly,
            }],
            rooms: vec![AdvisorRoomConfig {
                id: "release-war-room".into(),
                title: Some("Release War Room".into()),
                turn_mode: Some(AdvisorTurnMode::Synthesis),
                participants: vec!["kimi-advisor".into()],
                context: AdvisorRoomContextConfig {
                    tasks: vec![serde_json::json!({"status": "ready"})],
                    docs: vec!["docs/handoff.md".into()],
                },
            }],
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: AdvisorConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, config);
        assert!(!AdvisorConfig::default().enabled);
        assert!(AdvisorConfig::default().is_default());
    }

    #[test]
    fn research_config_roundtrip() {
        let config = ResearchConfig {
            enabled: true,
            pools: vec![ResearchPoolConfig {
                id: "specs".into(),
                lane: "cheap-worker".into(),
                instruction_profile: Some("cite sources".into()),
                role: "normative_requirements".into(),
                min: 0,
                max: 2,
                cost_units: 1,
                permissions: ResearchPermissions::ReadOnly,
                output_schema: ResearchOutputSchema::SpecBrief,
            }],
            routes: vec![ResearchRouteConfig {
                id: "protocol-specs".into(),
                criteria: ResearchRouteMatchConfig {
                    text_any: vec!["RFC".into()],
                    ..ResearchRouteMatchConfig::default()
                },
                jobs: vec![ResearchJobTemplateConfig {
                    pool: "specs".into(),
                    id: "normative".into(),
                    depends_on: Vec::new(),
                    prompt_template: "Task {{task_id}}".into(),
                }],
                ..ResearchRouteConfig::default()
            }],
            ..ResearchConfig::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: ResearchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, config);
        assert!(!ResearchConfig::default().enabled);
        assert!(ResearchConfig::default().is_default());
        assert!(ResearchRouteConfig::default().enabled);
    }

    #[test]
    fn project_prompt_for_role_orders_and_filters_fragments() {
        let config = BrehonConfig {
            version: 1,
            launchers: HashMap::new(),
            lanes: HashMap::new(),
            prompt_fragments: HashMap::from([
                (
                    "process.tdd".into(),
                    PromptFragmentConfig {
                        applies_to: vec![PromptTarget::Worker, PromptTarget::Reviewer],
                        priority: 60,
                        text: "Write tests with the change.".into(),
                    },
                ),
                (
                    "architecture.hexagonal".into(),
                    PromptFragmentConfig {
                        applies_to: vec![PromptTarget::All],
                        priority: 50,
                        text: "Prefer ports-and-adapters boundaries.".into(),
                    },
                ),
            ]),
            prompt_policy: PromptPolicyConfig {
                enabled: vec!["process.tdd".into(), "architecture.hexagonal".into()],
            },
            roles: RolesConfig {
                supervisor: RoleDefinition {
                    name: "supervisor".into(),
                    kind: crate::role::RoleKind::Supervisor,
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
        };

        let worker_prompt = config
            .project_prompt_for_role(PromptTarget::Worker)
            .unwrap();
        assert!(worker_prompt.contains("[architecture.hexagonal]"));
        assert!(worker_prompt.contains("[process.tdd]"));
        assert!(
            worker_prompt.find("[architecture.hexagonal]").unwrap()
                < worker_prompt.find("[process.tdd]").unwrap()
        );

        let supervisor_prompt = config
            .project_prompt_for_role(PromptTarget::Supervisor)
            .unwrap();
        assert!(supervisor_prompt.contains("[architecture.hexagonal]"));
        assert!(!supervisor_prompt.contains("[process.tdd]"));
    }

    #[test]
    fn permission_profile_roundtrip() {
        for profile in PermissionProfile::variants() {
            let json = serde_json::to_string(&profile).unwrap();
            let parsed: PermissionProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(profile, parsed);
        }
    }

    #[test]
    fn permission_profile_snake_case_serialization() {
        for profile in PermissionProfile::variants() {
            assert_eq!(
                serde_json::to_string(&profile).unwrap(),
                format!("\"{}\"", profile.as_str())
            );
        }
    }

    #[test]
    fn invalid_permission_profile_is_rejected() {
        let err = serde_json::from_str::<PermissionProfile>("\"not_a_profile\"").unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }

    #[test]
    fn sandbox_spec_roundtrip() {
        let spec = SandboxSpec {
            backend: SandboxBackend::OsDefault,
            read_roots: vec![FsRootSpec {
                path: ".".to_string(),
                recursive: true,
            }],
            write_roots: vec![FsRootSpec {
                path: ".brehon/worktrees".to_string(),
                recursive: true,
            }],
            denied_roots: vec![FsRootSpec {
                path: "/etc".to_string(),
                recursive: false,
            }],
            network_class: NetworkClass::ModelOnly,
            credential_class: CredentialClass::EnvAllowlist,
            env_policy: EnvPolicy::Minimal,
            unsafe_marker: false,
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: SandboxSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, parsed);
    }

    #[test]
    fn sandbox_spec_omitted_fields_use_defaults() {
        let parsed: SandboxSpec = serde_json::from_str(
            r#"{"backend":"none","network_class":"denied","credential_class":"none"}"#,
        )
        .unwrap();
        assert_eq!(parsed.backend, SandboxBackend::None);
        assert!(parsed.read_roots.is_empty());
        assert!(parsed.write_roots.is_empty());
        assert!(parsed.denied_roots.is_empty());
        assert_eq!(parsed.network_class, NetworkClass::Denied);
        assert_eq!(parsed.credential_class, CredentialClass::None);
        assert_eq!(parsed.env_policy, EnvPolicy::Inherit);
        assert!(!parsed.unsafe_marker);
    }

    #[test]
    fn invalid_sandbox_backend_is_rejected() {
        let err = serde_json::from_str::<SandboxSpec>(
            r#"{"backend":"bogus","network_class":"denied","credential_class":"none"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }

    #[test]
    fn sandbox_backend_accepts_pascal_case_aliases() {
        // OsDefault is the overlapping legacy spelling from SandboxProfile.
        let parsed: SandboxBackend = serde_json::from_str("\"OsDefault\"").unwrap();
        assert_eq!(parsed, SandboxBackend::OsDefault);

        let parsed: SandboxBackend = serde_json::from_str("\"None\"").unwrap();
        assert_eq!(parsed, SandboxBackend::None);

        let parsed: SandboxBackend = serde_json::from_str("\"Bubblewrap\"").unwrap();
        assert_eq!(parsed, SandboxBackend::Bubblewrap);

        let parsed: SandboxBackend = serde_json::from_str("\"Seatbelt\"").unwrap();
        assert_eq!(parsed, SandboxBackend::Seatbelt);
    }

    #[test]
    fn profiles_config_defaults_to_empty() {
        let parsed: ProfilesConfig = serde_json::from_str("{}").unwrap();
        assert!(parsed.defaults.is_empty());
        assert!(parsed.specs.is_empty());
        assert!(parsed.is_default());
    }

    #[test]
    fn brehon_config_without_profiles_parses() {
        let yaml = r#"
version: 1
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: "Test"
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
"#;
        let parsed: BrehonConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(parsed.profiles.defaults.is_empty());
        assert!(parsed.profiles.specs.is_empty());
        let json = serde_json::to_value(&parsed).unwrap();
        assert!(json.get("profiles").is_none());
    }

    #[test]
    fn resolve_worktree_root_uses_explicit_absolute_override() {
        let config = OrchestrationConfig {
            worktree_root: Some("/tmp/custom-worktrees".to_string()),
            ..OrchestrationConfig {
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
            }
        };
        let resolved = config.resolve_worktree_root(std::path::Path::new("/project"), "repo-abc12345");
        assert_eq!(resolved, std::path::PathBuf::from("/tmp/custom-worktrees"));
    }

    #[test]
    fn default_worktree_root_contains_brehon_and_identity() {
        let path = default_worktree_root("my-repo-abc12345");
        assert!(path.is_absolute(), "default worktree root must be absolute: {path:?}");
        let components: Vec<_> = path.components().map(|c| c.as_os_str().to_string_lossy()).collect();
        assert!(components.iter().any(|c| c == "brehon"), "path should contain 'brehon': {path:?}");
        assert!(components.iter().any(|c| c == "worktrees"), "path should contain 'worktrees': {path:?}");
        assert!(components.iter().any(|c| c == "my-repo-abc12345"), "path should contain identity: {path:?}");
    }

    #[test]
    fn default_worktree_root_sanitizes_identity() {
        let path = default_worktree_root("My Repo!!!");
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(file_name, "my-repo");
    }

    #[test]
    fn default_worktree_root_sanitizes_all_special_chars_to_unknown() {
        let path = default_worktree_root("!!!");
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(file_name, "unknown");
    }

    #[test]
    fn default_worktree_root_with_base_uses_injected_macos_path() {
        let base = Some(std::path::PathBuf::from("/Users/test/Library/Application Support/brehon"));
        let path = default_worktree_root_with_base(base, "my-repo-abc12345");
        assert_eq!(path, std::path::PathBuf::from("/Users/test/Library/Application Support/brehon/worktrees/my-repo-abc12345"));
    }

    #[test]
    fn default_worktree_root_with_base_uses_injected_linux_xdg_path() {
        let base = Some(std::path::PathBuf::from("/home/test/.local/share/brehon"));
        let path = default_worktree_root_with_base(base, "my-repo-abc12345");
        assert_eq!(path, std::path::PathBuf::from("/home/test/.local/share/brehon/worktrees/my-repo-abc12345"));
    }

    #[test]
    fn default_worktree_root_with_base_falls_back_to_temp_when_none() {
        let path = default_worktree_root_with_base(None, "my-repo-abc12345");
        assert!(path.is_absolute(), "fallback path must be absolute: {path:?}");
        assert!(path.to_string_lossy().contains("worktrees"), "path should contain 'worktrees': {path:?}");
        assert!(path.to_string_lossy().contains("my-repo-abc12345"), "path should contain identity: {path:?}");
    }

    #[test]
    fn legacy_worktree_root_returns_in_repo_path() {
        let path = OrchestrationConfig::legacy_worktree_root(std::path::Path::new("/project"));
        assert_eq!(path, std::path::PathBuf::from("/project/.brehon/worktrees"));
    }

    #[test]
    fn resolve_worktree_root_is_always_absolute() {
        // resolve_worktree_root must always return an absolute path,
        // whether from an explicit override, a platform default, or a fallback.
        let config = OrchestrationConfig {
            worktree_root: None,
            ..OrchestrationConfig {
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
            }
        };
        let resolved = config.resolve_worktree_root(std::path::Path::new("/project"), "repo-123");
        assert!(resolved.is_absolute(), "resolved worktree root must be absolute: {resolved:?}");
    }
}

