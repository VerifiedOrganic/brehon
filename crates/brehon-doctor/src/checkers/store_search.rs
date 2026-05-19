//! Store/search diagnostic checker.
//!
//! Validates persistence integrity across fjall queue state, materialized views,
//! and Tantivy index consistency.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;

use super::Checker;
use crate::types::{DiagnosticCategory, DiagnosticFinding, Severity};
use brehon_types::{
    deserialize_event_envelope, Event, EventEnvelope, EventId, EventKind, QueueClaim, ReviewStatus,
    ReviewView, TaskStatus, TaskView,
};
use chrono::Utc;
use fjall::PartitionHandle;
use tantivy::schema::Value;
use tantivy::TantivyDocument;

/// Fjall partition names used by the event store.
const EVENTS_PARTITION: &str = "events";
const QUEUE_PARTITION: &str = "queue";
const VIEWS_PARTITION: &str = "views";

/// Default on-disk Tantivy index path relative to `.brehon`.
const DEFAULT_TANTIVY_PATH: &str = "indexes/tantivy";

/// Store/search consistency checker.
pub struct StoreSearchChecker {
    brehon_root: PathBuf,
}

impl StoreSearchChecker {
    pub fn new(brehon_root: &Path) -> Self {
        Self {
            brehon_root: brehon_root.to_path_buf(),
        }
    }

    fn events_path(&self) -> PathBuf {
        self.brehon_root.join("runtime").join("events")
    }

    fn tantivy_path(&self) -> PathBuf {
        self.brehon_root.join(DEFAULT_TANTIVY_PATH)
    }

