use crate::Pane;
use crate::mux::*;
use std::time::{Duration, Instant};

#[test]
fn state_machine_caps_ready_prompt_dispatches_per_tick() {
    let mut mux = Mux::new(24, 80);
    for idx in 1..=3 {
        let pane_id = format!("pane-{idx}");
        mux.add_pane(Pane::director(&pane_id, 24, 80).expect("director pane"));
    }

    let due_at = Instant::now() - Duration::from_millis(1);
    for idx in 1..=3 {
        mux.queue_delayed_prompt(
            &format!("pane-{idx}"),
            format!("queued prompt {idx}"),
            None,
            due_at,
            None,
        );
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let tick_at = Instant::now();
    mux.tick_pane_state_machine_at(rt.handle(), tick_at);

    let mut deferred_for_later = 0usize;
    let mut still_due = 0usize;
    for idx in 1..=3 {
        let pane_id = format!("pane-{idx}");
        let inject_after = mux
            .get(&pane_id)
            .expect("pane exists")
            .delayed_prompt_in_flight()
            .expect("prompt remains queued after director delivery failure")
            .inject_after;
        if inject_after > tick_at {
            deferred_for_later += 1;
        } else {
            still_due += 1;
        }
    }

    assert_eq!(
        deferred_for_later,
        super::super::types::MAX_READY_PROMPT_DISPATCHES_PER_TICK,
        "only the bounded number of ready prompts should be attempted per tick"
    );
    assert_eq!(
        still_due,
        3usize.saturating_sub(super::super::types::MAX_READY_PROMPT_DISPATCHES_PER_TICK),
        "remaining due prompt should wait for next tick"
    );
    assert_eq!(
        mux.pending_delayed_prompt_count(),
        3,
        "failed director delivery attempts must requeue without dropping prompts"
    );
}
