//! Mock DecisionEngine implementation for testing.
//!
//! Deterministic responses for supervisor judgment tests.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;

use brehon_ports::{DecisionEngine, PortError};
use brehon_types::{DecisionConfidence, DecisionKind, DecisionRequest, DecisionResponse};

/// Scripted decision for mock engine.
#[derive(Debug, Clone)]
pub struct ScriptedDecision {
    pub decision: String,
    pub reasoning: String,
    pub confidence: DecisionConfidence,
    pub next_actions: Vec<String>,
}

impl Default for ScriptedDecision {
    fn default() -> Self {
        Self {
            decision: "proceed".into(),
            reasoning: "default decision".into(),
            confidence: DecisionConfidence::Medium,
            next_actions: vec![],
        }
    }
}

/// Mock decision engine for testing.
///
/// Can be configured with scripted responses for different decision types.
#[derive(Debug, Clone)]
pub struct MockDecisionEngine {
    inner: Arc<RwLock<DecisionEngineInner>>,
}

#[derive(Debug)]
struct DecisionEngineInner {
    responses: HashMapWithDefault<DecisionKind, Vec<ScriptedDecision>>,
    response_indices: std::collections::HashMap<DecisionKind, usize>,
    all_calls: Vec<DecisionRequest>,
}

#[derive(Debug, Clone)]
struct HashMapWithDefault<K, V>(std::collections::HashMap<K, V>);

impl<K: std::hash::Hash + Eq, V: Clone> HashMapWithDefault<K, V> {
    fn new() -> Self {
        Self(std::collections::HashMap::new())
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.0.get(key)
    }

    fn insert(&mut self, key: K, value: V) {
        self.0.insert(key, value);
    }
}

impl Default for DecisionEngineInner {
    fn default() -> Self {
        Self {
            responses: HashMapWithDefault::new(),
            response_indices: std::collections::HashMap::new(),
            all_calls: Vec::new(),
        }
    }
}

impl MockDecisionEngine {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(DecisionEngineInner::default())),
        }
    }

    pub fn with_responses(kind: DecisionKind, responses: Vec<ScriptedDecision>) -> Self {
        let engine = Self::new();
        engine.set_responses(kind, responses);
        engine
    }

    pub fn set_responses(&self, kind: DecisionKind, responses: Vec<ScriptedDecision>) {
        self.inner.write().responses.insert(kind, responses);
    }

    pub fn default_response() -> DecisionResponse {
        DecisionResponse {
            request_id: "default".into(),
            decision: "proceed".into(),
            reasoning: "default reasoning".into(),
            confidence: DecisionConfidence::Medium,
            next_actions: vec![],
            tokens_used: 100,
            decided_at: Utc::now(),
        }
    }

    pub fn calls(&self) -> Vec<DecisionRequest> {
        self.inner.read().all_calls.clone()
    }

    pub fn call_count(&self) -> usize {
        self.inner.read().all_calls.len()
    }

    pub fn clear_calls(&self) {
        self.inner.write().all_calls.clear();
    }
}

impl Default for MockDecisionEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DecisionEngine for MockDecisionEngine {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, PortError> {
        let mut inner = self.inner.write();

        inner.all_calls.push(request.clone());

        let kind = request.kind;

        let responses = inner.responses.get(&kind).cloned();

        if let Some(ref responses) = responses {
            if responses.is_empty() {
                return Ok(Self::default_response());
            }

            let idx = inner.response_indices.entry(kind).or_insert(0);
            let response = &responses[*idx];
            *idx = (*idx + 1) % responses.len();

            Ok(DecisionResponse {
                request_id: request.request_id,
                decision: response.decision.clone(),
                reasoning: response.reasoning.clone(),
                confidence: response.confidence,
                next_actions: response.next_actions.clone(),
                tokens_used: 100,
                decided_at: Utc::now(),
            })
        } else {
            Ok(Self::default_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_response() {
        let engine = MockDecisionEngine::new();

        let response = engine
            .decide(DecisionRequest {
                request_id: "req-1".into(),
                kind: DecisionKind::AssignWorker,
                context: "test".into(),
                event_ids: vec![],
                options: vec!["agent-1".into()],
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        assert_eq!(response.decision, "proceed");
    }

    #[tokio::test]
    async fn scripted_response() {
        let engine = MockDecisionEngine::with_responses(
            DecisionKind::AssignWorker,
            vec![ScriptedDecision {
                decision: "agent-1".into(),
                reasoning: "best fit".into(),
                confidence: DecisionConfidence::High,
                next_actions: vec!["assign task".into()],
            }],
        );

        let response = engine
            .decide(DecisionRequest {
                request_id: "req-1".into(),
                kind: DecisionKind::AssignWorker,
                context: "test".into(),
                event_ids: vec![],
                options: vec!["agent-1".into(), "agent-2".into()],
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        assert_eq!(response.decision, "agent-1");
        assert_eq!(response.confidence, DecisionConfidence::High);
    }

    #[tokio::test]
    async fn multiple_responses_cycle() {
        let engine = MockDecisionEngine::with_responses(
            DecisionKind::StuckGuidance,
            vec![
                ScriptedDecision {
                    decision: "nudge".into(),
                    reasoning: "try nudge".into(),
                    confidence: DecisionConfidence::Medium,
                    next_actions: vec![],
                },
                ScriptedDecision {
                    decision: "escalate".into(),
                    reasoning: "escalate to human".into(),
                    confidence: DecisionConfidence::Low,
                    next_actions: vec![],
                },
            ],
        );

        let r1 = engine
            .decide(DecisionRequest {
                request_id: "r1".into(),
                kind: DecisionKind::StuckGuidance,
                context: "".into(),
                event_ids: vec![],
                options: vec![],
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        let r2 = engine
            .decide(DecisionRequest {
                request_id: "r2".into(),
                kind: DecisionKind::StuckGuidance,
                context: "".into(),
                event_ids: vec![],
                options: vec![],
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        assert_eq!(r1.decision, "nudge");
        assert_eq!(r2.decision, "escalate");

        let r3 = engine
            .decide(DecisionRequest {
                request_id: "r3".into(),
                kind: DecisionKind::StuckGuidance,
                context: "".into(),
                event_ids: vec![],
                options: vec![],
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        assert_eq!(r3.decision, "nudge");
    }

    #[tokio::test]
    async fn call_tracking() {
        let engine = MockDecisionEngine::new();

        engine
            .decide(DecisionRequest {
                request_id: "r1".into(),
                kind: DecisionKind::PlanExecution,
                context: "".into(),
                event_ids: vec![],
                options: vec![],
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        engine
            .decide(DecisionRequest {
                request_id: "r2".into(),
                kind: DecisionKind::HeartbeatCheck,
                context: "".into(),
                event_ids: vec![],
                options: vec![],
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        assert_eq!(engine.call_count(), 2);

        let calls = engine.calls();
        assert_eq!(calls[0].request_id, "r1");
        assert_eq!(calls[1].request_id, "r2");
    }
}
