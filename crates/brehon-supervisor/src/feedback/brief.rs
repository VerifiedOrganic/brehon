//! Bounded feedback brief builder.
//!
//! `build_brief` is a pure function that converts a `FeedbackTrigger` plus
//! optional context snapshots into a `FeedbackBrief`. Briefs are
//! deterministic given the same inputs and never exceed
//! `policy.max_brief_bytes` total body bytes. Missing context is recorded
//! as explicit "missing" sections rather than silently omitted.

use chrono::Utc;

use brehon_types::{
    FeedbackBrief, FeedbackBriefSection, FeedbackPolicy, FeedbackTrigger, FeedbackTurnId,
    ProofSummary, FEEDBACK_CONTRACT_VERSION,
};

/// Optional context snapshots the brief builder consumes. Any field may be
/// `None`; the builder renders a `missing` section in that case so
/// supervisor responses cannot pretend the data was present.
#[derive(Debug, Clone, Default)]
pub struct BriefSourceContext {
    /// Compact task summary text (id, title, status, assignee).
    pub task_summary: Option<String>,
    /// Compact active-run summary text (run id, role, status, attempt).
    pub active_run: Option<String>,
    /// Compact review state summary text.
    pub review_state: Option<String>,
    /// Proof summary for the task.
    pub proof: Option<ProofSummary>,
    /// Bounded list of recent event summary lines.
    pub recent_events: Vec<String>,
    /// Optional notes about policy constraints (e.g., "drain disabled").
    pub policy_notes: Vec<String>,
    /// Optional explicit turn id; when absent the builder synthesizes one.
    pub turn_id: Option<FeedbackTurnId>,
}

/// Input for `build_brief`.
#[derive(Debug, Clone)]
pub struct BriefBuildInput<'a> {
    pub trigger: &'a FeedbackTrigger,
    pub context: &'a BriefSourceContext,
    pub policy: &'a FeedbackPolicy,
}

/// Build a bounded, deterministic feedback brief from the given inputs.
pub fn build_brief(input: &BriefBuildInput<'_>) -> FeedbackBrief {
    let mut budget = input.policy.max_brief_bytes;
    let mut sections: Vec<FeedbackBriefSection> = Vec::new();
    let mut truncated_any = false;

    // -- trigger section ---------------------------------------------------
    let trigger_body = format!(
        "kind: {}\nsummary: {}\nsource_events: {}\nevent_range: {}",
        input.trigger.kind.as_str(),
        input.trigger.summary,
        format_event_ids(&input.trigger.source_event_ids),
        format_range(&input.trigger.covered_event_range),
    );
    push_section(
        &mut sections,
        &mut budget,
        &mut truncated_any,
        "trigger",
        &trigger_body,
        false,
    );

    // -- task section ------------------------------------------------------
    match input.context.task_summary.as_deref() {
        Some(text) => push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "task",
            text,
            false,
        ),
        None => push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "task",
            "(no task summary available)",
            true,
        ),
    }

    // -- active run section ------------------------------------------------
    match input.context.active_run.as_deref() {
        Some(text) => push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "active_run",
            text,
            false,
        ),
        None => push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "active_run",
            "(no active run snapshot)",
            true,
        ),
    }

    // -- review state section ---------------------------------------------
    match input.context.review_state.as_deref() {
        Some(text) => push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "review",
            text,
            false,
        ),
        None => push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "review",
            "(no review state snapshot)",
            true,
        ),
    }

    // -- proof section ----------------------------------------------------
    match input.context.proof.as_ref() {
        Some(summary) => {
            let text = summary.render_text();
            push_section(
                &mut sections,
                &mut budget,
                &mut truncated_any,
                "proof",
                &text,
                false,
            );
        }
        None => push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "proof",
            "(no proof bundle attached)",
            true,
        ),
    }

    // -- recent events section --------------------------------------------
    if input.context.recent_events.is_empty() {
        push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "recent_events",
            "(no recent event summaries supplied)",
            true,
        );
    } else {
        let joined = input.context.recent_events.join("\n");
        push_section(
            &mut sections,
            &mut budget,
            &mut truncated_any,
            "recent_events",
            &joined,
            false,
        );
    }

    // -- policy section ---------------------------------------------------
    let mut policy_lines: Vec<String> = Vec::new();
    policy_lines.push(format!(
        "rationale_min={} rationale_max={}",
        input.policy.rationale_min_chars, input.policy.rationale_max_chars
    ));
    policy_lines.push(format!(
        "max_brief_bytes={} allow_drain={} allow_safe_mode={}",
        input.policy.max_brief_bytes, input.policy.allow_drain, input.policy.allow_safe_mode
    ));
    policy_lines.push(format!(
        "allowed_outcomes={}",
        format_outcomes(&input.policy.allowed_outcomes)
    ));
    for note in &input.context.policy_notes {
        policy_lines.push(note.clone());
    }
    let policy_body = policy_lines.join("\n");
    push_section(
        &mut sections,
        &mut budget,
        &mut truncated_any,
        "policy",
        &policy_body,
        false,
    );

    let total_bytes: usize = sections.iter().map(|section| section.body.len()).sum();
    let has_missing_context = sections.iter().any(|section| section.missing);
    let turn_id = input
        .context
        .turn_id
        .clone()
        .unwrap_or_else(|| synthesize_turn_id(input.trigger));
    let allowed_outcomes = input
        .policy
        .allowed_outcomes
        .iter()
        .filter(|kind| input.policy.allows(**kind))
        .copied()
        .collect();

    FeedbackBrief {
        turn_id,
        contract_version: FEEDBACK_CONTRACT_VERSION,
        trigger: input.trigger.clone(),
        sections,
        total_bytes,
        truncated: truncated_any,
        has_missing_context,
        allowed_outcomes,
        rationale_max_chars: input.policy.rationale_max_chars,
        rationale_min_chars: input.policy.rationale_min_chars,
        built_at: Utc::now(),
    }
}

