use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

#[derive(Debug, Clone)]
pub struct BrehonProcess {
    pub pid: u32,
    pub ppid: u32,
    pub stat: String,
    pub executable: String,
    pub agent_name: Option<String>,
    pub agent_role: Option<String>,
    pub agent_type: Option<String>,
    pub session_name: Option<String>,
    pub command: String,
    pub brehon_root: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct PsArgs {
    /// Show processes across all projects instead of only the current project.
    #[arg(long)]
    pub all_projects: bool,

    /// Show the full command line and environment payload.
    #[arg(long)]
    pub full: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct KillArgs {
    /// Explicit PID(s) to terminate.
    pub pids: Vec<u32>,

    /// Kill all matching Brehon processes in scope.
    #[arg(long)]
    pub all: bool,

    /// Expand scope to all Brehon projects, not just the current project.
    #[arg(long)]
    pub all_projects: bool,

    /// Send SIGKILL after SIGTERM if the process survives.
    #[arg(long)]
    pub force: bool,
}

fn extract_env_path(command: &str, key: &str) -> Option<PathBuf> {
    for token in command.split_whitespace() {
        let prefix = format!("{key}=");
        if let Some(value) = token.strip_prefix(&prefix) {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn extract_env_value(command: &str, key: &str) -> Option<String> {
    for token in command.split_whitespace() {
        let prefix = format!("{key}=");
        if let Some(value) = token.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

fn normalize_executable(command: &str, fallback: &str) -> String {
    for token in command.split_whitespace() {
        if token.contains('=') && !token.starts_with('/') {
            continue;
        }
        let candidate = Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(token);
        if !candidate.is_empty() {
            return candidate.to_string();
        }
    }
    fallback.to_string()
}

fn parse_ps_line(line: &str) -> Option<BrehonProcess> {
    let mut parts = line.split_whitespace();
    let pid: u32 = parts.next()?.parse().ok()?;
    let ppid: u32 = parts.next()?.parse().ok()?;
    let stat = parts.next()?.to_string();
    let comm = parts.next()?.to_string();
    let command = parts.collect::<Vec<_>>().join(" ");
    Some(BrehonProcess {
        pid,
        ppid,
        stat,
        executable: normalize_executable(&command, &comm),
        agent_name: extract_env_value(&command, "BREHON_AGENT_NAME"),
        agent_role: extract_env_value(&command, "BREHON_AGENT_ROLE"),
        agent_type: extract_env_value(&command, "BREHON_AGENT_TYPE"),
        session_name: extract_env_value(&command, "BREHON_SESSION_NAME"),
        brehon_root: extract_env_path(&command, "BREHON_ROOT"),
        workspace_root: extract_env_path(&command, "BREHON_WORKSPACE_ROOT"),
        working_directory: extract_env_path(&command, "PWD"),
        command,
    })
}

fn current_project_brehon_root(project_path: &Path) -> PathBuf {
    project_path.join(".brehon")
}

fn process_matches_scope(
    process: &BrehonProcess,
    current_brehon_root: Option<&Path>,
    all_projects: bool,
) -> bool {
    if process_is_internal_helper(process) {
        return false;
    }

    if all_projects {
        return process.brehon_root.is_some()
            || process.workspace_root.is_some()
            || process.executable == "brehon";
    }

    let Some(expected_root) = current_brehon_root else {
        return process.brehon_root.is_some() || process.workspace_root.is_some();
    };
    let expected_project_root = expected_root.parent().unwrap_or(expected_root);
    let is_project_local_brehon_run = process.executable == "brehon"
        && process
            .command
            .split_whitespace()
            .any(|token| matches!(token, "run" | "serve"))
        && (process.working_directory.as_deref() == Some(expected_project_root)
            || process
                .command
                .contains(expected_root.to_string_lossy().as_ref())
            || process
                .command
                .contains(expected_project_root.to_string_lossy().as_ref()));

    process.brehon_root.as_deref() == Some(expected_root)
        || process.workspace_root.as_deref() == Some(expected_project_root)
        || process.workspace_root.as_deref().and_then(Path::parent) == Some(expected_project_root)
        || is_project_local_brehon_run
}

fn process_is_internal_helper(process: &BrehonProcess) -> bool {
    process.ppid == std::process::id() && matches!(process.executable.as_str(), "ps" | "kill")
}

fn list_processes(project_path: Option<&Path>, all_projects: bool) -> Result<Vec<BrehonProcess>> {
    let output = Command::new("ps")
        .args(["eww", "-Ao", "pid=,ppid=,stat=,comm=,command="])
        .output()
        .context("failed to execute ps")?;

    if !output.status.success() {
        bail!(
            "ps failed with status {}",
            output.status.code().unwrap_or_default()
        );
    }

    let current_root = project_path.map(current_project_brehon_root);
    let mut processes: Vec<BrehonProcess> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_line)
        .filter(|process| process_matches_scope(process, current_root.as_deref(), all_projects))
        .collect();

    processes.sort_by_key(|process| process.pid);
    Ok(processes)
}

fn project_label(process: &BrehonProcess) -> String {
    if let Some(root) = process.brehon_root.as_ref() {
        if let Some(name) = root
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
        {
            return name.to_string();
        }
    }
    if let Some(root) = process.workspace_root.as_ref() {
        if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
            return name.to_string();
        }
    }
    "-".to_string()
}

fn print_process(process: &BrehonProcess, full: bool) {
    println!(
        "{:>6} {:>6} {:<4} {:<16} {:<16} {:<12} {:<18} {:<16}",
        process.pid,
        process.ppid,
        process.stat,
        process.executable,
        process.agent_name.as_deref().unwrap_or("-"),
        process.agent_role.as_deref().unwrap_or("-"),
        process.agent_type.as_deref().unwrap_or("-"),
        project_label(process),
    );
    if full {
        println!("       cmd: {}", process.command);
    }
}

pub fn execute_ps(project_path: Option<&Path>, args: &PsArgs) -> Result<()> {
    let processes = list_processes(project_path, args.all_projects)?;
    if processes.is_empty() {
        println!("No Brehon processes found.");
        return Ok(());
    }

    println!(
        "{:>6} {:>6} {:<4} {:<16} {:<16} {:<12} {:<18} {:<16}",
        "PID", "PPID", "STAT", "EXEC", "AGENT", "ROLE", "TYPE", "PROJECT"
    );
    for process in &processes {
        print_process(process, args.full);
    }
    println!("\n{} process(es).", processes.len());
    Ok(())
}

fn send_signal(signal: &str, pids: &[u32]) -> Result<()> {
    if pids.is_empty() {
        return Ok(());
    }

    let mut failures = Vec::new();
    for pid in pids {
        let output = Command::new("kill")
            .arg(signal)
            .arg(pid.to_string())
            .output()
            .context("failed to execute kill")?;
        if output.status.success() {
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if kill_error_is_process_missing(&stderr) {
            continue;
        }

        failures.push(format!("{pid}: {stderr}"));
    }

    if !failures.is_empty() {
        bail!("kill {} failed: {}", signal, failures.join("; "));
    }
    Ok(())
}

fn send_signal_best_effort(signal: &str, pids: &[u32]) -> Result<()> {
    for pid in pids {
        let output = Command::new("kill")
            .arg(signal)
            .arg(pid.to_string())
            .output()
            .context("failed to execute kill")?;
        if output.status.success() {
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if kill_error_is_process_missing(&stderr) {
            continue;
        }

        tracing::warn!(
            signal,
            pid,
            stderr = %stderr,
            "failed to signal matched Brehon process; will verify liveness before failing"
        );
    }
    Ok(())
}

fn kill_error_is_process_missing(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no such process") || stderr.contains("not found")
}

fn pid_is_live(pid: u32) -> Result<bool> {
    let output = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .context("failed to execute kill -0")?;
    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if kill_error_is_process_missing(&stderr) {
        return Ok(false);
    }

    // EPERM or another non-ESRCH failure means the pid still exists from this
    // sweep's perspective, even if we cannot signal it.
    Ok(true)
}

fn retain_live_pids(pids: Vec<u32>) -> Result<Vec<u32>> {
    let mut live = Vec::new();
    for pid in pids {
        if pid_is_live(pid)? {
            live.push(pid);
        }
    }
    Ok(live)
}

pub(crate) fn terminate_session_processes(
    project_path: Option<&Path>,
    session_name: &str,
    force: bool,
) -> Result<Vec<u32>> {
    if session_name.trim().is_empty() {
        return Ok(Vec::new());
    }

    let current_pid = std::process::id();
    let target_pids: Vec<u32> = list_processes(project_path, false)?
        .into_iter()
        .filter(|process| process.pid != current_pid)
        .filter(|process| process.session_name.as_deref() == Some(session_name))
        .map(|process| process.pid)
        .collect();

    if target_pids.is_empty() {
        return Ok(Vec::new());
    }

    send_signal_best_effort("-TERM", &target_pids)?;
    thread::sleep(Duration::from_millis(750));

    let mut remaining: Vec<u32> = list_processes(project_path, false)?
        .into_iter()
        .filter(|process| process.pid != current_pid)
        .filter(|process| process.session_name.as_deref() == Some(session_name))
        .map(|process| process.pid)
        .collect();

    if !remaining.is_empty() && force {
        send_signal_best_effort("-KILL", &remaining)?;
        thread::sleep(Duration::from_millis(250));
        remaining = list_processes(project_path, false)?
            .into_iter()
            .filter(|process| process.pid != current_pid)
            .filter(|process| process.session_name.as_deref() == Some(session_name))
            .map(|process| process.pid)
            .collect();
    }

    retain_live_pids(remaining)
}

fn collect_termination_targets(processes: &[BrehonProcess], excluded_pids: &[u32]) -> Vec<u32> {
    let mut excluded: std::collections::HashSet<u32> = excluded_pids.iter().copied().collect();
    excluded.insert(std::process::id());

    let mut target_pids: Vec<u32> = processes
        .iter()
        .filter(|process| !excluded.contains(&process.pid))
        .map(|process| process.pid)
        .collect();
    target_pids.sort_unstable();
    target_pids.dedup();
    target_pids
}

pub(crate) fn terminate_project_processes(
    project_path: Option<&Path>,
    excluded_pids: &[u32],
    force: bool,
) -> Result<Vec<u32>> {
    let target_pids =
        collect_termination_targets(&list_processes(project_path, false)?, excluded_pids);

    if target_pids.is_empty() {
        return Ok(Vec::new());
    }

    send_signal_best_effort("-TERM", &target_pids)?;
    thread::sleep(Duration::from_millis(750));

    let mut remaining =
        collect_termination_targets(&list_processes(project_path, false)?, excluded_pids);

    if !remaining.is_empty() && force {
        send_signal_best_effort("-KILL", &remaining)?;
        thread::sleep(Duration::from_millis(250));
        remaining =
            collect_termination_targets(&list_processes(project_path, false)?, excluded_pids);
    }

    retain_live_pids(remaining)
}

pub fn execute_kill(project_path: Option<&Path>, args: &KillArgs) -> Result<()> {
    if !args.all && args.pids.is_empty() {
        bail!("Specify one or more PIDs, or pass --all");
    }

    let scoped_processes = list_processes(project_path, args.all_projects)?;
    let scoped_pids: Vec<u32> = scoped_processes.iter().map(|process| process.pid).collect();

    let targets: Vec<u32> = if args.all {
        scoped_pids
    } else {
        args.pids
            .iter()
            .copied()
            .filter(|pid| scoped_pids.contains(pid))
            .collect()
    };

    if targets.is_empty() {
        println!("No matching Brehon processes to kill.");
        return Ok(());
    }

    send_signal("-TERM", &targets)?;
    thread::sleep(Duration::from_millis(750));

    let remaining: Vec<u32> = list_processes(project_path, args.all_projects)?
        .into_iter()
        .filter(|process| targets.contains(&process.pid))
        .map(|process| process.pid)
        .collect();

    if !remaining.is_empty() && args.force {
        send_signal("-KILL", &remaining)?;
    }

    let survivors: Vec<u32> = list_processes(project_path, args.all_projects)?
        .into_iter()
        .filter(|process| targets.contains(&process.pid))
        .map(|process| process.pid)
        .collect();

    if !survivors.is_empty() {
        return Err(anyhow!(
            "Some Brehon processes are still running: {}",
            survivors
                .into_iter()
                .map(|pid| pid.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    println!(
        "Terminated {} Brehon process(es){}.",
        targets.len(),
        if args.force {
            " (forced where needed)"
        } else {
            ""
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ps_line_extracts_session_name() {
        let process = parse_ps_line(
            "123 1 S brehon BREHON_AGENT_NAME=worker-1 BREHON_AGENT_ROLE=worker BREHON_AGENT_TYPE=codex-worker BREHON_SESSION_NAME=brehon-abcd BREHON_ROOT=/tmp/project/.brehon /usr/bin/brehon run",
        )
        .expect("process should parse");

        assert_eq!(process.pid, 123);
        assert_eq!(process.session_name.as_deref(), Some("brehon-abcd"));
        assert_eq!(process.agent_name.as_deref(), Some("worker-1"));
    }

    #[test]
    fn process_scope_matches_current_project_root() {
        let process = BrehonProcess {
            pid: 123,
            ppid: 1,
            stat: "S".to_string(),
            executable: "claude".to_string(),
            agent_name: Some("worker-1".to_string()),
            agent_role: Some("worker".to_string()),
            agent_type: Some("claude-worker".to_string()),
            session_name: Some("brehon-abcd".to_string()),
            command: "claude".to_string(),
            brehon_root: Some(PathBuf::from("/tmp/project/.brehon")),
            workspace_root: Some(PathBuf::from("/tmp/project")),
            working_directory: Some(PathBuf::from("/tmp/project")),
        };

        assert!(process_matches_scope(
            &process,
            Some(Path::new("/tmp/project/.brehon")),
            false
        ));
        assert!(!process_matches_scope(
            &process,
            Some(Path::new("/tmp/other/.brehon")),
            false
        ));
    }

    #[test]
    fn collect_termination_targets_excludes_current_and_explicit_pids() {
        let current_pid = std::process::id();
        let processes = vec![
            BrehonProcess {
                pid: current_pid,
                ppid: 1,
                stat: "S".to_string(),
                executable: "brehon".to_string(),
                agent_name: Some("self".to_string()),
                agent_role: Some("supervisor".to_string()),
                agent_type: Some("claude-supervisor".to_string()),
                session_name: Some("brehon-current".to_string()),
                command: "brehon run".to_string(),
                brehon_root: Some(PathBuf::from("/tmp/project/.brehon")),
                workspace_root: Some(PathBuf::from("/tmp/project")),
                working_directory: Some(PathBuf::from("/tmp/project")),
            },
            BrehonProcess {
                pid: 200,
                ppid: 1,
                stat: "S".to_string(),
                executable: "claude".to_string(),
                agent_name: Some("worker-a".to_string()),
                agent_role: Some("worker".to_string()),
                agent_type: Some("claude-worker".to_string()),
                session_name: Some("brehon-old".to_string()),
                command: "claude".to_string(),
                brehon_root: Some(PathBuf::from("/tmp/project/.brehon")),
                workspace_root: Some(PathBuf::from("/tmp/project")),
                working_directory: Some(PathBuf::from("/tmp/project")),
            },
            BrehonProcess {
                pid: 300,
                ppid: 1,
                stat: "S".to_string(),
                executable: "codex".to_string(),
                agent_name: Some("reviewer-a".to_string()),
                agent_role: Some("reviewer".to_string()),
                agent_type: Some("codex-reviewer".to_string()),
                session_name: None,
                command: "codex".to_string(),
                brehon_root: Some(PathBuf::from("/tmp/project/.brehon")),
                workspace_root: Some(PathBuf::from("/tmp/project")),
                working_directory: Some(PathBuf::from("/tmp/project")),
            },
        ];

        assert_eq!(collect_termination_targets(&processes, &[300]), vec![200]);
    }

    #[test]
    fn process_scope_matches_project_local_brehon_run_without_brehon_env() {
        let process = BrehonProcess {
            pid: 123,
            ppid: 1,
            stat: "S".to_string(),
            executable: "brehon".to_string(),
            agent_name: None,
            agent_role: None,
            agent_type: None,
            session_name: None,
            command: "target/debug/brehon --config /tmp/project/.brehon/config.no-isolation.yaml run PWD=/tmp/project".to_string(),
            brehon_root: None,
            workspace_root: None,
            working_directory: Some(PathBuf::from("/tmp/project")),
        };

        assert!(process_matches_scope(
            &process,
            Some(Path::new("/tmp/project/.brehon")),
            false
        ));
    }

    #[test]
    fn process_scope_ignores_non_run_brehon_commands_without_brehon_env() {
        let process = BrehonProcess {
            pid: 123,
            ppid: 1,
            stat: "S".to_string(),
            executable: "brehon".to_string(),
            agent_name: None,
            agent_role: None,
            agent_type: None,
            session_name: None,
            command: "brehon ps --full PWD=/tmp/project".to_string(),
            brehon_root: None,
            workspace_root: None,
            working_directory: Some(PathBuf::from("/tmp/project")),
        };

        assert!(!process_matches_scope(
            &process,
            Some(Path::new("/tmp/project/.brehon")),
            false
        ));
    }

    #[test]
    fn process_scope_ignores_internal_ps_helper_child() {
        let process = BrehonProcess {
            pid: 123,
            ppid: std::process::id(),
            stat: "S".to_string(),
            executable: "ps".to_string(),
            agent_name: None,
            agent_role: None,
            agent_type: None,
            session_name: None,
            command: "ps eww -Ao pid=,ppid=,stat=,comm=,command=".to_string(),
            brehon_root: Some(PathBuf::from("/tmp/project/.brehon")),
            workspace_root: Some(PathBuf::from("/tmp/project")),
            working_directory: Some(PathBuf::from("/tmp/project")),
        };

        assert!(!process_matches_scope(
            &process,
            Some(Path::new("/tmp/project/.brehon")),
            false
        ));
    }

    #[test]
    fn kill_error_is_process_missing_matches_expected_messages() {
        assert!(kill_error_is_process_missing(
            "kill: 91311: No such process"
        ));
        assert!(kill_error_is_process_missing("process not found"));
        assert!(!kill_error_is_process_missing("operation not permitted"));
    }

    #[test]
    fn send_signal_ignores_already_exited_processes() {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();

        send_signal("-TERM", &[pid]).unwrap();
        let _ = child.wait();

        send_signal("-TERM", &[pid]).unwrap();
    }

    #[test]
    fn retain_live_pids_drops_already_exited_processes() {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();

        send_signal("-TERM", &[pid]).unwrap();
        let _ = child.wait();

        assert!(retain_live_pids(vec![pid]).unwrap().is_empty());
    }

    #[test]
    fn retain_live_pids_keeps_running_processes() {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();

        let live = retain_live_pids(vec![pid]).unwrap();
        assert_eq!(live, vec![pid]);

        let _ = child.kill();
        let _ = child.wait();
    }
}
