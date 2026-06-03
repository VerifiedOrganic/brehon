use brehon_types::BrehonConfig;

use super::{ValidationWarning, ValidationWarningKind};

pub(super) fn validate_runtime_policy(config: &BrehonConfig) -> Vec<ValidationWarning> {
    let mut warnings = Vec::new();
    let retry = config.runtime.retry;
    let continuation = config.runtime.continuation;

    if retry.enabled {
        if retry.max_attempts == 0 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimePolicyConflict,
                "runtime.retry.max_attempts must be greater than 0 when retry is enabled",
            ));
        }
        if retry.base_delay_ms == 0 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimePolicyConflict,
                "runtime.retry.base_delay_ms must be greater than 0 when retry is enabled",
            ));
        }
        if retry.max_delay_ms == 0 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimePolicyConflict,
                "runtime.retry.max_delay_ms must be greater than 0 when retry is enabled",
            ));
        }
        if retry.max_delay_ms < retry.base_delay_ms {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimePolicyConflict,
                "runtime.retry.max_delay_ms must be greater than or equal to runtime.retry.base_delay_ms",
            ));
        }
    }

    if continuation.enabled {
        if continuation.max_turns_per_run == 0 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimePolicyConflict,
                "runtime.continuation.max_turns_per_run must be greater than 0 when continuation is enabled",
            ));
        }
        if continuation.idle_prompt_after_secs == 0 {
            warnings.push(ValidationWarning::new(
                ValidationWarningKind::RuntimePolicyConflict,
                "runtime.continuation.idle_prompt_after_secs must be greater than 0 when continuation is enabled",
            ));
        }
    }

    warnings
}
