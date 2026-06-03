//! Report formatting for diagnostic findings.
//!
//! Formats findings grouped by category and severity in human-readable
//! and machine-parseable formats.

use crate::types::{DiagnosticCategory, DiagnosticFinding, DiagnosticReport, Severity};
use std::fmt;

impl fmt::Display for DiagnosticReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "════════════════════════════════════════════════════════════"
        )?;
        writeln!(
            f,
            "                     BREHON DOCTOR REPORT                     "
        )?;
        writeln!(
            f,
            "════════════════════════════════════════════════════════════"
        )?;
        writeln!(f)?;

        // Summary
        writeln!(f, "SUMMARY")?;
        writeln!(f, "───────")?;
        writeln!(
            f,
            "  Total: {}  │  Critical: {}  │  Errors: {}  │  Warnings: {}  │  Info: {}",
            self.summary.total_count,
            self.summary.critical_count,
            self.summary.error_count,
            self.summary.warning_count,
            self.summary.info_count
        )?;
        writeln!(f)?;

        if self.findings.is_empty() {
            writeln!(f, "✓ No issues found.")?;
            return Ok(());
        }

        // Group by category
        let categories = [
            DiagnosticCategory::Worktree,
            DiagnosticCategory::Runtime,
            DiagnosticCategory::Task,
            DiagnosticCategory::Review,
            DiagnosticCategory::StoreSearch,
        ];

        for category in &categories {
            let category_findings = self.findings_by_category(*category);
            if category_findings.is_empty() {
                continue;
            }

            writeln!(f)?;
            writeln!(f, "{} ISSUES", category.to_string().to_uppercase())?;
            writeln!(f, "{}", "─".repeat(50))?;

            // Sort by severity within category
            let mut sorted = category_findings.clone();
            sorted.sort_by_key(|f| std::cmp::Reverse(f.severity));

            for finding in sorted {
                Self::format_finding(f, finding)?;
            }
        }

        // Recommendations
        writeln!(f)?;
        writeln!(f, "RECOMMENDATIONS")?;
        writeln!(f, "──────────────")?;

        let critical_count = self.findings_by_severity(Severity::Critical).len();
        let error_count = self.findings_by_severity(Severity::Error).len();

        if critical_count > 0 {
            writeln!(
                f,
                "  ‼ Address {} critical issue(s) before proceeding.",
                critical_count
            )?;
        }
        if error_count > 0 {
            writeln!(
                f,
                "  ✗ Fix {} error(s) to ensure system health.",
                error_count
            )?;
        }

        let warning_count = self.findings_by_severity(Severity::Warning).len();
        if warning_count > 0 {
            writeln!(
                f,
                "  ⚠ Review {} warning(s) when convenient.",
                warning_count
            )?;
        }

        Ok(())
    }
}

impl DiagnosticReport {
    fn format_finding(f: &mut fmt::Formatter<'_>, finding: &DiagnosticFinding) -> fmt::Result {
        let icon = finding.severity.icon();
        let severity_str = match finding.severity {
            Severity::Critical => "CRITICAL",
            Severity::Error => "ERROR",
            Severity::Warning => "WARNING",
            Severity::Info => "INFO",
        };

        writeln!(f, "  {} [{}] {}", icon, severity_str, finding.summary)?;

        if let Some(ref subject) = finding.subject {
            writeln!(f, "      Subject: {}", subject)?;
        }

        if let Some(ref desc) = finding.description {
            for line in desc.lines() {
                writeln!(f, "        {}", line)?;
            }
        }

        if let Some(ref suggestion) = finding.suggestion {
            writeln!(f, "      → {}", suggestion)?;
        }

        writeln!(f)?;
        Ok(())
    }

    /// Format as JSON for programmatic consumption.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Format as compact one-line per finding for machine parsing.
    pub fn to_compact(&self) -> String {
        fn escape_pipe(s: &str) -> String {
            s.replace('\\', "\\\\").replace('|', "\\|")
        }

        let mut lines = Vec::new();

        // Header
        lines.push(format!(
            "SUMMARY: total={} critical={} error={} warning={} info={}",
            self.summary.total_count,
            self.summary.critical_count,
            self.summary.error_count,
            self.summary.warning_count,
            self.summary.info_count
        ));

        // Each finding on one line
        for finding in &self.findings {
            let severity = finding.severity.as_str();
            let category = finding.category.as_str();
            let subject = escape_pipe(finding.subject.as_deref().unwrap_or("-"));
            let summary = escape_pipe(&finding.summary);
            let suggestion = escape_pipe(finding.suggestion.as_deref().unwrap_or("-"));

            lines.push(format!(
                "FINDING: {}|{}|{}|{}|{}",
                severity, category, subject, summary, suggestion
            ));
        }

        lines.join("\n")
    }
}

/// Generate a quick status summary for supervisor dashboard.
pub fn generate_status_summary(report: &DiagnosticReport) -> String {
    if report.findings.is_empty() {
        return "✓ All checks passed".to_string();
    }

    let parts: Vec<String> = [
        if report.summary.critical_count > 0 {
            Some(format!("{} critical", report.summary.critical_count))
        } else {
            None
        },
        if report.summary.error_count > 0 {
            Some(format!("{} error", report.summary.error_count))
        } else {
            None
        },
        if report.summary.warning_count > 0 {
            Some(format!("{} warning", report.summary.warning_count))
        } else {
            None
        },
        if report.summary.info_count > 0 {
            Some(format!("{} info", report.summary.info_count))
        } else {
            None
        },
    ]
    .into_iter()
    .flatten()
    .collect();

    if parts.is_empty() {
        format!("{} finding(s)", report.summary.total_count)
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_report_display() {
        let report = DiagnosticReport::new();
        let output = format!("{}", report);
        assert!(output.contains("No issues found"));
    }

    #[test]
    fn test_report_with_findings() {
        let mut report = DiagnosticReport::new();
        report.add(
            DiagnosticFinding::new(
                DiagnosticCategory::Worktree,
                Severity::Error,
                "Test finding",
            )
            .with_subject("test-subject"),
        );

        let output = format!("{}", report);
        assert!(output.contains("WORKTREE"));
        assert!(output.contains("Test finding"));
        assert!(output.contains("test-subject"));
    }

    #[test]
    fn test_compact_format() {
        let mut report = DiagnosticReport::new();
        report.add(DiagnosticFinding::new(
            DiagnosticCategory::Task,
            Severity::Warning,
            "Stuck task",
        ));

        let compact = report.to_compact();
        assert!(compact.starts_with("SUMMARY:"));
        assert!(compact.contains("FINDING:"));
    }

    #[test]
    fn test_json_format() {
        let mut report = DiagnosticReport::new();
        report.add(DiagnosticFinding::new(
            DiagnosticCategory::Runtime,
            Severity::Info,
            "Test",
        ));

        let json = report.to_json().unwrap();
        assert!(json.contains("\"category\""));
        assert!(json.contains("\"severity\""));
    }

    #[test]
    fn test_status_summary() {
        let report = DiagnosticReport::new();
        assert_eq!(generate_status_summary(&report), "✓ All checks passed");

        let mut report_with_errors = DiagnosticReport::new();
        report_with_errors.add(DiagnosticFinding::new(
            DiagnosticCategory::Task,
            Severity::Error,
            "Error",
        ));
        let summary = generate_status_summary(&report_with_errors);
        assert!(summary.contains("1 error"));
    }
}