    fn keyspace_has_data(path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        let entries = match path.read_dir() {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        entries.count() > 0
    }

    fn check_queue_lease_consistency(
        &self,
        queue: &PartitionHandle,
    ) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let state = scan_queue_state(&queue)?;

        for malformed in &state.malformed_claimed_values {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::StoreSearch,
                    Severity::Warning,
                    format!("Malformed claimed marker value: {}", malformed),
                )
                .with_subject("claimed value".to_string())
                .with_description("Claim marker value (claim id) is not valid UTF-8")
                .with_suggestion(
                    "Cleanup the queue partition and let runtime recreation handle missing markers",
                ),
            );
        }

        for malformed in &state.malformed_lease_values {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::StoreSearch,
                    Severity::Warning,
                    format!("Malformed lease payload for {}", malformed),
                )
                .with_subject("lease payload".to_string())
                .with_description(
                    "Queue lease marker payload is not a valid JSON QueueClaim, treat as orphan",
                )
                .with_suggestion("Inspect queue partition and consider restart/cleanup"),
            );
        }

        let mut claimed_counts: HashMap<(String, String), usize> = HashMap::new();
        for claimed in &state.claimed_entries {
            *claimed_counts
                .entry((claimed.queue_name.clone(), claimed.item_id.clone()))
                .or_default() += 1;
        }

        for ((queue_name, item_id), count) in claimed_counts {
            if count > 1 {
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::StoreSearch,
                        Severity::Warning,
                        format!("Duplicate claimed markers for queue item: {queue_name}:{item_id}"),
                    )
                    .with_subject(format!("{}:{item_id}", queue_name))
                    .with_description(format!(
                        "Found {count} claimed markers for the same queue item {item_id}"
                    ))
                    .with_suggestion("Clear stale claimed markers and restart recovery"),
                );
            }
        }

        for claimed in &state.claimed_entries {
            match state.leases.get(&claimed.claim_id) {
                Some(Some(claim)) => {
                    if claim.queue != claimed.queue_name || claim.item_id != claimed.item_id {
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::StoreSearch,
                                Severity::Warning,
                                format!(
                                    "Inconsistent claimed marker for {}:{}",
                                    claimed.queue_name, claimed.item_id
                                ),
                            )
                            .with_subject(claimed.claim_id.clone())
                            .with_description("Claim payload queue/item does not match claimed marker key")
                            .with_suggestion(
                                "Remove the mismatched claimed marker and let the queue allocator recover",
                            ),
                        );
                    }

                    if !state
                        .queue_items
                        .contains(&(claim.queue.clone(), claim.item_id.clone()))
                    {
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::StoreSearch,
                                Severity::Warning,
                                format!(
                                    "Claimed queue item missing from queue: {}:{}",
                                    claim.queue, claim.item_id
                                ),
                            )
                            .with_subject(claimed.claim_id.clone())
                            .with_description(format!(
                                "Claim {} points to a missing queue entry",
                                claimed.claim_id
                            ))
                            .with_suggestion(
                                "Re-enqueue the item or let retry logic recreate queue markers",
                            ),
                        );
                    }
                }
                Some(None) => {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::StoreSearch,
                            Severity::Warning,
                            format!("Invalid lease payload for claim {}", claimed.claim_id),
                        )
                        .with_subject(claimed.claim_id.clone())
                        .with_description("Lease data could not be parsed as QueueClaim")
                        .with_suggestion("Remove invalid lease marker and restart queue recovery"),
                    );
                }
                None => {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::StoreSearch,
                            Severity::Warning,
                            format!(
                                "Claim marker without lease: {} for {}:{}",
                                claimed.claim_id, claimed.queue_name, claimed.item_id
                            ),
                        )
                        .with_subject(claimed.claim_id.clone())
                        .with_suggestion(
                            "Remove stale claimed marker; expired marker cleanup should remove it",
                        ),
                    );
                }
            }
        }

        for (claim_id, lease) in &state.leases {
            if let Some(claim) = lease {
                let queue_name = claim.queue.clone();
                let item_id = claim.item_id.clone();
                let expected_claimed_key = format!("claimed:{}:{}", queue_name, item_id);
                if !state.claimed_keys.contains(&expected_claimed_key) {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::StoreSearch,
                            Severity::Warning,
                            format!("Lease without claimed marker: {claim_id}"),
                        )
                        .with_subject(format!("{}/{}", queue_name, item_id))
                        .with_description(format!(
                            "Lease {claim_id} has no matching claimed marker"
                        ))
                        .with_suggestion("Remove stale lease markers and allow reclaim"),
                    );
                }

                if claim_is_expired(claim) {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::StoreSearch,
                            Severity::Info,
                            format!(
                                "Lease expired while still present: {claim_id} for {}:{}",
                                queue_name, item_id
                            ),
                        )
                        .with_subject(claim_id.clone())
                        .with_description(
                            "Expired lease markers should be cleaned by queue recovery logic",
                        )
                        .with_suggestion("Run recovery/claim cleanup on startup"),
                    );
                }
            }
        }

        // Queue item duplicates with different claim state can still indicate queue leaks.
        for (queue_item, count) in state.queue_item_counts {
            if count > 1 {
                findings.push(
                    DiagnosticFinding::new(
                        DiagnosticCategory::StoreSearch,
                        Severity::Warning,
                        format!(
                            "Duplicate queue entries for {}:{}",
                            queue_item.0, queue_item.1
                        ),
                    )
                    .with_subject(format!("{}:{}", queue_item.0, queue_item.1))
                    .with_description(format!(
                        "Queue item appears {count} times with different sequence numbers"
                    ))
                    .with_suggestion("Deduplicate queue entries before retry processing"),
                );
            }
        }

        Ok(findings)
    }

    fn check_view_drift(
        &self,
        events: &PartitionHandle,
        views: &PartitionHandle,
    ) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let (expected_tasks, expected_reviews) = rebuild_views_from_events(&events)?;
        let (stored_tasks, stored_reviews) = load_persisted_views(&views)?;

        let mut task_ids = BTreeSet::new();
        task_ids.extend(expected_tasks.keys().cloned());
        task_ids.extend(stored_tasks.keys().cloned());

        for task_id in task_ids {
            let expected = expected_tasks.get(&task_id);
            let observed = stored_tasks.get(&task_id);
            match (expected, observed) {
                (Some(expected_view), Some(stored_view)) => {
                    if !task_views_equivalent(expected_view, stored_view) {
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::StoreSearch,
                                Severity::Error,
                                format!("Task view drift: {task_id}"),
                            )
                            .with_subject(format!("view:task:{task_id}"))
                            .with_description(format!(
                                "Expected {:?}, persisted {:?}",
                                expected_view, stored_view
                            ))
                            .with_suggestion("Rebuild views from events on startup"),
                        );
                    }
                }
                (Some(expected_view), None) => {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::StoreSearch,
                            Severity::Error,
                            format!("Task view missing: {task_id}"),
                        )
                        .with_subject(format!("view:task:{task_id}"))
                        .with_description(format!(
                            "Expected view {:?} but no persisted view exists",
                            expected_view
                        ))
                        .with_suggestion("Clear stale view cache and rebuild from event log"),
                    );
                }
                (None, Some(stored_view)) => {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::StoreSearch,
                            Severity::Error,
                            format!("Unexpected task view: {task_id}"),
                        )
                        .with_subject(format!("view:task:{task_id}"))
                        .with_description(format!(
                            "Persisted view exists but no log replay generated it: {:?}",
                            stored_view
                        ))
                        .with_suggestion("Remove stale view entry"),
                    );
                }
                (None, None) => {}
            }
        }

        let mut review_ids = BTreeSet::new();
        review_ids.extend(expected_reviews.keys().cloned());
        review_ids.extend(stored_reviews.keys().cloned());

        for review_id in review_ids {
            let expected = expected_reviews.get(&review_id);
            let observed = stored_reviews.get(&review_id);
            match (expected, observed) {
                (Some(expected_view), Some(stored_view)) => {
                    if !review_views_equivalent(expected_view, stored_view) {
                        findings.push(
                            DiagnosticFinding::new(
                                DiagnosticCategory::StoreSearch,
                                Severity::Error,
                                format!("Review view drift: {review_id}"),
                            )
                            .with_subject(format!("view:review:{review_id}"))
                            .with_description(format!(
                                "Expected {:?}, persisted {:?}",
                                expected_view, stored_view
                            ))
                            .with_suggestion("Rebuild views from events on startup"),
                        );
                    }
                }
                (Some(expected_view), None) => {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::StoreSearch,
                            Severity::Error,
                            format!("Review view missing: {review_id}"),
                        )
                        .with_subject(format!("view:review:{review_id}"))
                        .with_description(format!(
                            "Expected view {:?} but no persisted view exists",
                            expected_view
                        ))
                        .with_suggestion("Clear stale view cache and rebuild from event log"),
                    );
                }
                (None, Some(stored_view)) => {
                    findings.push(
                        DiagnosticFinding::new(
                            DiagnosticCategory::StoreSearch,
                            Severity::Error,
                            format!("Unexpected review view: {review_id}"),
                        )
                        .with_subject(format!("view:review:{review_id}"))
                        .with_description(format!(
                            "Persisted view exists but no log replay generated it: {:?}",
                            stored_view
                        ))
                        .with_suggestion("Remove stale view entry"),
                    );
                }
                (None, None) => {}
            }
        }

        Ok(findings)
    }

    fn check_tantivy_consistency(
        &self,
        events: &PartitionHandle,
    ) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let mut memory_ids = HashSet::new();
        for result in events.iter() {
            let (key, value) = result?;
            let Some((event, _event_id)) = decode_event_if_log_key(&key, &value) else {
                continue;
            };
            match event.kind {
                EventKind::MemoryCreated { memory_id, .. } => {
                    memory_ids.insert(memory_id);
                }
                EventKind::MemoryDeleted { memory_id } => {
                    memory_ids.remove(&memory_id);
                }
                _ => {}
            }
        }

        if memory_ids.is_empty() {
            return Ok(findings);
        }

        let mut tantivy_ids = HashSet::new();
        let index_root = self.tantivy_path();
        let index_dir = index_root.join("index");

        if !index_root.exists() {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::StoreSearch,
                    Severity::Error,
                    "Tantivy index path missing".to_string(),
                )
                .with_subject(index_root.display().to_string())
                .with_description(
                    "MemoryCreated events exist but Tantivy index directory is not present",
                )
                .with_suggestion("Initialize/rebuild search index"),
            );
            return Ok(findings);
        }

        if !index_dir.exists() {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::StoreSearch,
                    Severity::Error,
                    "Tantivy index shard missing".to_string(),
                )
                .with_subject(index_dir.display().to_string())
                .with_description("Index root exists but required `index` directory is missing"),
            );
            return Ok(findings);
        }

        let directory = tantivy::directory::MmapDirectory::open(&index_dir)
            .map_err(|error| anyhow::anyhow!("Unable to open tantivy index directory: {error}"))?;
        let tantivy_index = tantivy::Index::open(directory)
            .map_err(|error| anyhow::anyhow!("Unable to open tantivy index: {error}"))?;

        let schema = tantivy_index.schema();
        let id_field = schema
            .get_field("id")
            .map_err(|_| anyhow::anyhow!("Missing Tantivy `id` field"))?;
        let reader = tantivy_index
            .reader()
            .map_err(|error| anyhow::anyhow!("Unable to open tantivy reader: {error}"))?;
        let searcher = reader.searcher();
        let query = tantivy::query::AllQuery;
        let limit = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX.saturating_div(2));

        let top_docs = searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(limit))
            .map_err(|error| anyhow::anyhow!("Unable to execute tantivy scan: {error}"))?;

        for (_score, address) in top_docs {
            let doc: TantivyDocument = searcher
                .doc(address)
                .map_err(|error| anyhow::anyhow!("Unable to read tantivy doc: {error}"))?;
            if let Some(id) = doc.get_first(id_field).and_then(|value| value.as_str()) {
                tantivy_ids.insert(id.to_string());
            }
        }

        let missing_from_tantivy: Vec<_> = memory_ids
            .difference(&tantivy_ids)
            .map(std::string::ToString::to_string)
            .collect();
        if !missing_from_tantivy.is_empty() {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::StoreSearch,
                    Severity::Error,
                    "Tantivy index missing memory IDs".to_string(),
                )
                .with_subject(format!("memory_count:{}", missing_from_tantivy.len()))
                .with_description(format!(
                    "The following memories exist in events but are not indexed: {:?}",
                    missing_from_tantivy
                ))
                .with_suggestion("Re-index memory documents"),
            );
        }

        let extra_in_tantivy: Vec<_> = tantivy_ids
            .difference(&memory_ids)
            .map(std::string::ToString::to_string)
            .collect();
        if !extra_in_tantivy.is_empty() {
            findings.push(
                DiagnosticFinding::new(
                    DiagnosticCategory::StoreSearch,
                    Severity::Error,
                    "Tantivy index has stale memory IDs".to_string(),
                )
                .with_subject(format!("index_count:{}", extra_in_tantivy.len()))
                .with_description(format!(
                    "The following indexed memories have no corresponding MemoryCreated event: {:?}",
                    extra_in_tantivy
                ))
                .with_suggestion("Rebuild the index to remove stale documents"),
            );
        }

        Ok(findings)
    }
}

