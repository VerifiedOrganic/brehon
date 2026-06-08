//! Hardened git-subprocess runner used by MCP tools.
//!
//! Every git call that originates from MCP goes through [`run_git`] (or
//! [`run_git_with_stdin`] when we need to pipe a patch). The wrapper does
//! three things that raw `std::process::Command::new("git").output()`
//! doesn't:
//!
//! 1. **Non-interactive env.** Sets `GIT_TERMINAL_PROMPT=0`,
//!    `GIT_ASKPASS=echo`, `SSH_ASKPASS=echo`, `GIT_OPTIONAL_LOCKS=0`. Any
//!    credential-prompt, agent handshake, or ask-pass fallback fails fast
//!    instead of blocking on a TTY that the MCP subprocess doesn't have.
//! 2. **Pipe-draining while waiting.** stdout and stderr are drained on
//!    dedicated threads so git never blocks writing into a full pipe
//!    buffer (the classic 64 KiB deadlock that bites `.output()`-style
//!    callers on commands with large output, e.g. `git diff --binary`).
//! 3. **Wall-clock timeout.** If the subprocess runs longer than
//!    [`DEFAULT_GIT_TIMEOUT`], its process group is killed, the direct
//!    child is reaped, and pipe drains are bounded. The MCP tool surfaces
//!    that to the supervisor instead of wedging the tool handler forever.
//!
//! # Why this exists
//!
//! Before this helper, `task action=integrate` and every other MCP git
//! call went through a raw `Command::output()` with default env and no
//! timeout. A credential prompt, a stuck GPG agent, or a `.git/index.lock`
//! contention would hang the MCP tool indefinitely; the supervisor would
//! sit in "Enchanting..." with no recovery path short of killing the MCP
//! server. There is no legitimate reason for an MCP git call to block
//! interactively, so we cut that failure mode off at the source.
//!
//! # Tests
//!
//! The core wait/drain/timeout machinery is factored into
//! [`run_command_hardened`] so it can be exercised with `sh`/`sleep`/`cat`
//! in unit tests without requiring a real git repo. [`run_git`] is a
//! thin adapter over that core.

use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Wall-clock ceiling for any MCP-issued git command. 60 s is generous
/// for local-repo operations (status, rev-parse, merge-base, cherry-pick
/// on small diffs) and small enough that a hung git surfaces to the
/// supervisor promptly. Callers that know an operation will legitimately
/// take longer (e.g. full-repo initial cherry-pick sequence) can supply
/// a larger timeout via [`run_git_with_timeout`].
pub(crate) const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(60);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const PIPE_DRAIN_AFTER_KILL_TIMEOUT: Duration = Duration::from_millis(200);
const PROTECTED_BRANCH_BYPASS_DIR: &str = "protected-branch-bypass";

static PROTECTED_BRANCH_BYPASS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Run `git <args>` in `cwd`, returning the captured [`Output`] on exit
/// or a ready-to-display `String` error on spawn failure, I/O failure,
/// or timeout.
pub(crate) fn run_git(cwd: &Path, args: &[&str]) -> Result<Output, String> {
    run_command_hardened("git", cwd, args, None, DEFAULT_GIT_TIMEOUT)
}

/// Run a git command that is explicitly allowed to create a commit on a
/// protected branch. This is deliberately separate from [`run_git`] so
/// only narrow integration paths can bypass Brehon's repository hook guard.
pub(crate) fn run_git_allow_protected_branch_commit(
    cwd: &Path,
    args: &[&str],
) -> Result<Output, String> {
    let lease = create_protected_branch_bypass_lease(cwd)?;
    let token = lease.token.as_str();
    let bypass_dir = lease.path.parent().and_then(Path::to_str).ok_or_else(|| {
        format!(
            "Failed to expose protected-branch bypass dir for {}",
            lease.path.display()
        )
    })?;
    run_command_hardened_with_env(
        "git",
        cwd,
        args,
        None,
        DEFAULT_GIT_TIMEOUT,
        &[
            ("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT", "1"),
            ("BREHON_PROTECTED_BRANCH_BYPASS_TOKEN", token),
            ("BREHON_PROTECTED_BRANCH_BYPASS_DIR", bypass_dir),
        ],
    )
}

struct ProtectedBranchBypassLease {
    path: PathBuf,
    token: String,
}

impl Drop for ProtectedBranchBypassLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_protected_branch_bypass_lease(cwd: &Path) -> Result<ProtectedBranchBypassLease, String> {
    let git_common_dir = git_common_dir(cwd)?;
    let bypass_dir = git_common_dir
        .join("brehon")
        .join(PROTECTED_BRANCH_BYPASS_DIR);
    fs::create_dir_all(&bypass_dir).map_err(|err| {
        format!(
            "Failed to create protected-branch bypass dir {}: {err}",
            bypass_dir.display()
        )
    })?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = PROTECTED_BRANCH_BYPASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let token = format!("{}-{now_ms}-{counter}", std::process::id());
    let path = bypass_dir.join(&token);
    let content = format!("pid={}\ncreated_ms={now_ms}\n", std::process::id());
    fs::write(&path, content).map_err(|err| {
        format!(
            "Failed to create protected-branch bypass lease {}: {err}",
            path.display()
        )
    })?;

