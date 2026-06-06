use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

pub(crate) const BREHON_MCP_BACKING_ENV: &str = "BREHON_MCP_BACKING";
pub(crate) const MCP_BACKING_RUNTIME_FILES: &str = "runtime-files";
pub(crate) const MCP_BACKING_EMBEDDED_STORE: &str = "embedded-store";

pub async fn execute() -> Result<()> {
    let project_root = resolve_project_root()?;
    let backing_mode = McpBackingMode::from_env()?;

    if let Err(err) = write_mcp_server_metadata(
        &project_root,
        McpServerBackingStatus::starting(backing_mode),
    ) {
        tracing::warn!("failed to write brehon serve MCP startup metadata: {err}");
    }

    let (mut server, backing_status) = build_server(&project_root, backing_mode).await?;
    server.register_builtin_tools();

    if let Err(err) = write_mcp_server_metadata(&project_root, backing_status) {
        tracing::warn!("failed to write brehon serve MCP metadata: {err}");
    }

    tracing::info!(
        backing_mode = backing_status.backing_mode.as_str(),
        "Starting Brehon MCP server (stdio/rmcp)"
    );

    brehon_mcp::server::run_stdio(server)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;

    Ok(())
}

async fn build_server(
    project_root: &Path,
    backing_mode: McpBackingMode,
) -> Result<(brehon_mcp::McpServer, McpServerBackingStatus)> {
    let server = brehon_mcp::McpServer::new("brehon", env!("CARGO_PKG_VERSION"));
    match backing_mode {
        McpBackingMode::RuntimeFiles => {
            tracing::info!(
                "Starting Brehon MCP server with runtime-file backing; embedded Fjall store is not opened"
            );
            Ok((
                server,
                McpServerBackingStatus {
                    backing_mode,
                    startup_status: "ready",
                    event_store_attached: false,
                    proof_store_attached: false,
                    run_store_attached: false,
                    search_index_attached: false,
                },
            ))
        }
        McpBackingMode::EmbeddedStore => attach_embedded_store_backing(server, project_root).await,
    }
}

async fn attach_embedded_store_backing(
    mut server: brehon_mcp::McpServer,
    project_root: &Path,
) -> Result<(brehon_mcp::McpServer, McpServerBackingStatus)> {
    let config = brehon_config::load_config(Some(project_root))?;
    let store_path = resolve_state_path(project_root, &config.context.db_path);
    let store = Arc::new(brehon_store_fjall::FjallEventStore::new(&store_path)?);
    let search_index = Arc::new(
        brehon_search_tantivy::TantivySearchIndex::new(&resolve_state_path(
            project_root,
            &config.context.search_index_path,
        ))
        .await?,
    );
    let proof_store_available = store.proof_store_available();
    server = server
        .with_event_store(store.clone())
        .with_run_store(store.clone())
        .with_search_index(search_index);
    if proof_store_available {
        server = server.with_proof_store(store.clone());
    } else {
        tracing::error!(
            path = %store_path.display(),
            "Starting Brehon MCP server without durable proof projection; task coordination remains available"
        );
    }

    Ok((
        server,
        McpServerBackingStatus {
            backing_mode: McpBackingMode::EmbeddedStore,
            startup_status: "ready",
            event_store_attached: true,
            proof_store_attached: proof_store_available,
            run_store_attached: true,
            search_index_attached: true,
        },
    ))
}

fn resolve_project_root() -> Result<PathBuf> {
    if let Some(project_root) = non_empty_env_path("BREHON_PROJECT_ROOT") {
        return Ok(project_root);
    }

    if let Some(brehon_root) = non_empty_env_path("BREHON_ROOT") {
        if brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
            if let Some(project_root) = brehon_root.parent() {
                return Ok(project_root.to_path_buf());
            }
        }
        return Ok(brehon_root);
    }

    Ok(std::env::current_dir()?)
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpBackingMode {
    RuntimeFiles,
    EmbeddedStore,
}

impl McpBackingMode {
    fn from_env() -> Result<Self> {
        Self::parse(std::env::var(BREHON_MCP_BACKING_ENV).ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Result<Self> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::RuntimeFiles);
        };
        match value {
            MCP_BACKING_RUNTIME_FILES | "runtime" | "files" => Ok(Self::RuntimeFiles),
            MCP_BACKING_EMBEDDED_STORE | "embedded" | "fjall" => Ok(Self::EmbeddedStore),
            other => Err(anyhow::anyhow!(
                "unsupported {BREHON_MCP_BACKING_ENV}={other:?}; expected {MCP_BACKING_RUNTIME_FILES:?} or {MCP_BACKING_EMBEDDED_STORE:?}"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeFiles => MCP_BACKING_RUNTIME_FILES,
            Self::EmbeddedStore => MCP_BACKING_EMBEDDED_STORE,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct McpServerBackingStatus {
    backing_mode: McpBackingMode,
    startup_status: &'static str,
    event_store_attached: bool,
    proof_store_attached: bool,
    run_store_attached: bool,
    search_index_attached: bool,
}

impl McpServerBackingStatus {
    fn starting(backing_mode: McpBackingMode) -> Self {
        Self {
            backing_mode,
            startup_status: "starting",
            event_store_attached: false,
            proof_store_attached: false,
            run_store_attached: false,
            search_index_attached: false,
        }
    }
}

fn resolve_state_path(project_root: &Path, configured: &str) -> PathBuf {
    let path = PathBuf::from(configured);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

fn write_mcp_server_metadata(
    project_root: &Path,
    backing_status: McpServerBackingStatus,
) -> Result<()> {
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
        "backing_mode": backing_status.backing_mode.as_str(),
        "startup_status": backing_status.startup_status,
        "event_store_attached": backing_status.event_store_attached,
        "proof_store_attached": backing_status.proof_store_attached,
        "run_store_attached": backing_status.run_store_attached,
        "search_index_attached": backing_status.search_index_attached,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backing_mode_defaults_to_runtime_files() {
        assert_eq!(
            McpBackingMode::parse(None).unwrap(),
            McpBackingMode::RuntimeFiles
        );
        assert_eq!(
            McpBackingMode::parse(Some("")).unwrap(),
            McpBackingMode::RuntimeFiles
        );
    }

    #[test]
    fn backing_mode_accepts_explicit_values() {
        assert_eq!(
            McpBackingMode::parse(Some(MCP_BACKING_RUNTIME_FILES)).unwrap(),
            McpBackingMode::RuntimeFiles
        );
        assert_eq!(
            McpBackingMode::parse(Some(MCP_BACKING_EMBEDDED_STORE)).unwrap(),
            McpBackingMode::EmbeddedStore
        );
    }

    #[test]
    fn backing_mode_rejects_unknown_values() {
        let err = McpBackingMode::parse(Some("shared-db")).unwrap_err();
        assert!(err.to_string().contains(BREHON_MCP_BACKING_ENV));
    }

    #[tokio::test]
    async fn runtime_file_backing_does_not_open_embedded_store() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();
        std::fs::write(
            temp.path().join(".brehon").join("brehon.db"),
            "not a directory",
        )
        .unwrap();

        let (server, status) = build_server(temp.path(), McpBackingMode::RuntimeFiles)
            .await
            .unwrap();

        assert!(server.event_store().is_none());
        assert!(server.search_index().is_none());
        assert!(!status.event_store_attached);
        assert!(!status.proof_store_attached);
        assert!(!status.run_store_attached);
        assert!(!status.search_index_attached);
    }
}
