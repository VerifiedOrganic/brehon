//! Event monitoring for the supervisor.
//!
//! Tails EventStore for all events and maintains in-memory state.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use tracing::debug;

use brehon_ports::{EventStore, PortError};
use brehon_types::{Event, EventId, EventKind, NudgeDeliveryState, TaskStatus};

#[derive(Debug, Clone)]
pub struct ActiveNudge {
    pub nudge_id: String,
    pub nudge_kind: String,
    pub delivery_state: NudgeDeliveryState,
    pub sent_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub acted_on_at: Option<DateTime<Utc>>,
    pub timed_out_at: Option<DateTime<Utc>>,
}

impl ActiveNudge {
    pub fn new(nudge_kind: String, sent_at: DateTime<Utc>, session_id: &str) -> Self {
        let nudge_id = format!("{}-{}-{}", session_id, nudge_kind, sent_at.timestamp());
        Self {
            nudge_id,
            nudge_kind,
            delivery_state: NudgeDeliveryState::Delivered,
            sent_at,
            acknowledged_at: None,
            acted_on_at: None,
            timed_out_at: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentState {
    pub agent_id: String,
    pub session_id: String,
    pub role: String,
    pub is_alive: bool,
    pub current_task_id: Option<String>,
    pub last_event_at: DateTime<Utc>,
    pub current_operation: Option<String>,
    pub last_message_content: Option<String>,
    pub message_history: Vec<(DateTime<Utc>, String)>,
    pub active_nudge: Option<ActiveNudge>,
}

impl AgentState {
    pub fn new(agent_id: String, session_id: String, role: String) -> Self {
        Self {
            agent_id,
            session_id,
            role,
            is_alive: true,
            current_task_id: None,
            last_event_at: Utc::now(),
            current_operation: None,
            last_message_content: None,
            message_history: Vec::new(),
            active_nudge: None,
        }
    }

    pub fn is_in_operation(&self) -> bool {
        self.current_operation.is_some()
    }

    pub fn idle_duration(&self, now: DateTime<Utc>) -> Duration {
        let elapsed = now - self.last_event_at;
        elapsed.to_std().unwrap_or(Duration::ZERO)
    }

    pub fn record_message(&mut self, content: String) {
        self.last_message_content = Some(content.clone());
        self.message_history.push((Utc::now(), content));

        if self.message_history.len() > 100 {
            let remove_count = self.message_history.len() - 100;
            self.message_history.drain(0..remove_count);
        }
    }

    pub fn recent_messages(&self, within_minutes: u32) -> Vec<&str> {
        let cutoff = Utc::now() - chrono::Duration::minutes(i64::from(within_minutes));
        self.message_history
            .iter()
            .filter(|(ts, _)| ts >= &cutoff)
            .map(|(_, content)| content.as_str())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct TaskState {
    pub task_id: String,
    pub status: TaskStatus,
    pub assignee: Option<String>,
    pub session_id: Option<String>,
    pub last_event_at: DateTime<Utc>,
}

impl TaskState {
    pub fn new(task_id: String) -> Self {
        Self {
            task_id,
            status: TaskStatus::Pending,
            assignee: None,
            session_id: None,
            last_event_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SupervisorState {
    pub agents: HashMap<String, AgentState>,
    pub tasks: HashMap<String, TaskState>,
    pub last_event_id: Option<EventId>,
    pub system_state: brehon_types::SystemState,
}

impl SupervisorState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_agents(&self) -> Vec<&AgentState> {
        self.agents.values().filter(|a| a.is_alive).collect()
    }

    pub fn agents_in_operation(&self) -> Vec<&AgentState> {
        self.agents
            .values()
            .filter(|a| a.is_alive && a.is_in_operation())
            .collect()
    }

    pub fn agents_by_role(&self, role: &str) -> Vec<&AgentState> {
        self.agents
            .values()
            .filter(|a| a.is_alive && a.role == role)
            .collect()
    }

    pub fn stuck_candidates(&self, threshold_minutes: u64) -> Vec<&AgentState> {
        let now = Utc::now();
        self.agents
            .values()
            .filter(|a| {
                a.is_alive
                    && !a.is_in_operation()
                    && a.idle_duration(now) > Duration::from_secs(threshold_minutes * 60)
            })
            .collect()
    }

    pub fn nudge_state_for_session(&self, session_id: &str) -> Option<NudgeDeliveryState> {
        self.agents
            .get(session_id)
            .and_then(|agent| agent.active_nudge.as_ref().map(|n| n.delivery_state))
    }

    pub fn active_nudge_for_session(&self, session_id: &str) -> Option<&ActiveNudge> {
        self.agents
            .get(session_id)
            .and_then(|agent| agent.active_nudge.as_ref())
    }

    pub fn nudge_timeout_events(&self, now: DateTime<Utc>, timeout: Duration) -> Vec<Event> {
        self.agents
            .iter()
            .filter_map(|(session_id, agent)| {
                let nudge = agent.active_nudge.as_ref()?;
                if !matches!(
                    nudge.delivery_state,
                    NudgeDeliveryState::Delivered | NudgeDeliveryState::Acknowledged
                ) {
                    return None;
                }
                let elapsed = (now - nudge.sent_at).to_std().unwrap_or(Duration::ZERO);
                if elapsed < timeout {
                    return None;
                }
                Some(Event {
                    kind: EventKind::NudgeTimedOut {
                        session_id: session_id.clone(),
                        nudge_id: nudge.nudge_id.clone(),
                        nudge_kind: nudge.nudge_kind.clone(),
                        elapsed_secs: elapsed.as_secs(),
                    },
                    timestamp: now,
                    aggregate_id: session_id.clone(),
                })
            })
            .collect()
    }
}

pub struct EventMonitor {
    state: Arc<RwLock<SupervisorState>>,
    store: Arc<dyn EventStore>,
}

impl EventMonitor {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            state: Arc::new(RwLock::new(SupervisorState::new())),
            store,
        }
    }

    pub fn state(&self) -> SupervisorState {
        self.state.read().clone()
    }

    pub fn agent_state(&self, session_id: &str) -> Option<AgentState> {
        self.state.read().agents.get(session_id).cloned()
    }

    pub fn task_state(&self, task_id: &str) -> Option<TaskState> {
        self.state.read().tasks.get(task_id).cloned()
    }

    pub fn last_event_id(&self) -> Option<EventId> {
        self.state.read().last_event_id
    }

    pub async fn poll_events(&self, limit: usize) -> Result<Vec<(Event, EventId)>, PortError> {
        let since = self.state.read().last_event_id;
        self.store.stream(since, limit).await
    }

    pub fn process_event(&self, event: &Event, event_id: EventId) -> Vec<Event> {
        let mut state = self.state.write();
        let mut events_to_emit = Vec::new();
        state.last_event_id = Some(event_id);

        match &event.kind {
            EventKind::AgentSpawned {
                agent_id,
                session_id,
                role,
            } => {
                let agent_state =
                    AgentState::new(agent_id.clone(), session_id.clone(), role.clone());
                state.agents.insert(session_id.clone(), agent_state);
                debug!("Agent spawned: {} ({})", agent_id, session_id);
            }
            EventKind::AgentDied {
                agent_id,
                session_id,
                reason: _,
            } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.is_alive = false;
                    debug!("Agent died: {} ({})", agent_id, session_id);
                }
            }
            EventKind::PromptSent {
                session_id,
                content,
                ..
            } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = Utc::now();
                    agent.record_message(content.clone());
                }
            }
            EventKind::ResponseReceived { session_id, .. } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = Utc::now();
                    if let Some(nudge) = &agent.active_nudge {
                        if nudge.delivery_state == NudgeDeliveryState::Delivered {
                            let event = Event {
                                kind: EventKind::NudgeAcknowledged {
                                    session_id: session_id.clone(),
                                    nudge_kind: nudge.nudge_kind.clone(),
                                },
                                timestamp: Utc::now(),
                                aggregate_id: session_id.clone(),
                            };
                            events_to_emit.push(event);
                        }
                    }
                }
            }
            EventKind::OperationStarted {
                session_id,
                operation,
            } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = Utc::now();
                    agent.current_operation = Some(operation.clone());
                    debug!("Operation started: {} on {}", operation, session_id);
                    if let Some(nudge) = &agent.active_nudge {
                        if nudge.delivery_state == NudgeDeliveryState::Acknowledged {
                            let event = Event {
                                kind: EventKind::NudgeActedOn {
                                    session_id: session_id.clone(),
                                    nudge_kind: nudge.nudge_kind.clone(),
                                    progress_type: "operation".into(),
                                },
                                timestamp: Utc::now(),
                                aggregate_id: session_id.clone(),
                            };
                            events_to_emit.push(event);
                        }
                    }
                }
            }
            EventKind::OperationCompleted {
                session_id,
                operation: _,
                success: _,
            } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = Utc::now();
                    agent.current_operation = None;
                    if let Some(nudge) = &agent.active_nudge {
                        if nudge.delivery_state == NudgeDeliveryState::Acknowledged {
                            let event = Event {
                                kind: EventKind::NudgeActedOn {
                                    session_id: session_id.clone(),
                                    nudge_kind: nudge.nudge_kind.clone(),
                                    progress_type: "operation_completed".into(),
                                },
                                timestamp: Utc::now(),
                                aggregate_id: session_id.clone(),
                            };
                            events_to_emit.push(event);
                        }
                    }
                }
            }
            EventKind::TaskCreated { task_id } => {
                let task_state = TaskState::new(task_id.clone());
                state.tasks.insert(task_id.clone(), task_state);
            }
            EventKind::TaskAssigned { task_id, agent_id } => {
                if let Some(task) = state.tasks.get_mut(task_id) {
                    task.status = TaskStatus::Assigned;
                    task.assignee = Some(agent_id.clone());
                    task.last_event_at = Utc::now();
                }

                for agent in state.agents.values_mut() {
                    if agent.agent_id == *agent_id && agent.is_alive {
                        agent.current_task_id = Some(task_id.clone());
                        break;
                    }
                }
            }
            EventKind::TaskCompleted { task_id } => {
                if let Some(task) = state.tasks.get_mut(task_id) {
                    task.status = TaskStatus::InReview;
                    task.last_event_at = Utc::now();
                }
            }
            EventKind::NudgeSent {
                session_id, kind, ..
            } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = event.timestamp;
                    agent.active_nudge =
                        Some(ActiveNudge::new(kind.clone(), event.timestamp, session_id));
                }
            }
            EventKind::NudgeAcknowledged {
                session_id,
                nudge_kind,
            } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = event.timestamp;
                    if let Some(nudge) = &mut agent.active_nudge {
                        if nudge.delivery_state == NudgeDeliveryState::Delivered
                            && nudge.nudge_kind == *nudge_kind
                        {
                            nudge.delivery_state = NudgeDeliveryState::Acknowledged;
                            nudge.acknowledged_at = Some(event.timestamp);
                        }
                    }
                }
            }
            EventKind::NudgeActedOn {
                session_id,
                nudge_kind,
                ..
            } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = event.timestamp;
                    if let Some(nudge) = &mut agent.active_nudge {
                        if nudge.delivery_state == NudgeDeliveryState::Acknowledged
                            && nudge.nudge_kind == *nudge_kind
                        {
                            nudge.delivery_state = NudgeDeliveryState::ActedOn;
                            nudge.acted_on_at = Some(event.timestamp);
                        }
                    }
                }
            }
            EventKind::NudgeTimedOut {
                session_id,
                nudge_id,
                nudge_kind,
                ..
            } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = event.timestamp;
                    if let Some(nudge) = &mut agent.active_nudge {
                        if matches!(
                            nudge.delivery_state,
                            NudgeDeliveryState::Delivered | NudgeDeliveryState::Acknowledged
                        ) && nudge.nudge_id == *nudge_id
                            && nudge.nudge_kind == *nudge_kind
                        {
                            nudge.delivery_state = NudgeDeliveryState::TimedOut;
                            nudge.timed_out_at = Some(event.timestamp);
                        }
                    }
                }
            }
            EventKind::StuckDetected { session_id, .. } => {
                if let Some(agent) = state.agents.get_mut(session_id) {
                    agent.last_event_at = Utc::now();
                }
            }
            EventKind::SystemDraining { .. } => {
                state.system_state = brehon_types::SystemState::Draining;
            }
            _ => {}
        }
        events_to_emit
    }

    pub fn clear(&self) {
        *self.state.write() = SupervisorState::new();
    }

    /// Apply bounds to in-memory collections, removing dead agents and completed
    /// tasks that exceed the configured limits.
    pub fn apply_bounds(&self, max_dead_agents: usize, max_completed_tasks: usize) {
        let mut state = self.state.write();

        // Collect dead agents to potentially remove.
        let dead_agents: Vec<String> = state
            .agents
            .iter()
            .filter(|(_, a)| !a.is_alive)
            .map(|(sid, _)| sid.clone())
            .collect();

        if dead_agents.len() > max_dead_agents {
            let to_remove = dead_agents.len() - max_dead_agents;
            for session_id in dead_agents.into_iter().take(to_remove) {
                state.agents.remove(&session_id);
            }
        }

        // Collect completed/merged tasks to potentially remove.
        let completed_tasks: Vec<String> = state
            .tasks
            .iter()
            .filter(|(_, t)| matches!(t.status, brehon_types::TaskStatus::Merged))
            .map(|(tid, _)| tid.clone())
            .collect();

        if completed_tasks.len() > max_completed_tasks {
            let to_remove = completed_tasks.len() - max_completed_tasks;
            for task_id in completed_tasks.into_iter().take(to_remove) {
                state.tasks.remove(&task_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::InMemoryEventStore;

    use tokio::runtime::Runtime;

    fn make_test_runtime() -> Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn process_agent_spawned() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            let event = Event {
                kind: EventKind::AgentSpawned {
                    agent_id: "agent-1".into(),
                    session_id: "session-1".into(),
                    role: "worker".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "agent-1".into(),
            };

            monitor.process_event(&event, EventId::new(1));

            let state = monitor.state();
            assert_eq!(state.agents.len(), 1);
            assert!(state.agents.contains_key("session-1"));

            let agent = &state.agents["session-1"];
            assert_eq!(agent.agent_id, "agent-1");
            assert!(agent.is_alive);
        });
    }

    #[test]
    fn process_operation_lifecycle() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            let event = Event {
                kind: EventKind::AgentSpawned {
                    agent_id: "agent-1".into(),
                    session_id: "session-1".into(),
                    role: "worker".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "agent-1".into(),
            };
            monitor.process_event(&event, EventId::new(1));

            let event = Event {
                kind: EventKind::OperationStarted {
                    session_id: "session-1".into(),
                    operation: "cargo test".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "session-1".into(),
            };
            monitor.process_event(&event, EventId::new(2));

            let agent = monitor.agent_state("session-1").unwrap();
            assert!(agent.is_in_operation());
            assert_eq!(agent.current_operation, Some("cargo test".to_string()));

            let event = Event {
                kind: EventKind::OperationCompleted {
                    session_id: "session-1".into(),
                    operation: "cargo test".into(),
                    success: true,
                },
                timestamp: Utc::now(),
                aggregate_id: "session-1".into(),
            };
            monitor.process_event(&event, EventId::new(3));

            let agent = monitor.agent_state("session-1").unwrap();
            assert!(!agent.is_in_operation());
        });
    }

    #[test]
    fn process_task_lifecycle() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: "T001".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".into(),
            };
            monitor.process_event(&event, EventId::new(1));

            let task = monitor.task_state("T001").unwrap();
            assert_eq!(task.status, TaskStatus::Pending);

            let event = Event {
                kind: EventKind::TaskAssigned {
                    task_id: "T001".into(),
                    agent_id: "agent-1".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".into(),
            };
            monitor.process_event(&event, EventId::new(2));

            let task = monitor.task_state("T001").unwrap();
            assert_eq!(task.status, TaskStatus::Assigned);
            assert_eq!(task.assignee, Some("agent-1".to_string()));

            let event = Event {
                kind: EventKind::TaskCompleted {
                    task_id: "T001".into(),
                },
                timestamp: Utc::now(),
                aggregate_id: "T001".into(),
            };
            monitor.process_event(&event, EventId::new(3));

            let task = monitor.task_state("T001").unwrap();
            assert_eq!(task.status, TaskStatus::InReview);
        });
    }

    #[test]
    fn message_history_tracking() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            for i in 0..3 {
                monitor.process_event(
                    &Event {
                        kind: EventKind::PromptSent {
                            session_id: "session-1".into(),
                            prompt_id: format!("prompt-{}", i),
                            content: format!("message {}", i),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: "session-1".into(),
                    },
                    EventId::new(2 + i as u64),
                );
            }

            let agent = monitor.agent_state("session-1").unwrap();
            assert_eq!(agent.message_history.len(), 3);
        });
    }

    #[test]
    fn stuck_candidates() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            let monitor_ref = Arc::new(monitor);
            {
                let state = monitor_ref.state();
                let candidates = state.stuck_candidates(0);
                assert_eq!(candidates.len(), 1);
            }

            {
                let state = monitor_ref.state();
                let candidates = state.stuck_candidates(1000);
                assert!(candidates.is_empty());
            }
        });
    }

    #[test]
    fn nudge_state_tracking_delivered() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "soft".into(),
                        content: "Are you stuck?".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            let agent = monitor.agent_state("session-1").unwrap();
            let nudge = agent.active_nudge.as_ref().unwrap();
            assert_eq!(nudge.delivery_state, NudgeDeliveryState::Delivered);
            assert!(nudge.acknowledged_at.is_none());
            assert!(nudge.acted_on_at.is_none());
        });
    }

    #[test]
    fn nudge_state_transitions_to_acknowledged() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "soft".into(),
                        content: "Are you stuck?".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            let agent = monitor.agent_state("session-1").unwrap();
            let nudge = agent.active_nudge.as_ref().unwrap();
            assert_eq!(nudge.delivery_state, NudgeDeliveryState::Delivered);

            let events_to_emit = monitor.process_event(
                &Event {
                    kind: EventKind::ResponseReceived {
                        session_id: "session-1".into(),
                        prompt_id: "p1".into(),
                        tokens_used: 100,
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(3),
            );

            assert_eq!(
                events_to_emit.len(),
                1,
                "ResponseReceived should emit NudgeAcknowledged event"
            );
            if let EventKind::NudgeAcknowledged {
                session_id,
                nudge_kind,
            } = &events_to_emit[0].kind
            {
                assert_eq!(session_id, "session-1");
                assert_eq!(nudge_kind, "soft");
            } else {
                panic!("Expected NudgeAcknowledged event");
            }

            for event in events_to_emit {
                monitor.process_event(&event, EventId::new(4));
            }

            let agent = monitor.agent_state("session-1").unwrap();
            let nudge = agent.active_nudge.as_ref().unwrap();
            assert_eq!(nudge.delivery_state, NudgeDeliveryState::Acknowledged);
            assert!(nudge.acknowledged_at.is_some());
            assert!(nudge.acted_on_at.is_none());
        });
    }

    #[test]
    fn nudge_timeout_events_emit_for_stale_delivered_nudge() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);
            let sent_at = Utc::now() - chrono::Duration::seconds(121);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: sent_at,
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "soft".into(),
                        content: "Are you stuck?".into(),
                    },
                    timestamp: sent_at,
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            let timeout_events = monitor
                .state()
                .nudge_timeout_events(Utc::now(), Duration::from_secs(120));
            assert_eq!(timeout_events.len(), 1);
            if let EventKind::NudgeTimedOut {
                session_id,
                nudge_id,
                nudge_kind,
                elapsed_secs,
            } = &timeout_events[0].kind
            {
                assert_eq!(session_id, "session-1");
                assert_eq!(nudge_kind, "soft");
                assert!(nudge_id.starts_with("session-1-soft-"));
                assert!(*elapsed_secs >= 120);
            } else {
                panic!("Expected NudgeTimedOut event");
            }
        });
    }

    #[test]
    fn nudge_state_transitions_to_timed_out() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);
            let sent_at = Utc::now() - chrono::Duration::seconds(121);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: sent_at,
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );
            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "soft".into(),
                        content: "Are you stuck?".into(),
                    },
                    timestamp: sent_at,
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            let timeout_event = monitor
                .state()
                .nudge_timeout_events(Utc::now(), Duration::from_secs(120))
                .pop()
                .expect("stale delivered nudge should time out");
            monitor.process_event(&timeout_event, EventId::new(3));

            let agent = monitor.agent_state("session-1").unwrap();
            let nudge = agent.active_nudge.as_ref().unwrap();
            assert_eq!(nudge.delivery_state, NudgeDeliveryState::TimedOut);
            assert!(nudge.timed_out_at.is_some());

            let events_to_emit = monitor.process_event(
                &Event {
                    kind: EventKind::ResponseReceived {
                        session_id: "session-1".into(),
                        prompt_id: "late".into(),
                        tokens_used: 100,
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(4),
            );
            assert!(
                events_to_emit.is_empty(),
                "late response must not revive a timed-out nudge"
            );
        });
    }

    #[test]
    fn nudge_state_transitions_to_acted_on() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "soft".into(),
                        content: "Are you stuck?".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeAcknowledged {
                        session_id: "session-1".into(),
                        nudge_kind: "soft".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(3),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeActedOn {
                        session_id: "session-1".into(),
                        nudge_kind: "soft".into(),
                        progress_type: "percent".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(4),
            );

            let agent = monitor.agent_state("session-1").unwrap();
            let nudge = agent.active_nudge.as_ref().unwrap();
            assert_eq!(nudge.delivery_state, NudgeDeliveryState::ActedOn);
            assert!(nudge.acknowledged_at.is_some());
            assert!(nudge.acted_on_at.is_some());
        });
    }

    #[test]
    fn operation_started_emits_nudge_acted_on_when_acknowledged() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "soft".into(),
                        content: "Are you stuck?".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            let events_to_emit = monitor.process_event(
                &Event {
                    kind: EventKind::ResponseReceived {
                        session_id: "session-1".into(),
                        prompt_id: "p1".into(),
                        tokens_used: 100,
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(3),
            );

            assert_eq!(events_to_emit.len(), 1);
            for event in &events_to_emit {
                monitor.process_event(event, EventId::new(4));
            }

            let agent = monitor.agent_state("session-1").unwrap();
            assert_eq!(
                agent.active_nudge.as_ref().unwrap().delivery_state,
                NudgeDeliveryState::Acknowledged
            );

            let events_to_emit = monitor.process_event(
                &Event {
                    kind: EventKind::OperationStarted {
                        session_id: "session-1".into(),
                        operation: "cargo test".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(5),
            );

            assert_eq!(
                events_to_emit.len(),
                1,
                "OperationStarted should emit NudgeActedOn when nudge is Acknowledged"
            );
            if let EventKind::NudgeActedOn {
                session_id,
                nudge_kind,
                progress_type,
            } = &events_to_emit[0].kind
            {
                assert_eq!(session_id, "session-1");
                assert_eq!(nudge_kind, "soft");
                assert_eq!(progress_type, "operation");
            } else {
                panic!("Expected NudgeActedOn event");
            }

            for event in &events_to_emit {
                monitor.process_event(event, EventId::new(6));
            }

            let agent = monitor.agent_state("session-1").unwrap();
            assert_eq!(
                agent.active_nudge.as_ref().unwrap().delivery_state,
                NudgeDeliveryState::ActedOn
            );
        });
    }

    #[test]
    fn operation_completed_emits_nudge_acted_on_when_acknowledged() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "guidance".into(),
                        content: "Try a different approach".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            let events_to_emit = monitor.process_event(
                &Event {
                    kind: EventKind::ResponseReceived {
                        session_id: "session-1".into(),
                        prompt_id: "p1".into(),
                        tokens_used: 100,
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(3),
            );

            for event in &events_to_emit {
                monitor.process_event(event, EventId::new(4));
            }

            let events_to_emit = monitor.process_event(
                &Event {
                    kind: EventKind::OperationCompleted {
                        session_id: "session-1".into(),
                        operation: "cargo build".into(),
                        success: true,
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(5),
            );

            assert_eq!(
                events_to_emit.len(),
                1,
                "OperationCompleted should emit NudgeActedOn when nudge is Acknowledged"
            );
            if let EventKind::NudgeActedOn {
                session_id,
                nudge_kind,
                progress_type,
            } = &events_to_emit[0].kind
            {
                assert_eq!(session_id, "session-1");
                assert_eq!(nudge_kind, "guidance");
                assert_eq!(progress_type, "operation_completed");
            } else {
                panic!("Expected NudgeActedOn event");
            }
        });
    }

    #[test]
    fn no_nudge_acted_on_when_nudge_delivered() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "soft".into(),
                        content: "Are you stuck?".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            let events_to_emit = monitor.process_event(
                &Event {
                    kind: EventKind::OperationStarted {
                        session_id: "session-1".into(),
                        operation: "cargo test".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(3),
            );

            assert!(
                events_to_emit.is_empty(),
                "Should not emit NudgeActedOn when nudge is Delivered"
            );
        });
    }

    #[test]
    fn nudge_state_query_methods() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            monitor.process_event(
                &Event {
                    kind: EventKind::AgentSpawned {
                        agent_id: "agent-1".into(),
                        session_id: "session-1".into(),
                        role: "worker".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "agent-1".into(),
                },
                EventId::new(1),
            );

            assert!(monitor
                .state()
                .nudge_state_for_session("session-1")
                .is_none());

            monitor.process_event(
                &Event {
                    kind: EventKind::NudgeSent {
                        session_id: "session-1".into(),
                        kind: "soft".into(),
                        content: "Are you stuck?".into(),
                    },
                    timestamp: Utc::now(),
                    aggregate_id: "session-1".into(),
                },
                EventId::new(2),
            );

            let state = monitor.state();
            assert_eq!(
                state.nudge_state_for_session("session-1"),
                Some(NudgeDeliveryState::Delivered)
            );
            assert!(state.active_nudge_for_session("session-1").is_some());
        });
    }

    #[test]
    fn apply_bounds_removes_dead_agents_and_completed_tasks() {
        let rt = make_test_runtime();
        rt.block_on(async {
            let store = Arc::new(InMemoryEventStore::new());
            let monitor = EventMonitor::new(store);

            for i in 0..5 {
                monitor.process_event(
                    &Event {
                        kind: EventKind::AgentSpawned {
                            agent_id: format!("agent-{}", i),
                            session_id: format!("session-{}", i),
                            role: "worker".into(),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: format!("agent-{}", i),
                    },
                    EventId::new(i as u64 + 1),
                );
                monitor.process_event(
                    &Event {
                        kind: EventKind::AgentDied {
                            agent_id: format!("agent-{}", i),
                            session_id: format!("session-{}", i),
                            reason: "done".into(),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: format!("agent-{}", i),
                    },
                    EventId::new(i as u64 + 6),
                );
                monitor.process_event(
                    &Event {
                        kind: EventKind::TaskCreated {
                            task_id: format!("T{}", i),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: format!("T{}", i),
                    },
                    EventId::new(i as u64 + 11),
                );
            }

            monitor.apply_bounds(2, 2);
            let state = monitor.state();
            assert_eq!(state.agents.len(), 2);
            assert_eq!(state.tasks.len(), 5);
        });
    }
}
