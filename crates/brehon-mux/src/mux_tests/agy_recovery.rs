use super::{ScopedEnv, TEST_ENV_LOCK};
use crate::mux::*;
use crate::{AgentAdapter, DeathReason, Generation, Pane, PaneState, SupervisorCli};
use std::time::{Duration, Instant};

#[test]
fn test_agy_recovery_process_exited() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "agy-worker",
        "Agy Worker",
        crate::pane::PaneKind::Worker,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Agy),
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
    let recycled_pane = mux.get("agy-worker").unwrap();
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
fn test_agy_recovery_max_turn_exceeded() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "agy-worker",
        "Agy Worker",
        crate::pane::PaneKind::Worker,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Agy),
    )
    .unwrap();

    let now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);

    mux.add_pane(pane);

    // Tick at now + MAX_TURN_DURATION - should trigger recovery due to timeout
    let timeout_time = now + super::super::MAX_TURN_DURATION;
    mux.get_mut("agy-worker")
        .unwrap()
        .touch_busy_activity(timeout_time);
    mux.tick_pane_state_machine_at(rt.handle(), timeout_time);

    let recycled_pane = mux.get("agy-worker").unwrap();
    assert_eq!(recycled_pane.current_generation().0, 1);
    assert_eq!(recycled_pane.restart_count, 1);
    assert_eq!(recycled_pane.consecutive_crashes, 0);
    assert_eq!(
        recycled_pane.last_restart_reason.as_deref(),
        Some("max_turn_exceeded")
    );
}

#[test]
fn test_agy_recovery_no_output_after_delivery() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "agy-worker",
        "Agy Worker",
        crate::pane::PaneKind::Worker,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Agy),
    )
    .unwrap();

    let now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);
    pane.set_last_output_at(now);

    mux.add_pane(pane);

    // Tick at now + 60s - should trigger recovery due to no output
    let timeout_time = now + Duration::from_secs(60);
    mux.get_mut("agy-worker")
        .unwrap()
        .touch_busy_activity(timeout_time);
    mux.tick_pane_state_machine_at(rt.handle(), timeout_time);

    let recycled_pane = mux.get("agy-worker").unwrap();
    assert_eq!(recycled_pane.current_generation().0, 1);
    assert_eq!(recycled_pane.consecutive_crashes, 0);
    assert_eq!(
        recycled_pane.last_restart_reason.as_deref(),
        Some("no_output_after_delivery")
    );
}

#[test]
fn test_agy_recovery_helper_call_hung() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let brehon_root =
        std::env::temp_dir().join(format!("brehon-agy-helper-hung-{}", uuid::Uuid::new_v4()));
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
        "agy-worker",
        "Agy Worker",
        crate::pane::PaneKind::Worker,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Agy),
    )
    .unwrap();

    let now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);
    let marker_path = brehon_root
        .join("runtime")
        .join("mcp-helper-inflight")
        .join("agy-worker");
    std::fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
    std::fs::write(
        &marker_path,
        serde_json::json!({
            "agent": "agy-worker",
            "tool": "health",
            "started_at": (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339(),
        })
        .to_string(),
    )
    .unwrap();

    mux.add_pane(pane);

    // Tick at now + 45s - should trigger recovery due to helper call hung (no successful call after attempt)
    let timeout_time = now + Duration::from_secs(45);
    mux.tick_pane_state_machine_at(rt.handle(), timeout_time);

    let recycled_pane = mux.get("agy-worker").unwrap();
    assert_eq!(recycled_pane.current_generation().0, 1);
    assert_eq!(recycled_pane.consecutive_crashes, 0);
    assert_eq!(
        recycled_pane.last_restart_reason.as_deref(),
        Some("helper_call_hung")
    );
    assert!(
        !marker_path.exists(),
        "helper recovery should clear stale in-flight marker"
    );

    let _ = std::fs::remove_dir_all(brehon_root);
}

#[test]
fn test_agy_recovery_blocked_on_approval() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "agy-worker",
        "Agy Worker",
        crate::pane::PaneKind::Worker,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Agy),
    )
    .unwrap();

    let now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);
    pane.feed(b"Do you trust this workspace? [y/N]").unwrap();

    mux.add_pane(pane);

    mux.tick_pane_state_machine_at(rt.handle(), now);

    let recycled_pane = mux.get("agy-worker").unwrap();
    assert_eq!(recycled_pane.current_generation().0, 1);
    assert_eq!(recycled_pane.consecutive_crashes, 0);
    assert_eq!(
        recycled_pane.last_restart_reason.as_deref(),
        Some("blocked_on_approval")
    );
}

#[test]
fn test_agy_quarantine_after_repeated_crashes() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = ScopedEnv::set(&[("BREHON_SKIP_PREFLIGHT", "1")]);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::new_with_backend_cli(
        "agy-worker",
        "Agy Worker",
        crate::pane::PaneKind::Worker,
        crate::pane::PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Agy),
    )
    .unwrap();

    let mut now = Instant::now();
    pane.set_pane_busy(brehon_types::PromptId::new("P-1"), Generation(0), now);
    mux.add_pane(pane);

    // Crash 1
    mux.get_mut("agy-worker").unwrap().exited = true;
    mux.tick_pane_state_machine_at(rt.handle(), now);
    assert_eq!(mux.get("agy-worker").unwrap().consecutive_crashes, 1);

    // Wait a bit, then crash 2 (must be after backoff of 2 seconds)
    now += Duration::from_secs(3);
    mux.get_mut("agy-worker").unwrap().exited = true;
    mux.tick_pane_state_machine_at(rt.handle(), now);
    assert_eq!(mux.get("agy-worker").unwrap().consecutive_crashes, 2);

    // Wait a bit, then crash 3 (must be after backoff of 5 seconds)
    now += Duration::from_secs(6);
    mux.get_mut("agy-worker").unwrap().exited = true;
    mux.tick_pane_state_machine_at(rt.handle(), now);

    // Pane should now be quarantined (Dead state)
    let final_pane = mux.get("agy-worker").unwrap();
    assert!(matches!(
        final_pane.pane_state(),
        Some(PaneState::Dead {
            reason: DeathReason::Quarantined(_),
            ..
        })
    ));
}
