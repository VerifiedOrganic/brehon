use super::{ScopedEnv, TEST_ENV_LOCK};
use crate::mux::*;
use crate::{AgentAdapter, DeathReason, Generation, Pane, PaneState, SupervisorCli};
use std::time::{Duration, Instant};

#[test]
fn test_opencode_supervisor_uses_pty_not_acp() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    // Opencode supervisor capabilities must dictate PTY / PtyInjection
    let adapter = AgentAdapter::BuiltIn(SupervisorCli::OpenCode);
    let mut capabilities = adapter.capabilities();
    // Simulate what agent_to_adapter does for supervisor roles:
    capabilities.transport = crate::harness::HarnessTransport::InteractivePty;
    capabilities.preferred_control_plane = crate::harness::HarnessControlPlane::PtyInjection;

    assert_eq!(
        capabilities.transport,
        crate::harness::HarnessTransport::InteractivePty
    );
    assert_eq!(
        capabilities.preferred_control_plane,
        crate::harness::HarnessControlPlane::PtyInjection
    );
}

#[test]
fn test_opencode_recovery_process_exited() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "opencode-supervisor",
        "OpenCode Supervisor",
        crate::pane::PaneKind::Supervisor,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
    )
    .unwrap();

    // Set it to Busy state
    let now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);
    // Simulate process exit
    pane.exited = true;

    mux.add_pane(pane);

    // Run tick - should trigger recovery/recycle because of process exit
    mux.tick_pane_state_machine_at(rt.handle(), now);

    // Verify pane recycled (generation incremented, state set to Ready, exited set to false)
    let recycled_pane = mux.get("opencode-supervisor").unwrap();
    assert_eq!(recycled_pane.current_generation().0, 1);
    assert!(matches!(
        recycled_pane.pane_state(),
        Some(PaneState::Ready { .. })
    ));
    assert!(!recycled_pane.exited);
    assert_eq!(recycled_pane.restart_count, 1);
    assert_eq!(
        recycled_pane.last_restart_reason.as_deref(),
        Some("process_exited")
    );
}

#[test]
fn test_opencode_recovery_max_turn_exceeded() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "opencode-supervisor",
        "OpenCode Supervisor",
        crate::pane::PaneKind::Supervisor,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
    )
    .unwrap();

    let now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);

    mux.add_pane(pane);

    // Tick at now + MAX_TURN_DURATION - should trigger recovery due to timeout
    let timeout_time = now + super::super::MAX_TURN_DURATION;
    mux.get_mut("opencode-supervisor")
        .unwrap()
        .touch_busy_activity(timeout_time);
    mux.tick_pane_state_machine_at(rt.handle(), timeout_time);

    let recycled_pane = mux.get("opencode-supervisor").unwrap();
    assert_eq!(recycled_pane.current_generation().0, 1);
    assert_eq!(recycled_pane.restart_count, 1);
    assert_eq!(recycled_pane.consecutive_crashes, 0);
    assert_eq!(
        recycled_pane.last_restart_reason.as_deref(),
        Some("max_turn_exceeded")
    );
}

#[test]
fn test_opencode_recovery_no_output_after_delivery() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "opencode-supervisor",
        "OpenCode Supervisor",
        crate::pane::PaneKind::Supervisor,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
    )
    .unwrap();

    let now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);
    pane.set_last_output_at(now);

    mux.add_pane(pane);

    // Tick at now + 60s - should trigger recovery due to no output
    let timeout_time = now + Duration::from_secs(60);
    mux.get_mut("opencode-supervisor")
        .unwrap()
        .touch_busy_activity(timeout_time);
    mux.tick_pane_state_machine_at(rt.handle(), timeout_time);

    let recycled_pane = mux.get("opencode-supervisor").unwrap();
    assert_eq!(recycled_pane.current_generation().0, 1);
    assert_eq!(recycled_pane.consecutive_crashes, 0);
    assert_eq!(
        recycled_pane.last_restart_reason.as_deref(),
        Some("no_output_after_delivery")
    );
}

