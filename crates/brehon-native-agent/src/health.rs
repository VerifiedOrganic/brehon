use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentHealthStatus {
    Available,
    Recovering,
    Unavailable,
}

impl AgentHealthStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Recovering => "recovering",
            Self::Unavailable => "unavailable",
        }
    }
}

pub(crate) struct AgentHeartbeat<'a> {
    pub(crate) agent_name: &'a str,
    pub(crate) role: &'a str,
    pub(crate) agent_type: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) model: &'a str,
    pub(crate) reasoning_effort: Option<&'a str>,
}

pub(crate) fn refresh_agent_session(heartbeat: &AgentHeartbeat<'_>) {
    let Some(root) = brehon_root() else {
        return;
    };
    let path = root.join("runtime").join("sessions").join(format!(
        "{}.json",
        sanitize_path_component(heartbeat.agent_name)
    ));
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }

    let now = now_rfc3339();
    let registered_at = read_json(&path)
        .and_then(|value| {
            value
                .get("registered_at")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| now.clone());

    let mut entry = json!({
        "name": heartbeat.agent_name,
        "role": heartbeat.role,
        "session_id": heartbeat.session_id,
        "registered_at": registered_at,
        "last_seen_at": now,
        "agent_type": heartbeat.agent_type,
        "model": heartbeat.model,
    });
    if let Some(reasoning_effort) = heartbeat
        .reasoning_effort
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        entry["reasoning_effort"] = Value::String(reasoning_effort.to_string());
    }
    if let Ok(session_name) = std::env::var("BREHON_SESSION_NAME") {
        let session_name = session_name.trim();
        if !session_name.is_empty() {
            entry["session_name"] = Value::String(session_name.to_string());
        }
    }

    atomic_write_json(&path, &entry);
}

pub(crate) fn write_agent_health(
    heartbeat: &AgentHeartbeat<'_>,
    status: AgentHealthStatus,
    reason: Option<&str>,
    attempt: Option<usize>,
    last_error: Option<&str>,
) {
    let Some(root) = brehon_root() else {
        return;
    };
    let path = root.join("runtime").join("agent-health").join(format!(
        "{}.json",
        sanitize_path_component(heartbeat.agent_name)
    ));
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }

    let mut entry = json!({
        "agent": heartbeat.agent_name,
        "role": heartbeat.role,
        "agent_type": heartbeat.agent_type,
        "session_id": heartbeat.session_id,
        "model": heartbeat.model,
        "status": status.as_str(),
        "updated_at": now_rfc3339(),
    });
    if let Some(reason) = reason.map(str::trim).filter(|value| !value.is_empty()) {
        entry["reason"] = Value::String(reason.to_string());
    }
    if let Some(attempt) = attempt {
        entry["attempt"] = json!(attempt);
    }
    if let Some(last_error) = last_error.map(str::trim).filter(|value| !value.is_empty()) {
        entry["last_error"] = Value::String(last_error.to_string());
    }

    atomic_write_json(&path, &entry);
}

fn brehon_root() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

fn read_json(path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn atomic_write_json(path: &Path, value: &Value) {
    let Some(parent) = path.parent() else {
        return;
    };
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("agent-state")
    ));
    let Ok(data) = serde_json::to_vec_pretty(value) else {
        return;
    };
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn sanitize_path_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "agent".to_string()
    } else {
        out
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
