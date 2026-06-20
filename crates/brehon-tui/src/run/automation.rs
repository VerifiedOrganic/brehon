use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use brehon_mux::{Mux, MuxEvent, MuxRuntimeCommandPort, PaneState};
use brehon_ports::{PortError, RuntimeCommandPort, RuntimeCommandRouter};
use brehon_types::{RuntimeCommand, RuntimeCommandResult, RuntimePolicyContext};

use super::event_loop::{
    detect_and_handle_supervisor_resets, drain_runtime_command_receiver,
    new_headless_event_loop_ctx, process_pending_queued_gateway_prompt_deliveries,
    process_pending_runtime_approval_resolutions, process_pending_runtime_commands, EventLoopCtx,
};
use super::prompt_delivery::deliver_pending_prompts;
use super::recovery::{force_prompt_retry_due, runtime_prompt_queue_sweep_dirs};
use super::refresh::{
    apply_dashboard_refresh_snapshot, collect_dashboard_refresh, collect_session_refresh_entries,
};

const AUTOMATION_POLL_DELAY: Duration = Duration::from_millis(10);
const AUTOMATION_SETTLE_TIMEOUT: Duration = Duration::from_secs(20);
const AUTOMATION_BUSY_SETTLE_ADVANCE: Duration = Duration::from_secs(31);

#[derive(Clone)]
struct LoopbackRuntimeCommandRouter {
    port: MuxRuntimeCommandPort,
}

#[async_trait]
impl RuntimeCommandRouter for LoopbackRuntimeCommandRouter {
    async fn route_command(
        &self,
        command: RuntimeCommand,
        _context: RuntimePolicyContext,
    ) -> Result<RuntimeCommandResult, PortError> {
        RuntimeCommandPort::execute(&self.port, command).await
    }
}

/// Minimal headless harness for driving the real unattended prompt/reset paths.
///
/// This wraps the TUI event-loop helpers without rendering frames, so external
/// soak tests can exercise the same prompt-queue consumer, runtime command
/// queue, reset/recycle handlers, and supervisor crash recovery that `brehon
/// run` uses.
pub struct RuntimeAutomationHarness {
    brehon_root: PathBuf,
    ctx: EventLoopCtx,
    simulated_now: Instant,
}

impl RuntimeAutomationHarness {
    pub fn new(mux: Mux, brehon_root: PathBuf, rt: tokio::runtime::Handle) -> io::Result<Self> {
        let (port, receiver) = MuxRuntimeCommandPort::channel_default();
        let router: Arc<dyn RuntimeCommandRouter> = Arc::new(LoopbackRuntimeCommandRouter { port });
        let ctx = new_headless_event_loop_ctx(mux, rt, Some(receiver), Some(router), false)?;
        ctx.dashboard_data.lock().brehon_root = Some(brehon_root.clone());
        Ok(Self {
            brehon_root,
            ctx,
            simulated_now: Instant::now(),
        })
    }

    pub fn mux(&self) -> &Mux {
        &self.ctx.mux
    }

    pub fn mux_mut(&mut self) -> &mut Mux {
        &mut self.ctx.mux
    }

    pub fn refresh_dashboard_state(&mut self) {
        let session_entries = collect_session_refresh_entries(&self.ctx.mux);
        let snapshot = collect_dashboard_refresh(
            &self.brehon_root,
            &session_entries,
            &self.ctx.fallback_panels,
        );
        apply_dashboard_refresh_snapshot(
            &mut self.ctx.mux,
            &self.ctx.dashboard_data,
            &mut self.ctx.panels,
            &mut self.ctx.selected_panel,
            &mut self.ctx.selected_member,
            &mut self.ctx.reviewer_selection,
            &mut self.ctx.last_shared_root_issue,
            snapshot,
        );
        self.ctx.needs_redraw = true;
    }

