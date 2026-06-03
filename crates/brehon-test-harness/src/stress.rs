//! Concurrent load generators for stress testing.
//!
//! Storage stress testing and concurrent writer simulation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::future::join_all;
use tokio::sync::Barrier;

use brehon_ports::EventStore;
use brehon_types::{Event, EventKind};

use crate::InMemoryEventStore;

/// Stress test configuration.
#[derive(Debug, Clone)]
pub struct StressConfig {
    pub writer_count: usize,
    pub events_per_writer: usize,
    pub reader_count: usize,
    pub contention_target: String,
}

impl Default for StressConfig {
    fn default() -> Self {
        Self {
            writer_count: 10,
            events_per_writer: 1000,
            reader_count: 10,
            contention_target: "test".into(),
        }
    }
}

/// Stress test result.
#[derive(Debug, Clone)]
pub struct StressResult {
    pub total_events: usize,
    pub total_time: Duration,
    pub events_per_second: f64,
    pub errors: usize,
    pub writer_times: Vec<Duration>,
    pub reader_times: Vec<Duration>,
}

/// Storage stress tester.
pub struct StorageStress {
    config: StressConfig,
    store: Arc<InMemoryEventStore>,
}

impl StorageStress {
    pub fn new(store: Arc<InMemoryEventStore>) -> Self {
        Self {
            config: StressConfig::default(),
            store,
        }
    }

    pub fn with_config(store: Arc<InMemoryEventStore>, config: StressConfig) -> Self {
        Self { config, store }
    }

    pub async fn run_concurrent_writes(&self) -> StressResult {
        let start = Instant::now();
        let barrier = Arc::new(Barrier::new(self.config.writer_count));

        let handles: Vec<_> = (0..self.config.writer_count)
            .map(|writer_id| {
                let barrier = Arc::clone(&barrier);
                let store = Arc::clone(&self.store);
                let events_per_writer = self.config.events_per_writer;

                tokio::spawn(async move {
                    barrier.wait().await;

                    let writer_start = Instant::now();

                    for i in 0..events_per_writer {
                        let event = Event {
                            kind: EventKind::TaskCreated {
                                task_id: format!("T{}-{}", writer_id, i),
                            },
                            timestamp: Utc::now(),
                            aggregate_id: format!("writer-{}", writer_id),
                        };

                        if store.append(event).await.is_err() {
                            return Err(());
                        }
                    }

                    Ok(writer_start.elapsed())
                })
            })
            .collect();

        let results: Vec<_> = join_all(handles).await;

        let total_time = start.elapsed();
        let total_events = self.config.writer_count * self.config.events_per_writer;

        let writer_times: Vec<Duration> = results
            .into_iter()
            .filter_map(|r| r.ok())
            .filter_map(|r| r.ok())
            .collect();

        let errors = self.config.writer_count - writer_times.len();

        StressResult {
            total_events,
            total_time,
            events_per_second: total_events as f64 / total_time.as_secs_f64(),
            errors,
            writer_times,
            reader_times: vec![],
        }
    }