    Ok(ProtectedBranchBypassLease { path, token })
}

fn git_common_dir(cwd: &Path) -> Result<PathBuf, String> {
    let output = run_command_hardened(
        "git",
        cwd,
        &["rev-parse", "--git-common-dir"],
        None,
        DEFAULT_GIT_TIMEOUT,
    )?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse --git-common-dir failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return Err("git rev-parse --git-common-dir returned an empty path".to_string());
    }

    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(cwd.join(path))
    }
}

/// Same as [`run_git`] but with a caller-specified timeout. Use when
/// the operation is known to sometimes take longer than the default
/// (e.g. a full-history `git log` sweep).
#[allow(dead_code)]
pub(crate) fn run_git_with_timeout(
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, String> {
    run_command_hardened("git", cwd, args, None, timeout)
}

/// Variant that pipes `stdin_bytes` to git's stdin — used for `git apply
/// -` style patch-feeding.
pub(crate) fn run_git_with_stdin(
    cwd: &Path,
    args: &[&str],
    stdin_bytes: &[u8],
) -> Result<Output, String> {
    run_command_hardened("git", cwd, args, Some(stdin_bytes), DEFAULT_GIT_TIMEOUT)
}

/// Generic subprocess runner: spawns `program` with `args` in `cwd`,
/// drains stdout/stderr on reader threads, enforces `timeout`, and kills
/// the child on timeout. When `program == "git"` the non-interactive
/// env hardening is applied.
///
/// Exposed at `pub(crate)` visibility (rather than `pub(super)`) so that
/// unit tests in this module can exercise the wait/drain/timeout core
/// with `sh`, `sleep`, `cat`, etc. without needing a real git repo.
pub(crate) fn run_command_hardened(
    program: &str,
    cwd: &Path,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, String> {
    run_command_hardened_with_env(program, cwd, args, stdin_bytes, timeout, &[])
}

fn run_command_hardened_with_env(
    program: &str,
    cwd: &Path,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
    extra_env: &[(&str, &str)],
) -> Result<Output, String> {
    let args_joined = args.join(" ");

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if program == "git" {
        apply_git_non_interactive_env(&mut cmd);
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    // Put the subprocess in its own process group so timeout cleanup can
    // terminate git's helper children too. Otherwise a descendant can keep
    // stdout/stderr pipe fds open after the direct child is killed.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("Failed to spawn {program} {args_joined}: {err}"))?;
    let child_pid = child.id();

    // Feed stdin immediately and close it so the child sees EOF. Must
    // happen BEFORE we park on wait, or git would block reading.
    if let Some(bytes) = stdin_bytes {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(bytes).map_err(|err| {
                format!("Failed to write stdin to {program} {args_joined}: {err}")
            })?;
            // Dropping `stdin` here closes the pipe, signaling EOF.
        }
    }

    // Drain stdout/stderr on reader threads. This is the "pipe full ->
    // subprocess blocks forever" defense. For commands with small
    // output the overhead is trivial (two thread spawns); for commands
    // with large output (`git diff --binary`, `git log --all`) it's
    // the difference between working and deadlocking.
    let stdout_drain = child
        .stdout
        .take()
        .map(|pipe| PipeDrain::spawn("stdout", pipe));
    let stderr_drain = child
        .stderr
        .take()
        .map(|pipe| PipeDrain::spawn("stderr", pipe));

    let started_at = Instant::now();
    let poll_interval = Duration::from_millis(25);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started_at.elapsed() >= timeout {
                    kill_process_tree(&mut child, child_pid);
                    let _ = child.wait();

                    let stdout_bytes = stdout_drain
                        .map(PipeDrain::collect_after_kill)
                        .unwrap_or_default()
                        .len();
                    let stderr_bytes = stderr_drain
                        .map(PipeDrain::collect_after_kill)
                        .unwrap_or_default()
                        .len();

                    tracing::warn!(
                        program = %program,
                        args = %args_joined,
                        timeout_secs = timeout.as_secs(),
                        stdout_bytes,
                        stderr_bytes,
                        "MCP subprocess timed out; killed"
                    );
                    return Err(format!(
                        "{program} {args_joined} timed out after {}s and was killed",
                        timeout.as_secs()
                    ));
                }
                thread::sleep(poll_interval);
            }
            Err(err) => {
                return Err(format!("Failed to wait for {program} {args_joined}: {err}"));
            }
        }
    };

    // Process exited. Collect buffered output, but never wait forever: a
    // leaked child process can inherit pipe writers and keep them open.
    let stdout = stdout_drain
        .map(|drain| drain.collect(program, &args_joined))
        .transpose()?
        .unwrap_or_default();
    let stderr = stderr_drain
        .map(|drain| drain.collect(program, &args_joined))
        .transpose()?
        .unwrap_or_default();

    tracing::debug!(
        program = %program,
        args = %args_joined,
        status = %status,
        stdout_bytes = stdout.len(),
        stderr_bytes = stderr.len(),
        "MCP subprocess completed"
    );

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

