use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

pub const DIAGNOSTICS_BASELINE_SCHEMA_VERSION: u32 = 1;

pub fn normalize_diagnostic_snippet(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The snippet of LSP line `line` of `line_index`, cut from `lines`, the [`str::lines`] of
/// the same text.
///
/// A snippet line is broken by `\n` alone, so a line below a lone `\r` takes the whole
/// `\n` line it sits in: that keeps the fingerprints recorded when the line index did not
/// count a lone `\r` either, and no baseline entry stops matching its finding.
pub fn diagnostic_line_snippet(
    lines: &[&str],
    line_index: &line_index::LineIndex,
    line: usize,
) -> String {
    let lf_line = line_index.lf_line(line as u32) as usize;
    normalize_diagnostic_snippet(lines.get(lf_line).copied().unwrap_or_default())
}

pub fn diagnostic_fingerprint(path: &str, code: &str, snippet: &str, occurrence: u32) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in [path.as_bytes(), code.as_bytes(), snippet.as_bytes()] {
        hasher.update(part);
        hasher.update(&[0]);
    }
    hasher.update(&occurrence.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

#[derive(Debug, thiserror::Error)]
#[error("diagnostic source snippet is unavailable")]
pub struct MissingDiagnosticSnippet;

pub fn strict_diagnostic_fingerprint(
    path: &str,
    code: &str,
    snippet: Option<&str>,
    occurrence: u32,
) -> Result<String, MissingDiagnosticSnippet> {
    snippet
        .map(|snippet| diagnostic_fingerprint(path, code, snippet, occurrence))
        .ok_or(MissingDiagnosticSnippet)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaseline {
    pub schema_version: u32,
    pub scope: DiagnosticsBaselineScope,
    pub diagnostics: Vec<DiagnosticsBaselineEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineScope {
    /// `None` for an extension-only project, which has no base configuration.
    /// A project that has one serializes exactly as before.
    pub source_root: Option<String>,
    pub extensions: Vec<DiagnosticsBaselineExtension>,
}

pub type DiagnosticsBaselineExtension = project_model::DiagnosticsBaselineProjectExtension;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineEntry {
    pub fingerprint: String,
    pub path: String,
    pub code: String,
    pub snippet: String,
    pub occurrence: u32,
    pub message: String,
    pub severity: String,
    pub range: DiagnosticsBaselineRange,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticsBaselineCoverage {
    Full,
    Partial { completed_files: BTreeSet<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedDiagnostics<T> {
    pub new: Vec<ClassifiedDiagnostic<T>>,
    pub known: Vec<ClassifiedDiagnostic<T>>,
    pub resolved: Vec<DiagnosticsBaselineEntry>,
    pub summary: DiagnosticsBaselineSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedDiagnostic<T> {
    pub diagnostic: T,
    pub entry: DiagnosticsBaselineEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineDiagnosticCandidate<T> {
    pub diagnostic: T,
    pub path: String,
    pub code: String,
    pub snippet: Option<String>,
    pub message: String,
    pub severity: String,
    pub range: DiagnosticsBaselineRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsBaselineState {
    Disabled,
    Full,
    Partial,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineSummary {
    pub state: DiagnosticsBaselineState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<project_model::DiagnosticsBaselineSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partitions_enabled: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partitions_unsuppressed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsuppressed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_schema_version: Option<u32>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions: Vec<DiagnosticsBaselinePartitionSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<DiagnosticsBaselineErrorSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineErrorSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_id: Option<String>,
    pub code: String,
    pub detail: String,
    pub epoch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselinePartitionSummary {
    pub id: String,
    pub identity: project_model::DiagnosticsBaselinePartitionIdentity,
    pub policy: project_model::DiagnosticsBaselinePartitionPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    pub state: DiagnosticsBaselineState,
    pub new: usize,
    pub known: usize,
    pub resolved: usize,
    pub unsuppressed: usize,
    pub complete: bool,
}

impl DiagnosticsBaselineSummary {
    /// A baseline that is configured but whose classification never ran to a
    /// verdict — the sweep was cancelled first. Absent counts say that, where a
    /// zero would claim "nothing new was found" and [`Self::disabled`] would claim
    /// the project has no baseline at all.
    pub fn interrupted(path: Option<String>) -> Self {
        Self { state: DiagnosticsBaselineState::Partial, path, complete: false, ..Self::disabled() }
    }

    pub fn disabled() -> Self {
        Self {
            state: DiagnosticsBaselineState::Disabled,
            selection: None,
            partitions_enabled: None,
            partitions_unsuppressed: None,
            unsuppressed: None,
            new: None,
            known: None,
            resolved: None,
            path: None,
            schema_version: None,
            manifest_schema_version: None,
            complete: true,
            error_code: None,
            detail: None,
            partitions: vec![],
            errors: vec![],
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiagnosticsBaselineError {
    #[error("invalid diagnostics baseline JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported diagnostics baseline schema version {found}; expected {expected}")]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error("diagnostics baseline scope does not match the current project")]
    ScopeMismatch,
    #[error("diagnostics baseline has a contradictory legacy scope marker")]
    InvalidLegacyScopeMarker,
    #[error("diagnostics baseline contains invalid relative path: {0}")]
    InvalidPath(String),
    #[error("diagnostics baseline fingerprint does not match its fields: {0}")]
    FingerprintMismatch(String),
    #[error("diagnostics baseline contains duplicate entry: {0}")]
    Duplicate(String),
    #[error("diagnostics baseline cannot contain protected diagnostic: {0}")]
    ProtectedDiagnostic(String),
}

pub fn parse_diagnostics_baseline(
    bytes: &[u8],
    expected_scope: &DiagnosticsBaselineScope,
) -> Result<DiagnosticsBaseline, DiagnosticsBaselineError> {
    let baseline: DiagnosticsBaseline = serde_json::from_slice(bytes)?;
    validate_diagnostics_baseline(&baseline, expected_scope)?;
    Ok(baseline)
}

pub fn diagnostics_baseline_json(
    baseline: &DiagnosticsBaseline,
) -> Result<Vec<u8>, DiagnosticsBaselineError> {
    validate_diagnostics_baseline(baseline, &baseline.scope)?;
    let mut baseline = baseline.clone();
    baseline.diagnostics.sort_by(|a, b| {
        (&a.path, &a.code, &a.snippet, a.occurrence).cmp(&(
            &b.path,
            &b.code,
            &b.snippet,
            b.occurrence,
        ))
    });
    let mut bytes = serde_json::to_vec_pretty(&baseline)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Scope equality that tolerates the legacy spelling of "this project has no
/// nested configuration root".
///
/// Before the base became optional it was stored as an empty string, which is
/// indistinguishable from a base sitting at the project root — both spell `""`.
/// Published baselines still carry it, so an absent base and an empty one are the
/// same scope. Only the comparison is tolerant: the stored spelling is what gets
/// fingerprinted, so an existing baseline keeps its identity.
pub fn scopes_match(left: &DiagnosticsBaselineScope, right: &DiagnosticsBaselineScope) -> bool {
    fn base(scope: &DiagnosticsBaselineScope) -> &str {
        scope.source_root.as_deref().unwrap_or_default()
    }
    base(left) == base(right) && left.extensions == right.extensions
}

pub fn legacy_scope_markers_are_valid(scope: &DiagnosticsBaselineScope) -> bool {
    let valid = |scope: Option<bsl_metadata::ExtensionScope>, marker: Option<bool>| {
        marker != Some(false) && !(scope.is_some() && marker == Some(true))
    };
    scope.extensions.iter().all(|extension| {
        valid(extension.scope, extension.legacy_scope)
            && extension
                .dependency_scopes
                .iter()
                .all(|dependency| valid(dependency.scope, dependency.legacy_scope))
    })
}

fn validate_diagnostics_baseline(
    baseline: &DiagnosticsBaseline,
    expected_scope: &DiagnosticsBaselineScope,
) -> Result<(), DiagnosticsBaselineError> {
    if baseline.schema_version != DIAGNOSTICS_BASELINE_SCHEMA_VERSION {
        return Err(DiagnosticsBaselineError::UnsupportedSchema {
            found: baseline.schema_version,
            expected: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
        });
    }
    if !legacy_scope_markers_are_valid(&baseline.scope)
        || !legacy_scope_markers_are_valid(expected_scope)
    {
        return Err(DiagnosticsBaselineError::InvalidLegacyScopeMarker);
    }
    if !scopes_match(&baseline.scope, expected_scope) {
        return Err(DiagnosticsBaselineError::ScopeMismatch);
    }
    if let Some(source_root) = baseline.scope.source_root.as_deref() {
        validate_relative_path(source_root, true)?;
    }
    for extension in &baseline.scope.extensions {
        validate_relative_path(&extension.path, false)?;
    }

    let mut fingerprints = HashSet::new();
    let mut identities = HashSet::new();
    for entry in &baseline.diagnostics {
        validate_relative_path(&entry.path, false)?;
        if is_protected_diagnostic(&entry.code) {
            return Err(DiagnosticsBaselineError::ProtectedDiagnostic(entry.code.clone()));
        }
        let expected =
            diagnostic_fingerprint(&entry.path, &entry.code, &entry.snippet, entry.occurrence);
        if entry.fingerprint != expected {
            return Err(DiagnosticsBaselineError::FingerprintMismatch(entry.fingerprint.clone()));
        }
        if !fingerprints.insert(&entry.fingerprint) {
            return Err(DiagnosticsBaselineError::Duplicate(entry.fingerprint.clone()));
        }
        let identity = (&entry.path, &entry.code, &entry.snippet, entry.occurrence);
        if !identities.insert(identity) {
            return Err(DiagnosticsBaselineError::Duplicate(entry.fingerprint.clone()));
        }
    }
    Ok(())
}

fn validate_relative_path(path: &str, allow_empty: bool) -> Result<(), DiagnosticsBaselineError> {
    let valid = (allow_empty && path.is_empty())
        || (!path.is_empty()
            && !path.starts_with('/')
            && !path.contains('\\')
            && path
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != ".."));
    if valid {
        Ok(())
    } else {
        Err(DiagnosticsBaselineError::InvalidPath(path.to_owned()))
    }
}

/// Whether a classification has to produce the `resolved` set.
///
/// Computing it walks every entry of the baseline (of every enabled partition, in
/// the partitioned form) and asks the coverage whether that entry's file was
/// analysed. That is the right shape for a whole-project run, which does it once,
/// and pure cost for a caller classifying a single file to decide which of its
/// diagnostics stay active — there the walk is proportional to accumulated debt
/// and its result is discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedPolicy {
    Compute,
    Skip,
}

/// [`classify_diagnostics_with`] that also computes `resolved`.
pub fn classify_diagnostics<T>(
    baseline: &DiagnosticsBaseline,
    baseline_path: String,
    current: Vec<BaselineDiagnosticCandidate<T>>,
    coverage: &DiagnosticsBaselineCoverage,
) -> Result<ClassifiedDiagnostics<T>, MissingDiagnosticSnippet> {
    classify_diagnostics_with(baseline, baseline_path, current, coverage, ResolvedPolicy::Compute)
}

/// Classifies `current` against `baseline`, fixing each finding's identity in its
/// [`ClassifiedDiagnostic::entry`].
///
/// The `occurrence` in a fingerprint numbers a finding among the identical ones of its
/// file, so `current` must hold every finalized finding of each file it mentions — a
/// scope's line filter, an author filter, a requested range or an output cap applied
/// before this call would renumber the survivors and let a new finding take a recorded
/// one's fingerprint. Callers shape the result afterwards: they may drop entries of
/// `new` or `known` for presentation, and the `entry` of every survivor stays what this
/// call made it. Classifying a reduced set again is what they must not do. The order of
/// `current` does not matter; findings are numbered in a sort over path, range, code and
/// message.
pub fn classify_diagnostics_with<T>(
    baseline: &DiagnosticsBaseline,
    baseline_path: String,
    mut current: Vec<BaselineDiagnosticCandidate<T>>,
    coverage: &DiagnosticsBaselineCoverage,
    resolved_policy: ResolvedPolicy,
) -> Result<ClassifiedDiagnostics<T>, MissingDiagnosticSnippet> {
    current.sort_by(|a, b| {
        (&a.path, &a.range, &a.code, &a.message).cmp(&(&b.path, &b.range, &b.code, &b.message))
    });

    let baseline_fingerprints: HashSet<&str> =
        baseline.diagnostics.iter().map(|entry| entry.fingerprint.as_str()).collect();
    let mut matched = HashSet::new();
    let mut occurrences: HashMap<(String, String, String), u32> = HashMap::new();
    let mut new = Vec::new();
    let mut known = Vec::new();

    for candidate in current {
        let snippet = normalize_diagnostic_snippet(
            candidate.snippet.as_deref().ok_or(MissingDiagnosticSnippet)?,
        );
        let occurrence = occurrences
            .entry((candidate.path.clone(), candidate.code.clone(), snippet.clone()))
            .or_default();
        let occurrence_index = *occurrence;
        let fingerprint =
            diagnostic_fingerprint(&candidate.path, &candidate.code, &snippet, occurrence_index);
        *occurrence += 1;

        let classified = ClassifiedDiagnostic {
            diagnostic: candidate.diagnostic,
            entry: DiagnosticsBaselineEntry {
                fingerprint: fingerprint.clone(),
                path: candidate.path,
                code: candidate.code,
                snippet,
                occurrence: occurrence_index,
                message: candidate.message,
                severity: candidate.severity,
                range: candidate.range,
            },
        };
        if !is_protected_diagnostic(&classified.entry.code)
            && baseline_fingerprints.contains(fingerprint.as_str())
        {
            matched.insert(fingerprint);
            known.push(classified);
        } else {
            new.push(classified);
        }
    }

    let resolved: Vec<_> = match resolved_policy {
        ResolvedPolicy::Skip => Vec::new(),
        ResolvedPolicy::Compute => baseline
            .diagnostics
            .iter()
            .filter(|entry| {
                !matched.contains(&entry.fingerprint)
                    && match coverage {
                        DiagnosticsBaselineCoverage::Full => true,
                        DiagnosticsBaselineCoverage::Partial { completed_files } => {
                            completed_files.contains(&entry.path)
                        }
                    }
            })
            .cloned()
            .collect(),
    };
    let complete = matches!(coverage, DiagnosticsBaselineCoverage::Full);
    let summary = DiagnosticsBaselineSummary {
        state: if complete {
            DiagnosticsBaselineState::Full
        } else {
            DiagnosticsBaselineState::Partial
        },
        selection: None,
        partitions_enabled: None,
        partitions_unsuppressed: None,
        unsuppressed: None,
        new: Some(new.len()),
        known: Some(known.len()),
        resolved: match resolved_policy {
            ResolvedPolicy::Compute => Some(resolved.len()),
            ResolvedPolicy::Skip => None,
        },
        path: Some(baseline_path),
        schema_version: Some(baseline.schema_version),
        manifest_schema_version: None,
        complete,
        error_code: None,
        detail: None,
        partitions: vec![],
        errors: vec![],
    };

    Ok(ClassifiedDiagnostics { new, known, resolved, summary })
}

/// A diagnostic that a baseline must never suppress, and therefore must never
/// record: it reports a broken suppression directive, so hiding it would hide
/// the very fact that a suppression does not work.
pub fn is_protected_diagnostic(code: &str) -> bool {
    matches!(code, "UnknownSuppressionCode" | "SuppressionWithoutCode")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASELINE_JSON: &str = r#"{
        "schema_version": 1,
        "scope": {
            "source_root": "src",
            "extensions": [{
                "name": "Extension",
                "path": "extensions/Extension",
                "depends_on": []
            }]
        },
        "diagnostics": [{
            "fingerprint": "abc",
            "path": "CommonModules/Module/Ext/Module.bsl",
            "code": "LineLength",
            "snippet": "Message(\"long line\");",
            "occurrence": 0,
            "message": "Line is too long",
            "severity": "Warning",
            "range": {
                "start_line": 1,
                "start_column": 0,
                "end_line": 1,
                "end_column": 21
            }
        }]
    }"#;

    #[test]
    fn diagnostics_baseline_schema_round_trips() {
        let baseline: DiagnosticsBaseline = serde_json::from_str(BASELINE_JSON).unwrap();
        assert_eq!(baseline.schema_version, DIAGNOSTICS_BASELINE_SCHEMA_VERSION);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&baseline).unwrap())
                .unwrap(),
            serde_json::from_str::<serde_json::Value>(BASELINE_JSON).unwrap()
        );
    }

    #[test]
    fn diagnostics_baseline_schema_rejects_unknown_fields() {
        let json = BASELINE_JSON
            .replace("\"schema_version\": 1,", "\"schema_version\": 1, \"extra\": true,");
        assert!(serde_json::from_str::<DiagnosticsBaseline>(&json).is_err());
    }

    #[test]
    fn diagnostics_baseline_scope_distinguishes_full_and_partial() {
        assert_eq!(DiagnosticsBaselineCoverage::Full, DiagnosticsBaselineCoverage::Full);
        let completed_files = BTreeSet::from(["Module.bsl".to_owned()]);
        assert_eq!(
            DiagnosticsBaselineCoverage::Partial { completed_files: completed_files.clone() },
            DiagnosticsBaselineCoverage::Partial { completed_files }
        );
    }

    #[test]
    fn diagnostics_baseline_scope_serializes_summary_states() {
        for state in [
            DiagnosticsBaselineState::Disabled,
            DiagnosticsBaselineState::Full,
            DiagnosticsBaselineState::Partial,
            DiagnosticsBaselineState::Error,
        ] {
            let summary = DiagnosticsBaselineSummary {
                state,
                selection: None,
                partitions_enabled: None,
                partitions_unsuppressed: None,
                unsuppressed: None,
                new: None,
                known: None,
                resolved: None,
                path: None,
                schema_version: None,
                manifest_schema_version: None,
                complete: state == DiagnosticsBaselineState::Full,
                error_code: None,
                detail: None,
                partitions: vec![],
                errors: vec![],
            };
            let value = serde_json::to_value(summary).unwrap();
            assert_eq!(value["state"], state_string(state));
            assert!(value.get("new").is_none());
        }
    }

    /// A lone `\r` breaks the LSP line but not the snippet line, so the fingerprint of
    /// a finding below it is the one recorded when the line index ignored a lone `\r`.
    #[test]
    fn snippet_below_a_lone_cr_is_its_whole_lf_line() {
        let text = "Процедура А()\r\tБ = Б;\nКонецПроцедуры\n";
        let lines: Vec<_> = text.lines().collect();
        let index = line_index::LineIndex::new(text);

        assert_eq!(diagnostic_line_snippet(&lines, &index, 1), "Процедура А() Б = Б;");
        assert_eq!(diagnostic_line_snippet(&lines, &index, 2), "КонецПроцедуры");
    }

    #[test]
    fn diagnostics_fingerprint_normalizes_and_preserves_recipe() {
        let snippet = normalize_diagnostic_snippet("  Message(  A ); ");
        assert_eq!(snippet, "Message( A );");
        assert_eq!(
            diagnostic_fingerprint("a.bsl", "Rule", &snippet, 0),
            "55a27325281b7f3d47e95d1123a1c09590908e5b3378b73168372a0e665264cd"
        );
    }

    #[test]
    fn diagnostics_fingerprint_requires_snippet() {
        assert!(strict_diagnostic_fingerprint("a.bsl", "Rule", None, 0).is_err());
        assert_eq!(
            strict_diagnostic_fingerprint("a.bsl", "Rule", Some(""), 0).unwrap(),
            diagnostic_fingerprint("a.bsl", "Rule", "", 0)
        );
    }

    #[test]
    fn diagnostics_baseline_classify_survives_line_shift() {
        let baseline = baseline(vec![entry("A();", 0, 1)]);
        let result = classify_diagnostics(
            &baseline,
            "baseline.json".to_owned(),
            vec![candidate("A();", 9, "current")],
            &DiagnosticsBaselineCoverage::Full,
        )
        .unwrap();

        assert!(result.new.is_empty());
        assert_eq!(result.known.len(), 1);
        assert!(result.resolved.is_empty());
        assert_eq!(result.known[0].entry.range.start_line, 9);
    }

    #[test]
    fn diagnostics_baseline_classify_changed_expression_is_new_and_resolved() {
        let baseline = baseline(vec![entry("A();", 0, 1)]);
        let result = classify_diagnostics(
            &baseline,
            "baseline.json".to_owned(),
            vec![candidate("B();", 1, "current")],
            &DiagnosticsBaselineCoverage::Full,
        )
        .unwrap();

        assert_eq!(result.new.len(), 1);
        assert!(result.known.is_empty());
        assert_eq!(result.resolved.len(), 1);
    }

    #[test]
    fn diagnostics_baseline_classify_numbers_identical_lines() {
        let baseline = baseline(vec![entry("A();", 0, 1), entry("A();", 1, 2)]);
        let result = classify_diagnostics(
            &baseline,
            "baseline.json".to_owned(),
            vec![candidate("A();", 4, "first"), candidate("A();", 8, "second")],
            &DiagnosticsBaselineCoverage::Full,
        )
        .unwrap();

        assert_eq!(result.known.len(), 2);
        assert_eq!(result.known[0].entry.occurrence, 0);
        assert_eq!(result.known[1].entry.occurrence, 1);
    }

    #[test]
    fn diagnostics_baseline_io_is_byte_deterministic() {
        let baseline = baseline(vec![entry("B();", 0, 2), entry("A();", 0, 1)]);
        let first = diagnostics_baseline_json(&baseline).unwrap();
        let second = diagnostics_baseline_json(&baseline).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.last(), Some(&b'\n'));

        let parsed = parse_diagnostics_baseline(&first, &baseline.scope).unwrap();
        assert_eq!(parsed.diagnostics[0].snippet, "A();");
        assert_eq!(parsed.diagnostics[1].snippet, "B();");
    }

    #[test]
    fn diagnostics_baseline_io_recalculates_fingerprint() {
        let baseline = baseline(vec![entry("A();", 0, 1)]);
        let mut value = serde_json::to_value(&baseline).unwrap();
        value["diagnostics"][0]["fingerprint"] = "forged".into();
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            parse_diagnostics_baseline(&bytes, &baseline.scope),
            Err(DiagnosticsBaselineError::FingerprintMismatch(_))
        ));
    }

    #[test]
    fn diagnostics_baseline_io_rejects_bad_schema_and_json() {
        let mut baseline = baseline(vec![]);
        baseline.schema_version = 99;
        let bytes = serde_json::to_vec(&baseline).unwrap();
        assert!(matches!(
            parse_diagnostics_baseline(&bytes, &baseline.scope),
            Err(DiagnosticsBaselineError::UnsupportedSchema { .. })
        ));
        assert!(matches!(
            parse_diagnostics_baseline(b"{", &baseline.scope),
            Err(DiagnosticsBaselineError::Json(_))
        ));
    }

    #[test]
    fn diagnostics_baseline_io_rejects_duplicates() {
        let baseline = baseline(vec![entry("A();", 0, 1), entry("A();", 0, 2)]);
        let bytes = serde_json::to_vec(&baseline).unwrap();
        assert!(matches!(
            parse_diagnostics_baseline(&bytes, &baseline.scope),
            Err(DiagnosticsBaselineError::Duplicate(_))
        ));
    }

    /// A baseline published before the base became optional spells "no nested
    /// configuration root" as an empty string. The project now reports no base at
    /// all for such a layout, and the two must still be the same scope — otherwise
    /// every existing baseline of a project without a nested root is rejected as a
    /// scope mismatch on the first run after the upgrade.
    #[test]
    fn a_legacy_empty_source_root_still_matches_a_project_without_a_base() {
        let stored =
            DiagnosticsBaselineScope { source_root: Some(String::new()), extensions: vec![] };
        let computed = DiagnosticsBaselineScope { source_root: None, extensions: vec![] };
        assert!(scopes_match(&stored, &computed), "legacy empty base matches an absent one");
        assert!(scopes_match(&computed, &stored), "and the comparison is symmetric");

        // The control: a real difference is still a mismatch, so the tolerance is
        // not "everything matches".
        let nested =
            DiagnosticsBaselineScope { source_root: Some("src/cf".to_owned()), extensions: vec![] };
        assert!(!scopes_match(&nested, &computed), "a nested base is a different scope");
    }

    #[test]
    fn legacy_scope_markers_validate_fresh_v1_baselines_and_reject_old_or_contradictory_identity() {
        for legacy_root in [true, false] {
            let extension = DiagnosticsBaselineExtension {
                name: "Ext".to_owned(),
                path: "src/cfe/Ext".to_owned(),
                scope: if legacy_root {
                    None
                } else {
                    Some(bsl_metadata::ExtensionScope::InfoBase)
                },
                legacy_scope: legacy_root.then_some(true),
                dependency_scopes: if legacy_root {
                    vec![]
                } else {
                    vec![project_model::DiagnosticsBaselineDependencyScope {
                        name: "Base".to_owned(),
                        scope: None,
                        legacy_scope: Some(true),
                    }]
                },
                depends_on: vec![],
            };
            let expected =
                DiagnosticsBaselineScope { source_root: None, extensions: vec![extension.clone()] };
            let fresh = DiagnosticsBaseline {
                schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope: expected.clone(),
                diagnostics: vec![],
            };
            let bytes = diagnostics_baseline_json(&fresh).unwrap();
            assert_eq!(parse_diagnostics_baseline(&bytes, &expected).unwrap(), fresh);

            let mut markerless = fresh.clone();
            if legacy_root {
                markerless.scope.extensions[0].legacy_scope = None;
            } else {
                markerless.scope.extensions[0].dependency_scopes[0].legacy_scope = None;
            }
            let old_bytes = serde_json::to_vec(&markerless).unwrap();
            assert!(matches!(
                parse_diagnostics_baseline(&old_bytes, &expected),
                Err(DiagnosticsBaselineError::ScopeMismatch)
            ));

            let mut contradictory = fresh;
            if legacy_root {
                contradictory.scope.extensions[0].scope =
                    Some(bsl_metadata::ExtensionScope::InfoBase);
            } else {
                contradictory.scope.extensions[0].dependency_scopes[0].scope =
                    Some(bsl_metadata::ExtensionScope::DataSeparation);
                contradictory.scope.extensions[0].dependency_scopes[0].legacy_scope = Some(true);
            }
            let invalid_bytes = serde_json::to_vec(&contradictory).unwrap();
            assert!(matches!(
                parse_diagnostics_baseline(&invalid_bytes, &expected),
                Err(DiagnosticsBaselineError::InvalidLegacyScopeMarker)
            ));
            assert!(matches!(
                diagnostics_baseline_json(&contradictory),
                Err(DiagnosticsBaselineError::InvalidLegacyScopeMarker)
            ));
        }
    }

    #[test]
    fn diagnostics_baseline_io_rejects_incompatible_scope_and_paths() {
        let baseline = baseline(vec![]);
        let other_scope =
            DiagnosticsBaselineScope { source_root: Some("other".to_owned()), extensions: vec![] };
        let bytes = serde_json::to_vec(&baseline).unwrap();
        assert!(matches!(
            parse_diagnostics_baseline(&bytes, &other_scope),
            Err(DiagnosticsBaselineError::ScopeMismatch)
        ));

        let invalid = DiagnosticsBaseline {
            scope: DiagnosticsBaselineScope {
                source_root: Some("/absolute".to_owned()),
                extensions: vec![],
            },
            ..baseline
        };
        assert!(matches!(
            diagnostics_baseline_json(&invalid),
            Err(DiagnosticsBaselineError::InvalidPath(_))
        ));
    }

    #[test]
    fn diagnostics_baseline_protected_diagnostics_remain_active() {
        for code in ["UnknownSuppressionCode", "SuppressionWithoutCode"] {
            let mut protected = entry("A();", 0, 1);
            protected.code = code.to_owned();
            protected.fingerprint = diagnostic_fingerprint("a.bsl", code, "A();", 0);
            let baseline = baseline(vec![protected]);
            assert!(matches!(
                diagnostics_baseline_json(&baseline),
                Err(DiagnosticsBaselineError::ProtectedDiagnostic(found)) if found == code
            ));

            let mut current = candidate("A();", 1, "current");
            current.code = code.to_owned();
            let result = classify_diagnostics(
                &baseline,
                "baseline.json".to_owned(),
                vec![current],
                &DiagnosticsBaselineCoverage::Full,
            )
            .unwrap();
            assert_eq!(result.new.len(), 1);
            assert!(result.known.is_empty());
        }
    }

    fn baseline(diagnostics: Vec<DiagnosticsBaselineEntry>) -> DiagnosticsBaseline {
        DiagnosticsBaseline {
            schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: DiagnosticsBaselineScope {
                source_root: Some("src".to_owned()),
                extensions: vec![],
            },
            diagnostics,
        }
    }

    fn entry(snippet: &str, occurrence: u32, line: u32) -> DiagnosticsBaselineEntry {
        DiagnosticsBaselineEntry {
            fingerprint: diagnostic_fingerprint("a.bsl", "Rule", snippet, occurrence),
            path: "a.bsl".to_owned(),
            code: "Rule".to_owned(),
            snippet: snippet.to_owned(),
            occurrence,
            message: "old".to_owned(),
            severity: "Warning".to_owned(),
            range: range(line),
        }
    }

    fn candidate(
        snippet: &str,
        line: u32,
        diagnostic: &'static str,
    ) -> BaselineDiagnosticCandidate<&'static str> {
        BaselineDiagnosticCandidate {
            diagnostic,
            path: "a.bsl".to_owned(),
            code: "Rule".to_owned(),
            snippet: Some(snippet.to_owned()),
            message: "current".to_owned(),
            severity: "Warning".to_owned(),
            range: range(line),
        }
    }

    fn range(line: u32) -> DiagnosticsBaselineRange {
        DiagnosticsBaselineRange {
            start_line: line,
            start_column: 0,
            end_line: line,
            end_column: 1,
        }
    }

    /// The shared counterexample: three identical findings a, b, c in range order, the
    /// baseline holding occurrences 0 and 1 (a and b), the scope keeping lines of b and c.
    fn counterexample() -> (DiagnosticsBaseline, Vec<BaselineDiagnosticCandidate<&'static str>>) {
        let snippet = "А = А;";
        (
            baseline(vec![entry(snippet, 0, 1), entry(snippet, 1, 2)]),
            vec![
                candidate(snippet, 1, "a"),
                candidate(snippet, 2, "b"),
                candidate(snippet, 3, "c"),
            ],
        )
    }

    /// The identity a finding gets from the classification of the file's whole set is the
    /// one it keeps through every later projection: c is new with occurrence 2 whatever
    /// the order of the input and however the classified result is filtered. Handing the
    /// classifier only the scoped findings instead renumbers c to 1 and hides it as known.
    #[test]
    fn finality_projection_keeps_occurrence() {
        let (base, full) = counterexample();
        let coverage = DiagnosticsBaselineCoverage::Partial { completed_files: BTreeSet::new() };
        let classified =
            classify_diagnostics(&base, "b.json".to_owned(), full.clone(), &coverage).unwrap();
        let known: Vec<_> =
            classified.known.iter().map(|item| (item.diagnostic, item.entry.occurrence)).collect();
        assert_eq!(known, vec![("a", 0), ("b", 1)]);
        assert_eq!(classified.new.len(), 1);
        let c = &classified.new[0];
        assert_eq!((c.diagnostic, c.entry.occurrence), ("c", 2));
        assert_eq!(c.entry.fingerprint, diagnostic_fingerprint("a.bsl", "Rule", "А = А;", 2));
        assert_eq!(c.entry.range, range(3), "the surviving finding is the one on line 3");

        let mut reversed = full.clone();
        reversed.reverse();
        let permuted =
            classify_diagnostics(&base, "b.json".to_owned(), reversed, &coverage).unwrap();
        assert_eq!(permuted.new, classified.new, "the input order must not matter");
        assert_eq!(permuted.known, classified.known);

        let in_scope = |item: &&ClassifiedDiagnostic<&str>| item.entry.range.start_line >= 2;
        let visible: Vec<_> = classified.new.iter().filter(in_scope).collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].entry, c.entry, "a projection must not touch the identity");

        // Negative control: scope before identity renumbers c and hides it.
        let scoped_first: Vec<_> =
            full.iter().filter(|c| c.range.start_line >= 2).cloned().collect();
        let renumbered =
            classify_diagnostics(&base, "b.json".to_owned(), scoped_first, &coverage).unwrap();
        assert!(
            renumbered.new.iter().all(|item| item.diagnostic != "c"),
            "control: classifying the scoped set must lose c: {renumbered:#?}"
        );

        // Positive control: without the baseline both scoped findings are visible.
        let unrecorded =
            classify_diagnostics(&baseline(vec![]), "b.json".to_owned(), full, &coverage).unwrap();
        let shown: Vec<_> =
            unrecorded.new.iter().filter(in_scope).map(|item| item.diagnostic).collect();
        assert_eq!(shown, vec!["b", "c"]);
    }

    /// A superseded loser stays gone when its winner is hidden by the baseline: the pair is
    /// decided on the complete set before the classification, so the classifier never sees
    /// the loser at all.
    #[test]
    fn finality_late_scope_preserves_policy() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use vfs::{FileId, FileSet, VfsPath};

        let text = "Процедура Тест()\n    Справочники =\n        Справочники;\nКонецПроцедуры\n";
        let mut db = ide_db::RootDatabaseImpl::default();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text);
        let mut config = ide_diagnostics::DiagnosticsConfig::all_enabled();
        config.scope = Some(std::sync::Arc::new(ide_db::base_db::AnalysisScope::from_report(
            "vendor",
            std::path::Path::new("/"),
            [("test.bsl".to_string(), Some(vec![[2, 3]]))],
        )));
        config.defer_scope_line_gate = true;

        let mut without_winner = config.clone();
        without_winner.disabled.push(ide_diagnostics::DiagnosticCode::GlobalPropertyNotWritable);
        let loser_only = ide_diagnostics::file_diagnostics(&db, file_id, &without_winner);
        assert!(
            loser_only.iter().any(|d| d.code == ide_diagnostics::DiagnosticCode::SelfAssign),
            "control: the fixture must emit the loser: {loser_only:#?}"
        );

        let full = ide_diagnostics::file_diagnostics(&db, file_id, &config);
        let candidates = |diagnostics: &[ide_diagnostics::Diagnostic]| -> Vec<_> {
            let lines: Vec<_> = text.lines().collect();
            let index = line_index::LineIndex::new(text);
            diagnostics
                .iter()
                .map(|d| {
                    let out = d.to_output(text);
                    BaselineDiagnosticCandidate {
                        diagnostic: d.code,
                        path: "test.bsl".to_owned(),
                        code: out.code,
                        snippet: Some(diagnostic_line_snippet(&lines, &index, out.start_line)),
                        message: out.message,
                        severity: out.severity,
                        range: DiagnosticsBaselineRange {
                            start_line: out.start_line as u32,
                            start_column: out.start_column as u32,
                            end_line: out.end_line as u32,
                            end_column: out.end_column as u32,
                        },
                    }
                })
                .collect()
        };
        let coverage = DiagnosticsBaselineCoverage::Partial { completed_files: BTreeSet::new() };
        let recorded = classify_diagnostics(
            &baseline(vec![]),
            "b.json".to_owned(),
            candidates(&full),
            &coverage,
        )
        .unwrap();
        let winner: Vec<_> = recorded
            .new
            .iter()
            .filter(|item| {
                item.diagnostic == ide_diagnostics::DiagnosticCode::GlobalPropertyNotWritable
            })
            .map(|item| item.entry.clone())
            .collect();
        assert_eq!(winner.len(), 1, "the fixture must emit the winner: {full:#?}");

        let classified = classify_diagnostics(
            &baseline(winner),
            "b.json".to_owned(),
            candidates(&full),
            &coverage,
        )
        .unwrap();
        assert_eq!(classified.known.len(), 1, "the baseline must hide the winner");
        assert!(
            classified
                .new
                .iter()
                .all(|item| item.diagnostic != ide_diagnostics::DiagnosticCode::SelfAssign),
            "the loser must not come back when its winner is hidden: {:#?}",
            classified.new
        );
    }

    fn state_string(state: DiagnosticsBaselineState) -> &'static str {
        match state {
            DiagnosticsBaselineState::Disabled => "disabled",
            DiagnosticsBaselineState::Full => "full",
            DiagnosticsBaselineState::Partial => "partial",
            DiagnosticsBaselineState::Error => "error",
        }
    }
}
