use std::path::PathBuf;

/// Provider-neutral launch policy derived from the resolved permission profile.
///
/// Carries sandbox intent so that each provider launcher can map it to its
/// native CLI flags and config without silently falling back to unsafe
/// behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchPolicy {
    pub sandbox_profile: brehon_types::SandboxProfile,
}

impl LaunchPolicy {
    /// Create a launch policy from the project security configuration.
    pub fn from_security_config(security: &brehon_types::config::SecurityConfig) -> Self {
        Self {
            sandbox_profile: security.sandbox_profile,
        }
    }

    /// Whether this policy represents an explicit no-sandbox / unsafe state.
    pub fn is_unsafe(&self) -> bool {
        matches!(self.sandbox_profile, brehon_types::SandboxProfile::None)
    }

    /// Human-readable profile name for telemetry and UI labels.
    pub fn profile_name(&self) -> &'static str {
        match self.sandbox_profile {
            brehon_types::SandboxProfile::None => "unsafe",
            brehon_types::SandboxProfile::OsDefault => "os_default",
            brehon_types::SandboxProfile::Custom => "custom",
        }
    }
}

/// Configuration for spawning a PTY
#[derive(Debug, Clone)]
pub struct PtyConfig {
    /// Command to run (e.g., "claude")
    pub command: String,
    /// Arguments for the command
    pub args: Vec<String>,
    /// Working directory
    pub cwd: Option<PathBuf>,
    /// Environment variables to set
    pub env: Vec<(String, String)>,
    /// Initial terminal size
    pub rows: u16,
    pub cols: u16,
}

/// Configuration for spawning an agent with native Claude Code Agent Teams flags.
#[derive(Debug, Clone)]
pub struct TeamsSpawnConfig {
    /// Team name (factory session name)
    pub team_name: String,
    /// Agent ID (e.g., "worker-1@session-name")
    pub agent_id: String,
    /// Agent display name
    pub agent_name: String,
    /// Agent color for UI
    pub agent_color: String,
    /// Agent type (e.g., "team-lead", "general-purpose")
    pub agent_type: String,
    /// Parent session ID for analytics correlation (workers only)
    pub parent_session_id: Option<String>,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            command: "bash".to_string(),
            args: vec![],
            cwd: None,
            env: vec![],
            rows: 24,
            cols: 80,
        }
    }
}
