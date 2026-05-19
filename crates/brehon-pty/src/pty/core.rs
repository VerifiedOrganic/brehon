use crate::error::{Error, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::process::Command;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use super::config::PtyConfig;
use super::dump::DumpWriter;

use std::sync::OnceLock;

static PTY_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn pty_runtime() -> &'static tokio::runtime::Runtime {
    PTY_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to create PTY runtime")
    })
}

const SCRUBBED_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_ORG_ID",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_SECURITY_TOKEN",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GITLAB_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_CLIENT_SECRET",
    "AZURE_TENANT_ID",
    "VOYAGE_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GROQ_API_KEY",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
];

fn should_scrub_env_key(explicit_env_keys: &HashSet<&str>, key: &str) -> bool {
    !explicit_env_keys.contains(key)
}

/// Events emitted by a PTY
#[derive(Debug, Clone)]
pub enum PtyEvent {
    /// Terminal output (raw bytes - parsing done by ghostty_vt)
    Output(Vec<u8>),
    /// Child emitted CSI 6 n / CSI ? 6 n cursor-position query. The consumer
    /// owns the parser and is responsible for writing `\x1b[{row};{col}R`
    /// back to the PTY using its real cursor position. Emitted after the
    /// preceding Output frames for the same read chunk so the parser has
    /// seen the bytes that immediately preceded the query.
    CursorPositionRequested,
    /// Process exited
    Exited(Option<i32>),
    /// Error occurred
    Error(String),
}

/// A running PTY process
pub struct Pty {
    /// Unique identifier
    id: String,
    /// Writer handle for sending input
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Channel for receiving raw output
    event_rx: mpsc::Receiver<PtyEvent>,
    /// Handle to the reader task — owned for explicit shutdown.
    /// `None` after `kill()` has reaped it.
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// Output frames dropped because the bounded PTY event channel was full.
    dropped_output_frames: Arc<AtomicU64>,
    /// Output bytes dropped because the bounded PTY event channel was full.
    dropped_output_bytes: Arc<AtomicU64>,
    /// Child process handle (shared with reader thread for exit code collection)
    child: Arc<std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    /// Master PTY (keep alive)
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Whether this PTY is running Codex CLI
    is_codex: bool,
    /// Whether this PTY is running Gemini CLI
    is_gemini: bool,
}

impl Pty {
    /// Spawn a new PTY with the given configuration
    pub fn spawn(id: impl Into<String>, config: PtyConfig) -> Result<Self> {
        let id = id.into();
        let is_codex = config.command == "codex";
        let is_gemini = config.command == "gemini";

        // Create PTY system and open a PTY pair
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: config.rows,
                cols: config.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::pty(format!("Failed to open PTY: {e}")))?;

        // Build command
        let mut cmd = CommandBuilder::new(&config.command);
        cmd.args(&config.args);

        if let Some(cwd) = &config.cwd {
            cmd.cwd(cwd);
        }

        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        let explicit_env_keys: HashSet<&str> =
            config.env.iter().map(|(key, _)| key.as_str()).collect();

        // Strip CLAUDECODE to prevent nested-session detection in spawned Claude CLI
        cmd.env_remove("CLAUDECODE");

        // Environment scrubbing: strip sensitive credentials from worker processes.
        // Workers authenticate via their CLI's own config (claude /login, codex config),
        // not environment variables. Stripping prevents credential leakage to autonomous
        // workers and avoids interactive API key selection dialogs.
        for key in SCRUBBED_ENV_KEYS {
            if should_scrub_env_key(&explicit_env_keys, key) {
                cmd.env_remove(key);
            }
        }

