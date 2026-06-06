use crate::mux::*;
use crate::{AgentAdapter, Generation, Pane, SupervisorCli};
use std::path::PathBuf;

#[test]
fn test_gateway_startup_prompt_targets_first_spawn_generation() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::reviewer(
        "codex-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create gateway reviewer pane");
    assert!(pane.is_gateway_backed());
    assert_eq!(pane.current_generation(), Generation(0));
    assert!(pane.gateway_session_id().is_none());
    mux.add_pane(pane);

    mux.queue_startup_prompt("codex-reviewer", "startup prompt".to_string());

    let queued = mux
        .get("codex-reviewer")
        .expect("reviewer pane exists")
        .delayed_prompt_in_flight()
        .expect("startup prompt queued");
    assert_eq!(queued.generation, Generation(1));

    {
        let pane = mux.get_mut("codex-reviewer").expect("reviewer pane exists");
        pane.register_gateway_session_spawn("session-1".to_string());
        assert_eq!(pane.current_generation(), Generation(1));
        pane.delayed_prompt_in_flight_mut()
            .expect("startup prompt still queued after gateway spawn")
            .inject_after = std::time::Instant::now() - std::time::Duration::from_millis(1);
    }

    let queued = mux
        .get_mut("codex-reviewer")
        .expect("reviewer pane exists")
        .take_ready_delayed_prompt(std::time::Instant::now())
        .expect("startup prompt should remain deliverable after gateway spawn");
    assert_eq!(queued.generation, Generation(1));
    assert_eq!(queued.prompt, "startup prompt");
}
