use std::path::Path;

use anyhow::Result;
use serde_yaml::{Mapping, Value};

use crate::ui;

/// Known agents: (config_name, cli_command, display_description)
const KNOWN_AGENTS: &[(&str, &str, &str)] = &[
    ("claude-code", "claude", "Claude Code CLI"),
    ("copilot", "copilot", "GitHub Copilot CLI"),
    ("codex", "codex", "Codex CLI"),
    ("gemini", "gemini", "Gemini CLI"),
    ("kimi", "kimi", "Kimi Code CLI"),
    ("opencode", "opencode", "OpenCode CLI"),
    ("agy", "agy", "Antigravity CLI"),
];

const AGY_PROJECT_MCP_CONFIG_PATH: &str = ".agents/mcp_config.json";
const INIT_GITIGNORE_PATTERNS: &[&str] =
    &[".brehon/", ".agents/mcp_config.json", ".antigravitycli"];

/// Agent info for config generation.
struct DetectedAgent {
    check: ui::AgentCheck,
}

/// Detect which known agent CLIs are available on PATH.
fn detect_agents() -> Vec<DetectedAgent> {
    KNOWN_AGENTS
        .iter()
        .map(|(name, command, description)| {
            let found = which::which(command);
            DetectedAgent {
                check: ui::AgentCheck {
                    name: name.to_string(),
                    command: command.to_string(),
                    description: description.to_string(),
                    found: found.is_ok(),
                    path: found.ok().map(|p| p.display().to_string()),
                },
            }
        })
        .collect()
}