impl Checker for StoreSearchChecker {
    fn category(&self) -> DiagnosticCategory {
        DiagnosticCategory::StoreSearch
    }

    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();
        let events_path = self.events_path();

        if !Self::keyspace_has_data(&events_path) {
            return Ok(findings);
        }

        let keyspace = fjall::Config::new(&events_path).open()?;
        let queue =
            keyspace.open_partition(QUEUE_PARTITION, fjall::PartitionCreateOptions::default())?;
        let events =
            keyspace.open_partition(EVENTS_PARTITION, fjall::PartitionCreateOptions::default())?;
        let views =
            keyspace.open_partition(VIEWS_PARTITION, fjall::PartitionCreateOptions::default())?;

        findings.extend(self.check_queue_lease_consistency(&queue)?);
        findings.extend(self.check_view_drift(&events, &views)?);
        findings.extend(self.check_tantivy_consistency(&events)?);
        Ok(findings)
    }
}

#[derive(Debug)]
struct ClaimedEntry {
    queue_name: String,
    item_id: String,
    claim_id: String,
}

#[derive(Default)]
struct QueueState {
    queue_items: HashSet<(String, String)>,
    queue_item_counts: HashMap<(String, String), usize>,
    claimed_entries: Vec<ClaimedEntry>,
    claimed_keys: HashSet<String>,
    leases: HashMap<String, Option<QueueClaim>>,
    malformed_claimed_values: Vec<String>,
    malformed_lease_values: Vec<String>,
}

