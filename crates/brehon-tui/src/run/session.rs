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

fn runtime_registered_agent_names(
    brehon_root: &std::path::Path,
    role: &str,
) -> Option<std::collections::HashSet<String>> {
    let path = brehon_root
        .join("runtime")
        .join("daemon")
        .join("current.json");
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let panes = value
        .get("registry")
        .and_then(|registry| registry.get("panes"))
        .and_then(|panes| panes.as_array())?;
    let names = panes
        .iter()
        .filter(|pane| pane.get("kind").and_then(|value| value.as_str()) == Some(role))
        .filter(|pane| pane.get("state").and_then(|value| value.as_str()) != Some("dead"))
        .filter_map(|pane| {
            pane.get("pane_id")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    pane.get("title")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                })
        })
        .map(str::to_string)
        .collect();
    Some(names)
}

fn session_name_matches_current_runtime(
    brehon_root: &std::path::Path,
    role: &str,
    name: &str,
) -> bool {
    let role = role.trim();
    let name = name.trim();
    if role.is_empty() || name.is_empty() {
        return false;
    }

    runtime_registered_agent_names(brehon_root, role)
        .map(|names| names.contains(name))
        .unwrap_or(true)
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
    if !session_name_matches_current_runtime(brehon_root, role, agent_name) {
        return;
    }

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
    let registered_workers = runtime_registered_agent_names(brehon_root, "worker");
    let registered_reviewers = runtime_registered_agent_names(brehon_root, "reviewer");
    let registered_supervisors = runtime_registered_agent_names(brehon_root, "supervisor");
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
                let registered = match role.as_str() {
                    "worker" => registered_workers.as_ref(),
                    "reviewer" => registered_reviewers.as_ref(),
                    "supervisor" => registered_supervisors.as_ref(),
                    _ => None,
                };
                if registered.is_some_and(|names| !names.contains(&name)) {
                    continue;
                }
                if !name.is_empty() {
                    map.insert(name, (role, session_id, last_seen_at));
                }
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_daemon_current(root: &std::path::Path, panes: serde_json::Value) {
        let daemon_dir = root.join("runtime").join("daemon");
        std::fs::create_dir_all(&daemon_dir).unwrap();
        std::fs::write(
            daemon_dir.join("current.json"),
            serde_json::json!({
                "registry": {
                    "panes": panes
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn refresh_session_file_skips_unregistered_runtime_pane() {
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path().join(".brehon");
        write_daemon_current(
            &brehon_root,
            serde_json::json!([
                {
                    "pane_id": "live-worker",
                    "kind": "worker",
                    "state": "ready"
                }
            ]),
        );

        refresh_session_file(
            &brehon_root,
            "unregistered-worker",
            "worker",
            "session-unregistered",
            "opencode",
        );

        assert!(!brehon_root
            .join("runtime")
            .join("sessions")
            .join("unregistered-worker.json")
            .exists());
    }

    #[test]
    fn refresh_session_file_keeps_registered_runtime_pane() {
        let root = tempfile::tempdir().unwrap();
        let brehon_root = root.path().join(".brehon");
        write_daemon_current(
            &brehon_root,
            serde_json::json!([
                {
                    "pane_id": "live-worker",
                    "kind": "worker",
                    "state": "ready"
                }
            ]),
        );

        refresh_session_file(
            &brehon_root,
            "live-worker",
            "worker",
            "session-live",
            "opencode",
        );

        assert!(brehon_root
            .join("runtime")
            .join("sessions")
            .join("live-worker.json")
            .exists());
    }
}
