//! Raw PTY byte-stream capture for rendering diagnostics.
//!
//! When `BREHON_PTY_DUMP_DIR` is set to a non-empty directory path, every PTY
//! spawned through [`Pty::spawn`](super::core::Pty::spawn) writes two files
//! into that directory:
//!
//! - `<pane>.raw` — every byte read from the slave PTY, unmodified. Cat it
//!   into a reference terminal (`cat foo.raw > /dev/tty`) to replay exactly
//!   what the child CLI emitted.
//! - `<pane>.log` — JSON-lines metadata: one `spawn` record with the command,
//!   args, cwd, and initial winsize; one `read` record per chunk with a
//!   timestamp, byte count, and running offset into the `.raw` file.
//!
//! When the env var is unset or empty, [`DumpWriter::from_env`] returns
//! `None` and both files of bookkeeping (and the ~12 bytes of per-read
//! overhead) are elided completely.
//!
//! # Why this exists
//!
//! Claude Code's Ink TUI redraws in-place via cursor-up + erase-line rather
//! than alt-screen mode, and its interaction with Brehon's embedded ghostty_vt
//! emulator has produced garbled startup frames. Without a byte-level
//! reproduction it's impossible to tell whether the fault is in the child's
//! output sequence, the emulator's CSI handling, or the pane rendering.
//! This capture makes post-mortem analysis of a live garbling repro trivial.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Per-pane raw-byte capture writer.
///
/// Constructed at PTY spawn, then moved into the reader loop. Both handles
/// are buffered; the `.raw` file is flushed after each write so a crash
/// doesn't lose the tail. The `.log` file is line-flushed on each append.
pub(crate) struct DumpWriter {
    raw: Mutex<BufWriter<File>>,
    log: Mutex<BufWriter<File>>,
    byte_offset: Mutex<u64>,
    disabled: Mutex<bool>,
}

impl DumpWriter {
    /// If `BREHON_PTY_DUMP_DIR` is set to a non-empty path, return a writer
    /// that dumps this pane's raw PTY stream into that directory.
    ///
    /// Returns `None` in the common case (env unset) so callers pay zero
    /// overhead. All filesystem errors are warnings — dump setup failing
    /// must never prevent the PTY from spawning.
    pub(crate) fn from_env(
        pane_id: &str,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Option<Self> {
        let dir = std::env::var("BREHON_PTY_DUMP_DIR").ok()?;
        let dir = dir.trim();
        if dir.is_empty() {
            return None;
        }
        let dir = PathBuf::from(dir);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            tracing::warn!(
                pane = %pane_id,
                dir = %dir.display(),
                error = %err,
                "BREHON_PTY_DUMP_DIR: failed to create dump dir; capture disabled for this pane"
            );
            return None;
        }

        let safe = sanitize_for_filename(pane_id);
        let raw_path = dir.join(format!("{safe}.raw"));
        let log_path = dir.join(format!("{safe}.log"));

        let raw = match File::options().create(true).append(true).open(&raw_path) {
            Ok(file) => BufWriter::new(file),
            Err(err) => {
                tracing::warn!(
                    pane = %pane_id,
                    path = %raw_path.display(),
                    error = %err,
                    "BREHON_PTY_DUMP_DIR: failed to open raw dump file; capture disabled for this pane"
                );
                return None;
            }
        };
        let log = match File::options().create(true).append(true).open(&log_path) {
            Ok(file) => BufWriter::new(file),
            Err(err) => {
                tracing::warn!(
                    pane = %pane_id,
                    path = %log_path.display(),
                    error = %err,
                    "BREHON_PTY_DUMP_DIR: failed to open log file; capture disabled for this pane"
                );
                return None;
            }
        };

        let writer = Self {
            raw: Mutex::new(raw),
            log: Mutex::new(log),
            byte_offset: Mutex::new(0),
            disabled: Mutex::new(false),
        };

        // Record a spawn line so analysts can correlate the .raw file with
        // the command that produced it. Include the claim-relevant winsize
        // since that's usually the first question when diagnosing a TUI bug.
        let spawn_line = format!(
            "{{\"type\":\"spawn\",\"ts\":{},\"pane\":{},\"command\":{},\"args\":{},\"cwd\":{},\"rows\":{},\"cols\":{}}}\n",
            json_number_ms(),
            json_string(pane_id),
            json_string(command),
            json_string_array(args),
            cwd.map(|p| json_string(&p.to_string_lossy()))
                .unwrap_or_else(|| "null".to_string()),
            rows,
            cols,
        );
        if let Err(err) = writer.append_log_line(&spawn_line) {
            tracing::warn!(
                pane = %pane_id,
                error = %err,
                "BREHON_PTY_DUMP_DIR: failed to write spawn record; continuing with capture"
            );
        }

        tracing::info!(
            pane = %pane_id,
            raw = %raw_path.display(),
            log = %log_path.display(),
            "BREHON_PTY_DUMP_DIR: capturing raw PTY output for this pane"
        );

        Some(writer)
    }