        // Spawn the child process
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::pty(format!("Failed to spawn command: {e}")))?;

        // Drop slave - the child process owns it now
        drop(pair.slave);

        // Get reader and writer
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| Error::pty(format!("Failed to clone reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| Error::pty(format!("Failed to get writer: {e}")))?;

        let writer = Arc::new(Mutex::new(writer));

        // Create channel for events - larger buffer for multi-agent scenarios
        let (event_tx, event_rx) = mpsc::channel::<PtyEvent>(1024);

        let child = Arc::new(std::sync::Mutex::new(child));
        let dropped_output_frames = Arc::new(AtomicU64::new(0));
        let dropped_output_bytes = Arc::new(AtomicU64::new(0));

        // Opt-in raw-stream capture for rendering diagnostics. Gated on
        // BREHON_PTY_DUMP_DIR so the common case pays no cost (Option::None).
        let dump = DumpWriter::from_env(
            &id,
            &config.command,
            &config.args,
            config.cwd.as_deref(),
            config.rows,
            config.cols,
        )
        .map(Arc::new);

        // Spawn reader task - sends raw bytes, no parsing
        let reader_handle = pty_runtime().spawn_blocking({
            let child = Arc::clone(&child);
            let dump = dump.clone();
            let dropped_output_frames = Arc::clone(&dropped_output_frames);
            let dropped_output_bytes = Arc::clone(&dropped_output_bytes);
            move || {
                Self::reader_loop(
                    reader,
                    event_tx,
                    child,
                    dump,
                    dropped_output_frames,
                    dropped_output_bytes,
                );
            }
        });

        Ok(Self {
            id,
            writer,
            event_rx,
            reader_handle: Some(reader_handle),
            dropped_output_frames,
            dropped_output_bytes,
            child,
            master: pair.master,
            is_codex,
            is_gemini,
        })
    }

    /// Reader loop that forwards raw PTY output
    fn reader_loop(
        mut reader: Box<dyn Read + Send>,
        event_tx: mpsc::Sender<PtyEvent>,
        child: Arc<std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
        dump: Option<Arc<DumpWriter>>,
        dropped_output_frames: Arc<AtomicU64>,
        dropped_output_bytes: Arc<AtomicU64>,
    ) {
        // Larger buffer for high-throughput scenarios (6 Claudes generating long responses)
        let mut buf = [0u8; 16384];
        let mut carry: Vec<u8> = Vec::new();
        let mut sync_filter = SyncOutputFilter::new();

        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    // EOF - process exited; collect actual exit code
                    if !carry.is_empty() {
                        // Feed the tail through the sync filter so any
                        // partially-matched sync marker in `carry` has a
                        // final chance to complete.
                        for frame in sync_filter.process(&std::mem::take(&mut carry)) {
                            if !try_send_pty_event(
                                &event_tx,
                                PtyEvent::Output(frame),
                                &dropped_output_frames,
                                &dropped_output_bytes,
                            ) {
                                break;
                            }
                        }
                    }
                    if let Some(pending_frame) = sync_filter.drain() {
                        let _ = try_send_pty_event(
                            &event_tx,
                            PtyEvent::Output(pending_frame),
                            &dropped_output_frames,
                            &dropped_output_bytes,
                        );
                    }
                    let exit_code = child
                        .lock()
                        .ok()
                        .and_then(|mut c| c.try_wait().ok().flatten())
                        .map(|status| status.exit_code() as i32);
                    let _ = try_send_pty_event(
                        &event_tx,
                        PtyEvent::Exited(exit_code),
                        &dropped_output_frames,
                        &dropped_output_bytes,
                    );
                    break;
                }
                Ok(n) => {
                    // Capture raw bytes BEFORE any filtering so the dump reflects
                    // exactly what the child emitted. The filters strip CPR query
                    // bytes and coalesce synchronized-output frames; capturing
                    // post-filter would lose fidelity for diagnostics.
                    if let Some(dump) = dump.as_ref() {
                        dump.record_read(&buf[..n]);
                    }

                    // Filter 1: strip CPR query bytes from the data stream.
                    // The pane (which owns the parser) replies after the
                    // preceding bytes have been fed, via the
                    // CursorPositionRequested event emitted below.
                    // Must run before the sync filter so CPR bytes inside a
                    // sync block don't get buffered as frame content.
                    let (post_cpr, new_carry, saw_cpr) =
                        filter_cursor_position_requests(&carry, &buf[..n]);
                    carry = new_carry;

                    // Filter 2: coalesce synchronized-output (DEC 2026) frames.
                    // Claude's Ink TUI wraps each visual frame in
                    // `\x1b[?2026h`…`\x1b[?2026l`; emitting the bytes between
                    // markers as one atomic chunk prevents downstream
                    // emulators (ghostty_vt's readonly stream no-ops this
                    // mode) from exposing intermediate frame state.
                    let mut send_failed = false;
                    for frame in sync_filter.process(&post_cpr) {
                        if !try_send_pty_event(
                            &event_tx,
                            PtyEvent::Output(frame),
                            &dropped_output_frames,
                            &dropped_output_bytes,
                        ) {
                            send_failed = true;
                            break;
                        }
                    }
                    if send_failed {
                        break;
                    }

                    // Surface the CPR query AFTER the output frames so the
                    // consumer's parser has seen the preceding bytes (and
                    // therefore advanced its cursor) before being asked for
                    // a position. Multiple CPR queries in one chunk are
                    // coalesced into one event; Ink typically only cares
                    // about the most-recent answer per redraw cycle.
                    if saw_cpr
                        && !try_send_pty_event(
                            &event_tx,
                            PtyEvent::CursorPositionRequested,
                            &dropped_output_frames,
                            &dropped_output_bytes,
                        )
                    {
                        break;
                    }
                }
                Err(e) => {
                    let _ = try_send_pty_event(
                        &event_tx,
                        PtyEvent::Error(e.to_string()),
                        &dropped_output_frames,
                        &dropped_output_bytes,
                    );
                    break;
                }
            }
        }
    }

    /// Get the PTY's identifier
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns true when this PTY is running Codex CLI.
    pub fn is_codex(&self) -> bool {
        self.is_codex
    }

    /// Returns true when this PTY is running Gemini CLI.
    pub fn is_gemini(&self) -> bool {
        self.is_gemini
    }

    pub fn dropped_output_frames(&self) -> u64 {
        self.dropped_output_frames.load(Ordering::SeqCst)
    }

    pub fn dropped_output_bytes(&self) -> u64 {
        self.dropped_output_bytes.load(Ordering::SeqCst)
    }

    /// Get a clone of the writer handle (for concurrent writing)
    pub fn writer_handle(&self) -> Arc<Mutex<Box<dyn Write + Send>>> {
        self.writer.clone()
    }

    /// Write input to the PTY (for prompt injection).
    ///
    /// Backed by a `std::sync::Mutex` so the TUI thread can dispatch
    /// keystrokes without the deadlock hazard of `blocking_lock()` on
    /// a `tokio::sync::Mutex`. The critical section is a single sync
    /// `write_all` + `flush` on `portable_pty`'s `Box<dyn Write + Send>`,
    /// which never `.await`s — holding a sync mutex around it is correct
    /// and cheap.
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().expect("PTY writer mutex poisoned");
        writer
            .write_all(data)
            .map_err(|e| Error::pty(format!("Write failed: {e}")))?;
        writer
            .flush()
            .map_err(|e| Error::pty(format!("Flush failed: {e}")))?;
        Ok(())
    }

    /// Write a string to the PTY
    pub async fn write_str(&self, s: &str) -> Result<()> {
        self.write(s.as_bytes()).await
    }

    /// Send a line of input (appends carriage return to submit, same as Enter key)
    pub async fn send_line(&self, line: &str) -> Result<()> {
        self.write_str(&format!("{line}\r")).await
    }

    /// Receive the next event from the PTY (blocking)
    pub async fn recv(&mut self) -> Option<PtyEvent> {
        self.event_rx.recv().await
    }

    /// Try to receive an event from the PTY (non-blocking)
    pub fn try_recv(&mut self) -> Option<PtyEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Resize the PTY
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::pty(format!("Resize failed: {e}")))
    }

    /// Send Ctrl+C to the process
    pub async fn interrupt(&self) -> Result<()> {
        self.write(&[0x03]).await
    }

    /// Send Ctrl+D (EOF) to the process
    pub async fn send_eof(&self) -> Result<()> {
        self.write(&[0x04]).await
    }

    /// Kill the child process and abort the owned tasks.
    ///
    /// The reader task is aborted rather than synchronously joined so that
    /// this method never blocks a Tokio worker thread; it would otherwise
    /// exit naturally once the PTY fd sees EOF (after the child is killed).
    pub fn kill(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = kill_child(child.as_mut());
        }

        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.kill();
    }
}

