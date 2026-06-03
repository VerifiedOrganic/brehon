use std::path::PathBuf;
use std::time::{Duration, Instant};

use brehon_mux::PromptDeliveryAttempt;
use tokio::task::JoinHandle;

pub(super) struct AsyncQueuedGatewayPromptDeliveryTask {
    pub path: PathBuf,
    pub target: String,
    pub from: Option<String>,
    /// MCP-minted prompt id that the delivery-ack writer keys its file on.
    /// Optional because queue entries written before the prompt_id field
    /// existed may still be in flight after an upgrade.
    pub prompt_id: Option<String>,
    pub prompt_text: String,
    pub handle: JoinHandle<
        std::result::Result<PromptDeliveryAttempt, brehon_mux::AsyncGatewayPromptDeliveryError>,
    >,
    /// When the background handle was spawned. Used by the watchdog in the
    /// completion-scan loop to abort handles that never finish — otherwise a
    /// stuck gateway future silently prevents all future polls from touching
    /// the same `.prompt` file (it stays "already pending" forever).
    pub started_at: Instant,
}

/// Maximum wall-clock time a queued gateway prompt delivery handle may run
/// before the watchdog aborts it and records a retry failure. Normal delivery
/// completes in well under 5 seconds for any supported backend; anything past
/// this window is a stuck future (e.g., a Gemini ACP edge case that leaves
/// the JoinHandle unfinished despite the 1.5s short-acceptance timeout).
pub(super) const QUEUED_GATEWAY_PROMPT_WATCHDOG: Duration = Duration::from_secs(120);
