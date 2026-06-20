//! Budget enforcement diagnostic checker.
//!
//! Surfaces the "the budget kill-switch is effectively off" condition so an
//! operator who *thinks* they configured spend protection is told, loudly,
//! when they have not. This is the doctor-facing half of Wave-1 deliverable 6:
//! caps default to null/unlimited (so the owner's multi-day runs are never
//! surprise-killed), but the operator should never be silently unprotected.

use super::Checker;
use crate::types::{DiagnosticCategory, DiagnosticFinding, Severity};
use brehon_types::BudgetEnforcement;
use std::path::{Path, PathBuf};

/// Checker for budget enforcement policy.
pub struct BudgetChecker {
    /// The `.brehon` directory; the project config lives one level up.
    brehon_root: PathBuf,
}

impl BudgetChecker {
    pub fn new(brehon_root: &Path) -> Self {
        Self {
            brehon_root: brehon_root.to_path_buf(),
        }
    }

    /// Resolve the project root (parent of `.brehon`) for config loading.
    fn project_root(&self) -> PathBuf {
        if self.brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
            if let Some(parent) = self.brehon_root.parent() {
                return parent.to_path_buf();
            }
        }
        self.brehon_root.clone()
    }
}

impl Checker for BudgetChecker {
    fn category(&self) -> DiagnosticCategory {
        DiagnosticCategory::Runtime
    }

    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        // A missing/unreadable config is not this checker's concern; other
        // startup paths surface that. Treat it as "nothing to report here".
        let Ok(config) = brehon_config::load_config(Some(&self.project_root())) else {
            return Ok(Vec::new());
        };
        let budget = &config.budget;

        let enforcement_off = match budget.enforcement {
            // Soft never stops spend, regardless of caps.
            BudgetEnforcement::Soft => true,
            // Hard with no *enforceable* ceiling is a no-op kill-switch. This
            // shared predicate excludes max_cost_per_task (a per-task cap the
            // run-total kill-switch does not yet enforce), so a Hard config whose
            // only cap is max_cost_per_task is correctly flagged rather than
            // silently treated as armed.
            BudgetEnforcement::Hard => !budget.has_enforceable_ceiling(),
        };

        if !enforcement_off {
            return Ok(Vec::new());
        }

        let summary = match budget.enforcement {
            BudgetEnforcement::Soft => {
                "Budget enforcement is Soft: spend is never stopped, only warned"
            }
            BudgetEnforcement::Hard => {
                "Budget enforcement is Hard but no cap is set: the kill-switch will never fire"
            }
        };

        Ok(vec![DiagnosticFinding::new(
            DiagnosticCategory::Runtime,
            Severity::Warning,
            summary,
        )
        .with_subject("budget")
        .with_description(
            "Omission means unlimited: max_total_cost, max_cost_per_task, \
             max_tokens_per_agent and max_wall_clock_minutes all default to null \
             (no ceiling). The run can spend without bound until manually stopped.",
        )
        .with_suggestion(
            "Set enforcement: Hard and at least one of max_tokens_per_agent or \
             max_wall_clock_minutes in the budget block of .brehon/config.yaml to \
             arm the kill-switch.",
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(project_root: &Path, body: &str) {
        let brehon_dir = project_root.join(".brehon");
        std::fs::create_dir_all(&brehon_dir).unwrap();
        std::fs::write(brehon_dir.join("config.yaml"), body).unwrap();
    }

    #[test]
    fn soft_enforcement_is_flagged() {
        let project = tempfile::tempdir().unwrap();
        write_config(
            project.path(),
            "budget:\n  max_tokens_per_agent: 1000\n  enforcement: Soft\n",
        );
        let checker = BudgetChecker::new(&project.path().join(".brehon"));
        let findings = checker.check().unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].summary.contains("Soft"));
    }

    #[test]
    fn hard_with_no_cap_is_flagged() {
        let project = tempfile::tempdir().unwrap();
        write_config(project.path(), "budget:\n  enforcement: Hard\n");
        let checker = BudgetChecker::new(&project.path().join(".brehon"));
        let findings = checker.check().unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].summary.contains("never fire"));
    }

    #[test]
    fn hard_with_cap_is_not_flagged() {
        let project = tempfile::tempdir().unwrap();
        write_config(
            project.path(),
            "budget:\n  max_tokens_per_agent: 1000\n  enforcement: Hard\n",
        );
        let checker = BudgetChecker::new(&project.path().join(".brehon"));
        assert!(checker.check().unwrap().is_empty());
    }

    #[test]
    fn hard_with_wall_clock_only_is_not_flagged() {
        let project = tempfile::tempdir().unwrap();
        write_config(
            project.path(),
            "budget:\n  max_wall_clock_minutes: 120\n  enforcement: Hard\n",
        );
        let checker = BudgetChecker::new(&project.path().join(".brehon"));
        assert!(checker.check().unwrap().is_empty());
    }

    #[test]
    fn hard_with_only_cost_per_task_is_flagged() {
        // max_cost_per_task is a per-task cap the run-total kill-switch does not
        // enforce, so a Hard config with only that cap must be flagged rather
        // than treated as armed.
        let project = tempfile::tempdir().unwrap();
        write_config(
            project.path(),
            "budget:\n  max_cost_per_task: 2.0\n  enforcement: Hard\n",
        );
        let checker = BudgetChecker::new(&project.path().join(".brehon"));
        let findings = checker.check().unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].summary.contains("never fire"));
    }
}
