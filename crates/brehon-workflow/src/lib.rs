//! Audited workflow engine for runtime events.
//!
//! Workflows are dry-run by default. Explicitly enabled workflows may request
//! runtime commands, but command execution still routes through daemon policy.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brehon_ports::{RuntimeEventSink, RuntimeEventStream};
use brehon_types::{
    DetectionSeverity, RuntimeCommand, RuntimeCommandKind, RuntimeCommandTarget, RuntimeEvent,
    RuntimeEventKind, RuntimeEventMeta, RuntimeOperation, RuntimePolicyContext, RuntimeSource,
    WorkflowActionEvent, WorkflowActionStatus,
};

/// Workflow execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowMode {
    /// Emit audit events only.
    DryRun,
}

impl Default for WorkflowMode {
    fn default() -> Self {
        Self::DryRun
    }
}

/// One proposed workflow action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRecommendation {
    pub operation: RuntimeOperation,
    pub reason: String,
    pub command_id: Option<String>,
    pub command_kind: Option<RuntimeCommandKind>,
}

/// One workflow audit event plus an optional command request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowEmission {
    pub audit_event: RuntimeEvent,
    pub command: Option<RuntimeCommand>,
    pub policy_context: RuntimePolicyContext,
}

/// Synchronous workflow rule over runtime events.
pub trait WorkflowRule: Send + Sync {
    fn id(&self) -> &'static str;
    fn observe(&self, event: &RuntimeEvent) -> Vec<WorkflowRecommendation>;
}

/// Dry-run workflow engine.
#[derive(Clone)]
pub struct WorkflowEngine {
    mode: WorkflowMode,
    rules: Vec<Arc<dyn WorkflowRule>>,
    enabled_workflows: BTreeSet<String>,
}

impl Default for WorkflowEngine {
    fn default() -> Self {
        Self {
            mode: WorkflowMode::DryRun,
            rules: vec![Arc::new(RateLimitQuarantineWorkflow)],
            enabled_workflows: BTreeSet::new(),
        }
    }
}

impl WorkflowEngine {
    pub fn new(mode: WorkflowMode, rules: Vec<Arc<dyn WorkflowRule>>) -> Self {
        Self {
            mode,
            rules,
            enabled_workflows: BTreeSet::new(),
        }
    }

    /// Return a copy that may request commands for the named workflow ids.
    pub fn with_enabled_workflows(
        mut self,
        workflow_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.enabled_workflows = workflow_ids.into_iter().map(Into::into).collect();
        self
    }

    /// Observe one event and return workflow audit events.
    pub fn observe(&self, event: RuntimeEvent) -> Vec<RuntimeEvent> {
        self.observe_emissions(event)
            .into_iter()
            .map(|emission| emission.audit_event)
            .collect()
    }

    /// Observe one event and return audit events plus explicit command requests.
    pub fn observe_emissions(&self, event: RuntimeEvent) -> Vec<WorkflowEmission> {
        self.rules
            .iter()
            .flat_map(|rule| {
                rule.observe(&event)
                    .into_iter()
                    .map(|recommendation| self.emission(&event, rule.id(), recommendation))
            })
            .collect()
    }

    fn emission(
        &self,
        observed: &RuntimeEvent,
        workflow_id: &str,
        recommendation: WorkflowRecommendation,
    ) -> WorkflowEmission {
        let requested =
            self.enabled_workflows.contains(workflow_id) && recommendation.command_kind.is_some();
        let command_id = if requested {
            Some(format!("workflow-command-{}", uuid::Uuid::new_v4()))
        } else {
            recommendation.command_id.clone()
        };
        let command = requested.then(|| RuntimeCommand {
            command_id: command_id
                .clone()
                .expect("requested workflow command has id"),
            target: RuntimeCommandTarget {
                session_id: observed.meta.session_id.clone(),
                pane_id: Some(observed.meta.pane_id.clone()),
                generation: Some(observed.meta.generation),
            },
            issued_at_ms: unix_timestamp_ms(),
            kind: recommendation
                .command_kind
                .clone()
                .expect("requested workflow command has kind"),
        });

        let mut meta = RuntimeEventMeta::new(
            observed.meta.session_id.clone(),
            observed.meta.pane_id.clone(),
            observed.meta.generation,
            RuntimeSource::Other {
                name: "workflow".to_string(),
            },
            unix_timestamp_ms(),
        );
        meta.correlation_id = observed.meta.correlation_id.clone();

        let audit_event = RuntimeEvent::new(
            meta,
            RuntimeEventKind::WorkflowAction(WorkflowActionEvent {
                workflow_id: workflow_id.to_string(),
                action_id: format!("workflow-action-{}", uuid::Uuid::new_v4()),
                operation: recommendation.operation,
                status: if requested {
                    WorkflowActionStatus::Requested
                } else {
                    match self.mode {
                        WorkflowMode::DryRun => WorkflowActionStatus::DryRun,
                    }
                },
                reason: recommendation.reason,
                command_id,
            }),
        );

        WorkflowEmission {
            audit_event,
            command,
            policy_context: RuntimePolicyContext::default(),
        }
    }
}

/// Recommends quarantine when detectors observe provider rate limiting.
#[derive(Debug, Default, Clone, Copy)]
pub struct RateLimitQuarantineWorkflow;

