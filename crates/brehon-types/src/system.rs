//! System state types.

use serde::{Deserialize, Serialize};

/// System state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum SystemState {
    /// System is recovering from crash/error.
    Recovering,
    /// System is running normally.
    #[default]
    Running,
    /// System is draining (shutting down gracefully).
    Draining,
    /// System is in safe mode (limited operation).
    SafeMode,
    /// System is stopped.
    Stopped,
}

impl SystemState {
    /// Return `true` if the system can serve requests (Running or SafeMode).
    pub fn is_operational(&self) -> bool {
        matches!(self, SystemState::Running | SystemState::SafeMode)
    }

    /// Return `true` if new tasks can be dispatched (Running only).
    pub fn can_dispatch_tasks(&self) -> bool {
        matches!(self, SystemState::Running)
    }

    /// Return `true` if new agent sessions can be spawned (Running or SafeMode).
    pub fn can_spawn_agents(&self) -> bool {
        matches!(self, SystemState::Running | SystemState::SafeMode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_state_is_operational() {
        assert!(SystemState::Running.is_operational());
        assert!(SystemState::SafeMode.is_operational());
        assert!(!SystemState::Recovering.is_operational());
        assert!(!SystemState::Draining.is_operational());
        assert!(!SystemState::Stopped.is_operational());
    }

    #[test]
    fn system_state_can_dispatch() {
        assert!(SystemState::Running.can_dispatch_tasks());
        assert!(!SystemState::SafeMode.can_dispatch_tasks());
        assert!(!SystemState::Draining.can_dispatch_tasks());
    }

    #[test]
    fn system_state_can_spawn() {
        assert!(SystemState::Running.can_spawn_agents());
        assert!(SystemState::SafeMode.can_spawn_agents());
        assert!(!SystemState::Recovering.can_spawn_agents());
    }

    #[test]
    fn system_state_roundtrip() {
        let states = vec![
            SystemState::Recovering,
            SystemState::Running,
            SystemState::Draining,
            SystemState::SafeMode,
            SystemState::Stopped,
        ];
        for state in states {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: SystemState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, parsed);
        }
    }

    #[test]
    fn system_state_default() {
        let state = SystemState::default();
        assert_eq!(state, SystemState::Running);
    }
}
