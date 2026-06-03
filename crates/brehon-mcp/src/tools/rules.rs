//! Rules tools for MCP.
//!
//! Tools for searching and creating durable project coding rules/conventions.

use async_trait::async_trait;
use brehon_types::config::ContextCompressionTarget;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::context_efficiency::{
    compact_text_with_config, load_context_tool_options, truncate_snippet, ContextToolOptions,
};
use crate::tools::{error_result, text_result, Tool};

const RULE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const RULE_LOCK_RETRY: Duration = Duration::from_millis(10);
const RULE_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RULES: usize = 10_000;

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

fn rules_file_path() -> Result<PathBuf, McpError> {
    Ok(runtime_dir()?.join("rules.json"))
}

fn rules_lock_path() -> Result<PathBuf, McpError> {
    Ok(runtime_dir()?.join(".rules.lock"))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct RuleStore {
    rules: Vec<StoredRuleEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRuleEntry {
    id: String,
    name: String,
    content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compact_content: Option<String>,
    category: String,
    tags: Vec<String>,
    created_at: DateTime<Utc>,
    source_agent: String,
}

struct RuleLock {
    path: PathBuf,
}

impl Drop for RuleLock {
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
    if age >= RULE_LOCK_STALE_AFTER {
        let _ = std::fs::remove_file(path);
    }
}

async fn acquire_rule_lock() -> Result<RuleLock, McpError> {
    let path = rules_lock_path()?;
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(RuleLock { path }),
            Err(err)
                if err.kind() == ErrorKind::AlreadyExists
                    && start.elapsed() < RULE_LOCK_TIMEOUT =>
            {
                clear_stale_lock(&path);
                tokio::time::sleep(RULE_LOCK_RETRY).await;
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                return Err(McpError::Storage(
                    "Timed out waiting for rules lock".to_string(),
                ));
            }
            Err(err) => return Err(McpError::Io(err)),
        }
    }
}

fn load_rule_store() -> Result<RuleStore, McpError> {
    let path = rules_file_path()?;
    if !path.exists() {
        return Ok(RuleStore::default());
    }

    let raw = std::fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Ok(RuleStore::default());
    }

    serde_json::from_str(&raw)
        .map_err(|err| McpError::Storage(format!("Failed to parse rules store: {err}")))
}

fn save_rule_store(store: &RuleStore) -> Result<(), McpError> {
    let path = rules_file_path()?;
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
                        "Failed to prepare backup while replacing rules store: {backup_err} (initial rename error: {err})"
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
                            "Failed to replace rules store on Windows: {swap_err} (initial rename error: {err})"
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

fn default_rule_name(content: &str) -> String {
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("New Rule");
    truncate_snippet(first_line.trim(), 64)
}

fn compact_rule_content(content: &str, options: &ContextToolOptions) -> Option<String> {
    options.should_compact_rules().then(|| {
        compact_text_with_config(
            content,
            &options.compression,
            ContextCompressionTarget::Rule,
        )
    })
}

fn model_rule_content<'a>(
    rule: &'a StoredRuleEntry,
    options: &ContextToolOptions,
) -> std::borrow::Cow<'a, str> {
    if !options.should_compact_rules() {
        return std::borrow::Cow::Borrowed(&rule.content);
    }

    rule.compact_content
        .as_deref()
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| {
            std::borrow::Cow::Owned(compact_text_with_config(
                &rule.content,
                &options.compression,
                ContextCompressionTarget::Rule,
            ))
        })
}

