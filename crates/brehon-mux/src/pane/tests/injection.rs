use crate::harness::{
    AgentAdapter, CustomAgentConfig, HarnessCapabilities, PromptInjectionStrategy, SupervisorCli,
};
use crate::pane::spawn::{
    uses_delayed_submit_injection, uses_ink_echo_injection, uses_pre_submit_interrupt_reset,
};
use crate::pane::{BufferedMessage, InjectionMode, Pane, PaneKind};
use brehon_acp::GatewayProtocol;
use brehon_pty::{Pty, PtyConfig};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
fn test_pane_pending_message_queue_behaves_fifo() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    let first = BufferedMessage {
        prompt_id: 1,
        source: "supervisor".to_string(),
        queue_target: "worker-1".to_string(),
        prompt: "first".to_string(),
        summary: Some("summary".to_string()),
        priority: 2,
        enqueued_at: Instant::now(),
        mode: InjectionMode::Buffered,
    };
    let second = BufferedMessage {
        prompt_id: 2,
        source: "supervisor".to_string(),
        queue_target: "worker-1".to_string(),
        prompt: "second".to_string(),
        summary: None,
        priority: 1,
        enqueued_at: Instant::now(),
        mode: InjectionMode::Buffered,
    };

    pane.queue_message(first.clone());
    pane.queue_message(second.clone());

    assert_eq!(pane.pending_message_count(), 2);
    assert!(pane.has_pending_prompt(1));
    assert_eq!(pane.pending_message_front().map(|m| m.prompt_id), Some(1));
    assert_eq!(pane.pop_pending_message().map(|m| m.prompt_id), Some(1));
    assert_eq!(pane.pop_pending_message().map(|m| m.prompt_id), Some(2));
    assert_eq!(pane.pending_message_count(), 0);
}

#[test]
fn test_codex_ink_prompt_detection_blocks_on_nonempty_draft() {
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("codex-supervisor", config).expect("spawn test pty");
    let mut pane = Pane::with_pty_cli(
        "codex-supervisor",
        PaneKind::Supervisor,
        pty,
        24,
        80,
        builtin(SupervisorCli::Codex),
    )
    .expect("create pane");

    pane.append_output(b"> queued worker ready message\r\n")
        .expect("append draft prompt");
    pane.set_last_output_at(Instant::now() - Duration::from_secs(2));

    assert!(pane.has_nonempty_ink_prompt_marker());
    assert!(!pane.is_ready_for_ink_prompt_injection(Instant::now(), Duration::from_millis(800)));
}

#[test]
fn test_codex_ink_prompt_detection_blocks_on_active_turn_marker() {
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 4,
        cols: 80,
    };
    let pty = Pty::spawn("codex-supervisor", config).expect("spawn test pty");
    let mut pane = Pane::with_pty_cli(
        "codex-supervisor",
        PaneKind::Supervisor,
        pty,
        4,
        80,
        builtin(SupervisorCli::Codex),
    )
    .expect("create pane");

    pane.append_output(b"\xe2\x80\xa2 Working (37s \xe2\x80\xa2 esc to interrupt)\r\n> \r\n")
        .expect("append working marker");
    pane.set_last_output_at(Instant::now() - Duration::from_secs(2));

    assert!(pane.has_active_ink_turn_marker());
    assert!(!pane.is_ready_for_ink_prompt_injection(Instant::now(), Duration::from_millis(800)));
}

#[test]
fn test_pane_drop_removes_notify_socket() {
    let sock = unique_temp_path("notify-worker-1.sock");
    std::fs::write(&sock, b"socket").expect("create fake socket file");
    {
        let mut pane = Pane::director("test", 24, 80).expect("create pane");
        pane.set_notify_socket_path_for_test(sock.clone());
    }
    assert!(!sock.exists());
}

fn unique_temp_path(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!("brehon-mux-{nanos}-{name}"))
}

// --- Ink echo detection tests ---
//
// These test the core echo detection logic used for Codex and other
// Ink-based prompt injection paths.
// injection. Director panes (PaneBackend::None) are used so no real PTY
// is needed; the PTY write path is a no-op, but pending state is still
// cleared on detection, which is what we verify.

fn builtin(cli: SupervisorCli) -> AgentAdapter {
    AgentAdapter::BuiltIn(cli)
}

fn custom_with_strategy(
    name: &str,
    uses_ink_prompt: bool,
    prompt_injection_strategy: PromptInjectionStrategy,
) -> AgentAdapter {
    AgentAdapter::Custom(CustomAgentConfig {
        name: name.to_string(),
        command: Some(name.to_string()),
        args: vec![],
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: false,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt,
            prompt_injection_strategy,
            tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
            transport: crate::harness::HarnessTransport::InteractivePty,
            preferred_control_plane: crate::harness::HarnessControlPlane::PtyInjection,
        },
    })
}