    pub fn service_prompt_and_reset_queues(&mut self) {
        let deadline = Instant::now() + AUTOMATION_SETTLE_TIMEOUT;
        loop {
            tokio::task::block_in_place(|| {
                deliver_pending_prompts(&mut self.ctx, &self.brehon_root);
            });
            self.service_runtime_once();
            if !self.queues_are_idle() || self.has_pending_delayed_prompts() {
                self.fast_forward_busy_panes();
                self.service_runtime_once();
            }
            if self.queues_are_idle()
                && !self.has_pending_runtime_work()
                && !self.has_pending_delayed_prompts()
            {
                self.settle_quiet_period();
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "runtime automation harness did not settle queued work within {:?}: pending_runtime_commands={}, pending_approval_resolutions={}, pending_gateway_deliveries={}, pending_delayed_prompts={}, busy_panes={:?}, queues={:?}",
                    AUTOMATION_SETTLE_TIMEOUT,
                    self.ctx.pending_runtime_commands.len(),
                    self.ctx.pending_runtime_approval_resolutions.len(),
                    self.ctx.pending_queued_gateway_prompt_deliveries.len(),
                    self.ctx.mux.pending_delayed_prompt_count(),
                    self.describe_busy_panes(),
                    self.describe_live_queue_entries(),
                );
            }
            std::thread::sleep(AUTOMATION_POLL_DELAY);
        }
    }

    pub fn pump_until_idle(&mut self) {
        let deadline = Instant::now() + AUTOMATION_SETTLE_TIMEOUT;
        loop {
            self.service_runtime_once();
            if self.has_pending_delayed_prompts() {
                self.fast_forward_busy_panes();
                self.service_runtime_once();
            }
            if !self.has_pending_runtime_work() && !self.has_pending_delayed_prompts() {
                self.settle_quiet_period();
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "runtime automation harness still had pending work after {:?}: pending_runtime_commands={}, pending_approval_resolutions={}, pending_gateway_deliveries={}, pending_delayed_prompts={}, busy_panes={:?}",
                    AUTOMATION_SETTLE_TIMEOUT,
                    self.ctx.pending_runtime_commands.len(),
                    self.ctx.pending_runtime_approval_resolutions.len(),
                    self.ctx.pending_queued_gateway_prompt_deliveries.len(),
                    self.ctx.mux.pending_delayed_prompt_count(),
                    self.describe_busy_panes(),
                );
            }
            std::thread::sleep(AUTOMATION_POLL_DELAY);
        }
    }

    pub fn trigger_supervisor_recovery_from_crash_output(
        &mut self,
        pane_id: &str,
        crash_output: &[u8],
    ) -> Result<(), String> {
        let use_direct_reset_path = self
            .ctx
            .mux
            .get(pane_id)
            .is_some_and(|pane| !pane.is_gateway_backed());
        let generation = {
            let pane = self
                .ctx
                .mux
                .get_mut(pane_id)
                .ok_or_else(|| format!("supervisor pane '{pane_id}' was not found"))?;
            pane.append_output(crash_output)
                .map_err(|err| err.to_string())?;
            if use_direct_reset_path {
                pane.mark_exited(Some(137));
            }
            pane.current_generation()
        };
        let saved_router = if use_direct_reset_path {
            self.ctx.runtime_command_router.take()
        } else {
            None
        };
        let batch_events = if use_direct_reset_path {
            vec![MuxEvent::PaneExited {
                pane_id: pane_id.to_string(),
                exit_code: Some(137),
            }]
        } else {
            vec![MuxEvent::PaneOutput {
                pane_id: pane_id.to_string(),
                data: Vec::new(),
                generation,
            }]
        };
        let now = self.monotonic_now();
        tokio::task::block_in_place(|| {
            detect_and_handle_supervisor_resets(&mut self.ctx, &batch_events, now);
        });
        if let Some(router) = saved_router {
            self.ctx.runtime_command_router = Some(router);
        }
        self.pump_until_idle();
        Ok(())
    }

    pub async fn shutdown_all(&mut self) {
        self.ctx.mux.shutdown_all().await;
        self.pump_until_idle();
    }

    fn service_runtime_once(&mut self) {
        let now = self.monotonic_now();
        tokio::task::block_in_place(|| {
            drain_runtime_command_receiver(&mut self.ctx);
            self.ctx.mux.tick_pane_state_machine_at(&self.ctx.rt, now);
            let (_total_bytes, batch_events) = self.ctx.mux.poll_batch();
            self.ctx.mux.flush_pending_inbox_nudges(&self.ctx.rt);
            process_pending_runtime_commands(&mut self.ctx);
            process_pending_runtime_approval_resolutions(&mut self.ctx);
            process_pending_queued_gateway_prompt_deliveries(&mut self.ctx);
            detect_and_handle_supervisor_resets(&mut self.ctx, &batch_events, now);
            self.ctx.mux.flush_pending_startup_prompts(&self.ctx.rt);
        });
    }

    fn settle_quiet_period(&mut self) {
        for _ in 0..4 {
            std::thread::sleep(AUTOMATION_POLL_DELAY);
            self.service_runtime_once();
        }
    }

    fn has_pending_runtime_work(&self) -> bool {
        !self.ctx.pending_runtime_commands.is_empty()
            || !self.ctx.pending_runtime_approval_resolutions.is_empty()
            || !self.ctx.pending_queued_gateway_prompt_deliveries.is_empty()
    }

    fn has_pending_delayed_prompts(&self) -> bool {
        self.ctx.mux.pending_delayed_prompt_count() > 0
    }

    fn fast_forward_busy_panes(&mut self) {
        self.simulated_now += AUTOMATION_BUSY_SETTLE_ADVANCE;
        self.fast_forward_prompt_retry_backoffs();
    }

    fn monotonic_now(&mut self) -> Instant {
        let real_now = Instant::now();
        if real_now > self.simulated_now {
            self.simulated_now = real_now;
        }
        self.simulated_now
    }

    fn queues_are_idle(&self) -> bool {
        let prompt_dirs = runtime_prompt_queue_sweep_dirs(
            &self.brehon_root,
            self.ctx.runtime_session_name.as_deref(),
        );
        prompt_dirs
            .iter()
            .all(|dir| !queue_dir_has_live_entries(dir))
            && !queue_dir_has_live_entries(
                &self
                    .brehon_root
                    .join("runtime")
                    .join("reviewer-reset-queue"),
            )
            && !queue_dir_has_live_entries(
                &self
                    .brehon_root
                    .join("runtime")
                    .join("worker-recycle-queue"),
            )
    }

    fn describe_live_queue_entries(&self) -> Vec<String> {
        let mut live = Vec::new();
        let mut dirs = runtime_prompt_queue_sweep_dirs(
            &self.brehon_root,
            self.ctx.runtime_session_name.as_deref(),
        );
        dirs.push(
            self.brehon_root
                .join("runtime")
                .join("reviewer-reset-queue"),
        );
        dirs.push(
            self.brehon_root
                .join("runtime")
                .join("worker-recycle-queue"),
        );
        for dir in dirs {
            collect_live_queue_entries(&dir, &mut live);
        }
        live
    }

    fn describe_busy_panes(&self) -> Vec<String> {
        let now = std::cmp::max(self.simulated_now, Instant::now());
        self.ctx
            .mux
            .panes()
            .filter_map(|pane| match pane.pane_state() {
                Some(PaneState::Busy {
                    prompt_id,
                    delivered_at,
                    last_activity_at,
                    ..
                }) => Some(format!(
                    "{} busy prompt_id={} delivered_for_ms={} quiet_for_ms={}",
                    pane.id(),
                    prompt_id,
                    now.saturating_duration_since(*delivered_at).as_millis(),
                    now.saturating_duration_since(*last_activity_at).as_millis(),
                )),
                Some(PaneState::Blocked { info, .. }) => {
                    Some(format!("{} blocked {}", pane.id(), info.summary))
                }
                _ => None,
            })
            .collect()
    }

    fn fast_forward_prompt_retry_backoffs(&self) {
        let mut prompt_files_examined = 0usize;
        let mut prompt_files_fast_forwarded = 0usize;
        let mut prompt_files_failed = 0usize;
        let mut entry_read_failures = 0usize;
        let mut file_type_failures = 0usize;
        let mut read_dir_failures = 0usize;
        for dir in runtime_prompt_queue_sweep_dirs(
            &self.brehon_root,
            self.ctx.runtime_session_name.as_deref(),
        ) {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    tracing::debug!(
                        dir = %dir.display(),
                        "Skipped missing prompt queue directory while fast-forwarding retry backoffs"
                    );
                    continue;
                }
                Err(err) => {
                    read_dir_failures += 1;
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %err,
                        "Failed to read prompt queue directory while fast-forwarding retry backoffs"
                    );
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        entry_read_failures += 1;
                        tracing::warn!(
                            dir = %dir.display(),
                            error = %err,
                            "Failed to inspect prompt queue entry while fast-forwarding retry backoffs"
                        );
                        continue;
                    }
                };
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(err) => {
                        file_type_failures += 1;
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "Failed to inspect prompt queue entry type while fast-forwarding retry backoffs"
                        );
                        continue;
                    }
                };
                if !file_type.is_file() {
                    continue;
                }
                let is_prompt_file = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| matches!(ext, "prompt" | "entry"));
                if is_prompt_file {
                    prompt_files_examined += 1;
                    if force_prompt_retry_due(&path) {
                        prompt_files_fast_forwarded += 1;
                    } else {
                        prompt_files_failed += 1;
                        tracing::warn!(
                            path = %path.display(),
                            "Failed to force queued prompt retry metadata due during automation fast-forward"
                        );
                    }
                }
            }
        }
        if prompt_files_examined > 0
            || prompt_files_failed > 0
            || entry_read_failures > 0
            || file_type_failures > 0
            || read_dir_failures > 0
        {
            tracing::debug!(
                prompt_files_examined,
                prompt_files_fast_forwarded,
                prompt_files_failed,
                entry_read_failures,
                file_type_failures,
                read_dir_failures,
                "Automation fast-forwarded prompt retry backoffs"
            );
        }
    }
}

fn queue_dir_has_live_entries(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            return false;
        };
        if file_type.is_dir() {
            return queue_dir_has_live_entries(&path);
        }
        if !file_type.is_file() {
            return false;
        }
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                !name.starts_with('.') && !name.ends_with(".tmp") && !name.ends_with(".retry.json")
            })
    })
}

fn collect_live_queue_entries(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_live_queue_entries(&path, out);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name.ends_with(".tmp") || name.ends_with(".retry.json") {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .ok()
            .map(|body| body.replace('\n', "\\n"))
            .unwrap_or_else(|| "<binary>".to_string());
        out.push(format!("{} => {}", path.display(), contents));
    }
}
