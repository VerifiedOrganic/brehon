use std::path::PathBuf;

pub(crate) use brehon_test_harness::{ScopedEnv, TEST_ENV_LOCK};

mod activity;
mod agy_recovery;
mod basic;
mod delivery;
mod factory;
mod scoping;
mod suppression;

pub fn fresh_temp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub fn setup_fake_linked_worktree(project_root: &std::path::Path, relative: &str) -> PathBuf {
    let worktree = project_root.join(relative);
    std::fs::create_dir_all(&worktree).expect("create fake worktree");
    let gitdir_name = relative
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let gitdir = project_root
        .join(".git")
        .join("worktrees")
        .join(gitdir_name);
    std::fs::create_dir_all(&gitdir).expect("create fake gitdir");
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", gitdir.display()),
    )
    .expect("write linked worktree gitdir file");
    worktree
}
