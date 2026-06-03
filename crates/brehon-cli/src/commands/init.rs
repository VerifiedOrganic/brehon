use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

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
    ("grok", "grok", "Grok Build CLI"),
];

const AGY_PROJECT_MCP_CONFIG_PATH: &str = ".agents/mcp_config.json";
const BREHON_INIT_EXTRA_GITIGNORE_PATTERNS: &[&str] =
    &[".agents/mcp_config.json", ".antigravitycli"];

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

/// Catalog entry describing how to spawn an agent CLI and which models its
/// supervisor/worker/reviewer lanes should use. The generator emits a launcher
/// plus all three lanes for Claude (always) and for every other agent detected
/// on PATH. Only Claude is wired into the active roles; the rest sit ready to
/// be flipped on. Keep the model defaults in sync with `defaults.yaml`.
struct AgentTemplate {
    /// KNOWN_AGENTS config name used for PATH detection.
    detect_name: &'static str,
    /// Launcher key and lane prefix (e.g. `codex` -> `codex-worker`).
    launcher: &'static str,
    adapter: &'static str,
    command: &'static str,
    args: &'static [&'static str],
    provider: &'static str,
    supervisor_model: &'static str,
    worker_model: &'static str,
    reviewer_model: &'static str,
    /// Reasoning effort per role; `None` omits the field (e.g. Gemini, Antigravity).
    supervisor_effort: Option<&'static str>,
    worker_effort: Option<&'static str>,
    reviewer_effort: Option<&'static str>,
}

const CATALOG: &[AgentTemplate] = &[
    AgentTemplate {
        detect_name: "claude-code",
        launcher: "claude",
        adapter: "Acp",
        command: "claude",
        args: &[],
        provider: "anthropic",
        supervisor_model: "claude-opus-4-6",
        worker_model: "claude-sonnet-4-6",
        reviewer_model: "claude-opus-4-6",
        supervisor_effort: Some("high"),
        worker_effort: Some("medium"),
        reviewer_effort: Some("high"),
    },
    AgentTemplate {
        detect_name: "codex",
        launcher: "codex",
        adapter: "Acp",
        command: "codex",
        args: &["app-server"],
        provider: "openai",
        supervisor_model: "gpt-5.4",
        worker_model: "gpt-5.3-codex",
        reviewer_model: "gpt-5.4",
        supervisor_effort: Some("high"),
        worker_effort: Some("medium"),
        reviewer_effort: Some("high"),
    },
    AgentTemplate {
        detect_name: "copilot",
        launcher: "copilot",
        adapter: "Acp",
        command: "copilot",
        args: &[],
        provider: "github-copilot",
        supervisor_model: "gpt-5",
        worker_model: "gpt-5",
        reviewer_model: "gpt-5",
        supervisor_effort: Some("high"),
        worker_effort: Some("medium"),
        reviewer_effort: Some("high"),
    },
    AgentTemplate {
        detect_name: "gemini",
        launcher: "gemini",
        adapter: "Acp",
        command: "gemini",
        args: &["--acp"],
        provider: "google",
        supervisor_model: "gemini-3.1-pro-preview",
        worker_model: "gemini-3.1-pro-preview",
        reviewer_model: "gemini-3.1-pro-preview",
        supervisor_effort: None,
        worker_effort: None,
        reviewer_effort: None,
    },
    AgentTemplate {
        detect_name: "kimi",
        launcher: "kimi",
        adapter: "Acp",
        command: "kimi",
        args: &["acp"],
        provider: "kimi-code",
        supervisor_model: "kimi-for-coding",
        worker_model: "kimi-for-coding",
        reviewer_model: "kimi-for-coding",
        supervisor_effort: Some("high"),
        worker_effort: Some("medium"),
        reviewer_effort: Some("high"),
    },
    AgentTemplate {
        detect_name: "opencode",
        launcher: "opencode",
        adapter: "Acp",
        command: "opencode",
        args: &["acp", "--cwd", "."],
        provider: "ollama-cloud",
        supervisor_model: "glm-5.1",
        worker_model: "glm-5.1",
        reviewer_model: "glm-5.1",
        supervisor_effort: Some("high"),
        worker_effort: Some("medium"),
        reviewer_effort: Some("high"),
    },
    AgentTemplate {
        detect_name: "agy",
        launcher: "agy",
        adapter: "Agy",
        command: "agy",
        args: &[],
        provider: "google",
        supervisor_model: "antigravity-2.0",
        worker_model: "antigravity-2.0",
        reviewer_model: "antigravity-2.0",
        supervisor_effort: None,
        worker_effort: None,
        reviewer_effort: None,
    },
    AgentTemplate {
        detect_name: "grok",
        launcher: "grok",
        adapter: "Acp",
        command: "grok",
        args: &["agent", "--always-approve", "stdio"],
        provider: "xai",
        supervisor_model: "grok-build",
        worker_model: "grok-build",
        reviewer_model: "grok-build",
        supervisor_effort: Some("high"),
        worker_effort: Some("medium"),
        reviewer_effort: Some("high"),
    },
];

