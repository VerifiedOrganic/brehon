use super::*;
use tantivy::doc;
use tempfile::TempDir;

#[test]
fn test_view_drift_flags_missing_task_view() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    let events_dir = brehon_root.join("runtime").join("events");
    let keyspace = fjall::Config::new(&events_dir).open().unwrap();

    let events = keyspace
        .open_partition(EVENTS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();
    let _views = keyspace
        .open_partition(VIEWS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();

    let event = EventEnvelope {
        event: Event {
            kind: EventKind::TaskCreated {
                task_id: "T-001".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T-001".to_string(),
        },
        event_id: EventId::new(1),
        correlation_id: None,
        causation_id: None,
        idempotency_key: None,
    };
    let encoded = brehon_types::serialize_event_envelope(&event).unwrap();
    events
        .insert(format!("log:{:020}", 1).as_bytes(), &encoded)
        .unwrap();
    let views = keyspace
        .open_partition(VIEWS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();

    let checker = StoreSearchChecker::new(&brehon_root);
    let findings = checker.check_view_drift(&events, &views).unwrap();
    assert_eq!(findings.len(), 1);
    assert!(findings[0].summary.contains("Task view missing"));
}

#[test]
fn test_review_view_drift_flags_missing_review_view() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    let events_dir = brehon_root.join("runtime").join("events");
    let keyspace = fjall::Config::new(&events_dir).open().unwrap();

    let events = keyspace
        .open_partition(EVENTS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();
    let _views = keyspace
        .open_partition(VIEWS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();

    let event = EventEnvelope {
        event: Event {
            kind: EventKind::ReviewRequested {
                task_id: "T-001".to_string(),
                review_id: "REV-001".to_string(),
            },
            timestamp: Utc::now(),
            aggregate_id: "T-001".to_string(),
        },
        event_id: EventId::new(1),
        correlation_id: None,
        causation_id: None,
        idempotency_key: None,
    };
    let encoded = brehon_types::serialize_event_envelope(&event).unwrap();
    events
        .insert(format!("log:{:020}", 1).as_bytes(), &encoded)
        .unwrap();
    let views = keyspace
        .open_partition(VIEWS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();

    let checker = StoreSearchChecker::new(&brehon_root);
    let findings = checker.check_view_drift(&events, &views).unwrap();
    let review_findings: Vec<_> = findings
        .iter()
        .filter(|f| f.summary.contains("Review view missing"))
        .collect();
    assert_eq!(review_findings.len(), 1);
}

#[test]
fn test_view_drift_ignores_updated_at() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    let events_dir = brehon_root.join("runtime").join("events");
    let keyspace = fjall::Config::new(&events_dir).open().unwrap();
    let events = keyspace
        .open_partition(EVENTS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();
    let views = keyspace
        .open_partition(VIEWS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();

    let event_time = Utc::now() - chrono::Duration::minutes(5);
    let event = EventEnvelope {
        event: Event {
            kind: EventKind::TaskCreated {
                task_id: "T-001".to_string(),
            },
            timestamp: event_time,
            aggregate_id: "T-001".to_string(),
        },
        event_id: EventId::new(1),
        correlation_id: None,
        causation_id: None,
        idempotency_key: None,
    };
    let encoded = brehon_types::serialize_event_envelope(&event).unwrap();
    events
        .insert(format!("log:{:020}", 1).as_bytes(), &encoded)
        .unwrap();

    let mut stored_view = default_task_view("T-001");
    stored_view.last_event_id = 1;
    stored_view.updated_at = Utc::now();
    let encoded = serde_json::to_vec(&stored_view).unwrap();
    views.insert(b"view:task:T-001", &encoded).unwrap();

    let checker = StoreSearchChecker::new(&brehon_root);
    let findings = checker.check_view_drift(&events, &views).unwrap();
    assert!(findings.is_empty());
}

#[test]
fn test_review_view_drift_ignores_updated_at() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    let events_dir = brehon_root.join("runtime").join("events");
    let keyspace = fjall::Config::new(&events_dir).open().unwrap();
    let events = keyspace
        .open_partition(EVENTS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();
    let views = keyspace
        .open_partition(VIEWS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();

    let event_time = Utc::now() - chrono::Duration::minutes(5);
    let event = EventEnvelope {
        event: Event {
            kind: EventKind::ReviewRequested {
                task_id: "T-001".to_string(),
                review_id: "REV-001".to_string(),
            },
            timestamp: event_time,
            aggregate_id: "T-001".to_string(),
        },
        event_id: EventId::new(1),
        correlation_id: None,
        causation_id: None,
        idempotency_key: None,
    };
    let encoded = brehon_types::serialize_event_envelope(&event).unwrap();
    events
        .insert(format!("log:{:020}", 1).as_bytes(), &encoded)
        .unwrap();

    let mut stored_review = default_review_view("REV-001");
    stored_review.task_id = "T-001".to_string();
    stored_review.status = ReviewStatus::Pending;
    stored_review.round = 1;
    stored_review.last_event_id = 1;
    stored_review.updated_at = Utc::now();
    let encoded = serde_json::to_vec(&stored_review).unwrap();
    views.insert(b"view:review:REV-001", &encoded).unwrap();

    let mut stored_task = default_task_view("T-001");
    stored_task.status = TaskStatus::InReview;
    stored_task.review_rounds = 1;
    stored_task.last_event_id = 1;
    stored_task.updated_at = Utc::now();
    let encoded = serde_json::to_vec(&stored_task).unwrap();
    views.insert(b"view:task:T-001", &encoded).unwrap();

    let checker = StoreSearchChecker::new(&brehon_root);
    let findings = checker.check_view_drift(&events, &views).unwrap();
    assert!(findings.is_empty());
}

#[test]
fn test_queue_lease_detects_orphaned_claim() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    let events_dir = brehon_root.join("runtime").join("events");
    let keyspace = fjall::Config::new(&events_dir).open().unwrap();
    let queue = keyspace
        .open_partition(QUEUE_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();

    let claim = QueueClaim {
        claim_id: brehon_types::ClaimId::new("c1"),
        queue: "review:high".to_string(),
        item_id: "T-001".to_string(),
        consumer: "worker".to_string(),
        expires_at: Utc::now() + chrono::Duration::minutes(1),
        lease_epoch: None,
        lease_duration_ms: None,
        monotonic_deadline_ms: None,
    };
    queue
        .insert(b"lease:c1", &serde_json::to_vec(&claim).unwrap())
        .unwrap();
    let checker = StoreSearchChecker::new(&brehon_root);
    let findings = checker.check_queue_lease_consistency(&queue).unwrap();
    assert_eq!(findings.len(), 1);
    assert!(findings[0].summary.contains("Lease without claimed marker"));
}

#[test]
fn test_parse_claimed_key_with_colon_queue() {
    let parsed = parse_claimed_key("claimed:review:high:T-001").unwrap();
    assert_eq!(parsed, ("review:high".to_string(), "T-001".to_string()));
}

#[test]
fn test_parse_claimed_key_edge_cases() {
    assert_eq!(
        parse_claimed_key("claimed:review:high"),
        Some(("review".to_string(), "high".to_string()))
    );
    assert!(parse_claimed_key("claimed:").is_none());
}

#[test]
fn test_tantivy_fjall_drift_detects_extra_index_entries() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    let events_dir = brehon_root.join("runtime").join("events");
    let keyspace = fjall::Config::new(&events_dir).open().unwrap();
    let events = keyspace
        .open_partition(EVENTS_PARTITION, fjall::PartitionCreateOptions::default())
        .unwrap();

    let event = EventEnvelope {
        event: Event {
            kind: EventKind::MemoryCreated {
                memory_id: "mem-1".to_string(),
                content: "test".to_string(),
                tags: vec!["tag".to_string()],
                source_agent: Some("test-agent".to_string()),
            },
            timestamp: Utc::now(),
            aggregate_id: "mem-1".to_string(),
        },
        event_id: EventId::new(1),
        correlation_id: None,
        causation_id: None,
        idempotency_key: None,
    };
    let encoded = brehon_types::serialize_event_envelope(&event).unwrap();
    events
        .insert(format!("log:{:020}", 1).as_bytes(), &encoded)
        .unwrap();

    let tantivy_root = brehon_root.join(DEFAULT_TANTIVY_PATH);
    let index_root = tantivy_root.join("index");
    std::fs::create_dir_all(&index_root).unwrap();
    let mut schema_builder = tantivy::schema::Schema::builder();
    let id_field =
        schema_builder.add_text_field("id", tantivy::schema::STRING | tantivy::schema::STORED);
    let content_field =
        schema_builder.add_text_field("content", tantivy::schema::TEXT | tantivy::schema::STORED);
    let tags_field =
        schema_builder.add_text_field("tags", tantivy::schema::TEXT | tantivy::schema::STORED);
    let source_field =
        schema_builder.add_text_field("source", tantivy::schema::STRING | tantivy::schema::STORED);
    let category_field = schema_builder.add_text_field(
        "category",
        tantivy::schema::STRING | tantivy::schema::STORED,
    );
    let timestamp_field = schema_builder.add_text_field(
        "timestamp",
        tantivy::schema::STRING | tantivy::schema::STORED,
    );
    let schema = schema_builder.build();
    let directory = tantivy::directory::MmapDirectory::open(index_root).unwrap();
    let index =
        tantivy::Index::create(directory, schema.clone(), tantivy::IndexSettings::default())
            .unwrap();
    let mut writer = index.writer(15_000_000).unwrap();
    let doc = tantivy::doc!(
        id_field => "stale",
        content_field => "stale",
        tags_field => "stale",
        source_field => "test",
        category_field => "cat",
        timestamp_field => "2024-01-01T00:00:00Z",
    );
    writer.add_document(doc).unwrap();
    writer.commit().unwrap();

    let checker = StoreSearchChecker::new(&brehon_root);
    let findings = checker.check_tantivy_consistency(&events).unwrap();
    assert!(!findings.is_empty());
    assert!(
        findings
            .iter()
            .any(|finding| finding.summary.contains("Tantivy index missing memory IDs"))
            || findings.iter().any(|finding| finding
                .summary
                .contains("Tantivy index has stale memory IDs"))
    );
}
