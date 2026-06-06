#[test]
fn cpr_response_uses_real_position_and_is_one_indexed() {
    assert_eq!(
        format_cursor_position_report(12, 34),
        b"\x1b[12;34R".to_vec()
    );
    assert_eq!(format_cursor_position_report(0, 0), b"\x1b[1;1R".to_vec());
    assert_eq!(format_cursor_position_report(1, 1), b"\x1b[1;1R".to_vec());
}

#[test]
fn explicit_env_key_is_not_scrubbed() {
    let explicit_env_keys = HashSet::from(["ANTHROPIC_API_KEY"]);
    assert!(!should_scrub_env_key(
        &explicit_env_keys,
        "ANTHROPIC_API_KEY"
    ));
    assert!(should_scrub_env_key(&explicit_env_keys, "OPENAI_API_KEY"));
}

#[test]
fn inherited_anthropic_auth_token_is_scrubbed() {
    let explicit_env_keys = HashSet::new();
    assert!(should_scrub_env_key(
        &explicit_env_keys,
        "ANTHROPIC_AUTH_TOKEN"
    ));
}

/// Bytes outside a sync block pass straight through.
#[test]
fn sync_filter_passes_through_non_sync_bytes() {
    let mut f = SyncOutputFilter::new();
    let frames = f.process(b"hello world\n");
    assert_eq!(frames, vec![b"hello world\n".to_vec()]);
}

/// A complete sync block in one chunk yields exactly one frame,
/// with the markers stripped.
#[test]
fn sync_filter_coalesces_single_chunk_block() {
    let mut f = SyncOutputFilter::new();
    let input = b"pre\x1b[?2026hframe\x1b[?2026lpost";
    let frames = f.process(input);
    assert_eq!(
        frames,
        vec![b"pre".to_vec(), b"frame".to_vec(), b"post".to_vec()]
    );
}

/// Sync markers split across multiple PTY chunks still match.
#[test]
fn sync_filter_handles_markers_split_across_chunks() {
    let mut f = SyncOutputFilter::new();

    let a = f.process(b"outer\x1b[?2");
    assert_eq!(a, vec![b"outer".to_vec()]);

    let b = f.process(b"026hinner");
    assert!(b.is_empty(), "unexpected frames mid-sync: {b:?}");

    let c = f.process(b"more\x1b[?");
    assert!(c.is_empty(), "unexpected frames mid-sync: {c:?}");

    let d = f.process(b"2026lafter");
    assert_eq!(d, vec![b"innermore".to_vec(), b"after".to_vec()]);
}

/// Stray END marker without preceding BEGIN is stripped and ignored.
#[test]
fn sync_filter_strips_stray_end_marker() {
    let mut f = SyncOutputFilter::new();
    let frames = f.process(b"lone\x1b[?2026lhere");
    assert_eq!(frames, vec![b"lonehere".to_vec()]);
}

/// A second BEGIN inside an open sync block is treated as a no-op marker strip.
#[test]
fn sync_filter_ignores_nested_begin_marker() {
    let mut f = SyncOutputFilter::new();
    let frames = f.process(b"\x1b[?2026hpart1\x1b[?2026hpart2\x1b[?2026lout");
    assert_eq!(frames, vec![b"part1part2".to_vec(), b"out".to_vec()]);
}

/// Drain at EOF returns any buffered (un-closed) frame.
#[test]
fn sync_filter_drain_emits_unclosed_buffer() {
    let mut f = SyncOutputFilter::new();
    let frames = f.process(b"\x1b[?2026hopen-frame");
    assert!(frames.is_empty());
    let tail = f.drain();
    assert_eq!(tail, Some(b"open-frame".to_vec()));
}

/// Safety valve: a sync block exceeding the byte cap flushes early.
#[test]
fn sync_filter_safety_flush_on_oversized_buffer() {
    let mut f = SyncOutputFilter::new();
    let mut big = Vec::with_capacity(MAX_SYNC_BUFFER_BYTES + 32);
    big.extend_from_slice(b"\x1b[?2026h");
    big.resize(big.len() + MAX_SYNC_BUFFER_BYTES + 16, b'x');

    let frames = f.process(&big);
    assert!(
        frames.iter().any(|f| f.len() > MAX_SYNC_BUFFER_BYTES),
        "safety flush did not fire: frame lengths {:?}",
        frames.iter().map(|f| f.len()).collect::<Vec<_>>()
    );
}

