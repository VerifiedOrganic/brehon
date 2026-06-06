use std::path::PathBuf;

use clap::Args;

#[derive(Debug, Args)]
pub struct ReviewAuditArgs {
    /// Project root containing `.brehon/`. Defaults to the current project.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Git target branch/ref that should contain reviewed work.
    #[arg(long, default_value = "main")]
    pub target: String,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
    /// Exit non-zero when any task is not trusted.
    #[arg(long)]
    pub fail_on_findings: bool,
    /// Maximum target commits scanned for patch-id equivalence.
    #[arg(long, default_value_t = 1000)]
    pub max_target_commits: usize,
}
