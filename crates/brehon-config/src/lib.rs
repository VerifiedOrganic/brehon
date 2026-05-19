//! Configuration loading, merging, and validation for Brehon.
//!
//! This crate handles:
//! - Loading configuration from multiple layers (defaults → global → project)
//! - Merging, validating, and creating project config files
//!
//! # Example
//!
//! ```rust,no_run
//! use brehon_config::{load_config, init_project, global_config_path};
//! use std::path::Path;
//!
//! // Load config with layering
//! let config = load_config(None).expect("Failed to load config");
//!
//! // Get global config path
//! if let Some(path) = global_config_path() {
//!     println!("Global config at: {:?}", path);
//! }
//!
//! // Initialize a new project
//! init_project(Path::new(".")).expect("Failed to init project");
//! ```

mod error;
mod merge;
#[cfg(test)]
mod retry_tests;
mod validate;

use std::env;
use std::path::{Path, PathBuf};

use brehon_types::BrehonConfig;
pub use error::ConfigError;
pub use merge::merge_configs;
use serde_yaml::Value;
pub use validate::{validate, ValidationWarning, ValidationWarningKind};

const DEFAULTS_YAML: &str = include_str!("defaults.yaml");

/// Load configuration with layering: defaults → global → project.
///
/// # Errors
///
/// Returns `ConfigError` if:
/// - A config file exists but cannot be parsed
/// - A config file fails validation
pub fn load_config(project_path: Option<&Path>) -> Result<BrehonConfig, ConfigError> {
    load_config_with_override(project_path, None)
}

/// Load configuration with layering: defaults → global → project/override.
///
/// If `config_override_path` is provided, that file is treated as the project
/// layer regardless of the `project_path`. Overlay files may contain only the
/// fields they need to change.
pub fn load_config_with_override(
    project_path: Option<&Path>,
    config_override_path: Option<&Path>,
) -> Result<BrehonConfig, ConfigError> {
    let mut merged_value = parse_defaults_value()?;

    if let Some(global) = load_global_config_value()? {
        merge_yaml_overlay(&mut merged_value, global);
    }

    let project_config = match config_override_path {
        Some(path) => load_config_file_value(path, "project config override")?,
        None => match project_path {
            Some(path) => load_project_config_value(path)?,
            None => None,
        },
    };
    if let Some(project) = project_config {
        merge_yaml_overlay(&mut merged_value, project);
    }

    let mut merged = deserialize_config_value(merged_value, "merged config")?;
    resolve_launcher_env_placeholders(&mut merged)?;

    let warnings = validate(&merged);
    for warning in &warnings {
        tracing::warn!("Config validation: {}", warning);
    }
    let fatal_warnings = fatal_validation_warnings(&warnings);
    if !fatal_warnings.is_empty() {
        return Err(ConfigError::Validation(fatal_warnings.join("; ")));
    }

    Ok(merged)
}

fn fatal_validation_warnings(warnings: &[ValidationWarning]) -> Vec<String> {
    warnings
        .iter()
        .filter(|warning| warning.is_fatal)
        .map(|warning| warning.message.clone())
        .collect()
}

fn resolve_launcher_env_placeholders(config: &mut BrehonConfig) -> Result<(), ConfigError> {
    for (launcher_name, launcher) in &mut config.launchers {
        for (env_key, env_value) in &mut launcher.env {
            *env_value = interpolate_env_value(env_value).map_err(|err| {
                ConfigError::Missing(format!("launchers.{launcher_name}.env.{env_key}: {err}"))
            })?;
        }
    }
    Ok(())
}

