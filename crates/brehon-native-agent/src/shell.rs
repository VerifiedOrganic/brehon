// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph's `zeph-tools/src/shell/mod.rs` bash executor.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::runtime::CancellationToken;

const OUTPUT_CHANNEL_CAPACITY: usize = 128;
const GRACEFUL_TERM_MS: Duration = Duration::from_millis(250);

#[derive(Debug)]
enum ShellEvent {
    Output { is_stderr: bool, chunk: String },
}

pub(crate) async fn run_shell_command(
    worktree_root: &Path,
    cancel: &CancellationToken,
    shell: String,
    command: String,
    timeout_secs: u64,
    tool_env: &Option<Vec<(String, String)>>,
) -> Result<String, String> {
    let mut child = spawn_shell_command(worktree_root, shell, command, tool_env)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture shell stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture shell stderr".to_string())?;
    let mut output_rx = spawn_output_readers(stdout, stderr);

    collect_shell_output(&mut child, &mut output_rx, cancel, timeout_secs).await
}

fn spawn_shell_command(
    worktree_root: &Path,
    shell: String,
    command: String,
    tool_env: &Option<Vec<(String, String)>>,
) -> Result<Child, String> {
    let mut cmd = Command::new(shell);
    cmd.arg("-c");
    cmd.arg(command);
    cmd.current_dir(worktree_root);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if let Some(tool_env) = tool_env {
        cmd.env_clear();
        for (key, value) in tool_env {
            cmd.env(key, value);
        }
    }
    apply_noninteractive_tool_env(&mut cmd);
    cmd.spawn()
        .map_err(|err| format!("failed to spawn shell command: {err}"))
}

fn spawn_output_readers(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
) -> mpsc::Receiver<ShellEvent> {
    let (tx, rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);

    let stdout_tx = tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf = String::new();
        while reader.read_line(&mut buf).await.unwrap_or(0) > 0 {
            let _ = stdout_tx
                .send(ShellEvent::Output {
                    is_stderr: false,
                    chunk: buf.clone(),
                })
                .await;
            buf.clear();
        }
    });

    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = String::new();
        while reader.read_line(&mut buf).await.unwrap_or(0) > 0 {
            let _ = tx
                .send(ShellEvent::Output {
                    is_stderr: true,
                    chunk: buf.clone(),
                })
                .await;
            buf.clear();
        }
    });

    rx
}

async fn collect_shell_output(
    child: &mut Child,
    output_rx: &mut mpsc::Receiver<ShellEvent>,
    cancel: &CancellationToken,
    timeout_secs: u64,
) -> Result<String, String> {
    let mut output = String::new();
    let timeout = tokio::time::sleep(Duration::from_secs(timeout_secs));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            event = output_rx.recv() => {
                match event {
                    Some(ShellEvent::Output { is_stderr, chunk }) => {
                        if is_stderr {
                            output.push_str("[stderr] ");
                        }
                        output.push_str(&chunk);
                    }
                    None => return finalize_shell_output(child, output).await,
                }
            }
            _ = &mut timeout => {
                kill_process_tree(child).await;
                return Err(format!("command timed out after {timeout_secs}s"));
            }
            _ = cancel.cancelled() => {
                kill_process_tree(child).await;
                return Err("tool invocation cancelled".to_string());
            }
        }
    }
}

async fn finalize_shell_output(child: &mut Child, output: String) -> Result<String, String> {
    let exit_code = child
        .wait()
        .await
        .ok()
        .and_then(|status| status.code())
        .unwrap_or(1);
    Ok(format_shell_result(exit_code, &output))
}

