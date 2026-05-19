use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command;

const MAX_READ_LINES: usize = 400;
const MAX_LIST_FILES: usize = 2000;
const MAX_SEARCH_HITS: usize = 200;
const MAX_COMMAND_OUTPUT_BYTES: usize = 32 * 1024;
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 120;
const MAX_COMMAND_TIMEOUT_SECS: u64 = 600;

#[async_trait]
pub trait DirectToolBridge: Send + Sync {
    fn tool_definitions(&self) -> Vec<Value>;
    async fn invoke(&self, name: &str, args: Value) -> Result<String, String>;
}

pub trait DirectToolBridgeFactory: Send + Sync {
    fn build(
        &self,
        worktree_path: &str,
        env: &[(String, String)],
        tool_prefix: Option<&str>,
    ) -> Arc<dyn DirectToolBridge>;
}

pub struct CompositeToolBridge {
    bridges: Vec<Arc<dyn DirectToolBridge>>,
}

impl CompositeToolBridge {
    pub fn new(bridges: Vec<Arc<dyn DirectToolBridge>>) -> Arc<dyn DirectToolBridge> {
        Arc::new(Self { bridges })
    }
}

#[async_trait]
impl DirectToolBridge for CompositeToolBridge {
    fn tool_definitions(&self) -> Vec<Value> {
        self.bridges
            .iter()
            .flat_map(|bridge| bridge.tool_definitions())
            .collect()
    }

    async fn invoke(&self, name: &str, args: Value) -> Result<String, String> {
        for bridge in &self.bridges {
            if bridge.tool_definitions().iter().any(|definition| {
                definition
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    == Some(name)
            }) {
                return bridge.invoke(name, args).await;
            }
        }
        Err(format!("unsupported tool: {name}"))
    }
}

pub struct CodingToolBridge {
    worktree_root: PathBuf,
}

impl CodingToolBridge {
    pub fn new(worktree_root: PathBuf) -> Arc<dyn DirectToolBridge> {
        Arc::new(Self { worktree_root })
    }
}

#[async_trait]
impl DirectToolBridge for CodingToolBridge {
    fn tool_definitions(&self) -> Vec<Value> {
        vec![
            tool_definition(
                "list_files",
                "List files under the current worktree or a subdirectory.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Optional relative path under the worktree."
                        }
                    },
                    "required": []
                }),
            ),
            tool_definition(
                "search_text",
                "Search for text in files under the current worktree.",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Text or regex to search for."
                        },
                        "path": {
                            "type": "string",
                            "description": "Optional relative path under the worktree."
                        }
                    },
                    "required": ["pattern"]
                }),
            ),
            tool_definition(
                "read_file",
                "Read a file from the current worktree. Returns line-numbered text.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file."
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "Optional 1-based start line."
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "Optional 1-based end line."
                        }
                    },
                    "required": ["path"]
                }),
            ),
            tool_definition(
                "write_file",
                "Create or overwrite a text file in the current worktree.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file."
                        },
                        "content": {
                            "type": "string",
                            "description": "Full file contents to write."
                        }
                    },
                    "required": ["path", "content"]
                }),
            ),
            tool_definition(
                "replace_in_file",
                "Replace text in a file. Fails if the target text is ambiguous unless replace_all=true.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file."
                        },
                        "old": {
                            "type": "string",
                            "description": "Exact text to replace."
                        },
                        "new": {
                            "type": "string",
                            "description": "Replacement text."
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace every match instead of requiring a single unique match."
                        }
                    },
                    "required": ["path", "old", "new"]
                }),
            ),
            tool_definition(
                "bash",
                "Run a shell command in the current worktree and capture stdout/stderr.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute."
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Optional timeout in seconds. Defaults to 120 and is capped at 600."
                        }
                    },
                    "required": ["command"]
                }),
            ),
        ]
    }

    async fn invoke(&self, name: &str, args: Value) -> Result<String, String> {
        match name {
            "list_files" => list_files(&self.worktree_root, args).await,
            "search_text" => search_text(&self.worktree_root, args).await,
            "read_file" => read_file(&self.worktree_root, args).await,
            "write_file" => write_file(&self.worktree_root, args).await,
            "replace_in_file" => replace_in_file(&self.worktree_root, args).await,
            "bash" => run_command(&self.worktree_root, args).await,
            _ => Err(format!("unsupported tool: {name}")),
        }
    }
}

fn tool_definition(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

fn required_string(args: &Value, field: &str) -> Result<String, String> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing required string field '{field}'"))
}

fn optional_u64(args: &Value, field: &str) -> Option<u64> {
    args.get(field).and_then(Value::as_u64)
}

fn resolve_required_path(root: &Path, args: &Value, field: &str) -> Result<PathBuf, String> {
    let path = required_string(args, field)?;
    resolve_path(root, &path)
}

