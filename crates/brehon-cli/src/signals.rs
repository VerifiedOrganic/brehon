//! Signal handling for graceful shutdown with tracked drain of in-flight work.
//!
//! Signal handlers set the process-global draining flag via
//! [`brehon_types::drain::set_draining()`]. Subsystems that check
//! [`brehon_types::drain::is_draining()`] will refuse new work but complete
//! work in progress. In-flight work tracked via
//! [`brehon_types::drain::in_flight_guard()`] is drained by
//! [`wait_for_shutdown()`] before the process terminates.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

// Re-export the drain API so callers can use `crate::signals::is_draining()` etc.
// without depending on brehon_types directly. Only the pub use is needed — no
// separate private `use` for these symbols, which avoids E0252 duplicate imports.
pub use brehon_types::drain::{
    complete_in_flight, drain_sync, in_flight_count, in_flight_guard, in_flight_labels,
    is_draining, register_in_flight, set_draining, InFlightGuard,
};

/// Install signal handlers for graceful shutdown.
///
/// On SIGINT (Ctrl+C) or SIGTERM, sets both the global draining flag
/// (via [`set_draining()`]) and the shared `shutdown_flag` Arc so that
/// any loop waiting on the Arc (such as the TUI main loop) will break.
pub fn setup_signal_handlers(shutdown_flag: Arc<AtomicBool>) -> Result<()> {
    let sigterm_flag = shutdown_flag.clone();

    ctrlc::set_handler(move || {
        info!("Received interrupt signal, initiating graceful shutdown...");
        set_draining();
        shutdown_flag.store(true, Ordering::SeqCst);
    })?;

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        tokio::spawn(async move {
            let mut sigterm =
                signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");

            loop {
                _ = sigterm.recv().await;
                info!("Received SIGTERM, initiating graceful shutdown...");
                set_draining();
                sigterm_flag.store(true, Ordering::SeqCst);
            }
        });
    }

    Ok(())
}

/// Wait for the shutdown signal, then drain in-flight work up to the given timeout.
///
/// This function:
/// 1. Blocks until a shutdown signal is received (SIGINT or SIGTERM).
/// 2. Logs that draining has started.
/// 3. Waits for all tracked in-flight work to complete, or for `drain_timeout` to expire.
/// 4. If work remains after the timeout, logs a warning listing pending items.
/// 5. Returns, allowing the caller to proceed with final cleanup (kill panes, etc.).
pub async fn wait_for_shutdown(shutdown_flag: Arc<AtomicBool>, drain_timeout: std::time::Duration) {
    // Phase 1: Wait for shutdown signal
    while !shutdown_flag.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    info!("Draining started — no new tasks will be dispatched, in-flight work will complete");

    // Phase 2: Drain in-flight work up to the timeout
    let total_at_start = in_flight_count();
    if total_at_start == 0 {
        info!("No in-flight work to drain");
        return;
    }

    info!(
        in_flight = total_at_start,
        timeout_secs = drain_timeout.as_secs(),
        "Waiting for in-flight work to drain"
    );

    // Use the sync drain (condvar-driven) inside a blocking task
    // so we get immediate wake-on-completion instead of pure polling.
    let timeout_clone = drain_timeout;
    let (remaining, total) =
        tokio::task::spawn_blocking(move || brehon_types::drain::drain_sync(timeout_clone))
            .await
            .unwrap_or_else(|_| (in_flight_count(), total_at_start));

    if remaining == 0 {
        info!(count = total, "All in-flight work completed during drain");
    } else {
        // Log which items are still in flight
        for label in in_flight_labels() {
            warn!(label = %label, "In-flight work still pending at drain deadline");
        }
        warn!(
            remaining = remaining,
            total = total,
            "Drain timeout expired with in-flight work still pending — sessions will be terminated"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_draining_reflects_shutdown_flag() {
        brehon_types::drain::reset_draining_for_test();
        assert!(!is_draining());

        set_draining();
        assert!(is_draining());

        brehon_types::drain::reset_draining_for_test();
        assert!(!is_draining());
    }

    #[tokio::test]
    async fn wait_for_shutdown_returns_immediately_when_no_work() {
        // Create a flag already set to simulate "already shutting down"
        let flag = Arc::new(AtomicBool::new(true));
        // Should complete immediately since in_flight_count == 0
        wait_for_shutdown(flag, std::time::Duration::from_secs(5)).await;
        // If we get here, the function returned without hanging
    }
}
