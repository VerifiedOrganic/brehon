use brehon_types::BrehonConfig;

use super::{ValidationWarning, ValidationWarningKind};

pub(super) fn validate_security_policy(config: &BrehonConfig) -> Vec<ValidationWarning> {
    if config.orchestration.worktree_isolation
        && matches!(
            config.security.sandbox_profile,
            brehon_types::config::SandboxProfile::None
        )
    {
        return vec![ValidationWarning::new(
            ValidationWarningKind::SecurityPolicyConflict,
            "orchestration.worktree_isolation=true requires security.sandbox_profile to be OsDefault or Custom. \
             security.sandbox_profile=None launches unattended agents without a filesystem boundary, so agent writes can escape their assigned worktrees.",
        )];
    }

    Vec::new()
}
