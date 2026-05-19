use crate::mux::*;
use crate::{AgentAdapter, Pane, SupervisorCli};

#[test]
fn test_duplicate_tool_call_started_events_refresh_without_duplicate_entries() {
    use crate::MuxEvent;
    use crate::pane::activity::{ActivityEntry, ActivityKind};

    let mut mux = Mux::new(24, 80);

    let pane = Pane::worker(
        "kimi-worker",
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Kimi),
        None,
        None,
        24,
        80,
        None,
        None,
    )
    .expect("create kimi worker");

    mux.add_pane(pane);

    let started = |tool_name: &str| ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("tool-1".to_string()),
        tool_name: Some(tool_name.to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    };

    for tool_name in ["ReadFile", "ReadFile: crates/brehon-mux/src/mux.rs"] {
        mux.event_tx
            .try_send(MuxEvent::ActivityEvent {
                pane_id: "kimi-worker".to_string(),
                entry: started(tool_name),
                generation: crate::pane::Generation::default(),
            })
            .expect("send activity event");
        let (_bytes, _events) = mux.poll_batch();
    }

    let pane = mux.get("kimi-worker").expect("worker exists");
    let buf = pane.activity_buffer().expect("activity buffer");
    assert_eq!(
        buf.len(),
        1,
        "duplicate starts should not create extra entries"
    );
    assert_eq!(
        buf.active_tool("tool-1")
            .map(|tool| tool.tool_name.as_str()),
        Some("ReadFile: crates/brehon-mux/src/mux.rs"),
        "latest title should refresh the active tool name",
    );
}

#[test]
fn test_gateway_operation_lifecycle_keeps_pane_busy_until_outer_completion() {
    use crate::MuxEvent;
    use crate::pane::activity::{ActivityEntry, ActivityKind};

    let mut mux = Mux::new(24, 80);

    let pane = Pane::worker(
        "opencode-worker",
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
        None,
        None,
        24,
        80,
        None,
        None,
    )
    .expect("create opencode worker");

    mux.add_pane(pane);

    let started = |message: &str| ActivityEntry {
        kind: ActivityKind::Operation,
        ingested_at: std::time::Instant::now(),
        tool_id: None,
        tool_name: None,
        status: Some("started".to_string()),
        message: Some(message.to_string()),
        output_chunks: None,
        duration: None,
    };
    let completed = |message: &str| ActivityEntry {
        kind: ActivityKind::Operation,
        ingested_at: std::time::Instant::now(),
        tool_id: None,
        tool_name: None,
        status: Some("completed".to_string()),
        message: Some(message.to_string()),
        output_chunks: None,
        duration: None,
    };

    for entry in [started("opencode turn"), started("step"), completed("step")] {
        mux.event_tx
            .try_send(MuxEvent::ActivityEvent {
                pane_id: "opencode-worker".to_string(),
                entry,
                generation: crate::pane::Generation::default(),
            })
            .expect("send activity event");
        let (_bytes, _events) = mux.poll_batch();
    }

    let pane = mux.get("opencode-worker").expect("worker exists");
    assert!(
        pane.is_tool_executing(),
        "inner step completion must not clear the outer OpenCode turn"
    );

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "opencode-worker".to_string(),
            entry: completed("opencode turn"),
            generation: crate::pane::Generation::default(),
        })
        .expect("send completion event");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("opencode-worker").expect("worker exists");
    assert!(
        !pane.is_tool_executing(),
        "outer completion should release the busy flag"
    );
}

#[test]
fn test_raw_mode_rendering_unchanged() {
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::director("test", 24, 80).expect("create director pane");
    pane.append_output(b"Hello, world!\n")
        .expect("append output");
    mux.add_pane(pane);

    let p = mux.get("test").expect("pane exists");
    let viewport = p.dump_viewport().expect("dump viewport");

    assert!(viewport.contains("Hello, world!"));
}

