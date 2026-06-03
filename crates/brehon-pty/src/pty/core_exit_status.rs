fn collect_child_exit_code_after_eof(child: &mut dyn portable_pty::Child) -> Option<i32> {
    match child.try_wait() {
        Ok(Some(status)) => Some(status.exit_code() as i32),
        Ok(None) => child.wait().ok().map(|status| status.exit_code() as i32),
        Err(err) => {
            tracing::debug!(error = %err, "Failed to poll PTY child exit status after EOF");
            None
        }
    }
}

#[cfg(test)]
mod exit_status_tests {
    use super::*;
    use portable_pty::{Child, ChildKiller, ExitStatus};
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    enum TryWaitResult {
        Running,
        Exited(u32),
    }

    #[derive(Debug)]
    struct FakeChildState {
        try_wait_results: VecDeque<TryWaitResult>,
        wait_calls: usize,
    }

    #[derive(Clone, Debug)]
    struct FakeChild {
        state: Arc<Mutex<FakeChildState>>,
    }

    impl FakeChild {
        fn new(try_wait_results: Vec<TryWaitResult>) -> (Self, Arc<Mutex<FakeChildState>>) {
            let state = Arc::new(Mutex::new(FakeChildState {
                try_wait_results: try_wait_results.into(),
                wait_calls: 0,
            }));
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl ChildKiller for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl Child for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            let next = self
                .state
                .lock()
                .expect("state lock poisoned")
                .try_wait_results
                .pop_front()
                .unwrap_or(TryWaitResult::Running);
            match next {
                TryWaitResult::Running => Ok(None),
                TryWaitResult::Exited(code) => Ok(Some(ExitStatus::with_exit_code(code))),
            }
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            let mut state = self.state.lock().expect("state lock poisoned");
            state.wait_calls += 1;
            match state
                .try_wait_results
                .pop_front()
                .unwrap_or(TryWaitResult::Running)
            {
                TryWaitResult::Running => Ok(ExitStatus::with_signal("SIGKILL")),
                TryWaitResult::Exited(code) => Ok(ExitStatus::with_exit_code(code)),
            }
        }

        fn process_id(&self) -> Option<u32> {
            Some(42)
        }
    }

    #[test]
    fn waits_for_exit_status_when_try_wait_races_eof() {
        let (mut child, state) =
            FakeChild::new(vec![TryWaitResult::Running, TryWaitResult::Exited(7)]);

        let exit_code = collect_child_exit_code_after_eof(&mut child);

        assert_eq!(exit_code, Some(7));
        assert_eq!(state.lock().expect("state lock poisoned").wait_calls, 1);
    }
}