fn push_section(
    sections: &mut Vec<FeedbackBriefSection>,
    budget: &mut usize,
    truncated_any: &mut bool,
    heading: &str,
    raw_body: &str,
    missing: bool,
) {
    let ellipsis = "…";
    let ellipsis_len = ellipsis.len();

    if *budget == 0 {
        // No bytes left: emit a zero-byte marker section so the brief
        // still reports section coverage but never exceeds the budget.
        sections.push(FeedbackBriefSection {
            heading: heading.to_string(),
            body: String::new(),
            truncated: true,
            missing,
        });
        *truncated_any = true;
        return;
    }

    let trimmed = raw_body.trim();
    let (body, truncated) = if trimmed.len() <= *budget {
        (trimmed.to_string(), false)
    } else if *budget <= ellipsis_len {
        // Budget is too small to hold even the ellipsis marker; fit what
        // we can and skip the marker. Still record truncation.
        let cut = char_safe_truncate(trimmed, *budget);
        (cut, true)
    } else {
        let cut = char_safe_truncate(trimmed, budget.saturating_sub(ellipsis_len));
        let mut body = cut;
        body.push_str(ellipsis);
        (body, true)
    };
    if truncated {
        *truncated_any = true;
    }
    *budget = budget.saturating_sub(body.len());
    sections.push(FeedbackBriefSection {
        heading: heading.to_string(),
        body,
        truncated,
        missing,
    });
}

/// Truncate `text` to at most `max_bytes` while preserving UTF-8 char
/// boundaries. Returns a fresh string.
fn char_safe_truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = 0usize;
    for (idx, ch) in text.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    text[..end].to_string()
}

fn synthesize_turn_id(trigger: &FeedbackTrigger) -> FeedbackTurnId {
    let range = match trigger.covered_event_range {
        Some((low, high)) => format!("{}-{}", low.as_u64(), high.as_u64()),
        None => "no-range".to_string(),
    };
    FeedbackTurnId::new(format!("turn-{}-{range}", trigger.trigger_id.as_str()))
}

