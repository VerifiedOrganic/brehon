use super::dispatch::*;
use super::extraction::*;
use super::parsing::*;
use super::types::*;
use super::ExtractMode;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::commands::TEST_ENV_LOCK as IMPORT_PLAN_TEST_LOCK;

#[path = "tests/extraction_output.rs"]
mod extraction_output;
#[path = "tests/import_regressions.rs"]
mod import_regressions;

fn sample_plan() -> &'static str {
    r#"# Sample Plan

**Project:** Example
**Stack:** Rust + Go
**Target:** Bare Metal Kubernetes

## Phase 0: Foundation
> Detailed design: Phase0.md

### Epic 0.1: Base
| ID | Task | Deps | Size | Gate | Status |
|---|---|---|---|---|---|
| 0.1.1 | Scaffold | — | S | cargo test passes | `READY` |
| 0.1.2 | Config Bus | 0.1.1 | M | concurrent test passes | `BLOCKED` |

### Phase 0 Gate
| ID | Task | Deps | Size | Gate | Status |
|---|---|---|---|---|---|
| 0.G | Phase 0 Integration Test | 0.1.2 | L | end-to-end gate passes | `BLOCKED` |

## Cross-Phase Dependency Summary
"#
}

fn prose_only_plan() -> &'static str {
    r#"# Spindle continuity plan

This is a prose-heavy plan without `## Phase` headings in the table format.

## Status

- Current phase: continuity cleanup
- Owner: unassigned
"#
}

fn chunkable_phase_plan() -> &'static str {
    r#"# Spindle continuity plan

## Status

- **Current phase:** Phase 1 in progress (`4.1` shipped)

## 4. Phase 1 — Quick wins

### 4.1 First task

Do the first thing.

### 4.2 Second task

Do the second thing after the first.

### 4.G Phase 1 gate

Verify Phase 1.

## 5. Phase 2 — Follow-up

### 5.1 Third task

Do the third thing.
"#
}

fn normalized_plan_json() -> String {
    serde_json::json!({
        "title": "Normalized Plan",
        "project": "Example",
        "phases": [
            {
                "id": "1",
                "title": "Phase 1",
                "notes": ["LLM extracted"],
                "epics": [
                    {
                        "source_id": "1.x",
                        "title": "Phase 1 work items",
                        "tasks": [
                            {
                                "source_id": "1.1",
                                "title": "First task",
                                "dependencies": [],
                                "size": "M",
                                "gate": "unit tests pass",
                                "source_status": "READY"
                            },
                            {
                                "source_id": "1.2",
                                "title": "Second task",
                                "dependencies": ["1.1"],
                                "size": "L",
                                "gate": "integration tests pass",
                                "source_status": "BLOCKED"
                            }
                        ]
                    }
                ],
                "gate_task": {
                    "source_id": "1.G",
                    "title": "Phase 1 gate",
                    "dependencies": ["1.2"],
                    "size": "L",
                    "gate": "phase validation passes",
                    "source_status": "BLOCKED"
                }
            }
        ]
    })
    .to_string()
}

fn init_git_repo(root: &Path) -> Result<()> {
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(root)
        .output()
        .context("git init failed")?;
    fs::write(root.join(".gitignore"), ".brehon/\n")?;
    fs::write(root.join("README.md"), "seed")?;
    std::process::Command::new("git")
        .args(["add", "README.md", ".gitignore"])
        .current_dir(root)
        .output()
        .context("git add failed")?;
    let mut commit = std::process::Command::new("git");
    commit
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(root);
    commit.output().context("git commit failed")?;
    Ok(())
}

#[test]
fn parse_plan_document() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let plan_path = dir.path().join("plan.md");
    fs::write(&plan_path, sample_plan()).unwrap();

    let plan = parse_document(&plan_path).unwrap();
    assert_eq!(plan.title, "Sample Plan");
    assert_eq!(plan.phases.len(), 1);
    assert_eq!(plan.phases[0].epics.len(), 1);
    assert_eq!(plan.phases[0].epics[0].tasks.len(), 2);
    assert_eq!(plan.phases[0].gate_task.as_ref().unwrap().source_id, "0.G");
}

#[test]
fn parse_normalized_plan_document() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let plan_path = dir.path().join("plan.json");
    fs::write(&plan_path, normalized_plan_json()).unwrap();

    let plan = parse_normalized_plan(&plan_path).unwrap();
    assert_eq!(plan.title, "Normalized Plan");
    assert_eq!(plan.phases.len(), 1);
    assert_eq!(plan.phases[0].epics[0].tasks.len(), 2);
    assert_eq!(plan.phases[0].gate_task.as_ref().unwrap().source_id, "1.G");
    let task = &plan.phases[0].epics[0].tasks[0];
    assert_eq!(task.details_doc, None);
    assert!(task.required_reading.is_empty());
    assert!(task.context_refs.is_empty());
}

