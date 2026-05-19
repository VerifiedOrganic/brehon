//! Crash recovery testing framework.
//!
//! Framework for spawning Brehon as subprocess and killing at scripted event points.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Scripted crash point.
#[derive(Debug, Clone)]
pub enum CrashPoint {
    AfterEvent(String),
    AfterMessageCount(usize),
    AfterDuration(Duration),
    AtEventMatch(String, String),
}

/// Crash scenario configuration.
#[derive(Debug, Clone)]
pub struct CrashScenario {
    pub name: String,
    pub crash_points: Vec<CrashPoint>,
    pub restart_after_crash: bool,
    pub verify_recovery: bool,
}

/// Crash injector for testing recovery.
pub struct CrashInjector {
    scenarios: Vec<CrashScenario>,
    current_scenario: Option<usize>,
    event_count: usize,
    messages_sent: usize,
    crashed: bool,
}

impl CrashInjector {
    pub fn new() -> Self {
        Self {
            scenarios: Vec::new(),
            current_scenario: None,
            event_count: 0,
            messages_sent: 0,
            crashed: false,
        }
    }

    pub fn add_scenario(mut self, scenario: CrashScenario) -> Self {
        self.scenarios.push(scenario);
        self
    }

    pub fn start_scenario(&mut self, name: &str) {
        self.current_scenario = self.scenarios.iter().position(|s| s.name == name);
        self.event_count = 0;
        self.messages_sent = 0;
        self.crashed = false;
    }

    pub fn record_event(&mut self, event_kind: &str) -> bool {
        self.event_count += 1;

        if self.should_crash_at_event(event_kind) {
            self.crashed = true;
            true
        } else {
            false
        }
    }

    pub fn record_message(&mut self) -> bool {
        self.messages_sent += 1;

        if self.should_crash_at_count() {
            self.crashed = true;
            true
        } else {
            false
        }
    }

    pub fn should_crash(&self) -> bool {
        self.crashed
    }

    pub fn reset(&mut self) {
        self.event_count = 0;
        self.messages_sent = 0;
        self.crashed = false;
    }

    fn should_crash_at_event(&self, event_kind: &str) -> bool {
        if let Some(idx) = self.current_scenario {
            let scenario = &self.scenarios[idx];
            for point in &scenario.crash_points {
                match point {
                    CrashPoint::AfterEvent(kind) if kind == event_kind => return true,
                    CrashPoint::AtEventMatch(kind, pattern) if kind == event_kind => return true,
                    _ => {}
                }
            }
        }
        false
    }

    fn should_crash_at_count(&self) -> bool {
        if let Some(idx) = self.current_scenario {
            let scenario = &self.scenarios[idx];
            for point in &scenario.crash_points {
                if let CrashPoint::AfterMessageCount(count) = point {
                    if *count == self.messages_sent {
                        return true;
                    }
                }
            }
        }
        false
    }
}

impl Default for CrashInjector {
    fn default() -> Self {
        Self::new()
    }
}

/// Process handle for subprocess testing.
#[derive(Debug)]
pub struct SubprocessHandle {
    child: Option<Child>,
    binary_path: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
}

impl SubprocessHandle {
    pub fn new(binary_path: PathBuf) -> Self {
        Self {
            child: None,
            binary_path,
            args: Vec::new(),
            env: HashMap::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn spawn(&mut self) -> Result<(), std::io::Error> {
        let mut cmd = Command::new(&self.binary_path);
        cmd.args(&self.args);

        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        self.child = Some(cmd.spawn()?);
        Ok(())
    }

    pub fn kill(&mut self) -> Result<(), std::io::Error> {
        if let Some(ref mut child) = self.child {
            child.kill()?;
            child.wait()?;
            self.child = None;
        }
        Ok(())
    }

    pub fn send_signal(&mut self, signal: nix::sys::signal::Signal) -> Result<(), std::io::Error> {
        if let Some(ref child) = self.child {
            use nix::sys::signal::kill;
            use nix::unistd::Pid;

            kill(Pid::from_raw(child.id() as i32), signal).map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    pub fn is_running(&mut self) -> bool {
        if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) => {
                    self.child = None;
                    false
                }
                Err(_) => false,
            }
        } else {
            false
        }
    }

    pub fn wait(&mut self) -> Result<std::process::ExitStatus, std::io::Error> {
        if let Some(ref mut child) = self.child {
            child.wait()
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no child process",
            ))
        }
    }
}

impl Drop for SubprocessHandle {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_injector_basic() {
        let injector = CrashInjector::new().add_scenario(CrashScenario {
            name: "test".into(),
            crash_points: vec![CrashPoint::AfterEvent("TaskCompleted".into())],
            restart_after_crash: false,
            verify_recovery: true,
        });

        let mut inj = injector;
        inj.start_scenario("test");

        assert!(!inj.record_event("TaskCreated"));
        assert!(inj.record_event("TaskCompleted"));
        assert!(inj.should_crash());
    }

    #[test]
    fn crash_injector_message_count() {
        let injector = CrashInjector::new().add_scenario(CrashScenario {
            name: "test".into(),
            crash_points: vec![CrashPoint::AfterMessageCount(2)],
            restart_after_crash: false,
            verify_recovery: true,
        });

        let mut inj = injector;
        inj.start_scenario("test");

        assert!(!inj.record_message());
        assert!(inj.record_message());
        assert!(inj.should_crash());
    }

    #[test]
    fn crash_injector_reset() {
        let injector = CrashInjector::new().add_scenario(CrashScenario {
            name: "test".into(),
            crash_points: vec![CrashPoint::AfterMessageCount(2)],
            restart_after_crash: false,
            verify_recovery: true,
        });

        let mut inj = injector;
        inj.start_scenario("test");

        assert!(!inj.record_message()); // count = 1, crash_at = 2
        assert!(!inj.should_crash());
        assert!(inj.record_message()); // count = 2, crash!
        assert!(inj.should_crash());

        inj.reset();
        assert!(!inj.should_crash());

        // After reset, count is 0, so message count 1 won't trigger crash
        assert!(!inj.record_message()); // count = 1, crash_at = 2
        assert!(!inj.should_crash());
    }
}