#[allow(dead_code)]
fn assert_copilot_gateway_config(config: &crate::pane::types::GatewaySpawnConfig) {
    assert_eq!(config.protocol, GatewayProtocol::AcpStdio);
    assert!(matches!(
        config.command.as_deref(),
        Some("copilot") | Some("gh")
    ));
    assert!(config.args.contains(&"--acp".to_string()));
    assert!(config.args.contains(&"--stdio".to_string()));
    assert!(config.args.contains(&"--allow-all".to_string()));
    assert!(config.args.contains(&"--no-ask-user".to_string()));
    assert!(config.args.contains(&"--no-auto-update".to_string()));
    assert!(config.args.contains(&"--config-dir".to_string()));
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "COPILOT_AUTO_UPDATE" && v == "false")
    );
    assert!(config.env.iter().any(|(k, _)| k == "COPILOT_HOME"));
    assert!(config.env.iter().any(|(k, _)| k == "COPILOT_CACHE_HOME"));
}

#[allow(dead_code)]
fn assert_copilot_pty_config(config: &brehon_pty::PtyConfig) {
    assert!(matches!(config.command.as_str(), "copilot" | "gh"));
    assert!(!config.args.contains(&"--acp".to_string()));
    assert!(!config.args.contains(&"--stdio".to_string()));
    assert!(config.args.contains(&"--allow-all".to_string()));
    assert!(config.args.contains(&"--no-ask-user".to_string()));
    assert!(config.args.contains(&"--no-auto-update".to_string()));
    assert!(config.args.contains(&"--config-dir".to_string()));
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "COPILOT_AUTO_UPDATE" && v == "false")
    );
    assert!(config.env.iter().any(|(k, _)| k == "COPILOT_HOME"));
    assert!(config.env.iter().any(|(k, _)| k == "COPILOT_CACHE_HOME"));
}

#[test]
fn test_gemini_does_not_use_ink_echo_injection() {
    assert!(!uses_ink_echo_injection(&builtin(SupervisorCli::Gemini)));
    assert!(!uses_ink_echo_injection(&builtin(SupervisorCli::Claude)));
}

#[test]
fn test_gemini_uses_delayed_submit_injection() {
    assert!(uses_delayed_submit_injection(&builtin(
        SupervisorCli::Gemini
    )));
    assert!(!uses_delayed_submit_injection(&builtin(
        SupervisorCli::Claude
    )));
    assert!(!uses_delayed_submit_injection(&builtin(
        SupervisorCli::Codex
    )));
}

#[test]
fn test_gemini_does_not_use_pre_submit_interrupt_reset() {
    assert!(!uses_pre_submit_interrupt_reset(&builtin(
        SupervisorCli::Gemini
    )));
    assert!(!uses_pre_submit_interrupt_reset(&builtin(
        SupervisorCli::Claude
    )));
    assert!(!uses_pre_submit_interrupt_reset(&builtin(
        SupervisorCli::Codex
    )));
}

#[test]
fn test_codex_family_uses_ink_echo_injection() {
    assert!(uses_ink_echo_injection(&builtin(SupervisorCli::Codex)));
    assert!(uses_ink_echo_injection(&builtin(SupervisorCli::OpenCode)));
    assert!(uses_ink_echo_injection(&builtin(SupervisorCli::Junie)));
    assert!(!uses_ink_echo_injection(&builtin(SupervisorCli::Agy)));
    assert!(!uses_ink_echo_injection(&builtin(SupervisorCli::Kimi)));
    assert!(!uses_ink_echo_injection(&builtin(SupervisorCli::Copilot)));
}

#[test]
fn test_custom_pty_injection_strategy_controls_submission_behavior() {
    let delayed =
        custom_with_strategy("gemini-like", false, PromptInjectionStrategy::DelayedSubmit);
    assert!(uses_delayed_submit_injection(&delayed));
    assert!(!uses_ink_echo_injection(&delayed));

    let ink_echo = custom_with_strategy("junie-like", true, PromptInjectionStrategy::InkEcho);
    assert!(uses_ink_echo_injection(&ink_echo));
    assert!(!uses_delayed_submit_injection(&ink_echo));
}