struct PipeDrain {
    stream_name: &'static str,
    rx: Receiver<Vec<u8>>,
}

impl PipeDrain {
    fn spawn<R>(stream_name: &'static str, mut pipe: R) -> Self
    where
        R: Read + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::copy(&mut pipe, &mut buf);
            let _ = tx.send(buf);
        });
        Self { stream_name, rx }
    }

    fn collect(self, program: &str, args_joined: &str) -> Result<Vec<u8>, String> {
        match self.rx.recv_timeout(PIPE_DRAIN_TIMEOUT) {
            Ok(buf) => Ok(buf),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(Vec::new()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "{program} {args_joined} exited but {} pipe did not close within {}ms; possible child process inherited stdio",
                self.stream_name,
                PIPE_DRAIN_TIMEOUT.as_millis()
            )),
        }
    }

    fn collect_after_kill(self) -> Vec<u8> {
        self.rx
            .recv_timeout(PIPE_DRAIN_AFTER_KILL_TIMEOUT)
            .unwrap_or_default()
    }
}

#[cfg(unix)]
fn signal_process_tree(pid: u32, signal: libc::c_int) {
    let pid = pid as libc::pid_t;
    // Prefer the process group created in pre_exec, but also signal the direct
    // child in case group setup failed or the child moved groups.
    let group_result = unsafe { libc::kill(-pid, signal) };
    let group_error = (group_result != 0).then(|| std::io::Error::last_os_error().to_string());
    let child_result = unsafe { libc::kill(pid, signal) };
    let child_error = (child_result != 0).then(|| std::io::Error::last_os_error().to_string());
    if group_result != 0 && child_result != 0 {
        tracing::warn!(
            pid,
            signal,
            ?group_error,
            ?child_error,
            "Failed to signal MCP subprocess"
        );
    }
}