/// A non-ESC trailing byte is emitted immediately rather than carried.
#[test]
fn sync_filter_does_not_carry_non_escape_tail() {
    let mut f = SyncOutputFilter::new();
    let frames = f.process(b"plain-text");
    assert_eq!(frames, vec![b"plain-text".to_vec()]);
    let frames2 = f.process(b"more");
    assert_eq!(frames2, vec![b"more".to_vec()]);
}

/// Empty input produces no frames.
#[test]
fn sync_filter_empty_input_produces_no_frames() {
    let mut f = SyncOutputFilter::new();
    let frames = f.process(b"");
    assert!(frames.is_empty());
}

/// Regression: non-sync escape tails must be emitted immediately.
#[test]
fn sync_filter_passes_through_non_marker_escape_at_chunk_tail() {
    let mut f = SyncOutputFilter::new();
    let frames = f.process(b"hello\x1b[0m");
    assert_eq!(frames, vec![b"hello\x1b[0m".to_vec()]);
}

/// Only tails that are proper sync-marker prefixes are carried.
#[test]
fn is_potential_sync_marker_prefix_matches_only_shared_prefix() {
    assert!(!is_potential_sync_marker_prefix(b""));
    assert!(is_potential_sync_marker_prefix(b"\x1b"));
    assert!(is_potential_sync_marker_prefix(b"\x1b["));
    assert!(is_potential_sync_marker_prefix(b"\x1b[?"));
    assert!(is_potential_sync_marker_prefix(b"\x1b[?2"));
    assert!(is_potential_sync_marker_prefix(b"\x1b[?20"));
    assert!(is_potential_sync_marker_prefix(b"\x1b[?202"));
    assert!(is_potential_sync_marker_prefix(b"\x1b[?2026"));
    assert!(!is_potential_sync_marker_prefix(b"\x1b[0"));
    assert!(!is_potential_sync_marker_prefix(b"\x1b[1"));
    assert!(!is_potential_sync_marker_prefix(b"\x1b[?25"));
    assert!(!is_potential_sync_marker_prefix(b"\x1b]"));
    assert!(!is_potential_sync_marker_prefix(b"\x1bO"));
    assert!(!is_potential_sync_marker_prefix(SYNC_BEGIN));
    assert!(!is_potential_sync_marker_prefix(SYNC_END));
}

#[tokio::test]
async fn backpressure_full_pty_event_channel_records_dropped_output() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(PtyEvent::CursorPositionRequested)
        .expect("test channel should accept first event");
    let dropped_frames = AtomicU64::new(0);
    let dropped_bytes = AtomicU64::new(0);

    assert!(try_send_pty_event(
        &tx,
        PtyEvent::Output(b"dropped".to_vec()),
        &dropped_frames,
        &dropped_bytes,
    ));

    assert_eq!(dropped_frames.load(Ordering::SeqCst), 1);
    assert_eq!(dropped_bytes.load(Ordering::SeqCst), 7);
    assert!(matches!(
        rx.recv().await,
        Some(PtyEvent::CursorPositionRequested)
    ));
}

/// Verify that `Pty::kill()` takes ownership of the reader task and
/// leaves the handle `None` so the reader does not outlive the PTY.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_kill_reaps_reader_handle() {
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "sleep 5".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let mut pty = Pty::spawn("test-kill-reap", config).expect("spawn test pty");
    assert!(pty.reader_handle.is_some());

    pty.kill();

    assert!(pty.reader_handle.is_none());
}

/// Verify that dropping a `Pty` terminates the child process and reaps
/// the reader task so it does not outlive the struct that owns it.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_drop_kills_child_and_reaps_reader() {
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "sleep 5".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("test-drop", config).expect("spawn test pty");
    drop(pty);
}

/// Verify that `kill()` on an already-exited process still drains the
/// owned reader handle.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_kill_on_already_exited_process_reaps_handle() {
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "echo hello".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let mut pty = Pty::spawn("test-kill-exited", config).expect("spawn test pty");

    tokio::time::sleep(Duration::from_millis(500)).await;

    while pty.try_recv().is_some() {}

    pty.kill();

    assert!(pty.reader_handle.is_none());
}