    pub async fn run_concurrent_reads_writes(&self) -> StressResult {
        let barrier = Arc::new(Barrier::new(
            self.config.writer_count + self.config.reader_count,
        ));
        let store = Arc::clone(&self.store);

        let writer_handles: Vec<_> = (0..self.config.writer_count)
            .map(|writer_id| {
                let barrier = Arc::clone(&barrier);
                let store = Arc::clone(&store);
                let events_per_writer = self.config.events_per_writer;

                tokio::spawn(async move {
                    barrier.wait().await;

                    let start = Instant::now();

                    for i in 0..events_per_writer {
                        let event = Event {
                            kind: EventKind::TaskCreated {
                                task_id: format!("T{}-{}", writer_id, i),
                            },
                            timestamp: Utc::now(),
                            aggregate_id: format!("writer-{}", writer_id),
                        };

                        let _ = store.append(event).await;
                    }

                    start.elapsed()
                })
            })
            .collect();

        let reader_handles: Vec<_> = (0..self.config.reader_count)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let store = Arc::clone(&store);
                let events_per_writer = self.config.events_per_writer;
                let writer_count = self.config.writer_count;

                tokio::spawn(async move {
                    barrier.wait().await;

                    let start = Instant::now();

                    let target = writer_count * events_per_writer / 2;

                    for _ in 0..10 {
                        let _ = store.stream(None, target).await;
                    }

                    start.elapsed()
                })
            })
            .collect();

        let writer_results: Vec<_> = join_all(writer_handles).await;
        let reader_results: Vec<_> = join_all(reader_handles).await;

        let total_time = writer_results
            .iter()
            .chain(reader_results.iter())
            .filter_map(|r| r.as_ref().ok())
            .max()
            .copied()
            .unwrap_or_default();

        let total_events = self.config.writer_count * self.config.events_per_writer;

        let writer_times: Vec<Duration> =
            writer_results.into_iter().filter_map(|r| r.ok()).collect();

        let reader_times: Vec<Duration> =
            reader_results.into_iter().filter_map(|r| r.ok()).collect();

        StressResult {
            total_events,
            total_time,
            events_per_second: total_events as f64 / total_time.as_secs_f64(),
            errors: 0,
            writer_times,
            reader_times,
        }
    }

    pub async fn run_queue_contention(&self) -> StressResult {
        let store = Arc::clone(&self.store);
        store.enqueue(&self.config.contention_target, "item-1", "data");
        store.enqueue(&self.config.contention_target, "item-2", "data");
        store.enqueue(&self.config.contention_target, "item-3", "data");

        let barrier = Arc::new(Barrier::new(self.config.writer_count));
        let queue_name = self.config.contention_target.clone();

        let handles: Vec<_> = (0..self.config.writer_count)
            .map(|consumer_id| {
                let barrier = Arc::clone(&barrier);
                let store = Arc::clone(&store);
                let queue = queue_name.clone();

                tokio::spawn(async move {
                    barrier.wait().await;

                    let start = Instant::now();

                    let claim = store
                        .claim_next(
                            &queue,
                            &format!("consumer-{}", consumer_id),
                            Duration::from_secs(10),
                        )
                        .await
                        .unwrap();

                    if let Some(claim) = claim {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let _ = store.ack_claim(&claim.claim_id).await;
                        (start.elapsed(), true)
                    } else {
                        (start.elapsed(), false)
                    }
                })
            })
            .collect();

        let results: Vec<_> = join_all(handles).await;

        let total_claims: usize = results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .filter(|(_, claimed)| *claimed)
            .count();

        let writer_times: Vec<Duration> = results
            .into_iter()
            .filter_map(|r| r.ok())
            .map(|(d, _)| d)
            .collect();

        StressResult {
            total_events: total_claims,
            total_time: writer_times.iter().sum(),
            events_per_second: total_claims as f64,
            errors: self.config.writer_count - total_claims,
            writer_times,
            reader_times: vec![],
        }
    }
}

/// Concurrent writer simulation.
pub struct ConcurrentWriter {
    store: Arc<InMemoryEventStore>,
    writer_id: usize,
}

impl ConcurrentWriter {
    pub fn new(store: Arc<InMemoryEventStore>, writer_id: usize) -> Self {
        Self { store, writer_id }
    }

    pub async fn write_events(&self, count: usize) -> usize {
        let mut written = 0;

        for i in 0..count {
            let event = Event {
                kind: EventKind::TaskCreated {
                    task_id: format!("T{}-{}", self.writer_id, i),
                },
                timestamp: Utc::now(),
                aggregate_id: format!("writer-{}", self.writer_id),
            };

            if EventStore::append(&*self.store, event).await.is_ok() {
                written += 1;
            }
        }

        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_writes() {
        let store = Arc::new(InMemoryEventStore::new());

        let writer1 = ConcurrentWriter::new(Arc::clone(&store), 1);
        let writer2 = ConcurrentWriter::new(Arc::clone(&store), 2);

        let (r1, r2) = tokio::join!(writer1.write_events(100), writer2.write_events(100));

        assert_eq!(r1, 100);
        assert_eq!(r2, 100);
        assert_eq!(store.len(), 200);
    }

    #[tokio::test]
    async fn stress_concurrent_writes() {
        let config = StressConfig {
            writer_count: 5,
            events_per_writer: 100,
            reader_count: 0,
            contention_target: "test".into(),
        };

        let store = Arc::new(InMemoryEventStore::new());
        let stress = StorageStress::with_config(store, config);

        let result = stress.run_concurrent_writes().await;

        assert_eq!(result.total_events, 500);
        assert_eq!(result.errors, 0);
    }

    #[tokio::test]
    async fn queue_contention_no_double_claim() {
        let config = StressConfig {
            writer_count: 10,
            events_per_writer: 1,
            reader_count: 0,
            contention_target: "test-queue".into(),
        };

        let store = Arc::new(InMemoryEventStore::new());
        let stress = StorageStress::with_config(Arc::clone(&store), config);

        let result = stress.run_queue_contention().await;

        assert_eq!(result.total_events, 3);
        assert_eq!(result.errors, 7);
    }
}
