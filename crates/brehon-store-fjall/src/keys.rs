//! Key schema for the fjall key-value store.

pub const SCHEMA_VERSION: u64 = 1;

pub const KEY_META_SEQ: &str = "meta:seq";
pub const KEY_META_SCHEMA_VERSION: &str = "meta:schema_version";
pub const KEY_META_INDEX_START_SEQ: &str = "meta:index_start_seq";
pub const KEY_META_VIEWS_LAST_EVENT_ID: &str = "meta:views:last_event_id";

pub fn log_key(seq: u64) -> Vec<u8> {
    format!("log:{:020}", seq).into_bytes()
}

pub fn parse_seq_from_log_key(key: &[u8]) -> Option<u64> {
    let key_str = std::str::from_utf8(key).ok()?;
    if let Some(seq_str) = key_str.strip_prefix("log:") {
        seq_str.parse().ok()
    } else {
        None
    }
}

pub fn agent_index_key(agent_id: &str, seq: u64) -> Vec<u8> {
    format!("index:agent:{}:{:020}", agent_id, seq).into_bytes()
}

pub fn task_index_key(task_id: &str, seq: u64) -> Vec<u8> {
    format!("index:task:{}:{:020}", task_id, seq).into_bytes()
}

pub fn task_index_prefix(task_id: &str) -> Vec<u8> {
    format!("index:task:{}:", task_id).into_bytes()
}

pub fn review_index_key(review_id: &str, seq: u64) -> Vec<u8> {
    format!("index:review:{}:{:020}", review_id, seq).into_bytes()
}

pub fn review_index_prefix(review_id: &str) -> Vec<u8> {
    format!("index:review:{}:", review_id).into_bytes()
}

pub fn task_view_key(task_id: &str) -> Vec<u8> {
    format!("view:task:{}", task_id).into_bytes()
}

pub fn review_view_key(review_id: &str) -> Vec<u8> {
    format!("view:review:{}", review_id).into_bytes()
}

pub fn archive_key(seq: u64) -> Vec<u8> {
    format!("archive:{:020}", seq).into_bytes()
}

pub fn parse_seq_from_archive_key(key: &[u8]) -> Option<u64> {
    let key_str = std::str::from_utf8(key).ok()?;
    if let Some(seq_str) = key_str.strip_prefix("archive:") {
        seq_str.parse().ok()
    } else {
        None
    }
}

pub fn queue_key(queue: &str, seq: u64) -> Vec<u8> {
    format!("queue:{}:{:020}", queue, seq).into_bytes()
}

pub fn queue_prefix(queue: &str) -> Vec<u8> {
    format!("queue:{}:", queue).into_bytes()
}

pub fn parse_seq_from_queue_key(key: &[u8]) -> Option<u64> {
    let key_str = std::str::from_utf8(key).ok()?;
    let parts: Vec<&str> = key_str.split(':').collect();
    if parts.len() >= 3 {
        parts[2].parse().ok()
    } else {
        None
    }
}

pub fn lease_key(claim_id: &str) -> Vec<u8> {
    format!("lease:{}", claim_id).into_bytes()
}

pub fn lease_prefix() -> Vec<u8> {
    b"lease:".to_vec()
}

fn key_escape(value: &str) -> String {
    value.replace('%', "%25").replace(':', "%3A")
}

pub fn run_record_key(run_id: &str) -> Vec<u8> {
    format!("run:{}", key_escape(run_id)).into_bytes()
}

pub fn run_task_index_key(task_id: &str, run_id: &str) -> Vec<u8> {
    format!(
        "index:run:task:{}:{}",
        key_escape(task_id),
        key_escape(run_id)
    )
    .into_bytes()
}

pub fn run_task_index_prefix(task_id: &str) -> Vec<u8> {
    format!("index:run:task:{}:", key_escape(task_id)).into_bytes()
}

pub fn run_session_index_key(session_id: &str, run_id: &str) -> Vec<u8> {
    format!(
        "index:run:session:{}:{}",
        key_escape(session_id),
        key_escape(run_id)
    )
    .into_bytes()
}

pub fn run_owner_index_key(owner: &str, run_id: &str) -> Vec<u8> {
    format!(
        "index:run:owner:{}:{}",
        key_escape(owner),
        key_escape(run_id)
    )
    .into_bytes()
}

pub fn run_active_role_index_key(role: &str, run_id: &str) -> Vec<u8> {
    format!(
        "index:run:active-role:{}:{}",
        key_escape(role),
        key_escape(run_id)
    )
    .into_bytes()
}

pub fn run_active_role_prefix(role: &str) -> Vec<u8> {
    format!("index:run:active-role:{}:", key_escape(role)).into_bytes()
}