    /// Append a chunk of raw bytes read from the slave PTY.
    ///
    /// Errors silently disable further writes to this capture — the PTY
    /// reader loop must never propagate capture errors to the child.
    pub(crate) fn record_read(&self, bytes: &[u8]) {
        if *self.disabled.lock().unwrap_or_else(|e| e.into_inner()) {
            return;
        }
        if bytes.is_empty() {
            return;
        }

        if let Err(err) = self.write_and_flush_raw(bytes) {
            self.disable(format!("raw write failed: {err}"));
            return;
        }

        let mut offset = self.byte_offset.lock().unwrap_or_else(|e| e.into_inner());
        let start_offset = *offset;
        *offset += bytes.len() as u64;
        drop(offset);

        let line = format!(
            "{{\"type\":\"read\",\"ts\":{},\"bytes\":{},\"offset\":{}}}\n",
            json_number_ms(),
            bytes.len(),
            start_offset,
        );
        if let Err(err) = self.append_log_line(&line) {
            self.disable(format!("log write failed: {err}"));
        }
    }

    fn write_and_flush_raw(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut guard = self.raw.lock().unwrap_or_else(|e| e.into_inner());
        guard.write_all(bytes)?;
        // Flush after every chunk so a crash or SIGKILL still yields a
        // useful tail for diagnosis. The capture is already off the hot
        // path (only when the env var is set), so the extra syscall cost
        // is not a concern.
        guard.flush()?;
        Ok(())
    }

    fn append_log_line(&self, line: &str) -> std::io::Result<()> {
        let mut guard = self.log.lock().unwrap_or_else(|e| e.into_inner());
        guard.write_all(line.as_bytes())?;
        guard.flush()?;
        Ok(())
    }

    fn disable(&self, reason: String) {
        let mut disabled = self.disabled.lock().unwrap_or_else(|e| e.into_inner());
        if !*disabled {
            *disabled = true;
            tracing::warn!(reason = %reason, "BREHON_PTY_DUMP_DIR: capture disabled after error");
        }
    }
}

fn sanitize_for_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "pane".to_string()
    } else {
        out
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_string_array(values: &[String]) -> String {
    let mut out = String::from("[");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(v));
    }
    out.push(']');
    out
}

