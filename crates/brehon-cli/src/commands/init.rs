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

/// Top-of-file comment block. The dynamic "detected on PATH" lines are appended
/// after this and before [`CONFIG_BODY`].
const CONFIG_HEADER: &str = "\
# Brehon Project Configuration
# =============================
#
# This is a partial overlay. Settings here win over the built-in defaults and
# your global ~/.config/brehon/config.yaml. Keep it small: omit anything that
# should use the defaults. Run `brehon config list` to see the resolved config.
#
# Layering (lowest to highest precedence):
#   baked-in defaults  <  ~/.config/brehon/config.yaml  <  this file
#
# This starter is intentionally minimal and budget-safe: ONE Claude worker,
# reviewed by a single-member Claude panel, coordinated by a Claude supervisor.
# That is three Claude instances at most. Scale it up with the dials below and
# the commented EXTEND/ROUTING examples near the bottom of this file.
#
";

/// The actual YAML body plus the teaching comments. Kept free of `{}` so it can
/// live in a plain string constant (no `format!` brace-escaping needed).
const CONFIG_BODY: &str = r#"
version: 1

# --- Launchers --------------------------------------------------------------
# A launcher is HOW to spawn an agent CLI. You usually only need one.
launchers:
  claude:
    adapter: Acp
    command: claude

# --- Lanes ------------------------------------------------------------------
# A lane bundles a launcher + model + reasoning effort + (optional) system
# prompt. Roles point at lanes, never directly at launchers, so you can swap the
# model behind a role without touching the role itself.
lanes:
  claude-supervisor:
    launcher: claude
    model:
      provider: anthropic
      name: claude-opus-4-6
    reasoning_effort: high

  claude-worker:
    launcher: claude
    model:
      provider: anthropic
      name: claude-sonnet-4-6
    reasoning_effort: medium
    system_prompt: |
      You are an implementation worker on a Brehon team.
      Execute the single task assigned to you, and work only inside your own git
      worktree, never the shared repo root.
      Make the change, add or update tests for it, and run them before you hand
      off. Keep the diff tight: no drive-by refactors or unrelated cleanups.
      Report real progress as the work moves, and finish by completing the task
      through the Brehon task tool so it enters review. Prose alone does not
      update Brehon state.
      If you get stuck, mark the task blocked and message the supervisor instead
      of guessing or thrashing.

  claude-reviewer:
    launcher: claude
    model:
      provider: anthropic
      name: claude-opus-4-6
    reasoning_effort: high
    system_prompt: |
      You are a reviewer. Evaluate the submitted work; do not implement it.
      Read the diff against the task's intent and judge correctness, test
      coverage, and clarity.
      Score the work and attach every concern as a structured finding. Record
      real nitpicks at nitpick severity rather than waiving them away.
      Treat missing or insufficient tests as a real gap unless the task
      explicitly waives them.
      A review is complete only after a successful structured review submission;
      do not report idle while a review obligation remains.

# --- Roles ------------------------------------------------------------------
# Who does what. Each role points at one or more lanes.
roles:
  supervisor:
    name: claude-supervisor
  workers:
    # One worker. Raise `max` (and orchestration.max_active_workers below) to
    # run several workers in parallel; each gets its own git worktree.
    - lane: claude-worker
      min: 1
      max: 1
  reviewers:
    # One reviewer. Add more reviewer lanes here (and to review.panels) to form
    # a multi-model panel. See the EXTEND section below.
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

# ============================================================================
# EXTEND - uncomment and adapt to grow the team.
# ============================================================================
#
# Add a second model (e.g. Codex) as another worker AND a second reviewer:
#
#   launchers:
#     codex:
#       adapter: Acp
#       command: codex
#       args: ["app-server"]
#
#   lanes:
#     codex-worker:
#       launcher: codex
#       model:
#         provider: openai
#         name: gpt-5.4
#       reasoning_effort: medium
#     codex-reviewer:
#       launcher: codex
#       model:
#         provider: openai
#         name: gpt-5.4
#       reasoning_effort: high
#       system_prompt: |
#         You are a reviewer. Evaluate the submitted work; do not implement it.
#
#   roles:
#     workers:
#       - { lane: claude-worker, min: 1, max: 2 }
#       - { lane: codex-worker,  min: 1, max: 1 }
#     reviewers:
#       - { lane: claude-reviewer, min: 1, max: 1 }
#       - { lane: codex-reviewer,  min: 1, max: 1 }
#
#   review:
#     policy:
#       min_approvals: 2          # both reviewers must approve
#     default_reviewers: [claude-reviewer, codex-reviewer]
#     panels:
#       - id: primary
#         reviewers: [claude-reviewer, codex-reviewer]
#
#   orchestration:
#     max_active_workers: 3
#
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
#           text_any: ["security", "release", "migration", "Imported size estimate: L"]
#         policy:
#           preferred_lane: codex-worker
#           strict: false
"#;