pub fn run_active_task_role_key(task_id: &str, role: &str) -> Vec<u8> {
    format!(
        "index:run:active-task-role:{}:{}",
        key_escape(task_id),
        key_escape(role)
    )
    .into_bytes()
}

pub fn proof_bundle_key(proof_bundle_id: &str) -> Vec<u8> {
    format!("proof:bundle:{}", key_escape(proof_bundle_id)).into_bytes()
}

pub fn proof_task_index_key(task_id: &str) -> Vec<u8> {
    format!("index:proof:task:{}", key_escape(task_id)).into_bytes()
}

pub fn proof_run_index_key(run_id: &str) -> Vec<u8> {
    format!("index:proof:run:{}", key_escape(run_id)).into_bytes()
}

pub fn idempotency_key(key: &str) -> Vec<u8> {
    format!("meta:idempotency:{}", key).into_bytes()
}

pub fn queue_materialization_key(key: &str) -> Vec<u8> {
    format!("meta:queue-materialized:{}", key).into_bytes()
}

pub fn agent_index_prefix(agent_id: &str) -> Vec<u8> {
    format!("index:agent:{}:", agent_id).into_bytes()
}

pub fn parse_seq_from_index_key(key: &[u8]) -> Option<u64> {
    let key_str = std::str::from_utf8(key).ok()?;
    let seq_str = key_str.rsplit(':').next()?;
    seq_str.parse().ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Log,
    AgentIndex,
    TaskIndex,
    ReviewIndex,
    TaskView,
    ReviewView,
    Queue,
    Lease,
    RunRecord,
    RunIndex,
    ProofRecord,
    ProofIndex,
    Idempotency,
    QueueMaterialization,
    Meta,
    Unknown,
}

pub fn classify_key(key: &[u8]) -> KeyType {
    let key_str = std::str::from_utf8(key).unwrap_or("");
    if key_str.starts_with("log:") {
        KeyType::Log
    } else if key_str.starts_with("index:agent:") {
        KeyType::AgentIndex
    } else if key_str.starts_with("index:task:") {
        KeyType::TaskIndex
    } else if key_str.starts_with("index:review:") {
        KeyType::ReviewIndex
    } else if key_str.starts_with("view:task:") {
        KeyType::TaskView
    } else if key_str.starts_with("view:review:") {
        KeyType::ReviewView
    } else if key_str.starts_with("queue:") {
        KeyType::Queue
    } else if key_str.starts_with("lease:") {
        KeyType::Lease
    } else if key_str.starts_with("run:") {
        KeyType::RunRecord
    } else if key_str.starts_with("index:run:") {
        KeyType::RunIndex
    } else if key_str.starts_with("proof:bundle:") {
        KeyType::ProofRecord
    } else if key_str.starts_with("index:proof:") {
        KeyType::ProofIndex
    } else if key_str.starts_with("meta:queue-materialized:") {
        KeyType::QueueMaterialization
    } else if key_str.starts_with("meta:idempotency:") {
        KeyType::Idempotency
    } else if key_str.starts_with("meta:") {
        KeyType::Meta
    } else {
        KeyType::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_key_format() {
        let key = log_key(42);
        assert_eq!(key, b"log:00000000000000000042");
        assert_eq!(parse_seq_from_log_key(&key), Some(42));
    }

    #[test]
    fn log_key_ordering() {
        let key1 = log_key(100);
        let key2 = log_key(200);
        assert!(key1 < key2);
    }

    #[test]
    fn task_index_key_format() {
        let key = task_index_key("T001", 42);
        assert_eq!(key, b"index:task:T001:00000000000000000042");
    }

    #[test]
    fn queue_key_format() {
        let key = queue_key("review:high", 42);
        assert!(key.starts_with(b"queue:review:high:"));
    }

    #[test]
    fn classify_keys() {
        assert_eq!(classify_key(&log_key(1)), KeyType::Log);
        assert_eq!(classify_key(&task_index_key("T001", 1)), KeyType::TaskIndex);
        assert_eq!(classify_key(&task_view_key("T001")), KeyType::TaskView);
        assert_eq!(classify_key(&lease_key("c1")), KeyType::Lease);
        assert_eq!(classify_key(&run_record_key("RUN:1")), KeyType::RunRecord);
        assert_eq!(
            classify_key(&run_task_index_key("T001", "RUN:1")),
            KeyType::RunIndex
        );
        assert_eq!(
            classify_key(&proof_bundle_key("proof:T001")),
            KeyType::ProofRecord
        );
        assert_eq!(
            classify_key(&proof_task_index_key("T001")),
            KeyType::ProofIndex
        );
    }
}
