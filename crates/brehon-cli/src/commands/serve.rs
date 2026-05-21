use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
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

    if let Err(err) = write_mcp_server_metadata(&project_root) {
        tracing::warn!("failed to write brehon serve MCP metadata: {err}");
    }

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

fn write_mcp_server_metadata(project_root: &Path) -> Result<()> {
    let runtime_dir = project_root.join(".brehon").join("runtime");
    let servers_dir = runtime_dir.join("mcp-servers");
    std::fs::create_dir_all(&servers_dir)?;
    let binary_path = std::env::current_exe().ok();
    let binary_modified_unix_secs = binary_path.as_deref().and_then(file_modified_unix_secs);
    let metadata = serde_json::json!({
        "pid": std::process::id(),
        "server_name": "brehon",
        "server_version": env!("CARGO_PKG_VERSION"),
        "started_at": chrono::Utc::now().to_rfc3339(),
        "project_root": project_root,
        "binary_path": binary_path,
        "binary_modified_unix_secs": binary_modified_unix_secs,
        "source_revision": current_source_revision(project_root),
        "source_dirty": source_is_dirty(project_root),
    });
    std::fs::write(
        servers_dir.join(format!("{}.json", std::process::id())),
        serde_json::to_string_pretty(&metadata)?,
    )?;
    Ok(())
}

fn file_modified_unix_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

fn current_source_revision(project_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn source_is_dirty(project_root: &Path) -> Option<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(project_root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| !String::from_utf8_lossy(&output.stdout).trim().is_empty())
}
