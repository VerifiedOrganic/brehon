use std::path::Path;

use anyhow::Result;
use brehon_types::{BrehonConfig, PermissionProfileRole};

fn set_env_pair(pairs: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = pairs.iter_mut().find(|(env_key, _)| env_key == key) {
        *existing = value;
    } else {
        pairs.push((key.to_string(), value));
    }
}

fn safe_env_path_component(value: &str, fallback: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

fn cargo_target_dir(root: &Path, role: &str, name: &str) -> std::path::PathBuf {
    root.join(safe_env_path_component(role, "role"))
        .join(safe_env_path_component(name, "agent"))
}

pub(super) fn apply_agent_cargo_target_env(
    env: &mut Vec<(String, String)>,
    cargo_target_root: Option<&Path>,
    role: &str,
    name: &str,
) {
    let Some(root) = cargo_target_root else {
        return;
    };
    let target_dir = cargo_target_dir(root, role, name);
    set_env_pair(
        env,
        "CARGO_TARGET_DIR",
        target_dir.to_string_lossy().to_string(),
    );
}

fn apply_effective_permission_profile_env(
    config: &BrehonConfig,
    role: PermissionProfileRole,
    lane: &str,
    env: &mut Vec<(String, String)>,
) {
    let profile_name = config
        .effective_permission_profile(role, Some(lane), None)
        .profile
        .as_str();

    env.retain(|(key, _)| key != "BREHON_PERMISSION_PROFILE" && key != "CODEX_PERMISSION_PROFILE");
    set_env_pair(env, "BREHON_PERMISSION_PROFILE", profile_name.to_string());
    // The Codex app-server adapter consumes this to choose the thread/turn sandbox.
    set_env_pair(env, "CODEX_PERMISSION_PROFILE", profile_name.to_string());
}

pub(super) fn profile_env(
    config: &BrehonConfig,
    role: PermissionProfileRole,
    lane: &str,
    mut env: Vec<(String, String)>,
) -> Vec<(String, String)> {
    apply_effective_permission_profile_env(config, role, lane, &mut env);
    env
}

fn agent_runtime_tmp_dir(
    root: &Path,
    session_name: &str,
    role: &str,
    name: &str,
) -> std::path::PathBuf {
    root.join("tmp")
        .join(safe_env_path_component(session_name, "session"))
        .join(safe_env_path_component(role, "role"))
        .join(safe_env_path_component(name, "agent"))
}

pub(super) fn apply_agent_runtime_tmp_env(
    env: &mut Vec<(String, String)>,
    runtime_worktree_root: &Path,
    session_name: &str,
    role: &str,
    name: &str,
) -> Result<()> {
    let tmp_dir = agent_runtime_tmp_dir(runtime_worktree_root, session_name, role, name);
    std::fs::create_dir_all(&tmp_dir).map_err(|err| {
        anyhow::anyhow!(
            "failed to create runtime temp dir '{}': {err}",
            tmp_dir.display()
        )
    })?;
    let tmp_dir = tmp_dir.to_string_lossy().to_string();
    for key in ["BREHON_TMPDIR", "TMPDIR", "TEMP", "TMP"] {
        set_env_pair(env, key, tmp_dir.clone());
    }
    Ok(())
}

pub(super) async fn cleanup_session_runtime_tmp_dir(
    runtime_worktree_root: &Path,
    session_name: &str,
) {
    let tmp_dir = runtime_worktree_root
        .join("tmp")
        .join(safe_env_path_component(session_name, "session"));
    if !tmp_dir.exists() {
        return;
    }
    let display_path = tmp_dir.display().to_string();
    match tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&tmp_dir)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                path = %display_path,
                error = %err,
                "Failed to clean Brehon runtime temp directory"
            );
        }
        Err(err) => {
            tracing::warn!(
                path = %display_path,
                error = %err,
                "Runtime temp cleanup task failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn cargo_target_dir_is_role_and_agent_scoped() {
        let root = Path::new("/tmp/brehon-cargo-targets");
        assert_eq!(
            cargo_target_dir(root, "worker", "swift-fox-94"),
            PathBuf::from("/tmp/brehon-cargo-targets/worker/swift-fox-94")
        );
        assert_eq!(
            cargo_target_dir(root, "reviewer/panel", "bad/name"),
            PathBuf::from("/tmp/brehon-cargo-targets/reviewer-panel/bad-name")
        );
    }

    #[test]
    fn apply_agent_cargo_target_env_overrides_launcher_target_dir() {
        let mut env = vec![(
            "CARGO_TARGET_DIR".to_string(),
            "/old/shared-target".to_string(),
        )];

        apply_agent_cargo_target_env(
            &mut env,
            Some(Path::new("/tmp/brehon-cargo-targets")),
            "worker",
            "worker-1",
        );

        assert_eq!(
            env.iter()
                .find_map(|(key, value)| (key == "CARGO_TARGET_DIR").then_some(value.as_str())),
            Some("/tmp/brehon-cargo-targets/worker/worker-1")
        );
    }

    #[test]
    fn effective_permission_profile_env_overrides_stale_launcher_values() {
        let mut config = brehon_config::parse_defaults().expect("default config");
        config
            .lanes
            .get_mut("codex-worker")
            .expect("codex worker lane")
            .profile = Some(brehon_types::PermissionProfile::Unsafe);
        let mut env = vec![
            (
                "CODEX_PERMISSION_PROFILE".to_string(),
                "workspace".to_string(),
            ),
            (
                "BREHON_PERMISSION_PROFILE".to_string(),
                "workspace".to_string(),
            ),
        ];

        let env = profile_env(
            &config,
            brehon_types::PermissionProfileRole::Worker,
            "codex-worker",
            env,
        );

        for key in ["CODEX_PERMISSION_PROFILE", "BREHON_PERMISSION_PROFILE"] {
            assert_eq!(
                env.iter()
                    .find_map(|(env_key, value)| (env_key == key).then_some(value.as_str())),
                Some("unsafe")
            );
        }
    }

    #[test]
    fn apply_agent_runtime_tmp_env_overrides_launcher_temp_dirs() {
        let root = tempfile::tempdir().unwrap();
        let mut env = vec![
            ("TMPDIR".to_string(), "/tmp/shared".to_string()),
            ("TEMP".to_string(), "/tmp/shared".to_string()),
            ("TMP".to_string(), "/tmp/shared".to_string()),
        ];

        apply_agent_runtime_tmp_env(
            &mut env,
            root.path(),
            "session/name",
            "reviewer/panel",
            "bad/name",
        )
        .unwrap();

        let expected = root
            .path()
            .join("tmp/session-name/reviewer-panel/bad-name")
            .to_string_lossy()
            .to_string();
        for key in ["BREHON_TMPDIR", "TMPDIR", "TEMP", "TMP"] {
            assert_eq!(
                env.iter()
                    .find_map(|(env_key, value)| (env_key == key).then_some(value.as_str())),
                Some(expected.as_str())
            );
        }
        assert!(Path::new(&expected).is_dir());
    }

    #[tokio::test]
    async fn cleanup_session_runtime_tmp_dir_removes_session_root() {
        let root = tempfile::tempdir().unwrap();
        let session_tmp = agent_runtime_tmp_dir(root.path(), "session/name", "worker", "agent-1");
        std::fs::create_dir_all(&session_tmp).unwrap();
        std::fs::write(session_tmp.join("scratch"), "temporary").unwrap();

        cleanup_session_runtime_tmp_dir(root.path(), "session/name").await;

        assert!(!root.path().join("tmp/session-name").exists());
    }
}