fn resolve_optional_path(root: &Path, value: Option<&Value>) -> Result<PathBuf, String> {
    match value.and_then(Value::as_str) {
        Some(path) if !path.trim().is_empty() => resolve_path(root, path),
        _ => Ok(root.to_path_buf()),
    }
}

fn resolve_path(root: &Path, input: &str) -> Result<PathBuf, String> {
    let candidate = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        root.join(input)
    };
    let normalized_root = normalize_path(root);
    let normalized_candidate = normalize_path(&candidate);
    if !normalized_candidate.starts_with(&normalized_root) {
        return Err(format!(
            "path '{input}' escapes the worktree '{}'",
            normalized_root.display()
        ));
    }
    Ok(normalized_candidate)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

async fn list_files(worktree_root: &Path, args: Value) -> Result<String, String> {
    let root = resolve_optional_path(worktree_root, args.get("path"))?;
    let relative = root
        .strip_prefix(worktree_root)
        .unwrap_or(&root)
        .to_string_lossy()
        .to_string();
    let target = if relative.is_empty() {
        ".".to_string()
    } else {
        relative
    };

    if let Ok(output) = run_simple_command(
        worktree_root,
        "rg",
        &["--files", target.as_str()],
        Duration::from_secs(30),
    )
    .await
    {
        return Ok(truncate_lines(&output, MAX_LIST_FILES));
    }

    let mut files = Vec::new();
    collect_files(&root, worktree_root, &mut files)?;
    files.sort();
    if files.len() > MAX_LIST_FILES {
        files.truncate(MAX_LIST_FILES);
        files.push(format!("... truncated after {MAX_LIST_FILES} files"));
    }
    Ok(files.join("\n"))
}

async fn search_text(worktree_root: &Path, args: Value) -> Result<String, String> {
    let pattern = required_string(&args, "pattern")?;
    let root = resolve_optional_path(worktree_root, args.get("path"))?;
    let relative = root
        .strip_prefix(worktree_root)
        .unwrap_or(&root)
        .to_string_lossy()
        .to_string();
    let target = if relative.is_empty() {
        ".".to_string()
    } else {
        relative
    };

    if let Ok(output) = run_simple_command(
        worktree_root,
        "rg",
        &[
            "-n",
            "--hidden",
            "--glob",
            "!.git",
            "--glob",
            "!target",
            "--glob",
            "!node_modules",
            pattern.as_str(),
            target.as_str(),
        ],
        Duration::from_secs(30),
    )
    .await
    {
        return Ok(truncate_lines(&output, MAX_SEARCH_HITS));
    }

    let mut hits = Vec::new();
    search_files_fallback(&root, worktree_root, &pattern, &mut hits)?;
    if hits.is_empty() {
        return Ok("No matches found.".to_string());
    }
    Ok(truncate_lines(&hits.join("\n"), MAX_SEARCH_HITS))
}

async fn read_file(worktree_root: &Path, args: Value) -> Result<String, String> {
    let path = resolve_required_path(worktree_root, &args, "path")?;
    let content = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
    let start_line = optional_u64(&args, "start_line").unwrap_or(1).max(1) as usize;
    let end_line = optional_u64(&args, "end_line").map(|value| value.max(1) as usize);

    Ok(format_read_output(
        path.strip_prefix(worktree_root).unwrap_or(&path),
        &content,
        start_line,
        end_line,
    ))
}

async fn write_file(worktree_root: &Path, args: Value) -> Result<String, String> {
    let path = resolve_required_path(worktree_root, &args, "path")?;
    let content = required_string(&args, "content")?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create parent directory for '{}': {err}",
                path.display()
            )
        })?;
    }
    std::fs::write(&path, content.as_bytes())
        .map_err(|err| format!("failed to write '{}': {err}", path.display()))?;

    Ok(format!(
        "Wrote {} bytes to {}",
        content.len(),
        path.strip_prefix(worktree_root).unwrap_or(&path).display()
    ))
}