fn format_event_ids(ids: &[brehon_types::EventId]) -> String {
    if ids.is_empty() {
        return "none".to_string();
    }
    ids.iter()
        .map(|id| id.as_u64().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_range(range: &Option<(brehon_types::EventId, brehon_types::EventId)>) -> String {
    match range {
        Some((low, high)) => format!("[{}..{}]", low.as_u64(), high.as_u64()),
        None => "(none)".to_string(),
    }
}

fn format_outcomes(set: &std::collections::BTreeSet<brehon_types::FeedbackOutcomeKind>) -> String {
    let mut items: Vec<&str> = set.iter().map(|kind| kind.as_str()).collect();
    items.sort_unstable();
    items.join(", ")
}

#[cfg(test)]
mod feedback_brief_tests {
    use super::*;
    use brehon_types::{EventId, FeedbackTrigger, FeedbackTriggerId, FeedbackTriggerKind, TaskId};

    fn trigger() -> FeedbackTrigger {
        FeedbackTrigger {
            trigger_id: FeedbackTriggerId::new("fb-trig-1"),
            kind: FeedbackTriggerKind::ReviewerFollowup,
            task_id: Some(TaskId::new("T-brief")),
            run_id: None,
            review_id: None,
            source_event_ids: vec![EventId::new(7), EventId::new(8)],
            covered_event_range: Some((EventId::new(7), EventId::new(8))),
            summary: "Open follow-up FUP-1".into(),
            payload: serde_json::json!({}),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn brief_renders_all_required_sections_with_recorded_context() {
        let trigger = trigger();
        let mut summary = ProofSummary::absent();
        summary.absent = false;
        summary.proof_bundle_id = Some("proof-T-brief".into());
        summary.status = "incomplete".into();
        summary.command_count = 3;
        let context = BriefSourceContext {
            task_summary: Some("T-brief — Title — status=in_review".into()),
            active_run: Some("run-x role=worker attempt=2/3 status=running".into()),
            review_state: Some("round=1 outcome=changes_requested approval_count=1".into()),
            proof: Some(summary),
            recent_events: vec![
                "ev=7 ReviewChangesRequested REV-1".into(),
                "ev=8 ReviewerFollowupCreated FUP-1".into(),
            ],
            policy_notes: vec!["drain disabled".into()],
            turn_id: None,
        };
        let policy = FeedbackPolicy::conservative();
        let brief = build_brief(&BriefBuildInput {
            trigger: &trigger,
            context: &context,
            policy: &policy,
        });
        let headings: Vec<&str> = brief.sections.iter().map(|s| s.heading.as_str()).collect();
        for expected in [
            "trigger",
            "task",
            "active_run",
            "review",
            "proof",
            "recent_events",
            "policy",
        ] {
            assert!(
                headings.contains(&expected),
                "missing section {expected}: {headings:?}"
            );
        }
        assert!(!brief.truncated);
        assert!(!brief.has_missing_context);
        assert_eq!(brief.contract_version, FEEDBACK_CONTRACT_VERSION);
        assert_eq!(
            brief.total_bytes,
            brief.sections.iter().map(|s| s.body.len()).sum::<usize>()
        );
    }

    #[test]
    fn missing_context_is_marked_explicitly_per_section() {
        let trigger = trigger();
        let context = BriefSourceContext::default();
        let policy = FeedbackPolicy::conservative();
        let brief = build_brief(&BriefBuildInput {
            trigger: &trigger,
            context: &context,
            policy: &policy,
        });
        let missing: Vec<&str> = brief
            .sections
            .iter()
            .filter(|s| s.missing)
            .map(|s| s.heading.as_str())
            .collect();
        for expected in ["task", "active_run", "review", "proof", "recent_events"] {
            assert!(
                missing.contains(&expected),
                "{expected} should be marked missing: {missing:?}"
            );
        }
        assert!(brief.has_missing_context);
    }

    #[test]
    fn brief_enforces_max_byte_bound_and_marks_truncation() {
        let trigger = trigger();
        let context = BriefSourceContext {
            task_summary: Some("T".repeat(2000)),
            active_run: Some("R".repeat(2000)),
            review_state: Some("V".repeat(2000)),
            proof: None,
            recent_events: vec!["E".repeat(2000)],
            policy_notes: Vec::new(),
            turn_id: None,
        };
        let mut policy = FeedbackPolicy::conservative();
        policy.max_brief_bytes = 512;
        let brief = build_brief(&BriefBuildInput {
            trigger: &trigger,
            context: &context,
            policy: &policy,
        });
        assert!(brief.truncated);
        assert!(
            brief.total_bytes <= policy.max_brief_bytes,
            "total_bytes={} exceeded budget {}",
            brief.total_bytes,
            policy.max_brief_bytes
        );
    }

    #[test]
    fn brief_is_deterministic_when_turn_id_supplied() {
        let trigger = trigger();
        let turn_id = FeedbackTurnId::new("turn-fixed");
        let context = BriefSourceContext {
            turn_id: Some(turn_id.clone()),
            ..Default::default()
        };
        let policy = FeedbackPolicy::conservative();
        let first = build_brief(&BriefBuildInput {
            trigger: &trigger,
            context: &context,
            policy: &policy,
        });
        let second = build_brief(&BriefBuildInput {
            trigger: &trigger,
            context: &context,
            policy: &policy,
        });
        assert_eq!(first.turn_id, second.turn_id);
        // Section bodies are deterministic; only the built_at differs.
        let body_first: Vec<&String> = first.sections.iter().map(|s| &s.body).collect();
        let body_second: Vec<&String> = second.sections.iter().map(|s| &s.body).collect();
        assert_eq!(body_first, body_second);
    }
}