fn scan_queue_state(queue: &PartitionHandle) -> Result<QueueState, anyhow::Error> {
    let mut state = QueueState::default();

    let iter = queue.iter();
    for result in iter {
        let (raw_key, raw_value) = result?;
        let key = String::from_utf8_lossy(&raw_key);

        if let Some(queue_name) = parse_queue_key(&key) {
            let item_id = match std::str::from_utf8(&raw_value) {
                Ok(item_id) => item_id.to_string(),
                Err(_) => continue,
            };
            state
                .queue_items
                .insert((queue_name.clone(), item_id.clone()));
            *state
                .queue_item_counts
                .entry((queue_name, item_id))
                .or_default() += 1;
            continue;
        }

        if let Some((queue_name, item_id)) = parse_claimed_key(&key) {
            state
                .claimed_keys
                .insert(format!("claimed:{queue_name}:{item_id}"));
            match String::from_utf8(raw_value.to_vec()) {
                Ok(claim_id) => state.claimed_entries.push(ClaimedEntry {
                    queue_name,
                    item_id,
                    claim_id,
                }),
                Err(_) => state.malformed_claimed_values.push(key.to_string()),
            }
            continue;
        }

        if let Some(claim_id) = parse_lease_key(&key) {
            let claim = match std::str::from_utf8(&raw_value) {
                Ok(claim_json) => match serde_json::from_str::<QueueClaim>(claim_json) {
                    Ok(claim) => Some(claim),
                    Err(_) => {
                        state.malformed_lease_values.push(claim_id.to_string());
                        None
                    }
                },
                Err(_) => {
                    state.malformed_lease_values.push(claim_id.to_string());
                    None
                }
            };
            state.leases.insert(claim_id.to_string(), claim);
            continue;
        }
    }

    Ok(state)
}

