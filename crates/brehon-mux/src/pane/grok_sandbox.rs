use super::spawn::{config_env_value, set_config_env_value};
use crate::error::{Error, Result};
use crate::pty::PtyConfig;
use std::path::{Path, PathBuf};

const GROK_BREHON_SANDBOX_CONFIG_DIR_ENV: &str = "GROK_BREHON_SANDBOX_CONFIG_DIR";
const GROK_BREHON_SANDBOX_MARKER_PREFIX: &str = "# Brehon managed Grok sandbox profile:";

pub(super) fn apply_grok_acp_hardening(config: &mut PtyConfig, command: &str) -> Result<()> {
    if !is_grok_agent_stdio(command, &config.args) {
        return Ok(());
    }

    let mut prefix_args = Vec::new();
    if !args_contain_option(&config.args, "--sandbox")
        && config_env_value(&config.env, "GROK_SANDBOX").is_none()
    {
        prefix_args.push("--sandbox".to_string());
        prefix_args.push(grok_sandbox_profile_for_config(config)?);
    }
    if !args_contain_option(&config.args, "--cwd")
        && let Some(cwd) = config.cwd.as_ref()
    {
        prefix_args.push("--cwd".to_string());
        prefix_args.push(cwd.to_string_lossy().to_string());
    }
    if !prefix_args.is_empty() {
        config.args.splice(0..0, prefix_args);
    }

    let server_env = config
        .env
        .iter()
        .filter(|(key, _)| key.starts_with("BREHON_"))
        .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
        .collect::<Vec<_>>();
    let mcp_servers = serde_json::json!([{
        "name": "brehon",
        "type": "stdio",
        "command": current_brehon_exe(),
        "args": ["serve"],
        "env": server_env,
    }]);
    set_config_env_value(
        &mut config.env,
        "BREHON_ACP_MCP_SERVERS_JSON",
        &mcp_servers.to_string(),
    );
    Ok(())
}

fn current_brehon_exe() -> String {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| "brehon".to_string())
}

fn command_basename(command: &str) -> &str {
    std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}

fn is_grok_agent_stdio(command: &str, args: &[String]) -> bool {
    command_basename(command) == "grok"
        && args.iter().any(|arg| arg == "agent")
        && args.iter().any(|arg| arg == "stdio")
}

fn args_contain_option(args: &[String], option: &str) -> bool {
    args.iter().any(|arg| {
        arg == option
            || arg
                .strip_prefix(option)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

fn grok_sandbox_profile_for_config(config: &PtyConfig) -> Result<String> {
    if config_env_value(&config.env, "BREHON_LAUNCH_POLICY_UNSAFE").as_deref() == Some("true") {
        return Ok("off".to_string());
    }

    let Some(cwd) = config.cwd.as_deref() else {
        return Ok("workspace".to_string());
    };

    let read_write_paths = grok_brehon_read_write_paths(config);
    if read_write_paths.is_empty() {
        return Ok("workspace".to_string());
    }

    let profile_name = grok_brehon_profile_name(config, cwd);
    upsert_grok_brehon_sandbox_profile(config, &profile_name, &read_write_paths)?;
    Ok(profile_name)
}

fn grok_brehon_profile_name(config: &PtyConfig, cwd: &Path) -> String {
    let project_root = config_env_value(&config.env, "BREHON_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf());
    let label = project_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_grok_profile_component)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "project".to_string());
    let hash_input = format!(
        "{}\n{}",
        project_root.to_string_lossy(),
        cwd.to_string_lossy()
    );
    let hash = stable_path_hash(&hash_input);
    format!("brehon-{label}-{hash:016x}")
}

fn sanitize_grok_profile_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn stable_path_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn grok_brehon_read_write_paths(config: &PtyConfig) -> Vec<PathBuf> {
    let cwd = config.cwd.as_deref();
    let mut paths = Vec::new();
    push_env_path(&mut paths, cwd, &config.env, "BREHON_ROOT");

    if let Some(project_root) = config_env_path(cwd, &config.env, "BREHON_PROJECT_ROOT") {
        push_path(&mut paths, project_root.join(".git"));
        push_git_metadata_paths(&mut paths, &project_root);
    }
    if let Some(cwd) = cwd {
        push_git_metadata_paths(&mut paths, cwd);
    }

    dedupe_paths(paths)
}

fn push_env_path(
    paths: &mut Vec<PathBuf>,
    cwd: Option<&Path>,
    env: &[(String, String)],
    key: &str,
) {
    if let Some(path) = config_env_path(cwd, env, key) {
        push_path(paths, path);
    }
}

fn config_env_path(cwd: Option<&Path>, env: &[(String, String)], key: &str) -> Option<PathBuf> {
    let value = config_env_value(env, key)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    Some(if path.is_absolute() {
        path
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path
    })
}

fn push_git_metadata_paths(paths: &mut Vec<PathBuf>, repo_root: &Path) {
    let git_entry = repo_root.join(".git");
    if git_entry.is_dir() {
        push_path(paths, git_entry);
        return;
    }
    if !git_entry.is_file() {
        return;
    }

    let Ok(contents) = std::fs::read_to_string(&git_entry) else {
        return;
    };
    let Some(gitdir) = contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let gitdir = resolve_relative_path(repo_root, Path::new(gitdir));
    push_path(paths, gitdir.clone());

    let commondir_path = gitdir.join("commondir");
    let Ok(commondir) = std::fs::read_to_string(&commondir_path) else {
        return;
    };
    let commondir = commondir.trim();
    if !commondir.is_empty() {
        push_path(paths, resolve_relative_path(&gitdir, Path::new(commondir)));
    }
}

fn resolve_relative_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn push_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if path.as_os_str().is_empty() {
        return;
    }
    paths.push(std::fs::canonicalize(&path).unwrap_or(path));
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for path in paths {
        let key = path.to_string_lossy().to_string();
        if seen.insert(key) {
            unique.push(path);
        }
    }
    unique
}

fn upsert_grok_brehon_sandbox_profile(
    config: &PtyConfig,
    profile_name: &str,
    read_write_paths: &[PathBuf],
) -> Result<()> {
    let sandbox_path = grok_sandbox_config_path(config)?;
    if let Some(parent) = sandbox_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            Error::pty(format!(
                "Failed to create Grok sandbox config directory '{}': {err}",
                parent.display()
            ))
        })?;
    }

    let existing = match std::fs::read_to_string(&sandbox_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(Error::pty(format!(
                "Failed to read Grok sandbox config '{}': {err}",
                sandbox_path.display()
            )));
        }
    };
    let block = grok_brehon_sandbox_block(profile_name, read_write_paths);
    let updated = upsert_grok_brehon_sandbox_block(&sandbox_path, &existing, profile_name, &block)?;
    if updated != existing {
        std::fs::write(&sandbox_path, updated).map_err(|err| {
            Error::pty(format!(
                "Failed to write Grok sandbox config '{}': {err}",
                sandbox_path.display()
            ))
        })?;
    }
    Ok(())
}