/// Inline prompt for the active Claude worker lane. (Other agents' worker lanes
/// are left promptless — workers lean on the `brehon-worker` skill — until the
/// user activates and tunes them.)
const WORKER_PROMPT: &str = "\
You are an implementation worker on a Brehon team.
Execute the single task assigned to you, working only inside your own git
worktree, never the shared repo root. Make the change, add or update its tests,
and run them before you hand off. Keep the diff tight: no drive-by refactors.
Report real progress, and complete the task through the Brehon task tool so it
enters review. If you get stuck, mark it blocked and message the supervisor.";

/// Inline prompt applied to every reviewer lane (active or not) so flipping a
/// reviewer on is a one-line roles edit with no missing review behavior.
const REVIEWER_PROMPT: &str = "\
You are a reviewer. Evaluate the submitted work; do not implement it.
Judge correctness, test coverage, and clarity against the task's intent, and
attach every concern as a structured finding (real nitpicks at nitpick
severity). Treat missing tests as a real gap unless the task waives them.
A review is complete only after a successful structured review submission.";

/// Top-of-file comment block. Dynamic "detected on PATH" lines are appended
/// after this, then the generated YAML body.
const CONFIG_HEADER: &str = "\
# Brehon Project Configuration
# =============================
#
# This is a partial overlay. Settings here win over the built-in defaults and
# your global ~/.config/brehon/config.yaml. Run `brehon config list` to see the
# fully resolved config.
#
# Layering (lowest to highest precedence):
#   baked-in defaults  <  ~/.config/brehon/config.yaml  <  this file
#
# Budget-safe by default: exactly ONE active Claude worker, a single-member
# Claude review panel, and a Claude supervisor. Every agent CLI found on your
# PATH also gets a launcher + supervisor/worker/reviewer lanes defined below,
# but INACTIVE -- turning one on is a few lines in `roles`/`review`. See the
# TURN ON ANOTHER AGENT section near the bottom.
#
";

/// Static roles/review/orchestration block — Claude is the only active role.
const ACTIVE_ROLES_BLOCK: &str = "
# --- Roles (active) ---------------------------------------------------------
# Who actually runs. Only Claude is wired in; the other lanes above are idle
# until you add them here. See TURN ON ANOTHER AGENT below.
roles:
  supervisor:
    name: claude-supervisor
  workers:
    # One active worker. Raise `max` (and orchestration.max_active_workers) to
    # run several in parallel; each gets its own git worktree.
    - lane: claude-worker
      min: 1
      max: 1
  reviewers:
    - lane: claude-reviewer
      min: 1
      max: 1

# --- Review -----------------------------------------------------------------
review:
  policy:
    # One reviewer -> one approval. Raise this in lockstep with panel size.
    min_approvals: 1
  default_reviewers:
    - claude-reviewer
  panels:
    - id: primary
      reviewers:
        - claude-reviewer

# --- Orchestration ----------------------------------------------------------
orchestration:
  # Hard ceiling on workers running at once. Keep this >= the worker pool `max`.
  max_active_workers: 1
";

/// Commented routing example — appended to every generated config.
const ROUTING_BLOCK: &str = "
# ----------------------------------------------------------------------------
# ROUTING - send specific tasks to specific worker lanes at assignment time.
# An explicit per-task policy always wins; otherwise the first matching rule
# supplies the lane. Handy for sending scary work to a stronger/pricier lane.
# ----------------------------------------------------------------------------
#
#   routing:
#     default_worker_lane: claude-worker
#     escalation_lane: codex-worker
#     rules:
#       - id: high-risk-or-large
#         match:
#           text_any: [\"security\", \"release\", \"migration\", \"Imported size estimate: L\"]
#         policy:
#           preferred_lane: codex-worker
#           strict: false
";