fn json_number_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    // Tests mutate BREHON_PTY_DUMP_DIR so they must run serially. Cargo runs
    // tests in the same process by default, so we guard env access with a
    // test-local lock.
    use std::sync::Mutex as StdMutex;
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    struct ScopedEnv {
        saved: Option<std::ffi::OsString>,
    }
    impl ScopedEnv {
        fn set(value: Option<&str>) -> Self {
            let saved = std::env::var_os("BREHON_PTY_DUMP_DIR");
            // Rust 2024: std::env mutation is unsafe. Test module serializes
            // via ENV_LOCK, so the safety invariant (no concurrent env access)
            // holds for all tests in this file.
            unsafe {
                match value {
                    Some(v) => std::env::set_var("BREHON_PTY_DUMP_DIR", v),
                    None => std::env::remove_var("BREHON_PTY_DUMP_DIR"),
                }
            }
            Self { saved }
        }
    }
    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.saved {
                    Some(v) => std::env::set_var("BREHON_PTY_DUMP_DIR", v),
                    None => std::env::remove_var("BREHON_PTY_DUMP_DIR"),
                }
            }
        }
    }

    #[test]
    fn returns_none_when_env_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(None);
        let w = DumpWriter::from_env("pane", "cmd", &[], None, 24, 80);
        assert!(w.is_none());
    }

    #[test]
    fn returns_none_when_env_blank() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = ScopedEnv::set(Some("   "));
        let w = DumpWriter::from_env("pane", "cmd", &[], None, 24, 80);
        assert!(w.is_none());
    }

    #[test]
    fn captures_reads_and_records_spawn_metadata() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _env = ScopedEnv::set(Some(temp.path().to_str().unwrap()));

        let writer = DumpWriter::from_env(
            "claude-supervisor",
            "claude",
            &["--arg1".to_string(), "--arg2".to_string()],
            Some(Path::new("/repo/worktree")),
            40,
            120,
        )
        .expect("writer");

        writer.record_read(b"hello");
        writer.record_read(b" world\n");
        drop(writer);

        let raw_path = temp.path().join("claude-supervisor.raw");
        let mut raw_contents = Vec::new();
        File::open(&raw_path)
            .expect("raw file")
            .read_to_end(&mut raw_contents)
            .expect("read");
        assert_eq!(raw_contents, b"hello world\n");

        let log_path = temp.path().join("claude-supervisor.log");
        let log = std::fs::read_to_string(&log_path).expect("log file");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 3, "expected spawn + 2 read records, got {log}");
        assert!(lines[0].contains("\"type\":\"spawn\""), "{}", lines[0]);
        assert!(lines[0].contains("\"command\":\"claude\""));
        assert!(lines[0].contains("\"cwd\":\"/repo/worktree\""));
        assert!(lines[0].contains("\"rows\":40"));
        assert!(lines[0].contains("\"cols\":120"));
        assert!(lines[1].contains("\"type\":\"read\""));
        assert!(lines[1].contains("\"bytes\":5"));
        assert!(lines[1].contains("\"offset\":0"));
        assert!(lines[2].contains("\"bytes\":7"));
        assert!(lines[2].contains("\"offset\":5"));
    }

    #[test]
    fn sanitizes_pane_ids_with_unsafe_chars() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _env = ScopedEnv::set(Some(temp.path().to_str().unwrap()));

        let writer =
            DumpWriter::from_env("pane/with:bad*chars", "cmd", &[], None, 24, 80).expect("writer");
        writer.record_read(b"x");
        drop(writer);

        // Expect `pane_with_bad_chars.raw` — slashes/colons/asterisks replaced.
        assert!(temp.path().join("pane_with_bad_chars.raw").exists());
    }

    #[test]
    fn empty_reads_are_ignored() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _env = ScopedEnv::set(Some(temp.path().to_str().unwrap()));

        let writer = DumpWriter::from_env("p", "cmd", &[], None, 24, 80).expect("writer");
        writer.record_read(b"");
        drop(writer);

        let log = std::fs::read_to_string(temp.path().join("p.log")).expect("log");
        // Only the spawn record — no read line for an empty buffer.
        assert_eq!(log.lines().count(), 1);
    }

    #[test]
    fn json_string_escapes_special_characters() {
        assert_eq!(json_string("a\"b"), r#""a\"b""#);
        assert_eq!(json_string("a\\b"), r#""a\\b""#);
        assert_eq!(json_string("line1\nline2"), r#""line1\nline2""#);
        // Control chars below 0x20 get \u escapes.
        assert_eq!(json_string("\x01"), "\"\\u0001\"");
    }
}
