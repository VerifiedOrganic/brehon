//! Session management: session liveness, refresh, and reading session files.

use super::types::*;

fn current_runtime_session_name(brehon_root: &std::path::Path) -> Option<String> {
    let path = brehon_root.join("runtime").join("current-session.json");
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value
        .get("session_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn session_is_live(entry: &serde_json::Value) -> bool {
    let timestamp = entry
        .get("last_seen_at")
        .and_then(|v| v.as_str())
        .or_else(|| entry.get("registered_at").and_then(|v| v.as_str()))
        .or_else(|| entry.get("started_at").and_then(|v| v.as_str()));

    let Some(timestamp) = timestamp else {
        return true;
    };

    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return true;
    };

    chrono::Utc::now().signed_duration_since(parsed.with_timezone(&chrono::Utc))
        <= chrono::Duration::from_std(SESSION_STALE_AFTER)
            .unwrap_or_else(|_| chrono::Duration::seconds(30))
}

pub(crate) fn refresh_session_file(
    brehon_root: &std::path::Path,
    agent_name: &str,
    role: &str,
    session_id: &str,
    agent_type: &str,
) {
    let sessions_dir = brehon_root.join("runtime").join("sessions");
    if std::fs::create_dir_all(&sessions_dir).is_err() {
        return;
    }

    let path = sessions_dir.join(format!("{agent_name}.json"));
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut entry = serde_json::json!({
        "name": agent_name,
        "role": role,
        "session_id": session_id,
        "registered_at": now,
        "last_seen_at": now,
    });

    if !agent_type.is_empty() {
        entry["agent_type"] = serde_json::Value::String(agent_type.to_string());
    }
    if let Some(session_name) = current_runtime_session_name(brehon_root) {
        entry["session_name"] = serde_json::Value::String(session_name);
    }

    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(existing) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(registered_at) = existing.get("registered_at").and_then(|v| v.as_str()) {
                entry["registered_at"] = serde_json::Value::String(registered_at.to_string());
            }
            if entry.get("agent_type").is_none() {
                if let Some(existing_type) = existing.get("agent_type").and_then(|v| v.as_str()) {
                    entry["agent_type"] = serde_json::Value::String(existing_type.to_string());
                }
            }
        }
    }

    let tmp = sessions_dir.join(format!(".{agent_name}.tmp"));
    if let Ok(data) = serde_json::to_string_pretty(&entry) {
        if std::fs::write(&tmp, &data).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Read all per-agent session files from `.brehon/runtime/sessions/*.json`.
///
/// Returns a map of agent_name → (role, session_id, last_seen_at).
pub(crate) fn read_session_files(
    brehon_root: &std::path::Path,
) -> std::collections::HashMap<String, (String, String, String)> {
    let mut map = std::collections::HashMap::new();
    let expected_session = current_runtime_session_name(brehon_root);
    let sessions_dir = brehon_root.join("runtime").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if !session_is_live(&v) {
                    continue;
                }
                if let Some(expected_session) = expected_session.as_deref() {
                    let session_name = v
                        .get("session_name")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    if session_name != Some(expected_session) {
                        continue;
                    }
                }
                let name = v["name"].as_str().unwrap_or_default().to_string();
                let role = v["role"].as_str().unwrap_or_default().to_string();
                let session_id = v["session_id"].as_str().unwrap_or_default().to_string();
                let last_seen_at = v["last_seen_at"]
                    .as_str()
                    .or_else(|| v["registered_at"].as_str())
                    .unwrap_or_default()
                    .to_string();
                if !name.is_empty() {
                    map.insert(name, (role, session_id, last_seen_at));
                }
            }
        }
    }
    map
}
