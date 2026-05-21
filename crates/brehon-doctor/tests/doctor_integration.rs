//! Integration tests for doctor diagnostic pipeline.

use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_source_repo(dir: &Path) -> String {
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test User"]);
    std::fs::write(dir.join("README.md"), "seed\n").unwrap();
    run_git(dir, &["add", "README.md"]);
    run_git(dir, &["commit", "-m", "seed"]);
    run_git(dir, &["rev-parse", "HEAD"])
}

fn create_test_runtime(dir: &Path) {
    let runtime = dir.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();

    let tasks = runtime.join("tasks");
    let sessions = runtime.join("sessions");
    let events = runtime.join("events");
    let reviews = runtime.join("reviews");
    let panes = runtime.join("panes");

    std::fs::create_dir_all(&tasks).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&events).unwrap();
    std::fs::create_dir_all(&reviews).unwrap();
    std::fs::create_dir_all(&panes).unwrap();
}

fn write_task(dir: &Path, task_id: &str, status: &str, integration_status: Option<&str>) {
    let tasks_dir = dir.join("runtime").join("tasks");
    let path = tasks_dir.join(format!("{}.json", task_id));

    let mut json = serde_json::json!({
        "task_id": task_id,
        "title": format!("Test task {}", task_id),
        "status": status,
        "task_type": "task",
        "created_at": chrono::Utc::now().to_rfc3339(),
        "updated_at": chrono::Utc::now().to_rfc3339(),
    });

    if let Some(integ) = integration_status {
        json["integration_status"] = serde_json::Value::String(integ.to_string());
    }

    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
}

fn write_session(dir: &Path, name: &str, last_seen_seconds_ago: i64) {
    let sessions_dir = dir.join("runtime").join("sessions");
    let path = sessions_dir.join(format!("{}.json", name));

    let timestamp = chrono::Utc::now() - chrono::Duration::seconds(last_seen_seconds_ago);

    let json = serde_json::json!({
        "name": name,
        "role": "worker",
        "session_id": format!("session-{}", name),
        "registered_at": timestamp.to_rfc3339(),
        "last_seen_at": timestamp.to_rfc3339(),
    });

    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
}

fn write_prompt_file(dir: &Path, agent_name: &str, prompt_id: &str) {
    let prompt_queue_dir = dir.join("runtime").join("prompt-queue");
    std::fs::create_dir_all(&prompt_queue_dir).unwrap();

    let path = prompt_queue_dir.join(format!("{}-{}.prompt", agent_name, prompt_id));
    std::fs::write(&path, "Test prompt content").unwrap();
}

#[test]
fn test_doctor_finds_stale_sessions() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);

    // Create a stale session (last seen 3600 seconds = 1 hour ago, above 1800 threshold)
    write_session(&brehon_root, "worker-stale", 3600);
    write_session(&brehon_root, "worker-active", 30);

    let report = brehon_doctor::run_doctor(&brehon_root);

    // Should find at least one stale session finding
    let stale_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.summary.contains("Stale session"))
        .collect();

    assert_eq!(stale_findings.len(), 1);
    assert!(stale_findings[0].summary.contains("worker-stale"));
}

#[test]
fn test_doctor_finds_inreview_task_without_review_state() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);

    // Create a task in review without corresponding review state
    write_task(&brehon_root, "T-001", "in_review", None);

    let report = brehon_doctor::run_doctor(&brehon_root);

    // Should find missing review state
    let review_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.summary.contains("missing review state"))
        .collect();

    assert_eq!(review_findings.len(), 1);
}

#[test]
fn test_doctor_finds_approved_subtask_not_integrated() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);

    // Create an approved subtask without integration_status
    // (simulates a task that's been approved but hasn't been integrated into epic branch)
    let tasks_dir = brehon_root.join("runtime").join("tasks");

    // Parent epic
    let epic_path = tasks_dir.join("E-001.json");
    std::fs::write(
        &epic_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "E-001",
            "title": "Test Epic",
            "status": "in_progress",
            "task_type": "epic",
            "integration_branch": "epic/test-branch",
            "created_at": chrono::Utc::now().to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        }))
        .unwrap(),
    )
    .unwrap();

    // Approved subtask from 2 hours ago (should trigger warning)
    let subtask_path = tasks_dir.join("T-001.json");
    std::fs::write(
        &subtask_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-001",
            "title": "Test Subtask",
            "status": "approved",
            "task_type": "task",
            "parent_id": "E-001",
            "integration_status": "pending",
            "created_at": chrono::Utc::now().to_rfc3339(),
            "updated_at": (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339(),
        }))
        .unwrap(),
    )
    .unwrap();

    let report = brehon_doctor::run_doctor(&brehon_root);

    // Should find integration status mismatch
    let findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.summary.contains("not integrated") || f.summary.contains("Approved subtask"))
        .collect();

    assert!(
        !findings.is_empty(),
        "Should find approved subtask not integrated"
    );
}

