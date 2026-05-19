use std::ffi::OsString;
use std::path::Path;

use brehon_mcp::server::ContentBlock;
use brehon_mcp::tools::task_actions::TaskActionsTool;
use brehon_mcp::tools::Tool;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

#[derive(Debug, Clone, clap::Subcommand)]
pub enum TaskCommand {
    #[command(name = "create")]
    Create {
        #[arg(long)]
        title: String,

        #[arg(long, default_value = "task")]
        task_type: String,

        #[arg(long)]
        parent_id: Option<String>,

        #[arg(long)]
        description: Option<String>,

        #[arg(long)]
        priority: Option<String>,

        #[arg(long)]
        completion_mode: Option<String>,

        #[arg(long)]
        direct_to_main: bool,

        #[arg(long = "acceptance")]
        acceptance_criteria: Vec<String>,

        #[arg(long = "file-hint")]
        file_hints: Vec<String>,

        #[arg(long = "constraint")]
        constraints: Vec<String>,

        #[arg(long = "test")]
        test_requirements: Vec<String>,

        #[arg(long = "step")]
        plan_steps: Vec<String>,

        #[arg(long)]
        implementation_notes: Option<String>,
    },

    #[command(name = "ready")]
    Ready,

    #[command(name = "archive")]
    Archive {
        #[arg(long)]
        id: String,

        #[arg(long)]
        reason: Option<String>,

        #[arg(long)]
        recursive: bool,
    },
}

struct ScopedEnv {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnv {
    fn set(vars: &[(&'static str, String)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for (key, value) in vars {
            saved.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self { saved }
    }
}

fn put_non_empty_string(
    map: &mut serde_json::Map<String, Value>,
    key: &str,
    value: &Option<String>,
) {
    if let Some(value) = value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn put_non_empty_list(map: &mut serde_json::Map<String, Value>, key: &str, values: &[String]) {
    let values = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| Value::String(value.to_string()))
        .collect::<Vec<_>>();
    if !values.is_empty() {
        map.insert(key.to_string(), Value::Array(values));
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.saved.iter().rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn extract_text(result: &brehon_mcp::server::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

async fn call_task_tool(project_root: &Path, args: Value) -> Result<Value> {
    let brehon_root = project_root.join(".brehon");
    std::fs::create_dir_all(brehon_root.join("runtime").join("tasks"))?;
    let _env = ScopedEnv::set(&[
        ("BREHON_ROOT", brehon_root.display().to_string()),
        ("BREHON_PROJECT_ROOT", project_root.display().to_string()),
    ]);
    let tool = TaskActionsTool::new();
    let result = tool
        .execute(args)
        .await
        .map_err(|err| anyhow!("Task tool execution failed: {err}"))?;
    let text = extract_text(&result);
    if result.is_error.unwrap_or(false) {
        return Err(anyhow!(text));
    }
    serde_json::from_str(&text)
        .map_err(|err| anyhow!("Failed to parse task tool JSON response: {err}: {text}"))
}

pub async fn execute(project_path: &Path, command: &TaskCommand) -> Result<()> {
    match command {
        TaskCommand::Create {
            title,
            task_type,
            parent_id,
            description,
            priority,
            completion_mode,
            direct_to_main,
            acceptance_criteria,
            file_hints,
            constraints,
            test_requirements,
            plan_steps,
            implementation_notes,
        } => {
            let mut args = serde_json::Map::new();
            args.insert("action".into(), Value::String("create".into()));
            args.insert("title".into(), Value::String(title.clone()));
            args.insert("task_type".into(), Value::String(task_type.clone()));
            args.insert("role".into(), Value::String("supervisor".into()));
            args.insert("agent_name".into(), Value::String("brehon-cli".into()));
            if *direct_to_main {
                args.insert("direct_to_main".into(), Value::Bool(true));
            }
            put_non_empty_string(&mut args, "parent_id", parent_id);
            put_non_empty_string(&mut args, "description", description);
            put_non_empty_string(&mut args, "priority", priority);
            put_non_empty_string(&mut args, "completion_mode", completion_mode);
            put_non_empty_string(&mut args, "implementation_notes", implementation_notes);
            put_non_empty_list(&mut args, "acceptance_criteria", acceptance_criteria);
            put_non_empty_list(&mut args, "file_hints", file_hints);
            put_non_empty_list(&mut args, "constraints", constraints);
            put_non_empty_list(&mut args, "test_requirements", test_requirements);
            put_non_empty_list(&mut args, "plan_steps", plan_steps);

            let result = call_task_tool(project_path, Value::Object(args)).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        TaskCommand::Ready => {
            let result = call_task_tool(
                project_path,
                json!({
                    "action": "ready",
                    "role": "supervisor",
                    "agent_name": "brehon-cli"
                }),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        TaskCommand::Archive {
            id,
            reason,
            recursive,
        } => {
            let result = call_task_tool(
                project_path,
                json!({
                    "action": "archive",
                    "id": id,
                    "reason": reason.clone().unwrap_or_else(|| "Archived via brehon task archive".to_string()),
                    "recursive": recursive,
                    "role": "supervisor",
                    "agent_name": "brehon-cli"
                }),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }

    Ok(())
}
