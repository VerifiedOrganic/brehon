//! Doctor entry point - runs all checks and produces reports.
//!
//! This is the main interface for the diagnostic system.

use crate::checkers::run_all_checks;
use crate::types::{DiagnosticFinding, DiagnosticReport};
use std::path::Path;

/// Run all diagnostic checks and return a report.
///
/// # Arguments
/// * `brehon_root` - Path to the .brehon directory
///
/// # Returns
/// A `DiagnosticReport` containing all findings.
pub fn run_doctor(brehon_root: &Path) -> DiagnosticReport {
    run_doctor_with_path(brehon_root)
}

/// Internal implementation that handles paths.
fn run_doctor_with_path(brehon_root: &Path) -> DiagnosticReport {
    let runtime_dir = brehon_root.join("runtime");

    let findings = match run_all_checks(brehon_root, &runtime_dir) {
        Ok(f) => f,
        Err(e) => {
            // If we can't run checks, return a single critical finding
            return DiagnosticReport::with_findings(vec![DiagnosticFinding::new(
                crate::types::DiagnosticCategory::Runtime,
                crate::types::Severity::Critical,
                format!("Failed to run diagnostics: {}", e),
            )
            .with_suggestion("Check .brehon directory permissions and structure")]);
        }
    };

    DiagnosticReport::with_findings(findings)
}

/// Run doctor checks and format output for CLI.
///
/// Returns (human_readable_report, has_critical_errors).
pub fn run_doctor_cli(brehon_root: &Path) -> (String, bool) {
    let report = run_doctor(brehon_root);
    let has_critical = report.has_critical() || report.has_errors();
    (format!("{}", report), has_critical)
}

/// Run doctor and return JSON output.
pub fn run_doctor_json(brehon_root: &Path) -> Result<String, serde_json::Error> {
    let report = run_doctor(brehon_root);
    report.to_json()
}

/// Run doctor and return compact output.
pub fn run_doctor_compact(brehon_root: &Path) -> String {
    let report = run_doctor(brehon_root);
    report.to_compact()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_run_doctor_empty() {
        let tmp = TempDir::new().unwrap();
        let brehon_root = tmp.path().join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("sessions")).unwrap();
        std::fs::create_dir_all(brehon_root.join("runtime").join("events")).unwrap();

        let report = run_doctor(&brehon_root);
        // Should complete without error even if no issues found
        assert_eq!(report.summary.total_count, report.findings.len());
    }
}