fn apply_noninteractive_tool_env(cmd: &mut Command) {
    // Zeph's shell tool is intentionally non-interactive. Brehon's bash tool must
    // behave the same way even when the surrounding agent is visible in a PTY.
    cmd.env("PAGER", "cat");
    cmd.env("GIT_PAGER", "cat");
    cmd.env("MANPAGER", "cat");
    cmd.env("BAT_PAGER", "cat");
    cmd.env("DELTA_PAGER", "cat");
    cmd.env("LESS", "FRX");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GCM_INTERACTIVE", "never");
    cmd.env("NO_COLOR", "1");
    cmd.env("CLICOLOR", "0");
    cmd.env("TERM", "dumb");
}

async fn kill_process_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        send_signal_with_escalation(pid).await;
    }
    let _ = child.kill().await;
}

#[cfg(unix)]
async fn send_signal_with_escalation(pid: u32) {
    use nix::errno::Errno;
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let Ok(pid_i32) = i32::try_from(pid) else {
        return;
    };
    let target = Pid::from_raw(pid_i32);
    if let Err(err) = kill(target, Signal::SIGTERM) {
        if err != Errno::ESRCH {
            tracing::debug!(pid, error = %err, "SIGTERM failed");
        }
    }
    tokio::time::sleep(GRACEFUL_TERM_MS).await;
    let _ = Command::new("pkill")
        .args(["-KILL", "-P", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if let Err(err) = kill(target, Signal::SIGKILL) {
        if err != Errno::ESRCH {
            tracing::debug!(pid, error = %err, "SIGKILL failed");
        }
    }
}

fn format_shell_result(exit_code: i32, output: &str) -> String {
    let normalized = normalize_shell_text(output);
    format!(
        "exit_code: {exit_code}\n{}",
        if normalized.trim().is_empty() {
            "(no output)".to_string()
        } else {
            normalized
        }
    )
}

fn normalize_shell_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    #[tokio::test]
    async fn shell_runs_command() {
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let output = run_shell_command(
            temp.path(),
            &cancel,
            "/bin/sh".to_string(),
            "printf ok".to_string(),
            5,
            &None,
        )
        .await
        .unwrap();

        assert!(output.contains("exit_code: 0"));
        assert!(output.contains("ok"));
    }

    #[tokio::test]
    async fn shell_disables_interactive_pagers() {
        let temp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let output = run_shell_command(
            temp.path(),
            &cancel,
            "/bin/sh".to_string(),
            "printf 'pager-ok\\n' | \"$PAGER\"".to_string(),
            2,
            &None,
        )
        .await
        .unwrap();

        assert!(output.contains("exit_code: 0"));
        assert!(output.contains("pager-ok"));
    }

    #[tokio::test]
    async fn git_show_does_not_wait_in_a_pager() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init"]);
        git(
            temp.path(),
            &["config", "user.email", "native-agent@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "Native Agent Test"]);
        std::fs::write(temp.path().join("file.txt"), "hello\n").unwrap();
        git(temp.path(), &["add", "file.txt"]);
        git(temp.path(), &["commit", "-m", "initial"]);

        let cancel = CancellationToken::new();
        let output = run_shell_command(
            temp.path(),
            &cancel,
            "/bin/sh".to_string(),
            "git show HEAD --stat".to_string(),
            5,
            &None,
        )
        .await
        .unwrap();

        assert!(output.contains("exit_code: 0"));
        assert!(output.contains("file.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runner_does_not_use_login_shell_startup() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let shell_path = temp.path().join("fake-shell");
        std::fs::write(
            &shell_path,
            br#"#!/bin/sh
if [ "$1" = "-lc" ]; then
  sleep 5
  exit 42
fi
if [ "$1" = "-c" ]; then
  shift
  exec /bin/sh -c "$1"
fi
exit 43
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&shell_path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&shell_path, permissions).unwrap();

        let cancel = CancellationToken::new();
        let output = run_shell_command(
            temp.path(),
            &cancel,
            shell_path.display().to_string(),
            "printf non-login".to_string(),
            5,
            &None,
        )
        .await
        .unwrap();

        assert!(output.contains("exit_code: 0"));
        assert!(output.contains("non-login"));
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
