//! Crash and reset detection helpers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use brehon_mux::{ActivityEntry, ActivityKind, Mux, PaneKind};

use super::recovery::push_dashboard_event;
use super::types::*;

pub(crate) fn is_context_length_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let mentions_prompt = lower.contains("prompt is too long") || lower.contains("prompt too long");
    let mentions_context = lower.contains("maximum context length")
        || lower.contains("max context length")
        || lower.contains("exceeded max context length");
    mentions_prompt && mentions_context
}

pub(crate) fn is_stream_disconnect_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("stream disconnected before completion")
        || (lower.contains("processing your request")
            && (lower.contains("you can retry your request")
                || lower.contains("contact us through our help center")))
}

pub(crate) fn supervisor_viewport_contains_runtime_crash(viewport: &str) -> bool {
    let lower = viewport.to_ascii_lowercase();
    let has_cli_stack = lower.contains("/bunfs/root/src/entrypoints/cli.js:")
        || lower.contains("entrypoints/cli.js:");
    let has_runtime_failure = lower.contains("<anonymous>")
        || lower.contains("typeerror")
        || lower.contains("referenceerror")
        || lower.contains("syntaxerror")
        || lower.contains("cannot read properties")
        || lower.contains("bgblackbright");
    has_cli_stack && has_runtime_failure
}

pub(crate) fn supervisor_reset_reason(mux: &Mux, pane_id: &str) -> Option<&'static str> {
    let pane = mux.get(pane_id)?;
    if pane.kind() != &PaneKind::Supervisor {
        return None;
    }
    if pane.has_exited() {
        return Some("process exited");
    }
    let viewport = pane.dump_viewport().ok()?;
    if supervisor_viewport_contains_runtime_crash(&viewport) {
        return Some("runtime crash");
    }
    None
}

pub(crate) fn perform_manual_pane_reset(
    mux: &mut Mux,
    pane_id: &str,
    rt: &tokio::runtime::Handle,
    dashboard_data: &Arc<std::sync::Mutex<DashboardData>>,
    last_activity: &mut HashMap<String, Instant>,
    pending_self_improve_prompt: &mut HashMap<String, Instant>,
    host_owned: bool,
) -> bool {
    let Some(pane) = mux.get(pane_id) else {
        return false;
    };

    let reset_result = match pane.kind() {
        PaneKind::Worker => {
            let startup_prompt = if super::helpers::pane_needs_post_spawn_prompt(mux, pane_id) {
                let Some(startup_prompt) =
                    super::build_worker_context_reset_startup_prompt(mux, pane_id)
                else {
                    return false;
                };
                Some(startup_prompt)
            } else {
                None
            };
            rt.block_on(mux.reset_worker_gateway_session(pane_id))
                .map(|_| {
                    if let Some(startup_prompt) = startup_prompt {
                        mux.queue_startup_prompt(pane_id, startup_prompt);
                    }
                    format!("manually reset worker {pane_id}")
                })
                .map_err(|err| err.to_string())
        }
        PaneKind::Reviewer => {
            if pane.review_context().is_some() {
                Err(format!(
                    "cannot manually reset reviewer {pane_id} while an active review is in progress"
                ))
            } else {
                let startup_prompt = if super::helpers::pane_needs_post_spawn_prompt(mux, pane_id) {
                    let Some(startup_prompt) =
                        super::build_reviewer_reset_startup_prompt(mux, pane_id)
                    else {
                        return false;
                    };
                    Some(startup_prompt)
                } else {
                    None
                };
                rt.block_on(mux.reset_reviewer_session(pane_id))
                    .map(|_| {
                        if let Some(startup_prompt) = startup_prompt {
                            mux.queue_startup_prompt(pane_id, startup_prompt);
                        }
                        format!("manually reset reviewer {pane_id}")
                    })
                    .map_err(|err| err.to_string())
            }
        }
        PaneKind::Advisor => {
            let startup_prompt = if super::helpers::pane_needs_post_spawn_prompt(mux, pane_id) {
                let Some(startup_prompt) = super::build_advisor_reset_startup_prompt(mux, pane_id)
                else {
                    return false;
                };
                Some(startup_prompt)
            } else {
                None
            };
            rt.block_on(mux.reset_advisor_session(pane_id))
                .map(|_| {
                    if let Some(startup_prompt) = startup_prompt {
                        mux.queue_startup_prompt(pane_id, startup_prompt);
                    }
                    format!("manually reset advisor {pane_id}")
                })
                .map_err(|err| err.to_string())
        }
        PaneKind::Research => {
            let startup_prompt = if super::helpers::pane_needs_post_spawn_prompt(mux, pane_id) {
                let Some(startup_prompt) = super::build_research_reset_startup_prompt(mux, pane_id)
                else {
                    return false;
                };
                Some(startup_prompt)
            } else {
                None
            };
            rt.block_on(mux.reset_research_session(pane_id))
                .map(|_| {
                    if let Some(startup_prompt) = startup_prompt {
                        mux.queue_startup_prompt(pane_id, startup_prompt);
                    }
                    format!("manually reset research agent {pane_id}")
                })
                .map_err(|err| err.to_string())
        }
        PaneKind::Supervisor => {
            let startup_prompt = if super::helpers::pane_needs_post_spawn_prompt(mux, pane_id) {
                let Some(startup_prompt) =
                    super::build_supervisor_reset_startup_prompt(mux, pane_id, host_owned)
                else {
                    return false;
                };
                Some(startup_prompt)
            } else {
                None
            };
            rt.block_on(mux.reset_supervisor_session(pane_id))
                .map(|_| {
                    if let Some(startup_prompt) = startup_prompt {
                        mux.queue_startup_prompt(pane_id, startup_prompt);
                    }
                    format!("manually reset supervisor {pane_id}")
                })
                .map_err(|err| err.to_string())
        }
        PaneKind::Director | PaneKind::Shell => Err(format!(
            "manual reset is not supported for {} pane {pane_id}",
            match pane.kind() {
                PaneKind::Director => "director",
                PaneKind::Shell => "shell",
                _ => unreachable!(),
            }
        )),
    };

    match reset_result {
        Ok(summary) => {
            let now = Instant::now();
            last_activity.insert(pane_id.to_string(), now);
            pending_self_improve_prompt.remove(pane_id);
            push_dashboard_event(dashboard_data, summary.clone());
            tracing::warn!(pane = %pane_id, "{summary}");
            true
        }
        Err(err) => {
            let summary = format!("manual reset for {pane_id} failed: {err}");
            push_dashboard_event(dashboard_data, summary.clone());
            tracing::warn!(pane = %pane_id, error = %err, "{summary}");
            false
        }
    }
}

pub(crate) fn is_worker_context_reset_candidate(
    mux: &Mux,
    pane_id: &str,
    entry: &ActivityEntry,
) -> bool {
    if entry.kind != ActivityKind::Progress {
        return false;
    }
    let Some(message) = entry.message.as_deref() else {
        return false;
    };
    if !(is_context_length_error_message(message) || is_stream_disconnect_error_message(message)) {
        return false;
    }
    let Some(pane) = mux.get(pane_id) else {
        return false;
    };
    pane.kind() == &PaneKind::Worker
        && pane.is_gateway_backed()
        && pane.cli_type().needs_post_spawn_prompt()
}
