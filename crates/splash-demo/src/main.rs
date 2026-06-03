//! Local visual sanity-check for the brehon startup splash.
//!
//! The real splash lives in `crates/brehon-cli/src/ui.rs`; this binary pulls
//! that file in verbatim via `#[path = ...]` and drives it with a scripted
//! event sequence so the splash can be inspected outside of an actual
//! `brehon run`.
//!
//! Set `BREHON_SPLASH_DEBUG_FORCE_RENDER=1` and redirect stdout to a file to
//! capture a frame snapshot:
//!
//!     BREHON_SPLASH_DEBUG_FORCE_RENDER=1 cargo run -p splash-demo > /tmp/splash.out

// The ui.rs file contains many helpers used by the real brehon-cli binary
// (banner printing, agent-check tables, etc.) that this demo doesn't touch.
// Silence the resulting dead-code warnings in this dev tool.
#[allow(dead_code)]
#[path = "../../brehon-cli/src/ui.rs"]
mod ui;

use std::thread::sleep;
use std::time::Duration;

fn pause(ms: u64) {
    sleep(Duration::from_millis(ms));
}

fn main() {
    let mut splash = ui::StartupSplash::new();

    splash.set_stage("Loading configuration");
    splash.record("Project root: /Users/demo/workspace/brehon");
    pause(350);
    splash.record("Checking shared repository branch");
    pause(400);

    splash.set_stage("Preparing runtime");
    splash.record("Ensuring MCP configuration");
    pause(300);
    splash.record("Ensuring Codex instruction files");
    pause(300);
    splash.record("Reconciling initiative hierarchy");
    pause(300);

    splash.set_summary("3 workers, 9 reviewers, supervisor claude-supervisor");
    // Structured roster drives the new per-kind breakdown on the architecture
    // diagram's sub-row (e.g. "claude 2  codex 1").
    splash.set_roster(
        vec![
            ("claude-code".to_string(), 2),
            ("codex-tmux".to_string(), 1),
        ],
        vec![
            ("claude-code".to_string(), 5),
            ("kimi-cli".to_string(), 2),
            ("gemini-acp".to_string(), 2),
        ],
        "claude-supervisor".to_string(),
    );
    splash.record("Planned launch: 3 workers, 9 reviewers, supervisor claude-supervisor");
    pause(500);

    splash.set_stage("Preparing worker worktrees");
    // Three workers: two claude + one codex, matching the roster above.
    for name in ["free-20", "joy-30", "kit-05"] {
        splash.record(format!("Preparing worker workspace for {name}"));
        pause(140);
        splash.record(format!(
            "Creating worker worktree {name} on branch brehon/abc/{name}"
        ));
        pause(140);
        splash.record(format!(
            "worker {name} ready at /Users/demo/workspace/brehon/.brehon/worktrees/{name}"
        ));
        pause(100);
    }

    splash.set_stage("Preparing supervisor worktree");
    splash.record("Preparing supervisor workspace for claude-supervisor");
    pause(200);
    splash.record("Creating supervisor worktree claude-supervisor on branch brehon/abc/sup");
    pause(200);
    splash.record("supervisor claude-supervisor ready at /Users/demo/.../worktrees/sup");
    pause(200);

    splash.set_stage("Preparing reviewer worktrees");
    for name in ["sfe-01", "free-30", "wish-02", "lab-07", "deep-08"] {
        splash.record(format!("Preparing reviewer workspace for {name}"));
        pause(100);
        splash.record(format!(
            "Creating reviewer worktree {name} on branch brehon/xyz/{name}"
        ));
        pause(80);
        splash.record(format!(
            "reviewer {name} ready at /Users/demo/workspace/brehon/.brehon/worktrees/{name}"
        ));
        pause(80);
    }

    // Let the final frame linger briefly so the user can admire it.
    pause(2500);
    splash.finish();
}
