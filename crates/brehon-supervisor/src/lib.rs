//! Supervisor crate for the Brehon system.
//!
//! The supervisor is a persistent Rust process that runs for the entire Brehon session.
//! It monitors events from EventStore continuously, maintains in-memory state from events,
//! detects stuck workers and agents, sends nudges via AgentGateway, invokes AI (DecisionEngine)
//! when judgment is needed, tracks budget and enforces limits, and escalates to humans when needed.

mod autonomy;
mod budget_tracker;
mod escalation;
mod event_monitor;
pub mod feedback;
mod heartbeat;
mod nudge;
mod stuck_detection;
mod supervisor;

pub use autonomy::AutonomyConfig;
pub use budget_tracker::{BudgetPolicy, BudgetTracker};
pub use escalation::{EscalationConfig, EscalationManager};
pub use event_monitor::{ActiveNudge, AgentState, EventMonitor, SupervisorState};
pub use heartbeat::HeartbeatConfig;
pub use nudge::{Nudge, NudgeGenerator, NudgeHistoryEntry, NudgeId, NudgeKind, NudgeSender};
pub use stuck_detection::{
    RecommendedAction, StuckDetectionConfig, StuckDetector, StuckInfo, TaskAwareStuckDetector,
    TaskContext,
};
pub use supervisor::{Supervisor, SupervisorConfig, SupervisorDependencies};

pub use brehon_types::AutonomyLevel;
pub use brehon_types::NudgeDeliveryState;
