//! DecisionEngine implementation using ACP prompts.
//!
//! Uses AI prompts to make decisions for the supervisor.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tracing::debug;

use brehon_ports::{DecisionEngine, PortError};
use brehon_types::{DecisionConfidence, DecisionKind, DecisionRequest, DecisionResponse};

use super::session::AcpSession;

/// Decision engine that delegates decisions to an ACP agent session via prompts.
pub struct AcpDecisionEngine {
    session: Arc<AcpSession>,
    default_timeout: Duration,
}

impl AcpDecisionEngine {
    /// Creates a new decision engine backed by the given ACP session.
    pub fn new(session: Arc<AcpSession>) -> Self {
        Self {
            session,
            default_timeout: Duration::from_secs(60),
        }
    }

    /// Sets a custom default timeout for decision requests.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Sends a decision request to the ACP agent and waits up to `timeout` for a response.
    pub async fn decide_with_timeout(
        &self,
        request: DecisionRequest,
        timeout: Duration,
    ) -> Result<DecisionResponse, DecisionError> {
        debug!(request_id = %request.request_id, kind = ?request.kind, "Processing decision request");

        let prompt = self.build_prompt(&request);

        let prompt_id = brehon_types::PromptId::new(format!("decision-{}", request.request_id));

        let turn = brehon_types::PromptTurn {
            prompt_id: prompt_id.clone(),
            content: prompt,
            kind: brehon_types::MessageKind::System,
            sent_at: Utc::now(),
        };

        let _handle = self
            .session
            .send_prompt(turn)
            .await
            .map_err(|e| DecisionError::PromptFailed(e.to_string()))?;

        let result = self
            .session
            .wait_for_response(&prompt_id, timeout.as_millis() as u64)
            .await
            .map_err(|e| DecisionError::Timeout(e.to_string()))?;

        let response = match result.response {
            Some(content) => content,
            None => return Err(DecisionError::EmptyResponse),
        };

        let tokens_used = result.tokens_used.unwrap_or(0);

        let decision = self.parse_decision_response(&request, &response, tokens_used)?;

        debug!(request_id = %request.request_id, decision = %decision.decision, "Decision made");
        Ok(decision)
    }

    fn build_prompt(&self, request: &DecisionRequest) -> String {
        let kind_str = match request.kind {
            DecisionKind::PlanExecution => "Plan the execution order",
            DecisionKind::AssignWorker => "Choose the best worker for this task",
            DecisionKind::StuckGuidance => "Provide guidance to help this stuck worker",
            DecisionKind::ReviewDeadlock => "Resolve this review deadlock",
            DecisionKind::MergeConflict => "Help resolve this merge conflict",
            DecisionKind::HeartbeatCheck => "Perform a sanity check on the system",
            DecisionKind::NextAction => "Choose the next action",
            DecisionKind::ErrorHandler => "Handle this error situation",
        };

        let mut prompt = format!(
            "You are the Supervisor AI. Make a decision for: {}\n\n",
            kind_str
        );

        prompt.push_str(&format!("Request ID: {}\n", request.request_id));
        prompt.push_str(&format!("Decision Kind: {:?}\n\n", request.kind));
        prompt.push_str(&format!("Context:\n{}\n\n", request.context));

        if !request.options.is_empty() {
            prompt.push_str("Available Options:\n");
            for (i, opt) in request.options.iter().enumerate() {
                prompt.push_str(&format!("{}. {}\n", i + 1, opt));
            }
            prompt.push('\n');
        }

        prompt.push_str("Event Context:\n");
        for event_id in &request.event_ids {
            prompt.push_str(&format!("- {}\n", event_id));
        }
        prompt.push('\n');

        prompt.push_str("Please provide your decision in the following format:\n");
        prompt.push_str("DECISION: <your decision>\n");
        prompt.push_str("REASONING: <your reasoning>\n");
        prompt.push_str("CONFIDENCE: <low|medium|high>\n");
        prompt.push_str("NEXT_ACTIONS:\n");
        prompt.push_str("- <action 1>\n");
        prompt.push_str("- <action 2>\n");

        prompt
    }

    fn parse_decision_response(
        &self,
        request: &DecisionRequest,
        response: &str,
        tokens_used: u64,
    ) -> Result<DecisionResponse, DecisionError> {
        let decision = self
            .extract_field(response, "DECISION")
            .unwrap_or_else(|_| "unknown".to_string());

        let reasoning = self
            .extract_field(response, "REASONING")
            .unwrap_or_else(|_| String::new());

        let confidence = self
            .extract_field(response, "CONFIDENCE")
            .map(|s| match s.to_lowercase().as_str() {
                "high" => DecisionConfidence::High,
                "medium" => DecisionConfidence::Medium,
                "low" => DecisionConfidence::Low,
                _ => DecisionConfidence::Medium,
            })
            .unwrap_or(DecisionConfidence::Medium);

        let next_actions = self
            .extract_list(response, "NEXT_ACTIONS")
            .unwrap_or_else(|_| Vec::new());

        Ok(DecisionResponse {
            request_id: request.request_id.clone(),
            decision,
            reasoning,
            confidence,
            next_actions,
            tokens_used,
            decided_at: Utc::now(),
        })
    }

    fn extract_field(&self, response: &str, field: &str) -> Result<String, ()> {
        for line in response.lines() {
            if line.starts_with(&format!("{}:", field)) {
                return Ok(line
                    .trim_start_matches(&format!("{}:", field))
                    .trim()
                    .to_string());
            }
        }
        Err(())
    }

    fn extract_list(&self, response: &str, field: &str) -> Result<Vec<String>, ()> {
        let mut in_list = false;
        let mut items = Vec::new();

        for line in response.lines() {
            if line.starts_with(&format!("{}:", field)) {
                in_list = true;
                continue;
            }

            if in_list {
                if line.starts_with("- ") {
                    items.push(line.trim_start_matches("- ").trim().to_string());
                } else if !line.starts_with(|c: char| c.is_whitespace()) && !line.is_empty() {
                    break;
                }
            }
        }

        if items.is_empty() {
            Err(())
        } else {
            Ok(items)
        }
    }
}

#[async_trait]
impl DecisionEngine for AcpDecisionEngine {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, PortError> {
        self.decide_with_timeout(request, self.default_timeout)
            .await
            .map_err(|e| PortError::Agent(e.to_string()))
    }
}

/// Errors that can occur during decision processing.
#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    #[error("Prompt failed: {0}")]
    PromptFailed(String),
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Empty response")]
    EmptyResponse,
    #[error("Parse error: {0}")]
    ParseError(String),
}

impl From<DecisionError> for PortError {
    fn from(err: DecisionError) -> Self {
        PortError::Agent(err.to_string())
    }
}
