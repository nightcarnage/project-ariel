// SPDX-License-Identifier: GPL-2.0-only
//! Findings, and how bad they are.
//!
//! One vocabulary for the whole read-only family: `doctor` reports on a store,
//! `verify` reports on one snapshot, and both answer with the same kinds of line so
//! an operator (or a script reading `--json`) does not have to learn two shapes.

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Nothing to do.
    Ok,
    /// Works, but something is not as it should be.
    Warn,
    /// Broken, or about to be.
    Fail,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Fail => "FAIL",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Short name of what was checked, e.g. `manifests`.
    pub key: String,
    pub severity: Severity,
    pub detail: String,
}

/// The whole preflight.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub(crate) fn note(&mut self, key: &str, severity: Severity, detail: impl Into<String>) {
        self.findings.push(Finding {
            key: key.to_string(),
            severity,
            detail: detail.into(),
        });
    }

    /// The worst severity seen, which is what the exit gate keys off.
    pub fn worst(&self) -> Severity {
        self.findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or(Severity::Ok)
    }

    /// Findings a `--verify` run should refuse over.
    pub fn failures(&self) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Fail)
            .collect()
    }

    /// The report as printed, one line per finding.
    pub fn lines(&self) -> Vec<String> {
        self.findings
            .iter()
            .map(|f| format!("  {:<12} {:<5} {}", f.key, f.severity.label(), f.detail))
            .collect()
    }
}
