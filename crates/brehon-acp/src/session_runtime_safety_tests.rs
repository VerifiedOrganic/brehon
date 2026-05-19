/// Integration-style test: calling wait_for_response with a short timeout
/// should clean up both the sender and receiver from the pending maps,
/// exercising the real session codepath rather than raw HashMap operations.
#[tokio::test]
async fn test_wait_for_response_timeout_cleans_up_pending_maps() {
    let session = test_session();

    let (tx, rx) = oneshot::channel::<Result<crate::acp_types::PromptResult, String>>();
    let prompt_id = "prompt-timeout-test";
    session
        .inner
        .pending_prompt_responses
        .lock()
        .await
        .insert(prompt_id.to_string(), tx);
    session
        .inner
        .prompt_response_receivers
        .lock()
        .await
        .insert(prompt_id.to_string(), rx);

    assert_eq!(session.inner.pending_prompt_responses.lock().await.len(), 1);
    assert_eq!(
        session.inner.prompt_response_receivers.lock().await.len(),
        1
    );

    let result = session
        .wait_for_response(&PromptId::new(prompt_id), 50)
        .await;
    assert!(matches!(result, Err(SessionError::Timeout)));

    assert!(
        session
            .inner
            .pending_prompt_responses
            .lock()
            .await
            .is_empty(),
        "pending_prompt_responses should be empty after timeout"
    );
    assert!(
        session
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .is_empty(),
        "prompt_response_receivers should be empty after timeout"
    );
}

/// Integration-style test: when the sender half of a prompt-response
/// channel is dropped (simulating process death), wait_for_response
/// should return ProcessDied and both maps should be empty.
#[tokio::test]
async fn test_wait_for_response_sender_dropped_cleans_up() {
    let session = test_session();

    let (tx, rx) = oneshot::channel::<Result<crate::acp_types::PromptResult, String>>();
    let prompt_id = "prompt-dropped-test";
    session
        .inner
        .pending_prompt_responses
        .lock()
        .await
        .insert(prompt_id.to_string(), tx);
    session
        .inner
        .prompt_response_receivers
        .lock()
        .await
        .insert(prompt_id.to_string(), rx);

    let sender = session
        .inner
        .pending_prompt_responses
        .lock()
        .await
        .remove(prompt_id);
    drop(sender);

    let result = session
        .wait_for_response(&PromptId::new(prompt_id), 500)
        .await;
    assert!(
        matches!(result, Err(SessionError::ProcessDied)),
        "expected ProcessDied when sender is dropped, got {:?}",
        result
    );
}

/// Integration-style test: successful prompt completion should clean up
/// the sender from pending_prompt_responses, while the receiver is
/// consumed by wait_for_response.
#[tokio::test]
async fn test_wait_for_response_success_cleans_up_sender() {
    let session = test_session();

    let (tx, rx) = oneshot::channel::<Result<crate::acp_types::PromptResult, String>>();
    let prompt_id = "prompt-success-test";
    session
        .inner
        .pending_prompt_responses
        .lock()
        .await
        .insert(prompt_id.to_string(), tx);
    session
        .inner
        .prompt_response_receivers
        .lock()
        .await
        .insert(prompt_id.to_string(), rx);

    let sender_map = Arc::clone(&session.inner);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let sender = sender_map
            .pending_prompt_responses
            .lock()
            .await
            .remove(prompt_id);
        if let Some(sender) = sender {
            let _ = sender.send(Ok(crate::acp_types::PromptResult::default()));
        }
    });

    let result = session
        .wait_for_response(&PromptId::new(prompt_id), 2000)
        .await;
    assert!(
        result.is_ok(),
        "expected successful prompt result, got {:?}",
        result
    );

    assert!(
        session
            .inner
            .pending_prompt_responses
            .lock()
            .await
            .is_empty(),
        "pending_prompt_responses should be empty after success"
    );
    assert!(
        session
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .is_empty(),
        "prompt_response_receivers should be empty after success"
    );
}

