use super::*;

#[test]
fn worktree_isolation_with_no_sandbox_is_fatal() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_isolation = true;
    config.security.sandbox_profile = SandboxProfile::None;

    let warning = validate(&config)
        .into_iter()
        .find(|warning| warning.kind == ValidationWarningKind::SecurityPolicyConflict)
        .expect("unsafe isolated run should be rejected by validation");

    assert!(warning.is_fatal);
    assert!(warning.message.contains("security.sandbox_profile=None"));
}

#[test]
fn worktree_isolation_with_os_default_sandbox_is_valid() {
    let mut config = minimal_valid_config();
    config.orchestration.worktree_isolation = true;
    config.security.sandbox_profile = SandboxProfile::OsDefault;

    assert!(!validate(&config)
        .into_iter()
        .any(|warning| warning.kind == ValidationWarningKind::SecurityPolicyConflict));
}
