//! Confinement probe (ADR-0010 T16).
//!
//! ADR-0010 §5 records what stays unenforceable on each platform, and the
//! whole point of writing it down was that the operator sees it *before* a
//! run rather than in a postmortem. This is where that happens.
//!
//! None of these are findings in the "something is broken" sense — the
//! backend is reported as `Info`, and the gaps with it — but a run where
//! `ps`, `top` and `sudo` cannot exec, or where an agent can still `chmod`
//! anything it can read, is a run whose operator should have known that going
//! in.

use super::Checker;
use crate::types::{DiagnosticCategory, DiagnosticFinding, Severity};

pub struct ConfinementChecker {
    support: brehon_types::confine::ConfineSupport,
}

impl ConfinementChecker {
    pub fn new() -> Self {
        Self {
            support: brehon_types::confine::probe(),
        }
    }
}

impl Default for ConfinementChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl Checker for ConfinementChecker {
    fn category(&self) -> DiagnosticCategory {
        DiagnosticCategory::Worktree
    }

    fn check(&self) -> Result<Vec<DiagnosticFinding>, anyhow::Error> {
        let mut findings = Vec::new();

        match self.support.backend {
            Some(backend) => findings.push(
                DiagnosticFinding::new(
                    self.category(),
                    Severity::Info,
                    format!("Agent confinement backend: {backend}"),
                )
                .with_description(self.support.detail.clone()),
            ),
            // Not Info. Without a backend every agent runs unconfined, and
            // ADR-0010's isolation argument does not hold at all.
            None => findings.push(
                DiagnosticFinding::new(
                    self.category(),
                    Severity::Error,
                    "No agent confinement backend on this host",
                )
                .with_description(self.support.detail.clone())
                .with_suggestion(
                    "Agents will be refused at spawn unless the profile sets unsafe_marker",
                ),
            ),
        }

        for gap in &self.support.unenforceable {
            findings.push(
                DiagnosticFinding::new(
                    self.category(),
                    Severity::Warning,
                    "Confinement gap on this platform",
                )
                .with_description((*gap).to_string()),
            );
        }

        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_backend_and_every_gap() {
        let checker = ConfinementChecker::new();
        let findings = checker.check().unwrap();
        let support = brehon_types::confine::probe();

        assert_eq!(
            findings.len(),
            support.unenforceable.len() + 1,
            "one finding for the backend, one per gap"
        );
        assert!(
            findings
                .iter()
                .filter(|f| f.severity == Severity::Warning)
                .count()
                == support.unenforceable.len(),
            "every gap must be visible before a run, not after an incident"
        );
    }

    /// A host with no backend is not an informational state: every agent runs
    /// unconfined and the isolation argument does not hold.
    #[test]
    fn a_missing_backend_is_an_error() {
        let checker = ConfinementChecker {
            support: brehon_types::confine::ConfineSupport {
                backend: None,
                detail: "unsupported".to_string(),
                unenforceable: vec!["any path confinement"],
            },
        };
        let findings = checker.check().unwrap();
        assert!(findings.iter().any(|f| f.severity == Severity::Error));
    }
}
