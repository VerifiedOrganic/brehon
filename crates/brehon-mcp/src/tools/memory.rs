//! Memory tools for MCP.
//!
//! Tools for searching, creating, listing, and deleting memories.

use brehon_ports::{EventStore, SearchIndex};
use brehon_types::{Event, EventKind, SearchEntry};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::context_efficiency::{
    compact_text_if_enabled, load_context_tool_options, truncate_snippet, ContextToolOptions,
};
use crate::tools::freshness::ToolFreshness;
use crate::tools::{error_result, text_result, Tool};

const MEMORY_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const MEMORY_LOCK_RETRY: Duration = Duration::from_millis(10);
const MEMORY_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

fn brehon_root_dir() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

fn runtime_dir() -> Result<PathBuf, McpError> {
    let Some(root) = brehon_root_dir() else {
        return Err(McpError::Storage(
            "Could not resolve BREHON_ROOT or current working directory".to_string(),
        ));
    };
    let dir = root.join("runtime");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn memory_file_path() -> Result<PathBuf, McpError> {
    Ok(runtime_dir()?.join("memories.json"))
}

fn memory_lock_path() -> Result<PathBuf, McpError> {
    Ok(runtime_dir()?.join(".memories.lock"))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct MemoryStore {
    memories: Vec<StoredMemoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredMemoryEntry {
    id: String,
    content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compact_content: Option<String>,
    tags: Vec<String>,
    created_at: DateTime<Utc>,
    source_agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_event_id: Option<u64>,
}

struct MemoryLock {
    path: PathBuf,
}

impl Drop for MemoryLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn clear_stale_lock(path: &std::path::Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let Ok(age) = modified.elapsed() else {
        return;
    };
    if age >= MEMORY_LOCK_STALE_AFTER {
        let _ = std::fs::remove_file(path);
    }
}

async fn acquire_memory_lock() -> Result<MemoryLock, McpError> {
    let path = memory_lock_path()?;
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(MemoryLock { path }),
            Err(err)
                if err.kind() == ErrorKind::AlreadyExists
                    && start.elapsed() < MEMORY_LOCK_TIMEOUT =>
            {
                clear_stale_lock(&path);
                tokio::time::sleep(MEMORY_LOCK_RETRY).await;
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                return Err(McpError::Storage(
                    "Timed out waiting for memories lock".to_string(),
                ));
            }
            Err(err) => return Err(McpError::Io(err)),
        }
    }
}

fn load_memory_store() -> Result<MemoryStore, McpError> {
    let path = memory_file_path()?;
    if !path.exists() {
        return Ok(MemoryStore::default());
    }

    let raw = std::fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Ok(MemoryStore::default());
    }

    serde_json::from_str(&raw)
        .map_err(|err| McpError::Storage(format!("Failed to parse memories store: {err}")))
}

fn save_memory_store(store: &MemoryStore) -> Result<(), McpError> {
    let path = memory_file_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let payload =
        serde_json::to_vec_pretty(store).map_err(|e| McpError::Serialization(e.to_string()))?;
    std::fs::write(&tmp_path, payload)?;

    if let Err(err) = std::fs::rename(&tmp_path, &path) {
        #[cfg(windows)]
        {
            if path.exists() {
                let backup_path = path.with_extension(format!("bak-{}", uuid::Uuid::new_v4()));
                std::fs::rename(&path, &backup_path).map_err(|backup_err| {
                    let _ = std::fs::remove_file(&tmp_path);
                    McpError::Storage(format!(
                        "Failed to prepare backup while replacing memory store: {backup_err} (initial rename error: {err})"
                    ))
                })?;

                match std::fs::rename(&tmp_path, &path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&backup_path);
                    }
                    Err(swap_err) => {
                        let _ = std::fs::rename(&backup_path, &path);
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(McpError::Storage(format!(
                            "Failed to replace memory store on Windows: {swap_err} (initial rename error: {err})"
                        )));
                    }
                }
            } else {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(McpError::Io(err));
            }
        }
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(McpError::Io(err));
        }
    }

    Ok(())
}