fn interpolate_env_value(raw: &str) -> Result<String, String> {
    if !raw.contains("${") {
        return Ok(raw.to_string());
    }

    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let expr_start = start + 2;
        let after_start = &rest[expr_start..];
        let Some(end) = after_start.find('}') else {
            return Err(format!("unclosed interpolation in '{raw}'"));
        };
        let expr = &after_start[..end];
        let resolved = if let Some((var, default)) = expr.split_once(":-") {
            let var = var.trim();
            if var.is_empty() {
                return Err(format!("empty env var name in '{raw}'"));
            }
            match env::var(var) {
                Ok(value) if !value.is_empty() => value,
                Ok(_) | Err(env::VarError::NotPresent) => default.to_string(),
                Err(env::VarError::NotUnicode(_)) => {
                    return Err(format!("environment variable {var} is not valid unicode"));
                }
            }
        } else {
            let var = expr.trim();
            if var.is_empty() {
                return Err(format!("empty env var name in '{raw}'"));
            }
            env::var(var).map_err(|error| match error {
                env::VarError::NotPresent => format!("missing environment variable {var}"),
                env::VarError::NotUnicode(_) => {
                    format!("environment variable {var} is not valid unicode")
                }
            })?
        };
        out.push_str(&resolved);
        rest = &after_start[end + 1..];
    }

    out.push_str(rest);
    Ok(out)
}

/// Get the global config path (`~/.config/brehon/config.yaml`).
///
/// Returns `None` if the home directory cannot be determined.
pub fn global_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "brehon")
        .map(|dirs| dirs.config_dir().join("config.yaml"))
}

/// Initialize a project by creating `.brehon/config.yaml` with commented defaults.
///
/// Creates the `.brehon` directory if it doesn't exist.
/// The generated config file contains helpful comments explaining each setting.
///
/// # Errors
///
/// Returns `ConfigError` if:
/// - The `.brehon` directory cannot be created
/// - The config file cannot be written
pub fn init_project(path: &Path) -> Result<(), ConfigError> {
    let brehon_dir = path.join(".brehon");
    let config_path = brehon_dir.join("config.yaml");

    if config_path.exists() {
        return Err(ConfigError::AlreadyInitialized);
    }

    std::fs::create_dir_all(&brehon_dir)
        .map_err(|e| ConfigError::Io(format!("Failed to create .brehon directory: {}", e)))?;

    let commented_config = generate_commented_defaults();

    std::fs::write(&config_path, commented_config)
        .map_err(|e| ConfigError::Io(format!("Failed to write config file: {}", e)))?;

    Ok(())
}

/// Parse the baked-in defaults without any validation or merging.
pub fn parse_defaults() -> Result<BrehonConfig, ConfigError> {
    deserialize_config_value(parse_defaults_value()?, "defaults")
}

fn parse_defaults_value() -> Result<Value, ConfigError> {
    serde_yaml::from_str(DEFAULTS_YAML)
        .map_err(|e| ConfigError::Parse(format!("Failed to parse defaults: {}", e)))
}

fn deserialize_config_value(value: Value, layer_name: &str) -> Result<BrehonConfig, ConfigError> {
    serde_yaml::from_value(value).map_err(|err| {
        ConfigError::Parse(format!(
            "Failed to deserialize {layer_name} into config: {err}"
        ))
    })
}

/// Load global config if it exists.
fn load_global_config_value() -> Result<Option<Value>, ConfigError> {
    match global_config_path() {
        Some(path) if path.exists() => {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| ConfigError::Io(format!("Failed to read global config: {}", e)))?;
            Ok(Some(parse_config_yaml_layer(&content, "global config")?))
        }
        _ => Ok(None),
    }
}

/// Load project config if it exists.
#[cfg(test)]
fn load_project_config(project_path: &Path) -> Result<Option<BrehonConfig>, ConfigError> {
    let Some(layer) = load_project_config_value(project_path)? else {
        return Ok(None);
    };
    let mut merged_value = parse_defaults_value()?;
    merge_yaml_overlay(&mut merged_value, layer);
    Ok(Some(deserialize_config_value(
        merged_value,
        "project config",
    )?))
}

