//! Role-related types for agent role assignments and permissions.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Kind of role an agent can fill.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum RoleKind {
    /// Supervisor: coordinates work, assigns tasks.
    Supervisor,
    /// Worker: implements tasks.
    Worker,
    /// Reviewer: reviews code.
    Reviewer,
    /// Custom role defined in configuration.
    Custom,
}

/// Permission level for a role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Create tasks.
    CreateTasks,
    /// Assign tasks to workers.
    AssignTasks,
    /// Send messages to agents.
    SendMessage,
    /// Broadcast to all agents.
    Broadcast,
    /// Read/write shared context.
    ContextReadWrite,
    /// Manage agents (spawn, kill).
    ManageAgents,
    /// Manage role assignments.
    ManageRoles,
    /// Execute code.
    ExecuteCode,
    /// Write files.
    WriteFiles,
    /// Push to branches.
    PushBranches,
    /// Create branches.
    CreateBranches,
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Permission::CreateTasks => write!(f, "create_tasks"),
            Permission::AssignTasks => write!(f, "assign_tasks"),
            Permission::SendMessage => write!(f, "send_message"),
            Permission::Broadcast => write!(f, "broadcast"),
            Permission::ContextReadWrite => write!(f, "context_read_write"),
            Permission::ManageAgents => write!(f, "manage_agents"),
            Permission::ManageRoles => write!(f, "manage_roles"),
            Permission::ExecuteCode => write!(f, "execute_code"),
            Permission::WriteFiles => write!(f, "write_files"),
            Permission::PushBranches => write!(f, "push_branches"),
            Permission::CreateBranches => write!(f, "create_branches"),
        }
    }
}

/// Role definition with capabilities.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleDefinition {
    /// Role name.
    pub name: String,
    /// Kind of role.
    pub kind: RoleKind,
    /// Description of this role.
    pub description: String,
    /// Permissions this role has.
    pub permissions: Vec<Permission>,
    /// System prompt for this role.
    pub system_prompt: Option<String>,
}

/// Scope of role permissions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum RoleScope {
    /// Permissions apply to this task only.
    Task,
    /// Permissions apply to this project.
    Project,
    /// Permissions apply globally.
    Global,
}

/// Assignment of an agent to a role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleAssignment {
    /// Agent assigned.
    pub agent_id: String,
    /// Role being filled.
    pub role: String,
    /// Scope of this assignment.
    pub scope: RoleScope,
    /// Model configuration for this assignment.
    pub model: Option<ModelConfig>,
}

/// Model configuration for a role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelConfig {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_kind_serialization() {
        let kind = RoleKind::Worker;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, r#""Worker""#);
        let parsed: RoleKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, RoleKind::Worker);
    }

    #[test]
    fn permission_display() {
        assert_eq!(format!("{}", Permission::CreateTasks), "create_tasks");
        assert_eq!(format!("{}", Permission::ManageAgents), "manage_agents");
    }

    #[test]
    fn role_definition() {
        let role = RoleDefinition {
            name: "supervisor".into(),
            kind: RoleKind::Supervisor,
            description: "Coordinates work".into(),
            permissions: vec![
                Permission::CreateTasks,
                Permission::AssignTasks,
                Permission::SendMessage,
            ],
            system_prompt: Some("You are a supervisor.".into()),
        };
        let json = serde_json::to_string(&role).unwrap();
        let parsed: RoleDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(role, parsed);
    }
}