fn push_launcher(out: &mut String, template: &AgentTemplate) {
    out.push_str(&format!(
        "  {}:\n    adapter: {}\n    command: {}\n",
        template.launcher, template.adapter, template.command
    ));
    if !template.args.is_empty() {
        out.push_str("    args:\n");
        for arg in template.args {
            out.push_str(&format!("      - \"{arg}\"\n"));
        }
    }
}

fn push_lane(
    out: &mut String,
    name: &str,
    launcher: &str,
    provider: &str,
    model: &str,
    effort: Option<&str>,
    prompt: Option<&str>,
) {
    out.push_str(&format!(
        "  {name}:\n    launcher: {launcher}\n    model:\n      provider: {provider}\n      name: {model}\n"
    ));
    if let Some(effort) = effort {
        out.push_str(&format!("    reasoning_effort: {effort}\n"));
    }
    if let Some(prompt) = prompt {
        out.push_str("    system_prompt: |\n");
        for line in prompt.lines() {
            out.push_str(&format!("      {line}\n"));
        }
    }
}

fn push_lanes(out: &mut String, template: &AgentTemplate) {
    let is_claude = template.launcher == "claude";
    push_lane(
        out,
        &format!("{}-supervisor", template.launcher),
        template.launcher,
        template.provider,
        template.supervisor_model,
        template.supervisor_effort,
        None,
    );
    push_lane(
        out,
        &format!("{}-worker", template.launcher),
        template.launcher,
        template.provider,
        template.worker_model,
        template.worker_effort,
        is_claude.then_some(WORKER_PROMPT),
    );
    push_lane(
        out,
        &format!("{}-reviewer", template.launcher),
        template.launcher,
        template.provider,
        template.reviewer_model,
        template.reviewer_effort,
        Some(REVIEWER_PROMPT),
    );
}

/// Append the "how to activate another agent" guidance. When other agents were
/// detected, list their ready-to-flip lanes and the exact edits. Otherwise show
/// a from-scratch example so single-Claude users learn the pattern too.
fn push_turn_on(out: &mut String, others: &[&AgentTemplate]) {
    out.push_str(
        "\n# ============================================================================\n",
    );
    if let Some(example) = others.first() {
        let x = example.launcher;
        let ready: Vec<String> = others
            .iter()
            .map(|template| format!("{0}-supervisor/{0}-worker/{0}-reviewer", template.launcher))
            .collect();
        out.push_str(&format!(
            "# TURN ON ANOTHER AGENT\n\
             # The lanes for every CLI on your PATH are defined above but inactive. To\n\
             # activate one (example: {x}), make these edits:\n\
             #   roles.workers:                add `- {{ lane: {x}-worker, min: 1, max: 1 }}`\n\
             #   review.panels[0].reviewers:   add `{x}-reviewer`\n\
             #   review.default_reviewers:     add `{x}-reviewer`\n\
             #   review.policy.min_approvals:  raise to match the panel size (2 reviewers -> 2)\n\
             #   orchestration.max_active_workers: raise if you added parallel workers\n\
             #   (optional) roles.supervisor.name: {x}-supervisor   # swap the coordinator\n\
             #\n\
             # Defined and ready to flip on: {ready}\n\
             # Reviewer lanes already carry a review prompt; add a system_prompt to a\n\
             # worker lane if you want one (copy claude-worker's above).\n",
            ready = ready.join(", ")
        ));
    } else {
        out.push_str(
            "# ADD ANOTHER AGENT\n\
             # Only the `claude` CLI was found on your PATH, so only Claude lanes are\n\
             # defined. Install another agent CLI and re-run `brehon init`, or add one by\n\
             # hand -- uncomment and adapt:\n\
             #\n\
             #   launchers:\n\
             #     codex: { adapter: Acp, command: codex, args: [\"app-server\"] }\n\
             #   lanes:\n\
             #     codex-worker:\n\
             #       launcher: codex\n\
             #       model: { provider: openai, name: gpt-5.4 }\n\
             #       reasoning_effort: medium\n\
             #     codex-reviewer:\n\
             #       launcher: codex\n\
             #       model: { provider: openai, name: gpt-5.4 }\n\
             #       reasoning_effort: high\n\
             #       system_prompt: |\n\
             #         You are a reviewer. Evaluate the submitted work; do not implement it.\n\
             #   roles:\n\
             #     workers:   [ { lane: claude-worker, min: 1, max: 1 }, { lane: codex-worker, min: 1, max: 1 } ]\n\
             #     reviewers: [ { lane: claude-reviewer, min: 1, max: 1 }, { lane: codex-reviewer, min: 1, max: 1 } ]\n\
             #   review:\n\
             #     policy: { min_approvals: 2 }\n\
             #     default_reviewers: [claude-reviewer, codex-reviewer]\n\
             #     panels: [ { id: primary, reviewers: [claude-reviewer, codex-reviewer] } ]\n",
        );
    }
    out.push_str(
        "# ============================================================================\n",
    );
}