#[test]
fn parse_normalized_plan_preserves_task_packet_fields() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let plan_path = dir.path().join("plan.json");
    let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    value["phases"][0]["epics"][0]["tasks"][0]["details_doc"] =
        json!("docs/task-packets/1.1-first-task.md");
    value["phases"][0]["epics"][0]["tasks"][0]["required_reading"] = json!([
        "crates/brehon-cli/src/commands/import_plan/types.rs",
        "crates/brehon-cli/src/commands/import_plan/parsing.rs"
    ]);
    value["phases"][0]["epics"][0]["tasks"][0]["context_refs"] =
        json!(["docs/task-packets/README.md#tasklist-convention"]);
    fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();

    let plan = parse_normalized_plan(&plan_path).unwrap();
    let task = &plan.phases[0].epics[0].tasks[0];
    assert_eq!(
        task.details_doc.as_deref(),
        Some("docs/task-packets/1.1-first-task.md")
    );
    assert_eq!(
        task.required_reading,
        vec![
            "crates/brehon-cli/src/commands/import_plan/types.rs",
            "crates/brehon-cli/src/commands/import_plan/parsing.rs"
        ]
    );
    assert_eq!(
        task.context_refs,
        vec!["docs/task-packets/README.md#tasklist-convention"]
    );
}

#[test]
fn parse_chunkable_phase_sections_detects_prose_phase_plan() {
    let chunked = parse_chunkable_plan_document(Path::new("spindle.md"), chunkable_phase_plan())
        .unwrap()
        .unwrap();
    assert_eq!(chunked.title, "Spindle continuity plan");
    assert_eq!(chunked.phases.len(), 2);
    assert_eq!(chunked.phases[0].id, "1");
    assert_eq!(chunked.phases[0].title, "Quick wins");
    assert!(chunked.phases[0].body.contains("### 4.1 First task"));
    assert_eq!(chunked.phases[1].id, "2");
    assert_eq!(chunked.phases[1].title, "Follow-up");
}

#[test]
fn parse_chunkable_phase_heading_accepts_colon_then_em_dash_title() {
    let heading = "## Phase 3: Core Control Plane — AMF & NRF Rebuild";
    let (id, title) = parse_chunkable_phase_heading(heading).unwrap();
    assert_eq!(id, "3");
    assert_eq!(title, "Core Control Plane — AMF & NRF Rebuild");
}

#[test]
fn parse_task_extraction_sections_detects_task_headings() {
    let chunked = parse_chunkable_plan_document(Path::new("spindle.md"), chunkable_phase_plan())
        .unwrap()
        .unwrap();
    let tasks = parse_task_extraction_sections(&chunked.phases[0]);
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0].source_id, "4.1");
    assert_eq!(tasks[0].title, "First task");
    assert_eq!(tasks[2].source_id, "4.G");
}

#[test]
fn extracted_metadata_matches_ignores_inline_code_formatting() {
    assert!(extracted_metadata_matches(
        "`update_writer_position`",
        "update_writer_position"
    ));
    assert!(extracted_metadata_matches(
        "Phase 1 — `save_scene_draft` performance fix",
        "Phase 1 — save_scene_draft performance fix"
    ));
    assert!(!extracted_metadata_matches(
        "`update_writer_position`",
        "update_writer_state"
    ));
    assert!(extracted_metadata_matches(
        "Renderer (TDD)",
        "Phase 4: Renderer (TDD)"
    ));
    assert!(extracted_metadata_matches(
        "Validation",
        "Phase 7 — Validation"
    ));
}

#[test]
fn extracted_phase_id_matches_accepts_phase_slug_aliases() {
    assert!(extracted_phase_id_matches("0", "phase-0"));
    assert!(extracted_phase_id_matches("10", "Phase 10"));
    assert!(extracted_phase_id_matches("4", "phase_4"));
    assert!(!extracted_phase_id_matches("4", "phase-5"));
}

#[test]
fn extraction_schemas_and_prompts_allow_task_packet_fields() {
    let schema = extraction_schema();
    let task_properties = &schema["properties"]["phases"]["items"]["properties"]["epics"]["items"]
        ["properties"]["tasks"]["items"]["properties"];
    assert!(!task_properties["details_doc"].is_null());
    assert!(!task_properties["required_reading"].is_null());
    assert!(!task_properties["context_refs"].is_null());

    let phase_schema = phase_extraction_schema();
    let phase_task_properties =
        &phase_schema["properties"]["epics"]["items"]["properties"]["tasks"]["items"]["properties"];
    assert!(!phase_task_properties["details_doc"].is_null());
    assert!(!phase_task_properties["required_reading"].is_null());
    assert!(!phase_task_properties["context_refs"].is_null());

    let task_schema = task_extraction_schema();
    assert!(!task_schema["properties"]["details_doc"].is_null());
    assert!(!task_schema["properties"]["required_reading"].is_null());
    assert!(!task_schema["properties"]["context_refs"].is_null());

    let prompt = build_extraction_prompt(Path::new("plan.md"), "# Plan");
    assert!(prompt.contains("details_doc"));
    assert!(prompt.contains("required_reading"));
    assert!(prompt.contains("context_refs"));

    let phase = PhaseExtractionSection {
        id: "1".into(),
        title: "Phase 1".into(),
        heading: "## Phase 1: Phase 1".into(),
        body: "### 1.1 First task".into(),
    };
    let phase_prompt = build_phase_extraction_prompt(Path::new("plan.md"), "Plan", None, &phase);
    assert!(phase_prompt.contains("details_doc"));
    assert!(phase_prompt.contains("required_reading"));
    assert!(phase_prompt.contains("context_refs"));

    let task = TaskExtractionSection {
        source_id: "1.1".into(),
        title: "First task".into(),
        heading: "### 1.1 First task".into(),
        body: "Do the first task.".into(),
    };
    let task_prompt =
        build_task_extraction_prompt(Path::new("plan.md"), "Plan", None, &phase, &task, &[]);
    assert!(task_prompt.contains("details_doc"));
    assert!(task_prompt.contains("required_reading"));
    assert!(task_prompt.contains("context_refs"));
}