#[cfg(unix)]
fn kill_process_tree(child: &mut std::process::Child, pid: u32) {
    signal_process_tree(pid, libc::SIGKILL);
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_process_tree(child: &mut std::process::Child, _pid: u32) {
    let _ = child.kill();
}

/// Apply the non-interactive env overrides to a git subprocess.
///
/// `GIT_TERMINAL_PROMPT=0` prevents git from prompting on the terminal
/// for credentials. `GIT_ASKPASS=echo` (and `SSH_ASKPASS=echo`) prevent
/// fallback credential helpers from prompting when terminal is not an
/// option — `echo` exits 0 with empty output, which git treats as a
/// blank password and fails fast. `GIT_OPTIONAL_LOCKS=0` disables
/// optional `.git/index.lock` acquisition for read-only commands, so
/// a probe from this helper doesn't contend with a worker's active
/// write and a probe from a worker doesn't contend with our read.
fn apply_git_non_interactive_env(cmd: &mut Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "echo")
        .env("SSH_ASKPASS", "echo")
        .env("GIT_OPTIONAL_LOCKS", "0");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper returning a short timeout for timeout-path tests.
    const TEST_TIMEOUT_SHORT: Duration = Duration::from_millis(400);
    /// Helper returning a plenty-of-headroom timeout for success-path tests.
    const TEST_TIMEOUT_LONG: Duration = Duration::from_secs(10);

    fn tmp_cwd() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn runs_command_and_captures_stdout() {
        let dir = tmp_cwd();
        let out = run_command_hardened(
            "sh",
            dir.path(),
            &["-c", "printf 'hello world'"],
            None,
            TEST_TIMEOUT_LONG,
        )
        .expect("ran");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello world");
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn captures_stderr_separately_from_stdout() {
        let dir = tmp_cwd();
        let out = run_command_hardened(
            "sh",
            dir.path(),
            &["-c", "printf 'to-out'; printf 'to-err' 1>&2"],
            None,
            TEST_TIMEOUT_LONG,
        )
        .expect("ran");
        assert_eq!(out.stdout, b"to-out");
        assert_eq!(out.stderr, b"to-err");
    }

    #[test]
    fn non_zero_exit_status_propagates() {
        let dir = tmp_cwd();
        let out =
            run_command_hardened("sh", dir.path(), &["-c", "exit 7"], None, TEST_TIMEOUT_LONG)
                .expect("ran");
        assert!(!out.status.success());
        assert_eq!(out.status.code(), Some(7));
    }

    #[test]
    fn large_stdout_does_not_deadlock_pipe() {
        // Emit well over the default 64 KiB pipe buffer in one burst.
        // If pipes weren't drained while we waited, the child would
        // block on write and we'd hit the timeout instead of success.
        let dir = tmp_cwd();
        let out = run_command_hardened(
            "sh",
            dir.path(),
            &[
                "-c",
                "yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -c 300000",
            ],
            None,
            TEST_TIMEOUT_LONG,
        )
        .expect("ran");
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 300_000);
    }

    #[test]
    fn timeout_fires_on_hung_subprocess_and_kills_child() {
        let dir = tmp_cwd();
        let err = run_command_hardened(
            "sh",
            dir.path(),
            &["-c", "sleep 30"],
            None,
            TEST_TIMEOUT_SHORT,
        )
        .expect_err("expected timeout");
        assert!(
            err.contains("timed out"),
            "error should name the timeout path: {err}"
        );
        // Timeout error must also name the command for operator
        // diagnosis. Callers rely on this for supervisor messages.
        assert!(err.contains("sleep 30"), "error missing args: {err}");
    }

    #[test]
    fn timeout_does_not_hang_when_descendant_holds_pipe_open() {
        let dir = tmp_cwd();
        let started_at = Instant::now();
        let err = run_command_hardened(
            "sh",
            dir.path(),
            &["-c", "sleep 30 & echo $! > leaked-pipe.pid; sleep 30"],
            None,
            TEST_TIMEOUT_SHORT,
        )
        .expect_err("expected timeout");
        let elapsed = started_at.elapsed();
        cleanup_test_pid(dir.path().join("leaked-pipe.pid"));

        assert!(
            err.contains("timed out"),
            "error should name the timeout path: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "timeout path waited on leaked pipe for {elapsed:?}"
        );
    }

    #[test]
    fn exited_process_with_leaked_pipe_returns_error_instead_of_hanging() {
        let dir = tmp_cwd();
        let started_at = Instant::now();
        let err = run_command_hardened(
            "sh",
            dir.path(),
            &["-c", "sleep 30 & echo $! > leaked-pipe.pid"],
            None,
            TEST_TIMEOUT_LONG,
        )
        .expect_err("expected leaked pipe error");
        let elapsed = started_at.elapsed();
        cleanup_test_pid(dir.path().join("leaked-pipe.pid"));

        assert!(
            err.contains("pipe did not close"),
            "error should identify the leaked pipe: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "pipe drain waited indefinitely for {elapsed:?}"
        );
    }

    #[test]
    fn stdin_is_piped_and_closed_so_reader_sees_eof() {
        let dir = tmp_cwd();
        let out = run_command_hardened(
            "cat",
            dir.path(),
            &[],
            Some(b"piped-input\n"),
            TEST_TIMEOUT_LONG,
        )
        .expect("ran");
        assert!(out.status.success());
        assert_eq!(out.stdout, b"piped-input\n");
    }

    #[cfg(unix)]
    fn cleanup_test_pid(path: PathBuf) {
        let Ok(pid_raw) = fs::read_to_string(path) else {
            return;
        };
        let Ok(pid) = pid_raw.trim().parse::<libc::pid_t>() else {
            return;
        };
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    #[cfg(not(unix))]
    fn cleanup_test_pid(_path: PathBuf) {}

    #[test]
    fn git_wrapper_sets_non_interactive_env() {
        // Spawn git with a config probe that would normally prompt if
        // credentials were needed. We can't easily assert "no prompt
        // appeared" in-process, so instead verify the env is set by
        // asking the shell to echo it via a non-git command run with
        // the same env-application helper applied manually.
        let dir = tmp_cwd();
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "echo PROMPT=${GIT_TERMINAL_PROMPT:-unset} ASKPASS=${GIT_ASKPASS:-unset} \
             OPTLOCKS=${GIT_OPTIONAL_LOCKS:-unset}",
        ])
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
        apply_git_non_interactive_env(&mut cmd);
        let out = cmd.output().expect("ran");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("PROMPT=0"), "stdout: {stdout}");
        assert!(stdout.contains("ASKPASS=echo"), "stdout: {stdout}");
        assert!(stdout.contains("OPTLOCKS=0"), "stdout: {stdout}");
    }

    #[test]
    fn run_git_invokes_real_git_with_args() {
        // Smoke test: `git --version` always succeeds, costs nothing,
        // and proves the helper is wired through to an actual git
        // binary. Anything more specific would require a test repo
        // fixture; those live in the per-module migration tests.
        let dir = tmp_cwd();
        let out = run_git(dir.path(), &["--version"]).expect("ran");
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stdout).starts_with("git version"),
            "stdout did not start with 'git version': {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