fn load_project_config_value(project_path: &Path) -> Result<Option<Value>, ConfigError> {
    // Accept both forms: the project root (contains `.brehon/`) or the
    // `.brehon` directory itself. Silent fallback to defaults when callers
    // pass the wrong form was the root cause of a codex reviewer reset bug.
    let config_path = if project_path.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        project_path.join("config.yaml")
    } else {
        project_path.join(".brehon/config.yaml")
    };

    if !config_path.exists() {
        return Ok(None);
    }

    load_config_file_value(&config_path, "project config")
}

fn load_config_file_value(
    config_path: &Path,
    layer_name: &str,
) -> Result<Option<Value>, ConfigError> {
    if !config_path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(config_path)
        .map_err(|e| ConfigError::Io(format!("Failed to read project config: {}", e)))?;

    let config = parse_config_yaml_layer(&content, layer_name)?;

    Ok(Some(config))
}

fn parse_config_yaml_layer(content: &str, layer_name: &str) -> Result<Value, ConfigError> {
    let value: Value = serde_yaml::from_str(content)
        .map_err(|err| ConfigError::Parse(format!("Failed to parse {layer_name}: {err}")))?;

    validate_required_structure_layer(&value)?;

    Ok(value)
}

fn merge_yaml_overlay(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(base_value) => merge_yaml_overlay(base_value, overlay_value),
                    None => {
                        base_map.insert(key, overlay_value);
                    }
                }
            }
        }
        (base_value, overlay_value) => {
            *base_value = overlay_value;
        }
    }
}

