//! Orchestrator application of bounded continuation decisions.

use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use brehon_ports::{RunContinuation, RunStore};
use brehon_types::{
    Event, EventKind, HealthStatus, MessageKind, PromptId, PromptTurn, RunRecord, RunStatus,
    SessionId, TaskStatus,
};

use crate::continuation::{
    decide_continuation, ContinuationDecision, ContinuationInput, ContinuationReviewState,
    ContinuationSessionHealth,
};
use crate::error::{OrchestratorError, Result};
use crate::orchestrator::Orchestrator;
use crate::task_board::TaskEntry;

#[derive(Debug, Default)]
pub(crate) struct ContinuationApplyReport {
    pub continued: usize,
    pub stopped: usize,
    pub escalated: usize,
    pub no_action: usize,
    pub events_emitted: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinuationPromptOutcome {
    Sent,
    Escalated,
}

impl Orchestrator {
    pub(crate) async fn continue_active_runs(&mut self) -> Result<ContinuationApplyReport> {
        let Some(run_store) = self.run_store.clone() else {
            return Ok(ContinuationApplyReport::default());
        };
        let now = Utc::now();
        let active_runs = run_store
            .active_runs()
            .await
            .map_err(|err| OrchestratorError::PortError(err.to_string()))?;
        let mut report = ContinuationApplyReport::default();
        let mut events = Vec::new();

        for run in active_runs
            .iter()
            .filter(|run| matches!(run.status, RunStatus::Claimed | RunStatus::Running))
        {
            self.apply_continuation_decision(
                run_store.as_ref(),
                run,
                now,
                &mut events,
                &mut report,
            )
            .await?;
        }

        report.events_emitted = self.emit_continuation_events(events).await?;
        Ok(report)
    }

    async fn apply_continuation_decision(
        &self,
        run_store: &dyn RunStore,
        run: &RunRecord,
        now: DateTime<Utc>,
        events: &mut Vec<Event>,
        report: &mut ContinuationApplyReport,
    ) -> Result<()> {
        let task = self.task_board.get_task(&run.task_id);
        let session_health = self.continuation_session_health(run).await;
        let input = continuation_input(run, task.as_ref(), session_health, now);

        match decide_continuation(input, self.config.continuation_policy) {
            ContinuationDecision::ContinueNow { next_turn, reason } => {
                let outcome = self
                    .send_continuation_prompt(run_store, run, task.as_ref(), next_turn, now, events)
                    .await?;
                match outcome {
                    ContinuationPromptOutcome::Sent => {
                        report.continued += 1;
                        debug!(run_id = %run.run_id, next_turn, reason, "Sent continuation prompt");
                    }
                    ContinuationPromptOutcome::Escalated => {
                        report.escalated += 1;
                    }
                }
            }
            ContinuationDecision::Stop { reason } => {
                report.stopped += 1;
                debug!(run_id = %run.run_id, reason, "Continuation stopped");
            }
            ContinuationDecision::Escalate { reason } => {
                report.escalated += 1;
                events.push(continuation_escalation_event(run, reason, now));
            }
            ContinuationDecision::NoAction { reason } => {
                report.no_action += 1;
                debug!(run_id = %run.run_id, reason, "No continuation action");
            }
        }

        Ok(())
    }

    async fn continuation_session_health(&self, run: &RunRecord) -> ContinuationSessionHealth {
        let Some(session_id) = run.session_id.as_ref() else {
            return ContinuationSessionHealth::Missing;
        };

        match self.deps.gateway.health_check(session_id).await {
            Ok(HealthStatus::Healthy) => ContinuationSessionHealth::Healthy,
            Ok(HealthStatus::Unhealthy) => ContinuationSessionHealth::Unhealthy,
            Ok(HealthStatus::Unknown) => ContinuationSessionHealth::Unknown,
            Err(error) => {
                warn!(run_id = %run.run_id, session_id = %session_id, error = ?error, "Failed to read continuation session health");
                ContinuationSessionHealth::Missing
            }
        }
    }