fn local_search(
    rules: &[StoredRuleEntry],
    query: &str,
    category: Option<&str>,
    limit: usize,
    options: &ContextToolOptions,
) -> Vec<RuleResult> {
    let query_lower = query.trim().to_lowercase();
    let query_terms = query_lower
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    let category_lower = category.map(|value| value.trim().to_lowercase());

    let mut scored = rules
        .iter()
        .filter_map(|rule| {
            if let Some(category) = category_lower.as_ref() {
                if rule.category.to_lowercase() != *category {
                    return None;
                }
            }

            let name_lower = rule.name.to_lowercase();
            let content_lower = rule.content.to_lowercase();
            let category_lower = rule.category.to_lowercase();
            let tag_lowers = rule
                .tags
                .iter()
                .map(|tag| tag.to_lowercase())
                .collect::<Vec<_>>();

            let mut score = 0.0f32;
            if query_lower.is_empty() {
                score = 1.0;
            } else {
                if name_lower.contains(&query_lower) {
                    score += 3.0;
                }
                if content_lower.contains(&query_lower) {
                    score += 2.0;
                }
                if category_lower.contains(&query_lower) {
                    score += 1.0;
                }
                for term in &query_terms {
                    if name_lower.contains(term) {
                        score += 1.0;
                    }
                    if content_lower.contains(term) {
                        score += 1.0;
                    }
                    if category_lower.contains(term) {
                        score += 0.5;
                    }
                    if tag_lowers.iter().any(|tag| tag.contains(term)) {
                        score += 0.5;
                    }
                }
            }

            (score > 0.0).then_some((score, rule))
        })
        .collect::<Vec<_>>();

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
        .map(|(score, rule)| {
            let content = model_rule_content(rule, options).into_owned();
            RuleResult {
                id: rule.id.clone(),
                name: rule.name.clone(),
                description: truncate_snippet(&content, options.snippet_chars()),
                content,
                category: rule.category.clone(),
                tags: rule.tags.clone(),
                score,
                created_at: rule.created_at.to_rfc3339(),
                source_agent: rule.source_agent.clone(),
            }
        })
        .collect()
}

/// MCP tool that searches project coding rules and conventions by keyword.
pub struct SearchRulesTool;

impl Default for SearchRulesTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchRulesTool {
    /// Create a new search-rules tool instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SearchRulesTool {
    fn name(&self) -> &str {
        "search_rules"
    }

    fn description(&self) -> &str {
        "Search durable project coding rules and conventions by keyword query."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query for finding relevant rules. Empty returns recent rules."
                },
                "category": {
                    "type": "string",
                    "description": "Optional category filter (e.g., 'style', 'security', 'testing')"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results",
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let params: SearchRulesParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let options = load_context_tool_options();
        let limit = options.search_limit(params.limit);
        let store = load_rule_store()?;
        let rules = local_search(
            &store.rules,
            &params.query,
            params.category.as_deref(),
            limit,
            &options,
        );

        let response = SearchRulesResponse {
            rules,
            query: params.query.clone(),
            category: params.category,
        };

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `search_rules` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchRulesParams {
    pub query: String,
    pub category: Option<String>,
    pub limit: Option<usize>,
}

/// A single rule returned from a search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleResult {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
    pub score: f32,
    pub created_at: String,
    pub source_agent: String,
}

/// Response payload for the `search_rules` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRulesResponse {
    pub rules: Vec<RuleResult>,
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// MCP tool that creates a new coding rule or convention for the project.
pub struct CreateRuleTool;

impl Default for CreateRuleTool {
    fn default() -> Self {
        Self::new()
    }
}

impl CreateRuleTool {
    /// Create a new create-rule tool instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for CreateRuleTool {
    fn name(&self) -> &str {
        "create_rule"
    }

    fn description(&self) -> &str {
        "Create a new durable coding rule or convention for the project."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Rule content/description"
                },
                "category": {
                    "type": "string",
                    "description": "Rule category (e.g., 'style', 'security', 'testing')"
                },
                "name": {
                    "type": "string",
                    "description": "Optional name for the rule"
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
        let params: CreateRuleParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let content = params.content.trim().to_string();
        if content.is_empty() {
            return Ok(error_result("Rule content cannot be empty"));
        }

        let name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_rule_name(&content));
        let category = params
            .category
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "general".to_string());
        let tags = params
            .tags
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect::<Vec<_>>();

        let rule_id = uuid::Uuid::new_v4().to_string();
        let created_at = Utc::now();
        let source_agent = resolve_source_agent();
        let options = load_context_tool_options();
        let compact_content = compact_rule_content(&content, &options);
        let stored_content = if options.compression.store_raw {
            content.clone()
        } else {
            compact_content.clone().unwrap_or_else(|| content.clone())
        };
        let rule = StoredRuleEntry {
            id: rule_id.clone(),
            name: name.clone(),
            content: stored_content,
            compact_content: if options.compression.store_raw {
                compact_content
            } else {
                None
            },
            category: category.clone(),
            tags: tags.clone(),
            created_at,
            source_agent: source_agent.clone(),
        };

