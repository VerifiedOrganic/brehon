//! Main supervisor implementation.
//!
//! The supervisor monitors events, detects stuck agents, manages budget,
//! and invokes AI for decisions when needed.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, error, info, warn};

use brehon_ports::{AgentGateway, DecisionEngine, EventStore, NotificationSink, PortError};
use brehon_types::{AutonomyLevel, Event, EventId, EventKind, SystemState};

use crate::autonomy::AutonomyConfig;
use crate::budget_tracker::{BudgetPolicy, BudgetTracker};
use crate::escalation::{EscalationConfig, EscalationManager};
use crate::event_monitor::EventMonitor;
use crate::feedback::{
    detect_triggers, record_detected_triggers, FeedbackTriggerDetectorInput, TriggerDetectorPolicy,
};
use crate::heartbeat::{HeartbeatConfig, HeartbeatRunner, HeartbeatSummary};
use crate::nudge::{NudgeKind, NudgeSender};
use crate::stuck_detection::{StuckDetectionConfig, StuckDetector};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_EVENT_BATCH_SIZE: usize = 100;
const DEFAULT_NUDGE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub autonomy: AutonomyConfig,
    pub stuck_detection: StuckDetectionConfig,
    pub heartbeat: HeartbeatConfig,
    pub budget: BudgetPolicy,
    pub escalation: EscalationConfig,
    pub poll_interval: Duration,
    pub event_batch_size: usize,
    pub max_dead_agents: usize,
    pub max_completed_tasks: usize,
    pub nudge_timeout: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            autonomy: AutonomyConfig::default(),
            stuck_detection: StuckDetectionConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            budget: BudgetPolicy::default(),
            escalation: EscalationConfig::default(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            event_batch_size: DEFAULT_EVENT_BATCH_SIZE,
            max_dead_agents: 100,
            max_completed_tasks: 1000,
            nudge_timeout: DEFAULT_NUDGE_TIMEOUT,
        }
    }
}

impl SupervisorConfig {
    pub fn new(autonomy_level: AutonomyLevel) -> Self {
        Self {
            autonomy: AutonomyConfig::new(autonomy_level),
            ..Self::default()
        }
    }
}

pub struct SupervisorDependencies {
    pub event_store: Arc<dyn EventStore>,
    pub gateway: Arc<dyn AgentGateway>,
    pub decision_engine: Arc<dyn DecisionEngine>,
    pub notifications: Option<Arc<dyn NotificationSink>>,
}

pub struct Supervisor {
    config: SupervisorConfig,
    deps: SupervisorDependencies,
    state: SystemState,
    running: bool,
    last_processed_event: Option<EventId>,
    feedback_known_dedup_keys: HashSet<String>,
}

impl Supervisor {
    pub fn new(config: SupervisorConfig, deps: SupervisorDependencies) -> Self {
        Self {
            config,
            deps,
            state: SystemState::default(),
            running: false,
            last_processed_event: None,
            feedback_known_dedup_keys: HashSet::new(),
        }
    }

