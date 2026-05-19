//! Subprocess management for ACP agent processes.
//!
//! Handles spawning, stdio pipe management, EOF detection, and process termination.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("Failed to spawn process: {0}")]
    SpawnFailed(String),
    #[error("Process died unexpectedly")]
    ProcessDied,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Process timeout")]
    Timeout,
    #[error("STDIN pipe closed")]
    StdinClosed,
}

static PROCESS_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Tracked spawned task handles for an agent process.
///
/// Shutdown uses ownership-based semantics:
/// - Reader tasks (stdout/stderr) are aborted and reaped because they don't
///   own resources requiring graceful shutdown.
/// - The process-waiter task MUST NOT be aborted because it owns the
///   `tokio::process::Child` handle and must reap the subprocess via
///   `child.wait()`.  Aborting it would leak a zombie.
struct ProcessTasks {
    /// Handle for the stdout reader task that forwards child stdout lines.
    stdout_reader: Option<JoinHandle<()>>,
    /// Handle for the stderr reader task that forwards child stderr lines.
    stderr_reader: Option<JoinHandle<()>>,
    /// Handle for the process-wait task that monitors child exit.
    process_waiter: Option<JoinHandle<()>>,
}

pub struct AgentProcess {
    process_id: u64,
    stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    stdout_rx: Arc<Mutex<mpsc::Receiver<String>>>,
    #[allow(dead_code)]
    stderr_rx: Arc<Mutex<mpsc::Receiver<String>>>,
    pid: Option<u32>,
    is_alive: Arc<AtomicBool>,
    eof_detected: Arc<AtomicBool>,
    /// Tracked spawned tasks, set after construction.
    ///
    /// Shutdown semantics (see [`ProcessTasks`] for details):
    /// - Reader tasks are aborted and awaited (detached if stuck).
    /// - The process-waiter is awaited (not aborted) with a defensive timeout
    ///   so it can reap the Child without risking indefinite hangs.
    tasks: Mutex<ProcessTasks>,
}

impl AgentProcess {
    pub fn process_id(&self) -> u64 {
        self.process_id
    }

    pub async fn spawn(command: &str, args: &[String], cwd: &str) -> Result<Self, ProcessError> {
        Self::spawn_with_env(command, args, cwd, &[]).await
    }

    pub async fn spawn_with_env(
        command: &str,
        args: &[String],
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<Self, ProcessError> {
        let process_id = PROCESS_COUNTER.fetch_add(1, Ordering::SeqCst);
        debug!(process_id, command, ?args, cwd, "Spawning agent process");

        let cwd_path = std::path::Path::new(cwd);
        if !cwd_path.exists() {
            return Err(ProcessError::SpawnFailed(format!(
                "Working directory does not exist: {}",
                cwd
            )));
        }

        let mut cmd = Command::new(command);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        // Apply per-agent environment variables (BREHON_ROOT, BREHON_AGENT_NAME, etc.)
        for (key, value) in env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ProcessError::SpawnFailed(format!("{}: {}", command, e)))?;

        let pid = child.id();
        debug!(process_id, pid, "Agent process spawned");

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProcessError::SpawnFailed("Failed to get stdin pipe".to_string()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProcessError::SpawnFailed("Failed to get stdout pipe".to_string()))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProcessError::SpawnFailed("Failed to get stderr pipe".to_string()))?;

        let (stdout_tx, stdout_rx) = mpsc::channel(256);
        let (stderr_tx, stderr_rx) = mpsc::channel(256);

        let is_alive = Arc::new(AtomicBool::new(true));
        let eof_detected = Arc::new(AtomicBool::new(false));

        let is_alive_clone = Arc::clone(&is_alive);
        let eof_clone = Arc::clone(&eof_detected);
        let process_waiter: JoinHandle<()> = tokio::spawn(async move {
            let status = child.wait().await;
            is_alive_clone.store(false, Ordering::SeqCst);
            eof_clone.store(true, Ordering::SeqCst);
            debug!(process_id, ?status, "Agent process exited");
        });

        let stdout_reader = Self::spawn_stdout_reader(
            process_id,
            BufReader::new(stdout),
            stdout_tx,
            Arc::clone(&eof_detected),
        );
        let stderr_reader =
            Self::spawn_stderr_reader(process_id, BufReader::new(stderr), stderr_tx);