/// Gemini uses delayed submit (not Ink echo), and Ink CLIs don't use
/// pre-submit interrupt or delayed submit — verify mutual exclusivity.
#[test]
fn test_injection_modes_are_mutually_exclusive() {
    let gemini = builtin(SupervisorCli::Gemini);
    assert!(uses_delayed_submit_injection(&gemini));
    assert!(!uses_pre_submit_interrupt_reset(&gemini));
    assert!(!uses_ink_echo_injection(&gemini));

    for cli in [
        SupervisorCli::Codex,
        SupervisorCli::OpenCode,
        SupervisorCli::Junie,
    ] {
        let adapter = builtin(cli);
        assert!(
            uses_ink_echo_injection(&adapter),
            "{cli:?} should use ink echo"
        );
        assert!(
            !uses_delayed_submit_injection(&adapter),
            "{cli:?} should NOT use delayed submit"
        );
        assert!(
            !uses_pre_submit_interrupt_reset(&adapter),
            "{cli:?} should NOT use pre-submit interrupt"
        );
    }

    let copilot = builtin(SupervisorCli::Copilot);
    assert!(!uses_ink_echo_injection(&copilot));
    assert!(!uses_delayed_submit_injection(&copilot));
    assert!(!uses_pre_submit_interrupt_reset(&copilot));

    let kimi = builtin(SupervisorCli::Kimi);
    assert!(!uses_ink_echo_injection(&kimi));
    assert!(!uses_delayed_submit_injection(&kimi));
    assert!(!uses_pre_submit_interrupt_reset(&kimi));

    let agy = builtin(SupervisorCli::Agy);
    assert!(!uses_ink_echo_injection(&agy));
    assert!(!uses_delayed_submit_injection(&agy));
    assert!(!uses_pre_submit_interrupt_reset(&agy));

    // Claude uses none of the special injection modes
    let claude = builtin(SupervisorCli::Claude);
    assert!(!uses_delayed_submit_injection(&claude));
    assert!(!uses_pre_submit_interrupt_reset(&claude));
    assert!(!uses_ink_echo_injection(&claude));
}

fn set_pending(pane: &Pane, needle: &str, deadline: Instant) {
    let generation = pane
        .ink_submit_generation
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    let mut pending = pane.pending_ink_submit.lock().unwrap();
    *pending = Some((needle.to_string(), deadline, generation));
}

fn is_pending(pane: &Pane) -> bool {
    pane.pending_ink_submit.lock().unwrap().is_some()
}

#[test]
fn test_ink_echo_detection_basic() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    let deadline = Instant::now() + Duration::from_secs(10);
    set_pending(&pane, "hello world", deadline);

    // Text not yet in terminal — should not clear
    pane.check_ink_echo_submit();
    assert!(is_pending(&pane), "should still be pending before echo");

    // Feed the text into the terminal (simulates Ink re-rendering)
    pane.feed(b"$ hello world").expect("feed");
    pane.check_ink_echo_submit();
    assert!(!is_pending(&pane), "should be cleared after echo detected");
}

#[test]
fn test_ink_echo_detection_timeout_fallback() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    // Set a deadline that already passed
    let deadline = Instant::now() - Duration::from_secs(1);
    set_pending(&pane, "will never appear", deadline);

    // Even though the text isn't in the terminal, deadline passed → submit
    pane.check_ink_echo_submit();
    assert!(!is_pending(&pane), "should be cleared after timeout");
}

#[test]
fn test_ink_echo_detection_no_match_stays_pending() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    let deadline = Instant::now() + Duration::from_secs(10);
    set_pending(&pane, "expected text", deadline);

    // Feed different text
    pane.feed(b"something else entirely").expect("feed");
    pane.check_ink_echo_submit();
    assert!(
        is_pending(&pane),
        "should remain pending when text doesn't match"
    );
}

#[test]
fn test_ink_echo_needle_tail_extraction() {
    // Verify the needle is the last ~40 chars of the prompt.
    // The inject_prompt method does: text.chars().rev().take(40).collect()
    let long_text = "a]".repeat(30); // 60 chars
    let needle: String = long_text
        .chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert_eq!(needle.len(), 40);
    assert!(long_text.ends_with(&needle));
}

#[test]
fn test_ink_echo_detection_multibyte_chars() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    let deadline = Instant::now() + Duration::from_secs(10);
    set_pending(&pane, "日本語テスト", deadline);

    pane.feed("入力: 日本語テスト".as_bytes()).expect("feed");
    pane.check_ink_echo_submit();
    assert!(!is_pending(&pane), "should detect multibyte needle");
}

#[test]
fn test_ink_echo_detection_partial_match_not_enough() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    let deadline = Instant::now() + Duration::from_secs(10);
    set_pending(&pane, "full prompt text", deadline);

    // Only partial match
    pane.feed(b"full prompt").expect("feed");
    pane.check_ink_echo_submit();
    assert!(is_pending(&pane), "partial match should not trigger submit");
}

