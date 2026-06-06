// Many tests use `let mut x = T::default(); x.field = ...;` instead of struct
// literals to keep diffs tight when new fields land. Lint is style-only.
#![allow(clippy::field_reassign_with_default)]

pub mod commands;
pub mod names;
pub mod recovery;
pub mod signals;
pub mod ui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing::info;

use crate::commands::{
    claude_hook, clean, config, doctor, epic_truth, factory, import_plan, init, maintenance,
    process, reset, review_audit, run, runtime, serve, task, test as test_cmd,
};

fn absolutize_project_root(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() || path == Path::new(".") {
        return std::env::current_dir().ok();
    }
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

fn project_root_from_config_file(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let root = if parent.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        // For a relative path like `.brehon/local.yaml`, `.brehon` has an
        // empty parent. Treat that as the current directory instead of
        // passing an empty project root through startup.
        parent
            .parent()
            .filter(|candidate| !candidate.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    } else {
        parent
    };
    absolutize_project_root(root)
}

fn resolve_project_root(config_arg: Option<&PathBuf>) -> Option<PathBuf> {
    match config_arg {
        Some(path) if path.is_file() => project_root_from_config_file(path),
        Some(path) => absolutize_project_root(path),
        None => std::env::current_dir().ok(),
    }
}

fn project_root_from_brehon_env() -> Option<PathBuf> {
    if let Some(project_root) = std::env::var_os("BREHON_PROJECT_ROOT")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return absolutize_project_root(&project_root);
    }

    let brehon_root = std::env::var_os("BREHON_ROOT")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())?;
    let project_root = if brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon")
    {
        brehon_root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        brehon_root
    };
    absolutize_project_root(&project_root)
}

fn task_command_project_root(
    config_arg: Option<&PathBuf>,
    resolved_project_path: Option<PathBuf>,
) -> PathBuf {
    if config_arg.is_none() {
        if let Some(project_root) = project_root_from_brehon_env() {
            return project_root;
        }
    }
    resolved_project_path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn resolve_config_override(config_arg: Option<&PathBuf>) -> Option<PathBuf> {
    match config_arg {
        Some(path) if path.is_file() => Some(path.clone()),
        _ => None,
    }
}

/// Whether this invocation will launch the TUI (default or `run` command).
fn is_tui_command(command: &Option<Commands>) -> bool {
    matches!(
        command,
        None | Some(Commands::Run { .. })
            | Some(Commands::Runtime {
                command: RuntimeCommands::Dashboard
            })
    )
}

/// Whether this invocation is the MCP serve command (stdout must be clean JSON-RPC).
fn is_serve_command(command: &Option<Commands>) -> bool {
    matches!(command, Some(Commands::Serve))
}

fn tui_log_path(project_path: Option<&std::path::Path>) -> std::path::PathBuf {
    project_path
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".brehon")
        .join("brehon.log")
}

fn print_tui_startup_error(err: &anyhow::Error, project_path: Option<&std::path::Path>) {
    eprintln!("\n  {} Brehon failed to start: {err:#}", ui::red("Error:"));
    eprintln!("    See {}", tui_log_path(project_path).display());
}

