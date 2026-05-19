use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub async fn execute() -> Result<()> {
    use brehon_mcp::McpServer;

    let project_root = std::env::current_dir()?;
    let config = brehon_config::load_config(Some(&project_root))?;
    let store = Arc::new(brehon_store_fjall::FjallEventStore::new(
        resolve_state_path(&project_root, &config.context.db_path),
    )?);
    let search_index = Arc::new(
        brehon_search_tantivy::TantivySearchIndex::new(&resolve_state_path(
            &project_root,
            &config.context.search_index_path,
        ))
        .await?,
    );
    let mut server = McpServer::new("brehon", env!("CARGO_PKG_VERSION"))
        .with_event_store(store.clone())
        .with_proof_store(store.clone())
        .with_run_store(store)
        .with_search_index(search_index);
    server.register_builtin_tools();

    tracing::info!("Starting Brehon MCP server (stdio/rmcp)");

    brehon_mcp::server::run_stdio(server)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;

    Ok(())
}

fn resolve_state_path(project_root: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}
