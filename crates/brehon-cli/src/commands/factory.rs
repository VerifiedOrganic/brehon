use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::names;

#[derive(Debug, Clone, clap::Args)]
pub struct SpawnArgs {
    /// Number of workers to spawn
    #[arg(long, default_value = "1")]
    pub count: usize,

    /// Agent type to spawn (worker or reviewer)
    #[arg(long, default_value = "worker")]
    pub role: String,

    /// Agent CLI to use (claude, codex, gemini, opencode)
    #[arg(long, default_value = "claude")]
    pub agent: String,

    /// Model to use for the agent
    #[arg(long)]
    pub model: Option<String>,

    /// Working directory for spawned agents (defaults to current directory)
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Print spawn plan without creating PTY sessions
    #[arg(long)]
    pub dry_run: bool,

    /// Output spawn plan as JSON to stdout
    #[arg(long, conflicts_with = "dry_run")]
    pub dry_run_json: bool,
}

#[derive(Debug, Serialize)]
struct SpawnPlan {
    count: usize,
    role: String,
    agent_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    working_directory: String,
    workers: Vec<WorkerInfo>,
}

#[derive(Debug, Serialize)]
struct WorkerInfo {
    name: String,
    agent_type: String,
    role: String,
    model: Option<String>,
    working_directory: String,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum FactoryCommand {
    #[command(name = "spawn")]
    Spawn(SpawnArgs),
}

pub fn execute(args: &FactoryCommand) -> Result<()> {
    match args {
        FactoryCommand::Spawn(spawn_args) => execute_spawn(spawn_args),
    }
}

fn execute_spawn(args: &SpawnArgs) -> Result<()> {
    let cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let role = args.role.to_lowercase();
    if role != "worker" && role != "reviewer" {
        anyhow::bail!(
            "Invalid role '{}'. Must be 'worker' or 'reviewer'.",
            args.role
        );
    }

    let agent_type = args.agent.to_lowercase();
    let valid_agents = ["claude", "claude-code", "codex", "gemini", "opencode"];
    if !valid_agents.contains(&agent_type.as_str()) {
        anyhow::bail!(
            "Invalid agent '{}'. Must be one of: {}",
            args.agent,
            valid_agents.join(", ")
        );
    }

    let generated_names = names::generate_names(args.count);

    if args.dry_run {
        println!("Spawn plan:");
        println!("  Working directory: {}", cwd.display());
        println!("  Agent type: {}", agent_type);
        println!("  Role: {}", role);
        if let Some(ref model) = args.model {
            println!("  Model: {}", model);
        }
        println!();
        println!("Agents to spawn ({}):", args.count);
        for name in &generated_names {
            println!("  - Name: {}", name);
            println!("    Type: {}", agent_type);
            println!("    Role: {}", role);
            if let Some(ref model) = args.model {
                println!("    Model: {}", model);
            }
            println!("    Working directory: {}", cwd.display());
        }
    } else if args.dry_run_json {
        let json = build_spawn_plan_json(args, &cwd, &role, &agent_type, &generated_names)?;
        println!("{}", json);
    } else {
        anyhow::bail!(
            "Factory spawn requires a running TUI session. Use 'brehon' to start the TUI first."
        );
    }

    Ok(())
}

fn build_spawn_plan_json(
    args: &SpawnArgs,
    cwd: &std::path::Path,
    role: &str,
    agent_type: &str,
    generated_names: &[String],
) -> Result<String> {
    let workers: Vec<WorkerInfo> = generated_names
        .iter()
        .map(|name| WorkerInfo {
            name: name.clone(),
            agent_type: agent_type.to_string(),
            role: role.to_string(),
            model: args.model.clone(),
            working_directory: cwd.display().to_string(),
        })
        .collect();

    let plan = SpawnPlan {
        count: generated_names.len(),
        role: role.to_string(),
        agent_type: agent_type.to_string(),
        model: args.model.clone(),
        working_directory: cwd.display().to_string(),
        workers,
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_json_produces_valid_json() {
        let args = SpawnArgs {
            count: 3,
            role: "worker".to_string(),
            agent: "claude".to_string(),
            model: None,
            cwd: None,
            dry_run: false,
            dry_run_json: true,
        };

        let result = execute_spawn(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn dry_run_json_has_required_fields() {
        let args = SpawnArgs {
            count: 2,
            role: "worker".to_string(),
            agent: "claude".to_string(),
            model: None,
            cwd: None,
            dry_run: false,
            dry_run_json: true,
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let names = names::generate_names(args.count);
        let json = build_spawn_plan_json(&args, &cwd, "worker", "claude", &names).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(plan.get("count").is_some(), "missing 'count' field");
        assert!(plan.get("role").is_some(), "missing 'role' field");
        assert!(
            plan.get("agent_type").is_some(),
            "missing 'agent_type' field"
        );
        assert!(
            plan.get("working_directory").is_some(),
            "missing 'working_directory' field"
        );
        assert!(plan.get("workers").is_some(), "missing 'workers' field");

        assert_eq!(plan["count"], 2);
        assert_eq!(plan["role"], "worker");
        assert_eq!(plan["agent_type"], "claude");
    }

    #[test]
    fn workers_array_matches_count() {
        let args = SpawnArgs {
            count: 5,
            role: "worker".to_string(),
            agent: "claude".to_string(),
            model: None,
            cwd: None,
            dry_run: false,
            dry_run_json: true,
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let names = names::generate_names(args.count);
        let json = build_spawn_plan_json(&args, &cwd, "worker", "claude", &names).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();

        let workers = plan["workers"]
            .as_array()
            .expect("workers should be an array");
        assert_eq!(workers.len(), 5, "workers array length should match count");
        assert_eq!(
            plan["count"].as_u64().unwrap(),
            5,
            "count field should be 5"
        );
    }

    #[test]
    fn dry_run_json_conflicts_with_dry_run() {
        use clap::Command;

        let cmd = Command::new("test").subcommand_required(true).subcommand(
            Command::new("spawn")
                .arg(
                    clap::Arg::new("dry_run")
                        .long("dry-run")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    clap::Arg::new("dry_run_json")
                        .long("dry-run-json")
                        .action(clap::ArgAction::SetTrue)
                        .conflicts_with("dry_run"),
                ),
        );

        let result = cmd.try_get_matches_from(["test", "spawn", "--dry-run", "--dry-run-json"]);
        assert!(
            result.is_err(),
            "should reject both --dry-run and --dry-run-json"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot be used with") || err.contains("conflict"),
            "error message should mention conflict: {err}"
        );
    }

    #[test]
    fn optional_model_serialization_null_when_absent() {
        let args = SpawnArgs {
            count: 1,
            role: "worker".to_string(),
            agent: "claude".to_string(),
            model: None,
            cwd: None,
            dry_run: false,
            dry_run_json: true,
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let names = names::generate_names(args.count);
        let json = build_spawn_plan_json(&args, &cwd, "worker", "claude", &names).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            plan["model"].is_null(),
            "model should be null when not provided"
        );
    }

    #[test]
    fn optional_model_serialization_present_when_provided() {
        let args = SpawnArgs {
            count: 1,
            role: "worker".to_string(),
            agent: "claude".to_string(),
            model: Some("claude-3-opus".to_string()),
            cwd: None,
            dry_run: false,
            dry_run_json: true,
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let names = names::generate_names(args.count);
        let json = build_spawn_plan_json(&args, &cwd, "worker", "claude", &names).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            plan["model"], "claude-3-opus",
            "model should serialize provided value"
        );
    }

    #[test]
    fn worker_info_has_required_fields() {
        let args = SpawnArgs {
            count: 1,
            role: "reviewer".to_string(),
            agent: "codex".to_string(),
            model: Some("gpt-4".to_string()),
            cwd: None,
            dry_run: false,
            dry_run_json: true,
        };

        let cwd = std::env::current_dir().unwrap_or_default();
        let names = names::generate_names(args.count);
        let json = build_spawn_plan_json(&args, &cwd, "reviewer", "codex", &names).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();

        let worker = &plan["workers"][0];
        assert!(worker.get("name").is_some(), "worker missing 'name'");
        assert!(
            worker.get("agent_type").is_some(),
            "worker missing 'agent_type'"
        );
        assert!(worker.get("role").is_some(), "worker missing 'role'");
        assert!(worker.get("model").is_some(), "worker missing 'model'");
        assert!(
            worker.get("working_directory").is_some(),
            "worker missing 'working_directory'"
        );

        assert_eq!(worker["agent_type"], "codex");
        assert_eq!(worker["role"], "reviewer");
        assert_eq!(worker["model"], "gpt-4");
    }
}