#[test]
fn parse_normalized_plan_rejects_unknown_dependency() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let plan_path = dir.path().join("plan.json");
    let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    value["phases"][0]["epics"][0]["tasks"][1]["dependencies"] = json!(["9.9"]);
    fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();

    let err = parse_normalized_plan(&plan_path).unwrap_err();
    assert!(format!("{err:#}").contains("depends on unknown source task"));
}

#[test]
fn parse_normalized_plan_rejects_invalid_task_packet_fields() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let cases = [
        (json!(""), "details_doc for task 1.1 must not be empty"),
        (
            json!("../task.md"),
            "details_doc for task 1.1 must not contain parent traversal",
        ),
        (
            json!("docs/task-packets/task.txt"),
            "details_doc for task 1.1 must be a .md file path",
        ),
    ];

    for (details_doc, expected) in cases {
        let dir = TempDir::new().unwrap();
        let plan_path = dir.path().join("plan.json");
        let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
        value["phases"][0]["epics"][0]["tasks"][0]["details_doc"] = details_doc;
        fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();

        let err = parse_normalized_plan(&plan_path).unwrap_err();
        assert!(
            format!("{err:#}").contains(expected),
            "error did not contain {expected:?}: {err:#}"
        );
    }
}

#[test]
fn parse_normalized_plan_accepts_absolute_task_packet_paths() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let plan_path = dir.path().join("plan.json");
    let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    let details_doc = dir.path().join("docs/task-packets/1.1-first-task.md");
    let required_reading = dir
        .path()
        .join("crates/brehon-cli/src/commands/import_plan/types.rs");
    let context_ref = dir.path().join("docs/task-packets/README.md");
    let details_doc = details_doc.display().to_string();
    let required_reading = required_reading.display().to_string();
    let context_ref = context_ref.display().to_string();
    value["phases"][0]["epics"][0]["tasks"][0]["details_doc"] = json!(details_doc);
    value["phases"][0]["epics"][0]["tasks"][0]["required_reading"] = json!([required_reading]);
    value["phases"][0]["epics"][0]["tasks"][0]["context_refs"] = json!([context_ref]);
    fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();

    let plan = parse_normalized_plan(&plan_path).unwrap();
    let task = &plan.phases[0].epics[0].tasks[0];
    assert_eq!(task.details_doc.as_deref(), Some(details_doc.as_str()));
    assert_eq!(task.required_reading, vec![required_reading]);
    assert_eq!(task.context_refs, vec![context_ref]);
}

#[test]
fn parse_normalized_plan_rejects_details_doc_list() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let plan_path = dir.path().join("plan.json");
    let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    value["phases"][0]["epics"][0]["tasks"][0]["details_doc"] =
        json!(["docs/task-packets/not-allowed.md"]);
    fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();

    let err = parse_normalized_plan(&plan_path).unwrap_err();
    assert!(format!("{err:#}").contains("Failed to parse normalized plan JSON"));
}

#[test]
fn parse_normalized_plan_rejects_execution_manifest_with_import_hint() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let manifest_path = dir.path().join("execution-order.manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string(&json!({
            "name": "execution manifest",
            "import_file": "initiative.import.json",
            "do_not_import": ["execution-order.manifest.json"],
            "import_order": [
                {
                    "order": 1,
                    "file": "initiative.import.json",
                    "command": "brehon import-plan initiative.import.json"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let err = parse_normalized_plan(&manifest_path).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("execution manifest"), "{message}");
    assert!(message.contains("initiative.import.json"), "{message}");
    assert!(
        message.contains("brehon import-plan initiative.import.json"),
        "{message}"
    );
}

#[test]
fn parse_normalized_plan_rejects_invalid_reference_list_entries() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let cases = [
        (
            "required_reading",
            json!([""]),
            "required_reading for task 1.1 must not be empty",
        ),
        (
            "required_reading",
            json!(["../source.rs"]),
            "required_reading for task 1.1 must not contain parent traversal",
        ),
        (
            "context_refs",
            json!([""]),
            "context_refs for task 1.1 must not be empty",
        ),
        (
            "context_refs",
            json!(["https://example.test/context.md"]),
            "context_refs for task 1.1 must be a filesystem path",
        ),
    ];

    for (field, field_value, expected) in cases {
        let dir = TempDir::new().unwrap();
        let plan_path = dir.path().join("plan.json");
        let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
        value["phases"][0]["epics"][0]["tasks"][0][field] = field_value;
        fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();

        let err = parse_normalized_plan(&plan_path).unwrap_err();
        assert!(
            format!("{err:#}").contains(expected),
            "error did not contain {expected:?}: {err:#}"
        );
    }
}