fn grok_sandbox_config_path(config: &PtyConfig) -> Result<PathBuf> {
    if let Some(config_dir) = config_env_value(&config.env, GROK_BREHON_SANDBOX_CONFIG_DIR_ENV)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(config_dir).join("sandbox.toml"));
    }

    let home = std::env::var_os("HOME").ok_or_else(|| {
        Error::pty("HOME is not set; cannot install managed Grok sandbox profile".to_string())
    })?;
    Ok(PathBuf::from(home).join(".grok").join("sandbox.toml"))
}

fn grok_brehon_sandbox_block(profile_name: &str, read_write_paths: &[PathBuf]) -> String {
    let begin_marker = grok_brehon_begin_marker(profile_name);
    let end_marker = grok_brehon_end_marker(profile_name);
    let read_write = read_write_paths
        .iter()
        .map(|path| format!("  \"{}\"", toml_escape(&path.to_string_lossy())))
        .collect::<Vec<_>>()
        .join(",\n");

    format!(
        "{begin_marker}\n[profiles.\"{}\"]\nextends = \"workspace\"\nread_write = [\n{read_write}\n]\n{end_marker}\n",
        toml_escape(profile_name),
    )
}

fn upsert_grok_brehon_sandbox_block(
    sandbox_path: &Path,
    existing: &str,
    profile_name: &str,
    block: &str,
) -> Result<String> {
    let begin_marker = grok_brehon_begin_marker(profile_name);
    let end_marker = grok_brehon_end_marker(profile_name);

    if let Some(start) = existing.find(&begin_marker) {
        let Some(end_offset) = existing[start..].find(&end_marker) else {
            return Err(Error::pty(format!(
                "Failed to update Grok sandbox profile '{profile_name}' in '{}': missing managed end marker",
                sandbox_path.display()
            )));
        };
        let end = start + end_offset + end_marker.len();
        let replace_end = if existing[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        let mut updated = String::new();
        updated.push_str(&existing[..start]);
        updated.push_str(block);
        updated.push_str(&existing[replace_end..]);
        return Ok(updated);
    }

    let mut updated = existing.to_string();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(block);
    Ok(updated)
}

fn grok_brehon_begin_marker(profile_name: &str) -> String {
    format!("{GROK_BREHON_SANDBOX_MARKER_PREFIX} {profile_name} BEGIN")
}

fn grok_brehon_end_marker(profile_name: &str) -> String {
    format!("{GROK_BREHON_SANDBOX_MARKER_PREFIX} {profile_name} END")
}

fn toml_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}