#[tokio::test]
async fn pending_requests_send_request_not_running_cleans_up_and_counts_blocked_send() {
    let session = test_session();
    let request = super::super::super::protocol::JsonRpcRequest::new("method", None);

    let err = session.send_request(request).await.unwrap_err();
    assert!(matches!(err, SessionError::NotRunning));
    assert!(session.inner.pending_requests.lock().await.is_empty());
    assert_eq!(session.inner.blocked_sends.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn pending_requests_cancel_prompt_not_running_cleans_prompt_results() {
    let session = test_session();
    let prompt_id = PromptId::new("prompt-cancel-cleanup");
    let (tx, rx) = oneshot::channel::<Result<crate::acp_types::PromptResult, String>>();
    session
        .inner
        .pending_prompt_responses
        .lock()
        .await
        .insert(prompt_id.as_str().to_string(), tx);
    session
        .inner
        .prompt_response_receivers
        .lock()
        .await
        .insert(prompt_id.as_str().to_string(), rx);
    *session.inner.active_prompt_id.lock().await = Some(prompt_id.as_str().to_string());

    let err = session
        .cancel_prompt(&prompt_id, Some("test cancellation"))
        .await
        .unwrap_err();

    assert!(matches!(err, SessionError::NotRunning));
    assert!(session
        .inner
        .pending_prompt_responses
        .lock()
        .await
        .is_empty());
    assert!(session
        .inner
        .prompt_response_receivers
        .lock()
        .await
        .is_empty());
    assert!(session.inner.active_prompt_id.lock().await.is_none());
}

#[tokio::test]
async fn test_kill_wakes_outstanding_prompt_and_request_waiters() {
    let session = test_session_with_long_lived_process().await;

    let prompt = PromptTurn {
        prompt_id: PromptId::new("prompt-kill"),
        content: "hello".to_string(),
        kind: MessageKind::TaskAssignment,
        sent_at: chrono::Utc::now(),
    };
    let prompt_id = prompt.prompt_id.clone();
    session
        .send_prompt(prompt)
        .await
        .expect("prompt should register successfully");

    let prompt_waiting = Arc::new(Notify::new());
    let request_waiting = Arc::new(Notify::new());

    let prompt_wait = tokio::spawn({
        let session = Arc::clone(&session);
        let prompt_id = prompt_id.clone();
        let prompt_waiting = Arc::clone(&prompt_waiting);
        async move {
            session
                .wait_for_response_internal(&prompt_id, 30_000, Some(prompt_waiting))
                .await
        }
    });

    let request_wait = tokio::spawn({
        let session = Arc::clone(&session);
        let request_waiting = Arc::clone(&request_waiting);
        async move {
            session
                .send_request_internal(
                    JsonRpcRequest::new("test/request", None),
                    Some(request_waiting),
                )
                .await
        }
    });

    prompt_waiting.notified().await;
    request_waiting.notified().await;

    session.kill().await.expect("kill should succeed");

    let prompt_result = tokio::time::timeout(std::time::Duration::from_millis(250), prompt_wait)
        .await
        .expect("prompt waiter should wake promptly")
        .expect("prompt waiter task should join");
    assert!(matches!(prompt_result, Err(SessionError::ProcessDied)));

    let request_result = tokio::time::timeout(std::time::Duration::from_millis(250), request_wait)
        .await
        .expect("request waiter should wake promptly")
        .expect("request waiter task should join");
    assert!(matches!(request_result, Err(SessionError::ProcessDied)));

    assert!(session
        .inner
        .pending_prompt_responses
        .lock()
        .await
        .is_empty());
    assert!(session
        .inner
        .prompt_response_receivers
        .lock()
        .await
        .is_empty());
    assert!(session.inner.pending_requests.lock().await.is_empty());
}

/// Verify that the session reader task is tracked and its JoinHandle is
/// awaited during kill(), ensuring deterministic shutdown ownership.
#[tokio::test]
async fn test_kill_awaits_reader_task() {
    let session = test_session_with_long_lived_process().await;

    assert!(
        session.inner.reader_handle.lock().await.is_some(),
        "reader_handle should be set after spawning a session with a real process"
    );

    let kill_result =
        tokio::time::timeout(std::time::Duration::from_secs(10), session.kill()).await;
    assert!(kill_result.is_ok(), "kill should complete within timeout");
    kill_result.unwrap().expect("kill should succeed");

    assert!(
        session.inner.reader_handle.lock().await.is_none(),
        "reader_handle should be None after kill consumes it"
    );
}

#[tokio::test]
async fn test_drop_schedules_shutdown_and_reaps_reader_task() {
    let session = test_session_with_long_lived_process().await;
    let inner = Arc::clone(&session.inner);
    assert!(inner.reader_handle.lock().await.is_some());

    drop(session);

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let peer_gone = inner.peer.lock().await.is_none();
            let reader_gone = inner.reader_handle.lock().await.is_none();
            if peer_gone && reader_gone && inner.shutdown.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dropped session should clean up peer and reader");

    assert!(!inner.alive.load(Ordering::SeqCst));
    assert!(inner.pending_requests.lock().await.is_empty());
    assert!(inner.pending_prompt_responses.lock().await.is_empty());
    assert!(inner.prompt_response_receivers.lock().await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn backpressure_high_output_process_does_not_deadlock_reader() {
    let process = AgentProcess::spawn(
        "sh",
        &[
            "-c".to_string(),
            "i=0; while [ \"$i\" -lt 5000 ]; do echo \"line-$i\"; i=$((i + 1)); done"
                .to_string(),
        ],
        std::env::temp_dir()
            .to_str()
            .expect("temp dir should be valid UTF-8"),
    )
    .await
    .expect("high-output helper process should spawn");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while process.is_alive() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("high-output process should exit without stdout backpressure deadlock");

    process
        .kill()
        .await
        .expect("dead process cleanup should succeed");
    assert!(
        process.stdout_dropped_lines() > 0,
        "bounded stdout channel should record dropped lines when the reader falls behind"
    );
}

/// Verify that setting the shutdown flag causes the reader to exit promptly
/// without waiting for the next recv_line timeout.
#[tokio::test]
async fn test_shutdown_flag_stops_reader_loop() {
    let session = test_session_with_long_lived_process().await;

    assert!(session.inner.alive.load(Ordering::SeqCst));
    assert!(!session.inner.shutdown.load(Ordering::SeqCst));

    session.inner.shutdown.store(true, Ordering::SeqCst);
    session.kill().await.expect("kill should succeed");

    assert!(session.inner.shutdown.load(Ordering::SeqCst));
    assert!(!session.inner.alive.load(Ordering::SeqCst));
}

/// Verify that kill() on a session with no peer still clears state and
/// awaits the reader handle gracefully (even if it's None).
#[tokio::test]
async fn test_kill_with_no_process_clears_state() {
    let session = test_session();

    assert!(session.inner.peer.lock().await.is_none());
    assert!(session.inner.reader_handle.lock().await.is_none());

    session
        .kill()
        .await
        .expect("kill on session with no process should succeed");

    assert!(!session.inner.alive.load(Ordering::SeqCst));
    assert!(session.inner.shutdown.load(Ordering::SeqCst));
    assert!(session.inner.pending_requests.lock().await.is_empty());
    assert!(session
        .inner
        .pending_prompt_responses
        .lock()
        .await
        .is_empty());
    assert!(session
        .inner
        .prompt_response_receivers
        .lock()
        .await
        .is_empty());
}
