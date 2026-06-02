use std::ffi::OsString;
use std::sync::Mutex;

pub static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

const DEFAULT_EMPTY_VARS: &[&str] = &[
    "BREHON_WORKTREE_BRANCH",
    "BREHON_WORKSPACE_ROOT",
    "BREHON_PROJECT_ROOT",
];

pub struct ScopedEnv {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnv {
    /// Set environment variables for the duration of this guard's lifetime.
    ///
    /// # Safety
    /// This calls `std::env::set_var` which is `unsafe` in Rust 2024 edition.
    /// Callers must hold `TEST_ENV_LOCK` to ensure thread-safety; the lock
    /// provides the synchronization that makes this sound in practice.
    pub fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for (key, value) in vars {
            saved.push((*key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self { saved }
    }

    /// Like [`Self::set`], but also clears `DEFAULT_EMPTY_VARS` to empty strings
    /// unless they are explicitly present in `vars`.
    ///
    /// # Safety
    /// Same thread-safety requirements as [`Self::set`]: callers must hold
    /// `TEST_ENV_LOCK`.
    pub fn set_with_defaults(vars: &[(&'static str, &str)]) -> Self {
        let mut all_vars: Vec<(&'static str, &str)> = vars.to_vec();
        for key in DEFAULT_EMPTY_VARS {
            if !all_vars.iter().any(|(existing, _)| existing == key) {
                all_vars.push((key, ""));
            }
        }
        Self::set(&all_vars)
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.saved.iter().rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
