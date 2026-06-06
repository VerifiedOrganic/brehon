//! Brehon-owned local agent scaffold paths.

/// Repo-relative paths that Brehon may rewrite with machine-local agent
/// configuration. These files should not make a worker worktree unsafe to
/// reassign or remove.
pub const BREHON_LOCAL_SCAFFOLD_PATHS: &[&str] = &[
    ".mcp.json",
    ".agents/mcp_config.json",
    "opencode.json",
    ".claude/settings.local.json",
];

/// Return whether a repo-relative path is Brehon-owned local scaffold.
pub fn is_brehon_local_scaffold_path(path: &str) -> bool {
    let path = path.trim_matches('"');
    BREHON_LOCAL_SCAFFOLD_PATHS.contains(&path)
        || path == ".antigravitycli"
        || path.starts_with(".antigravitycli/")
}