fn try_send_pty_event(
    event_tx: &mpsc::Sender<PtyEvent>,
    event: PtyEvent,
    dropped_output_frames: &AtomicU64,
    dropped_output_bytes: &AtomicU64,
) -> bool {
    match event_tx.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Full(PtyEvent::Output(frame))) => {
            let bytes = frame.len() as u64;
            let frames = dropped_output_frames.fetch_add(1, Ordering::SeqCst) + 1;
            let total_bytes = dropped_output_bytes.fetch_add(bytes, Ordering::SeqCst) + bytes;
            if frames == 1 || frames.is_power_of_two() {
                tracing::warn!(
                    dropped_frames = frames,
                    dropped_bytes = total_bytes,
                    "PTY event channel full; dropping output frame to avoid reader backpressure deadlock"
                );
            }
            true
        }
        Err(TrySendError::Full(event)) => {
            tracing::warn!(
                event = ?event,
                "PTY event channel full; dropping non-output event"
            );
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

pub(crate) fn kill_child(child: &mut dyn portable_pty::Child) -> Result<()> {
    if child
        .try_wait()
        .map_err(|e| Error::pty(format!("Failed to poll child before kill: {e}")))?
        .is_some()
    {
        return Ok(());
    }

    let descendant_pids = child
        .process_id()
        .map(kill_descendant_enumeration)
        .transpose()
        .unwrap_or(None)
        .unwrap_or_default();

    if !descendant_pids.is_empty() {
        let _ = signal_processes("TERM", &descendant_pids);
        std::thread::sleep(Duration::from_millis(100));
    }

    child
        .kill()
        .map_err(|e| Error::pty(format!("Failed to kill child process: {e}")))?;
    child
        .wait()
        .map_err(|e| Error::pty(format!("Failed to reap child process after kill: {e}")))?;

    if !descendant_pids.is_empty() {
        let survivors = running_pids(&descendant_pids).unwrap_or(descendant_pids);
        if !survivors.is_empty() {
            let _ = signal_processes("KILL", &survivors);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn kill_descendant_enumeration(root_pid: u32) -> Result<Vec<u32>> {
    list_descendant_process_ids(root_pid)
}

#[cfg(not(unix))]
fn kill_descendant_enumeration(_root_pid: u32) -> Result<Vec<u32>> {
    Ok(Vec::new())
}

fn parse_process_snapshot(snapshot: &str) -> HashMap<u32, Vec<u32>> {
    let mut children_by_parent = HashMap::new();
    for line in snapshot.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        children_by_parent
            .entry(ppid)
            .or_insert_with(Vec::new)
            .push(pid);
    }
    children_by_parent
}

pub(crate) fn descendant_process_ids_from_snapshot(snapshot: &str, root_pid: u32) -> Vec<u32> {
    let children_by_parent = parse_process_snapshot(snapshot);
    let mut stack = children_by_parent
        .get(&root_pid)
        .cloned()
        .unwrap_or_default();
    let mut descendants = Vec::new();

    while let Some(pid) = stack.pop() {
        descendants.push(pid);
        if let Some(children) = children_by_parent.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }

    descendants
}

fn list_descendant_process_ids(root_pid: u32) -> Result<Vec<u32>> {
    let output = Command::new("ps")
        .args(["-Ao", "pid=,ppid="])
        .output()
        .map_err(|e| Error::pty(format!("Failed to enumerate process tree: {e}")))?;
    if !output.status.success() {
        return Err(Error::pty(format!(
            "Process enumeration failed with status {}",
            output.status.code().unwrap_or_default()
        )));
    }
    Ok(descendant_process_ids_from_snapshot(
        &String::from_utf8_lossy(&output.stdout),
        root_pid,
    ))
}

#[cfg(unix)]
fn running_pids(pids: &[u32]) -> Result<Vec<u32>> {
    if pids.is_empty() {
        return Ok(Vec::new());
    }

    let pid_args: Vec<String> = pids.iter().map(u32::to_string).collect();
    let output = Command::new("ps")
        .arg("-p")
        .args(&pid_args)
        .arg("-o")
        .arg("pid=")
        .output()
        .map_err(|e| Error::pty(format!("Failed to check running processes: {e}")))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect())
}

#[cfg(not(unix))]
fn running_pids(_pids: &[u32]) -> Result<Vec<u32>> {
    Ok(Vec::new())
}

#[cfg(unix)]
fn signal_processes(signal: &str, pids: &[u32]) -> Result<()> {
    if pids.is_empty() {
        return Ok(());
    }

    let mut command = Command::new("kill");
    command.arg(format!("-{signal}"));
    for pid in pids {
        command.arg(pid.to_string());
    }

    let output = command
        .output()
        .map_err(|e| Error::pty(format!("Failed to run kill -{signal}: {e}")))?;
    if !output.status.success() {
        return Err(Error::pty(format!(
            "kill -{signal} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(())
}

#[cfg(not(unix))]
fn signal_processes(_signal: &str, _pids: &[u32]) -> Result<()> {
    Ok(())
}

/// Format a CSI cursor-position-report response (`\x1b[{row};{col}R`).
///
/// Both `row` and `col` are 1-indexed, matching the convention used by
/// ghostty_vt's `Terminal::cursor_position()` and the underlying
/// xterm-style CSI sequence. The reply matches both `CSI 6 n` (DSR) and
/// `CSI ? 6 n` (DECDSR) queries — Ink only checks the row/col fields.
pub fn format_cursor_position_report(row: u16, col: u16) -> Vec<u8> {
    // Clamp to 1-indexed minimum so a parser that hasn't seen any input
    // yet (and therefore reports (0, 0)) doesn't emit a nonsense reply.
    let row = row.max(1);
    let col = col.max(1);
    format!("\x1b[{row};{col}R").into_bytes()
}

pub(crate) fn filter_cursor_position_requests(
    carry: &[u8],
    chunk: &[u8],
) -> (Vec<u8>, Vec<u8>, bool) {
    const CPR: [u8; 4] = [0x1b, 0x5b, 0x36, 0x6e]; // ESC [ 6 n
    const CPR_ALT: [u8; 5] = [0x1b, 0x5b, 0x3f, 0x36, 0x6e]; // ESC [ ? 6 n
    let max_seq = CPR_ALT.len();

    let total_len = carry.len() + chunk.len();
    if total_len == 0 {
        return (Vec::new(), Vec::new(), false);
    }

    let process_len = total_len.saturating_sub(max_seq - 1);
    let mut out = Vec::with_capacity(process_len);
    let mut i = 0usize;
    let mut saw_cpr = false;

    let byte_at = |idx: usize| -> u8 {
        if idx < carry.len() {
            carry[idx]
        } else {
            chunk[idx - carry.len()]
        }
    };

    while i < process_len {
        if i + CPR_ALT.len() <= total_len {
            let mut matches = true;
            for (j, byte) in CPR_ALT.iter().enumerate() {
                if byte_at(i + j) != *byte {
                    matches = false;
                    break;
                }
            }
            if matches {
                saw_cpr = true;
                i += CPR_ALT.len();
                continue;
            }
        }
        if i + CPR.len() <= total_len {
            let mut matches = true;
            for (j, byte) in CPR.iter().enumerate() {
                if byte_at(i + j) != *byte {
                    matches = false;
                    break;
                }
            }
            if matches {
                saw_cpr = true;
                i += CPR.len();
                continue;
            }
        }
        out.push(byte_at(i));
        i += 1;
    }

    let mut new_carry = Vec::with_capacity(total_len - process_len);
    for idx in process_len..total_len {
        new_carry.push(byte_at(idx));
    }

    (out, new_carry, saw_cpr)
}

/// Synchronized Output mode (DEC mode 2026) buffering filter.
///
/// # Background
///
/// TUI frameworks like Ink (Claude Code) wrap each visual frame in
/// `\x1b[?2026h` … `\x1b[?2026l` pairs to tell the terminal "don't paint
/// intermediate state between these markers — show the complete frame
/// atomically." The upstream ghostty terminal library supports this mode,
/// but the readonly-observer stream variant used by `ghostty_vt` silently
/// no-ops the mode change (see `stream_readonly.zig` in the vendored source).
/// That left Brehon's supervisor panel rendering every intermediate cursor
/// movement and write between the markers — classic Ink garbling, most
/// visible on Claude Code startup where ~420 sync-framed updates arrive in
/// the first couple of seconds.
///
/// # What this filter does
///
/// While **not** in sync mode, bytes pass through unchanged. When
/// `\x1b[?2026h` is observed, the filter enters sync mode, strips the
/// marker, and accumulates subsequent bytes into an internal buffer without
/// forwarding them. When `\x1b[?2026l` arrives the filter strips that
/// marker too and flushes the accumulated buffer as one atomic chunk —
/// so the downstream emulator and Brehon's viewport scraper only ever see
/// the completed frame, never the intermediate states.
///
/// # Safety valves
///
/// * If a sync block exceeds `MAX_SYNC_BUFFER_BYTES` (512 KiB), flush
///   early. Protects against misbehaving CLIs that never emit `2026l`.
/// * If a sync block is open for more than `MAX_SYNC_DURATION` (1 s),
///   flush on the next chunk. Protects against CLIs that crash mid-frame.
///
/// The spec (https://gist.github.com/christianparpart/d8a62cc1ab659194337d73c20c1e3d89)
/// says sync blocks don't nest; observed `2026h` while already in sync is
/// treated as a no-op marker strip, preserving the current block.
pub(crate) struct SyncOutputFilter {
    in_sync: bool,
    buffer: Vec<u8>,
    sync_started_at: Option<std::time::Instant>,
    carry: Vec<u8>,
}

const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
/// The first 7 bytes of both SYNC_BEGIN and SYNC_END are identical — only the
/// final byte (`h` vs `l`) differs. A chunk tail is a potential marker
/// prefix iff it is a proper prefix of this shared prefix.
const SYNC_SHARED_PREFIX: &[u8] = b"\x1b[?2026";
const MAX_SYNC_BUFFER_BYTES: usize = 512 * 1024;
const MAX_SYNC_DURATION: Duration = Duration::from_secs(1);

/// Return true iff `tail` is a proper prefix of either `SYNC_BEGIN` or
/// `SYNC_END` — i.e. it's worth waiting for the next chunk to see if
/// it completes into a sync marker.
///
/// This is the hot path for keystroke-echo latency. Common cases that must
/// return `false`:
/// * an SGR reset (`\x1b[0m`) ending a styled string
/// * a cursor move (`\x1b[nA`, `\x1b[n;mH`)
/// * any other non-`?`-led CSI sequence
///
/// Before this check existed, every ESC in a chunk tail caused the
/// surrounding bytes to be held until the next chunk arrived — adding a
/// kernel-pipe-roundtrip of latency to every typing-echo update on
/// Claude-driven panes.
fn is_potential_sync_marker_prefix(tail: &[u8]) -> bool {
    if tail.is_empty() || tail.len() > SYNC_SHARED_PREFIX.len() {
        return false;
    }
    SYNC_SHARED_PREFIX.starts_with(tail)
}

impl SyncOutputFilter {
    pub(crate) fn new() -> Self {
        Self {
            in_sync: false,
            buffer: Vec::new(),
            sync_started_at: None,
            carry: Vec::new(),
        }
    }

    /// Process a new chunk from the PTY. Returns `Vec<Vec<u8>>` because a
    /// single input chunk can produce multiple output frames (bytes
    /// before a sync, then the sync block, then bytes after).
    ///
    /// The caller is expected to forward each returned vec as a single
    /// `PtyEvent::Output`. Empty vecs are not emitted.
    pub(crate) fn process(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        // Stitch carry + chunk so markers split across chunks still match.
        let combined_len = self.carry.len() + chunk.len();
        if combined_len == 0 {
            return Vec::new();
        }
        let mut bytes = Vec::with_capacity(combined_len);
        bytes.extend_from_slice(&self.carry);
        bytes.extend_from_slice(chunk);
        self.carry.clear();

        // Frames we will emit after processing this chunk.
        let mut frames: Vec<Vec<u8>> = Vec::new();
        // Pending buffer for the current non-sync region.
        let mut pending: Vec<u8> = Vec::new();
        let mut i = 0usize;

        // Max marker length — we need to defer emitting the last
        // (max_seq - 1) bytes as `carry` so a partial marker doesn't
        // leak through as plain output.
        let max_seq = SYNC_BEGIN.len().max(SYNC_END.len());

        while i < bytes.len() {
            let remaining = bytes.len() - i;

            // Enforce safety valves before matching more markers.
            if self.in_sync {
                if self.buffer.len() > MAX_SYNC_BUFFER_BYTES
                    || self
                        .sync_started_at
                        .map(|start| start.elapsed() > MAX_SYNC_DURATION)
                        .unwrap_or(false)
                {
                    tracing::warn!(
                        buffered = self.buffer.len(),
                        elapsed_ms = self
                            .sync_started_at
                            .map(|s| s.elapsed().as_millis())
                            .unwrap_or(0),
                        "Synchronized-output block exceeded safety limits; flushing early"
                    );
                    if !self.buffer.is_empty() {
                        frames.push(std::mem::take(&mut self.buffer));
                    }
                    self.in_sync = false;
                    self.sync_started_at = None;
                }
            }

            // Try to match a sync-begin marker.
            if remaining >= SYNC_BEGIN.len() && bytes[i..i + SYNC_BEGIN.len()] == *SYNC_BEGIN {
                if !self.in_sync {
                    // Flush pending non-sync bytes first, then enter sync.
                    if !pending.is_empty() {
                        frames.push(std::mem::take(&mut pending));
                    }
                    self.in_sync = true;
                    self.sync_started_at = Some(std::time::Instant::now());
                    self.buffer.clear();
                }
                // Already in sync → treat as noise, just strip the marker.
                i += SYNC_BEGIN.len();
                continue;
            }

            // Try to match a sync-end marker.
            if remaining >= SYNC_END.len() && bytes[i..i + SYNC_END.len()] == *SYNC_END {
                if self.in_sync {
                    // Flush the accumulated sync frame.
                    if !self.buffer.is_empty() {
                        frames.push(std::mem::take(&mut self.buffer));
                    }
                    self.in_sync = false;
                    self.sync_started_at = None;
                }
                // Stray 2026l without preceding 2026h → strip and move on.
                i += SYNC_END.len();
                continue;
            }

            // If the tail could be a proper prefix of a sync marker, defer
            // it so the next chunk can complete the match. Checking just
            // for a leading ESC is too loose — it catches every unrelated
            // CSI/SGR sequence (colors, cursor moves, etc.) that happens to
            // end a PTY chunk, and delays emitting them until the next
            // read. On typing-echo paths that stall is perceptible as
            // keystroke lag. A tail is a potential marker prefix only when
            // it matches a prefix of `\x1b[?2026` (the 7-byte common
            // prefix shared by SYNC_BEGIN and SYNC_END, which differ only
            // in the 8th byte).
            if remaining < max_seq && is_potential_sync_marker_prefix(&bytes[i..]) {
                self.carry.extend_from_slice(&bytes[i..]);
                break;
            }

            let byte = bytes[i];
            if self.in_sync {
                self.buffer.push(byte);
            } else {
                pending.push(byte);
            }
            i += 1;
        }

        if !pending.is_empty() {
            frames.push(pending);
        }

        frames
    }

    /// Flush any pending sync-mode buffer. Call at EOF so a sync block
    /// that was left open by a dying child still produces output.
    pub(crate) fn drain(&mut self) -> Option<Vec<u8>> {
        self.in_sync = false;
        self.sync_started_at = None;
        self.carry.clear();
        if self.buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod runtime_safety_tests {
        use super::*;
        include!("core_runtime_safety_tests.rs");
    }
}
