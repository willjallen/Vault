use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::version::app_version;

pub const REPORT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityResult {
    Pass,
    Warnings,
    Fail,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pass,
    Findings,
    Incomplete,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FindingCounts {
    pub errors: u64,
    pub warnings: u64,
    pub info: u64,
    pub total: u64,
}

impl FindingCounts {
    fn add(&mut self, severity: Severity) {
        self.total = self.total.saturating_add(1);
        match severity {
            Severity::Error => self.errors = self.errors.saturating_add(1),
            Severity::Warning => self.warnings = self.warnings.saturating_add(1),
            Severity::Info => self.info = self.info.saturating_add(1),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScanCounters {
    pub rows: u64,
    pub files: u64,
    pub objects: u64,
    pub bytes_hashed: u64,
}

impl ScanCounters {
    fn merge(&mut self, other: &Self) {
        self.rows = self.rows.saturating_add(other.rows);
        self.files = self.files.saturating_add(other.files);
        self.objects = self.objects.saturating_add(other.objects);
        self.bytes_hashed = self.bytes_hashed.saturating_add(other.bytes_hashed);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub code: String,
    pub severity: Severity,
    pub check: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    pub name: String,
    pub state: CheckState,
    pub findings: FindingCounts,
    pub counters: ScanCounters,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrityReport {
    pub report_version: u32,
    pub app_version: String,
    pub started_at: String,
    pub duration_ms: u128,
    pub backend: String,
    pub scope: String,
    pub result: IntegrityResult,
    pub complete: bool,
    pub findings_summary: FindingCounts,
    pub finding_totals_by_code: BTreeMap<String, u64>,
    pub counters: ScanCounters,
    pub checks: Vec<CheckReport>,
    pub findings: Vec<Finding>,
    pub omitted_by_code: BTreeMap<String, u64>,
}

impl IntegrityReport {
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self.result {
            IntegrityResult::Pass => 0,
            IntegrityResult::Warnings | IntegrityResult::Fail => 1,
            IntegrityResult::Incomplete => 2,
        }
    }

    pub fn render_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    #[must_use]
    pub fn render_human(&self) -> String {
        use std::fmt::Write as _;

        let mut output = String::new();
        let _ = writeln!(
            output,
            "Vault integrity check: {}",
            match self.result {
                IntegrityResult::Pass => "PASS",
                IntegrityResult::Warnings => "WARNINGS",
                IntegrityResult::Fail => "FAIL",
                IntegrityResult::Incomplete => "INCOMPLETE",
            }
        );
        let _ = writeln!(
            output,
            "backend={} duration_ms={} rows={} files={} objects={} bytes_hashed={}",
            self.backend,
            self.duration_ms,
            self.counters.rows,
            self.counters.files,
            self.counters.objects,
            self.counters.bytes_hashed
        );
        let _ = writeln!(
            output,
            "findings: {} error(s), {} warning(s), {} info",
            self.findings_summary.errors,
            self.findings_summary.warnings,
            self.findings_summary.info
        );
        for check in &self.checks {
            let _ = writeln!(
                output,
                "  [{:?}] {} ({} finding(s))",
                check.state, check.name, check.findings.total
            );
        }
        if !self.findings.is_empty() {
            output.push_str("\nFindings:\n");
        }
        for finding in &self.findings {
            let entity = finding
                .entity
                .as_deref()
                .map_or(String::new(), |value| format!(" [{value}]"));
            let _ = writeln!(
                output,
                "- {:?} {}{}: {}",
                finding.severity, finding.code, entity, finding.evidence
            );
            if let Some(remediation) = &finding.remediation {
                let _ = writeln!(output, "  remediation: {remediation}");
            }
        }
        if !self.omitted_by_code.is_empty() {
            output.push_str("\nAdditional findings omitted from detail:\n");
            for (code, count) in &self.omitted_by_code {
                let _ = writeln!(output, "- {code}: {count}");
            }
        }
        output
    }
}

#[derive(Debug, Clone, Default)]
struct CheckAccumulator {
    findings: FindingCounts,
    counters: ScanCounters,
}

#[derive(Debug, Clone)]
pub struct ReportBuilder {
    started: Instant,
    started_at: String,
    backend: String,
    max_findings_per_code: usize,
    checks: BTreeMap<String, CheckAccumulator>,
    incomplete_checks: BTreeSet<String>,
    finding_counts: FindingCounts,
    finding_totals_by_code: BTreeMap<String, u64>,
    retained_per_code: BTreeMap<String, usize>,
    omitted_by_code: BTreeMap<String, u64>,
    findings: Vec<Finding>,
    scope: String,
}

impl ReportBuilder {
    #[must_use]
    pub fn new(backend: impl Into<String>, max_findings_per_code: usize) -> Self {
        let started_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "unknown".to_string());
        Self {
            started: Instant::now(),
            started_at,
            backend: backend.into(),
            max_findings_per_code: max_findings_per_code.max(1),
            checks: BTreeMap::new(),
            incomplete_checks: BTreeSet::new(),
            finding_counts: FindingCounts::default(),
            finding_totals_by_code: BTreeMap::new(),
            retained_per_code: BTreeMap::new(),
            omitted_by_code: BTreeMap::new(),
            findings: Vec::new(),
            scope: "full".to_string(),
        }
    }

    pub fn set_scope(&mut self, scope: impl Into<String>) {
        self.scope = scope.into();
    }

    pub fn ensure_check(&mut self, check: impl Into<String>) {
        self.checks.entry(check.into()).or_default();
    }

    pub fn finding(
        &mut self,
        check: impl Into<String>,
        code: impl Into<String>,
        severity: Severity,
        entity: Option<String>,
        evidence: impl Into<String>,
        remediation: Option<String>,
    ) {
        let check = check.into();
        let code = code.into();
        self.finding_counts.add(severity);
        let total_for_code = self.finding_totals_by_code.entry(code.clone()).or_default();
        *total_for_code = total_for_code.saturating_add(1);
        self.checks
            .entry(check.clone())
            .or_default()
            .findings
            .add(severity);
        let candidate = Finding {
            code: code.clone(),
            severity,
            check,
            entity,
            evidence: evidence.into(),
            remediation,
        };
        let retained = self.retained_per_code.entry(code.clone()).or_default();
        if *retained >= self.max_findings_per_code {
            let omitted = self.omitted_by_code.entry(code).or_default();
            *omitted = omitted.saturating_add(1);
            if let Some((index, worst)) = self
                .findings
                .iter()
                .enumerate()
                .filter(|(_, finding)| finding.code == candidate.code)
                .max_by(|(_, left), (_, right)| compare_findings(left, right))
                && compare_findings(&candidate, worst) == Ordering::Less
            {
                self.findings[index] = candidate;
            }
            return;
        }
        *retained += 1;
        self.findings.push(candidate);
    }

    pub fn error(
        &mut self,
        check: impl Into<String>,
        code: impl Into<String>,
        entity: Option<String>,
        evidence: impl Into<String>,
    ) {
        self.finding(check, code, Severity::Error, entity, evidence, None);
    }

    pub fn warning(
        &mut self,
        check: impl Into<String>,
        code: impl Into<String>,
        entity: Option<String>,
        evidence: impl Into<String>,
    ) {
        self.finding(check, code, Severity::Warning, entity, evidence, None);
    }

    pub fn info(
        &mut self,
        check: impl Into<String>,
        code: impl Into<String>,
        entity: Option<String>,
        evidence: impl Into<String>,
    ) {
        self.finding(check, code, Severity::Info, entity, evidence, None);
    }

    pub fn mark_incomplete(
        &mut self,
        check: impl Into<String>,
        code: impl Into<String>,
        entity: Option<String>,
        evidence: impl Into<String>,
    ) {
        let check = check.into();
        self.incomplete_checks.insert(check.clone());
        self.error(check, code, entity, evidence);
    }

    /// Marks a dependent check as not fully executed without manufacturing an
    /// additional root-cause finding for it.
    pub fn mark_check_incomplete(&mut self, check: impl Into<String>) {
        let check = check.into();
        self.ensure_check(check.clone());
        self.incomplete_checks.insert(check);
    }

    pub fn record_rows(&mut self, check: impl Into<String>, count: u64) {
        let check = check.into();
        let accumulator = self.checks.entry(check).or_default();
        accumulator.counters.rows = accumulator.counters.rows.saturating_add(count);
    }

    pub fn add_counters(&mut self, check: impl Into<String>, counters: &ScanCounters) {
        self.checks
            .entry(check.into())
            .or_default()
            .counters
            .merge(counters);
    }

    pub fn record_files(&mut self, check: impl Into<String>, count: u64) {
        let check = check.into();
        let accumulator = self.checks.entry(check).or_default();
        accumulator.counters.files = accumulator.counters.files.saturating_add(count);
    }

    pub fn record_objects(&mut self, check: impl Into<String>, count: u64) {
        let check = check.into();
        let accumulator = self.checks.entry(check).or_default();
        accumulator.counters.objects = accumulator.counters.objects.saturating_add(count);
    }

    pub fn record_bytes_hashed(&mut self, check: impl Into<String>, count: u64) {
        let check = check.into();
        let accumulator = self.checks.entry(check).or_default();
        accumulator.counters.bytes_hashed = accumulator.counters.bytes_hashed.saturating_add(count);
    }

    #[must_use]
    pub fn snapshot(&self) -> IntegrityReport {
        self.clone().finish()
    }

    #[must_use]
    pub fn finish(mut self) -> IntegrityReport {
        self.findings.sort_by(compare_findings);
        let complete = self.incomplete_checks.is_empty();
        if !complete && self.scope == "full" {
            self.scope = "partial".to_string();
        }
        let result = if !complete {
            IntegrityResult::Incomplete
        } else if self.finding_counts.errors > 0 {
            IntegrityResult::Fail
        } else if self.finding_counts.warnings > 0 {
            IntegrityResult::Warnings
        } else {
            IntegrityResult::Pass
        };
        let mut counters = ScanCounters::default();
        let checks = self
            .checks
            .into_iter()
            .map(|(name, accumulator)| {
                counters.merge(&accumulator.counters);
                let state = if self.incomplete_checks.contains(&name) {
                    CheckState::Incomplete
                } else if accumulator.findings.total > 0 {
                    CheckState::Findings
                } else {
                    CheckState::Pass
                };
                CheckReport {
                    name,
                    state,
                    findings: accumulator.findings,
                    counters: accumulator.counters,
                }
            })
            .collect();
        IntegrityReport {
            report_version: REPORT_VERSION,
            app_version: app_version().to_string(),
            started_at: self.started_at,
            duration_ms: self.started.elapsed().as_millis(),
            backend: self.backend,
            scope: self.scope,
            result,
            complete,
            findings_summary: self.finding_counts,
            finding_totals_by_code: self.finding_totals_by_code,
            counters,
            checks,
            findings: self.findings,
            omitted_by_code: self.omitted_by_code,
        }
    }
}

fn compare_findings(left: &Finding, right: &Finding) -> Ordering {
    left.code
        .cmp(&right.code)
        .then_with(|| left.entity.cmp(&right.entity))
        .then_with(|| left.evidence.cmp(&right.evidence))
        .then_with(|| left.severity.cmp(&right.severity))
        .then_with(|| left.check.cmp(&right.check))
        .then_with(|| left.remediation.cmp(&right.remediation))
}
