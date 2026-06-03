// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph's agent runtime turn-state boundaries.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentIdentity {
    pub(crate) role: String,
    pub(crate) name: String,
    pub(crate) agent_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnStopReason {
    Stop,
    Cancelled,
    ProviderTimeout,
}

impl TurnStopReason {
    #[allow(dead_code)]
    pub(crate) fn as_protocol_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::Cancelled => "cancelled",
            Self::ProviderTimeout => "provider_timeout",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnState {
    pub(crate) session_id: String,
    pub(crate) identity: AgentIdentity,
    pub(crate) rounds: usize,
    pub(crate) stop_reason: TurnStopReason,
}

impl TurnState {
    pub(crate) fn new(
        session_id: impl Into<String>,
        role: impl Into<String>,
        name: impl Into<String>,
        agent_type: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            identity: AgentIdentity {
                role: role.into(),
                name: name.into(),
                agent_type: agent_type.into(),
            },
            rounds: 0,
            stop_reason: TurnStopReason::Stop,
        }
    }

    pub(crate) fn advance_round(&mut self) {
        self.rounds = self.rounds.saturating_add(1);
    }

    pub(crate) fn set_stop_reason(&mut self, reason: TurnStopReason) {
        self.stop_reason = reason;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_maps_to_protocol_strings() {
        assert_eq!(TurnStopReason::Stop.as_protocol_str(), "stop");
        assert_eq!(TurnStopReason::Cancelled.as_protocol_str(), "cancelled");
        assert_eq!(
            TurnStopReason::ProviderTimeout.as_protocol_str(),
            "provider_timeout"
        );
    }

    #[test]
    fn turn_state_tracks_rounds() {
        let mut state = TurnState::new("s1", "worker", "w1", "native");
        state.advance_round();
        state.advance_round();
        assert_eq!(state.rounds, 2);
        assert_eq!(state.identity.role, "worker");
    }
}
