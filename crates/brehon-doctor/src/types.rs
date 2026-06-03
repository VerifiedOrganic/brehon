//! Diagnostic types shared by all checker modules.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Category of diagnostic finding.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum DiagnosticCategory {
    /// Worktree state issues (stale, detached, uncommitted work).
    Worktree,
    /// Runtime state issues (dead sessions, stuck processes).
    Runtime,
    /// Task lifecycle issues (stuck tasks, missing dependencies).
    Task,
    /// Review/validation issues (stale reviews, missing approvals).
    Review,
    /// Store/search integrity issues (queue leases, view drift, tantivy consistency).
    StoreSearch,
}

impl DiagnosticCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::Runtime => "runtime",
            Self::Task => "task",
            Self::Review => "review",
            Self::StoreSearch => "store-search",
        }
    }
}

impl fmt::Display for DiagnosticCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Severity level for diagnostic findings.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    /// Informational, no action required.
    Info,
    /// Warning, may need attention.
    Warning,
    /// Error, should be fixed.
    Error,
    /// Critical, blocks normal operation.
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Warning => "⚠",
            Self::Error => "✗",
            Self::Critical => "‼",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single diagnostic finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticFinding {
    /// Category of the finding.
    pub category: DiagnosticCategory,
    /// Severity level.
    pub severity: Severity,
    /// Short summary (one line).
    pub summary: String,
    /// Detailed description (optional).
    pub description: Option<String>,
    /// Affected entity (e.g., worktree path, task ID).
    pub subject: Option<String>,
    /// Suggested fix (optional).
    pub suggestion: Option<String>,
}

impl DiagnosticFinding {
    pub fn new(
        category: DiagnosticCategory,
        severity: Severity,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            category,
            severity,
            summary: summary.into(),
            description: None,
            subject: None,
            suggestion: None,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }
}

/// Result of running diagnostic checks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiagnosticReport {
    /// All findings, grouped by category.
    pub findings: Vec<DiagnosticFinding>,
    /// Summary counts by severity.
    pub summary: DiagnosticSummary,
}

/// Summary counts for a diagnostic report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiagnosticSummary {
    pub info_count: usize,
    pub warning_count: usize,
    pub error_count: usize,
    pub critical_count: usize,
    pub total_count: usize,
}

impl DiagnosticSummary {
    pub fn from_findings(findings: &[DiagnosticFinding]) -> Self {
        let mut summary = Self::default();
        for finding in findings {
            summary.total_count += 1;
            match finding.severity {
                Severity::Info => summary.info_count += 1,
                Severity::Warning => summary.warning_count += 1,
                Severity::Error => summary.error_count += 1,
                Severity::Critical => summary.critical_count += 1,
            }
        }
        summary
    }

    pub fn has_issues(&self) -> bool {
        self.error_count > 0 || self.critical_count > 0
    }
}

impl DiagnosticReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_findings(findings: Vec<DiagnosticFinding>) -> Self {
        let summary = DiagnosticSummary::from_findings(&findings);
        Self { findings, summary }
    }

    pub fn add(&mut self, finding: DiagnosticFinding) {
        match finding.severity {
            Severity::Info => self.summary.info_count += 1,
            Severity::Warning => self.summary.warning_count += 1,
            Severity::Error => self.summary.error_count += 1,
            Severity::Critical => self.summary.critical_count += 1,
        }
        self.summary.total_count += 1;
        self.findings.push(finding);
    }

    pub fn findings_by_category(&self, category: DiagnosticCategory) -> Vec<&DiagnosticFinding> {
        self.findings
            .iter()
            .filter(|f| f.category == category)
            .collect()
    }

    pub fn findings_by_severity(&self, severity: Severity) -> Vec<&DiagnosticFinding> {
        self.findings
            .iter()
            .filter(|f| f.severity == severity)
            .collect()
    }

    pub fn has_critical(&self) -> bool {
        self.summary.critical_count > 0
    }

    pub fn has_errors(&self) -> bool {
        self.summary.error_count > 0
    }
}

/// Task status labels for supervisor visibility.
///
/// Distinguishes between:
/// - approved: review passed, not yet integrated
/// - integrated: merged into epic branch
/// - merged: merged to main
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TaskStatusLabel {
    Pending,
    Assigned,
    InProgress,
    InReview,
    ChangesRequested,
    Approved,
    Integrated,
    Merged,
    Closed,
    Blocked,
}

impl TaskStatusLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Assigned => "assigned",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::ChangesRequested => "changes_requested",
            Self::Approved => "approved",
            Self::Integrated => "integrated",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Blocked => "blocked",
        }
    }

    /// Determine status label from task state.
    ///
    /// An approved task with integration_status="integrated" becomes "Integrated".
    /// An approved task with merge_commit becomes "Merged".
    pub fn from_task(
        status: &str,
        integration_status: Option<&str>,
        merged_commit: Option<&str>,
    ) -> Self {
        let normalized = status.to_ascii_lowercase();

        // Check terminal states first
        if let Some(commit) = merged_commit {
            if !commit.is_empty() {
                return Self::Merged;
            }
        }

        // Check integration status for approved tasks
        if normalized == "approved" {
            if let Some(integ) = integration_status {
                if integ == "integrated" {
                    return Self::Integrated;
                }
            }
            return Self::Approved;
        }

        // Standard status mapping
        match normalized.as_str() {
            "pending" => Self::Pending,
            "assigned" => Self::Assigned,
            "in_progress" | "inprogress" => Self::InProgress,
            "in_review" | "inreview" => Self::InReview,
            "changes_requested" | "changesrequested" => Self::ChangesRequested,
            "merged" => Self::Merged,
            "closed" => Self::Closed,
            "blocked" => Self::Blocked,
            _ => Self::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_ordering() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
        assert!(Severity::Error < Severity::Critical);
    }

    #[test]
    fn test_task_status_label_approved() {
        let label = TaskStatusLabel::from_task("approved", None, None);
        assert_eq!(label.as_str(), "approved");
    }

    #[test]
    fn test_task_status_label_integrated() {
        let label = TaskStatusLabel::from_task("approved", Some("integrated"), None);
        assert_eq!(label.as_str(), "integrated");
    }

    #[test]
    fn test_task_status_label_merged() {
        let label = TaskStatusLabel::from_task("approved", None, Some("abc123"));
        assert_eq!(label.as_str(), "merged");
    }

    #[test]
    fn test_diagnostic_report_grouping() {
        let mut report = DiagnosticReport::new();
        report.add(DiagnosticFinding::new(
            DiagnosticCategory::Worktree,
            Severity::Error,
            "Stale worktree",
        ));
        report.add(DiagnosticFinding::new(
            DiagnosticCategory::Task,
            Severity::Warning,
            "Stuck task",
        ));

        let worktree_issues = report.findings_by_category(DiagnosticCategory::Worktree);
        assert_eq!(worktree_issues.len(), 1);

        let warnings = report.findings_by_severity(Severity::Warning);
        assert_eq!(warnings.len(), 1);

        assert_eq!(report.summary.total_count, 2);
        assert_eq!(report.summary.error_count, 1);
        assert_eq!(report.summary.warning_count, 1);
    }
}