async fn replace_in_file(worktree_root: &Path, args: Value) -> Result<String, String> {
    let path = resolve_required_path(worktree_root, &args, "path")?;
    let old = required_string(&args, "old")?;
    let new = required_string(&args, "new")?;
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if old.is_empty() {
        return Err("field 'old' must not be empty".to_string());
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
    let match_count = content.matches(old.as_str()).count();
    if match_count == 0 {
        return Err(format!(
            "target text not found in {}",
            path.strip_prefix(worktree_root).unwrap_or(&path).display()
        ));
    }
    if match_count > 1 && !replace_all {
        return Err(format!(
            "target text matched {match_count} times in {}; retry with replace_all=true or use a more specific match",
            path.strip_prefix(worktree_root).unwrap_or(&path).display()
        ));
    }

    let updated = if replace_all {
        content.replace(old.as_str(), new.as_str())
    } else {
        content.replacen(old.as_str(), new.as_str(), 1)
    };

    std::fs::write(&path, updated.as_bytes())
        .map_err(|err| format!("failed to write '{}': {err}", path.display()))?;

    Ok(format!(
        "Updated {} ({match_count} replacement{})",
        path.strip_prefix(worktree_root).unwrap_or(&path).display(),
        if match_count == 1 { "" } else { "s" }
    ))
}

async fn run_command(worktree_root: &Path, args: Value) -> Result<String, String> {
    let command = required_string(&args, "command")?;
    let timeout_secs = optional_u64(&args, "timeout_secs")
        .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS)
        .clamp(1, MAX_COMMAND_TIMEOUT_SECS);
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    let mut child = Command::new(shell);
    child
        .arg("-lc")
        .arg(command.as_str())
        .current_dir(worktree_root)
        .kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), child.output())
        .await
        .map_err(|_| format!("command timed out after {timeout_secs}s"))?
        .map_err(|err| format!("failed to run command: {err}"))?;

    let mut combined = String::new();
    if !output.stdout.is_empty() {
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    let combined = truncate_bytes(&combined, MAX_COMMAND_OUTPUT_BYTES);
    Ok(format!(
        "exit_code: {}\n{}",
        output.status.code().unwrap_or(-1),
        if combined.trim().is_empty() {
            "(no output)".to_string()
        } else {
            combined
        }
    ))
}

fn format_read_output(
    path: &Path,
    content: &str,
    start_line: usize,
    end_line: Option<usize>,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let capped_end = end_line.unwrap_or(lines.len()).min(lines.len());
    let start_index = start_line.saturating_sub(1).min(lines.len());
    let mut selected = Vec::new();

    for (index, line) in lines
        .iter()
        .enumerate()
        .skip(start_index)
        .take(capped_end.saturating_sub(start_index).min(MAX_READ_LINES))
    {
        selected.push(format!("{:>4}: {}", index + 1, line));
    }

    let mut output = format!("File: {}\n", path.display());
    if selected.is_empty() {
        output.push_str("(no content in requested range)");
    } else {
        output.push_str(&selected.join("\n"));
    }

    let requested_lines = capped_end.saturating_sub(start_index);
    if requested_lines > MAX_READ_LINES {
        output.push_str(&format!(
            "\n... truncated after {MAX_READ_LINES} lines; request a narrower range for more"
        ));
    }

    output
}

fn truncate_lines(text: &str, max_lines: usize) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.trim_end().to_string();
    }
    lines.truncate(max_lines);
    let mut out = lines.join("\n");
    out.push_str(&format!("\n... truncated after {max_lines} lines"));
    out
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.trim_end().to_string();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...\n[truncated]", text[..end].trim_end())
}

async fn run_simple_command(
    cwd: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let mut child = Command::new(program);
    child.args(args).current_dir(cwd).kill_on_drop(true);
    let output = tokio::time::timeout(timeout, child.output())
        .await
        .map_err(|_| format!("{program} timed out"))?
        .map_err(|err| err.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Err(format!("{program} exited with {}", output.status));
        }
        return Err(stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn collect_files(dir: &Path, root: &Path, out: &mut Vec<String>) -> Result<(), String> {
    if out.len() >= MAX_LIST_FILES {
        return Ok(());
    }

    if dir.is_file() {
        out.push(
            dir.strip_prefix(root)
                .unwrap_or(dir)
                .to_string_lossy()
                .to_string(),
        );
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|err| format!("failed to read directory '{}': {err}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if should_skip_path(&name) {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, root, out)?;
        } else if path.is_file() {
            out.push(
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string(),
            );
        }
        if out.len() >= MAX_LIST_FILES {
            break;
        }
    }

    Ok(())
}

fn search_files_fallback(
    dir: &Path,
    root: &Path,
    pattern: &str,
    out: &mut Vec<String>,
) -> Result<(), String> {
    if out.len() >= MAX_SEARCH_HITS {
        return Ok(());
    }

    if dir.is_file() {
        if let Ok(content) = std::fs::read_to_string(dir) {
            for (index, line) in content.lines().enumerate() {
                if line.contains(pattern) {
                    out.push(format!(
                        "{}:{}:{}",
                        dir.strip_prefix(root).unwrap_or(dir).display(),
                        index + 1,
                        line
                    ));
                    if out.len() >= MAX_SEARCH_HITS {
                        break;
                    }
                }
            }
        }
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|err| format!("failed to read directory '{}': {err}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if should_skip_path(&name) {
            continue;
        }
        if path.is_dir() {
            search_files_fallback(&path, root, pattern, out)?;
        } else if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                for (index, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        out.push(format!(
                            "{}:{}:{}",
                            path.strip_prefix(root).unwrap_or(&path).display(),
                            index + 1,
                            line
                        ));
                        if out.len() >= MAX_SEARCH_HITS {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn should_skip_path(name: &str) -> bool {
    matches!(name, ".git" | ".brehon" | "target" | "node_modules")
}