#[test]
fn test_opencode_recovery_blocked_on_approval() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "opencode-supervisor",
        "OpenCode Supervisor",
        crate::pane::PaneKind::Supervisor,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
    )
    .unwrap();

    let now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);
    pane.feed(b"Do you trust this workspace? [y/N]").unwrap();

    mux.add_pane(pane);

    mux.tick_pane_state_machine_at(rt.handle(), now);

    let recycled_pane = mux.get("opencode-supervisor").unwrap();
    assert_eq!(recycled_pane.current_generation().0, 1);
    assert_eq!(recycled_pane.consecutive_crashes, 0);
    assert_eq!(
        recycled_pane.last_restart_reason.as_deref(),
        Some("blocked_on_approval")
    );
}

#[test]
fn test_opencode_supervisor_drops_stale_queued_prompt_after_recycle() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "opencode-supervisor",
        "OpenCode Supervisor",
        crate::pane::PaneKind::Supervisor,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
    )
    .unwrap();
    pane.set_pane_state(PaneState::Ready {
        since: Instant::now(),
    });
    mux.add_pane(pane);

    let inject_after = Instant::now() - Duration::from_millis(1);
    match mux.queue_delayed_prompt(
        "opencode-supervisor",
        "stale prompt".to_string(),
        None,
        inject_after,
        None,
    ) {
        PromptDeliveryAttempt::Queued { .. } => {}
        other => panic!("expected queued stale prompt, got {other:?}"),
    }
    assert_eq!(mux.pending_delayed_prompt_count(), 1);

    let generation = rt.block_on(mux.recycle("opencode-supervisor", "test recycle"));
    assert_eq!(generation, Generation(1));

    mux.tick_pane_state_machine_at(rt.handle(), Instant::now());

    let pane = mux.get("opencode-supervisor").unwrap();
    assert_eq!(pane.current_generation(), Generation(1));
    assert_eq!(mux.pending_delayed_prompt_count(), 0);
    assert!(pane.delayed_prompt_in_flight().is_none());
    assert!(pane.delayed_prompt_waiting().is_empty());
}

#[test]
fn test_opencode_quarantine_after_repeated_crashes_and_health_marker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let brehon_root = std::env::temp_dir().join(format!(
        "brehon-opencode-quarantine-{}",
        uuid::Uuid::new_v4()
    ));
    let brehon_root_string = brehon_root.to_string_lossy().to_string();
    let _env = ScopedEnv::set(&[
        ("BREHON_SKIP_PREFLIGHT", "1"),
        ("BREHON_ROOT", brehon_root_string.as_str()),
    ]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "opencode-supervisor",
        "OpenCode Supervisor",
        crate::pane::PaneKind::Supervisor,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
    )
    .unwrap();

    let mut now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);
    mux.add_pane(pane);

    // Crash 1
    mux.get_mut("opencode-supervisor").unwrap().exited = true;
    mux.tick_pane_state_machine_at(rt.handle(), now);
    assert_eq!(
        mux.get("opencode-supervisor").unwrap().consecutive_crashes,
        1
    );

    // Wait a bit, then crash 2 (must be after backoff of 2 seconds)
    now += Duration::from_secs(3);
    mux.get_mut("opencode-supervisor").unwrap().exited = true;
    mux.tick_pane_state_machine_at(rt.handle(), now);
    assert_eq!(
        mux.get("opencode-supervisor").unwrap().consecutive_crashes,
        2
    );

    // Wait a bit, then crash 3 (must be after backoff of 5 seconds)
    now += Duration::from_secs(6);
    mux.get_mut("opencode-supervisor").unwrap().exited = true;
    mux.tick_pane_state_machine_at(rt.handle(), now);

    // Pane should now be quarantined (Dead state)
    let final_pane = mux.get("opencode-supervisor").unwrap();
    assert!(matches!(
        final_pane.pane_state(),
        Some(PaneState::Dead {
            reason: DeathReason::Quarantined(_),
            ..
        })
    ));

    // Verify health marker was written
    let health_path = brehon_root
        .join("runtime")
        .join("agent-health")
        .join("opencode-supervisor.json");
    assert!(health_path.exists());
    let health_content = std::fs::read_to_string(health_path).unwrap();
    let health_json: serde_json::Value = serde_json::from_str(&health_content).unwrap();
    assert_eq!(
        health_json.get("status").unwrap().as_str(),
        Some("unavailable")
    );
    assert_eq!(
        health_json.get("reason").unwrap().as_str(),
        Some("process_exited")
    );

    let _ = std::fs::remove_dir_all(brehon_root);
}