/// Generate a clean, Claude-only starter overlay.
///
/// The output is intentionally minimal and budget-safe: one Claude worker,
/// reviewed by a single-member Claude panel, coordinated by a Claude supervisor.
/// Other agent CLIs are not wired into the roster; adding them is a copy-paste
/// away via the EXTEND/ROUTING examples baked into the generated file. The
/// detected agents are used only to annotate the header and warn if `claude`
/// is missing.
fn generate_config_for_agents(agents: &[DetectedAgent]) -> String {
    let detected: Vec<&str> = agents
        .iter()
        .filter(|agent| agent.check.found)
        .map(|agent| agent.check.name.as_str())
        .collect();

    let detected_comment = if detected.is_empty() {
        "# Agent CLIs detected on PATH: none.\n".to_string()
    } else {
        format!("# Agent CLIs detected on PATH: {}.\n", detected.join(", "))
    };

    let claude_note = if detected.iter().any(|name| *name == "claude-code") {
        String::new()
    } else {
        "# NOTE: the `claude` CLI was not found on PATH. Install it before running\n\
         #       `brehon`, or edit the `claude` launcher below to point at your CLI.\n"
            .to_string()
    };

    format!("{CONFIG_HEADER}{detected_comment}{claude_note}{CONFIG_BODY}")
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
    println!();
    if claude_found {
        println!(
            "    {}",
            ui::dim("Generating a single-Claude starter config (one worker, one reviewer).")
        );
    } else {
        ui::print_warning("The 'claude' CLI was not found on PATH.");
        println!(
            "    {}",
            ui::dim(
                "A Claude-only starter config will still be written. Install the claude CLI, or \
                 edit .brehon/config.yaml to point at the agent you use."
            )
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
            "Read {} - one Claude worker + one reviewer, with EXTEND examples",
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

    #[test]
    fn generated_config_is_single_claude_worker_and_reviewer() {
        let yaml = generate_config_for_agents(&[detected_agent("claude-code", "claude")]);
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
    fn generated_config_uses_claude_role_models() {
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
    fn generated_config_inlines_worker_and_reviewer_prompts() {
        let yaml = generate_config_for_agents(&[detected_agent("claude-code", "claude")]);
        let config = load_generated_config(&yaml);

        assert!(
            yaml.contains("system_prompt:"),
            "generated overlay should inline the role prompts"
        );

        let worker_prompt = config
            .lane_system_prompt("claude-worker", None)
            .expect("worker prompt");
        assert!(worker_prompt.contains("implementation worker"));
        assert!(worker_prompt.contains("worktree"));

        let reviewer_prompt = config
            .lane_system_prompt("claude-reviewer", None)
            .expect("reviewer prompt");
        assert!(reviewer_prompt.contains("You are a reviewer."));
        assert!(reviewer_prompt.contains("structured review submission"));
    }

    #[test]
    fn generated_config_is_claude_only_and_loads_without_agents() {
        let yaml = generate_config_for_agents(&[]);
        let config = load_generated_config(&yaml);

        assert_eq!(config.version, 1);
        assert!(config.launchers.contains_key("claude"));

        // The only uncommented worker/reviewer lanes are Claude's. Other lanes
        // (codex, gemini) appear only inside commented EXTEND examples.
        let active_lines: Vec<&str> = yaml
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect();
        assert!(active_lines.iter().any(|line| line.contains("claude-worker:")));
        assert!(active_lines
            .iter()
            .any(|line| line.contains("claude-reviewer:")));
        assert!(
            !active_lines.iter().any(|line| line.contains("codex-worker:")),
            "codex-worker should only appear in commented examples"
        );

        // With no claude on PATH, the file warns the user.
        assert!(yaml.contains("`claude` CLI was not found"));
    }

    #[test]
    fn generated_config_notes_detected_agents() {
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
