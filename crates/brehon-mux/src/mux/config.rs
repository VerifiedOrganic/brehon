use crate::harness::{AgentAdapter, SupervisorCli};
use crate::pty::TeamsSpawnConfig;
use brehon_ports::{PolicyGate, RuntimeEventSink};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Whether agent pane construction should start local processes or only build
/// restartable launch contracts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentPaneMaterialization {
    /// Create mux-owned PTY child processes immediately.
    #[default]
    Spawn,
    /// Build pane metadata and launch contracts without starting a process.
    PlanOnly,
}

/// Configuration for the multiplexer
#[derive(Clone)]
pub struct MuxConfig {
    /// Working directory for agents (supervisor and fallback for workers)
    pub cwd: PathBuf,
    /// Logical runtime session name for this Brehon run.
    pub session_name: Option<String>,
    /// Whether agent panes must use explicit isolated worktrees.
    pub worktree_isolation: bool,
    /// Agent pane construction mode.
    pub pane_materialization: AgentPaneMaterialization,
    /// Path to the .brehon directory (if set, passes BREHON_ROOT env var to agents)
    /// This allows workers in clone directories to access the main repo's Brehon state.
    pub brehon_root: Option<PathBuf>,
    /// Per-worker working directories (worker_name -> clone path)
    /// Workers not in this map use the default `cwd`.
    pub worker_cwds: HashMap<String, PathBuf>,
    /// Isolated supervisor working directory.
    pub supervisor_cwd: Option<PathBuf>,
    /// Per-reviewer working directories (reviewer_name -> clone path).
    pub reviewer_cwds: HashMap<String, PathBuf>,
    /// Per-advisor working directories (advisor_name -> clone path).
    pub advisor_cwds: HashMap<String, PathBuf>,
    /// Per-research-agent working directories (researcher_name -> clone path).
    pub research_cwds: HashMap<String, PathBuf>,
    /// Number of worker agents
    pub workers: usize,
    /// Worker names (if not provided, generated)
    pub worker_names: Vec<String>,
    /// Supervisor name
    pub supervisor_name: String,
    /// Supervisor agent adapter
    pub supervisor_cli: AgentAdapter,
    /// Worker agent adapter
    pub worker_cli: AgentAdapter,
    /// Per-worker adapter overrides. Key = worker name, Value = adapter.
    /// If a worker isn't in this map, falls back to worker_cli.
    pub worker_cli_map: HashMap<String, AgentAdapter>,
    /// Per-worker configured agent type (pool key / alias). Key = worker name.
    pub worker_agent_type_map: HashMap<String, String>,
    /// Per-worker model overrides. Key = worker name, Value = model string.
    pub worker_model_map: HashMap<String, String>,
    /// Per-worker reasoning effort overrides. Key = worker name, Value = effort string.
    /// Launchers map this to their backend-specific setting (for example,
    /// Claude `--effort`, Codex `model_reasoning_effort`, or OpenCode
    /// `reasoningEffort`).
    pub worker_reasoning_effort_map: HashMap<String, String>,
    /// Per-worker launcher environment overrides.
    pub worker_env_map: HashMap<String, Vec<(String, String)>>,
    /// Per-worker structured server URLs. Used by harnesses that expose a local
    /// control plane through the pane process itself.
    pub worker_server_url_map: HashMap<String, String>,
    /// Per-reviewer structured server URLs. Used by harnesses that expose a local
    /// control plane through the pane process itself.
    pub reviewer_server_url_map: HashMap<String, String>,
    /// Structured server URL for the supervisor, when its harness exposes a
    /// local control plane through the pane process itself.
    pub supervisor_server_url: Option<String>,
    /// Model for supervisor (passed as --model flag)
    pub supervisor_model: Option<String>,
    /// Reasoning effort for supervisor, when supported by the launcher.
    pub supervisor_reasoning_effort: Option<String>,
    /// Model for workers (passed as --model flag)
    pub worker_model: Option<String>,
    /// Optional reviewer pane name.
    pub reviewer_name: Option<String>,
    /// Reviewer pane names derived from reviewer-role pools.
    pub reviewer_names: Vec<String>,
    /// Reviewer agent adapter.
    pub reviewer_cli: AgentAdapter,
    /// Per-reviewer adapter overrides.
    pub reviewer_cli_map: HashMap<String, AgentAdapter>,
    /// Per-reviewer configured agent type (pool key / alias). Key = reviewer name.
    pub reviewer_agent_type_map: HashMap<String, String>,
    /// Model for reviewer (passed as --model flag)
    pub reviewer_model: Option<String>,
    /// Per-reviewer model overrides.
    pub reviewer_model_map: HashMap<String, String>,
    /// Per-reviewer reasoning effort overrides.
    pub reviewer_reasoning_effort_map: HashMap<String, String>,
    /// Per-reviewer launcher environment overrides.
    pub reviewer_env_map: HashMap<String, Vec<(String, String)>>,
    /// Per-reviewer configured review panel id. Key = reviewer name.
    pub reviewer_panel_map: HashMap<String, String>,
    /// Per-reviewer terminal-host tab name. Key = reviewer name.
    pub reviewer_panel_tab_map: HashMap<String, String>,
    /// Advisor pane names derived from advisor-role pools.
    pub advisor_names: Vec<String>,
    /// Advisor agent adapter.
    pub advisor_cli: AgentAdapter,
    /// Per-advisor adapter overrides.
    pub advisor_cli_map: HashMap<String, AgentAdapter>,
    /// Per-advisor configured agent type (pool key / alias). Key = advisor name.
    pub advisor_agent_type_map: HashMap<String, String>,
    /// Model for advisor panes.
    pub advisor_model: Option<String>,
    /// Per-advisor model overrides.
    pub advisor_model_map: HashMap<String, String>,
    /// Per-advisor reasoning effort overrides.
    pub advisor_reasoning_effort_map: HashMap<String, String>,
    /// Per-advisor launcher environment overrides.
    pub advisor_env_map: HashMap<String, Vec<(String, String)>>,
    /// Per-advisor structured server URLs.
    pub advisor_server_url_map: HashMap<String, String>,
    /// Research pane names derived from research pools.
    pub research_names: Vec<String>,
    /// Research agent adapter.
    pub research_cli: AgentAdapter,
    /// Per-research adapter overrides.
    pub research_cli_map: HashMap<String, AgentAdapter>,
    /// Per-research configured pool id. Key = research pane name.
    pub research_agent_type_map: HashMap<String, String>,
    /// Model for research panes.
    pub research_model: Option<String>,
    /// Per-research model overrides.
    pub research_model_map: HashMap<String, String>,
    /// Per-research reasoning effort overrides.
    pub research_reasoning_effort_map: HashMap<String, String>,
    /// Per-research launcher environment overrides.
    pub research_env_map: HashMap<String, Vec<(String, String)>>,
    /// Per-research structured server URLs.
    pub research_server_url_map: HashMap<String, String>,
    /// Configured supervisor agent type (role key / alias).
    pub supervisor_agent_type: Option<String>,
    /// Supervisor launcher environment overrides.
    pub supervisor_env: Vec<(String, String)>,
    /// Include director pane
    pub include_director: bool,
    /// Terminal size
    pub rows: u16,
    pub cols: u16,
    /// Per-agent Teams spawn configs (agent_name -> config).
    /// When set, agents are spawned with native Agent Teams CLI flags.
    pub teams_configs: HashMap<String, TeamsSpawnConfig>,
    /// Factory for direct API tool bridges used by managed API sessions.
    pub direct_tool_bridge_factory: Option<Arc<dyn brehon_acp::DirectToolBridgeFactory>>,
    /// Optional runtime event side-channel sink.
    pub runtime_event_sink: Option<Arc<dyn RuntimeEventSink>>,
    /// Optional policy gate for audited mutating runtime operations.
    pub policy_gate: Option<Arc<dyn PolicyGate>>,
}