        {
            let _rule_lock = acquire_rule_lock().await?;
            let mut store = load_rule_store()?;
            store.rules.push(rule);
            if store.rules.len() > DEFAULT_MAX_RULES {
                let excess = store.rules.len() - DEFAULT_MAX_RULES;
                store.rules.drain(0..excess);
            }
            save_rule_store(&store)?;
        }

        let response = CreateRuleResponse {
            id: rule_id,
            name,
            content,
            category,
            tags,
            status: "created".to_string(),
            created_at: created_at.to_rfc3339(),
            source_agent,
        };

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `create_rule` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateRuleParams {
    pub content: String,
    pub category: Option<String>,
    pub name: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// Response payload for the `create_rule` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRuleResponse {
    pub id: String,
    pub name: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
    pub status: String,
    pub created_at: String,
    pub source_agent: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ContentBlock;
    use crate::tools::TEST_ENV_LOCK;
    use std::ffi::OsString;
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

    #[tokio::test]
    async fn test_create_search_rules_round_trip() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("BREHON_AGENT_NAME", "reviewer-1"),
        ]);

        let create = CreateRuleTool::new();
        let search = SearchRulesTool::new();

        let create_result = create
            .execute(serde_json::json!({
                "content": "The authentication middleware should validate the configuration before returning the response.",
                "category": "style",
                "name": "Auth Middleware",
                "tags": ["auth", "rust"]
            }))
            .await
            .unwrap();
        let created: CreateRuleResponse =
            serde_json::from_str(text_content(&create_result)).unwrap();
        assert_eq!(created.status, "created");
        assert_eq!(created.source_agent, "reviewer-1");

        let search_result = search
            .execute(serde_json::json!({
                "query": "configuration",
                "category": "style",
                "limit": 5
            }))
            .await
            .unwrap();
        let response: SearchRulesResponse =
            serde_json::from_str(text_content(&search_result)).unwrap();
        assert_eq!(response.rules.len(), 1);
        assert_eq!(response.rules[0].id, created.id);
        assert_eq!(response.rules[0].name, "Auth Middleware");
        assert!(response.rules[0]
            .content
            .contains("authentication middleware should validate the configuration"));
    }

    #[tokio::test]
    async fn test_rules_compact_when_context_compression_enabled() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().unwrap();
        enable_context_compression(temp.path());
        let brehon_root = temp.path().join(".brehon");
        let _env = ScopedEnv::set(&[
            ("BREHON_ROOT", brehon_root.to_str().unwrap()),
            ("BREHON_AGENT_NAME", "reviewer-1"),
        ]);

        let create = CreateRuleTool::new();
        let search = SearchRulesTool::new();

        let create_result = create
            .execute(serde_json::json!({
                "content": "The authentication middleware should validate the configuration before returning the response.",
                "category": "style",
                "name": "Auth Middleware",
                "tags": ["auth", "rust"]
            }))
            .await
            .unwrap();
        let created: CreateRuleResponse =
            serde_json::from_str(text_content(&create_result)).unwrap();

        let search_result = search
            .execute(serde_json::json!({
                "query": "configuration",
                "category": "style",
                "limit": 5
            }))
            .await
            .unwrap();
        let response: SearchRulesResponse =
            serde_json::from_str(text_content(&search_result)).unwrap();
        assert_eq!(response.rules.len(), 1);
        assert_eq!(response.rules[0].id, created.id);
        assert!(response.rules[0]
            .content
            .contains("auth mw should validate config pre returning resp"));
    }

    #[tokio::test]
    async fn test_search_rules_empty_query_returns_recent_rules() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

        let create = CreateRuleTool::new();
        let search = SearchRulesTool::new();

        create
            .execute(serde_json::json!({
                "content": "Run focused tests before reporting task ready",
                "category": "testing"
            }))
            .await
            .unwrap();

        let result = search
            .execute(serde_json::json!({
                "query": "",
                "limit": 20
            }))
            .await
            .unwrap();
        let response: SearchRulesResponse = serde_json::from_str(text_content(&result)).unwrap();
        assert_eq!(response.rules.len(), 1);
        assert_eq!(response.rules[0].category, "testing");
    }

    #[tokio::test]
    async fn test_create_rule_empty_content() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);
        let tool = CreateRuleTool::new();

        let result = tool
            .execute(serde_json::json!({
                "content": ""
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }
}
