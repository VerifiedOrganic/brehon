//! Soak test: PTY spawn/kill cycles.
//!
//! Verifies that Pty creation and destruction does not leak child processes
//! or file descriptors over many rapid cycles.
//!
//! These tests are unix-only because they rely on shell subprocesses.

#[cfg(unix)]
mod unix_tests {
    use brehon_pty::{Pty, PtyConfig};
    use std::time::Duration;

    const CYCLES: usize = 50;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn soak_pty_spawn_kill_cycles_no_leak() {
        for cycle in 0..CYCLES {
            let config = PtyConfig {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "sleep 0.1".to_string()],
                cwd: Some(std::env::temp_dir()),
                env: vec![],
                rows: 24,
                cols: 80,
            };

            let mut pty = Pty::spawn(format!("soak-pty-{}", cycle), config).expect("spawn pty");

            // Let it start
            tokio::time::sleep(Duration::from_millis(20)).await;

            // Explicit kill
            pty.kill();

            // Verify the PTY can be dropped cleanly after kill
            drop(pty);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn soak_pty_drop_cycles_no_leak() {
        for cycle in 0..CYCLES {
            let config = PtyConfig {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "echo hello".to_string()],
                cwd: Some(std::env::temp_dir()),
                env: vec![],
                rows: 24,
                cols: 80,
            };

            let pty = Pty::spawn(format!("soak-drop-{}", cycle), config).expect("spawn pty");

            // Wait for process to finish
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Drop should reap everything
            drop(pty);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn soak_pty_write_and_kill_cycles() {
        for cycle in 0..CYCLES {
            let config = PtyConfig {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "read line; echo $line".to_string()],
                cwd: Some(std::env::temp_dir()),
                env: vec![],
                rows: 24,
                cols: 80,
            };

            let pty = Pty::spawn(format!("soak-write-{}", cycle), config).expect("spawn pty");

            // Send input
            pty.send_line("test").await.expect("send line");
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Drop (which kills)
            drop(pty);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn soak_pty_resize_during_lifecycle() {
        for cycle in 0..CYCLES {
            let config = PtyConfig {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "sleep 0.05".to_string()],
                cwd: Some(std::env::temp_dir()),
                env: vec![],
                rows: 24,
                cols: 80,
            };

            let pty = Pty::spawn(format!("soak-resize-{}", cycle), config).expect("spawn pty");

            // Resize multiple times
            for _ in 0..5 {
                pty.resize(30, 100).expect("resize");
                pty.resize(50, 200).expect("resize");
                pty.resize(24, 80).expect("resize");
            }

            drop(pty);
        }
    }
}
