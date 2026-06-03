use serde_json::Value;

use brehon_types::ReviewPolicy;

use super::state::{ConsolidatedReport, ReviewState, StoredFinding};

pub(crate) fn is_supported_review_verdict(verdict: &str) -> bool {
    matches!(
        verdict.trim(),
        "approved" | "needs_revision" | "changes_requested" | "rejected"
    )
}

pub(crate) fn has_actionable_blocking_finding(findings: &[StoredFinding]) -> bool {
    findings.iter().any(|finding| {
        finding.severity.trim() == "blocking" && !finding.description.trim().is_empty()
    })
}

pub(crate) fn negative_review_requires_blocking_finding(
    policy: &ReviewPolicy,
    score: u8,
    verdict: &str,
) -> bool {
    score <= policy.blocking_score
        || matches!(
            verdict.trim(),
            "needs_revision" | "changes_requested" | "rejected"
        )
}

pub(crate) fn unsupported_negative_review_reason(
    policy: &ReviewPolicy,
    score: u8,
    verdict: &str,
    findings: &[StoredFinding],
) -> Option<String> {
    if !is_supported_review_verdict(verdict) {
        return Some(format!("unsupported verdict `{}`", verdict.trim()));
    }

    if negative_review_requires_blocking_finding(policy, score, verdict)
        && !has_actionable_blocking_finding(findings)
    {
        Some(format!(
            "score {score}/10 with verdict `{}` requires at least one `blocking` finding",
            verdict.trim()
        ))
    } else {
        None
    }
}

pub(crate) fn format_stored_finding(finding: &StoredFinding) -> String {
    let location = match (&finding.file, finding.line) {
        (Some(file), Some(line)) => format!("[{file}:{line}] "),
        (Some(file), None) => format!("[{file}] "),
        _ => String::new(),
    };
    match finding
        .suggestion
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(suggestion) => format!(
            "{location}{} — Suggestion: {suggestion}",
            finding.description
        ),
        None => format!("{location}{}", finding.description),
    }
}

pub(crate) fn build_task_review_feedback(
    state: &ReviewState,
    report: &ConsolidatedReport,
) -> Value {
    serde_json::json!({
        "review_id": report.review_id,
        "round": report.round,
        "outcome": report.outcome,
        "panel_id": state.panel_id,
        "panel": state.panel,
        "threshold_result": report.threshold_result,
        "threshold_reason": report.threshold_reason,
        "blocking": report.blocking,
        "suggestions": report.suggestions,
        "nitpicks": report.nitpicks,
        "dissent": report.dissent,
        "evaluated_at": report.evaluated_at,
    })
}

pub(crate) fn build_task_review_followups(report: &ConsolidatedReport) -> Vec<Value> {
    let recorded_at = chrono::Utc::now().to_rfc3339();
    report
        .suggestions
        .iter()
        .chain(report.nitpicks.iter())
        .enumerate()
        .map(|(idx, finding)| {
            serde_json::json!({
                "followup_id": format!("FUP-{}-{}", report.review_id, idx + 1),
                "source_review_id": report.review_id,
                "round": report.round,
                "status": "open",
                "severity": finding.severity,
                "description": finding.description,
                "file": finding.file,
                "line": finding.line,
                "suggestion": finding.suggestion,
                "source": "approved_review",
                "created_at": recorded_at.clone(),
                "updated_at": recorded_at.clone(),
            })
        })
        .collect()
}

pub(crate) fn build_override_feedback(
    state: &ReviewState,
    override_verdict: &str,
    reason: &str,
    overrider: &str,
) -> Value {
    serde_json::json!({
        "review_id": state.current_review_id,
        "round": state.current_round,
        "outcome": override_verdict,
        "panel_id": state.panel_id,
        "panel": state.panel,
        "threshold_result": "supervisor_override",
        "threshold_reason": reason,
        "blocking": [],
        "suggestions": [],
        "nitpicks": [],
        "dissent": [format!("Supervisor override by {overrider}: {reason}")],
        "evaluated_at": chrono::Utc::now().to_rfc3339(),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn format_worker_feedback_message(
    task_id: &str,
    review_id: &str,
    round: u32,
    panel_id: &str,
    outcome: &str,
    threshold_reason: &str,
    blocking: &[StoredFinding],
    suggestions: &[StoredFinding],
) -> String {
    let mut message = format!(
        "Review feedback for task {task_id}\n\
         Review ID: {review_id}\n\
         Round: {round}\n\
         Panel: {panel_id}\n\
         Outcome: {}\n\
         Reason: {threshold_reason}\n",
        outcome.to_uppercase()
    );

    if !blocking.is_empty() {
        message.push_str("\nBlocking issues to fix:\n");
        for (idx, finding) in blocking.iter().enumerate() {
            message.push_str(&format!(
                "  {}. {}\n",
                idx + 1,
                format_stored_finding(finding)
            ));
        }
    }

    if !suggestions.is_empty() {
        message.push_str("\nSuggestions to consider while revising:\n");
        for (idx, finding) in suggestions.iter().enumerate() {
            message.push_str(&format!(
                "  {}. {}\n",
                idx + 1,
                format_stored_finding(finding)
            ));
        }
    }

    message.push_str(
        "\nThe structured review_feedback is now attached to your task. \
         Call `task action=mine` to inspect it before revising.",
    );
    message
}

pub(crate) fn task_status_for_review_outcome(outcome: &str) -> Option<&'static str> {
    match outcome {
        "approved" => Some("approved"),
        "changes_requested" | "rejected" | "escalated" => Some("changes_requested"),
        _ => None,
    }
}