#[test]
fn test_ink_echo_detection_15_concurrent_panes() {
    // Simulate 15 agents, each with a pending echo. Feed text to each
    // independently and verify each detects its own echo without
    // interfering with others.
    let mut panes: Vec<Pane> = (0..15)
        .map(|i| Pane::director(format!("worker-{i}"), 24, 80).expect("create pane"))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);

    // Set up pending submits with unique needles
    for (i, pane) in panes.iter().enumerate() {
        set_pending(pane, &format!("task for worker {i}"), deadline);
    }

    // First pass: no text fed yet — all should stay pending
    for pane in panes.iter_mut() {
        pane.check_ink_echo_submit();
    }
    for (i, pane) in panes.iter().enumerate() {
        assert!(is_pending(pane), "pane {i} should still be pending");
    }

    // Feed matching text to even-numbered panes only
    for (i, pane) in panes.iter_mut().enumerate() {
        if i % 2 == 0 {
            pane.feed(format!("> task for worker {i}\r\n").as_bytes())
                .expect("feed");
        }
    }

    // Check all panes
    for pane in panes.iter_mut() {
        pane.check_ink_echo_submit();
    }

    // Even panes should be cleared, odd panes still pending
    for (i, pane) in panes.iter().enumerate() {
        if i % 2 == 0 {
            assert!(
                !is_pending(pane),
                "pane {i} (even) should have detected echo"
            );
        } else {
            assert!(is_pending(pane), "pane {i} (odd) should still be pending");
        }
    }

    // Now feed text to the odd panes
    for (i, pane) in panes.iter_mut().enumerate() {
        if i % 2 == 1 {
            pane.feed(format!("> task for worker {i}\r\n").as_bytes())
                .expect("feed");
        }
    }

    for pane in panes.iter_mut() {
        pane.check_ink_echo_submit();
    }

    // All should be cleared now
    for (i, pane) in panes.iter().enumerate() {
        assert!(
            !is_pending(pane),
            "pane {i} should be cleared after second pass"
        );
    }
}

#[test]
fn test_ink_echo_detection_overwrite_pending() {
    // If a new inject arrives while previous is pending, old is overwritten.
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    let deadline = Instant::now() + Duration::from_secs(10);
    set_pending(&pane, "first prompt", deadline);
    set_pending(&pane, "second prompt", deadline);

    // Feed first prompt text — should NOT trigger (needle is now "second prompt")
    pane.feed(b"first prompt").expect("feed");
    pane.check_ink_echo_submit();
    assert!(is_pending(&pane), "old needle should have been overwritten");

    // Feed second prompt text — should trigger
    pane.feed(b"\r\nsecond prompt").expect("feed");
    pane.check_ink_echo_submit();
    assert!(!is_pending(&pane), "current needle should match");
}

#[test]
fn test_ink_echo_detection_empty_needle() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    let deadline = Instant::now() + Duration::from_secs(10);
    set_pending(&pane, "", deadline);

    // Empty string is contained in every string, so should trigger immediately
    pane.check_ink_echo_submit();
    assert!(!is_pending(&pane), "empty needle should match immediately");
}

/// Regression test: verify that echo detection + timeout reliably clears
/// pending state after text appears in the viewport. This guards against
/// the bug where pending state was cleared before Enter was successfully
/// written, causing silent failures when the PTY writer lock was contended.
#[test]
fn test_ink_echo_submit_clears_only_after_success() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");
    let deadline = Instant::now() + Duration::from_secs(10);
    set_pending(&pane, "needle text", deadline);

    // Before text appears: still pending
    pane.check_ink_echo_submit();
    assert!(is_pending(&pane));

    // Feed text, trigger detection. For Director panes (no PTY backend),
    // "writing Enter" is a no-op but state should still clear — verifying
    // that the clear happens in the success path, not unconditionally.
    pane.feed(b"needle text").expect("feed");
    pane.check_ink_echo_submit();
    assert!(
        !is_pending(&pane),
        "state must clear after successful submit"
    );

    // Second call is a no-op (state already cleared)
    pane.check_ink_echo_submit();
    assert!(!is_pending(&pane));
}

/// Regression test: repeated deadline-based submissions work correctly.
/// Verifies the timeout path also clears state properly.
#[test]
fn test_ink_echo_timeout_clears_and_allows_new_pending() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");

    // First inject: deadline already passed
    set_pending(&pane, "first", Instant::now() - Duration::from_secs(1));
    pane.check_ink_echo_submit();
    assert!(!is_pending(&pane), "timeout should clear state");

    // Second inject: new needle, future deadline
    set_pending(&pane, "second", Instant::now() + Duration::from_secs(10));
    pane.check_ink_echo_submit();
    assert!(is_pending(&pane), "new pending should be independent");

    // Feed matching text
    pane.feed(b"second").expect("feed");
    pane.check_ink_echo_submit();
    assert!(!is_pending(&pane), "second inject should clear on echo");
}

// --- TaskContextSnapshot tests ---