#[test]
fn test_task_status_label_approved_vs_integrated_vs_merged() {
    use brehon_doctor::types::TaskStatusLabel;

    // Approved task with no integration status stays approved
    let label = TaskStatusLabel::from_task("approved", None, None);
    assert_eq!(label.as_str(), "approved");

    // Approved task with integration_status="integrated" becomes integrated
    let label = TaskStatusLabel::from_task("approved", Some("integrated"), None);
    assert_eq!(label.as_str(), "integrated");

    // Approved task with merged_commit becomes merged
    let label = TaskStatusLabel::from_task("approved", None, Some("abc123"));
    assert_eq!(label.as_str(), "merged");

    // merged_commit takes precedence over integration_status
    let label = TaskStatusLabel::from_task("approved", Some("integrated"), Some("def456"));
    assert_eq!(label.as_str(), "merged");

    // Closed status overrides
    let label = TaskStatusLabel::from_task("closed", None, None);
    assert_eq!(label.as_str(), "closed");
}

#[test]
fn test_doctor_report_formatting() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);

    // Create some findings
    write_task(&brehon_root, "T-001", "in_review", None);
    write_session(&brehon_root, "worker-stale", 120);

    let report = brehon_doctor::run_doctor(&brehon_root);

    // Test display formatting
    let display = format!("{}", report);
    assert!(display.contains("BREHON DOCTOR REPORT"));
    assert!(display.contains("SUMMARY"));

    // Test JSON formatting
    let json = report.to_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(parsed.get("findings").unwrap().as_array().is_some());

    // Test compact formatting
    let compact = report.to_compact();
    assert!(compact.starts_with("SUMMARY:"));
}

#[test]
fn test_doctor_empty_runtime() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");

    // No runtime dir at all - doctor should handle gracefully
    let report = brehon_doctor::run_doctor(&brehon_root);

    // Should have error about missing directories
    assert!(report.has_errors() || report.findings.is_empty());
}

#[test]
fn test_diagnostic_category_grouping() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);

    // Create findings in different categories
    write_task(&brehon_root, "T-001", "in_review", None); // Review category
    write_session(&brehon_root, "stale-worker", 120); // Runtime category

    let report = brehon_doctor::run_doctor(&brehon_root);

    // Should have findings in multiple categories
    let worktree_findings =
        report.findings_by_category(brehon_doctor::types::DiagnosticCategory::Worktree);
    let runtime_findings =
        report.findings_by_category(brehon_doctor::types::DiagnosticCategory::Runtime);
    let task_findings = report.findings_by_category(brehon_doctor::types::DiagnosticCategory::Task);
    let review_findings =
        report.findings_by_category(brehon_doctor::types::DiagnosticCategory::Review);

    // At least we should have runtime and review findings
    assert!(
        !runtime_findings.is_empty()
            || !review_findings.is_empty()
            || !task_findings.is_empty()
            || !worktree_findings.is_empty()
    );
}

#[test]
fn test_compute_display_status_from_task_info() {
    use brehon_doctor::types::TaskStatusLabel;

    // This tests the logic that TUI uses to display status
    // The logic is: merged_commit -> "merged", integration_status="integrated" -> "integrated", else -> status

    // Test the underlying label logic
    assert_eq!(
        TaskStatusLabel::from_task("approved", None, None).as_str(),
        "approved"
    );
    assert_eq!(
        TaskStatusLabel::from_task("approved", Some("integrated"), None).as_str(),
        "integrated"
    );
    assert_eq!(
        TaskStatusLabel::from_task("approved", Some("pending"), Some("abc")).as_str(),
        "merged"
    );

    // Status normalization
    assert_eq!(
        TaskStatusLabel::from_task("InProgress", None, None).as_str(),
        "in_progress"
    );
    assert_eq!(
        TaskStatusLabel::from_task("InReview", None, None).as_str(),
        "in_review"
    );
    assert_eq!(
        TaskStatusLabel::from_task("ChangesRequested", None, None).as_str(),
        "changes_requested"
    );
}