fn parse_queue_key(key: &str) -> Option<String> {
    if !key.starts_with("queue:") {
        return None;
    }

    let rest = &key["queue:".len()..];
    let delimiter = rest.rfind(':')?;
    let queue_name = &rest[..delimiter];
    if rest[delimiter + 1..].parse::<u64>().ok().is_none() {
        return None;
    }
    Some(queue_name.to_string())
}

/// Parses a claimed marker key of the form `claimed:<queue_name>:<item_id>`.
///
/// Uses `rfind(':')` so that `queue_name` may itself contain colons (e.g.
/// `"review:high"`), while `item_id` is always the last colon-delimited segment
/// and is expected never to contain colons (holds for `T-xxx` / `REV-xxx` IDs).
fn parse_claimed_key(key: &str) -> Option<(String, String)> {
    let prefix = key.strip_prefix("claimed:")?;
    let delimiter = prefix.rfind(':')?;
    let queue_name = &prefix[..delimiter];
    let item_id = &prefix[delimiter + 1..];
    if queue_name.is_empty() || item_id.is_empty() {
        return None;
    }
    Some((queue_name.to_string(), item_id.to_string()))
}

fn parse_lease_key(key: &str) -> Option<&str> {
    key.strip_prefix("lease:")
}

fn claim_is_expired(claim: &QueueClaim) -> bool {
    claim.expires_at <= Utc::now()
}

