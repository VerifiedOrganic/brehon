mod crash_recovery_cycles;
mod git_stability;
mod integration_lifecycle;
mod mcp_stability;
mod pty_stability;
mod queue_boundedness;

use std::ffi::OsString;
use std::sync::MutexGuard;

use brehon_test_harness::TEST_ENV_LOCK;

const BREHON_SOAK_CYCLES_ENV: &str = "BREHON_SOAK_CYCLES";

/// Returns the soak cycle count for a test, allowing `BREHON_SOAK_CYCLES` to
/// replace the caller's default.
///
/// Note: `BREHON_SOAK_CYCLES` is a single process-global override for the
/// `soak_tests` target, so every soak test that uses these helpers shares the
/// same value.
///
/// Values below 1 are clamped to 1 so soak tests never run zero iterations
/// silently.
pub fn soak_cycles(default: usize) -> usize {
    match std::env::var(BREHON_SOAK_CYCLES_ENV) {
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(cycles) => cycles.max(1),
            Err(_) => panic!("{BREHON_SOAK_CYCLES_ENV} must be a valid positive integer, got: {v}"),
        },
        Err(std::env::VarError::NotPresent) => default.max(1),
        Err(std::env::VarError::NotUnicode(v)) => panic!(
            "{BREHON_SOAK_CYCLES_ENV} must be valid Unicode, got: {:?}",
            v
        ),
    }
}

pub fn soak_cycles_locked(default: usize) -> usize {
    let _lock = lock_env();
    soak_cycles(default)
}

fn lock_env() -> MutexGuard<'static, ()> {
    TEST_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

struct EnvGuard {
    prev: Option<OsString>,
}

impl EnvGuard {
    fn capture() -> Self {
        Self {
            prev: std::env::var_os(BREHON_SOAK_CYCLES_ENV),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => std::env::set_var(BREHON_SOAK_CYCLES_ENV, value),
            None => std::env::remove_var(BREHON_SOAK_CYCLES_ENV),
        }
    }
}

#[test]
fn soak_cycles_uses_default_when_env_missing() {
    let _lock = lock_env();
    let _guard = EnvGuard::capture();
    std::env::remove_var(BREHON_SOAK_CYCLES_ENV);

    assert_eq!(soak_cycles(200), 200);
    assert_eq!(soak_cycles(3), 3);
}

#[test]
fn soak_cycles_clamps_default_when_env_missing() {
    let _lock = lock_env();
    let _guard = EnvGuard::capture();
    std::env::remove_var(BREHON_SOAK_CYCLES_ENV);

    assert_eq!(soak_cycles(0), 1);
}

#[test]
fn soak_cycles_respects_env_override() {
    let _lock = lock_env();
    let _guard = EnvGuard::capture();
    std::env::set_var(BREHON_SOAK_CYCLES_ENV, "7");

    assert_eq!(soak_cycles(200), 7);
}

#[test]
fn soak_cycles_clamps_zero_override_to_one() {
    let _lock = lock_env();
    let _guard = EnvGuard::capture();
    std::env::set_var(BREHON_SOAK_CYCLES_ENV, "0");

    assert_eq!(soak_cycles(200), 1);
}

#[test]
fn soak_cycles_rejects_invalid_override() {
    let _lock = lock_env();
    let _guard = EnvGuard::capture();
    std::env::set_var(BREHON_SOAK_CYCLES_ENV, "not-a-number");

    let result = std::panic::catch_unwind(|| soak_cycles(200));
    assert!(result.is_err(), "invalid override should panic");
}

#[test]
fn env_guard_restores_previous_value() {
    let _lock = lock_env();
    let _cleanup = EnvGuard::capture();
    std::env::set_var(BREHON_SOAK_CYCLES_ENV, "42");

    {
        let _guard = EnvGuard::capture();
        std::env::set_var(BREHON_SOAK_CYCLES_ENV, "7");
        assert_eq!(soak_cycles(200), 7);
    }

    assert_eq!(std::env::var(BREHON_SOAK_CYCLES_ENV).unwrap(), "42");
}
