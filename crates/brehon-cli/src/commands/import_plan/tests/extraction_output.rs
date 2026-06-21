use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::Value;
use tempfile::TempDir;

use super::*;

#[test]
fn parse_claude_json_output_extracts_structured_output() {
    let payload = serde_json::json!([
        {
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "name": "StructuredOutput",
                        "input": serde_json::from_str::<Value>(&normalized_plan_json()).unwrap()
                    }
                ]
            }
        },
        {
            "type": "result",
            "structured_output": serde_json::from_str::<Value>(&normalized_plan_json()).unwrap()
        }
    ]);

    let plan: PlanDocument = json_from_text_output(&payload.to_string()).unwrap();
    assert_eq!(plan.title, "Normalized Plan");
    assert_eq!(plan.phases.len(), 1);
}

#[test]
fn parse_claude_result_object_extracts_structured_output() {
    let payload = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "structured_output": serde_json::from_str::<Value>(&normalized_plan_json()).unwrap()
    });

    let plan: PlanDocument = json_from_text_output(&payload.to_string()).unwrap();
    assert_eq!(plan.title, "Normalized Plan");
    assert_eq!(plan.phases.len(), 1);
}

#[test]
fn parse_claude_result_object_extracts_result_text_json() {
    let payload = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "result": format!(
            "Here is the normalized plan:\n```json\n{}\n```",
            normalized_plan_json()
        )
    });

    let plan: PlanDocument = json_from_text_output(&payload.to_string()).unwrap();
    assert_eq!(plan.title, "Normalized Plan");
    assert_eq!(plan.phases.len(), 1);
}

#[test]
fn parse_text_output_uses_balanced_json_candidate() {
    let output = format!(
        "I checked {{the prose plan}} and produced this:\n```json\n{}\n```",
        normalized_plan_json()
    );

    let plan: PlanDocument = json_from_text_output(&output).unwrap();
    assert_eq!(plan.title, "Normalized Plan");
    assert_eq!(plan.phases.len(), 1);
}

#[tokio::test]
async fn extract_plan_supervisor_result_wrapper_writes_normalized_json() {
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
cat >/dev/null
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
    let output_path = dir.path().join("normalized.json");

    let path = std::env::var("PATH").unwrap_or_default();
    let fake_response = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "result": format!("```json\n{}\n```", normalized_plan_json())
    })
    .to_string();
    let _env = ScopedEnv::set(&[
        ("PATH", format!("{}:{}", bin_dir.display(), path)),
        ("FAKE_EXTRACT_RESPONSE", fake_response),
    ]);

    execute_extract(
        dir.path(),
        &plan_path,
        Some(&output_path),
        ExtractMode::Supervisor,
    )
    .await
    .unwrap();

    let extracted: Value =
        serde_json::from_str(&fs::read_to_string(&output_path).unwrap()).unwrap();
    assert_eq!(extracted["title"], "Normalized Plan");
    assert_eq!(extracted["phases"][0]["id"], "1");
    assert_eq!(
        extracted["phases"][0]["epics"][0]["tasks"][0]["source_id"],
        "1.1"
    );
}
