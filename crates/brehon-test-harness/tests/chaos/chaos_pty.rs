//! Chaos test: PTY operations under random spawn/kill and concurrent load.
//!
//! Tests that Pty creation, input, resize, and destruction remain safe
//! when operations are interleaved with random delays and rapid lifecycle
//! events.
//!
//! These tests are unix-only because they rely on shell subprocesses.

#[cfg(unix)]
mod unix_tests {
    use brehon_pty::{Pty, PtyConfig};
    use brehon_test_harness::{ChaosConfig, ChaosInjector};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn chaos_pty_rapid_spawn_kill_with_delays() {
        let config = ChaosConfig::with_delays(Duration::from_millis(0), Duration::from_millis(20));
        let cycles = 30;

        for cycle in 0..cycles {
            let mut injector = ChaosInjector::new(config.clone());

            let pty_config = PtyConfig {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "sleep 0.2".to_string()],
                cwd: Some(std::env::temp_dir()),
                env: vec![],
                rows: 24,
                cols: 80,
            };

            let mut pty = Pty::spawn(format!("chaos-pty-{}", cycle), pty_config).expect("spawn");

            injector.delay().await;

            // Sometimes send input before kill
            if cycle % 3 == 0 {
                let _ = pty.send_line("echo test").await;
            }

            // Sometimes resize before kill
            if cycle % 5 == 0 {
                let _ = pty.resize(40, 120);
            }

            injector.delay().await;

            pty.kill();
            drop(pty);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn chaos_pty_concurrent_spawn_kill() {
        // Spawn tasks that each own a Pty; avoid async &self calls
        // across spawn boundaries because Pty is Send but not Sync.
        let mut handles = vec![];

        for i in 0..10 {
            handles.push(tokio::spawn(async move {
                let pty_config = PtyConfig {
                    command: "sh".to_string(),
                    args: vec!["-c".to_string(), "sleep 0.1".to_string()],
                    cwd: Some(std::env::temp_dir()),
                    env: vec![],
                    rows: 24,
                    cols: 80,
                };

                let mut pty =
                    Pty::spawn(format!("concurrent-pty-{}", i), pty_config).expect("spawn");

                tokio::time::sleep(Duration::from_millis(10)).await;
                pty.kill();
                drop(pty);
                true
            }));
        }

        let results: Vec<_> = futures::future::join_all(handles).await;
        let all_ok: bool = results.into_iter().all(|r| r.unwrap());

        assert!(
            all_ok,
            "All concurrent PTY operations should complete without panic"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn chaos_pty_resize_race() {
        let pty_config = PtyConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 0.5".to_string()],
            cwd: Some(std::env::temp_dir()),
            env: vec![],
            rows: 24,
            cols: 80,
        };

        // Use std::sync::Mutex because resize() is synchronous.
        let pty = std::sync::Arc::new(std::sync::Mutex::new(
            Pty::spawn("resize-race", pty_config).expect("spawn"),
        ));

        let mut handles = vec![];

        for i in 0..10 {
            let pty = std::sync::Arc::clone(&pty);
            handles.push(std::thread::spawn(move || {
                for _ in 0..10 {
                    let rows = 20 + (i * 3) % 40;
                    let cols = 60 + (i * 5) % 80;
                    let _ = pty.lock().unwrap().resize(rows, cols);
                    std::thread::sleep(Duration::from_millis(5));
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let mut pty = match std::sync::Arc::try_unwrap(pty) {
            Ok(mutex) => mutex.into_inner().unwrap(),
            Err(_) => panic!("Arc refs still held"),
        };
        pty.kill();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn chaos_pty_input_flood_then_kill() {
        let pty_config = PtyConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 0.3".to_string()],
            cwd: Some(std::env::temp_dir()),
            env: vec![],
            rows: 24,
            cols: 80,
        };

        let mut pty = Pty::spawn("input-flood", pty_config).expect("spawn");

        // Rapid sequential input (no spawn, so Pty stays in one async task)
        for i in 0..50 {
            let line = format!("echo flood-{}", i);
            let _ = pty.send_line(&line).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        pty.kill();
    }
}