fn resolve_source_agent() -> String {
    std::env::var("BREHON_AGENT_NAME").unwrap_or_else(|_| "unknown".to_string())
}

async fn memory_freshness(
    event_store: Option<&Arc<dyn EventStore + Send + Sync>>,
    state_source: &str,
    options: &ContextToolOptions,
    truncated: bool,
) -> ToolFreshness {
    let compacted = options.should_compact_memories();
    let Some(store) = event_store else {
        return ToolFreshness::new(None, state_source)
            .compacted(compacted)
            .stale(true)
            .warning("event_store_unavailable");
    };

    match store.high_water_mark().await {
        Ok(event_id) => ToolFreshness::new(Some(event_id.as_u64()), state_source)
            .compacted(compacted)
            .truncated(truncated),
        Err(err) => ToolFreshness::new(None, state_source)
            .compacted(compacted)
            .stale(true)
            .truncated(truncated)
            .warning(format!("event_store_high_water_failed: {err}")),
    }
}

fn compact_memory_content(content: &str, options: &ContextToolOptions) -> Option<String> {
    options
        .should_compact_memories()
        .then(|| compact_text_if_enabled(content, true, options.compression.mode))
}

fn model_memory_content<'a>(
    memory: &'a StoredMemoryEntry,
    options: &ContextToolOptions,
) -> std::borrow::Cow<'a, str> {
    if !options.should_compact_memories() {
        return std::borrow::Cow::Borrowed(&memory.content);
    }

    memory
        .compact_content
        .as_deref()
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| {
            std::borrow::Cow::Owned(compact_text_if_enabled(
                &memory.content,
                true,
                options.compression.mode,
            ))
        })
}

fn local_search(
    memories: &[StoredMemoryEntry],
    query: &str,
    limit: usize,
    options: &ContextToolOptions,
) -> Vec<SearchResult> {
    let query_lower = query.to_lowercase();
    let query_terms: Vec<&str> = query_lower
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .collect();

    let mut scored: Vec<(f32, &StoredMemoryEntry, Vec<String>)> = memories
        .iter()
        .filter_map(|memory| {
            let content_lower = memory.content.to_lowercase();
            let tag_lowers: Vec<String> =
                memory.tags.iter().map(|tag| tag.to_lowercase()).collect();

            let mut score = 0.0f32;
            if !query_lower.is_empty() && content_lower.contains(&query_lower) {
                score += 2.0;
            }

            for term in &query_terms {
                if content_lower.contains(term) {
                    score += 1.0;
                }
                if tag_lowers.iter().any(|tag| tag.contains(term)) {
                    score += 0.5;
                }
            }

            if score <= 0.0 {
                return None;
            }

            let matched_tags = memory
                .tags
                .iter()
                .filter(|tag| {
                    let tag_lower = tag.to_lowercase();
                    query_terms.iter().any(|term| tag_lower.contains(term))
                })
                .cloned()
                .collect::<Vec<_>>();

            Some((score, memory, matched_tags))
        })
        .collect();

    scored.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.1.created_at.cmp(&left.1.created_at))
    });

    scored
        .into_iter()
        .take(limit)
        .map(|(score, memory, matched_tags)| SearchResult {
            id: memory.id.clone(),
            snippet: truncate_snippet(
                &model_memory_content(memory, options),
                options.snippet_chars(),
            ),
            score,
            matched_tags,
        })
        .collect()
}

fn rewrite_index_result_snippets(
    results: &mut [SearchResult],
    options: &ContextToolOptions,
) -> Result<(), McpError> {
    if results.is_empty() {
        return Ok(());
    }

    let ids = results
        .iter()
        .map(|result| result.id.clone())
        .collect::<HashSet<_>>();
    let store = load_memory_store()?;
    for result in results {
        if let Some(memory) = store
            .memories
            .iter()
            .find(|memory| ids.contains(&memory.id) && memory.id == result.id)
        {
            result.snippet = truncate_snippet(
                &model_memory_content(memory, options),
                options.snippet_chars(),
            );
        }
    }
    Ok(())
}