#[test]
fn resolve_extractor_launch_for_claude_supervisor() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".brehon")).unwrap();
    let mut config = brehon_config::parse_defaults().unwrap();
    config.roles.supervisor.name = "claude-code".to_string();
    config.launchers.insert(
        "claude-code".to_string(),
        brehon_types::AgentConnectionConfig {
            adapter: brehon_types::agent::AdapterKind::Acp,
            command: Some("claude".to_string()),
            args: vec![],
            provider: None,
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        },
    );
    fs::write(
        dir.path().join(".brehon/config.yaml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();

    let (kind, launch) = resolve_extractor_launch(dir.path()).unwrap();
    assert_eq!(kind, ExtractorKind::Claude);
    assert_eq!(launch.command, "claude");
    assert!(launch.args.contains(&"-p".to_string()));
    assert!(launch.args.contains(&"--tools".to_string()));
    assert!(launch.args.contains(&"--output-format".to_string()));
    assert!(launch.args.contains(&"json".to_string()));
    assert!(launch
        .args
        .contains(&"--no-session-persistence".to_string()));
    assert!(launch
        .args
        .contains(&"--disable-slash-commands".to_string()));
    assert!(launch.args.contains(&"--strict-mcp-config".to_string()));
}

#[tokio::test]
async fn prose_plan_falls_back_to_supervisor_extraction_via_stdin() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();

    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let capture_path = dir.path().join("extractor-stdin.txt");
    let fake_claude = bin_dir.join("claude");
    fs::write(
        &fake_claude,
        r#"#!/bin/sh
cat > "$FAKE_EXTRACT_STDIN_CAPTURE"
printf '%s' "$FAKE_EXTRACT_RESPONSE"
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_claude).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_claude, perms).unwrap();

    fs::create_dir_all(dir.path().join(".brehon")).unwrap();
    let mut config = brehon_config::parse_defaults().unwrap();
    config.roles.supervisor.name = "claude-code".to_string();
    config.launchers.insert(
        "claude-code".to_string(),
        brehon_types::AgentConnectionConfig {
            adapter: brehon_types::agent::AdapterKind::Acp,
            command: Some("claude".to_string()),
            args: vec![],
            provider: None,
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        },
    );
    fs::write(
        dir.path().join(".brehon/config.yaml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();

    let plan_path = dir.path().join("continuity.md");
    fs::write(&plan_path, prose_only_plan()).unwrap();

    let path = std::env::var("PATH").unwrap_or_default();
    let fake_response = normalized_plan_json();
    let _env = ScopedEnv::set(&[
        ("PATH", format!("{}:{}", bin_dir.display(), path)),
        (
            "FAKE_EXTRACT_STDIN_CAPTURE",
            capture_path.display().to_string(),
        ),
        ("FAKE_EXTRACT_RESPONSE", fake_response),
    ]);

    let plan = load_plan_document(dir.path(), &plan_path, ExtractMode::Auto)
        .await
        .unwrap();
    assert_eq!(plan.title, "Normalized Plan");
    assert_eq!(plan.phases.len(), 1);

    let captured = fs::read_to_string(&capture_path).unwrap();
    assert!(captured.contains("You are extracting a software implementation plan"));
    assert!(captured.contains("# Spindle continuity plan"));
}

#[tokio::test]
async fn supervisor_extraction_failure_reports_blank_stderr_clearly() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();

    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let fake_claude = bin_dir.join("claude");
    fs::write(
        &fake_claude,
        r#"#!/bin/sh
exit 9
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_claude).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_claude, perms).unwrap();

    fs::create_dir_all(dir.path().join(".brehon")).unwrap();
    let mut config = brehon_config::parse_defaults().unwrap();
    config.roles.supervisor.name = "claude-code".to_string();
    config.launchers.insert(
        "claude-code".to_string(),
        brehon_types::AgentConnectionConfig {
            adapter: brehon_types::agent::AdapterKind::Acp,
            command: Some("claude".to_string()),
            args: vec![],
            provider: None,
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        },
    );
    fs::write(
        dir.path().join(".brehon/config.yaml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();

    let plan_path = dir.path().join("continuity.md");
    fs::write(&plan_path, prose_only_plan()).unwrap();

    let path = std::env::var("PATH").unwrap_or_default();
    let _env = ScopedEnv::set(&[("PATH", format!("{}:{}", bin_dir.display(), path))]);

    let err = load_plan_document(dir.path(), &plan_path, ExtractMode::Supervisor)
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("claude-code"));
    assert!(text.contains("status exit status: 9"));
    assert!(text.contains("produced no stdout/stderr"));
}

#[tokio::test]
async fn extract_plan_uses_direct_parser_for_structured_markdown() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();

    let plan_path = dir.path().join("plan.md");
    fs::write(&plan_path, sample_plan()).unwrap();
    let output_path = dir.path().join("plan.json");

    execute_extract(
        dir.path(),
        &plan_path,
        Some(&output_path),
        ExtractMode::Auto,
    )
    .await
    .unwrap();

    let extracted: Value =
        serde_json::from_str(&fs::read_to_string(&output_path).unwrap()).unwrap();
    assert_eq!(extracted["title"], "Sample Plan");
    assert_eq!(extracted["phases"][0]["id"], "0");
    assert_eq!(extracted["phases"][0]["title"], "Foundation");
    assert_eq!(extracted["phases"][0]["gate_task"]["source_id"], "0.G");
}