fn decode_event(raw: &[u8]) -> Result<(Event, EventId), anyhow::Error> {
    let envelope: EventEnvelope = deserialize_event_envelope(raw)?;
    Ok((envelope.event, envelope.event_id))
}

fn decode_event_if_log_key(key: &[u8], value: &[u8]) -> Option<(Event, EventId)> {
    if !key.starts_with(b"log:") {
        return None;
    }
    decode_event(value).ok()
}

#[derive(Default)]
struct RebuildViewsState {
    task_views: HashMap<String, TaskView>,
    review_views: HashMap<String, ReviewView>,
    review_to_task: HashMap<String, String>,
}

impl RebuildViewsState {
    fn apply(&mut self, event_id: EventId, event: &Event) {
        match &event.kind {
            EventKind::TaskCreated { task_id } => {
                let view = self
                    .task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| default_task_view(task_id));
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::TaskAssigned { task_id, agent_id } => {
                let view = self
                    .task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| default_task_view(task_id));
                view.assignee = Some(agent_id.clone());
                view.status = TaskStatus::Assigned;
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::TaskCompleted { task_id } => {
                let view = self
                    .task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| default_task_view(task_id));
                view.status = TaskStatus::InReview;
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::ReviewRequested { task_id, review_id } => {
                let task_view = self
                    .task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| default_task_view(task_id));
                task_view.status = TaskStatus::InReview;
                task_view.review_rounds = task_view.review_rounds.saturating_add(1);
                task_view.last_event_id = event_id.as_u64();
                task_view.updated_at = event.timestamp;

                let review_view = self
                    .review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| default_review_view(review_id));
                review_view.task_id = task_id.clone();
                review_view.status = ReviewStatus::Pending;
                review_view.round = review_view.round.saturating_add(1);
                review_view.scores.clear();
                review_view.panel.clear();
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;
                self.review_to_task
                    .insert(review_id.clone(), task_id.clone());
            }
            EventKind::ReviewScoreReceived {
                review_id,
                reviewer_id,
                score,
            } => {
                let review_view = self
                    .review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| default_review_view(review_id));
                review_view.status = ReviewStatus::InProgress;
                review_view.scores.push((reviewer_id.clone(), *score));
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;
            }
            EventKind::ReviewApproved { review_id } => {
                let review_view = self
                    .review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| default_review_view(review_id));
                review_view.status = ReviewStatus::Completed;
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;
                if let Some(task_id) = self.review_to_task.get(review_id) {
                    let task_view = self
                        .task_views
                        .entry(task_id.clone())
                        .or_insert_with(|| default_task_view(task_id));
                    task_view.status = TaskStatus::Approved;
                    task_view.last_event_id = event_id.as_u64();
                    task_view.updated_at = event.timestamp;
                }
            }
            EventKind::ReviewRejected { review_id } => {
                let review_view = self
                    .review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| default_review_view(review_id));
                review_view.status = ReviewStatus::Completed;
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;
                if let Some(task_id) = self.review_to_task.get(review_id) {
                    let task_view = self
                        .task_views
                        .entry(task_id.clone())
                        .or_insert_with(|| default_task_view(task_id));
                    task_view.status = TaskStatus::ChangesRequested;
                    task_view.last_event_id = event_id.as_u64();
                    task_view.updated_at = event.timestamp;
                }
            }
            EventKind::ReviewChangesRequested { review_id } => {
                let review_view = self
                    .review_views
                    .entry(review_id.clone())
                    .or_insert_with(|| default_review_view(review_id));
                review_view.status = ReviewStatus::Completed;
                review_view.last_event_id = event_id.as_u64();
                review_view.updated_at = event.timestamp;
                if let Some(task_id) = self.review_to_task.get(review_id) {
                    let task_view = self
                        .task_views
                        .entry(task_id.clone())
                        .or_insert_with(|| default_task_view(task_id));
                    task_view.status = TaskStatus::ChangesRequested;
                    task_view.last_event_id = event_id.as_u64();
                    task_view.updated_at = event.timestamp;
                }
            }
            EventKind::MergePrepared { task_id, branch } => {
                let view = self
                    .task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| default_task_view(task_id));
                view.branch = Some(branch.clone());
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::MergeAborted { task_id, .. } => {
                let view = self
                    .task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| default_task_view(task_id));
                view.status = TaskStatus::InProgress;
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::MergeCommitted { task_id } => {
                let view = self
                    .task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| default_task_view(task_id));
                view.status = TaskStatus::Merged;
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            EventKind::WorkerReassigned {
                task_id,
                new_worker,
                ..
            } => {
                let view = self
                    .task_views
                    .entry(task_id.clone())
                    .or_insert_with(|| default_task_view(task_id));
                view.assignee = Some(new_worker.clone());
                view.last_event_id = event_id.as_u64();
                view.updated_at = event.timestamp;
            }
            _ => {}
        }
    }
}