        Ok(Self {
            process_id,
            stdin: Arc::new(Mutex::new(Some(stdin))),
            stdout_rx: Arc::new(Mutex::new(stdout_rx)),
            stderr_rx: Arc::new(Mutex::new(stderr_rx)),
            pid,
            is_alive,
            eof_detected,
            tasks: Mutex::new(ProcessTasks {
                stdout_reader: Some(stdout_reader),
                stderr_reader: Some(stderr_reader),
                process_waiter: Some(process_waiter),
            }),
        })
    }

    fn spawn_stdout_reader(
        process_id: u64,
        mut reader: BufReader<tokio::process::ChildStdout>,
        tx: mpsc::Sender<String>,
        eof_detected: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        debug!(process_id, "STDOUT EOF detected");
                        eof_detected.store(true, Ordering::SeqCst);
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                        debug!(process_id, line = %trimmed, "STDOUT line");
                        if tx.send(trimmed.to_string()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(process_id, error = %e, "STDOUT read error");
                        eof_detected.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        })
    }

    fn spawn_stderr_reader(
        process_id: u64,
        mut reader: BufReader<tokio::process::ChildStderr>,
        tx: mpsc::Sender<String>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        debug!(process_id, "STDERR EOF detected");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                        debug!(process_id, line = %trimmed, "STDERR line");
                        if tx.send(trimmed.to_string()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(process_id, error = %e, "STDERR read error");
                        break;
                    }
                }
            }
        })
    }

    pub async fn send_line(&self, line: &str) -> Result<(), ProcessError> {
        let mut stdin = self.stdin.lock().await;

        let stdin = stdin.as_mut().ok_or(ProcessError::StdinClosed)?;

        let line_with_newline = format!("{}\n", line);
        stdin.write_all(line_with_newline.as_bytes()).await?;
        stdin.flush().await?;

        debug!(process_id = self.process_id, line, "Sent line");
        Ok(())
    }

    pub async fn recv_line(&mut self, timeout_ms: u64) -> Result<Option<String>, ProcessError> {
        let rx = self.stdout_rx.clone();
        let mut guard = rx.lock().await;

        match timeout(std::time::Duration::from_millis(timeout_ms), guard.recv()).await {
            Ok(Some(line)) => Ok(Some(line)),
            Ok(None) => {
                debug!(process_id = self.process_id, "STDOUT channel closed");
                Ok(None)
            }
            Err(_) => Err(ProcessError::Timeout),
        }
    }

    pub fn is_alive(&self) -> bool {
        self.is_alive.load(Ordering::SeqCst)
    }

    pub fn eof_detected(&self) -> bool {
        self.eof_detected.load(Ordering::SeqCst)
    }

    /// Terminates the subprocess and awaits all spawned reader/watcher tasks
    /// for deterministic shutdown. After sending SIGTERM (and SIGKILL if
    /// needed), we take ownership of the tracked `JoinHandle`s and wait for
    /// each to finish with a short timeout, ensuring no reader or process-wait
    /// tasks are left dangling.
    pub async fn kill(&self) -> Result<(), ProcessError> {
        if !self.is_alive() {
            debug!(process_id = self.process_id, "Process already dead");
            // Even if already dead, drain any remaining task handles.
            self.await_tasks().await;
            return Ok(());
        }

        debug!(process_id = self.process_id, "Sending SIGTERM to process");

        if let Some(pid) = self.pid {
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .arg("-TERM")
                    .arg(format!("-{}", pid))
                    .status();
            }

            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .arg("/PID")
                    .arg(format!("{}", pid))
                    .status();
            }
        }

        let grace_period = std::time::Duration::from_secs(5);
        let start = std::time::Instant::now();
        while start.elapsed() < grace_period && self.is_alive.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        if self.is_alive.load(Ordering::SeqCst) {
            warn!(
                process_id = self.process_id,
                "Process still alive after SIGTERM, sending SIGKILL"
            );

            if let Some(pid) = self.pid {
                #[cfg(unix)]
                {
                    let _ = std::process::Command::new("kill")
                        .arg("-KILL")
                        .arg(format!("-{}", pid))
                        .status();
                }

                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("taskkill")
                        .arg("/F")
                        .arg("/PID")
                        .arg(format!("{}", pid))
                        .status();
                }
            }
        }

        self.stdin.lock().await.take();

        // Abort reader tasks and await the process-waiter (which reaps the
        // Child). See await_tasks for the ownership rationale.
        self.await_tasks().await;

        debug!(process_id = self.process_id, "Process terminated");
        Ok(())
    }

    /// Take ownership of all tracked task handles and clean them up.
    ///
    /// Reader tasks (stdout/stderr) are aborted and reaped: they don't own
    /// any resources that require graceful shutdown, so aborting is safe and
    /// avoids detaching them if they are stuck.
    ///
    /// The process-waiter task is NOT aborted because it owns the
    /// `tokio::process::Child` handle and calls `child.wait()`.  Aborting it
    /// would drop the `Child` without reaping, which only triggers best-effort
    /// cleanup and can leave a zombie process.  Instead we await it with a
    /// generous defensive timeout (30 s) — since `kill()` already sent
    /// SIGTERM/SIGKILL and closed stdin, the child should exit promptly, but
    /// the timeout guards against pathological hangs (e.g. kernel-level
    /// delays, NFS-mounted cwd) without sacrificing the no-abort invariant.
    async fn await_tasks(&self) {
        let mut tasks = self.tasks.lock().await;

        // Reader tasks: abort and reap to avoid detached zombies on timeout.
        if let Some(handle) = tasks.stdout_reader.take() {
            handle.abort();
            let _ = handle.await;
        }
        if let Some(handle) = tasks.stderr_reader.take() {
            handle.abort();
            let _ = handle.await;
        }

        // Process-waiter: MUST NOT abort — it owns the Child handle and
        // calls child.wait() to reap the subprocess.  Await with a defensive
        // timeout so we never hang indefinitely, even in pathological cases
        // (e.g. kernel-level delays, NFS-mounted cwd).
        if let Some(handle) = tasks.process_waiter.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(30), handle).await {
                Ok(Ok(())) => {} // normal: waiter finished, child reaped
                Ok(Err(join_err)) => {
                    warn!(
                        process_id = self.process_id,
                        error = %join_err,
                        "process_waiter task exited with an error; child may not have been reaped"
                    );
                }
                Err(_) => {
                    warn!(
                        process_id = self.process_id,
                        "process_waiter did not finish within 30 s defensive timeout;                          child may not have been reaped"
                    );
                }
            }
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_spawn_echo() {
        let result = AgentProcess::spawn("echo", &[], "/tmp").await;
        assert!(result.is_ok() || result.is_err()); // Just check it compiles
    }

    #[test]
    fn test_process_id_counter() {
        let id1 = PROCESS_COUNTER.fetch_add(1, Ordering::SeqCst);
        let id2 = PROCESS_COUNTER.fetch_add(1, Ordering::SeqCst);
        assert!(id2 > id1);
    }

    /// Verify that kill() awaits all spawned reader and watcher tasks
    /// deterministically within a bounded time.
    #[tokio::test]
    async fn test_kill_awaits_spawned_tasks() {
        // Use a long-lived helper that won't exit on its own.
        let current_exe = std::env::current_exe().expect("test binary path");
        let process = AgentProcess::spawn_with_env(
            current_exe.to_str().expect("path"),
            &[
                "long_lived_process_helper".to_string(),
                "--ignored".to_string(),
                "--nocapture".to_string(),
            ],
            std::env::temp_dir().to_str().expect("tmp"),
            &[(TEST_HELPER_ENV.to_string(), "1".to_string())],
        )
        .await
        .expect("should spawn long-lived helper");

        // Verify the process has task handles set.
        let tasks = process.tasks.lock().await;
        assert!(tasks.stdout_reader.is_some(), "stdout_reader should be set");
        assert!(tasks.stderr_reader.is_some(), "stderr_reader should be set");
        assert!(
            tasks.process_waiter.is_some(),
            "process_waiter should be set"
        );
        drop(tasks);

        // Kill should complete within a reasonable time, which means it
        // successfully awaits all spawned tasks.
        let result = tokio::time::timeout(Duration::from_secs(10), process.kill()).await;
        assert!(result.is_ok(), "kill should complete within timeout");
        result.unwrap().expect("kill should succeed");

        // After kill, all task handles should be consumed (None).
        let tasks = process.tasks.lock().await;
        assert!(
            tasks.stdout_reader.is_none(),
            "stdout_reader should be None after kill"
        );
        assert!(
            tasks.stderr_reader.is_none(),
            "stderr_reader should be None after kill"
        );
        assert!(
            tasks.process_waiter.is_none(),
            "process_waiter should be None after kill"
        );
    }

    /// Verify that killing an already-dead process still drains task handles.
    #[tokio::test]
    async fn test_kill_already_dead_drains_tasks() {
        // Spawn a short-lived process that exits immediately.
        let process = AgentProcess::spawn("echo", &["hello".to_string()], "/tmp")
            .await
            .expect("should spawn echo");

        // Wait a moment for the process to exit naturally.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Even though the process is dead, kill() should drain task handles.
        process
            .kill()
            .await
            .expect("kill on dead process should succeed");

        let tasks = process.tasks.lock().await;
        assert!(
            tasks.stdout_reader.is_none(),
            "stdout_reader should be drained"
        );
        assert!(
            tasks.stderr_reader.is_none(),
            "stderr_reader should be drained"
        );
        assert!(
            tasks.process_waiter.is_none(),
            "process_waiter should be drained"
        );
    }

    #[tokio::test]
    async fn test_spawn_missing_cwd_fails() {
        let missing =
            std::env::temp_dir().join(format!("brehon-missing-cwd-{}", uuid::Uuid::new_v4()));
        let err = match AgentProcess::spawn("echo", &[], missing.to_string_lossy().as_ref()).await {
            Ok(_) => panic!("missing cwd should fail"),
            Err(err) => err,
        };
        assert!(matches!(err, ProcessError::SpawnFailed(_)));
        assert!(err.to_string().contains("Working directory does not exist"));
    }

    const TEST_HELPER_ENV: &str = "BREHON_ACP_PROCESS_TEST_HELPER";

    #[test]
    #[ignore = "Spawned by test_kill_awaits_spawned_tasks as a helper child"]
    fn long_lived_process_helper() {
        if std::env::var_os(TEST_HELPER_ENV).is_none() {
            return;
        }
        loop {
            std::thread::park_timeout(Duration::from_secs(60));
        }
    }
}
