//! Budget tracking types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current budget state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetState {
    /// Total cost spent.
    pub total_cost: f64,
    /// Total tokens used.
    pub total_tokens: u64,
    /// Cost per agent.
    pub cost_by_agent: Vec<(String, f64)>,
    /// Tokens per agent.
    pub tokens_by_agent: Vec<(String, u64)>,
    /// Soft limit alert triggered.
    pub soft_alert_triggered: bool,
    /// Hard limit hit.
    pub hard_limit_hit: bool,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
}

impl Default for BudgetState {
    fn default() -> Self {
        Self {
            total_cost: 0.0,
            total_tokens: 0,
            cost_by_agent: vec![],
            tokens_by_agent: vec![],
            soft_alert_triggered: false,
            hard_limit_hit: false,
            updated_at: Utc::now(),
        }
    }
}

/// Token usage tracking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    /// Input tokens (prompt).
    pub input_tokens: u64,
    /// Output tokens (response).
    pub output_tokens: u64,
    /// Total tokens.
    pub total_tokens: u64,
    /// Model used (for cost calculation).
    pub model: Option<String>,
    /// Agent that used these tokens.
    pub agent_id: String,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
}

impl TokenUsage {
    /// Create a new `TokenUsage` record with input/output counts and the agent that consumed them.
    pub fn new(input: u64, output: u64, agent_id: impl Into<String>) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input.saturating_add(output),
            model: None,
            agent_id: agent_id.into(),
            timestamp: Utc::now(),
        }
    }

    /// Attach a model name to this usage record (builder pattern).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// Cost estimate for planning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostEstimate {
    /// Estimated input tokens.
    pub estimated_input_tokens: u64,
    /// Estimated output tokens.
    pub estimated_output_tokens: u64,
    /// Estimated cost in dollars.
    pub estimated_cost: f64,
    /// Confidence level (0.0-1.0).
    pub confidence: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_state_default() {
        let state = BudgetState::default();
        assert_eq!(state.total_cost, 0.0);
        assert_eq!(state.total_tokens, 0);
        assert!(!state.soft_alert_triggered);
        assert!(!state.hard_limit_hit);
    }

    #[test]
    fn token_usage() {
        let usage = TokenUsage::new(1000, 500, "agent-1");
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.total_tokens, 1500);
        assert!(usage.model.is_none());
    }

    #[test]
    fn token_usage_with_model() {
        let usage = TokenUsage::new(1000, 500, "agent-1").with_model("claude-opus");
        assert_eq!(usage.model, Some("claude-opus".into()));
    }

    #[test]
    fn cost_estimate_serialization() {
        let estimate = CostEstimate {
            estimated_input_tokens: 1000,
            estimated_output_tokens: 500,
            estimated_cost: 0.05,
            confidence: 0.8,
        };
        let json = serde_json::to_string(&estimate).unwrap();
        let parsed: CostEstimate = serde_json::from_str(&json).unwrap();
        assert_eq!(estimate, parsed);
    }
}
