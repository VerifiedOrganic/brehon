use super::*;
use crate::server::ContentBlock;
use crate::tools::Tool;
use crate::tools::TEST_ENV_LOCK;
use brehon_ports::{EventStore, PortError};
use brehon_types::{ClaimId, EventFilter, EventId, QueueClaim, ViewUpdate};
use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tempfile::tempdir;

struct ScopedEnv {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnv {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for (key, value) in vars {
            saved.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self { saved }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.saved.iter().rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn text_content(result: &ToolResult) -> &str {
    match &result.content[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("Expected text content"),
    }
}

fn enable_context_compression(project_root: &std::path::Path) {
    let brehon_dir = project_root.join(".brehon");
    std::fs::create_dir_all(&brehon_dir).unwrap();
    std::fs::write(
        brehon_dir.join("config.yaml"),
        "version: 1\ncontext:\n  compression:\n    enabled: true\n",
    )
    .unwrap();
}

#[derive(Default)]
struct TestSearchIndex {
    entries: Mutex<HashMap<String, SearchEntry>>,
    index_calls: AtomicUsize,
    delete_calls: AtomicUsize,
}

#[async_trait]
impl SearchIndex for TestSearchIndex {
    async fn index(&self, entry: SearchEntry) -> Result<(), PortError> {
        self.index_calls.fetch_add(1, Ordering::SeqCst);
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(entry.id.clone(), entry);
        Ok(())
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<brehon_types::SearchResult>, PortError> {
        let query_lower = query.to_lowercase();
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut results = entries
            .values()
            .filter(|entry| entry.content.to_lowercase().contains(&query_lower))
            .take(limit)
            .map(|entry| brehon_types::SearchResult {
                id: entry.id.clone(),
                snippet: truncate_snippet(&entry.content, 240),
                score: 1.0,
                matched_tags: entry.tags.clone(),
            })
            .collect::<Vec<_>>();
        results.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(results)
    }

    async fn delete(&self, id: &str) -> Result<(), PortError> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        Ok(())
    }

    async fn reindex(&self, entries: Vec<SearchEntry>) -> Result<(), PortError> {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.clear();
        for entry in entries {
            map.insert(entry.id.clone(), entry);
        }
        Ok(())
    }
}

#[derive(Default)]
struct TestEventStore {
    events: Mutex<Vec<Event>>,
}

#[async_trait]
impl EventStore for TestEventStore {
    async fn append(&self, event: Event) -> Result<EventId, PortError> {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.push(event);
        Ok(EventId::new(events.len() as u64))
    }

    async fn append_atomic(
        &self,
        events: Vec<Event>,
        _views: Vec<ViewUpdate>,
    ) -> Result<Vec<EventId>, PortError> {
        let mut stored = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let mut ids = Vec::with_capacity(events.len());
        for event in events {
            stored.push(event);
            ids.push(EventId::new(stored.len() as u64));
        }
        Ok(ids)
    }

    async fn append_and_enqueue(
        &self,
        event: Event,
        _queue: &str,
        _item_id: &str,
        _idempotency_key: Option<&str>,
    ) -> Result<EventId, PortError> {
        self.append(event).await
    }

    async fn query(&self, _filter: EventFilter) -> Result<Vec<Event>, PortError> {
        Ok(self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone())
    }

    async fn stream(
        &self,
        _since: Option<EventId>,
        _limit: usize,
    ) -> Result<Vec<(Event, EventId)>, PortError> {
        Ok(self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .enumerate()
            .map(|(idx, event)| (event, EventId::new((idx + 1) as u64)))
            .collect())
    }

    async fn claim_next(
        &self,
        _queue: &str,
        _consumer: &str,
        _lease_for: Duration,
    ) -> Result<Option<QueueClaim>, PortError> {
        Ok(None)
    }

    async fn ack_claim(&self, _claim_id: &ClaimId) -> Result<(), PortError> {
        Ok(())
    }

    async fn renew_claim(
        &self,
        _claim_id: &ClaimId,
        _lease_for: Duration,
    ) -> Result<(), PortError> {
        Ok(())
    }

    async fn high_water_mark(&self) -> Result<EventId, PortError> {
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        Ok(EventId::new(events.len() as u64))
    }

    async fn retain_events(&self, _before: EventId) -> Result<usize, PortError> {
        Ok(0)
    }

    async fn expire_idempotency_keys(&self, _older_than: Duration) -> Result<usize, PortError> {
        Ok(0)
    }
}

#[tokio::test]
async fn test_create_list_search_delete_memory_round_trip() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempdir().unwrap();
    let brehon_root = temp.path().join(".brehon");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_NAME", "deep-ant-51"),
    ]);

    let create = CreateMemoryTool::new();
    let get = GetMemoriesTool::new();
    let list = ListMemoriesTool::new();
    let search = SearchMemoriesTool::new();
    let delete = DeleteMemoryTool::new();

    let create_result = create
            .execute(serde_json::json!({
                "content": "The authentication middleware uses bcrypt because the configuration stores password hashing",
                "tags": ["auth", "security"]
            }))
            .await
            .unwrap();
    let created: CreateMemoryResponse = serde_json::from_str(text_content(&create_result)).unwrap();

    let list_result = list
        .execute(serde_json::json!({ "tag": "auth", "limit": 10 }))
        .await
        .unwrap();
    let listed: ListMemoriesResponse = serde_json::from_str(text_content(&list_result)).unwrap();
    assert_eq!(listed.count, 1);
    assert_eq!(listed.memories[0].id, created.id);
    assert!(listed.memories[0]
        .snippet
        .contains("authentication middleware uses bcrypt because the configuration"));

