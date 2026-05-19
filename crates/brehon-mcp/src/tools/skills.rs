//! Skills tools for MCP.
//!
//! Searches builtin workflow skills (supervisor planning, worker execution)
//! and returns results filtered by the caller's role.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::builtins;
use crate::error::McpError;
use crate::tools::{text_result, Tool};

/// MCP tool that searches builtin workflow skills filtered by the caller's role.
pub struct SearchSkillsTool;

impl Default for SearchSkillsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchSkillsTool {
    /// Create a new search-skills tool instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SearchSkillsTool {
    fn name(&self) -> &str {
        "search_skills"
    }

    fn description(&self) -> &str {
        "Search Brehon workflow skills by keyword query. Builtin skill names are \
         brehon-* namespaced. Returns role-appropriate skills \
         for planning, coordination, execution, and review. Supervisors see planning \
         and orchestration skills; workers see execution skills."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query for finding relevant skills. Use empty string to list all available skills for your role."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tags to filter by (e.g. 'planning', 'execution', 'brainstorming')"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results",
                    "default": 20
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value) -> Result<crate::server::ToolResult, McpError> {
        let params: SearchSkillsParams = serde_json::from_value(args.clone())
            .map_err(|e| McpError::InvalidRequest(format!("Invalid arguments: {}", e)))?;

        let limit = params.limit.unwrap_or(20).min(100);
        let caller_role = std::env::var("BREHON_AGENT_ROLE").unwrap_or_default();

        let tag_refs = params.tags.as_deref();
        let results = builtins::search_skills(&params.query, &caller_role, tag_refs);

        let skills: Vec<SkillResult> = results
            .into_iter()
            .take(limit)
            .map(|s| SkillResult {
                id: s.name.clone(),
                name: s.name,
                description: s.description,
                content: s.content,
                tags: s.tags,
                source: "builtin".to_string(),
            })
            .collect();

        let response = SearchSkillsResponse {
            skills,
            query: params.query.clone(),
            tags: params.tags,
        };

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

/// Input parameters for the `search_skills` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchSkillsParams {
    pub query: String,
    pub tags: Option<Vec<String>>,
    pub limit: Option<usize>,
}

/// A single skill returned from a search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResult {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source: String,
}

/// Response payload for the `search_skills` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSkillsResponse {
    pub skills: Vec<SkillResult>,
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ContentBlock;
    use crate::tools::TEST_ENV_LOCK;
    use std::ffi::OsString;

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

    #[tokio::test]
    async fn test_search_skills_returns_builtins() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[("BREHON_AGENT_ROLE", "supervisor")]);
        let tool = SearchSkillsTool::new();

        let args = serde_json::json!({
            "query": "supervisor"
        });

        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());

        if let ContentBlock::Text { text } = &result.content[0] {
            let response: SearchSkillsResponse = serde_json::from_str(text).unwrap();
            assert!(
                !response.skills.is_empty(),
                "Should find skills matching 'supervisor'"
            );
            assert!(
                response.skills.iter().any(|s| s.name == "brehon-supervisor"),
                "Should include brehon-supervisor skill"
            );
        }
    }

    #[tokio::test]
    async fn test_search_skills_with_tags() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[("BREHON_AGENT_ROLE", "supervisor")]);
        let tool = SearchSkillsTool::new();

        let args = serde_json::json!({
            "query": "",
            "tags": ["brainstorming"],
            "limit": 10
        });

        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());

        if let ContentBlock::Text { text } = &result.content[0] {
            let response: SearchSkillsResponse = serde_json::from_str(text).unwrap();
            assert!(response.tags.is_some());
            assert!(
                response.skills.iter().any(|s| s.name == "brehon-discovery"),
                "Tag search for 'brainstorming' should find brehon-discovery"
            );
        }
    }

    #[tokio::test]
    async fn test_search_skills_empty_query_lists_all() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(&[("BREHON_AGENT_ROLE", "")]);
        let tool = SearchSkillsTool::new();

        let args = serde_json::json!({
            "query": ""
        });

        let result = tool.execute(args).await.unwrap();
        assert!(result.is_error.is_none());

        if let ContentBlock::Text { text } = &result.content[0] {
            let response: SearchSkillsResponse = serde_json::from_str(text).unwrap();
            assert!(
                response.skills.len() == 6,
                "Empty query should return all 6 skills (got {})",
                response.skills.len()
            );
        }
    }

    #[tokio::test]
    async fn test_search_skills_missing_query() {
        let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tool = SearchSkillsTool::new();

        let args = serde_json::json!({
            "limit": 10
        });

        let result = tool.execute(args).await;
        assert!(result.is_err());
    }
}