impl Default for MuxConfig {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            session_name: None,
            worktree_isolation: false,
            pane_materialization: AgentPaneMaterialization::default(),
            brehon_root: None,
            worker_cwds: HashMap::new(),
            supervisor_cwd: None,
            reviewer_cwds: HashMap::new(),
            advisor_cwds: HashMap::new(),
            research_cwds: HashMap::new(),
            workers: 2,
            worker_names: vec![],
            supervisor_name: "supervisor".to_string(),
            supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
            worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
            worker_cli_map: HashMap::new(),
            worker_agent_type_map: HashMap::new(),
            worker_model_map: HashMap::new(),
            worker_reasoning_effort_map: HashMap::new(),
            worker_env_map: HashMap::new(),
            worker_server_url_map: HashMap::new(),
            reviewer_server_url_map: HashMap::new(),
            supervisor_server_url: None,
            supervisor_model: None,
            supervisor_reasoning_effort: None,
            worker_model: None,
            reviewer_name: None,
            reviewer_names: vec![],
            reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
            reviewer_cli_map: HashMap::new(),
            reviewer_agent_type_map: HashMap::new(),
            reviewer_model: None,
            reviewer_model_map: HashMap::new(),
            reviewer_reasoning_effort_map: HashMap::new(),
            reviewer_env_map: HashMap::new(),
            reviewer_panel_map: HashMap::new(),
            reviewer_panel_tab_map: HashMap::new(),
            advisor_names: vec![],
            advisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
            advisor_cli_map: HashMap::new(),
            advisor_agent_type_map: HashMap::new(),
            advisor_model: None,
            advisor_model_map: HashMap::new(),
            advisor_reasoning_effort_map: HashMap::new(),
            advisor_env_map: HashMap::new(),
            advisor_server_url_map: HashMap::new(),
            research_names: vec![],
            research_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
            research_cli_map: HashMap::new(),
            research_agent_type_map: HashMap::new(),
            research_model: None,
            research_model_map: HashMap::new(),
            research_reasoning_effort_map: HashMap::new(),
            research_env_map: HashMap::new(),
            research_server_url_map: HashMap::new(),
            supervisor_agent_type: None,
            supervisor_env: Vec::new(),
            include_director: true,
            rows: 24,
            cols: 80,
            teams_configs: HashMap::new(),
            direct_tool_bridge_factory: None,
            runtime_event_sink: None,
            policy_gate: None,
        }
    }
}