/// Generate a complete project config tailored to detected agents.
///
/// Loads defaults, modifies agent/role sections based on detected agents,
/// and serializes to YAML with a header comment.
fn generate_config_for_agents(agents: &[DetectedAgent]) -> String {
    use brehon_config::parse_defaults;
    use brehon_types::agent::AdapterKind;
    use brehon_types::{
        AgentConnectionConfig, LaneConfig, ModelConfig, ReviewPanelConfig, ReviewerPoolConfig,
        WorkerAssignmentMode, WorkerPoolConfig,
    };
    use std::collections::HashMap;

    fn arg_list(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn env_map(values: &[(&str, &str)]) -> HashMap<String, String> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn launcher(
        command: &str,
        launcher_args: &[&str],
        env: &[(&str, &str)],
    ) -> AgentConnectionConfig {
        AgentConnectionConfig {
            adapter: AdapterKind::Acp,
            command: Some(command.to_string()),
            args: arg_list(launcher_args),
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
            env: env_map(env),
            headers: HashMap::new(),
        }
    }

    fn insert_lane(
        lanes: &mut HashMap<String, LaneConfig>,
        lane: &str,
        launcher: &str,
        provider: &str,
        model: &str,
        reasoning_effort: Option<&str>,
        system_prompt: Option<String>,
    ) {
        lanes.insert(
            lane.to_string(),
            LaneConfig {
                launcher: launcher.to_string(),
                model: Some(ModelConfig {
                    provider: provider.to_string(),
                    name: model.to_string(),
                }),
                reasoning_effort: reasoning_effort.map(str::to_string),
                system_prompt,
            },
        );
    }

    fn template_launchers() -> HashMap<String, AgentConnectionConfig> {
        HashMap::from([
            ("claude".to_string(), launcher("claude", &[], &[])),
            (
                "claude-ollama-cloud".to_string(),
                launcher(
                    "claude",
                    &[],
                    &[
                        ("ANTHROPIC_AUTH_TOKEN", "${OLLAMA_API_KEY:-ollama}"),
                        ("ANTHROPIC_API_KEY", "${OLLAMA_API_KEY:-}"),
                        (
                            "ANTHROPIC_BASE_URL",
                            "${OLLAMA_ANTHROPIC_BASE_URL:-https://ollama.com}",
                        ),
                    ],
                ),
            ),
            ("copilot".to_string(), launcher("copilot", &[], &[])),
            ("codex".to_string(), launcher("codex", &["app-server"], &[])),
            (
                "codex-ollama-cloud".to_string(),
                launcher(
                    "codex",
                    &[
                        "-c",
                        "model_provider=\"ollama_cloud\"",
                        "-c",
                        "model_providers.ollama_cloud={name=\"Ollama Cloud\", base_url=\"https://ollama.com/v1\", env_key=\"OLLAMA_API_KEY\", wire_api=\"responses\"}",
                        "app-server",
                    ],
                    &[],
                ),
            ),
            (
                "gemini".to_string(),
                launcher("gemini", &["--acp"], &[]),
            ),
            ("kimi".to_string(), launcher("kimi", &["acp"], &[])),
            (
                "opencode".to_string(),
                launcher("opencode", &["acp", "--cwd", "."], &[]),
            ),
            (
                "agy".to_string(),
                AgentConnectionConfig {
                    adapter: AdapterKind::Agy,
                    command: Some("agy".to_string()),
                    args: arg_list(&["--dangerously-skip-permissions"]),
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
                    env: HashMap::new(),
                    headers: HashMap::new(),
                },
            ),
        ])
    }

    fn template_lanes(default_reviewer_prompt: Option<&str>) -> HashMap<String, LaneConfig> {
        let mut lanes = HashMap::new();
        let reviewer_prompt = default_reviewer_prompt.map(str::to_string);

        let lane_effort = |launcher: &str, role: &str| -> Option<&'static str> {
            match launcher {
                "gemini" | "agy" => None,
                _ => Some(if role == "worker" { "medium" } else { "high" }),
            }
        };

        for (launcher, provider, default_model) in [
            ("claude", "anthropic", "claude-sonnet-4-6"),
            ("claude-ollama-cloud", "ollama-cloud", "glm-5.1"),
            ("copilot", "github-copilot", "gpt-5"),
            ("codex", "openai", "gpt-5.4"),
            ("codex-ollama-cloud", "ollama-cloud", "glm-5.1:cloud"),
            ("gemini", "google", "gemini-3.1-pro-preview"),
            ("kimi", "kimi-code", "kimi-for-coding"),
            ("opencode", "ollama-cloud", "glm-5.1"),
            ("agy", "google", "antigravity-2.0"),
        ] {
            insert_lane(
                &mut lanes,
                &format!("{launcher}-supervisor"),
                launcher,
                provider,
                default_model,
                lane_effort(launcher, "supervisor"),
                None,
            );
            insert_lane(
                &mut lanes,
                &format!("{launcher}-worker"),
                launcher,
                provider,
                default_model,
                lane_effort(launcher, "worker"),
                None,
            );
            insert_lane(
                &mut lanes,
                &format!("{launcher}-reviewer"),
                launcher,
                provider,
                default_model,
                lane_effort(launcher, "reviewer"),
                reviewer_prompt.clone(),
            );
        }

        lanes.insert(
            "claude-reviewer".to_string(),
            LaneConfig {
                launcher: "claude".to_string(),
                model: Some(ModelConfig {
                    provider: "anthropic".to_string(),
                    name: "claude-opus-4-6".to_string(),
                }),
                reasoning_effort: Some("high".to_string()),
                system_prompt: reviewer_prompt.clone(),
            },
        );
        lanes.insert(
            "codex-worker".to_string(),
            LaneConfig {
                launcher: "codex".to_string(),
                model: Some(ModelConfig {
                    provider: "openai".to_string(),
                    name: "gpt-5.3-codex".to_string(),
                }),
                reasoning_effort: Some("medium".to_string()),
                system_prompt: None,
            },
        );
        lanes.insert(
            "codex-hardening".to_string(),
            LaneConfig {
                launcher: "codex".to_string(),
                model: Some(ModelConfig {
                    provider: "openai".to_string(),
                    name: "gpt-5.5".to_string(),
                }),
                reasoning_effort: Some("xhigh".to_string()),
                system_prompt: None,
            },
        );
        lanes.insert(
            "codex-reviewer".to_string(),
            LaneConfig {
                launcher: "codex".to_string(),
                model: Some(ModelConfig {
                    provider: "openai".to_string(),
                    name: "gpt-5.4".to_string(),
                }),
                reasoning_effort: Some("high".to_string()),
                system_prompt: reviewer_prompt.clone(),
            },
        );

        lanes
    }

    fn sort_mapping_keys(mapping: &Mapping, preferred_keys: &[&str]) -> Mapping {
        let mut ordered = Mapping::new();
        for key in preferred_keys {
            let value_key = Value::String((*key).to_string());
            if let Some(value) = mapping.get(&value_key) {
                ordered.insert(value_key, value.clone());
            }
        }

        let mut remaining: Vec<(String, Value)> = mapping
            .iter()
            .filter_map(|(key, value)| match key {
                Value::String(string_key) if !preferred_keys.contains(&string_key.as_str()) => {
                    Some((string_key.clone(), value.clone()))
                }
                _ => None,
            })
            .collect();
        remaining.sort_by(|left, right| left.0.cmp(&right.0));

        for (key, value) in remaining {
            ordered.insert(Value::String(key), value);
        }

        ordered
    }

    fn reorder_yaml_catalogs(document: &mut Value) {
        let launcher_order = [
            "claude",
            "claude-ollama-cloud",
            "copilot",
            "codex",
            "codex-ollama-cloud",
            "gemini",
            "kimi",
            "opencode",
            "agy",
        ];
        let lane_order = [
            "claude-supervisor",
            "claude-worker",
            "claude-reviewer",
            "claude-ollama-cloud-supervisor",
            "claude-ollama-cloud-worker",
            "claude-ollama-cloud-reviewer",
            "copilot-supervisor",
            "copilot-worker",
            "copilot-reviewer",
            "codex-supervisor",
            "codex-worker",
            "codex-hardening",
            "codex-reviewer",
            "codex-ollama-cloud-supervisor",
            "codex-ollama-cloud-worker",
            "codex-ollama-cloud-reviewer",
            "gemini-supervisor",
            "gemini-worker",
            "gemini-reviewer",
            "kimi-supervisor",
            "kimi-worker",
            "kimi-reviewer",
            "opencode-supervisor",
            "opencode-worker",
            "opencode-reviewer",
            "agy-supervisor",
            "agy-worker",
            "agy-reviewer",
        ];

        let Some(root) = document.as_mapping_mut() else {
            return;
        };

        for field in ["launchers", "lanes"] {
            let Some(value) = root.get_mut(Value::String(field.to_string())) else {
                continue;
            };
            let Some(mapping) = value.as_mapping() else {
                continue;
            };

            let ordered = match field {
                "launchers" => sort_mapping_keys(mapping, &launcher_order),
                "lanes" => sort_mapping_keys(mapping, &lane_order),
                _ => unreachable!(),
            };
            *value = Value::Mapping(ordered);
        }
    }

    fn put_str(mapping: &mut Mapping, key: &str, value: Value) {
        mapping.insert(Value::String(key.to_string()), value);
    }

    fn worker_pool_overlay(pool_config: &WorkerPoolConfig) -> Value {
        let mut pool = Mapping::new();
        put_str(
            &mut pool,
            "lane",
            Value::String(pool_config.lane.to_string()),
        );
        if pool_config.assignment_mode != WorkerAssignmentMode::Normal {
            put_str(
                &mut pool,
                "assignment_mode",
                serde_yaml::to_value(pool_config.assignment_mode).unwrap(),
            );
        }
        if !pool_config.accepts.is_empty() {
            put_str(
                &mut pool,
                "accepts",
                serde_yaml::to_value(&pool_config.accepts).unwrap(),
            );
        }
        put_str(
            &mut pool,
            "min",
            serde_yaml::to_value(pool_config.min).unwrap(),
        );
        put_str(
            &mut pool,
            "max",
            serde_yaml::to_value(pool_config.max).unwrap(),
        );
        Value::Mapping(pool)
    }

    fn pool_overlay(lane: &str, min: u32, max: u32) -> Value {
        let mut pool = Mapping::new();
        put_str(&mut pool, "lane", Value::String(lane.to_string()));
        put_str(&mut pool, "min", serde_yaml::to_value(min).unwrap());
        put_str(&mut pool, "max", serde_yaml::to_value(max).unwrap());
        Value::Mapping(pool)
    }

    fn lane_overlay(lane: &LaneConfig, include_system_prompt: bool) -> Value {
        let mut value = Mapping::new();
        put_str(
            &mut value,
            "launcher",
            Value::String(lane.launcher.to_string()),
        );
        if let Some(model) = &lane.model {
            put_str(&mut value, "model", serde_yaml::to_value(model).unwrap());
        }
        if let Some(effort) = &lane.reasoning_effort {
            put_str(
                &mut value,
                "reasoning_effort",
                Value::String(effort.clone()),
            );
        }
        if include_system_prompt {
            if let Some(prompt) = &lane.system_prompt {
                put_str(&mut value, "system_prompt", Value::String(prompt.clone()));
            }
        }
        Value::Mapping(value)
    }

    fn active_overlay(
        config: &brehon_types::BrehonConfig,
        defaults: &brehon_types::BrehonConfig,
    ) -> Value {
        let mut root = Mapping::new();
        put_str(
            &mut root,
            "version",
            serde_yaml::to_value(config.version).unwrap(),
        );

        let active_lanes: std::collections::BTreeSet<String> =
            std::iter::once(config.roles.supervisor.name.clone())
                .chain(
                    config
                        .roles
                        .workers
                        .iter()
                        .map(|worker| worker.lane.clone()),
                )
                .chain(
                    config
                        .roles
                        .reviewers
                        .iter()
                        .map(|reviewer| reviewer.lane.clone()),
                )
                .collect();

        let mut launchers = Mapping::new();
        let mut lanes = Mapping::new();
        for lane_name in &active_lanes {
            let Some(lane) = config.lanes.get(lane_name) else {
                continue;
            };
            let include_system_prompt = lane_name != &config.roles.supervisor.name;
            let needs_lane_overlay = match defaults.lanes.get(lane_name) {
                Some(default_lane) => {
                    default_lane.launcher != lane.launcher
                        || default_lane.model != lane.model
                        || (include_system_prompt
                            && default_lane.system_prompt != lane.system_prompt)
                }
                None => true,
            };
            if needs_lane_overlay {
                put_str(
                    &mut lanes,
                    lane_name,
                    lane_overlay(lane, include_system_prompt),
                );
            }
            if let Some(launcher) = config.launchers.get(&lane.launcher) {
                if defaults.launchers.get(&lane.launcher) != Some(launcher) {
                    put_str(
                        &mut launchers,
                        &lane.launcher,
                        serde_yaml::to_value(launcher).unwrap(),
                    );
                }
            }
        }
        if !launchers.is_empty() {
            put_str(&mut root, "launchers", Value::Mapping(launchers));
        }
        if !lanes.is_empty() {
            put_str(&mut root, "lanes", Value::Mapping(lanes));
        }

        let mut roles = Mapping::new();
        let mut supervisor = Mapping::new();
        put_str(
            &mut supervisor,
            "name",
            Value::String(config.roles.supervisor.name.clone()),
        );
        put_str(&mut roles, "supervisor", Value::Mapping(supervisor));
        put_str(
            &mut roles,
            "workers",
            Value::Sequence(
                config
                    .roles
                    .workers
                    .iter()
                    .map(worker_pool_overlay)
                    .collect(),
            ),
        );
        put_str(
            &mut roles,
            "reviewers",
            Value::Sequence(
                config
                    .roles
                    .reviewers
                    .iter()
                    .map(|reviewer| pool_overlay(&reviewer.lane, reviewer.min, reviewer.max))
                    .collect(),
            ),
        );
        put_str(&mut root, "roles", Value::Mapping(roles));

        let mut policy = Mapping::new();
        put_str(
            &mut policy,
            "min_approvals",
            serde_yaml::to_value(config.review.policy.min_approvals).unwrap(),
        );
        let mut review = Mapping::new();
        put_str(&mut review, "policy", Value::Mapping(policy));
        put_str(
            &mut review,
            "default_reviewers",
            serde_yaml::to_value(&config.review.default_reviewers).unwrap(),
        );
        put_str(
            &mut review,
            "panels",
            serde_yaml::to_value(&config.review.panels).unwrap(),
        );
        put_str(&mut root, "review", Value::Mapping(review));

        let mut orchestration = Mapping::new();
        put_str(
            &mut orchestration,
            "max_active_workers",
            serde_yaml::to_value(config.orchestration.max_active_workers).unwrap(),
        );
        put_str(&mut root, "orchestration", Value::Mapping(orchestration));

        Value::Mapping(root)
    }

    // Start from the full defaults config (without triggering validation warnings)
    let defaults = parse_defaults().expect("Failed to load default config");
    let mut config = defaults.clone();

    let found: Vec<&DetectedAgent> = agents.iter().filter(|a| a.check.found).collect();
    let default_reviewer_prompt = config.roles.reviewers.iter().find_map(|reviewer| {
        config
            .lane_system_prompt(&reviewer.lane, reviewer.system_prompt.as_deref())
            .map(str::to_string)
    });

    let launcher_key = |detected_name: &str| match detected_name {
        "claude-code" => "claude".to_string(),
        other => other.to_string(),
    };
    let supervisor_lane = |launcher: &str| format!("{launcher}-supervisor");
    let worker_lane = |launcher: &str| format!("{launcher}-worker");
    let reviewer_lane = |launcher: &str| format!("{launcher}-reviewer");
    let found_launcher =
        |name: &str| -> bool { found.iter().any(|agent| agent.check.name == name) };
    config.launchers = template_launchers();
    config.lanes = template_lanes(default_reviewer_prompt.as_deref());

    // Tailor supervisor: prefer claude, then codex, then first detected.
    let supervisor_choice = [
        "claude-code",
        "codex",
        "copilot",
        "opencode",
        "kimi",
        "gemini",
        "agy",
    ]
    .iter()
    .find(|name| found_launcher(name))
    .and_then(|name| found.iter().find(|agent| agent.check.name == **name))
    .or(found.first());
    if let Some(sup) = supervisor_choice {
        let launcher = launcher_key(&sup.check.name);
        let lane = supervisor_lane(&launcher);
        let model = config
            .lanes
            .get(&lane)
            .and_then(|lane| lane.model.clone())
            .expect("template supervisor lane should have a model");
        let reasoning_effort = config
            .lanes
            .get(&lane)
            .and_then(|lane| lane.reasoning_effort.clone());
        config.supervisor.model = Some(model);
        config.supervisor.reasoning_effort = reasoning_effort;
        config.roles.supervisor.name = lane.clone();
        if let Some(supervisor_lane) = config.lanes.get_mut(&lane) {
            supervisor_lane.system_prompt = config.roles.supervisor.system_prompt.clone();
        }
    }

    // Tailor workers: prefer codex, then opencode, then first detected.
    let worker_choice = [
        "codex",
        "copilot",
        "opencode",
        "kimi",
        "claude-code",
        "gemini",
        "agy",
    ]
    .iter()
    .find(|name| found_launcher(name))
    .and_then(|name| found.iter().find(|agent| agent.check.name == **name))
    .or(found.first());
    if let Some(worker) = worker_choice {
        let launcher = launcher_key(&worker.check.name);
        let lane = worker_lane(&launcher);
        let template_lane = config
            .lanes
            .get(&lane)
            .cloned()
            .expect("template worker lane should exist");
        config.roles.workers = vec![WorkerPoolConfig {
            lane,
            model: template_lane.model,
            reasoning_effort: template_lane.reasoning_effort,
            assignment_mode: WorkerAssignmentMode::Normal,
            accepts: Vec::new(),
            min: 1,
            max: 5,
        }];
        if found_launcher("codex") {
            let hardening_lane = "codex-hardening".to_string();
            let template_lane = config
                .lanes
                .get(&hardening_lane)
                .cloned()
                .expect("template hardening lane should exist");
            config.roles.workers.push(WorkerPoolConfig {
                lane: hardening_lane,
                model: template_lane.model,
                reasoning_effort: template_lane.reasoning_effort,
                assignment_mode: WorkerAssignmentMode::Reserved,
                accepts: vec!["final_hardening".to_string()],
                min: 1,
                max: 1,
            });
        }
        config.orchestration.max_active_workers = 5;
    }

    // Tailor reviewers using canonical role lanes, not leftovers from supervisor/worker selection.
    {
        let reviewer_candidates = [
            "claude-code",
            "codex",
            "copilot",
            "gemini",
            "kimi",
            "opencode",
            "agy",
        ];
        let mut reviewers = Vec::new();
        for candidate in reviewer_candidates {
            if reviewers.len() >= 3 {
                break;
            }
            let Some(agent) = found.iter().find(|agent| agent.check.name == candidate) else {
                continue;
            };
            let launcher = launcher_key(&agent.check.name);
            let lane = reviewer_lane(&launcher);
            let template_lane = config
                .lanes
                .get(&lane)
                .cloned()
                .expect("template reviewer lane should exist");
            reviewers.push(ReviewerPoolConfig {
                lane,
                model: template_lane.model,
                reasoning_effort: template_lane.reasoning_effort,
                system_prompt: template_lane.system_prompt,
                min: 1,
                max: 3,
            });
        }
        if !reviewers.is_empty() {
            config.roles.reviewers = reviewers;
            // Update default_reviewers to match
            config.review.default_reviewers = config
                .roles
                .reviewers
                .iter()
                .map(|reviewer| reviewer.lane.clone())
                .collect();
            config.review.panels = vec![ReviewPanelConfig {
                id: "primary".to_string(),
                reviewers: config.review.default_reviewers.clone(),
            }];
            config.review.policy.min_approvals = config
                .review
                .policy
                .min_approvals
                .min(config.review.default_reviewers.len().max(1) as u8);
        }
    }

    // Serialize a small project overlay, not the full resolved config. Built-in
    // defaults supply the advanced sections.
    let mut yaml_value = active_overlay(&config, &defaults);
    reorder_yaml_catalogs(&mut yaml_value);
    let yaml = serde_yaml::to_string(&yaml_value).expect("Failed to serialize config");

    format!(
        r#"# Brehon Project Configuration
# Layers: baked-in defaults < ~/.config/brehon/config.yaml < this file
#
# This is a partial overlay. Keep it small: omit anything that should use the
# built-in defaults. Run `brehon config list` to inspect the resolved config.
#
# Common edits:
# - roles.workers / roles.reviewers: choose lanes and pool sizes.
# - review.policy.min_approvals: how many review approvals are required.
# - orchestration.max_active_workers: concurrent active worker tasks.
# - permissions: allow/deny common tool actions.
#
# Optional research scaffold (uncomment and rename lanes/pools):
# research:
#   enabled: false
#   pools:
#   - id: spec-research
#     lane: cheap-worker
#     instruction_profile: "Cite primary sources and summarize task-relevant facts."
#     role: normative_requirements
#     min: 0
#     max: 2
#   routes:
#   - id: specs-for-protocol-work
#     trigger: before_assignment
#     match: {{ text_any: [RFC, protocol, PFCP] }}
#     jobs:
#     - id: normative-requirements
#       pool: spec-research
#       prompt_template: "Task {{{{task_id}}}}: {{{{title}}}}\nSummarize requirements and cite sources."

{yaml}"#
    )
}