    let get_result = get
        .execute(serde_json::json!({ "ids": [created.id.clone()] }))
        .await
        .unwrap();
    let fetched: GetMemoriesResponse = serde_json::from_str(text_content(&get_result)).unwrap();
    assert_eq!(fetched.memories.len(), 1);
    assert!(fetched.memories[0]
        .content
        .contains("authentication middleware uses bcrypt because the configuration"));
    assert_eq!(fetched.memories[0].content, created.content);
    assert!(fetched.missing.is_empty());

    let raw_result = get
        .execute(serde_json::json!({
            "ids": [created.id.clone()],
            "representation": "raw"
        }))
        .await
        .unwrap();
    let raw: GetMemoriesResponse = serde_json::from_str(text_content(&raw_result)).unwrap();
    assert_eq!(raw.memories.len(), 1);
    assert_eq!(raw.memories[0].content, created.content);

    let search_result = search
        .execute(serde_json::json!({ "query": "bcrypt", "limit": 5 }))
        .await
        .unwrap();
    let searched: SearchMemoriesResponse =
        serde_json::from_str(text_content(&search_result)).unwrap();
    assert_eq!(searched.results.len(), 1);
    assert_eq!(searched.results[0].id, created.id);

    let delete_result = delete
        .execute(serde_json::json!({ "id": created.id }))
        .await
        .unwrap();
    let deleted: DeleteMemoryResponse = serde_json::from_str(text_content(&delete_result)).unwrap();
    assert_eq!(deleted.status, "deleted");

    let post_delete = list
        .execute(serde_json::json!({ "limit": 10 }))
        .await
        .unwrap();
    let listed_after: ListMemoriesResponse =
        serde_json::from_str(text_content(&post_delete)).unwrap();
    assert_eq!(listed_after.count, 0);
}

#[tokio::test]
async fn test_memory_tools_compact_when_context_compression_enabled() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempdir().unwrap();
    enable_context_compression(temp.path());
    let brehon_root = temp.path().join(".brehon");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_NAME", "deep-ant-51"),
    ]);

    let create = CreateMemoryTool::new();
    let get = GetMemoriesTool::new();
    let list = ListMemoriesTool::new();

    let create_result = create
            .execute(serde_json::json!({
                "content": "The authentication middleware uses bcrypt because the configuration stores password hashing",
                "tags": ["auth", "security"]
            }))
            .await
            .unwrap();
    let created: CreateMemoryResponse = serde_json::from_str(text_content(&create_result)).unwrap();

    let list_result = list
        .execute(serde_json::json!({ "tag": "auth", "limit": 10 }))
        .await
        .unwrap();
    let listed: ListMemoriesResponse = serde_json::from_str(text_content(&list_result)).unwrap();
    assert_eq!(listed.count, 1);
    assert!(listed.memories[0]
        .snippet
        .contains("auth mw uses bcrypt bc config"));

    let get_result = get
        .execute(serde_json::json!({ "ids": [created.id.clone()] }))
        .await
        .unwrap();
    let fetched: GetMemoriesResponse = serde_json::from_str(text_content(&get_result)).unwrap();
    assert_eq!(fetched.memories.len(), 1);
    assert!(fetched.memories[0]
        .content
        .contains("auth mw uses bcrypt bc config"));
    assert_ne!(fetched.memories[0].content, created.content);

    let raw_result = get
        .execute(serde_json::json!({
            "ids": [created.id],
            "representation": "raw"
        }))
        .await
        .unwrap();
    let raw: GetMemoriesResponse = serde_json::from_str(text_content(&raw_result)).unwrap();
    assert_eq!(raw.memories.len(), 1);
    assert_eq!(
            raw.memories[0].content,
            "The authentication middleware uses bcrypt because the configuration stores password hashing"
        );
}

#[tokio::test]
async fn test_memory_tools_use_attached_search_index_and_event_store() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempdir().unwrap();
    let brehon_root = temp.path().join(".brehon");
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.to_str().unwrap()),
        ("BREHON_AGENT_NAME", "deep-ant-51"),
    ]);

    let index = Arc::new(TestSearchIndex::default());
    let events = Arc::new(TestEventStore::default());

    let create = CreateMemoryTool::new()
        .with_search_index(index.clone())
        .with_event_store(events.clone());
    let search = SearchMemoriesTool::new().with_search_index(index.clone());
    let delete = DeleteMemoryTool::new()
        .with_search_index(index.clone())
        .with_event_store(events.clone());

    let create_result = create
        .execute(serde_json::json!({
            "content": "Decision: use fjall for durable event storage",
            "tags": ["decision", "storage"]
        }))
        .await
        .unwrap();
    let created: CreateMemoryResponse = serde_json::from_str(text_content(&create_result)).unwrap();

    assert_eq!(index.index_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        events
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        1
    );

    let search_result = search
        .execute(serde_json::json!({ "query": "fjall", "limit": 5 }))
        .await
        .unwrap();
    let searched: SearchMemoriesResponse =
        serde_json::from_str(text_content(&search_result)).unwrap();
    assert_eq!(searched.results.len(), 1);
    assert_eq!(searched.results[0].id, created.id);

    let delete_result = delete
        .execute(serde_json::json!({ "id": created.id }))
        .await
        .unwrap();
    let deleted: DeleteMemoryResponse = serde_json::from_str(text_content(&delete_result)).unwrap();
    assert_eq!(deleted.status, "deleted");
    assert_eq!(index.delete_calls.load(Ordering::SeqCst), 1);
    let events = events.events.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].kind, EventKind::MemoryCreated { .. }));
    assert!(matches!(events[1].kind, EventKind::MemoryDeleted { .. }));
    assert!(created.freshness.source_event_id.is_some());
    assert!(deleted.freshness.source_event_id.is_some());
}