/// MCP tool that searches memories by keyword query and returns ranked results.
pub struct SearchMemoriesTool {
    search_index: Option<Arc<dyn SearchIndex + Send + Sync>>,
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
}

impl Default for SearchMemoriesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchMemoriesTool {
    /// Create a new search memories tool instance.
    pub fn new() -> Self {
        Self {
            search_index: None,
            event_store: None,
        }
    }

    /// Attach a search index backend for full-text memory lookup.
    pub fn with_search_index(mut self, index: Arc<dyn SearchIndex + Send + Sync>) -> Self {
        self.search_index = Some(index);
        self
    }

    /// Attach an event store so responses can report durable revision metadata.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }
}

#[async_trait]
impl Tool for SearchMemoriesTool {
    fn name(&self) -> &str {
        "search_memories"
    }

    fn description(&self) -> &str {
        "Search memories by keyword query. Returns ranked results from the search index."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (keyword or phrase)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return",
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    fn max_argument_bytes(&self) -> Option<usize> {
        Some(64 * 1024)
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let params: SearchMemoriesParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let options = load_context_tool_options();
        let limit = options.search_limit(params.limit);

        let mut state_source = "runtime_memory_file_fallback";
        let mut results = if let Some(index) = self.search_index.as_ref() {
            let index_results = index
                .search(&params.query, limit)
                .await
                .map_err(|err| McpError::Storage(format!("Memory search failed: {err}")))?;

            state_source = "search_index";
            index_results
                .into_iter()
                .map(|result| SearchResult {
                    id: result.id,
                    snippet: result.snippet,
                    score: result.score,
                    matched_tags: result.matched_tags,
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        rewrite_index_result_snippets(&mut results, &options)?;

        if results.is_empty() {
            // Intentionally lock-free read for fallback search: eventual consistency is
            // sufficient here and avoids blocking concurrent create/delete writers.
            let store = load_memory_store()?;
            results = local_search(&store.memories, &params.query, limit, &options);
            state_source = "runtime_memory_file_fallback";
        }
        let freshness =
            memory_freshness(self.event_store.as_ref(), state_source, &options, false).await;

        let result_json = serde_json::to_string_pretty(&SearchMemoriesResponse {
            results,
            query: params.query.clone(),
            freshness,
        })
        .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `search_memories` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchMemoriesParams {
    pub query: String,
    pub limit: Option<usize>,
}

/// A single search hit from a memory query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub snippet: String,
    pub score: f32,
    pub matched_tags: Vec<String>,
}

/// Response payload for the `search_memories` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMemoriesResponse {
    pub results: Vec<SearchResult>,
    pub query: String,
    pub freshness: ToolFreshness,
}

/// MCP tool that fetches full memory bodies by ID after a summary search/list step.
pub struct GetMemoriesTool {
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
}

impl Default for GetMemoriesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GetMemoriesTool {
    /// Create a new get-memories tool instance.
    pub fn new() -> Self {
        Self { event_store: None }
    }

    /// Attach an event store so responses can report durable revision metadata.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }
}

#[async_trait]
impl Tool for GetMemoriesTool {
    fn name(&self) -> &str {
        "get_memories"
    }

    fn description(&self) -> &str {
        "Fetch full memory bodies by ID. Use after search_memories or list_memories returns summaries."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Memory IDs to fetch"
                },
                "representation": {
                    "type": "string",
                    "enum": ["compact", "raw"],
                    "description": "Requested representation. Compact returns model-facing terse text only when context compression is enabled; raw returns stored raw content when available.",
                    "default": "compact"
                }
            },
            "required": ["ids"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let params: GetMemoriesParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let options = load_context_tool_options();
        let max_ids = options.retrieval.max_limit.max(1);
        let ids = params
            .ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .take(max_ids)
            .collect::<Vec<_>>();

        if ids.is_empty() {
            return Ok(error_result("At least one memory ID is required"));
        }

        let representation = params.representation.unwrap_or_default();
        let requested = ids.iter().cloned().collect::<HashSet<_>>();
        let store = load_memory_store()?;
        let mut found_ids = HashSet::new();
        let mut memories = store
            .memories
            .into_iter()
            .filter(|memory| requested.contains(&memory.id))
            .map(|memory| {
                found_ids.insert(memory.id.clone());
                let content = match representation {
                    MemoryRepresentation::Compact => {
                        model_memory_content(&memory, &options).into_owned()
                    }
                    MemoryRepresentation::Raw => memory.content.clone(),
                };
                MemoryDetail {
                    id: memory.id,
                    content,
                    tags: memory.tags,
                    created_at: memory.created_at.to_rfc3339(),
                    source_agent: memory.source_agent,
                }
            })
            .collect::<Vec<_>>();

        let order = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect::<std::collections::HashMap<_, _>>();
        memories.sort_by_key(|memory| order.get(memory.id.as_str()).copied().unwrap_or(max_ids));

        let missing = ids
            .into_iter()
            .filter(|id| !found_ids.contains(id))
            .collect::<Vec<_>>();
        let freshness = memory_freshness(
            self.event_store.as_ref(),
            "runtime_memory_file",
            &options,
            false,
        )
        .await;

        let response = GetMemoriesResponse {
            memories,
            missing,
            representation,
            freshness,
        };

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `get_memories` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct GetMemoriesParams {
    pub ids: Vec<String>,
    pub representation: Option<MemoryRepresentation>,
}

/// Memory body representation requested by `get_memories`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRepresentation {
    /// Compact model-oriented representation.
    Compact,
    /// Raw stored content.
    Raw,
}

impl Default for MemoryRepresentation {
    fn default() -> Self {
        Self::Compact
    }
}

/// A fetched memory body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDetail {
    pub id: String,
    pub content: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub source_agent: String,
}

/// Response payload for the `get_memories` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetMemoriesResponse {
    pub memories: Vec<MemoryDetail>,
    pub missing: Vec<String>,
    pub representation: MemoryRepresentation,
    pub freshness: ToolFreshness,
}

/// MCP tool that creates a new memory entry with content and optional tags.
pub struct CreateMemoryTool {
    search_index: Option<Arc<dyn SearchIndex + Send + Sync>>,
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
}

impl Default for CreateMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl CreateMemoryTool {
    /// Create a new create-memory tool instance.
    pub fn new() -> Self {
        Self {
            search_index: None,
            event_store: None,
        }
    }

    /// Attach a search index backend to index newly created memory entries.
    pub fn with_search_index(mut self, index: Arc<dyn SearchIndex + Send + Sync>) -> Self {
        self.search_index = Some(index);
        self
    }

    /// Attach an event store backend to emit memory-created events.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }
}

#[async_trait]
impl Tool for CreateMemoryTool {
    fn name(&self) -> &str {
        "create_memory"
    }

    fn description(&self) -> &str {
        "Create a new memory entry with content and optional tags. Persists to storage and indexes for search."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Memory content to store"
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tags for categorization"
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let params: CreateMemoryParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let content = params.content.trim().to_string();
        if content.is_empty() {
            return Ok(error_result("Memory content cannot be empty"));
        }

        let tags = params
            .tags
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect::<Vec<_>>();

        let memory_id = uuid::Uuid::new_v4().to_string();
        let created_at = Utc::now();
        let source_agent = resolve_source_agent();
        let options = load_context_tool_options();
        let compact_content = compact_memory_content(&content, &options);
        let stored_content = if options.compression.store_raw {
            content.clone()
        } else {
            compact_content.clone().unwrap_or_else(|| content.clone())
        };
        let source_event_id = if let Some(event_store) = self.event_store.as_ref() {
            let event = Event {
                kind: EventKind::MemoryCreated {
                    memory_id: memory_id.clone(),
                    content: stored_content.clone(),
                    tags: tags.clone(),
                    source_agent: Some(source_agent.clone()),
                },
                timestamp: created_at,
                aggregate_id: memory_id.clone(),
            };
            Some(event_store.append(event).await.map_err(|err| {
                McpError::Storage(format!(
                    "Failed to append memory-created event for {memory_id}: {err}"
                ))
            })?)
            .map(|event_id| event_id.as_u64())
        } else {
            None
        };
        let memory = StoredMemoryEntry {
            id: memory_id.clone(),
            content: stored_content,
            compact_content: if options.compression.store_raw {
                compact_content
            } else {
                None
            },
            tags: tags.clone(),
            created_at,
            source_agent: source_agent.clone(),
            source_event_id,
        };

        {
            let _memory_lock = acquire_memory_lock().await?;
            let mut store = load_memory_store()?;
            store.memories.push(memory.clone());
            let max_memories = options.max_memories.max(1);
            if store.memories.len() > max_memories {
                let excess = store.memories.len() - max_memories;
                store.memories.drain(0..excess);
            }
            save_memory_store(&store)?;
        }

        if let Some(index) = self.search_index.as_ref() {
            if let Err(err) = index
                .index(SearchEntry {
                    id: memory.id.clone(),
                    content: memory.content.clone(),
                    tags: memory.tags.clone(),
                    source: source_agent.clone(),
                    timestamp: created_at,
                })
                .await
            {
                let _ = delete_memory_by_id(&memory_id).await;
                return Err(McpError::Storage(format!(
                    "Failed to index memory {}: {}",
                    memory_id, err
                )));
            }
        }

        let freshness = memory_freshness(
            self.event_store.as_ref(),
            "event_store+runtime_memory_file",
            &options,
            false,
        )
        .await;
        let response = CreateMemoryResponse {
            id: memory_id,
            content,
            tags,
            status: "created".to_string(),
            freshness,
        };

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `create_memory` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateMemoryParams {
    pub content: String,
    pub tags: Option<Vec<String>>,
}

/// Response payload for the `create_memory` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMemoryResponse {
    pub id: String,
    pub content: String,
    pub tags: Vec<String>,
    pub status: String,
    pub freshness: ToolFreshness,
}

/// MCP tool that lists memories with optional tag and time-range filters.
pub struct ListMemoriesTool {
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
}

impl Default for ListMemoriesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ListMemoriesTool {
    /// Create a new list-memories tool instance.
    pub fn new() -> Self {
        Self { event_store: None }
    }

    /// Attach an event store so responses can report durable revision metadata.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }
}

#[async_trait]
impl Tool for ListMemoriesTool {
    fn name(&self) -> &str {
        "list_memories"
    }

    fn description(&self) -> &str {
        "List memories, optionally filtered by tag or time range."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tag": {
                    "type": "string",
                    "description": "Filter by tag"
                },
                "since": {
                    "type": "string",
                    "description": "ISO 8601 timestamp for minimum creation time"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results",
                    "default": 5
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let params: ListMemoriesParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let options = load_context_tool_options();
        let limit = options.search_limit(params.limit);
        let since = params
            .since
            .as_deref()
            .map(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map(|parsed| parsed.with_timezone(&Utc))
                    .map_err(|err| {
                        McpError::InvalidRequest(format!(
                            "Invalid 'since' timestamp '{}': {}",
                            value, err
                        ))
                    })
            })
            .transpose()?;

        let tag_filter = params.tag.as_deref().map(str::to_lowercase);
        let mut memories = load_memory_store()?.memories;
        memories.sort_by(|left, right| right.created_at.cmp(&left.created_at));

        let memories = memories
            .into_iter()
            .filter(|memory| {
                let tag_matches = tag_filter.as_ref().map_or(true, |filter| {
                    memory.tags.iter().any(|tag| tag.to_lowercase() == *filter)
                });
                let since_matches = since.map_or(true, |since| memory.created_at >= since);
                tag_matches && since_matches
            })
            .take(limit)
            .map(|memory| {
                let snippet = truncate_snippet(
                    &model_memory_content(&memory, &options),
                    options.snippet_chars(),
                );
                MemoryEntry {
                    id: memory.id,
                    snippet,
                    tags: memory.tags,
                    created_at: memory.created_at.to_rfc3339(),
                    source_agent: memory.source_agent,
                }
            })
            .collect::<Vec<_>>();
        let freshness = memory_freshness(
            self.event_store.as_ref(),
            "runtime_memory_file",
            &options,
            false,
        )
        .await;

        let response = ListMemoriesResponse {
            count: memories.len(),
            filter: params.tag.clone(),
            memories,
            freshness,
        };

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `list_memories` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct ListMemoriesParams {
    pub tag: Option<String>,
    pub since: Option<String>,
    pub limit: Option<usize>,
}

/// A single memory summary with a bounded snippet, tags, and creation timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub snippet: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub source_agent: String,
}

/// Response payload for the `list_memories` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListMemoriesResponse {
    pub memories: Vec<MemoryEntry>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    pub freshness: ToolFreshness,
}

/// MCP tool that deletes a memory by its ID.
pub struct DeleteMemoryTool {
    search_index: Option<Arc<dyn SearchIndex + Send + Sync>>,
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
}

impl Default for DeleteMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DeleteMemoryTool {
    /// Create a new delete-memory tool instance.
    pub fn new() -> Self {
        Self {
            search_index: None,
            event_store: None,
        }
    }

    /// Attach a search index backend to remove deleted memory entries from search.
    pub fn with_search_index(mut self, index: Arc<dyn SearchIndex + Send + Sync>) -> Self {
        self.search_index = Some(index);
        self
    }

    /// Attach an event store backend to emit memory-deleted events.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }
}

#[async_trait]
impl Tool for DeleteMemoryTool {
    fn name(&self) -> &str {
        "delete_memory"
    }

    fn description(&self) -> &str {
        "Delete a memory by its ID. Removes from storage and search index."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Memory ID to delete"
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let params: DeleteMemoryParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let memory_id = params.id.trim();
        if memory_id.is_empty() {
            return Ok(error_result("Memory ID cannot be empty"));
        }

        let deleted = {
            let _memory_lock = acquire_memory_lock().await?;
            let mut store = load_memory_store()?;
            let before = store.memories.len();
            store.memories.retain(|memory| memory.id != memory_id);
            let deleted = before != store.memories.len();
            if deleted {
                save_memory_store(&store)?;
            }
            deleted
        };

        if let Some(index) = self.search_index.as_ref() {
            if let Err(err) = index.delete(memory_id).await {
                warn!(
                    "failed to delete memory {} from search index after storage delete: {}",
                    memory_id, err
                );
            }
        }
        if deleted {
            if let Some(event_store) = self.event_store.as_ref() {
                event_store
                    .append(Event {
                        kind: EventKind::MemoryDeleted {
                            memory_id: memory_id.to_string(),
                        },
                        timestamp: Utc::now(),
                        aggregate_id: memory_id.to_string(),
                    })
                    .await
                    .map_err(|err| {
                        McpError::Storage(format!(
                            "Failed to append memory-deleted event for {memory_id}: {err}"
                        ))
                    })?;
            }
        }
        let options = load_context_tool_options();
        let freshness = memory_freshness(
            self.event_store.as_ref(),
            "event_store+runtime_memory_file",
            &options,
            false,
        )
        .await;

        let response = DeleteMemoryResponse {
            id: memory_id.to_string(),
            status: if deleted { "deleted" } else { "not_found" }.to_string(),
            freshness,
        };

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `delete_memory` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteMemoryParams {
    pub id: String,
}

/// Response payload for the `delete_memory` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteMemoryResponse {
    pub id: String,
    pub status: String,
    pub freshness: ToolFreshness,
}

async fn delete_memory_by_id(memory_id: &str) -> Result<(), McpError> {
    let _memory_lock = acquire_memory_lock().await?;
    let mut store = load_memory_store()?;
    store.memories.retain(|memory| memory.id != memory_id);
    save_memory_store(&store)
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod memory_tests;