/// Generate a starter overlay: Claude is the only active role, while every
/// agent CLI detected on PATH also gets launcher + lanes defined (inactive) so
/// it can be turned on with a small `roles`/`review` edit.
fn generate_config_for_agents(agents: &[DetectedAgent]) -> String {
    let found: HashSet<&str> = agents
        .iter()
        .filter(|agent| agent.check.found)
        .map(|agent| agent.check.name.as_str())
        .collect();

    // Active catalog: Claude always, plus any detected agent, in catalog order.
    let active: Vec<&AgentTemplate> = CATALOG
        .iter()
        .filter(|template| template.launcher == "claude" || found.contains(template.detect_name))
        .collect();
    let others: Vec<&AgentTemplate> = active
        .iter()
        .copied()
        .filter(|template| template.launcher != "claude")
        .collect();

    let mut out = String::new();
    out.push_str(CONFIG_HEADER);

    let detected: Vec<&str> = CATALOG
        .iter()
        .map(|template| template.detect_name)
        .filter(|name| found.contains(name))
        .collect();
    if detected.is_empty() {
        out.push_str("# Agent CLIs detected on PATH: none.\n");
    } else {
        out.push_str(&format!(
            "# Agent CLIs detected on PATH: {}.\n",
            detected.join(", ")
        ));
    }
    if !found.contains("claude-code") {
        out.push_str(
            "# NOTE: the `claude` CLI was not found on PATH. Install it before running\n\
             #       `brehon`, or point the `claude` launcher below at your CLI.\n",
        );
    }

    out.push_str("\nversion: 1\n");

    out.push_str(
        "\n# --- Launchers --------------------------------------------------------------\n\
         # A launcher is HOW to spawn an agent CLI.\n\
         launchers:\n",
    );
    for template in &active {
        push_launcher(&mut out, template);
    }

    out.push_str(
        "\n# --- Lanes ------------------------------------------------------------------\n\
         # A lane bundles a launcher + model + reasoning effort + (optional) system\n\
         # prompt. Roles point at lanes, so you can swap the model behind a role\n\
         # without touching the role. Non-Claude lanes below are defined but unused.\n\
         lanes:\n",
    );
    for (index, template) in active.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        push_lanes(&mut out, template);
    }

    out.push_str(ACTIVE_ROLES_BLOCK);
    push_turn_on(&mut out, &others);
    out.push_str(ROUTING_BLOCK);

    out
}

fn gitignore_pattern_present(lines: &[&str], pattern: &str) -> bool {
    lines.iter().any(|line| line.trim() == pattern)
}

