use crate::{load_config_with_override, parse_defaults};

fn temp_config_path(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}.yaml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("unix epoch")
            .as_nanos()
    ))
}

#[test]
fn retry_defaults_are_conservative() {
    let config = parse_defaults().expect("defaults");

    assert!(config.runtime.retry.enabled);
    assert_eq!(config.runtime.retry.max_attempts, 2);
    assert_eq!(config.runtime.retry.base_delay_ms, 10_000);
    assert_eq!(config.runtime.retry.max_delay_ms, 60_000);
    assert_eq!(config.runtime.retry.jitter_ms, 250);
    assert!(config.runtime.continuation.enabled);
    assert_eq!(config.runtime.continuation.max_turns_per_run, 5);
    assert_eq!(config.runtime.continuation.idle_prompt_after_secs, 300);
}

#[test]
fn retry_project_overlay_parses_runtime_policy() {
    let path = temp_config_path("brehon-config-retry-overlay");
    std::fs::write(
        &path,
        r#"
runtime:
  retry:
    enabled: true
    max_attempts: 4
    base_delay_ms: 5000
    max_delay_ms: 30000
    jitter_ms: 100
  continuation:
    enabled: true
    max_turns_per_run: 7
    idle_prompt_after_secs: 120
"#,
    )
    .expect("write overlay");

    let config = load_config_with_override(None, Some(&path)).expect("load overlay");
    std::fs::remove_file(&path).ok();

    assert_eq!(config.runtime.retry.max_attempts, 4);
    assert_eq!(config.runtime.retry.base_delay_ms, 5_000);
    assert_eq!(config.runtime.retry.max_delay_ms, 30_000);
    assert_eq!(config.runtime.retry.jitter_ms, 100);
    assert_eq!(config.runtime.continuation.max_turns_per_run, 7);
    assert_eq!(config.runtime.continuation.idle_prompt_after_secs, 120);
}

#[test]
fn retry_project_overlay_rejects_zero_values() {
    let path = temp_config_path("brehon-config-retry-invalid");
    std::fs::write(
        &path,
        r#"
runtime:
  retry:
    max_attempts: 0
"#,
    )
    .expect("write overlay");

    let err = load_config_with_override(None, Some(&path)).expect_err("invalid retry config");
    std::fs::remove_file(&path).ok();

    assert!(err
        .to_string()
        .contains("runtime.retry.max_attempts must be greater than 0"));
}
