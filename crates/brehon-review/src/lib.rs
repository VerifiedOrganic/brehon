//! Review coordinator for managing code review workflows.
//!
//! This crate handles:
//! - Priority review queue with FIFO within lanes
//! - Panel management (assigning reviewers to tasks)
//! - Score collection and threshold evaluation
//! - Feedback consolidation (dedup, categorize, preserve dissent)
//! - Stale detection (main moves during review)
//! - Session lifecycle (spawn, collect, kill)

pub mod calibration;
pub mod chunking;
pub mod consolidation;
pub mod coordinator;
pub mod lifecycle;
pub mod panel;
pub mod queue;
pub mod scoring;
pub mod stale;

pub use calibration::{PerReviewerStats, ReviewerCalibration};
pub use chunking::{ChunkingConfig, DiffChunker};
pub use consolidation::FeedbackConsolidator;
pub use coordinator::ReviewCoordinator;
pub use lifecycle::{LifecycleError, ReviewLifecycle};
pub use panel::{PanelAffinity, ReviewPanel};
pub use queue::{PriorityQueue, QueueError};
pub use scoring::{ScoreCollector, ThresholdEvaluator, ThresholdResult};
pub use stale::{StaleDetection, StaleDetector};

use thiserror::Error;

use crate::panel::PanelError;

#[derive(Debug, Error)]
pub enum ReviewError {
    #[error("Queue error: {0}")]
    Queue(#[from] QueueError),

    #[error("Lifecycle error: {0}")]
    Lifecycle(#[from] LifecycleError),

    #[error("Panel error: {0}")]
    Panel(#[from] PanelError),

    #[error("Scoring error: {0}")]
    Scoring(String),

    #[error("Stale detection error: {0}")]
    StaleDetection(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Agent gateway error: {0}")]
    Gateway(String),

    #[error("Git error: {0}")]
    Git(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Port error: {0}")]
    Port(#[from] brehon_ports::PortError),
}