    async fn send_continuation_prompt(
        &self,
        run_store: &dyn RunStore,
        run: &RunRecord,
        task: Option<&TaskEntry>,
        next_turn: u32,
        now: DateTime<Utc>,
        events: &mut Vec<Event>,
    ) -> Result<ContinuationPromptOutcome> {
        let Some(session_id) = run.session_id.as_ref() else {
            events.push(continuation_escalation_event(
                run,
                "run has no session for continuation",
                now,
            ));
            return Ok(ContinuationPromptOutcome::Escalated);
        };

        let prompt = build_continuation_prompt(run, task, next_turn, now);
        match self.deps.gateway.send_prompt(session_id, prompt).await {
            Ok(_) => {
                let updated = run_store
                    .record_continuation(RunContinuation::new(
                        run.run_id.clone(),
                        run.claim_generation,
                        now,
                        next_turn,
                    ))
                    .await
                    .map_err(|err| OrchestratorError::PortError(err.to_string()))?;
                events.push(continuation_activity_event(&updated, next_turn, now));
                Ok(ContinuationPromptOutcome::Sent)
            }
            Err(error) => {
                warn!(run_id = %run.run_id, session_id = %session_id, error = ?error, "Failed to send continuation prompt");
                events.push(continuation_escalation_event(
                    run,
                    "failed to send continuation prompt",
                    now,
                ));
                Ok(ContinuationPromptOutcome::Escalated)
            }
        }
    }

    async fn emit_continuation_events(&self, events: Vec<Event>) -> Result<usize> {
        let mut emitted = 0;
        for event in events {
            self.deps.event_store.append(event).await?;
            emitted += 1;
        }
        Ok(emitted)
    }
}

fn continuation_input(
    run: &RunRecord,
    task: Option<&TaskEntry>,
    session_health: ContinuationSessionHealth,
    now: DateTime<Utc>,
) -> ContinuationInput {
    let task_status = task.map(|task| task.status).unwrap_or(TaskStatus::Pending);
    let run_matches_task = task.is_some_and(|task| task.id == run.task_id);
    let session_matches_task = task
        .and_then(|task| task.session_id.as_deref())
        .zip(run.session_id.as_ref().map(SessionId::as_str))
        .is_some_and(|(task_session, run_session)| task_session == run_session);
    let idle_since = run
        .last_continuation_at
        .or(run.last_activity_at)
        .or(run.started_at)
        .or(run.claimed_at)
        .unwrap_or(run.updated_at);

    ContinuationInput {
        task_status,
        run_status: run.status,
        session_health,
        turn_count: run.continuation_turns,
        run_matches_task,
        session_matches_task,
        review_state: review_state_for_task(task_status),
        idle_for_secs: (now - idle_since).num_seconds().max(0) as u64,
    }
}

fn review_state_for_task(status: TaskStatus) -> Option<ContinuationReviewState> {
    match status {
        TaskStatus::InReview => Some(ContinuationReviewState::WaitingForReview),
        TaskStatus::Approved => Some(ContinuationReviewState::Approved),
        TaskStatus::ChangesRequested => Some(ContinuationReviewState::ChangesRequested),
        _ => Some(ContinuationReviewState::NotInReview),
    }
}

fn build_continuation_prompt(
    run: &RunRecord,
    task: Option<&TaskEntry>,
    next_turn: u32,
    now: DateTime<Utc>,
) -> PromptTurn {
    let title = task
        .map(|task| task.title.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("current task");
    let status = task
        .map(|task| format!("{:?}", task.status))
        .unwrap_or_else(|| "unknown".to_string());
    let content = format!(
        "Continue task {} ({title}). State: {status}. Run: {} attempt {} continuation turn {next_turn}. Continue from existing session context; report completion or blockers.",
        run.task_id, run.run_id, run.attempt
    );

    PromptTurn {
        prompt_id: PromptId::new(uuid::Uuid::new_v4().to_string()),
        content,
        kind: MessageKind::Nudge,
        sent_at: now,
    }
}

fn continuation_activity_event(record: &RunRecord, turn: u32, now: DateTime<Utc>) -> Event {
    Event {
        aggregate_id: record.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::RunActivityObserved {
            run_id: record.run_id.clone(),
            task_id: record.task_id.clone(),
            role: record.role.clone(),
            generation: record.claim_generation,
            activity: format!("continuation prompt sent: turn {turn}"),
            observed_at: now,
        },
    }
}

fn continuation_escalation_event(
    run: &RunRecord,
    reason: &'static str,
    now: DateTime<Utc>,
) -> Event {
    Event {
        aggregate_id: run.task_id.as_str().to_string(),
        timestamp: now,
        kind: EventKind::EscalationTriggered {
            reason: format!("continuation decision escalated: {reason}"),
            context: format!(
                "run {} task {} role {} attempt {} status {}",
                run.run_id, run.task_id, run.role, run.attempt, run.status
            ),
        },
    }
}