#[test]
fn test_retention_does_not_corrupt_active_tools() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(3);

    buf.start_tool("tool-1".to_string(), "long-op".to_string());
    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("tool-1".to_string()),
        tool_name: Some("long-op".to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });

    for i in 0..5 {
        buf.push(crate::pane::activity::ActivityEntry {
            kind: ActivityKind::Progress,
            ingested_at: std::time::Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some(format!("{}%", i * 20)),
            message: Some(format!("progress {}", i)),
            output_chunks: None,
            duration: None,
        });
    }

    assert!(buf.active_tool("tool-1").is_some());
}

#[test]
fn test_structured_path_preserves_ordering() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(10);

    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("t1".to_string()),
        tool_name: Some("tool1".to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });
    buf.flush_output_buffer();
    buf.append_output("output1");
    buf.flush_output_buffer();
    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("t1".to_string()),
        tool_name: Some("tool1".to_string()),
        status: Some("completed".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });

    let entries = buf.entries_with_pending();
    assert_eq!(entries.len(), 3);
    assert!(matches!(entries[0].kind, ActivityKind::ToolCall));
    assert!(matches!(entries[1].kind, ActivityKind::Output));
    assert!(matches!(entries[2].kind, ActivityKind::ToolCall));
}

#[test]
fn test_suppression_operation_started_event() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::OperationStarted {
        session_id: brehon_types::SessionId::new("s-1"),
        operation: "turn".to_string(),
    };

    let entry = session_event_to_activity_entry(&event);
    assert!(entry.is_some());
    assert_eq!(
        entry.unwrap().kind,
        crate::pane::activity::ActivityKind::Operation
    );
}

#[test]
fn test_suppression_operation_completed_event() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::OperationCompleted {
        session_id: brehon_types::SessionId::new("s-1"),
        operation: "turn".to_string(),
        success: true,
    };

    let entry = session_event_to_activity_entry(&event);
    assert!(entry.is_some());
    assert_eq!(
        entry.unwrap().kind,
        crate::pane::activity::ActivityKind::Operation
    );
}

#[test]
fn test_suppression_thread_status_progress() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1"),
        message: "Codex thread status: active".to_string(),
        percent: None,
    };

    assert!(session_event_to_activity_entry(&event).is_none());

    let event2 = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2"),
        message: "Codex thread status: idle".to_string(),
        percent: None,
    };

    assert!(session_event_to_activity_entry(&event2).is_none());
}

#[test]
fn test_suppression_session_idle_progress() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-idle"),
        message: "session idle".to_string(),
        percent: None,
    };

    assert!(session_event_to_activity_entry(&event).is_none());
}

#[test]
fn test_suppression_brehon_bootstrap_tools() {
    use crate::mux::session_event_to_activity_entry;

    let started = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-tool-started"),
        tool_id: "tool-1".to_string(),
        tool_name: "brehon_agent".to_string(),
        details: None,
    };

    let completed = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-tool-completed"),
        tool_id: "tool-2".to_string(),
        tool_name: "brehon_task".to_string(),
        status: "completed".to_string(),
        details: None,
    };

    assert!(session_event_to_activity_entry(&started).is_none());
    assert!(session_event_to_activity_entry(&completed).is_none());
}

#[test]
fn test_suppression_hyphenated_brehon_bootstrap_tools() {
    use crate::mux::session_event_to_activity_entry;

    let started = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-tool-started"),
        tool_id: "tool-1".to_string(),
        tool_name: "brehon-agent".to_string(),
        details: None,
    };

    let completed = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-tool-completed"),
        tool_id: "tool-2".to_string(),
        tool_name: "brehon-task".to_string(),
        status: "completed".to_string(),
        details: None,
    };

    assert!(session_event_to_activity_entry(&started).is_none());
    assert!(session_event_to_activity_entry(&completed).is_none());
}

#[test]
fn test_suppression_low_signal_tool_success() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-tool"),
        tool_id: "tool-submit".to_string(),
        tool_name: "submit_review".to_string(),
        status: "completed".to_string(),
        details: None,
    };

    assert!(session_event_to_activity_entry(&event).is_none());
}

#[test]
fn test_high_signal_permission_request() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::PermissionRequest {
        session_id: brehon_types::SessionId::new("s-1"),
        permission_id: "perm-1".to_string(),
        action: "bash".to_string(),
        details: None,
    };

    let entry =
        session_event_to_activity_entry(&event).expect("permission events should be high signal");
    assert_eq!(entry.kind, crate::pane::activity::ActivityKind::Permission);
    assert_eq!(entry.tool_id, Some("perm-1".to_string()));
    assert_eq!(entry.message, Some("bash".to_string()));
}