impl WorkflowRule for RateLimitQuarantineWorkflow {
    fn id(&self) -> &'static str {
        "rate_limit.quarantine_recommendation"
    }

    fn observe(&self, event: &RuntimeEvent) -> Vec<WorkflowRecommendation> {
        let RuntimeEventKind::DetectionEvent(detection) = &event.kind else {
            return Vec::new();
        };

        let looks_rate_limited = detection.rule_id.contains("rate_limit")
            || detection
                .message
                .to_ascii_lowercase()
                .contains("rate limit");
        let severity_is_actionable = matches!(
            detection.severity,
            DetectionSeverity::Warning | DetectionSeverity::Blocking
        );
        if !(looks_rate_limited && severity_is_actionable) {
            return Vec::new();
        }

        vec![WorkflowRecommendation {
            operation: RuntimeOperation::QuarantinePane,
            reason: format!(
                "rate-limit detection '{}' recommends operator review before more prompts",
                detection.rule_id
            ),
            command_id: None,
            command_kind: Some(RuntimeCommandKind::QuarantinePane {
                reason: format!("rate-limit detection '{}'", detection.rule_id),
            }),
        }]
    }
}

/// Workflow loop accounting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowLoopStats {
    pub observed_events: u64,
    pub emitted_actions: u64,
    pub routed_commands: u64,
    pub command_errors: u64,
    pub stream_errors: u64,
    pub publish_errors: u64,
}

/// Run workflows until the input stream closes.
pub async fn run_workflow_loop(
    stream: &mut dyn RuntimeEventStream,
    engine: &WorkflowEngine,
    sink: &dyn RuntimeEventSink,
) -> WorkflowLoopStats {
    let mut stats = WorkflowLoopStats::default();
    loop {
        let event = match stream.next_event().await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(err) => {
                stats.stream_errors = stats.stream_errors.saturating_add(1);
                tracing::warn!(error = %err, "Runtime workflow stream error");
                tokio::task::yield_now().await;
                continue;
            }
        };

        stats.observed_events = stats.observed_events.saturating_add(1);
        for action in engine.observe(event) {
            stats.emitted_actions = stats.emitted_actions.saturating_add(1);
            if let Err(err) = sink.publish(action).await {
                stats.publish_errors = stats.publish_errors.saturating_add(1);
                tracing::warn!(error = %err, "Failed to publish workflow action");
            }
        }
    }
    stats
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_ports::PortError;
    use brehon_types::{
        DetectionEvent, RuntimeEventKind, RuntimeEventMeta, RuntimeSource, RuntimeTextSpan,
    };
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    fn detection(rule_id: &str, severity: DetectionSeverity) -> RuntimeEvent {
        RuntimeEvent::new(
            RuntimeEventMeta::new("session", "pane", 1, RuntimeSource::Detector, 1),
            RuntimeEventKind::DetectionEvent(DetectionEvent {
                detection_id: "detection".to_string(),
                rule_id: rule_id.to_string(),
                severity,
                message: "rate limit reached".to_string(),
                span: Some(RuntimeTextSpan {
                    start_line: 1,
                    end_line: 1,
                }),
            }),
        )
    }

    #[test]
    fn rate_limit_detection_emits_dry_run_quarantine_recommendation() {
        let engine = WorkflowEngine::default();
        let actions = engine.observe(detection("rate_limit.warning", DetectionSeverity::Warning));

        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0].kind,
            RuntimeEventKind::WorkflowAction(ref action)
                if action.workflow_id == "rate_limit.quarantine_recommendation"
                    && action.operation == RuntimeOperation::QuarantinePane
                    && action.status == WorkflowActionStatus::DryRun
        ));
    }

    #[test]
    fn non_actionable_detection_is_ignored() {
        let engine = WorkflowEngine::default();
        let actions = engine.observe(detection(
            "agent.completion_marker",
            DetectionSeverity::Info,
        ));

        assert!(actions.is_empty());
    }

    #[test]
    fn explicitly_enabled_workflow_requests_command() {
        let engine = WorkflowEngine::default()
            .with_enabled_workflows(["rate_limit.quarantine_recommendation"]);
        let emissions =
            engine.observe_emissions(detection("rate_limit.warning", DetectionSeverity::Warning));

        assert_eq!(emissions.len(), 1);
        let action = match &emissions[0].audit_event.kind {
            RuntimeEventKind::WorkflowAction(action) => action,
            other => panic!("expected workflow action, got {other:?}"),
        };
        assert_eq!(action.status, WorkflowActionStatus::Requested);
        assert!(action.command_id.is_some());
        assert!(matches!(
            emissions[0].command.as_ref().map(|command| &command.kind),
            Some(RuntimeCommandKind::QuarantinePane { .. })
        ));
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<RuntimeEvent>>,
    }

    #[async_trait]
    impl RuntimeEventSink for RecordingSink {
        async fn publish(&self, event: RuntimeEvent) -> Result<(), PortError> {
            self.events.lock().await.push(event);
            Ok(())
        }
    }

    struct OneEventStream {
        event: Option<RuntimeEvent>,
    }

    #[async_trait]
    impl RuntimeEventStream for OneEventStream {
        async fn next_event(&mut self) -> Result<Option<RuntimeEvent>, PortError> {
            Ok(self.event.take())
        }
    }

    #[tokio::test]
    async fn workflow_loop_publishes_audit_events_and_exits_on_close() {
        let mut stream = OneEventStream {
            event: Some(detection("rate_limit.warning", DetectionSeverity::Warning)),
        };
        let sink = RecordingSink::default();
        let stats = run_workflow_loop(&mut stream, &WorkflowEngine::default(), &sink).await;

        assert_eq!(stats.observed_events, 1);
        assert_eq!(stats.emitted_actions, 1);
        assert_eq!(sink.events.lock().await.len(), 1);
    }
}