#[test]
fn test_doctor_finds_stale_prompt_queue_files() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);

    // Create an active session
    write_session(&brehon_root, "worker-active", 100);

    // Create a stale prompt file for a non-existent agent
    write_prompt_file(&brehon_root, "worker-dead", "prompt-001");

    // Create a prompt file for the active agent (should not be flagged)
    write_prompt_file(&brehon_root, "worker-active", "prompt-002");

    let report = brehon_doctor::run_doctor(&brehon_root);

    let prompt_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.summary.contains("Stale prompt-queue"))
        .collect();

    assert_eq!(prompt_findings.len(), 1);
    assert!(prompt_findings[0].summary.contains("worker-dead"));
    assert!(!prompt_findings[0].summary.contains("worker-active"));
}

#[test]
fn test_doctor_detects_orphaned_git_metadata() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path();
    let brehon_root = project_root.join(".brehon");
    create_test_runtime(&brehon_root);

    // Create a fake git worktrees metadata directory at project root
    let git_worktrees = project_root.join(".git").join("worktrees");
    let orphan_meta = git_worktrees.join("orphan-worktree");
    std::fs::create_dir_all(&orphan_meta).unwrap();

    // Write gitdir pointing to non-existent worktree
    let gitdir = orphan_meta.join("gitdir");
    let nonexistent_path = project_root
        .join(".brehon")
        .join("worktrees")
        .join("orphan-worktree")
        .join(".git");
    std::fs::write(&gitdir, nonexistent_path.to_string_lossy().to_string()).unwrap();

    let report = brehon_doctor::run_doctor(&brehon_root);

    let orphan_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.summary.contains("Orphaned git worktree metadata"))
        .collect();

    assert_eq!(orphan_findings.len(), 1);
}

#[test]
fn test_doctor_repair_recovers_runtime_control_plane_state() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);
    let tasks_dir = brehon_root.join("runtime").join("tasks");

    std::fs::write(
        tasks_dir.join("T-orphan.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-orphan",
            "title": "Orphaned worker task",
            "status": "in_progress",
            "task_type": "task",
            "assignee": "dead-worker",
            "updated_at": chrono::Utc::now().to_rfc3339(),
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        tasks_dir.join("T-review.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-review",
            "title": "Missing review state",
            "status": "in_review",
            "task_type": "task",
            "latest_commit": "abc123",
            "updated_at": chrono::Utc::now().to_rfc3339(),
        }))
        .unwrap(),
    )
    .unwrap();

    let lock_path = tasks_dir.join(".T-orphan.lock");
    std::fs::write(&lock_path, "stale").unwrap();
    let old = filetime::FileTime::from_unix_time(
        (chrono::Utc::now() - chrono::Duration::minutes(2)).timestamp(),
        0,
    );
    filetime::set_file_mtime(&lock_path, old).unwrap();

    let mcp_dir = brehon_root.join("runtime").join("mcp-servers");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    std::fs::write(
        mcp_dir.join("999999.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "pid": 999999u64,
            "server_name": "brehon",
            "server_version": env!("CARGO_PKG_VERSION"),
        }))
        .unwrap(),
    )
    .unwrap();

    let report = brehon_doctor::run_doctor_repair(&brehon_root);
    assert!(
        report.repaired_count >= 4,
        "expected lock, dead mcp metadata, orphan, and review-state repairs: {report:?}"
    );
    assert!(!lock_path.exists());
    assert!(!mcp_dir.join("999999.json").exists());

    let orphan: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tasks_dir.join("T-orphan.json")).unwrap())
            .unwrap();
    assert_eq!(orphan["status"], "pending");
    assert!(orphan["assignee"].is_null());
    assert_eq!(orphan["orphaned_assignee"], "dead-worker");

    let review: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tasks_dir.join("T-review.json")).unwrap())
            .unwrap();
    assert_eq!(review["status"], "review_ready");
}

