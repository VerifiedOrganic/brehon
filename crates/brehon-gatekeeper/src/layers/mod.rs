//! Gatekeeper pipeline layers.

pub mod go_build;
pub mod plan;

use serde::{Deserialize, Serialize};

use crate::findings::{Finding, Severity};

/// Status of a single gatekeeper layer after execution.
///
/// `passed` is automatically derived from `findings`: a layer passes
/// when it has no blocking findings.  It is recomputed on
/// deserialization and whenever a finding is added via
/// [`add_finding`](Self::add_finding), so the flag can never go stale.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LayerStatus {
    pub layer_name: String,
    passed: bool,
    findings: Vec<Finding>,
}

impl LayerStatus {
    pub fn new(layer_name: impl Into<String>, findings: Vec<Finding>) -> Self {
        let passed = !findings.iter().any(|f| f.severity == Severity::Blocking);
        Self {
            layer_name: layer_name.into(),
            passed,
            findings,
        }
    }

    pub fn passed(&self) -> bool {
        self.passed
    }

    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    pub fn add_finding(&mut self, finding: Finding) {
        if finding.severity == Severity::Blocking {
            self.passed = false;
        }
        self.findings.push(finding);
    }

    pub fn has_blocking(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == Severity::Blocking)
    }
}

impl<'de> Deserialize<'de> for LayerStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename = "LayerStatus")]
        struct Raw {
            layer_name: String,
            findings: Vec<Finding>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(LayerStatus::new(raw.layer_name, raw.findings))
    }
}
