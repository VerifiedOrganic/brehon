//! DecisionEngine trait for AI-assisted decisions.

use async_trait::async_trait;

use brehon_types::{DecisionRequest, DecisionResponse};

use crate::PortError;

/// Trait for AI-assisted decision making.
///
/// The decision engine is used by the supervisor to make judgment calls
/// when deterministic logic is insufficient. This includes:
/// - Planning task execution order
/// - Assigning workers to tasks
/// - Providing guidance to stuck workers
/// - Resolving review deadlocks
/// - Handling merge conflicts
/// - Performing heartbeat sanity checks
///
/// Implementations may call out to actual AI models (via ACP) or use
/// mock/deterministic logic for testing.
#[async_trait]
pub trait DecisionEngine: Send + Sync {
    /// Request an AI decision.
    ///
    /// Given a decision request with context and options, returns the
    /// AI's decision along with reasoning and confidence.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The AI model is unavailable
    /// - The request cannot be processed
    /// - The response is invalid
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, PortError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_error_variant() {
        let e = PortError::Agent("model unavailable".into());
        assert!(matches!(e, PortError::Agent(_)));
    }
}