fn gitignore_pattern_present(lines: &[&str], pattern: &str) -> bool {
    lines.iter().any(|line| {
        let trimmed = line.trim();
        trimmed == pattern || (pattern == ".brehon/" && trimmed == ".brehon")
    })
}

/// Update .gitignore to include Brehon and machine-local agent files.
fn update_gitignore(project_path: &Path) -> Result<bool> {
    let gitignore_path = project_path.join(".gitignore");
    let content = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };
    let lines = content.lines().collect::<Vec<_>>();
    let missing = INIT_GITIGNORE_PATTERNS
        .iter()
        .filter(|pattern| !gitignore_pattern_present(&lines, pattern))
        .copied()
        .collect::<Vec<_>>();

    if missing.is_empty() {
        return Ok(false);
    }

    let mut new_content = content;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    if !new_content.is_empty() {
        new_content.push('\n');
    }
    new_content.push_str("# Brehon orchestration data\n");
    for pattern in missing {
        new_content.push_str(pattern);
        new_content.push('\n');
    }
    std::fs::write(&gitignore_path, new_content)?;

    Ok(true)
}

fn current_brehon_exe() -> String {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| "brehon".to_string())
}

fn desired_brehon_mcp_server() -> serde_json::Value {
    serde_json::json!({
        "command": current_brehon_exe(),
        "args": ["serve"]
    })
}