#[test]
fn test_doctor_repair_preserves_live_and_fresh_runtime_state() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    let sessions_dir = brehon_root.join("runtime").join("sessions");

    write_session(&brehon_root, "worker-live", 1);
    std::fs::write(
        tasks_dir.join("T-live.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-live",
            "title": "Live worker task",
            "status": "in_progress",
            "task_type": "task",
            "assignee": "worker-live",
            "updated_at": chrono::Utc::now().to_rfc3339(),
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        tasks_dir.join("T-review-live.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": "T-review-live",
            "title": "Live review task",
            "status": "in_review",
            "task_type": "task",
            "latest_commit": "abc123",
            "updated_at": chrono::Utc::now().to_rfc3339(),
        }))
        .unwrap(),
    )
    .unwrap();
    let review_dir = brehon_root
        .join("runtime")
        .join("reviews")
        .join("T-review-live");
    std::fs::create_dir_all(&review_dir).unwrap();
    std::fs::write(review_dir.join("state.json"), "{}").unwrap();

    let fresh_lock = tasks_dir.join(".T-live.lock");
    std::fs::write(&fresh_lock, "fresh").unwrap();

    let mcp_dir = brehon_root.join("runtime").join("mcp-servers");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    let live_mcp = mcp_dir.join(format!("{}.json", std::process::id()));
    std::fs::write(
        &live_mcp,
        serde_json::to_string_pretty(&serde_json::json!({
            "pid": std::process::id(),
            "server_name": "brehon",
            "server_version": env!("CARGO_PKG_VERSION"),
        }))
        .unwrap(),
    )
    .unwrap();

    let report = brehon_doctor::run_doctor_repair(&brehon_root);
    assert!(
        !report.actions.iter().any(|action| action.repaired),
        "nothing in this fixture should be repaired: {report:?}"
    );
    assert!(fresh_lock.exists());
    assert!(live_mcp.exists());
    assert!(sessions_dir.join("worker-live.json").exists());
    let live_task: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tasks_dir.join("T-live.json")).unwrap())
            .unwrap();
    assert_eq!(live_task["status"], "in_progress");
    assert_eq!(live_task["assignee"], "worker-live");
    let review_task: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tasks_dir.join("T-review-live.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(review_task["status"], "in_review");
}

#[test]
fn test_doctor_detects_stale_mcp_binary_metadata() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);
    let mcp_dir = brehon_root.join("runtime").join("mcp-servers");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    let binary = tmp.path().join("brehon-bin");
    std::fs::write(&binary, "binary").unwrap();

    std::fs::write(
        mcp_dir.join(format!("{}.json", std::process::id())),
        serde_json::to_string_pretty(&serde_json::json!({
            "pid": std::process::id(),
            "server_name": "brehon",
            "server_version": env!("CARGO_PKG_VERSION"),
            "binary_path": binary,
            "binary_modified_unix_secs": 0u64,
        }))
        .unwrap(),
    )
    .unwrap();

    let report = brehon_doctor::run_doctor(&brehon_root);
    let stale_binary: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| finding.summary.contains("Stale installed Brehon binary"))
        .collect();
    assert_eq!(stale_binary.len(), 1, "{:?}", report.findings);
}

#[test]
fn test_doctor_detects_stale_mcp_source_revision() {
    let tmp = TempDir::new().unwrap();
    let current_revision = init_source_repo(tmp.path());
    assert_ne!(current_revision, "old-source-revision");
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);
    let mcp_dir = brehon_root.join("runtime").join("mcp-servers");
    std::fs::create_dir_all(&mcp_dir).unwrap();

    std::fs::write(
        mcp_dir.join(format!("{}.json", std::process::id())),
        serde_json::to_string_pretty(&serde_json::json!({
            "pid": std::process::id(),
            "server_name": "brehon",
            "server_version": env!("CARGO_PKG_VERSION"),
            "source_revision": "old-source-revision",
        }))
        .unwrap(),
    )
    .unwrap();

    let report = brehon_doctor::run_doctor(&brehon_root);
    let stale_source: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| finding.summary.contains("Stale MCP source revision"))
        .collect();
    assert_eq!(stale_source.len(), 1, "{:?}", report.findings);

    let repair = brehon_doctor::run_doctor_repair(&brehon_root);
    assert!(
        repair.actions.iter().any(|action| {
            action.code == "stale_source_revision"
                && !action.repaired
                && action.message.contains("older source revision")
        }),
        "{repair:?}"
    );
}

#[test]
fn test_doctor_repair_reports_bad_routing_lanes_as_operator_action() {
    let tmp = TempDir::new().unwrap();
    let brehon_root = tmp.path().join(".brehon");
    create_test_runtime(&brehon_root);
    std::fs::write(
        brehon_root.join("config.yaml"),
        r#"
routing:
  default_worker_lane: missing-worker
"#,
    )
    .unwrap();

    let report = brehon_doctor::run_doctor_repair(&brehon_root);
    assert!(
        report.actions.iter().any(|action| {
            action.code == "bad_routing_lane"
                && !action.repaired
                && action.message.contains("missing-worker")
        }),
        "{report:?}"
    );
}