fn validate_required_structure_layer(value: &Value) -> Result<(), ConfigError> {
    let mut violations = Vec::new();

    if let Some(lanes) = value.get("lanes") {
        if let Some(lanes) = lanes.as_mapping() {
            if lanes.is_empty() {
                violations.push("must not set 'lanes' to an empty map".to_string());
            }
        }
    }

    if let Some(roles) = value.get("roles") {
        if let Some(roles) = roles.as_mapping() {
            for (key, values) in roles {
                if let Some(key) = key.as_str() {
                    match key {
                        "workers" => {
                            if values
                                .as_sequence()
                                .map_or(false, |values| values.is_empty())
                            {
                                violations.push(
                                    "must not set 'roles.workers' to an empty list".to_string(),
                                );
                            }
                        }
                        "reviewers" => {
                            if values
                                .as_sequence()
                                .map_or(false, |values| values.is_empty())
                            {
                                violations.push(
                                    "must not set 'roles.reviewers' to an empty list".to_string(),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if violations.is_empty() {
        return Ok(());
    }

    Err(ConfigError::Validation(format!(
        "Config layer has invalid required structure: {}",
        violations.join(", ")
    )))
}

/// Generate a usable config file for project initialization.
fn generate_commented_defaults() -> String {
    r#"# Brehon Project Configuration
# Layers: built-in defaults < ~/.config/brehon/config.yaml < this file
#
# Keep this file small. Brehon now treats project config as a partial overlay,
# so you only need to write fields that differ from the defaults.
#
# Common edits:
#
# roles:
#   workers:
#     - lane: codex-worker
#       min: 2
#       max: 4
#   reviewers:
#     - lane: claude-reviewer
#       min: 1
#       max: 1
#     - lane: codex-reviewer
#       min: 1
#       max: 1
#
# review:
#   policy:
#     min_approvals: 2
#     min_average_score: 7
#
# orchestration:
#   max_active_workers: 4
#
# routing:
#   # Rename these lanes to match your roles.workers entries.
#   default_worker_lane: cheap-worker
#   escalation_lane: codex-worker
#   rules:
#     - id: high-risk-or-large
#       match:
#         text_any:
#           - "Imported size estimate: L"
#           - phase gate
#           - controller
#           - reconcile
#           - interop
#           - chaos
#           - release
#           - security
#       policy:
#         preferred_lane: codex-worker
#         strict: false
#
# advisors:
#   enabled: false
#   response_timeout_secs: 45
#   default_turn_mode: open_chat
#   pools:
#     - lane: codex-worker
#       min: 1
#       max: 2
#       permissions: read_only
#       rooms:
#         - release-war-room
#   rooms:
#     - id: release-war-room
#       title: Release War Room
#       turn_mode: debate
#       participants:
#         - codex-worker
#       context:
#         docs:
#           - docs/PHASE5_COMPLETION_HANDOFF.md
#         tasks:
#           - status: ready
#
# research:
#   enabled: false
#   worker_requests:
#     enabled: true
#     max_requests_per_task: 3
#     max_cost_units_per_task: 6
#   pools:
#     - id: spec-research
#       lane: cheap-worker
#       instruction_profile: "Cite primary sources and summarize only task-relevant facts."
#       role: normative_requirements
#       min: 0
#       max: 2
#       cost_units: 1
#       permissions: read_only
#   routes:
#     - id: specs-for-protocol-work
#       trigger: before_assignment
#       continue: true
#       match:
#         text_any: [RFC, protocol, 5G, PFCP, NGAP]
#       jobs:
#         - id: normative-requirements
#           pool: spec-research
#           prompt_template: |
#             Task {{task_id}}: {{title}}
#             Summarize relevant requirements and cite sources.
#
# permissions:
#   bash:
#     "cargo test *": Allow
#     "rm -rf *": Deny

version: 1
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_config_without_files() {
        let config = load_config(None).expect("Should load defaults");
        assert_eq!(config.version, 1);
        assert!(config.launchers.contains_key("claude"));
        assert!(config.lanes.contains_key("claude-supervisor"));
        assert!(config.runtime.enabled_workflows.is_empty());
        assert_eq!(
            config.runtime.terminal_host.effective_kind(),
            brehon_types::RuntimeTerminalHostKind::Embedded
        );
        assert!(!config.runtime.terminal_host.preview_pane_enabled());
        assert_eq!(
            config.runtime.terminal_host.effective_pane_ownership(),
            brehon_types::RuntimeTerminalHostPaneOwnership::Mux
        );
    }

    #[test]
    fn load_config_with_override_file_uses_exact_file() {
        let project_root = std::env::temp_dir().join(format!(
            "brehon-config-override-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        if project_root.exists() {
            std::fs::remove_dir_all(&project_root).expect("cleanup tempdir");
        }
        std::fs::create_dir_all(&project_root).expect("create tempdir");
        let brehon_dir = project_root.join(".brehon");
        std::fs::create_dir_all(&brehon_dir).expect("mkdir");

        std::fs::write(
            brehon_dir.join("config.yaml"),
            r#"
version: 1
launchers:
  claude:
    adapter: Acp
    command: claude
    args: []
  opencode:
    adapter: Acp
    command: opencode
    args: []
  gemini:
    adapter: Acp
    command: gemini
    args: [--acp]
lanes:
  claude-supervisor:
    launcher: claude
  opencode-worker:
    launcher: opencode
    model: { provider: ollama-cloud, name: glm-5.1 }
  codex-reviewer:
    launcher: codex
    model: { provider: openai, name: gpt-5.4 }
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: project
    permissions: [CreateTasks]
  workers:
    - lane: opencode-worker
      min: 1
      max: 1
  reviewers:
    - lane: codex-reviewer
      min: 1
      max: 1
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 2
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: [codex-reviewer]
  panel_mode: full_council
  panels:
    - id: primary
      reviewers: [codex-reviewer]
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: SelfImprove
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  max_total_cost: null
  max_cost_per_task: null
  max_tokens_per_agent: null
  alert_threshold_percent: 80
  enforcement: Soft
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
permissions:
  "*": Ask
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: [PATH]
"#,
        )
        .expect("write project config");

        let override_file = project_root.join("override.yaml");
        std::fs::write(
            &override_file,
            r#"
version: 1
launchers:
  claude:
    adapter: Acp
    command: claude
    args: []
  opencode:
    adapter: Acp
    command: opencode
    args: []
  gemini:
    adapter: Acp
    command: gemini
    args: [--acp]
lanes:
  claude-supervisor:
    launcher: claude
  opencode-worker:
    launcher: opencode
    model: { provider: ollama-cloud, name: glm-5.1 }
  gemini-reviewer:
    launcher: gemini
    model: { provider: google, name: gemini-3.1-pro-preview }
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: override
    permissions: [CreateTasks]
  workers:
    - lane: opencode-worker
      min: 1
      max: 1
  reviewers:
    - lane: gemini-reviewer
      min: 2
      max: 2
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 2
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: [gemini-reviewer]
  panel_mode: full_council
  panels:
    - id: primary
      reviewers: [gemini-reviewer]
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: SelfImprove
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  max_total_cost: null
  max_cost_per_task: null
  max_tokens_per_agent: null
  alert_threshold_percent: 80
  enforcement: Soft
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
permissions:
  "*": Ask
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: [PATH]
"#,
        )
        .expect("write override");

        let config = load_config_with_override(Some(&project_root), Some(&override_file))
            .expect("load override config");
        assert_eq!(config.roles.reviewers.len(), 1);
        assert_eq!(config.roles.reviewers[0].lane, "gemini-reviewer");
        assert_eq!(
            config.review.default_reviewers,
            vec!["gemini-reviewer".to_string()]
        );

        std::fs::remove_dir_all(&project_root).expect("remove tempdir");
    }

    #[test]
    fn load_config_with_override_accepts_partial_overlay() {
        let project_root = std::env::temp_dir().join(format!(
            "brehon-config-partial-overlay-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        if project_root.exists() {
            std::fs::remove_dir_all(&project_root).expect("cleanup tempdir");
        }
        std::fs::create_dir_all(&project_root).expect("create tempdir");

        let override_file = project_root.join("partial.yaml");
        std::fs::write(
            &override_file,
            r#"
version: 1
roles:
  workers:
    - lane: codex-worker
      min: 2
      max: 4
review:
  policy:
    min_approvals: 1
orchestration:
  max_active_workers: 4
"#,
        )
        .expect("write override");

        let config = load_config_with_override(Some(&project_root), Some(&override_file))
            .expect("load partial overlay");

        assert_eq!(config.roles.supervisor.name, "claude-supervisor");
        assert_eq!(config.roles.workers.len(), 1);
        assert_eq!(config.roles.workers[0].lane, "codex-worker");
        assert_eq!(config.roles.workers[0].min, 2);
        assert_eq!(config.roles.workers[0].max, 4);
        assert_eq!(config.review.policy.min_approvals, 1);
        assert_eq!(config.review.policy.min_average_score, 7);
        assert_eq!(config.orchestration.max_active_workers, 4);
        assert!(config.launchers.contains_key("codex"));
        assert!(config.lanes.contains_key("claude-reviewer"));

        std::fs::remove_dir_all(&project_root).expect("remove tempdir");
    }

    #[test]
    fn partial_overlay_can_patch_nested_launcher_without_redeclaring_it() {
        let project_root = std::env::temp_dir().join(format!(
            "brehon-config-partial-launcher-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        if project_root.exists() {
            std::fs::remove_dir_all(&project_root).expect("cleanup tempdir");
        }
        std::fs::create_dir_all(&project_root).expect("create tempdir");

        let override_file = project_root.join("partial.yaml");
        std::fs::write(
            &override_file,
            r#"
version: 1
launchers:
  codex:
    args: ["app-server", "--experimental"]
"#,
        )
        .expect("write override");

        let config = load_config_with_override(Some(&project_root), Some(&override_file))
            .expect("load partial overlay");

        let codex = config.launchers.get("codex").expect("codex launcher");
        assert_eq!(codex.adapter, brehon_types::agent::AdapterKind::Acp);
        assert_eq!(codex.command.as_deref(), Some("codex"));
        assert_eq!(
            codex.args,
            vec!["app-server".to_string(), "--experimental".to_string()]
        );

        std::fs::remove_dir_all(&project_root).expect("remove tempdir");
    }

    #[test]
    fn load_config_with_override_rejects_empty_worker_pools() {
        let project_root = std::env::temp_dir().join(format!(
            "brehon-config-empty-workers-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        if project_root.exists() {
            std::fs::remove_dir_all(&project_root).expect("cleanup tempdir");
        }
        std::fs::create_dir_all(&project_root).expect("create tempdir");

        let override_file = project_root.join("override.yaml");
        std::fs::write(
            &override_file,
            r#"
version: 1
launchers:
  claude:
    adapter: Acp
    command: claude
    args: []
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: override
    permissions: [CreateTasks]
  workers: []
  reviewers:
    - lane: claude
      min: 1
      max: 1
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 2
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: [claude]
  panel_mode: full_council
  panels:
    - id: primary
      reviewers: [claude]
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: SelfImprove
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  max_total_cost: null
  max_cost_per_task: null
  max_tokens_per_agent: null
  alert_threshold_percent: 80
  enforcement: Soft
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: [PATH]
prompt_fragments: {}
permissions:
  "*": Ask
prompt_policy:
  enabled: []
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
  "#,
        )
        .expect("write override");

        assert!(
            load_config_with_override(Some(&project_root), Some(&override_file)).is_err(),
            "empty workers should be rejected as malformed layer"
        );

        std::fs::remove_dir_all(&project_root).expect("remove tempdir");
    }

    #[test]
    fn load_config_with_override_rejects_empty_reviewer_pools() {
        let project_root = std::env::temp_dir().join(format!(
            "brehon-config-empty-reviewers-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        if project_root.exists() {
            std::fs::remove_dir_all(&project_root).expect("cleanup tempdir");
        }
        std::fs::create_dir_all(&project_root).expect("create tempdir");

        let override_file = project_root.join("override.yaml");
        std::fs::write(
            &override_file,
            r#"
version: 1
launchers:
  claude:
    adapter: Acp
    command: claude
    args: []
  codex:
    adapter: Acp
    command: codex
    args: []
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: override
    permissions: [CreateTasks]
  workers:
    - lane: claude
      min: 1
      max: 1
  reviewers: []
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 2
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: [claude]
  panel_mode: full_council
  panels:
    - id: primary
      reviewers: [claude]
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: SelfImprove
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  max_total_cost: null
  max_cost_per_task: null
  max_tokens_per_agent: null
  alert_threshold_percent: 80
  enforcement: Soft
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: [PATH]
prompt_fragments: {}
permissions:
  "*": Ask
prompt_policy:
  enabled: []
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
  "#,
        )
        .expect("write override");

        assert!(
            load_config_with_override(Some(&project_root), Some(&override_file)).is_err(),
            "empty reviewers should be rejected as malformed layer"
        );

        std::fs::remove_dir_all(&project_root).expect("remove tempdir");
    }

    #[test]
    fn load_config_with_override_rejects_empty_lanes_map() {
        let project_root = std::env::temp_dir().join(format!(
            "brehon-config-empty-lanes-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        if project_root.exists() {
            std::fs::remove_dir_all(&project_root).expect("cleanup tempdir");
        }
        std::fs::create_dir_all(&project_root).expect("create tempdir");

        let override_file = project_root.join("override.yaml");
        std::fs::write(
            &override_file,
            r#"
version: 1
launchers:
  claude:
    adapter: Acp
    command: claude
    args: []
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: override
    permissions: [CreateTasks]
  workers:
    - lane: claude
      min: 1
      max: 1
  reviewers:
    - lane: claude
      min: 1
      max: 1
lanes: {}
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 2
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: [claude]
  panel_mode: full_council
  panels:
    - id: primary
      reviewers: [claude]
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 3
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: SelfImprove
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  max_total_cost: null
  max_cost_per_task: null
  max_tokens_per_agent: null
  alert_threshold_percent: 80
  enforcement: Soft
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: [PATH]
prompt_fragments: {}
permissions:
  "*": Ask
prompt_policy:
  enabled: []
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
escalation:
  human_in_loop: true
  notify_via: Terminal
  escalation_timeout_minutes: 15
  "#,
        )
        .expect("write override");

        assert!(
            load_config_with_override(Some(&project_root), Some(&override_file)).is_err(),
            "empty lanes should be rejected as malformed layer"
        );

        std::fs::remove_dir_all(&project_root).expect("remove tempdir");
    }

    #[test]
    fn load_config_with_override_rejects_review_panel_unknown_lane() {
        let project_root = std::env::temp_dir().join(format!(
            "brehon-config-invalid-panel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        if project_root.exists() {
            std::fs::remove_dir_all(&project_root).expect("cleanup tempdir");
        }
        std::fs::create_dir_all(&project_root).expect("create tempdir");

        let override_file = project_root.join("invalid.yaml");
        std::fs::write(
            &override_file,
            r#"
version: 1
launchers:
  claude:
    adapter: Acp
    command: claude
    args: []
  gemini:
    adapter: Acp
    command: gemini
    args: [--acp]
lanes:
  claude-supervisor:
    launcher: claude
  gemini-reviewer:
    launcher: gemini
    model: { provider: google, name: gemini-3.1-pro-preview }
roles:
  supervisor:
    name: claude-supervisor
    kind: Supervisor
    description: override
    permissions: [CreateTasks]
  workers:
    - lane: claude-supervisor
      min: 1
      max: 1
  reviewers:
    - lane: gemini-reviewer
      min: 1
      max: 1
review:
  policy:
    min_average_score: 7
    min_individual_score: 6
    blocking_score: 5
    min_approvals: 2
    require_blocking_feedback_resolution: true
    max_review_rounds: 3
  timeout_minutes: 30
  auto_assign: true
  default_reviewers: [gemini-reviewer]
  panel_mode: full_council
  panels:
    - id: primary
      reviewers: [copilot-reviewer]
  max_diff_tokens: 8000
  chunk_strategy: ByDirectory
  stale_detection:
    enabled: true
    ignore_files: []
    strategy: DeltaReview
supervisor:
  autonomy: Guided
  heartbeat_minutes: 15
  stuck_detection:
    time_threshold_minutes: 10
    operation_aware: true
    pattern_detection: true
  nudge:
    soft_after_minutes: 5
    guidance_after_minutes: 10
orchestration:
  max_active_workers: 0
  worktree_isolation: true
  branch_prefix: "brehon/"
  auto_cleanup_worktrees: true
  worker_idle_behavior: Wait
  allow_mutating_idle_work: false
  self_improve_tasks: []
budget:
  max_total_cost: null
  max_cost_per_task: null
  max_tokens_per_agent: null
  alert_threshold_percent: 80
  enforcement: Soft
tui:
  default_layout: Balanced
  terminal_mode: Auto
  notifications:
    toast_duration_seconds: 5
    flash_tabs: true
    modal_on_critical: true
  keybindings: default
escalation:
  human_in_loop: true
  notify_via: Terminal
context:
  db_path: ".brehon/brehon.db"
  search_index_path: ".brehon/indexes/tantivy"
  memory_ttl_days: null
  max_memories: 10000
  agents_md: Auto
permissions:
  "*": Ask
security:
  sandbox_profile: OsDefault
  persist_transcripts: true
  redact_patterns: []
  env_allowlist: [PATH]
"#,
        )
        .expect("write override");

        assert!(
            load_config_with_override(Some(&project_root), Some(&override_file)).is_err(),
            "invalid panel should fail"
        );

        std::fs::remove_dir_all(&project_root).expect("remove tempdir");
    }

    #[test]
    fn global_config_path_returns_path() {
        let path = global_config_path();
        assert!(path.is_some());
        let path = path.unwrap();
        assert!(path.ends_with("config.yaml"));
    }

    #[test]
    fn interpolate_env_value_resolves_required_and_default_placeholders() {
        let key_name = "BREHON_TEST_OLLAMA_KEY_INTERPOLATION";
        let old = std::env::var_os(key_name);
        std::env::set_var(key_name, "secret-token");

        let resolved =
            interpolate_env_value(&format!("Bearer ${{{key_name}}}")).expect("resolve env var");
        assert_eq!(resolved, "Bearer secret-token");

        std::env::remove_var(key_name);
        let defaulted =
            interpolate_env_value("https://${BREHON_TEST_OLLAMA_HOST:-ollama.com}/v1/messages")
                .expect("resolve default");
        assert_eq!(defaulted, "https://ollama.com/v1/messages");

        match old {
            Some(value) => std::env::set_var(key_name, value),
            None => std::env::remove_var(key_name),
        }
    }

    #[test]
    fn resolve_launcher_env_placeholders_errors_on_missing_required_var() {
        let mut config = parse_defaults().expect("defaults");
        config.launchers.insert(
            "claude-ollama-cloud".to_string(),
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
                max_parallel_tool_calls: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::from([(
                    "ANTHROPIC_AUTH_TOKEN".to_string(),
                    "${BREHON_TEST_MISSING_OLLAMA_KEY}".to_string(),
                )]),
                headers: std::collections::HashMap::new(),
            },
        );

        let err = resolve_launcher_env_placeholders(&mut config).expect_err("missing env");
        assert!(err
            .to_string()
            .contains("launchers.claude-ollama-cloud.env.ANTHROPIC_AUTH_TOKEN"));
    }

    #[test]
    fn roundtrip_defaults() {
        let config = parse_defaults().expect("Failed to parse defaults");
        let yaml = serde_yaml::to_string(&config).expect("Failed to serialize");
        let parsed: BrehonConfig = serde_yaml::from_str(&yaml).expect("Failed to re-parse");
        assert_eq!(config, parsed);
    }

    #[test]
    fn load_project_config_accepts_either_project_root_or_brehon_dir() {
        // Regression: callers that hand `load_project_config` a path already
        // pointing at `.brehon` (e.g. MCP servers whose only populated env var
        // is BREHON_ROOT) used to silently get `Ok(None)` because the old code
        // unconditionally appended `.brehon/config.yaml` — producing the
        // nonexistent path `.brehon/.brehon/config.yaml`. Downstream that made
        // `VerificationTool` fall back to the default `Exclusive` lease mode,
        // suppressing reviewer session resets.
        let project_root = std::env::temp_dir().join(format!(
            "brehon-config-project-root-or-brehon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        if project_root.exists() {
            std::fs::remove_dir_all(&project_root).expect("cleanup tempdir");
        }
        let brehon_dir = project_root.join(".brehon");
        std::fs::create_dir_all(&brehon_dir).expect("mkdir");

        let defaults_yaml =
            serde_yaml::to_string(&parse_defaults().expect("defaults")).expect("yaml");
        std::fs::write(brehon_dir.join("config.yaml"), defaults_yaml).expect("write");

        let via_project_root = load_project_config(&project_root)
            .expect("load via project root")
            .expect("some config");
        let via_brehon_dir = load_project_config(&brehon_dir)
            .expect("load via .brehon dir")
            .expect("some config");
        assert_eq!(via_project_root, via_brehon_dir);

        // Sanity: unrelated path returns None rather than erroring.
        let stray = project_root.join("does-not-exist");
        assert!(load_project_config(&stray).expect("stray ok").is_none());

        std::fs::remove_dir_all(&project_root).ok();
    }
}
