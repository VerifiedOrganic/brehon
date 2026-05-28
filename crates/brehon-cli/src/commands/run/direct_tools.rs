use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use brehon_acp::{DirectToolBridge, DirectToolBridgeFactory};
use brehon_mcp::server::{ContentBlock, ToolResult};
use brehon_mcp::McpServer;
use serde_json::{json, Value};
use tokio::sync::Mutex;

static TOOL_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(crate) struct BrehonDirectToolBridgeFactory;

impl BrehonDirectToolBridgeFactory {
    // Returns Arc<dyn _> so callers can store the factory as a trait object
    // without naming the concrete type.
    #[allow(clippy::new_ret_no_self)]
    pub(crate) fn new() -> Arc<dyn DirectToolBridgeFactory> {
        Arc::new(Self)
    }
}

impl DirectToolBridgeFactory for BrehonDirectToolBridgeFactory {
    fn build(
        &self,
        _worktree_path: &str,
        env: &[(String, String)],
        tool_prefix: Option<&str>,
    ) -> Arc<dyn DirectToolBridge> {
        BrehonMcpToolBridge::new(env.to_vec(), tool_prefix.unwrap_or("mcp_brehon_"))
    }
}

struct BrehonMcpToolBridge {
    server: McpServer,
    env: Vec<(String, String)>,
    tool_prefix: String,
}

impl BrehonMcpToolBridge {
    // Returns Arc<dyn _> for the same reason as BrehonDirectToolBridgeFactory::new.
    #[allow(clippy::new_ret_no_self)]
    fn new(env: Vec<(String, String)>, tool_prefix: &str) -> Arc<dyn DirectToolBridge> {
        let env = with_derived_env(env);
        let mut server = attach_durable_backing(
            McpServer::new("brehon-direct-tools", env!("CARGO_PKG_VERSION")),
            &env,
        );
        server.register_builtin_tools();

        Arc::new(Self {
            server,
            env,
            tool_prefix: tool_prefix.to_string(),
        })
    }
}

fn attach_durable_backing(mut server: McpServer, env: &[(String, String)]) -> McpServer {
    let Some(project_root) = env_path(env, "BREHON_PROJECT_ROOT") else {
        return server;
    };
    let config = match brehon_config::load_config(Some(&project_root)) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(error = %err, "Direct MCP tools running without durable config backing");
            return server;
        }
    };
    let db_path = resolve_state_path(&project_root, &config.context.db_path);
    match brehon_store_fjall::FjallEventStore::new(&db_path) {
        Ok(store) => {
            let store = Arc::new(store);
            let proof_store_available = store.proof_store_available();
            server = server
                .with_event_store(store.clone())
                .with_run_store(store.clone());
            if proof_store_available {
                server = server.with_proof_store(store);
            } else {
                tracing::warn!(path = %db_path.display(), "Direct MCP tools running without durable proof projection");
            }
        }
        Err(err) => {
            tracing::warn!(path = %db_path.display(), error = %err, "Direct MCP tools running without fjall backing");
        }
    }
    let index_path = resolve_state_path(&project_root, &config.context.search_index_path);
    if index_path.join("index").exists() {
        match brehon_search_tantivy::TantivySearchIndex::load_existing(&index_path) {
            Ok(index) => server = server.with_search_index(Arc::new(index)),
            Err(err) => {
                tracing::warn!(path = %index_path.display(), error = %err, "Direct MCP tools running without Tantivy search backing");
            }
        }
    }
    server
}

fn resolve_state_path(project_root: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

fn env_path(env: &[(String, String)], key: &str) -> Option<PathBuf> {
    env.iter()
        .find_map(|(candidate, value)| (candidate == key).then(|| PathBuf::from(value)))
        .filter(|path| !path.as_os_str().is_empty())
}

#[async_trait]
impl DirectToolBridge for BrehonMcpToolBridge {
    fn tool_definitions(&self) -> Vec<Value> {
        self.server
            .tool_definitions()
            .into_iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": format!("{}{}", self.tool_prefix, tool.name),
                        "description": format!("Brehon coordination tool: {}", tool.description),
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect()
    }

    async fn invoke(&self, name: &str, args: Value) -> Result<String, String> {
        let Some(tool_name) = name.strip_prefix(&self.tool_prefix) else {
            return Err(format!("unsupported tool: {name}"));
        };

        let _guard = TOOL_ENV_LOCK.lock().await;
        let mut previous = Vec::with_capacity(self.env.len());
        for (key, value) in &self.env {
            previous.push((key.clone(), std::env::var(key).ok()));
            std::env::set_var(key, value);
        }

        let result = self.server.call_tool(tool_name, args).await;

        for (key, value) in previous {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        result
            .map(tool_result_to_string)
            .map_err(|err| err.to_string())
    }
}

fn with_derived_env(mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    let brehon_root = env
        .iter()
        .find_map(|(key, value)| (key == "BREHON_ROOT").then(|| PathBuf::from(value)));
    let has_project_root = env.iter().any(|(key, _)| key == "BREHON_PROJECT_ROOT");
    let has_workspace_root = env.iter().any(|(key, _)| key == "BREHON_WORKSPACE_ROOT");

    if let Some(root) = brehon_root {
        let project_root = if root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
            root.parent().map(Path::to_path_buf)
        } else {
            Some(root.clone())
        };

        if !has_project_root {
            if let Some(project_root) = project_root.as_ref() {
                env.push((
                    "BREHON_PROJECT_ROOT".to_string(),
                    project_root.to_string_lossy().to_string(),
                ));
            }
        }
        if !has_workspace_root {
            if let Some(project_root) = project_root {
                env.push((
                    "BREHON_WORKSPACE_ROOT".to_string(),
                    project_root.to_string_lossy().to_string(),
                ));
            }
        }
    }

    env
}

fn tool_result_to_string(result: ToolResult) -> String {
    let mut blocks = Vec::new();
    for block in result.content {
        match block {
            ContentBlock::Text { text } => blocks.push(text),
            ContentBlock::Image { mime_type, .. } => {
                blocks.push(format!("[image output omitted: {mime_type}]"))
            }
        }
    }
    let mut text = blocks.join("\n\n");
    if result.is_error == Some(true) && !text.starts_with("ERROR:") {
        text = format!("ERROR: {text}");
    }
    text
}