#[derive(Parser)]
#[command(name = "brehon")]
#[command(about = "Brehon - Multi-agent orchestration system", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(long, global = true)]
    workers: Option<String>,

    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[arg(long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    #[command(name = "run")]
    Run {
        #[arg(long)]
        workers: Option<String>,
    },

    #[command(name = "init")]
    Init {
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },

    #[command(name = "config")]
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    #[command(name = "test")]
    Test {
        #[command(subcommand)]
        command: Option<TestCommands>,

        #[arg(long)]
        live: bool,
    },

    #[command(name = "doctor")]
    Doctor {
        #[arg(long)]
        repair: bool,
        #[arg(long)]
        json: bool,
    },

    #[command(name = "runtime")]
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommands,
    },

    #[command(name = "ps")]
    Ps(process::PsArgs),

    #[command(name = "kill")]
    Kill(process::KillArgs),

    #[command(name = "import-plan")]
    ImportPlan {
        file: PathBuf,

        #[arg(long, default_value = ".")]
        path: PathBuf,

        #[arg(long)]
        dry_run: bool,

        #[arg(long, value_enum, default_value_t = import_plan::ExtractMode::Auto)]
        mode: import_plan::ExtractMode,
    },

    #[command(name = "extract-plan")]
    ExtractPlan {
        file: PathBuf,

        #[arg(long, default_value = ".")]
        path: PathBuf,

        #[arg(long)]
        output: Option<PathBuf>,

        #[arg(long, value_enum, default_value_t = import_plan::ExtractMode::Auto)]
        mode: import_plan::ExtractMode,
    },

    /// Start the MCP server (used by agents to access Brehon tools)
    #[command(name = "serve")]
    Serve,

    /// Claude Code PreToolUse hook. Reads tool-call JSON on stdin and
    /// enforces worktree containment. Wired up automatically during
    /// `brehon run`; not intended for direct invocation.
    #[command(name = "claude-hook", hide = true)]
    ClaudeHook,

    /// Remove all brehon artifacts from the project
    #[command(name = "clean")]
    Clean {
        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,
    },

    /// Reset brehon runtime state (worktrees, queues, logs, brehon/* branches)
    /// while preserving `.brehon/config.yaml` and any user-authored content.
    /// Use this between runs; use `clean` for a full uninstall.
    #[command(name = "reset")]
    Reset {
        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,
    },

    /// Report stale/prunable Brehon worktrees, run branches, and failed-import
    /// branches. Distinguishes active runtime state from leftovers. Use --prune
    /// to remove stale items after explicit confirmation.
    #[command(name = "maintenance")]
    Maintenance {
        /// Actually remove stale branches and worktrees (requires confirmation)
        #[arg(long)]
        prune: bool,
        /// Skip confirmation prompt when pruning
        #[arg(long, requires = "prune")]
        force: bool,
        /// Output report as JSON
        #[arg(long, conflicts_with = "prune")]
        json: bool,
    },

    /// Audit review artifacts and target-branch evidence from a completed run.
    #[command(name = "review-audit")]
    ReviewAudit(review_audit::ReviewAuditArgs),

    #[command(name = "factory")]
    Factory {
        #[command(subcommand)]
        command: factory::FactoryCommand,
    },

    #[command(name = "task")]
    Task {
        #[command(subcommand)]
        command: task::TaskCommand,
    },

    #[command(name = "epic-truth")]
    EpicTruth {
        #[arg(long)]
        epic_id: String,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    #[command(name = "list")]
    List,

    #[command(name = "describe")]
    Describe { key: String },

    #[command(name = "validate")]
    Validate,

    #[command(name = "profiles")]
    Profiles,
}

#[derive(Subcommand)]
enum TestCommands {
    #[command(name = "scenario")]
    Scenario { name: String },

    #[command(name = "terminal-host")]
    TerminalHost {
        #[arg(long, value_enum)]
        host: test_cmd::TerminalHostSmokeKind,

        #[arg(long, value_enum, default_value = "lifecycle")]
        mode: test_cmd::TerminalHostSmokeMode,

        #[arg(long)]
        live: bool,
    },

    #[command(name = "runtime-daemon")]
    RuntimeDaemon {
        #[arg(long, value_enum, default_value = "headless")]
        host: test_cmd::TerminalHostSmokeKind,

        #[arg(long)]
        live: bool,
    },

    #[command(name = "run-wiring")]
    RunWiring {
        #[arg(long, value_enum, default_value = "headless")]
        host: test_cmd::TerminalHostSmokeKind,

        #[arg(long, value_enum, default_value = "host")]
        pane_ownership: test_cmd::TerminalHostPaneOwnershipArg,

        #[arg(long)]
        live: bool,
    },
}

#[derive(Subcommand)]
enum RuntimeCommands {
    #[command(name = "dashboard", hide = true)]
    Dashboard,

    #[command(name = "status")]
    Status {
        #[arg(long)]
        json: bool,
    },

    #[command(name = "approvals")]
    Approvals {
        #[arg(long)]
        json: bool,
    },

    #[command(name = "approve")]
    Approve {
        approval_id: String,
        #[arg(long, default_value_t = 5000)]
        wait_ms: u64,
    },

    #[command(name = "deny")]
    Deny {
        approval_id: String,
        #[arg(long, default_value_t = 5000)]
        wait_ms: u64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if cli.verbose {
            tracing_subscriber::EnvFilter::new("debug")
        } else {
            tracing_subscriber::EnvFilter::new("info")
        }
    });

    let config_override = resolve_config_override(cli.config.as_ref());
    let project_path = resolve_project_root(cli.config.as_ref());

    // For TUI mode, redirect logs to a file so they don't corrupt the display.
    // For serve mode, logs MUST go to stderr — stdout is the JSON-RPC channel.
    // For other commands, log to stderr as usual.
    if is_tui_command(&cli.command) {
        let log_dir = project_path
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(".brehon");
        let _ = std::fs::create_dir_all(&log_dir);
        let log_file = std::fs::File::create(log_dir.join("brehon.log")).unwrap_or_else(|_| {
            // Fallback: write to /dev/null if we can't create the log file
            std::fs::File::open("/dev/null").expect("cannot open /dev/null")
        });
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(log_file)
            .with_ansi(false)
            .init();
    } else if is_serve_command(&cli.command) {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    match cli.command {
        None => {
            let workers_override = cli.workers;
            match run::execute(
                project_path.as_deref(),
                config_override.as_deref(),
                workers_override.as_deref(),
            )
            .await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!("Error running Brehon: {:?}", e);
                    print_tui_startup_error(&e, project_path.as_deref());
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Run { workers }) => {
            let workers_override = workers.or(cli.workers);
            match run::execute(
                project_path.as_deref(),
                config_override.as_deref(),
                workers_override.as_deref(),
            )
            .await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!("Error running Brehon: {:?}", e);
                    print_tui_startup_error(&e, project_path.as_deref());
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Init { path }) => match init::execute(&path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\n  {} {}", ui::red("Error:"), e);
                ExitCode::FAILURE
            }
        },
        Some(Commands::Config { command }) => match command {
            ConfigCommands::List => {
                match config::list(project_path.as_deref(), config_override.as_deref()) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        tracing::error!("Error listing config: {:?}", e);
                        ExitCode::FAILURE
                    }
                }
            }
            ConfigCommands::Describe { key } => {
                match config::describe(project_path.as_deref(), config_override.as_deref(), &key) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        tracing::error!("Error describing config key: {:?}", e);
                        ExitCode::FAILURE
                    }
                }
            }
            ConfigCommands::Validate => {
                match config::validate(project_path.as_deref(), config_override.as_deref()) {
                    Ok(()) => {
                        info!("Configuration is valid");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        tracing::error!("Configuration validation failed: {:?}", e);
                        ExitCode::FAILURE
                    }
                }
            }
            ConfigCommands::Profiles => {
                match config::profiles(project_path.as_deref(), config_override.as_deref()) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        tracing::error!("Error showing config profiles: {:?}", e);
                        ExitCode::FAILURE
                    }
                }
            }
        },
        Some(Commands::Test { command, live }) => match command {
            Some(TestCommands::Scenario { name }) => {
                match test_cmd::run_scenario(
                    project_path.as_deref(),
                    config_override.as_deref(),
                    &name,
                )
                .await
                {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        tracing::error!("Error running scenario: {:?}", e);
                        ExitCode::FAILURE
                    }
                }
            }
            Some(TestCommands::TerminalHost {
                host,
                mode,
                live: command_live,
            }) => {
                if live || command_live {
                    match test_cmd::run_terminal_host_smoke(
                        project_path.as_deref(),
                        config_override.as_deref(),
                        host,
                        mode,
                    )
                    .await
                    {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            tracing::error!("Error running terminal host smoke test: {:?}", e);
                            ExitCode::FAILURE
                        }
                    }
                } else {
                    tracing::error!("terminal-host smoke test requires --live");
                    ExitCode::FAILURE
                }
            }
            Some(TestCommands::RuntimeDaemon {
                host,
                live: command_live,
            }) => {
                if live || command_live {
                    match test_cmd::run_runtime_daemon_smoke(
                        project_path.as_deref(),
                        config_override.as_deref(),
                        host,
                    )
                    .await
                    {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            tracing::error!("Error running runtime daemon smoke test: {:?}", e);
                            ExitCode::FAILURE
                        }
                    }
                } else {
                    tracing::error!("runtime-daemon smoke test requires --live");
                    ExitCode::FAILURE
                }
            }
            Some(TestCommands::RunWiring {
                host,
                pane_ownership,
                live: command_live,
            }) => {
                if live || command_live {
                    match test_cmd::run_runtime_host_wiring_smoke(
                        project_path.as_deref(),
                        config_override.as_deref(),
                        host,
                        pane_ownership,
                    )
                    .await
                    {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            tracing::error!(
                                "Error running runtime host wiring smoke test: {:?}",
                                e
                            );
                            ExitCode::FAILURE
                        }
                    }
                } else {
                    tracing::error!("run-wiring smoke test requires --live");
                    ExitCode::FAILURE
                }
            }
            None => {
                if live {
                    match test_cmd::run_live_conformance(
                        project_path.as_deref(),
                        config_override.as_deref(),
                    )
                    .await
                    {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            tracing::error!("Error running live tests: {:?}", e);
                            ExitCode::FAILURE
                        }
                    }
                } else {
                    tracing::error!("Specify --live or scenario <name>");
                    ExitCode::FAILURE
                }
            }
        },
        Some(Commands::Serve) => match serve::execute().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("MCP server error: {:?}", e);
                ExitCode::FAILURE
            }
        },
        Some(Commands::ClaudeHook) => claude_hook::execute(),
        Some(Commands::Clean { force }) => {
            let path = project_path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match clean::execute(&path, force) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n  {} {}", ui::red("Error:"), e);
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Reset { force }) => {
            let path = project_path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match reset::execute(&path, force) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n  {} {}", ui::red("Error:"), e);
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Maintenance { prune, force, json }) => {
            let path = project_path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match maintenance::execute(&path, prune, force, json) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n  {} {}", ui::red("Error:"), e);
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::ReviewAudit(args)) => {
            let path = args.root.or(project_path);
            match review_audit::execute(
                path.as_deref(),
                &args.target,
                args.json,
                args.fail_on_findings,
                args.max_target_commits,
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n  {} {}", ui::red("Error:"), e);
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Doctor { repair, json }) => {
            match doctor::execute(project_path.as_deref(), repair, json) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!("Diagnostics failed: {:?}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Runtime { command }) => {
            let result = match command {
                RuntimeCommands::Dashboard => runtime::dashboard(project_path.as_deref()).await,
                RuntimeCommands::Status { json } => {
                    runtime::status(project_path.as_deref(), json).await
                }
                RuntimeCommands::Approvals { json } => {
                    runtime::approvals(project_path.as_deref(), json).await
                }
                RuntimeCommands::Approve {
                    approval_id,
                    wait_ms,
                } => {
                    runtime::resolve_approval(project_path.as_deref(), &approval_id, true, wait_ms)
                        .await
                }
                RuntimeCommands::Deny {
                    approval_id,
                    wait_ms,
                } => {
                    runtime::resolve_approval(project_path.as_deref(), &approval_id, false, wait_ms)
                        .await
                }
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n  {} {}", ui::red("Error:"), e);
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Ps(args)) => match process::execute_ps(project_path.as_deref(), &args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\n  {} {}", ui::red("Error:"), e);
                ExitCode::FAILURE
            }
        },
        Some(Commands::Kill(args)) => match process::execute_kill(project_path.as_deref(), &args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\n  {} {}", ui::red("Error:"), e);
                ExitCode::FAILURE
            }
        },
        Some(Commands::ImportPlan {
            file,
            path,
            dry_run,
            mode,
        }) => match import_plan::execute(&path, &file, dry_run, mode).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\n  {} {}", ui::red("Error:"), e);
                ExitCode::FAILURE
            }
        },
        Some(Commands::ExtractPlan {
            file,
            path,
            output,
            mode,
        }) => match import_plan::execute_extract(&path, &file, output.as_deref(), mode).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\n  {} {}", ui::red("Error:"), e);
                ExitCode::FAILURE
            }
        },
        Some(Commands::Factory { command }) => match factory::execute(&command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("Factory command failed: {:?}", e);
                ExitCode::FAILURE
            }
        },
        Some(Commands::Task { command }) => {
            let path = task_command_project_root(cli.config.as_ref(), project_path);
            match task::execute(&path, &command).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n  {} {}", ui::red("Error:"), e);
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::EpicTruth { epic_id }) => {
            match epic_truth::execute(&epic_id, project_path.as_deref()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("\n  {} {}", ui::red("Error:"), e);
                    ExitCode::FAILURE
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn project_root_from_relative_dot_brehon_config_uses_current_dir() {
        let cwd = std::env::current_dir().unwrap();

        let root =
            project_root_from_config_file(Path::new(".brehon/config.yaml")).expect("project root");

        assert_eq!(root, cwd);
    }

    #[test]
    fn project_root_from_nested_dot_brehon_config_uses_nested_root() {
        let cwd = std::env::current_dir().unwrap();

        let root = project_root_from_config_file(Path::new("workspace/.brehon/config.yaml"))
            .expect("project root");

        assert_eq!(root, cwd.join("workspace"));
    }

    #[test]
    fn resolve_project_root_infers_absolute_brehon_config_parent() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_dir = temp.path().join(".brehon");
        std::fs::create_dir_all(&brehon_dir).unwrap();
        let config_path = brehon_dir.join("config.yaml");
        std::fs::write(&config_path, "version: 1\n").unwrap();

        let root = resolve_project_root(Some(&config_path)).expect("project root");

        assert_eq!(root, temp.path());
    }

    #[test]
    fn task_command_project_root_prefers_brehon_project_root_env_without_config() {
        let _lock = ENV_LOCK.lock().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cwd_project = tempfile::tempdir().unwrap();
        let _project_root = EnvGuard::set("BREHON_PROJECT_ROOT", project.path());
        let _brehon_root = EnvGuard::unset("BREHON_ROOT");

        let root = task_command_project_root(None, Some(cwd_project.path().to_path_buf()));

        assert_eq!(root, project.path());
    }

    #[test]
    fn task_command_project_root_derives_project_root_from_brehon_root_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cwd_project = tempfile::tempdir().unwrap();
        let _project_root = EnvGuard::unset("BREHON_PROJECT_ROOT");
        let _brehon_root = EnvGuard::set("BREHON_ROOT", &project.path().join(".brehon"));

        let root = task_command_project_root(None, Some(cwd_project.path().to_path_buf()));

        assert_eq!(root, project.path());
    }

    #[test]
    fn task_command_project_root_keeps_explicit_config_over_brehon_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let env_project = tempfile::tempdir().unwrap();
        let explicit_project = tempfile::tempdir().unwrap();
        let config_path = explicit_project.path().join(".brehon/config.yaml");
        let _project_root = EnvGuard::set("BREHON_PROJECT_ROOT", env_project.path());

        let root = task_command_project_root(
            Some(&config_path),
            Some(explicit_project.path().to_path_buf()),
        );

        assert_eq!(root, explicit_project.path());
    }
}
