use super::*;

#[test]
fn worktree_root_validation_accepts_none() {
    let config = minimal_valid_config();
    let warnings = validate(&config);
    assert!(!warnings
        .iter()
        .any(|w| w.kind == ValidationWarningKind::InvalidWorktreeRoot));
}

#[test]
fn worktree_root_validation_accepts_valid_absolute_path() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_root = Some("/tmp/brehon-worktrees".into());
    let warnings = validate(&config);
    assert!(!warnings
        .iter()
        .any(|w| w.kind == ValidationWarningKind::InvalidWorktreeRoot));
}

#[test]
fn worktree_root_validation_rejects_relative_path() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_root = Some(".brehon/worktrees".into());
    let warnings = validate(&config);
    assert!(warnings.iter().any(|w| {
        w.kind == ValidationWarningKind::InvalidWorktreeRoot
            && w.is_fatal
            && w.message.contains("must be an absolute path")
    }));
}

#[test]
fn worktree_root_validation_rejects_empty_string() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_root = Some("".into());
    let warnings = validate(&config);
    assert!(warnings.iter().any(|w| {
        w.kind == ValidationWarningKind::InvalidWorktreeRoot
            && w.is_fatal
            && w.message.contains("must not be empty")
    }));
}

#[test]
fn worktree_root_validation_rejects_path_traversal() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_root = Some("../outside".into());
    let warnings = validate(&config);
    assert!(warnings.iter().any(|w| {
        w.kind == ValidationWarningKind::InvalidWorktreeRoot
            && w.is_fatal
            && w.message.contains("path traversal")
    }));
}

#[test]
fn worktree_root_validation_rejects_embedded_traversal() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_root = Some("/safe/../unsafe".into());
    let warnings = validate(&config);
    assert!(warnings.iter().any(|w| {
        w.kind == ValidationWarningKind::InvalidWorktreeRoot
            && w.is_fatal
            && w.message.contains("path traversal")
    }));
}

#[test]
fn worktree_root_validation_rejects_null_bytes() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_root = Some("/tmp/brehon\0worktrees".into());
    let warnings = validate(&config);
    assert!(warnings.iter().any(|w| {
        w.kind == ValidationWarningKind::InvalidWorktreeRoot
            && w.is_fatal
            && w.message.contains("null bytes")
    }));
}

#[test]
fn worktree_root_validation_accepts_dotdot_as_path_component_prefix() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_root = Some("/tmp/..cache/build".into());
    let warnings = validate(&config);
    assert!(!warnings
        .iter()
        .any(|w| w.kind == ValidationWarningKind::InvalidWorktreeRoot));
}

#[test]
fn cargo_target_root_validation_uses_same_absolute_path_rules() {
    let mut config = minimal_valid_config();
    config.orchestration.cargo_target_root = Some("relative/cargo-targets".into());
    let warnings = validate(&config);
    assert!(warnings.iter().any(|w| {
        w.kind == ValidationWarningKind::InvalidWorktreeRoot
            && w.is_fatal
            && w.message.contains("orchestration.cargo_target_root")
            && w.message.contains("must be an absolute path")
    }));
}
