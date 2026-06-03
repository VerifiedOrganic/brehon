use crate::{PromptQueueEntry, SessionScopedQueue};

#[test]
fn prompt_queue_session_scoping_drains_only_current_session() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let queue_root = tempdir.path().join("prompt-queue");

    let queue_a = SessionScopedQueue::new("session-a", queue_root.clone());
    let queue_b = SessionScopedQueue::new("session-b", queue_root.clone());

    queue_a
        .enqueue(PromptQueueEntry::new(
            "worker-a",
            Some("supervisor"),
            "session-a message",
        ))
        .expect("enqueue session-a prompt");
    queue_b
        .enqueue(PromptQueueEntry::new(
            "worker-b",
            Some("supervisor"),
            "session-b message",
        ))
        .expect("enqueue session-b prompt");

    let queue_a_reader =
        SessionScopedQueue::<PromptQueueEntry>::new("session-a", queue_root.clone());
    let drained: Vec<_> = queue_a_reader.drain().collect();
    assert_eq!(
        drained.len(),
        1,
        "session-a should drain only its own prompt"
    );

    let entry = drained[0]
        .as_ref()
        .expect("session-a entry should decode correctly");
    assert_eq!(entry.session_name, "session-a");
    assert_eq!(entry.entry.target, "worker-a");
    assert_eq!(entry.entry.message, "session-a message");

    let dead_letter_dir = queue_root.join("dead-letter");
    assert!(
        !dead_letter_dir.exists(),
        "foreign session prompts should not be dead-lettered"
    );

    let queue_b_reader = SessionScopedQueue::<PromptQueueEntry>::new("session-b", queue_root);
    let drained_b: Vec<_> = queue_b_reader.drain().collect();
    assert_eq!(
        drained_b.len(),
        1,
        "session-b prompt should remain available to session-b"
    );
    let entry = drained_b[0]
        .as_ref()
        .expect("session-b entry should decode correctly");
    assert_eq!(entry.session_name, "session-b");
    assert_eq!(entry.entry.target, "worker-b");
    assert_eq!(entry.entry.message, "session-b message");
}
