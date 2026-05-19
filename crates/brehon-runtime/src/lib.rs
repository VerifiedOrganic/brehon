//! Runtime side-channel primitives.
//!
//! This crate contains concrete runtime coordination building blocks that sit
//! behind the abstract ports in `brehon-ports`. The first primitive is a bounded
//! in-process event bus. A future daemon can use the same semantics at its
//! process boundary.

use brehon_ports::{PortError, RuntimeEventSink, RuntimeEventStream};
use brehon_types::RuntimeEvent;
use async_trait::async_trait;
use tokio::sync::broadcast;

/// Default runtime event capacity per subscriber.
pub const DEFAULT_RUNTIME_EVENT_CAPACITY: usize = 1024;

/// Bounded in-process fanout for runtime events.
#[derive(Debug, Clone)]
pub struct RuntimeEventBus {
    tx: broadcast::Sender<RuntimeEvent>,
    capacity: usize,
}

impl Default for RuntimeEventBus {
    fn default() -> Self {
        Self::new(DEFAULT_RUNTIME_EVENT_CAPACITY)
    }
}

impl RuntimeEventBus {
    /// Create a bounded bus. A capacity of zero is promoted to one.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx, capacity }
    }

    /// Subscribe to future runtime events.
    pub fn subscribe(&self) -> RuntimeEventReceiver {
        RuntimeEventReceiver {
            rx: self.tx.subscribe(),
        }
    }

    /// Number of active subscribers.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Per-subscriber buffer capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[async_trait]
impl RuntimeEventSink for RuntimeEventBus {
    async fn publish(&self, event: RuntimeEvent) -> Result<(), PortError> {
        match self.tx.send(event) {
            Ok(_) => Ok(()),
            Err(err) => {
                // Tokio broadcast reports an error when there are no active
                // receivers. Runtime publication is best-effort in sidecar
                // mode, so no-subscriber publication is not a runtime failure.
                tracing::trace!(
                    event = ?err.0,
                    "Dropped runtime event because no subscribers are registered"
                );
                Ok(())
            }
        }
    }
}

/// Runtime event stream returned by [`RuntimeEventBus::subscribe`].
#[derive(Debug)]
pub struct RuntimeEventReceiver {
    rx: broadcast::Receiver<RuntimeEvent>,
}

#[async_trait]
impl RuntimeEventStream for RuntimeEventReceiver {
    async fn next_event(&mut self) -> Result<Option<RuntimeEvent>, PortError> {
        match self.rx.recv().await {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::RecvError::Closed) => Ok(None),
            Err(broadcast::error::RecvError::Lagged(skipped)) => Err(PortError::Runtime(format!(
                "runtime event stream lagged by {skipped} events"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{AgentTurnEvent, RuntimeEventKind, RuntimeEventMeta, RuntimeSource};

    fn event(seq: u64) -> RuntimeEvent {
        RuntimeEvent::new(
            RuntimeEventMeta::new("session", "pane", seq, RuntimeSource::Mux, seq),
            RuntimeEventKind::AgentTurnStarted(AgentTurnEvent {
                prompt_id: Some(format!("prompt-{seq}")),
                reason: None,
            }),
        )
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_ok() {
        let bus = RuntimeEventBus::new(8);

        bus.publish(event(1))
            .await
            .expect("publish without subscribers");
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test]
    async fn fanout_delivers_to_all_subscribers() {
        let bus = RuntimeEventBus::new(8);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        bus.publish(event(7)).await.expect("publish");

        let a_event = a.next_event().await.expect("stream a").expect("event a");
        let b_event = b.next_event().await.expect("stream b").expect("event b");

        assert_eq!(a_event.meta.generation, 7);
        assert_eq!(b_event.meta.generation, 7);
    }

    #[tokio::test]
    async fn lagged_receiver_reports_backpressure_loss_and_can_continue() {
        let bus = RuntimeEventBus::new(1);
        let mut rx = bus.subscribe();

        bus.publish(event(1)).await.expect("publish 1");
        bus.publish(event(2)).await.expect("publish 2");

        let err = rx.next_event().await.expect_err("lag error");
        assert!(matches!(err, PortError::Runtime(_)));

        let recovered = rx
            .next_event()
            .await
            .expect("stream recovers")
            .expect("latest event remains");
        assert_eq!(recovered.meta.generation, 2);
    }

    #[tokio::test]
    async fn receiver_returns_none_after_bus_closes() {
        let bus = RuntimeEventBus::new(8);
        let mut rx = bus.subscribe();

        drop(bus);

        assert!(rx.next_event().await.expect("closed stream").is_none());
    }
}
