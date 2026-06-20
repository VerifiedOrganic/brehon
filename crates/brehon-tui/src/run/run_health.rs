//! Runtime health heartbeat helpers for the live TUI loop.

use std::time::{Duration, Instant};

use brehon_types::RuntimeHealthSnapshot;

use super::event_loop::{runtime_command_timestamp_ms, EventLoopCtx};

pub(crate) struct RunHealthState {
    pub last_write: Instant,
    pub write_interval: Duration,
    pub pending_write: Option<tokio::task::JoinHandle<Result<(), String>>>,
    pub max_tick_elapsed: Duration,
    pub slow_tick_count: u64,
    pub last_warning: Option<Instant>,
    pub warning_cooldown: Duration,
}

impl RunHealthState {
    pub(crate) fn live() -> Self {
        Self::new(Duration::from_secs(5), Duration::from_secs(10 * 60))
    }

    pub(crate) fn new(write_interval: Duration, warning_cooldown: Duration) -> Self {
        let now = Instant::now();
        Self {
            last_write: now.checked_sub(write_interval).unwrap_or(now),
            write_interval,
            pending_write: None,
            max_tick_elapsed: Duration::ZERO,
            slow_tick_count: 0,
            last_warning: None,
            warning_cooldown,
        }
    }

    pub(crate) fn idle(now: Instant, write_interval: Duration, warning_cooldown: Duration) -> Self {
        Self {
            last_write: now,
            write_interval,
            pending_write: None,
            max_tick_elapsed: Duration::ZERO,
            slow_tick_count: 0,
            last_warning: None,
            warning_cooldown,
        }
    }

    pub(crate) fn observe_tick(&mut self, tick_elapsed: Duration, slow_tick: bool) {
        self.max_tick_elapsed = self.max_tick_elapsed.max(tick_elapsed);
        if slow_tick {
            self.slow_tick_count = self.slow_tick_count.saturating_add(1);
        }
    }

    pub(crate) fn should_warn(&self, tick_elapsed: Duration) -> bool {
        tick_elapsed >= Duration::from_secs(1)
            && self
                .last_warning
                .is_none_or(|last| last.elapsed() >= self.warning_cooldown)
    }

    pub(crate) fn mark_warning(&mut self) {
        self.last_warning = Some(Instant::now());
    }
}

pub(crate) fn finish_runtime_observability(ctx: &mut EventLoopCtx) {
    super::notifications::finish_pending_outbox_drain(ctx);
    finish_pending_write(ctx);
    write_final_snapshot(ctx);
}

pub(crate) fn service(
    ctx: &mut EventLoopCtx,
    tick_elapsed: Duration,
    total_bytes: usize,
    batch_event_count: usize,
) {
    if ctx
        .run_health
        .pending_write
        .as_ref()
        .is_some_and(tokio::task::JoinHandle::is_finished)
    {
        finish_pending_write(ctx);
    }

    if ctx.run_health.pending_write.is_some()
        || ctx.run_health.last_write.elapsed() < ctx.run_health.write_interval
    {
        return;
    }

    let Some(root) = ctx.dashboard_data.lock().brehon_root.clone() else {
        return;
    };
    let snapshot = running_snapshot(ctx, tick_elapsed, total_bytes, batch_event_count);
    ctx.run_health.last_write = Instant::now();
    ctx.run_health.pending_write = Some(ctx.rt.spawn_blocking(move || {
        let path = brehon_types::runtime_health_snapshot_path(&root);
        brehon_types::write_json_atomic(&path, &snapshot).map_err(|err| err.to_string())
    }));
}

pub(crate) fn finish_pending_write(ctx: &mut EventLoopCtx) {
    if let Some(handle) = ctx.run_health.pending_write.take() {
        match ctx.rt.block_on(handle) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(error = %err, "run health snapshot write failed"),
            Err(err) => tracing::warn!(error = %err, "run health snapshot task failed"),
        }
    }
}

pub(crate) fn write_final_snapshot(ctx: &EventLoopCtx) {
    let Some(root) = ctx.dashboard_data.lock().brehon_root.clone() else {
        return;
    };
    let snapshot = RuntimeHealthSnapshot {
        session_name: ctx.runtime_session_name.clone(),
        status: "shutdown".to_string(),
        pid: std::process::id(),
        updated_at_ms: runtime_command_timestamp_ms(),
        elapsed_secs: ctx.started_at.elapsed().as_secs(),
        last_tick_elapsed_ms: 0,
        max_tick_elapsed_ms: duration_millis_u64(ctx.run_health.max_tick_elapsed),
        slow_tick_count: ctx.run_health.slow_tick_count,
        last_mux_output_bytes: 0,
        last_mux_events: 0,
        pending_prompts: ctx.mux.pending_delayed_prompt_count(),
        notification_drain_pending: ctx.notification_outbox.pending.is_some(),
    };
    let path = brehon_types::runtime_health_snapshot_path(&root);
    if let Err(err) = brehon_types::write_json_atomic(&path, &snapshot) {
        tracing::warn!(error = %err, "final run health snapshot write failed");
    }
}

fn running_snapshot(
    ctx: &EventLoopCtx,
    tick_elapsed: Duration,
    total_bytes: usize,
    batch_event_count: usize,
) -> RuntimeHealthSnapshot {
    RuntimeHealthSnapshot {
        session_name: ctx.runtime_session_name.clone(),
        status: "running".to_string(),
        pid: std::process::id(),
        updated_at_ms: runtime_command_timestamp_ms(),
        elapsed_secs: ctx.started_at.elapsed().as_secs(),
        last_tick_elapsed_ms: duration_millis_u64(tick_elapsed),
        max_tick_elapsed_ms: duration_millis_u64(ctx.run_health.max_tick_elapsed),
        slow_tick_count: ctx.run_health.slow_tick_count,
        last_mux_output_bytes: total_bytes,
        last_mux_events: batch_event_count,
        pending_prompts: ctx.mux.pending_delayed_prompt_count(),
        notification_drain_pending: ctx.notification_outbox.pending.is_some(),
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}