#[test]
fn test_high_signal_tool_call_started() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "tool-123".to_string(),
        tool_name: "bash".to_string(),
        details: None,
    };

    let entry =
        session_event_to_activity_entry(&event).expect("bash tool started should be high signal");
    assert_eq!(entry.kind, crate::pane::activity::ActivityKind::ToolCall);
    assert_eq!(entry.tool_id, Some("tool-123".to_string()));
    assert_eq!(entry.tool_name, Some("bash".to_string()));
    assert_eq!(entry.status, Some("started".to_string()));
}

#[test]
fn test_high_signal_tool_call_completed() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "tool-456".to_string(),
        tool_name: "bash".to_string(),
        status: "failed".to_string(),
        details: None,
    };

    let entry =
        session_event_to_activity_entry(&event).expect("bash tool failed should be high signal");
    assert_eq!(entry.kind, crate::pane::activity::ActivityKind::ToolCall);
    assert_eq!(entry.status, Some("failed".to_string()));
}

#[test]
fn test_high_signal_output_event() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-1"),
        text: "Starting work on feature.".to_string(),
    };

    let entry = session_event_to_activity_entry(&event).expect("output should be high signal");
    assert_eq!(entry.kind, crate::pane::activity::ActivityKind::Output);
    assert!(entry.output_chunks.is_some());
    assert_eq!(
        entry.output_chunks.unwrap(),
        vec!["Starting work on feature.".to_string()]
    );
}

#[test]
fn test_output_event_empty_text_suppressed() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-1"),
        text: "".to_string(),
    };

    assert!(session_event_to_activity_entry(&event).is_none());
}

#[test]
fn test_high_signal_progress_with_percent() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1"),
        message: "Downloading dependency...".to_string(),
        percent: Some(75),
    };

    let entry = session_event_to_activity_entry(&event)
        .expect("progress with percent should be high signal");
    assert_eq!(entry.kind, crate::pane::activity::ActivityKind::Progress);
    assert_eq!(entry.status, Some("75%".to_string()));
}

#[test]
fn test_concurrent_tools_tracked_independently() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(10);

    buf.start_tool("tool-a".to_string(), "read".to_string());
    buf.start_tool("tool-b".to_string(), "write".to_string());

    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("tool-a".to_string()),
        tool_name: Some("read".to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });

    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("tool-b".to_string()),
        tool_name: Some("write".to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });

    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("tool-b".to_string()),
        tool_name: Some("write".to_string()),
        status: Some("completed".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });
    buf.complete_tool("tool-b");

    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("tool-a".to_string()),
        tool_name: Some("read".to_string()),
        status: Some("completed".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });
    buf.complete_tool("tool-a");

    assert!(buf.active_tool("tool-a").is_none());
    assert!(buf.active_tool("tool-b").is_none());
    assert_eq!(buf.len(), 4);

    let entries: Vec<_> = buf.entries().collect();
    assert_eq!(entries[0].tool_id, Some("tool-a".to_string()));
    assert_eq!(entries[1].tool_id, Some("tool-b".to_string()));
    assert_eq!(entries[2].tool_id, Some("tool-b".to_string()));
    assert_eq!(entries[3].tool_id, Some("tool-a".to_string()));
}

#[test]
fn test_buffer_retention_max_capacity_plus_entries() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(5);

    for i in 0..10 {
        buf.push(crate::pane::activity::ActivityEntry {
            kind: ActivityKind::Progress,
            ingested_at: std::time::Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some(format!("{}%", i * 10)),
            message: Some(format!("step {}", i)),
            output_chunks: None,
            duration: None,
        });
    }

    assert_eq!(buf.len(), 5);

    let messages: Vec<_> = buf
        .entries()
        .map(|e| e.message.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        messages,
        vec!["step 5", "step 6", "step 7", "step 8", "step 9"]
    );
}