#[tokio::test]
async fn direct_mode_does_not_fall_back_to_supervisor_for_prose_plan() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();

    let plan_path = dir.path().join("continuity.md");
    fs::write(&plan_path, prose_only_plan()).unwrap();

    let err = load_plan_document(dir.path(), &plan_path, ExtractMode::Direct)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("No phase sections found"));
}

#[tokio::test]
async fn import_plan_creates_initiative_epic_and_dependency_blocked_tasks() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.md");
    fs::write(&plan_path, sample_plan()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.md"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "plan"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let mut entries = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    assert_eq!(entries.len(), 10);
    let tasks = entries
        .iter()
        .map(|entry| {
            serde_json::from_str::<Value>(&fs::read_to_string(entry.path()).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();

    assert!(tasks.iter().any(|task| task["task_type"] == "initiative"));
    assert!(tasks.iter().any(|task| task["task_type"] == "epic"));
    let phase_epic = tasks
        .iter()
        .find(|task| task["plan_import"]["kind"] == "phase_epic")
        .unwrap();
    let hardening_epic = tasks
        .iter()
        .find(|task| task["plan_import"]["kind"] == "final_hardening_epic")
        .unwrap();
    assert_eq!(
        hardening_epic["title"],
        "Final Hardening and Cross-Epic Cleanup"
    );
    assert_eq!(hardening_epic["status"], "blocked");
    assert_eq!(hardening_epic["dependencies"][0], phase_epic["task_id"]);
    assert_eq!(hardening_epic["blocked_by"][0], phase_epic["task_id"]);
    let mut hardening_tasks = tasks
        .iter()
        .filter(|task| task["plan_import"]["kind"] == "final_hardening_task")
        .collect::<Vec<_>>();
    hardening_tasks.sort_by_key(|task| task["plan_import"]["sequence"].as_u64().unwrap());
    assert_eq!(hardening_tasks.len(), 3);
    assert!(hardening_tasks
        .iter()
        .all(|task| task["parent_id"] == hardening_epic["task_id"]));
    assert_eq!(hardening_tasks[0]["completion_mode"], "close");
    assert_eq!(hardening_tasks[1]["completion_mode"], "merge");
    assert!(hardening_tasks[1]["dependencies"]
        .as_array()
        .unwrap()
        .contains(&hardening_tasks[0]["task_id"]));
    let scaffold = tasks
        .iter()
        .find(|task| task["plan_import"]["source_task_id"] == "0.1.1")
        .unwrap();
    assert_eq!(scaffold["status"], "pending");

    let config_bus = tasks
        .iter()
        .find(|task| task["plan_import"]["source_task_id"] == "0.1.2")
        .unwrap();
    assert_eq!(config_bus["status"], "blocked");
    assert_eq!(config_bus["dependencies"][0], scaffold["task_id"]);
    assert_eq!(config_bus["blocked_by"][0], scaffold["task_id"]);

    let phase_gate = tasks
        .iter()
        .find(|task| task["plan_import"]["source_task_id"] == "0.G")
        .unwrap();
    assert_eq!(phase_gate["status"], "blocked");
    assert_eq!(phase_gate["dependencies"][0], config_bus["task_id"]);
    assert_eq!(phase_gate["blocked_by"][0], config_bus["task_id"]);
    assert_eq!(phase_gate["plan_import"]["is_phase_gate"], true);
    assert_eq!(phase_gate["completion_mode"], "merge");

    let seam_steward = tasks
        .iter()
        .find(|task| task["plan_import"]["source_task_id"] == "brehon.phase.0.seam-steward")
        .unwrap();
    assert_eq!(
        seam_steward["plan_import"]["source_epic_id"],
        "brehon.integration"
    );
    assert_eq!(seam_steward["completion_mode"], "merge");
    let seam_dependencies = seam_steward["dependencies"].as_array().unwrap();
    assert!(seam_dependencies.contains(&scaffold["task_id"]));
    assert!(seam_dependencies.contains(&config_bus["task_id"]));
    assert!(seam_dependencies.contains(&phase_gate["task_id"]));
    assert_eq!(seam_steward["status"], "blocked");
}

#[tokio::test]
async fn import_normalized_plan_json_creates_records() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    fs::write(&plan_path, normalized_plan_json()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let entries = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count();
    assert_eq!(entries, 10);
}

#[tokio::test]
async fn import_normalized_plan_persists_task_packet_fields() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    value["phases"][0]["epics"][0]["tasks"][0]["details_doc"] =
        json!("docs/task-packets/1.1-first-task.md");
    value["phases"][0]["epics"][0]["tasks"][0]["required_reading"] = json!([
        "crates/brehon-cli/src/commands/import_plan/types.rs",
        "crates/brehon-cli/src/commands/import_plan/dispatch.rs"
    ]);
    value["phases"][0]["epics"][0]["tasks"][0]["context_refs"] =
        json!(["docs/task-packets/README.md#tasklist-convention"]);
    fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan with task packet fields",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let tasks = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| {
            serde_json::from_str::<Value>(&fs::read_to_string(entry.path()).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();

    let imported = tasks
        .iter()
        .find(|task| task["plan_import"]["source_task_id"] == "1.1")
        .unwrap();
    assert_eq!(
        imported["plan_import"]["details_doc"],
        "docs/task-packets/1.1-first-task.md"
    );
    assert_eq!(
        imported["plan_import"]["required_reading"],
        json!([
            "crates/brehon-cli/src/commands/import_plan/types.rs",
            "crates/brehon-cli/src/commands/import_plan/dispatch.rs"
        ])
    );
    assert_eq!(
        imported["plan_import"]["context_refs"],
        json!(["docs/task-packets/README.md#tasklist-convention"])
    );

    let file_hints = imported["file_hints"].as_array().unwrap();
    assert!(file_hints
        .iter()
        .any(|hint| hint == "Task details packet: docs/task-packets/1.1-first-task.md"));
    assert!(file_hints.iter().any(
        |hint| hint == "Required reading: crates/brehon-cli/src/commands/import_plan/types.rs"
    ));
    assert!(imported["implementation_notes"]
        .as_str()
        .unwrap()
        .contains("Task details packet: docs/task-packets/1.1-first-task.md."));
}

#[tokio::test]
async fn import_normalized_plan_with_done_task_seeds_terminal_state() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    value["phases"][0]["epics"][0]["tasks"][0]["source_status"] = json!("DONE");
    value["phases"][0]["epics"][0]["tasks"][1]["dependencies"] = json!(["1.1"]);
    fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan with done task",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let tasks = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| {
            serde_json::from_str::<Value>(&fs::read_to_string(entry.path()).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();

    let done_task = tasks
        .iter()
        .find(|task| task["plan_import"]["source_task_id"] == "1.1")
        .unwrap();
    assert_eq!(done_task["status"], "closed");
    assert_eq!(done_task["terminal_status"], "closed");
    assert_eq!(done_task["closed_by"], "plan-importer");
    assert_eq!(done_task["integration_status"], "integrated");

    let dependent = tasks
        .iter()
        .find(|task| task["plan_import"]["source_task_id"] == "1.2")
        .unwrap();
    assert_eq!(dependent["dependencies"][0], done_task["task_id"]);
}

#[tokio::test]
async fn import_normalized_plan_allows_source_titles_that_mention_dot_brehon() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    value["phases"][0]["epics"][0]["title"] =
        json!("Workspace Cargo.toml & CI (exclude .brehon/ and scratch crates)");
    value["phases"][0]["epics"][0]["tasks"][0]["title"] = json!(
        "Workspace Cargo.toml & CI — Normalize workspace membership to repo-owned crates only (exclude .brehon/ and scratch crates)."
    );
    fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan with dot brehon title",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let tasks = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| {
            serde_json::from_str::<Value>(&fs::read_to_string(entry.path()).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();

    let imported = tasks
        .iter()
        .find(|task| task["plan_import"]["source_task_id"] == "1.1")
        .unwrap();
    assert_eq!(
        imported["title"],
        "Workspace Cargo.toml & CI — Normalize workspace membership to repo-owned crates only (exclude .brehon/ and scratch crates)."
    );
    assert_eq!(
        imported["file_hints"][1],
        "Search this repository for the relevant implementation area."
    );
    assert_eq!(
        imported["plan_import"]["source_epic_title"],
        "Workspace Cargo.toml & CI (exclude .brehon/ and scratch crates)"
    );
}

#[tokio::test]
async fn import_normalized_plan_always_assigns_phase_epic_branch() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    let mut value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    value["phases"][0]["id"] = json!("12");
    value["phases"][0]["title"] = json!("Phase 12: Exposure, Analytics, and Roaming Security");
    value["phases"][0]["notes"] = json!([
        "Implements the next shared-repo sequence after SMSF: NEF, NWDAF, and SEPP.",
        "NEF/NWDAF depend on both Phase 7 (SBI runtime) and Phase 10 gates; SEPP depends only on Phase 7 gate."
    ]);
    value["phases"][0]["epics"] = json!([
        {
            "source_id": "12.1",
            "title": "NEF & NWDAF",
            "tasks": [
                {
                    "source_id": "12.1.1",
                    "title": "NEF wiring",
                    "dependencies": [],
                    "size": "M",
                    "gate": "nef tests pass",
                    "source_status": "READY"
                }
            ]
        },
        {
            "source_id": "12.2",
            "title": "SEPP",
            "tasks": [
                {
                    "source_id": "12.2.1",
                    "title": "SEPP hardening",
                    "dependencies": [],
                    "size": "M",
                    "gate": "sepp tests pass",
                    "source_status": "READY"
                }
            ]
        }
    ]);
    value["phases"][0]["gate_task"] = json!({
        "source_id": "12.G",
        "title": "Phase 12 Integration Gate",
        "dependencies": ["12.1.1", "12.2.1"],
        "size": "L",
        "gate": "Exposure, analytics, and roaming-security gates all pass with compliance-scoped evidence.",
        "source_status": "BLOCKED"
    });
    fs::write(&plan_path, serde_json::to_string(&value).unwrap()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan with phase epic branch case",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let tasks = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| {
            serde_json::from_str::<Value>(&fs::read_to_string(entry.path()).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();

    let initiative = tasks
        .iter()
        .find(|task| task["task_type"] == "initiative")
        .unwrap();
    let initiative_suffix = initiative["task_id"]
        .as_str()
        .unwrap()
        .trim_start_matches("T-")
        .to_ascii_lowercase();
    let phase_epic = tasks
        .iter()
        .find(|task| task["plan_import"]["kind"] == "phase_epic")
        .unwrap();
    let branch = phase_epic["integration_branch"].as_str().unwrap();
    assert!(branch.starts_with("epic/phase-12-"));
    assert!(branch.ends_with(&initiative_suffix));
}

fn git_branches(cwd: &std::path::Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(cwd)
        .output()
        .unwrap();
    let mut branches: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();
    branches.sort();
    branches
}

fn git_worktrees(cwd: &std::path::Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
        .unwrap();
    let mut worktrees: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();
    worktrees.sort();
    worktrees
}

#[tokio::test]
async fn import_plan_honors_configured_worktree_root_for_container_worktrees() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    fs::create_dir_all(dir.path().join(".brehon")).unwrap();
    fs::write(
        dir.path().join(".brehon/config.yaml"),
        format!(
            "version: 1\norchestration:\n  worktree_root: {}\n",
            external.path().display()
        ),
    )
    .unwrap();
    let plan_path = dir.path().join("plan.md");
    fs::write(&plan_path, sample_plan()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.md"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "add plan"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path())
        .output()
        .unwrap();

    execute(dir.path(), &plan_path, false, ExtractMode::Direct)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon/runtime/tasks");
    let mut container_worktrees = Vec::new();
    for entry in fs::read_dir(tasks_dir).unwrap().flatten() {
        let content = fs::read_to_string(entry.path()).unwrap();
        let task: Value = serde_json::from_str(&content).unwrap();
        if let Some(worktree) = task.get("integration_worktree").and_then(|v| v.as_str()) {
            container_worktrees.push(worktree.to_string());
        }
    }

    assert!(
        !container_worktrees.is_empty(),
        "import should create initiative/epic worktrees"
    );
    assert!(
        container_worktrees
            .iter()
            .all(|path| Path::new(path).starts_with(external.path())),
        "all container worktrees should be under {}: {:?}",
        external.path().display(),
        container_worktrees
    );
}

#[tokio::test]
async fn import_normalized_plan_can_be_rerun_without_epic_branch_conflicts() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    fs::write(&plan_path, normalized_plan_json()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let initial_task_count = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count();
    let initial_branches = git_branches(dir.path());
    let initial_worktrees = git_worktrees(dir.path());

    let err = execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("already imported"),
        "expected duplicate import refusal, got: {message}"
    );
    assert!(
        message.contains("residual follow-up plan is required"),
        "expected residual follow-up guidance, got: {message}"
    );

    let final_task_count = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count();
    assert_eq!(
        final_task_count, initial_task_count,
        "duplicate import must not create additional tasks"
    );
    assert_eq!(
        git_branches(dir.path()),
        initial_branches,
        "duplicate import must not create branches"
    );
    assert_eq!(
        git_worktrees(dir.path()),
        initial_worktrees,
        "duplicate import must not create worktrees"
    );
}

#[tokio::test]
async fn import_plan_fails_from_different_relative_path_spelling() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    fs::write(&plan_path, normalized_plan_json()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    // First import via absolute path
    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    // Second import via differently-spelled absolute path should still be detected as duplicate
    let alternate_spelling = dir.path().join("./plan.json");
    let err = execute(dir.path(), &alternate_spelling, false, ExtractMode::Auto)
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("already imported"),
        "expected duplicate import refusal for different path spelling, got: {message}"
    );
}

#[tokio::test]
async fn import_plan_fails_when_landed_commit_is_ancestor() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();

    // Create a second commit so we can tag the first one as 31f95d9
    fs::write(dir.path().join("second.md"), "second\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "second.md"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit = std::process::Command::new("git");
    commit
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "second"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit.output().unwrap();

    let first_commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD~1"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let first_commit = String::from_utf8_lossy(&first_commit.stdout)
        .trim()
        .to_string();
    assert!(!first_commit.is_empty());

    let plan_path = dir.path().join("plan.json");
    let mut plan_value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    plan_value["already_landed_commit"] = json!(first_commit);
    fs::write(&plan_path, serde_json::to_string(&plan_value).unwrap()).unwrap();

    let initial_branches = git_branches(dir.path());
    let initial_worktrees = git_worktrees(dir.path());

    let err = execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains(&first_commit),
        "expected landed commit ancestor refusal, got: {message}"
    );
    assert!(
        message.contains("residual follow-up plan is required"),
        "expected residual follow-up guidance, got: {message}"
    );

    // Must fail before creating any tasks, branches, or worktrees
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    assert!(
        !tasks_dir.exists() || fs::read_dir(&tasks_dir).unwrap().flatten().next().is_none(),
        "import must fail before creating tasks"
    );
    assert_eq!(
        git_branches(dir.path()),
        initial_branches,
        "import must fail before creating branches"
    );
    assert_eq!(
        git_worktrees(dir.path()),
        initial_worktrees,
        "import must fail before creating worktrees"
    );
}

#[tokio::test]
async fn import_plan_succeeds_when_landed_commit_is_not_ancestor() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();

    // Create a second commit on main so we can branch from the first.
    fs::write(dir.path().join("second.md"), "second\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "second.md"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit = std::process::Command::new("git");
    commit
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "second"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit.output().unwrap();

    // Create a side branch from the first commit and add a commit to it.
    // This side-branch commit is NOT an ancestor of main's HEAD.
    std::process::Command::new("git")
        .args(["checkout", "-b", "side", "HEAD~1"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    fs::write(dir.path().join("side.md"), "side\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "side.md"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut side_commit = std::process::Command::new("git");
    side_commit
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "side"])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    side_commit.output().unwrap();
    let side_hash = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();

    // Back to main for the import.
    std::process::Command::new("git")
        .args(["checkout", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let plan_path = dir.path().join("plan.json");
    let mut plan_value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    plan_value["already_landed_commit"] = json!(side_hash);
    fs::write(&plan_path, serde_json::to_string(&plan_value).unwrap()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan with landed commit",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    // Should succeed because the side-branch commit is not an ancestor of main's HEAD.
    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    let entries = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count();
    assert_eq!(
        entries, 10,
        "plan with non-ancestor landed_commit should import normally"
    );
}

#[tokio::test]
async fn import_plan_fails_for_invalid_landed_commit_ref() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();

    let plan_path = dir.path().join("plan.json");
    let mut plan_value: Value = serde_json::from_str(&normalized_plan_json()).unwrap();
    plan_value["already_landed_commit"] = json!("deadbeef");
    fs::write(&plan_path, serde_json::to_string(&plan_value).unwrap()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan with invalid landed commit",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    let err = execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("deadbeef"),
        "expected deadbeef error in message, got: {message}"
    );

    // Must fail before creating any tasks
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    assert!(
        !tasks_dir.exists() || fs::read_dir(&tasks_dir).unwrap().flatten().next().is_none(),
        "import must fail before creating tasks"
    );
}

#[tokio::test]
async fn import_plan_fails_from_subdirectory_path() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();
    let plan_path = dir.path().join("plan.json");
    fs::write(&plan_path, normalized_plan_json()).unwrap();
    std::process::Command::new("git")
        .args(["add", "plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    // First import from a subdirectory via ../plan.json.
    // We construct the path explicitly rather than mutating process-global CWD
    // so that unrelated concurrent tests are never exposed to a temporary CWD.
    let subdir = dir.path().join("subdir");
    fs::create_dir_all(&subdir).unwrap();
    let subdir_plan_path = subdir.join("..").join("plan.json");
    execute(dir.path(), &subdir_plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    // Second import from repo root via plan.json should be detected as duplicate
    let err = execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("already imported"),
        "expected duplicate import refusal for subdirectory path, got: {message}"
    );
}

#[tokio::test]
async fn import_plan_succeeds_for_ambiguous_legacy_external_path() {
    let _lock = IMPORT_PLAN_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    init_git_repo(dir.path()).unwrap();

    // Create the in-repo plan at outside/plan.json
    let outside_dir = dir.path().join("outside");
    fs::create_dir_all(&outside_dir).unwrap();
    let plan_path = outside_dir.join("plan.json");
    fs::write(&plan_path, normalized_plan_json()).unwrap();
    std::process::Command::new("git")
        .args(["add", "outside/plan.json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let mut commit_plan = std::process::Command::new("git");
    commit_plan
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "normalized plan",
        ])
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir.path());
    commit_plan.output().unwrap();

    // Seed a legacy task file with a genuinely external-looking relative
    // path. Because the basename (plan-a.json) differs from the plan we are
    // about to import (plan.json), Detection A should skip the unrelated
    // entry and let the import proceed.
    let tasks_dir = dir.path().join(".brehon").join("runtime").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("T-external.json"),
        r#"{"task_id":"T-external","plan_import":{"source_file":"../outside/plan-a.json"}}"#,
    )
    .unwrap();

    let initial_branches = git_branches(dir.path());
    let initial_worktrees = git_worktrees(dir.path());

    // Import of the in-repo file should succeed — the ambiguous legacy
    // path is skipped by Detection A, and there is no already_landed_commit
    // for Detection B to catch.
    execute(dir.path(), &plan_path, false, ExtractMode::Auto)
        .await
        .unwrap();

    // Should have created the full set of imported tasks (9 total: 1 legacy +
    // 8 new, or if the tool counts differently, at least more than 1).
    let final_task_count = fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .count();
    assert!(
        final_task_count > 1,
        "import should have created new tasks, but only found {final_task_count}"
    );
    assert!(
        git_branches(dir.path()).len() > initial_branches.len(),
        "import should have created new branches"
    );
    assert!(
        git_worktrees(dir.path()).len() > initial_worktrees.len(),
        "import should have created new worktrees"
    );
}