/// Update .gitignore to include Brehon and machine-local agent files.
fn update_gitignore(project_path: &Path) -> Result<bool> {
    let gitignore_path = project_path.join(".gitignore");
    let content = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };
    let (mut new_content, removed_legacy) = brehon_git::remove_legacy_brehon_dir_ignores(&content);
    let lines = new_content.lines().collect::<Vec<_>>();
    let missing = brehon_git::WORKTREE_AWARE_BREHON_IGNORE_PATTERNS
        .iter()
        .chain(BREHON_INIT_EXTRA_GITIGNORE_PATTERNS.iter())
        .filter(|pattern| !gitignore_pattern_present(&lines, pattern))
        .copied()
        .collect::<Vec<_>>();

    if missing.is_empty() && !removed_legacy {
        return Ok(false);
    }

    if !missing.is_empty() {
        let header_present = new_content
            .lines()
            .any(|l| l.trim() == "# Brehon orchestration data");

        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        if !header_present {
            if !new_content.is_empty() {
                new_content.push('\n');
            }
            new_content.push_str("# Brehon orchestration data\n");
        }
        for pattern in missing {
            new_content.push_str(pattern);
            new_content.push('\n');
        }
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

    let claude_found = agents
        .iter()
        .any(|agent| agent.check.name == "claude-code" && agent.check.found);
    let other_found = agents
        .iter()
        .filter(|agent| agent.check.found && agent.check.name != "claude-code")
        .count();
    println!();
    if claude_found {
        println!(
            "    {}",
            ui::dim(
                "Active roster: one Claude worker, one Claude reviewer, one Claude supervisor."
            )
        );
    } else {
        ui::print_warning("The 'claude' CLI was not found on PATH.");
        println!(
            "    {}",
            ui::dim(
                "A Claude-active starter config will still be written. Install the claude CLI, or \
                 edit .brehon/config.yaml to activate an agent you have."
            )
        );
    }
    if other_found > 0 {
        println!(
            "    {}",
            ui::dim(&format!(
                "{} other CLI{} also templated (inactive) -- see 'TURN ON ANOTHER AGENT' in the config.",
                other_found,
                if other_found == 1 { "" } else { "s" }
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

    // Write config
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
            "Read {} - Claude is active; other agents are templated and ready to flip on",
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

    fn load_generated_config(yaml: &str) -> brehon_types::BrehonConfig {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, yaml).expect("write generated config");
        brehon_config::load_config_with_override(None, Some(&path)).expect("load generated config")
    }

    fn detected_agent(name: &str, command: &str) -> DetectedAgent {
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

    fn active_lines(yaml: &str) -> Vec<&str> {
        yaml.lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect()
    }

    #[test]
    fn claude_is_the_only_active_role() {
        let yaml = generate_config_for_agents(&[
            detected_agent("claude-code", "claude"),
            detected_agent("codex", "codex"),
            detected_agent("gemini", "gemini"),
        ]);
        let config = load_generated_config(&yaml);

        assert_eq!(config.roles.supervisor.name, "claude-supervisor");
        assert_eq!(config.roles.workers.len(), 1);
        assert_eq!(config.roles.workers[0].lane, "claude-worker");
        assert_eq!(config.roles.reviewers.len(), 1);
        assert_eq!(config.roles.reviewers[0].lane, "claude-reviewer");

        assert_eq!(
            config.review.default_reviewers,
            vec!["claude-reviewer".to_string()]
        );
        assert_eq!(config.review.panels.len(), 1);
        assert_eq!(config.review.panels[0].id, "primary");
        assert_eq!(
            config.review.panels[0].reviewers,
            vec!["claude-reviewer".to_string()]
        );
        assert_eq!(config.review.policy.min_approvals, 1);
        assert_eq!(config.orchestration.max_active_workers, 1);
    }

    #[test]
    fn detected_agents_are_templated_but_inactive() {
        let yaml = generate_config_for_agents(&[
            detected_agent("claude-code", "claude"),
            detected_agent("codex", "codex"),
        ]);
        let config = load_generated_config(&yaml);

        // Codex lanes + launcher are defined and uncommented...
        for lane in ["codex-supervisor", "codex-worker", "codex-reviewer"] {
            assert!(config.lanes.contains_key(lane), "missing lane {lane}");
        }
        assert!(config.launchers.contains_key("codex"));
        assert!(active_lines(&yaml)
            .iter()
            .any(|line| line.contains("codex-worker:")));

        // ...but not wired into any active role.
        assert!(config
            .roles
            .workers
            .iter()
            .all(|w| w.lane != "codex-worker"));
        assert!(config
            .roles
            .reviewers
            .iter()
            .all(|r| r.lane != "codex-reviewer"));

        // Codex models match the catalog.
        assert_eq!(
            config.lanes["codex-worker"].model.as_ref().unwrap().name,
            "gpt-5.3-codex"
        );
        assert_eq!(
            config.lanes["codex-reviewer"].model.as_ref().unwrap().name,
            "gpt-5.4"
        );
    }

    #[test]
    fn claude_lanes_use_expected_models() {
        let yaml = generate_config_for_agents(&[detected_agent("claude-code", "claude")]);
        let config = load_generated_config(&yaml);

        assert_eq!(
            config.lanes["claude-supervisor"]
                .model
                .as_ref()
                .unwrap()
                .name,
            "claude-opus-4-6"
        );
        assert_eq!(
            config.lanes["claude-worker"].model.as_ref().unwrap().name,
            "claude-sonnet-4-6"
        );
        assert_eq!(
            config.lanes["claude-reviewer"].model.as_ref().unwrap().name,
            "claude-opus-4-6"
        );
    }

    #[test]
    fn worker_and_reviewer_prompts_are_inlined() {
        let yaml = generate_config_for_agents(&[
            detected_agent("claude-code", "claude"),
            detected_agent("codex", "codex"),
        ]);
        let config = load_generated_config(&yaml);

        let worker_prompt = config
            .lane_system_prompt("claude-worker", None)
            .expect("worker prompt");
        assert!(worker_prompt.contains("implementation worker"));
        assert!(worker_prompt.contains("worktree"));

        // Every reviewer lane (active or not) carries the review prompt.
        for lane in ["claude-reviewer", "codex-reviewer"] {
            let prompt = config
                .lane_system_prompt(lane, None)
                .unwrap_or_else(|| panic!("reviewer prompt for {lane}"));
            assert!(prompt.contains("You are a reviewer."));
            assert!(prompt.contains("structured review submission"));
        }
    }

    #[test]
    fn no_agents_defines_only_claude_and_warns() {
        let yaml = generate_config_for_agents(&[]);
        let config = load_generated_config(&yaml);

        assert_eq!(config.version, 1);
        assert!(config.launchers.contains_key("claude"));

        // No detected agents -> only Claude lanes are emitted (codex appears only
        // inside the commented ADD example).
        assert!(!active_lines(&yaml)
            .iter()
            .any(|line| line.contains("codex-worker:")));
        assert!(yaml.contains("`claude` CLI was not found"));
    }

    #[test]
    fn header_lists_detected_agents() {
        let yaml = generate_config_for_agents(&[
            detected_agent("claude-code", "claude"),
            detected_agent("codex", "codex"),
        ]);

        assert!(yaml.contains("Agent CLIs detected on PATH: claude-code, codex."));
        assert!(
            !yaml.contains("`claude` CLI was not found"),
            "no missing-claude warning when claude is present"
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
        let lines = gitignore
            .lines()
            .map(str::trim)
            .collect::<std::collections::HashSet<_>>();

        assert!(gitignore.contains("target/"));
        assert!(!lines.contains(".brehon"), "{gitignore}");
        assert!(lines.contains(".brehon/"), "{gitignore}");
        for pattern in brehon_git::WORKTREE_AWARE_BREHON_IGNORE_PATTERNS {
            assert!(lines.contains(*pattern), "{pattern} missing: {gitignore}");
        }
        for pattern in BREHON_INIT_EXTRA_GITIGNORE_PATTERNS {
            assert!(lines.contains(*pattern), "{pattern} missing: {gitignore}");
        }

        assert!(!update_gitignore(temp.path()).expect("second update"));
    }

    #[test]
    fn init_gitignore_no_duplicate_header_when_partially_present() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Pre-populate with one Brehon pattern and the header already present.
        std::fs::write(
            temp.path().join(".gitignore"),
            "target/\n# Brehon orchestration data\n.brehon/\n",
        )
        .expect("write gitignore");

        assert!(update_gitignore(temp.path()).expect("update gitignore"));
        let gitignore =
            std::fs::read_to_string(temp.path().join(".gitignore")).expect("read gitignore");

        let header_count = gitignore
            .lines()
            .filter(|l| l.trim() == "# Brehon orchestration data")
            .count();
        assert_eq!(header_count, 1, "duplicate header found:\n{gitignore}");

        // Patterns should land contiguously under the existing header - no blank
        // line gap between the already-present `.brehon/` and newly appended lines.
        let all_lines: Vec<_> = gitignore.lines().collect();
        let header_idx = all_lines
            .iter()
            .position(|l| l.trim() == "# Brehon orchestration data")
            .expect("header exists");
        let brehon_block = &all_lines[header_idx..];
        let blank_inside_block = brehon_block
            .windows(2)
            .any(|w| w[0].trim() == ".brehon/" && w[1].trim().is_empty());
        assert!(
            !blank_inside_block,
            "blank line inside Brehon block:\n{gitignore}"
        );

        // All patterns should now be present.
        let lines: std::collections::HashSet<_> = gitignore.lines().map(str::trim).collect();
        for pattern in brehon_git::WORKTREE_AWARE_BREHON_IGNORE_PATTERNS {
            assert!(lines.contains(*pattern), "{pattern} missing: {gitignore}");
        }
        for pattern in BREHON_INIT_EXTRA_GITIGNORE_PATTERNS {
            assert!(lines.contains(*pattern), "{pattern} missing: {gitignore}");
        }
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