    pub async fn run(&mut self, shutdown: Arc<AtomicBool>) -> Result<(), PortError> {
        info!("Starting supervisor");
        self.running = true;
        self.state = SystemState::Running;

        let event_monitor = Arc::new(EventMonitor::new(self.deps.event_store.clone()));
        let budget_tracker = Arc::new(BudgetTracker::new(self.config.budget.clone()));
        let escalation_manager = Arc::new(EscalationManager::new(self.config.escalation.clone()));
        let nudge_sender = Arc::new(parking_lot::RwLock::new(NudgeSender::new(
            self.deps.gateway.clone(),
        )));
        let stuck_detector = StuckDetector::new(self.config.stuck_detection.clone());
        let mut heartbeat_runner = HeartbeatRunner::new(
            self.config.heartbeat.clone(),
            self.deps.decision_engine.clone(),
        );
        self.hydrate_feedback_dedup_keys().await?;

        while self.running && !shutdown.load(Ordering::SeqCst) {
            match self
                .tick(
                    &event_monitor,
                    &budget_tracker,
                    &escalation_manager,
                    &nudge_sender,
                    &stuck_detector,
                    &mut heartbeat_runner,
                )
                .await
            {
                Ok(should_continue) => {
                    if !should_continue {
                        break;
                    }
                }
                Err(e) => {
                    error!(error = ?e, "Error in supervisor tick");
                    if self.state == SystemState::Draining {
                        break;
                    }
                }
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }

        info!("Supervisor stopped");
        Ok(())
    }

    async fn tick(
        &mut self,
        event_monitor: &Arc<EventMonitor>,
        budget_tracker: &Arc<BudgetTracker>,
        _escalation_manager: &Arc<EscalationManager>,
        nudge_sender: &Arc<parking_lot::RwLock<NudgeSender>>,
        stuck_detector: &StuckDetector,
        heartbeat_runner: &mut HeartbeatRunner,
    ) -> Result<bool, PortError> {
        if self.state == SystemState::Draining {
            debug!("Supervisor in draining state, checking for completion");
            let state = event_monitor.state();
            let active_count = state.agents_in_operation().len();
            if active_count == 0 {
                info!("All agents finished, shutting down");
                self.running = false;
                return Ok(false);
            }
            return Ok(true);
        }

        let events = event_monitor
            .poll_events(self.config.event_batch_size)
            .await?;

        for (event, event_id) in &events {
            if let Err(e) = self
                .process_event(event_monitor, budget_tracker, event, *event_id)
                .await
            {
                warn!(error = ?e, "Failed to process event");
            }
        }

        if !events.is_empty() {
            self.last_processed_event = event_monitor.last_event_id();
        }

        self.detect_feedback_triggers(&events).await?;

        self.check_nudge_timeouts(event_monitor).await?;

        self.check_stuck_agents(event_monitor, stuck_detector, nudge_sender)
            .await?;

        if self.config.autonomy.should_invoke_ai_for_heartbeat() && heartbeat_runner.should_run() {
            self.run_heartbeat(event_monitor, heartbeat_runner).await?;
        }

        if budget_tracker.is_over_budget() {
            self.handle_budget_exceeded(budget_tracker, event_monitor)
                .await?;
        }

        event_monitor.apply_bounds(self.config.max_dead_agents, self.config.max_completed_tasks);

        Ok(true)
    }

    async fn process_event(
        &mut self,
        event_monitor: &Arc<EventMonitor>,
        budget_tracker: &Arc<BudgetTracker>,
        event: &Event,
        event_id: EventId,
    ) -> Result<(), PortError> {
        let events_to_emit = event_monitor.process_event(event, event_id);

        budget_tracker.process_event(event);

        for emit_event in events_to_emit {
            self.deps.event_store.append(emit_event).await?;
        }

        if let EventKind::SystemDraining { reason } = &event.kind {
            warn!(reason = reason, "System entering drain mode");
            self.state = SystemState::Draining;
        }
        if let EventKind::FeedbackTriggerDetected { dedup_key, .. } = &event.kind {
            self.feedback_known_dedup_keys.insert(dedup_key.clone());
        }

        Ok(())
    }

    async fn hydrate_feedback_dedup_keys(&mut self) -> Result<(), PortError> {
        let mut since = None;
        loop {
            let events = self.deps.event_store.stream(since, 1_000).await?;
            if events.is_empty() {
                break;
            }
            for (event, event_id) in &events {
                if let EventKind::FeedbackTriggerDetected { dedup_key, .. } = &event.kind {
                    self.feedback_known_dedup_keys.insert(dedup_key.clone());
                }
                since = Some(*event_id);
            }
        }
        Ok(())
    }

    async fn detect_feedback_triggers(
        &mut self,
        events: &[(Event, EventId)],
    ) -> Result<(), PortError> {
        if events.is_empty() {
            return Ok(());
        }
        let ordered: Vec<(EventId, Event)> = events
            .iter()
            .map(|(event, event_id)| (*event_id, event.clone()))
            .collect();
        let policy = TriggerDetectorPolicy::default();
        let input = FeedbackTriggerDetectorInput {
            events: &ordered,
            open_followups: &[],
            stuck_runs: &[],
            pending_permissions: &[],
            timed_out_nudges: &[],
            known_dedup_keys: &self.feedback_known_dedup_keys,
            policy: &policy,
        };
        let triggers = detect_triggers(&input);
        if triggers.is_empty() {
            return Ok(());
        }
        record_detected_triggers(self.deps.event_store.as_ref(), &triggers).await?;
        for trigger in triggers {
            self.feedback_known_dedup_keys.insert(trigger.dedup_key());
        }
        Ok(())
    }

    async fn check_stuck_agents(
        &mut self,
        event_monitor: &Arc<EventMonitor>,
        stuck_detector: &StuckDetector,
        nudge_sender: &Arc<parking_lot::RwLock<NudgeSender>>,
    ) -> Result<(), PortError> {
        let state = event_monitor.state();
        let stuck_agents = stuck_detector.detect_stuck(&state.active_agents());

        for stuck in stuck_agents {
            debug!(
                session_id = %stuck.session_id,
                idle_minutes = stuck.idle_minutes,
                pattern = ?stuck.pattern,
                "Detected stuck agent"
            );

            if self.config.autonomy.should_invoke_ai_for_stuck() && !stuck.is_in_operation {
                let kind = if stuck.idle_minutes > 30 {
                    NudgeKind::Guidance
                } else {
                    NudgeKind::Soft
                };

                let session_id = brehon_types::SessionId::new(&stuck.session_id);
                let (gateway, prepared) = {
                    let sender = nudge_sender.read();
                    (
                        sender.gateway(),
                        sender.prepare_nudge_with_pattern(
                            &session_id,
                            kind,
                            stuck.pattern.as_deref(),
                        ),
                    )
                };
                let send_result = gateway
                    .send_prompt(&session_id, prepared.prompt.clone())
                    .await;
                {
                    let mut sender = nudge_sender.write();
                    sender.record_prepared_nudge(&prepared, send_result.is_ok());
                }
                send_result?;
                let content = prepared.content.clone();
                let event = Event {
                    kind: EventKind::NudgeSent {
                        session_id: stuck.session_id.clone(),
                        kind: kind.as_str().to_string(),
                        content,
                    },
                    timestamp: chrono::Utc::now(),
                    aggregate_id: stuck.session_id.clone(),
                };
                let event_id = self.deps.event_store.append(event.clone()).await?;
                event_monitor.process_event(&event, event_id);
                self.last_processed_event = Some(event_id);
            }
        }

        Ok(())
    }

    async fn run_heartbeat(
        &mut self,
        event_monitor: &Arc<EventMonitor>,
        heartbeat_runner: &mut HeartbeatRunner,
    ) -> Result<(), PortError> {
        let state = event_monitor.state();

        let summary = HeartbeatSummary {
            active_agents: state.active_agents().len(),
            active_tasks: state
                .tasks
                .values()
                .filter(|t| t.status == brehon_types::TaskStatus::InProgress)
                .count(),
            pending_tasks: state
                .tasks
                .values()
                .filter(|t| t.status == brehon_types::TaskStatus::Pending)
                .count(),
            stuck_agents: state.stuck_candidates(100).len(),
            recent_events: 0,
            budget_used_percent: 0.0,
            system_state: "running".to_string(),
        };

        let response = heartbeat_runner.run_heartbeat(summary).await?;

        debug!(
            decision = %response.decision,
            confidence = ?response.confidence,
            "Heartbeat decision"
        );

        Ok(())
    }

    async fn handle_budget_exceeded(
        &mut self,
        budget_tracker: &Arc<BudgetTracker>,
        _event_monitor: &Arc<EventMonitor>,
    ) -> Result<(), PortError> {
        warn!(
            total_tokens = budget_tracker.total_tokens(),
            "Budget exceeded, entering drain mode"
        );

        self.state = SystemState::Draining;

        let event = Event {
            kind: EventKind::SystemDraining {
                reason: "Budget exceeded".to_string(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "system".to_string(),
        };

        self.deps.event_store.append(event).await?;

        Ok(())
    }

    async fn check_nudge_timeouts(
        &mut self,
        event_monitor: &Arc<EventMonitor>,
    ) -> Result<(), PortError> {
        let timeout_events = event_monitor
            .state()
            .nudge_timeout_events(chrono::Utc::now(), self.config.nudge_timeout);
        for event in timeout_events {
            let event_id = self.deps.event_store.append(event.clone()).await?;
            event_monitor.process_event(&event, event_id);
            self.last_processed_event = Some(event_id);
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        info!("Stopping supervisor");
        self.running = false;
    }

    pub fn state(&self) -> SystemState {
        self.state
    }

    pub fn last_processed_event(&self) -> Option<EventId> {
        self.last_processed_event
    }

    pub fn is_running(&self) -> bool {
        self.running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::{InMemoryEventStore, MockDecisionEngine, MockGateway};
    use brehon_types::SessionSpec;

    #[tokio::test]
    async fn supervisor_config_defaults() {
        let config = SupervisorConfig::default();
        assert_eq!(config.autonomy.level, AutonomyLevel::Guided);
        assert_eq!(config.stuck_detection.time_threshold_minutes, 10);
        assert_eq!(config.poll_interval, DEFAULT_POLL_INTERVAL);
        assert_eq!(config.nudge_timeout, DEFAULT_NUDGE_TIMEOUT);
    }

    #[tokio::test]
    async fn supervisor_initial_state() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let decision = Arc::new(MockDecisionEngine::new());

        let supervisor = Supervisor::new(
            SupervisorConfig::default(),
            SupervisorDependencies {
                event_store: store,
                gateway,
                decision_engine: decision,
                notifications: None,
            },
        );

        assert_eq!(supervisor.state(), SystemState::Running);
        assert!(!supervisor.is_running());
    }

    #[tokio::test]
    async fn supervisor_processes_events() {
        let store = Arc::new(InMemoryEventStore::new());
        let _gateway = Arc::new(MockGateway::new());
        let _decision = Arc::new(MockDecisionEngine::new());

        let event = Event {
            kind: EventKind::AgentSpawned {
                agent_id: "agent-1".into(),
                session_id: "session-1".into(),
                role: "worker".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "agent-1".into(),
        };
        store.append(event).await.unwrap();

        let monitor = EventMonitor::new(store.clone());
        let events = monitor.poll_events(10).await.unwrap();
        assert_eq!(events.len(), 1);

        let (event, event_id) = &events[0];
        monitor.process_event(event, *event_id);

        let state = monitor.state();
        assert_eq!(state.agents.len(), 1);
        assert!(state.agents.contains_key("session-1"));
    }

    #[tokio::test]
    async fn supervisor_persists_feedback_triggers_from_event_stream() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let decision = Arc::new(MockDecisionEngine::new());
        let source = Event {
            kind: EventKind::ReviewChangesRequested {
                review_id: "REV-1".into(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "REV-1".into(),
        };
        let source_id = store.append(source.clone()).await.unwrap();
        let mut supervisor = Supervisor::new(
            SupervisorConfig::default(),
            SupervisorDependencies {
                event_store: store.clone(),
                gateway,
                decision_engine: decision,
                notifications: None,
            },
        );

        supervisor
            .detect_feedback_triggers(&[(source, source_id)])
            .await
            .unwrap();
        let events = store.stream(None, 10).await.unwrap();
        assert!(events.iter().any(|(event, _)| matches!(
            event.kind,
            EventKind::FeedbackTriggerDetected {
                ref source_event_ids,
                ref covered_event_range,
                ..
            } if source_event_ids == &vec![source_id]
                && covered_event_range == &Some((source_id, source_id))
        )));
    }

    #[tokio::test]
    async fn stuck_detection_with_operation_awareness() {
        let config = StuckDetectionConfig {
            time_threshold_minutes: 5,
            operation_aware: true,
            ..Default::default()
        };
        let detector = StuckDetector::new(config);

        let mut agent = crate::event_monitor::AgentState::new(
            "agent-1".to_string(),
            "session-1".to_string(),
            "worker".to_string(),
        );
        agent.current_operation = Some("cargo test".to_string());
        agent.last_event_at = chrono::Utc::now() - chrono::Duration::minutes(10);

        let stuck = detector.detect_stuck(&[&agent]);
        assert!(stuck.is_empty(), "Agent in operation should not be stuck");
    }

    #[tokio::test]
    async fn budget_tracking_from_events() {
        let _store = Arc::new(InMemoryEventStore::new());
        let policy = BudgetPolicy::new(10000);
        let tracker = BudgetTracker::new(policy);

        let event = Event {
            kind: EventKind::ResponseReceived {
                session_id: "session-1".into(),
                prompt_id: "prompt-1".into(),
                tokens_used: 1000,
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "session-1".into(),
        };

        tracker.process_event(&event);
        assert_eq!(tracker.total_tokens(), 1000);
    }

    #[tokio::test]
    async fn escalation_after_retries() {
        let config = EscalationConfig::new(2);
        let manager = EscalationManager::new(config);

        manager.record_retry("decision-1");
        assert!(!manager.should_escalate("decision-1"));

        manager.record_retry("decision-1");
        assert!(manager.should_escalate("decision-1"));
    }

    #[tokio::test]
    async fn autonomy_level_controls_ai_invocation() {
        let full_config = AutonomyConfig::new(AutonomyLevel::Full);
        assert!(full_config.should_invoke_ai_for_planning());
        assert!(full_config.should_invoke_ai_for_heartbeat());

        let minimal_config = AutonomyConfig::new(AutonomyLevel::Minimal);
        assert!(!minimal_config.should_invoke_ai_for_planning());
        assert!(!minimal_config.should_invoke_ai_for_heartbeat());
        assert!(minimal_config.should_invoke_ai_for_stuck());
    }

    #[tokio::test]
    async fn nudge_sent_via_gateway() {
        let gateway = Arc::new(MockGateway::new());
        let session_id = gateway
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();

        let mut sender = NudgeSender::new(gateway.clone());
        sender
            .send_nudge(&session_id, NudgeKind::Soft, None)
            .await
            .unwrap();

        let history = sender.history();
        assert_eq!(history.len(), 1);
        assert!(history[0].success);
    }

    #[tokio::test]
    async fn check_stuck_agents_records_nudge_sent_event() {
        let store = Arc::new(InMemoryEventStore::new());
        let gateway = Arc::new(MockGateway::new());
        let session_id = gateway
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();
        let decision = Arc::new(MockDecisionEngine::new());
        let mut supervisor = Supervisor::new(
            SupervisorConfig {
                stuck_detection: StuckDetectionConfig {
                    time_threshold_minutes: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            SupervisorDependencies {
                event_store: store.clone(),
                gateway: gateway.clone(),
                decision_engine: decision,
                notifications: None,
            },
        );
        let event_monitor = Arc::new(EventMonitor::new(store.clone()));
        event_monitor.process_event(
            &Event {
                kind: EventKind::AgentSpawned {
                    agent_id: "agent-1".into(),
                    session_id: session_id.as_str().to_string(),
                    role: "worker".into(),
                },
                timestamp: chrono::Utc::now(),
                aggregate_id: "agent-1".into(),
            },
            EventId::new(1),
        );
        let nudge_sender = Arc::new(parking_lot::RwLock::new(NudgeSender::new(gateway)));
        let stuck_detector = StuckDetector::new(supervisor.config.stuck_detection.clone());

        supervisor
            .check_stuck_agents(&event_monitor, &stuck_detector, &nudge_sender)
            .await
            .unwrap();

        let events = store.all_events();
        assert!(events.iter().any(|event| {
            matches!(
                &event.kind,
                EventKind::NudgeSent {
                    session_id: sid,
                    kind,
                    content,
                } if sid == session_id.as_str() && kind == "soft" && !content.is_empty()
            )
        }));
        assert_eq!(
            event_monitor
                .state()
                .nudge_state_for_session(session_id.as_str()),
            Some(brehon_types::NudgeDeliveryState::Delivered)
        );
    }
}
