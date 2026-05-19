//! Proof bundle rendering for the task detail dialog.
//!
//! Reads the compact `ProofSummary` mirrored into
//! `.brehon/runtime/proof/{task_id}.json` by the MCP proof recorders and
//! appends a bounded "Proof of Work" section to the task detail lines.

use brehon_types::ProofSummary;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::layout::{append_bullet_section, append_section_heading};

/// Append a "Proof of Work" section to `lines` for the given task summary.
/// Renders nothing when the task has no proof summary attached, matching
/// other optional task-detail sections (review feedback, integration
/// conflict, etc.).
pub(crate) fn append_proof_section(lines: &mut Vec<Line<'static>>, proof: &ProofSummary) {
    let heading_color = if proof.absent || !proof.missing.is_empty() {
        crate::theme::detail::FINDING_SUGGESTION
    } else {
        crate::theme::status::APPROVED
    };
    append_section_heading(lines, "Proof of Work", heading_color);

    // Status + bundle id metadata row
    let mut header_spans: Vec<Span<'static>> = vec![Span::styled("  ", Style::default())];
    header_spans.push(Span::styled(
        "Status ",
        Style::default().fg(crate::theme::chrome::TEXT_LABEL),
    ));
    header_spans.push(Span::styled(
        proof.status.clone(),
        Style::default()
            .fg(heading_color)
            .add_modifier(Modifier::BOLD),
    ));
    if let Some(ref bundle_id) = proof.proof_bundle_id {
        header_spans.push(Span::styled(
            "  │  ",
            Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
        ));
        header_spans.push(Span::styled(
            "Bundle ",
            Style::default().fg(crate::theme::chrome::TEXT_LABEL),
        ));
        header_spans.push(Span::styled(
            bundle_id.clone(),
            Style::default().fg(Color::White),
        ));
    }
    lines.push(Line::from(header_spans));

    if proof.absent {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "No proof bundle has been recorded for this task yet.",
                Style::default().fg(crate::theme::detail::FINDING_SUGGESTION),
            ),
        ]));
        return;
    }

    let counts = format!(
        "commands {} · tests {} ({} failed) · checks {}",
        proof.command_count, proof.test_count, proof.failed_tests, proof.check_count
    );
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(counts, Style::default().fg(Color::White)),
    ]));

    if !proof.commits.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Commits ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
            Span::styled(proof.commits.join(", "), Style::default().fg(Color::White)),
        ]));
    }

    if let Some(ref diff) = proof.diff_summary {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Diff ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
            Span::styled(diff.clone(), Style::default().fg(Color::White)),
        ]));
    }

    if !proof.reviews.is_empty() {
        let verdicts = if proof.review_verdicts.is_empty() {
            "no verdicts".to_string()
        } else {
            proof.review_verdicts.join(", ")
        };
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Reviews ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
            Span::styled(proof.reviews.join(", "), Style::default().fg(Color::White)),
            Span::styled(
                "  │  ",
                Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
            ),
            Span::styled(
                "Verdicts ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
            Span::styled(verdicts, Style::default().fg(Color::White)),
        ]));
    }

    if let Some(ref status) = proof.integration_status {
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Integration ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
            Span::styled(status.clone(), Style::default().fg(Color::White)),
        ];
        if let Some(ref branch) = proof.integration_branch {
            spans.push(Span::styled(
                "  │  ",
                Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
            ));
            spans.push(Span::styled(
                "Branch ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ));
            spans.push(Span::styled(
                branch.clone(),
                Style::default().fg(Color::White),
            ));
        }
        if let Some(ref base) = proof.integration_base {
            spans.push(Span::styled(
                "  │  ",
                Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
            ));
            spans.push(Span::styled(
                "Base ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ));
            spans.push(Span::styled(
                base.clone(),
                Style::default().fg(Color::White),
            ));
        }
        if let Some(ref commit) = proof.integration_commit {
            spans.push(Span::styled(
                "  │  ",
                Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
            ));
            spans.push(Span::styled(
                "Commit ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ));
            spans.push(Span::styled(
                commit.clone(),
                Style::default().fg(Color::White),
            ));
        }
        lines.push(Line::from(spans));
        if !proof.integration_conflicts.is_empty() {
            append_bullet_section(
                lines,
                "Integration conflicts",
                &proof.integration_conflicts,
                crate::theme::detail::FINDING_BLOCKING,
            );
        }
    }

    if !proof.open_blockers.is_empty() {
        append_bullet_section(
            lines,
            "Open proof blockers",
            &proof.open_blockers,
            crate::theme::detail::FINDING_BLOCKING,
        );
    }
    if !proof.review_findings.is_empty() {
        append_bullet_section(
            lines,
            "Review findings (recorded)",
            &proof.review_findings,
            crate::theme::detail::FINDING_SUGGESTION,
        );
    }
    if !proof.followups.is_empty() {
        append_bullet_section(
            lines,
            "Recorded followups",
            &proof.followups,
            crate::theme::detail::FINDING_NITPICK,
        );
    }
    if !proof.missing.is_empty() {
        append_bullet_section(
            lines,
            "Missing or incomplete evidence",
            &proof.missing,
            crate::theme::detail::FINDING_SUGGESTION,
        );
    }
    if let Some(ref updated_at) = proof.updated_at {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Updated ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
            Span::styled(updated_at.clone(), Style::default().fg(Color::White)),
        ]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_to_string(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn recorded_summary() -> ProofSummary {
        let mut summary = ProofSummary::absent();
        summary.absent = false;
        summary.proof_bundle_id = Some("proof-T-tui".to_string());
        summary.status = "incomplete".to_string();
        summary.command_count = 4;
        summary.test_count = 2;
        summary.failed_tests = 1;
        summary.check_count = 1;
        summary.commits = vec!["abc1234".to_string(), "def5678".to_string()];
        summary.diff_summary = Some("3 files changed".to_string());
        summary.reviews = vec!["REV-1".to_string()];
        summary.review_verdicts = vec!["changes_requested".to_string()];
        summary.integration_status = Some("integrated".to_string());
        summary.integration_branch = Some("worker/x".to_string());
        summary.integration_base = Some("epic/y".to_string());
        summary.integration_commit = Some("deadbeef".to_string());
        summary.open_blockers = vec!["needs final test".to_string()];
        summary.review_findings = vec!["missing edge case test".to_string()];
        summary.followups = vec!["add unit test".to_string()];
        summary.missing = vec!["1 test result(s) recorded as failed.".to_string()];
        summary.updated_at = Some("2026-05-16T00:00:00Z".to_string());
        summary
    }

    #[test]
    fn renders_proof_section_with_command_check_review_and_integration_summary() {
        let summary = recorded_summary();
        let mut lines = Vec::new();
        append_proof_section(&mut lines, &summary);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Proof of Work"));
        assert!(rendered.contains("Bundle proof-T-tui"));
        assert!(rendered.contains("commands 4"));
        assert!(rendered.contains("tests 2 (1 failed)"));
        assert!(rendered.contains("checks 1"));
        assert!(rendered.contains("abc1234"));
        assert!(rendered.contains("REV-1"));
        assert!(rendered.contains("changes_requested"));
        assert!(rendered.contains("Integration "));
        assert!(rendered.contains("worker/x"));
        assert!(rendered.contains("epic/y"));
        assert!(rendered.contains("deadbeef"));
    }

    #[test]
    fn highlights_missing_evidence_and_blockers() {
        let summary = recorded_summary();
        let mut lines = Vec::new();
        append_proof_section(&mut lines, &summary);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Open proof blockers"));
        assert!(rendered.contains("needs final test"));
        assert!(rendered.contains("Missing or incomplete evidence"));
        assert!(rendered.contains("recorded as failed"));
    }

    #[test]
    fn absent_summary_renders_explicit_no_proof_recorded_line() {
        let summary = ProofSummary::absent();
        let mut lines = Vec::new();
        append_proof_section(&mut lines, &summary);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Proof of Work"));
        assert!(rendered.contains("No proof bundle has been recorded"));
    }
}