#[test]
fn test_output_coalescing_across_multiple_appends() {
    let mut buf = crate::pane::activity::ActivityBuffer::new(10);

    buf.append_output("first ");
    buf.append_output("second ");
    buf.append_output("third");

    let entries = buf.entries_with_pending();
    assert_eq!(entries.len(), 1);
    let entry = entries.first().unwrap();
    assert_eq!(entry.kind, crate::pane::activity::ActivityKind::Output);
    let chunks = entry.output_chunks.as_ref().unwrap();
    assert_eq!(chunks, &vec!["first second third".to_string()]);
}

#[test]
fn test_output_coalescing_broken_by_tool_call() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(10);

    buf.append_output("output before tool\n");
    buf.flush_output_buffer();

    buf.start_tool("tool-1".to_string(), "bash".to_string());
    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("tool-1".to_string()),
        tool_name: Some("bash".to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });

    buf.append_output("output after tool\n");
    buf.flush_output_buffer();

    assert_eq!(buf.len(), 3);
}

#[test]
fn test_tool_call_started_without_completed_produces_stale_entry() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(10);

    buf.start_tool("orphan-id".to_string(), "long-running".to_string());
    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("orphan-id".to_string()),
        tool_name: Some("long-running".to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });

    assert!(buf.active_tool("orphan-id").is_some());
    assert_eq!(buf.len(), 1);

    let stale = buf.sweep_stale(std::time::Duration::from_secs(0));
    assert_eq!(stale, vec!["orphan-id".to_string()]);
    assert!(buf.active_tool("orphan-id").is_none());

    assert_eq!(buf.len(), 1);
}

#[test]
fn test_render_structured_pane_empty_buffer() {
    let buf = crate::pane::activity::ActivityBuffer::new(10);
    let entries = buf.entries_with_pending();
    assert!(entries.is_empty());
}

#[test]
fn test_render_structured_pane_with_tool_call() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(10);

    buf.start_tool("test-tool-id".to_string(), "cargo test".to_string());
    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("test-tool-id".to_string()),
        tool_name: Some("cargo test".to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    });

    let entries = buf.entries_with_pending();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, ActivityKind::ToolCall);
    assert_eq!(entries[0].tool_name, Some("cargo test".to_string()));
    assert_eq!(entries[0].status, Some("started".to_string()));
}

#[test]
fn test_render_structured_pane_with_output() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(10);

    buf.append_output("Build starting...\n");
    buf.append_output("Compiling brehon-mux\n");
    buf.flush_output_buffer();

    let entries = buf.entries_with_pending();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, ActivityKind::Output);

    let chunks = entries[0].output_chunks.as_ref().unwrap();
    assert!(chunks[0].contains("Build starting"));
    assert!(chunks[0].contains("Compiling"));
}

#[test]
fn test_render_structured_pane_with_progress() {
    use crate::pane::activity::ActivityKind;

    let mut buf = crate::pane::activity::ActivityBuffer::new(10);

    buf.push(crate::pane::activity::ActivityEntry {
        kind: ActivityKind::Progress,
        ingested_at: std::time::Instant::now(),
        tool_id: None,
        tool_name: None,
        status: Some("50%".to_string()),
        message: Some("Downloading...".to_string()),
        output_chunks: None,
        duration: None,
    });

    let entries = buf.entries_with_pending();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, ActivityKind::Progress);
    assert_eq!(entries[0].message, Some("Downloading...".to_string()));
}

#[test]
fn test_raw_pane_rendering_unchanged_with_activity_buffer() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    pane.append_output(b"Hello, World!\n").expect("append");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("Hello, World!"));
    assert!(pane.activity_buffer().is_none());
}

#[test]
fn test_prompt_delivery_notice_marks_idle_nudge() {
    let notice = prompt_delivery_notice(
        "You have been idle for 5 minutes. Check your assigned tasks.",
        Some("claude-code"),
    );
    assert!(notice.contains("idle nudge delivered"));
    assert!(notice.contains("claude-code"));
}

#[test]
fn test_prompt_delivery_notice_marks_task_assignment() {
    let notice = prompt_delivery_notice("You have been assigned task T-1: Example", None);
    assert!(notice.contains("task assignment delivered"));
    assert!(notice.contains("[brehon]"));
}