fn task_views_equivalent(expected: &TaskView, observed: &TaskView) -> bool {
    expected.task_id == observed.task_id
        && expected.status == observed.status
        && expected.assignee == observed.assignee
        && expected.session_id == observed.session_id
        && expected.branch == observed.branch
        && expected.review_rounds == observed.review_rounds
}

fn review_views_equivalent(expected: &ReviewView, observed: &ReviewView) -> bool {
    expected.review_id == observed.review_id
        && expected.task_id == observed.task_id
        && expected.status == observed.status
        && expected.round == observed.round
        && expected.scores == observed.scores
        && expected.panel == observed.panel
}

fn rebuild_views_from_events(
    events: &PartitionHandle,
) -> Result<(HashMap<String, TaskView>, HashMap<String, ReviewView>), anyhow::Error> {
    let mut state = RebuildViewsState::default();

    let iter = events.iter();
    for result in iter {
        let (key, value) = result?;
        let Some((event, event_id)) = decode_event_if_log_key(&key, &value) else {
            continue;
        };
        state.apply(event_id, &event);
    }

    Ok((state.task_views, state.review_views))
}

fn load_persisted_views(
    views: &PartitionHandle,
) -> Result<(HashMap<String, TaskView>, HashMap<String, ReviewView>), anyhow::Error> {
    let mut task_views = HashMap::new();
    let mut review_views = HashMap::new();

    let iter = views.iter();
    for result in iter {
        let (key, value) = result?;
        if let Some(task_id) = key.strip_prefix(b"view:task:") {
            let view: TaskView = serde_json::from_slice(&value)?;
            task_views.insert(String::from_utf8_lossy(task_id).to_string(), view);
            continue;
        }
        if let Some(review_id) = key.strip_prefix(b"view:review:") {
            let view: ReviewView = serde_json::from_slice(&value)?;
            review_views.insert(String::from_utf8_lossy(review_id).to_string(), view);
        }
    }

    Ok((task_views, review_views))
}

fn default_task_view(task_id: &str) -> TaskView {
    TaskView {
        task_id: task_id.to_string(),
        status: TaskStatus::Pending,
        assignee: None,
        session_id: None,
        branch: None,
        review_rounds: 0,
        last_event_id: 0,
        updated_at: Utc::now(),
    }
}

fn default_review_view(review_id: &str) -> ReviewView {
    ReviewView {
        review_id: review_id.to_string(),
        task_id: String::new(),
        status: ReviewStatus::Pending,
        round: 0,
        scores: Vec::new(),
        panel: Vec::new(),
        last_event_id: 0,
        updated_at: Utc::now(),
    }
}

#[cfg(test)]
#[path = "store_search_tests.rs"]
mod store_search_tests;