fn ensure_agy_mcp_config(project_path: &Path) -> Result<bool> {
    let path = project_path.join(AGY_PROJECT_MCP_CONFIG_PATH);
    let brehon_server = desired_brehon_mcp_server();
    let mut doc = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !doc.is_object() {
        doc = serde_json::json!({});
    }

    let servers = doc
        .as_object_mut()
        .expect("JSON object initialized above")
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        *servers = serde_json::json!({});
    }
    let servers = servers
        .as_object_mut()
        .expect("mcpServers object initialized above");
    if servers.get("brehon") == Some(&brehon_server) {
        return Ok(false);
    }
    servers.insert("brehon".to_string(), brehon_server);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(true)
}

pub fn execute(project_path: &Path) -> Result<()> {
    // Banner
    ui::print_banner();

    // Detect agents
    ui::print_section("Detecting agents...");
    let agents = detect_agents();
    let checks: Vec<&ui::AgentCheck> = agents.iter().map(|a| &a.check).collect();
    ui::print_agent_checks(&checks);

    let found_count = agents.iter().filter(|a| a.check.found).count();
    println!();
    if found_count == 0 {
        ui::print_warning("No agents detected on PATH. Generating a minimal default overlay.");
        println!(
            "    {}",
            ui::dim(
                "Install an agent CLI and re-run 'brehon init', or add launchers and lanes manually."
            )
        );
    } else {
        println!(
            "    {}",
            ui::dim(&format!(
                "{} agent{} detected, configuration will be tailored.",
                found_count,
                if found_count == 1 { "" } else { "s" }
            ))
        );
    }
    println!();

    // Check if already initialized
    let brehon_dir = project_path.join(".brehon");
    let config_path = brehon_dir.join("config.yaml");

    if config_path.exists() {
        ui::print_rule();
        println!();
        ui::print_warning("Project already initialized.");
        println!(
            "    {}",
            ui::dim(&format!("Config exists at {}", config_path.display()))
        );
        println!(
            "    {}",
            ui::dim("Delete .brehon/config.yaml to re-initialize.")
        );
        println!();
        return Ok(());
    }

    // Initialize
    ui::print_section("Initializing project...");

    // Create .brehon directory
    std::fs::create_dir_all(&brehon_dir)?;
    ui::print_success(&format!("Created {}", ui::dim(".brehon/")));

    // Write config (temporarily suppress tracing since load_config emits warnings
    // about default config validation, which is expected during init)
    let config_content = generate_config_for_agents(&agents);
    std::fs::write(&config_path, &config_content)?;
    ui::print_success(&format!("Created {}", ui::dim(".brehon/config.yaml")));

    if agents
        .iter()
        .any(|agent| agent.check.name == "agy" && agent.check.found)
    {
        match ensure_agy_mcp_config(project_path) {
            Ok(true) => {
                ui::print_success(&format!("Created {}", ui::dim(AGY_PROJECT_MCP_CONFIG_PATH)))
            }
            Ok(false) => ui::print_success(&format!(
                "{} already configured",
                ui::dim(AGY_PROJECT_MCP_CONFIG_PATH)
            )),
            Err(e) => ui::print_warning(&format!(
                "Could not create {}: {}",
                AGY_PROJECT_MCP_CONFIG_PATH, e
            )),
        }
    }

    // Update .gitignore
    match update_gitignore(project_path) {
        Ok(true) => ui::print_success(&format!("Updated {}", ui::dim(".gitignore"))),
        Ok(false) => ui::print_success(&format!(
            "{} already in {}",
            ui::dim("Brehon ignore patterns"),
            ui::dim(".gitignore")
        )),
        Err(e) => ui::print_warning(&format!("Could not update .gitignore: {}", e)),
    }

    println!();
    ui::print_rule();
    println!();

    // Quick start table
    ui::print_section("Quick Start");

    ui::print_table(
        (" Command", " Description"),
        &[
            (" brehon", " Launch the orchestration TUI"),
            (" brehon config validate", " Validate your configuration"),
            (" brehon doctor", " Run full diagnostics"),
            (" brehon init", " Re-initialize (after removing .brehon/)"),
        ],
    );

    println!();

    // Next steps
    ui::print_section("Next steps");

    ui::print_steps(&[
        &format!(
            "Edit {} to configure agents and roles",
            ui::bold(".brehon/config.yaml")
        ),
        &format!("Run {} to verify your setup", ui::cyan("brehon doctor")),
        &format!("Run {} to start orchestrating", ui::cyan("brehon")),
    ]);

    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::agent::AdapterKind;

    fn load_generated_config(yaml: &str) -> brehon_types::BrehonConfig {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, yaml).expect("write generated config");
        brehon_config::load_config_with_override(None, Some(&path)).expect("load generated config")
    }

    fn detected_agent(name: &str, command: &str, provider: &str, model: &str) -> DetectedAgent {
        let _ = (provider, model);
        DetectedAgent {
            check: ui::AgentCheck {
                name: name.to_string(),
                command: command.to_string(),
                description: format!("{name} description"),
                found: true,
                path: Some(format!("/usr/bin/{command}")),
            },
        }
    }

    #[test]
    fn generated_config_includes_default_reviewer_system_prompt() {
        let agents = vec![
            detected_agent("claude-code", "claude", "anthropic", "claude-sonnet-4-6"),
            detected_agent("codex", "codex", "openai", "gpt-5.3-codex"),
        ];

        let yaml = generate_config_for_agents(&agents);
        let config = load_generated_config(&yaml);

        assert!(yaml.contains("roles:"));
        assert!(yaml.contains("reviewers:"));
        assert!(
            !yaml.contains("system_prompt:"),
            "generated overlay should not inline the default reviewer prompt"
        );
        let prompt = config
            .lane_system_prompt("claude-reviewer", None)
            .expect("default reviewer prompt");
        assert!(prompt.contains("You are a reviewer."));
        assert!(prompt.contains("evaluate"));
        assert!(prompt.contains("Treat active review obligations as your source of truth"));
        assert!(prompt.contains("Review panels are leased to tasks"));
    }

    #[test]
    fn generated_config_excludes_primary_worker_and_supervisor_from_default_panel_when_possible() {
        let agents = vec![
            detected_agent("claude-code", "claude", "anthropic", "claude-sonnet-4-6"),
            detected_agent("codex", "codex", "openai", "gpt-5.3-codex"),
            detected_agent("gemini", "gemini", "google", "gemini-3.1-pro-preview"),
            detected_agent("opencode", "opencode", "ollama-cloud", "glm-5.1"),
        ];

        let yaml = generate_config_for_agents(&agents);
        let config = load_generated_config(&yaml);

        assert_eq!(config.roles.supervisor.name, "claude-supervisor");
        assert_eq!(config.roles.workers[0].lane, "codex-worker");
        assert!(config.roles.workers.iter().any(|worker| {
            worker.lane == "codex-hardening"
                && worker.assignment_mode == brehon_types::WorkerAssignmentMode::Reserved
                && worker.accepts == vec!["final_hardening".to_string()]
        }));
        assert_eq!(
            config.review.default_reviewers,
            vec![
                "claude-reviewer".to_string(),
                "codex-reviewer".to_string(),
                "gemini-reviewer".to_string()
            ]
        );
        assert_eq!(config.review.panels.len(), 1);
        assert_eq!(config.review.panels[0].id, "primary");
        assert_eq!(
            config.review.panels[0].reviewers,
            vec![
                "claude-reviewer".to_string(),
                "codex-reviewer".to_string(),
                "gemini-reviewer".to_string()
            ]
        );
    }

    #[test]
    fn generated_config_uses_role_specific_models() {
        let agents = vec![
            detected_agent("claude-code", "claude", "anthropic", "claude-sonnet-4-6"),
            detected_agent("codex", "codex", "openai", "gpt-5.3-codex"),
            detected_agent("gemini", "gemini", "google", "gemini-3.1-pro-preview"),
        ];

        let yaml = generate_config_for_agents(&agents);
        let config = load_generated_config(&yaml);

        assert_eq!(
            config.lanes["claude-supervisor"]
                .model
                .as_ref()
                .unwrap()
                .name,
            "claude-sonnet-4-6"
        );
        assert_eq!(
            config.lanes["codex-worker"].model.as_ref().unwrap().name,
            "gpt-5.3-codex"
        );
        assert_eq!(
            config.lanes["codex-hardening"].model.as_ref().unwrap().name,
            "gpt-5.5"
        );
        assert_eq!(
            config.lanes["codex-hardening"].reasoning_effort.as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            config.lanes["claude-reviewer"].model.as_ref().unwrap().name,
            "claude-opus-4-6"
        );
        assert_eq!(
            config.lanes["codex-reviewer"].model.as_ref().unwrap().name,
            "gpt-5.4"
        );
        assert_eq!(config.orchestration.max_active_workers, 5);
    }

    #[test]
    fn generated_config_only_includes_needed_custom_catalog_entries() {
        let agents = vec![detected_agent(
            "kimi",
            "kimi",
            "kimi-code",
            "kimi-for-coding",
        )];
        let yaml = generate_config_for_agents(&agents);
        let config = load_generated_config(&yaml);

        assert!(
            yaml.contains("  kimi:"),
            "active non-default launcher should be included"
        );
        assert!(
            yaml.contains("  kimi-worker:"),
            "active non-default lane should be included"
        );
        assert!(
            !yaml.contains("claude-ollama-cloud"),
            "unused template catalog entries should not be generated"
        );
        assert!(config.launchers.contains_key("kimi"));
        assert!(config.lanes.contains_key("kimi-worker"));
        assert_eq!(config.roles.workers[0].lane, "kimi-worker");
    }

    #[test]
    fn generated_config_is_compact_overlay() {
        let yaml = generate_config_for_agents(&[]);

        assert!(yaml.lines().count() < 80, "{yaml}");
        assert!(!yaml.contains("budget:"));
        assert!(!yaml.contains("retention:"));
        assert!(!yaml.contains("terminal_host:"));
        let config = load_generated_config(&yaml);
        assert_eq!(config.version, 1);
        assert!(config.launchers.contains_key("claude"));
    }

    #[test]
    fn generated_config_tailors_agy() {
        let agents = vec![detected_agent(
            "agy",
            "agy",
            "antigravity",
            "antigravity-2.0",
        )];
        let yaml = generate_config_for_agents(&agents);
        let config = load_generated_config(&yaml);

        assert_eq!(config.roles.supervisor.name, "agy-supervisor");
        assert_eq!(config.roles.workers[0].lane, "agy-worker");
        assert_eq!(
            config.review.default_reviewers,
            vec!["agy-reviewer".to_string()]
        );
        assert_eq!(config.launchers["agy"].adapter, AdapterKind::Agy);
        assert_eq!(config.launchers["agy"].command, Some("agy".to_string()));
        assert_eq!(
            config.launchers["agy"].args,
            vec!["--dangerously-skip-permissions".to_string()]
        );
        assert_eq!(
            config.lanes["agy-worker"].model.as_ref().unwrap().name,
            "antigravity-2.0"
        );
    }

    #[test]
    fn init_gitignore_adds_agy_local_patterns() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join(".gitignore"), "target/\n.brehon\n")
            .expect("write gitignore");

        assert!(update_gitignore(temp.path()).expect("update gitignore"));
        let gitignore =
            std::fs::read_to_string(temp.path().join(".gitignore")).expect("read gitignore");

        assert!(gitignore.contains("target/"));
        assert_eq!(gitignore.matches(".brehon").count(), 1, "{gitignore}");
        assert!(gitignore.contains(".agents/mcp_config.json"));
        assert!(gitignore.contains(".antigravitycli"));

        assert!(!update_gitignore(temp.path()).expect("second update"));
    }

    #[test]
    fn ensure_agy_mcp_config_creates_workspace_config_and_preserves_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join(AGY_PROJECT_MCP_CONFIG_PATH);
        std::fs::create_dir_all(config_path.parent().unwrap()).expect("create .agents");
        std::fs::write(
            &config_path,
            r#"{"mcpServers":{"other":{"command":"other","args":["serve"]}}}"#,
        )
        .expect("write existing mcp config");

        assert!(ensure_agy_mcp_config(temp.path()).expect("ensure agy mcp config"));
        let config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&config_path).expect("read agy mcp config"),
        )
        .expect("parse agy mcp config");

        assert_eq!(config["mcpServers"]["other"]["command"], "other");
        assert_eq!(
            config["mcpServers"]["brehon"]["args"],
            serde_json::json!(["serve"])
        );
        assert!(config["mcpServers"]["brehon"]["command"]
            .as_str()
            .is_some_and(|command| !command.is_empty()));
    }
}
