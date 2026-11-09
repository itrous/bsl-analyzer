use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;

use project_model::{
    DiagnosticsBaselinePartitionIdentity, DiagnosticsBaselinePartitionPlan,
    DiagnosticsBaselineSelection, ManagedBaselineDirectory,
};
use serde::de::{DeserializeSeed, Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::diagnostics_baseline::{
    diagnostic_fingerprint, normalize_diagnostic_snippet, BaselineDiagnosticCandidate,
    ClassifiedDiagnostic, DiagnosticsBaseline, DiagnosticsBaselineCoverage,
    DiagnosticsBaselineEntry, DiagnosticsBaselineError, DiagnosticsBaselinePartitionSummary,
    DiagnosticsBaselineRange, DiagnosticsBaselineState, DiagnosticsBaselineSummary,
    MissingDiagnosticSnippet, ResolvedPolicy,
};

pub const DIAGNOSTICS_BASELINE_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineManifest {
    pub schema_version: u32,
    pub generation: String,
    pub project_scope_fingerprint: String,
    pub partitions: Vec<DiagnosticsBaselineManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselineManifestEntry {
    pub partition_id: String,
    pub file: String,
    pub blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsBaselinePartitionFile {
    pub schema_version: u32,
    pub partition: DiagnosticsBaselinePartitionIdentity,
    pub diagnostics: Vec<DiagnosticsBaselineEntry>,
}

#[derive(Debug, Clone)]
pub struct DiagnosticsBaselineSetSnapshot {
    pub manifest: DiagnosticsBaselineManifest,
    pub manifest_hash: [u8; 32],
    pub partitions: BTreeMap<String, Arc<PartitionSnapshot>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticsBaselineLoadStats {
    pub partitions_parsed: usize,
    pub fingerprints_validated: usize,
    pub objects_read: BTreeSet<String>,
    /// Read calls the loader issued against the object files. Buffered reading keeps
    /// this near `bytes / 8192`; an unbuffered reader makes it one per byte.
    pub object_reads: usize,
}

#[derive(Debug, Clone)]
pub struct PartitionSnapshot {
    pub identity: DiagnosticsBaselinePartitionIdentity,
    pub file: Arc<str>,
    pub hash: [u8; 32],
    entries: Vec<CompactBaselineEntry>,
}

#[derive(Debug, Clone)]
struct CompactBaselineEntry {
    fingerprint: [u8; 32],
    path: Arc<str>,
    code: Arc<str>,
    snippet: Box<str>,
    occurrence: u32,
    message: Box<str>,
    severity: Arc<str>,
    range: DiagnosticsBaselineRange,
}

impl CompactBaselineEntry {
    fn owned(&self) -> DiagnosticsBaselineEntry {
        DiagnosticsBaselineEntry {
            fingerprint: hex(&self.fingerprint),
            path: self.path.to_string(),
            code: self.code.to_string(),
            snippet: self.snippet.to_string(),
            occurrence: self.occurrence,
            message: self.message.to_string(),
            severity: self.severity.to_string(),
            range: self.range.clone(),
        }
    }
}

impl PartitionSnapshot {
    fn find(&self, fingerprint: &[u8; 32]) -> Option<&CompactBaselineEntry> {
        self.entries
            .binary_search_by_key(fingerprint, |entry| entry.fingerprint)
            .ok()
            .map(|index| &self.entries[index])
    }

    pub fn entries_len(&self) -> usize {
        self.entries.len()
    }

    pub fn owned_entries(&self) -> impl Iterator<Item = DiagnosticsBaselineEntry> + '_ {
        self.entries.iter().map(CompactBaselineEntry::owned)
    }
}

#[derive(Debug, Clone)]
pub struct PartitionedBaselineDiagnosticCandidate<T> {
    pub partition_id: String,
    pub candidate: BaselineDiagnosticCandidate<T>,
}

#[derive(Debug, Clone)]
pub struct ClassifiedPartitionedDiagnostics<T> {
    pub new: Vec<ClassifiedDiagnostic<T>>,
    pub known: Vec<ClassifiedDiagnostic<T>>,
    pub unsuppressed: Vec<ClassifiedDiagnostic<T>>,
    pub resolved: ResolvedPartitionedDiagnostics,
    pub summary: DiagnosticsBaselineSummary,
}

#[derive(Debug, thiserror::Error)]
pub enum PartitionedDiagnosticsClassificationError {
    #[error(transparent)]
    MissingSnippet(#[from] MissingDiagnosticSnippet),
    #[error("enabled partition is absent from the loaded snapshot: {0}")]
    MissingEnabledPartition(String),
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedPartitionedDiagnostics {
    partitions: Vec<ResolvedPartition>,
    len: usize,
}

#[derive(Debug, Clone)]
struct ResolvedPartition {
    id: String,
    partition: Arc<PartitionSnapshot>,
    matched: HashSet<[u8; 32]>,
    coverage: DiagnosticsBaselineCoverage,
}

impl ResolvedPartitionedDiagnostics {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn retain_partition(mut self, id: &str) -> Self {
        self.partitions.retain(|partition| partition.id == id);
        self.len = self
            .partitions
            .iter()
            .map(|partition| {
                partition
                    .partition
                    .entries
                    .iter()
                    .filter(|entry| {
                        let covered = match &partition.coverage {
                            DiagnosticsBaselineCoverage::Full => true,
                            DiagnosticsBaselineCoverage::Partial { completed_files } => {
                                completed_files.contains(entry.path.as_ref())
                            }
                        };
                        covered && !partition.matched.contains(&entry.fingerprint)
                    })
                    .count()
            })
            .sum();
        self
    }
}

pub struct ResolvedPartitionedDiagnosticsIter {
    partitions: std::vec::IntoIter<ResolvedPartition>,
    current: Option<(ResolvedPartition, usize)>,
    remaining: usize,
}

impl Iterator for ResolvedPartitionedDiagnosticsIter {
    type Item = DiagnosticsBaselineEntry;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (partition, index) = match &mut self.current {
                Some(current) => current,
                None => {
                    self.current = Some((self.partitions.next()?, 0));
                    continue;
                }
            };
            let Some(entry) = partition.partition.entries.get(*index) else {
                self.current = None;
                continue;
            };
            *index += 1;
            let covered = match &partition.coverage {
                DiagnosticsBaselineCoverage::Full => true,
                DiagnosticsBaselineCoverage::Partial { completed_files } => {
                    completed_files.contains(entry.path.as_ref())
                }
            };
            if covered && !partition.matched.contains(&entry.fingerprint) {
                self.remaining -= 1;
                return Some(entry.owned());
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for ResolvedPartitionedDiagnosticsIter {}

impl IntoIterator for ResolvedPartitionedDiagnostics {
    type Item = DiagnosticsBaselineEntry;
    type IntoIter = ResolvedPartitionedDiagnosticsIter;

    fn into_iter(self) -> Self::IntoIter {
        ResolvedPartitionedDiagnosticsIter {
            partitions: self.partitions.into_iter(),
            current: None,
            remaining: self.len,
        }
    }
}

pub fn partitioned_coverage(
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
    coverage: &DiagnosticsBaselineCoverage,
    all_project_files: Option<&BTreeSet<String>>,
) -> Result<BTreeMap<String, DiagnosticsBaselineCoverage>, String> {
    let mut result: BTreeMap<_, _> = plan
        .partitions
        .iter()
        .map(|partition| {
            (
                partition.id.clone(),
                match coverage {
                    DiagnosticsBaselineCoverage::Full => DiagnosticsBaselineCoverage::Full,
                    DiagnosticsBaselineCoverage::Partial { .. } => {
                        DiagnosticsBaselineCoverage::Partial { completed_files: BTreeSet::new() }
                    }
                },
            )
        })
        .collect();
    let DiagnosticsBaselineCoverage::Partial { completed_files } = coverage else {
        return Ok(result);
    };
    for path in completed_files {
        let owner = plan
            .owner_for_project_path(path)
            .ok_or_else(|| format!("completed diagnostics file has no partition owner: {path}"))?;
        let DiagnosticsBaselineCoverage::Partial { completed_files } =
            result.get_mut(owner).expect("plan owner exists")
        else {
            unreachable!()
        };
        completed_files.insert(path.clone());
    }
    let Some(all_project_files) = all_project_files else { return Ok(result) };
    let mut owner_files: BTreeMap<_, BTreeSet<_>> =
        plan.partitions.iter().map(|partition| (partition.id.clone(), BTreeSet::new())).collect();
    for path in all_project_files {
        let owner = plan
            .owner_for_project_path(path)
            .ok_or_else(|| format!("diagnostics file has no partition owner: {path}"))?;
        owner_files.get_mut(owner).expect("plan owner exists").insert(path.clone());
    }
    for (partition_id, partition_coverage) in &mut result {
        let DiagnosticsBaselineCoverage::Partial { completed_files } = partition_coverage else {
            continue;
        };
        if !owner_files[partition_id].is_empty()
            && owner_files[partition_id].is_subset(completed_files)
        {
            *partition_coverage = DiagnosticsBaselineCoverage::Full;
        }
    }
    Ok(result)
}

/// [`classify_partitioned_diagnostics_with`] that also computes `resolved`.
pub fn classify_partitioned_diagnostics<T>(
    snapshot: &DiagnosticsBaselineSetSnapshot,
    plan: &DiagnosticsBaselinePartitionPlan,
    baseline_path: String,
    current: Vec<PartitionedBaselineDiagnosticCandidate<T>>,
    coverage: &BTreeMap<String, DiagnosticsBaselineCoverage>,
) -> Result<ClassifiedPartitionedDiagnostics<T>, PartitionedDiagnosticsClassificationError> {
    classify_partitioned_diagnostics_with(
        snapshot,
        plan,
        baseline_path,
        current,
        coverage,
        ResolvedPolicy::Compute,
    )
}

/// The partitioned form of
/// [`classify_diagnostics_with`](crate::diagnostics_baseline::classify_diagnostics_with),
/// under the same contract: `current` holds every finalized finding of each file it
/// mentions, identity is fixed here in each [`ClassifiedDiagnostic::entry`], and callers
/// only shape the result afterwards — they may drop entries of `new`, `known` or
/// `unsuppressed` without renumbering them, never classify a reduced set again. A
/// finding of an `Unsuppressed` partition is numbered like any other; the policy only
/// decides that it stays active.
pub fn classify_partitioned_diagnostics_with<T>(
    snapshot: &DiagnosticsBaselineSetSnapshot,
    plan: &DiagnosticsBaselinePartitionPlan,
    baseline_path: String,
    mut current: Vec<PartitionedBaselineDiagnosticCandidate<T>>,
    coverage: &BTreeMap<String, DiagnosticsBaselineCoverage>,
    resolved_policy: ResolvedPolicy,
) -> Result<ClassifiedPartitionedDiagnostics<T>, PartitionedDiagnosticsClassificationError> {
    current.sort_by(|left, right| {
        let left = &left.candidate;
        let right = &right.candidate;
        (&left.path, &left.range, &left.code, &left.message).cmp(&(
            &right.path,
            &right.range,
            &right.code,
            &right.message,
        ))
    });
    let mut occurrences = HashMap::<(String, String, String), u32>::new();
    let mut matched = BTreeMap::<String, HashSet<[u8; 32]>>::new();
    let mut counts = BTreeMap::<String, (usize, usize, usize)>::new();
    let mut new = Vec::new();
    let mut known = Vec::new();
    let mut unsuppressed = Vec::new();

    for item in current {
        let candidate = item.candidate;
        let snippet = normalize_diagnostic_snippet(
            candidate.snippet.as_deref().ok_or(MissingDiagnosticSnippet)?,
        );
        let occurrence = occurrences
            .entry((candidate.path.clone(), candidate.code.clone(), snippet.clone()))
            .or_default();
        let fingerprint =
            diagnostic_fingerprint(&candidate.path, &candidate.code, &snippet, *occurrence);
        let fingerprint_bytes = parse_hash(&fingerprint).expect("generated BLAKE3 is valid");
        let entry = DiagnosticsBaselineEntry {
            fingerprint,
            path: candidate.path,
            code: candidate.code,
            snippet,
            occurrence: *occurrence,
            message: candidate.message,
            severity: candidate.severity,
            range: candidate.range,
        };
        *occurrence += 1;
        let classified = ClassifiedDiagnostic { diagnostic: candidate.diagnostic, entry };
        if !protected(&classified.entry.code)
            && plan.policy_for_partition(&item.partition_id)
                == Some(project_model::DiagnosticsBaselinePartitionPolicy::Unsuppressed)
        {
            counts.entry(item.partition_id).or_default().2 += 1;
            unsuppressed.push(classified);
            continue;
        }
        let partition = snapshot.partitions.get(&item.partition_id);
        let is_known = !protected(&classified.entry.code)
            && partition.is_some_and(|partition| partition.find(&fingerprint_bytes).is_some());
        let partition_counts = counts.entry(item.partition_id.clone()).or_default();
        if is_known {
            partition_counts.1 += 1;
            matched.entry(item.partition_id).or_default().insert(fingerprint_bytes);
            known.push(classified);
        } else {
            partition_counts.0 += 1;
            new.push(classified);
        }
    }

    let mut resolved = ResolvedPartitionedDiagnostics::default();
    let mut partition_summaries = Vec::with_capacity(plan.partitions.len());
    for expected_partition in &plan.partitions {
        let policy =
            plan.policy_for_partition(&expected_partition.id).expect("plan partition has policy");
        // A partition the coverage map does not mention was NOT proven analysed. Assuming
        // full coverage there would report every unmatched entry of that partition as
        // resolved — "the debt is gone" about files nobody looked at.
        let unproven = DiagnosticsBaselineCoverage::Partial { completed_files: BTreeSet::new() };
        let partition_coverage = coverage.get(&expected_partition.id).unwrap_or(&unproven);
        let complete = matches!(partition_coverage, DiagnosticsBaselineCoverage::Full);
        let (partition_new, partition_known, partition_unsuppressed) =
            counts.get(&expected_partition.id).copied().unwrap_or_default();
        let mut partition_resolved = 0;
        let (identity, path, schema_version) = match policy {
            project_model::DiagnosticsBaselinePartitionPolicy::Baseline => {
                let manifest_entry = snapshot
                    .manifest
                    .partitions
                    .iter()
                    .find(|entry| entry.partition_id == expected_partition.id)
                    .ok_or_else(|| {
                        PartitionedDiagnosticsClassificationError::MissingEnabledPartition(
                            expected_partition.id.clone(),
                        )
                    })?;
                let partition =
                    snapshot.partitions.get(&expected_partition.id).cloned().ok_or_else(|| {
                        PartitionedDiagnosticsClassificationError::MissingEnabledPartition(
                            expected_partition.id.clone(),
                        )
                    })?;
                let matched = matched.remove(&expected_partition.id).unwrap_or_default();
                if resolved_policy == ResolvedPolicy::Compute {
                    for entry in &partition.entries {
                        let covered = match partition_coverage {
                            DiagnosticsBaselineCoverage::Full => true,
                            DiagnosticsBaselineCoverage::Partial { completed_files } => {
                                completed_files.contains(entry.path.as_ref())
                            }
                        };
                        if covered && !matched.contains(&entry.fingerprint) {
                            partition_resolved += 1;
                        }
                    }
                }
                resolved.len += partition_resolved;
                // Under `Skip` nothing was counted, so nothing may be handed out either:
                // `ResolvedPartitionedDiagnostics` promises that `len` equals what its
                // iterator yields, and it is an `ExactSizeIterator`.
                if resolved_policy == ResolvedPolicy::Compute {
                    resolved.partitions.push(ResolvedPartition {
                        id: expected_partition.id.clone(),
                        partition: partition.clone(),
                        matched,
                        coverage: partition_coverage.clone(),
                    });
                }
                (
                    partition.identity.clone(),
                    Some(manifest_entry.file.clone()),
                    Some(DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION),
                )
            }
            project_model::DiagnosticsBaselinePartitionPolicy::Unsuppressed => {
                (expected_partition.identity.clone(), None, None)
            }
        };
        partition_summaries.push(DiagnosticsBaselinePartitionSummary {
            id: expected_partition.id.clone(),
            identity,
            policy,
            path,
            schema_version,
            state: if complete {
                DiagnosticsBaselineState::Full
            } else {
                DiagnosticsBaselineState::Partial
            },
            new: partition_new,
            known: partition_known,
            resolved: partition_resolved,
            unsuppressed: partition_unsuppressed,
            complete,
        });
    }
    let complete = partition_summaries.iter().all(|partition| partition.complete);
    let summary = DiagnosticsBaselineSummary {
        state: if complete {
            DiagnosticsBaselineState::Full
        } else {
            DiagnosticsBaselineState::Partial
        },
        selection: Some(plan.selection),
        partitions_enabled: Some(plan.enabled_partition_ids.len()),
        partitions_unsuppressed: Some(plan.partitions.len() - plan.enabled_partition_ids.len()),
        unsuppressed: Some(unsuppressed.len()),
        new: Some(new.len()),
        known: Some(known.len()),
        resolved: match resolved_policy {
            ResolvedPolicy::Compute => Some(resolved.len()),
            ResolvedPolicy::Skip => None,
        },
        path: Some(baseline_path),
        schema_version: Some(DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION),
        manifest_schema_version: Some(DIAGNOSTICS_BASELINE_MANIFEST_SCHEMA_VERSION),
        complete,
        error_code: None,
        detail: None,
        partitions: partition_summaries,
        errors: vec![],
    };
    Ok(ClassifiedPartitionedDiagnostics { new, known, unsuppressed, resolved, summary })
}

pub fn migrate_v1_to_partitioned(
    baseline: &DiagnosticsBaseline,
    plan: &DiagnosticsBaselinePartitionPlan,
) -> Result<BTreeMap<String, Vec<DiagnosticsBaselineEntry>>, PartitionedDiagnosticsBaselineError> {
    // Reuse the unchanged schema-v1 validator; migration must not define a second recipe.
    crate::diagnostics_baseline::diagnostics_baseline_json(baseline)?;
    let expected_scope = expected_v1_scope(plan);
    if !crate::diagnostics_baseline::legacy_scope_markers_are_valid(&expected_scope) {
        return Err(PartitionedDiagnosticsBaselineError::InvalidLegacyScopeMarker);
    }
    if !crate::diagnostics_baseline::scopes_match(&baseline.scope, &expected_scope) {
        return Err(PartitionedDiagnosticsBaselineError::ScopeMismatch);
    }
    let mut result: BTreeMap<_, Vec<_>> =
        plan.partitions.iter().map(|partition| (partition.id.clone(), Vec::new())).collect();
    for entry in &baseline.diagnostics {
        let owner = plan.owner_for_project_path(&entry.path).ok_or_else(|| {
            PartitionedDiagnosticsBaselineError::UnownedDiagnostic(entry.path.clone())
        })?;
        result.get_mut(owner).expect("plan owner exists").push(entry.clone());
    }
    for entries in result.values_mut() {
        entries.sort_by(entry_sort);
    }
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticsBaselineMigrationStats {
    pub migrated: usize,
    pub skipped_unsuppressed: usize,
}

pub fn migrate_v1_reader<R, F>(
    reader: R,
    plan: &DiagnosticsBaselinePartitionPlan,
    mut write_entry: F,
) -> Result<DiagnosticsBaselineMigrationStats, PartitionedDiagnosticsBaselineError>
where
    R: Read,
    F: FnMut(&str, &DiagnosticsBaselineEntry) -> Result<(), PartitionedDiagnosticsBaselineError>,
{
    // Buffered here rather than at the call site: `from_reader` reads byte by byte,
    // and a caller that forgets the wrapper gets no error, only a hundredfold cost.
    let mut deserializer = serde_json::Deserializer::from_reader(std::io::BufReader::new(reader));
    let parsed = LegacyMigrationSeed { plan, write_entry: &mut write_entry }
        .deserialize(&mut deserializer)?;
    deserializer.end()?;
    if parsed.schema_version != crate::diagnostics_baseline::DIAGNOSTICS_BASELINE_SCHEMA_VERSION {
        return Err(DiagnosticsBaselineError::UnsupportedSchema {
            found: parsed.schema_version,
            expected: crate::diagnostics_baseline::DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
        }
        .into());
    }
    let expected_scope = expected_v1_scope(plan);
    if !crate::diagnostics_baseline::legacy_scope_markers_are_valid(&parsed.scope)
        || !crate::diagnostics_baseline::legacy_scope_markers_are_valid(&expected_scope)
    {
        return Err(PartitionedDiagnosticsBaselineError::InvalidLegacyScopeMarker);
    }
    if !crate::diagnostics_baseline::scopes_match(&parsed.scope, &expected_scope) {
        return Err(PartitionedDiagnosticsBaselineError::ScopeMismatch);
    }
    Ok(parsed.entries)
}

fn expected_v1_scope(
    plan: &DiagnosticsBaselinePartitionPlan,
) -> crate::diagnostics_baseline::DiagnosticsBaselineScope {
    crate::diagnostics_baseline::DiagnosticsBaselineScope {
        source_root: plan.project_scope.source_root.clone(),
        extensions: plan.project_scope.extensions.clone(),
    }
}

struct LegacyMigration {
    schema_version: u32,
    scope: crate::diagnostics_baseline::DiagnosticsBaselineScope,
    entries: DiagnosticsBaselineMigrationStats,
}

struct LegacyMigrationSeed<'a, F> {
    plan: &'a DiagnosticsBaselinePartitionPlan,
    write_entry: &'a mut F,
}

impl<'de, F> DeserializeSeed<'de> for LegacyMigrationSeed<'_, F>
where
    F: FnMut(&str, &DiagnosticsBaselineEntry) -> Result<(), PartitionedDiagnosticsBaselineError>,
{
    type Value = LegacyMigration;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(LegacyMigrationVisitor {
            plan: self.plan,
            write_entry: self.write_entry,
        })
    }
}

struct LegacyMigrationVisitor<'a, F> {
    plan: &'a DiagnosticsBaselinePartitionPlan,
    write_entry: &'a mut F,
}

impl<'de, F> Visitor<'de> for LegacyMigrationVisitor<'_, F>
where
    F: FnMut(&str, &DiagnosticsBaselineEntry) -> Result<(), PartitionedDiagnosticsBaselineError>,
{
    type Value = LegacyMigration;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a schema-v1 diagnostics baseline")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut schema_version = None;
        let mut scope = None;
        let mut entries = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "schema_version" if schema_version.is_none() => {
                    schema_version = Some(map.next_value()?);
                }
                "scope" if scope.is_none() => scope = Some(map.next_value()?),
                "diagnostics" if entries.is_none() => {
                    entries = Some(map.next_value_seed(LegacyEntriesSeed {
                        plan: self.plan,
                        write_entry: self.write_entry,
                    })?);
                }
                "schema_version" | "scope" | "diagnostics" => {
                    return Err(A::Error::custom(format!("duplicate field {key}")));
                }
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                    return Err(A::Error::custom(format!("unknown field {key}")));
                }
            }
        }
        Ok(LegacyMigration {
            schema_version: schema_version
                .ok_or_else(|| A::Error::missing_field("schema_version"))?,
            scope: scope.ok_or_else(|| A::Error::missing_field("scope"))?,
            entries: entries.ok_or_else(|| A::Error::missing_field("diagnostics"))?,
        })
    }
}

struct LegacyEntriesSeed<'a, F> {
    plan: &'a DiagnosticsBaselinePartitionPlan,
    write_entry: &'a mut F,
}

impl<'de, F> DeserializeSeed<'de> for LegacyEntriesSeed<'_, F>
where
    F: FnMut(&str, &DiagnosticsBaselineEntry) -> Result<(), PartitionedDiagnosticsBaselineError>,
{
    type Value = DiagnosticsBaselineMigrationStats;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(LegacyEntriesVisitor {
            plan: self.plan,
            write_entry: self.write_entry,
        })
    }
}

struct LegacyEntriesVisitor<'a, F> {
    plan: &'a DiagnosticsBaselinePartitionPlan,
    write_entry: &'a mut F,
}

impl<'de, F> Visitor<'de> for LegacyEntriesVisitor<'_, F>
where
    F: FnMut(&str, &DiagnosticsBaselineEntry) -> Result<(), PartitionedDiagnosticsBaselineError>,
{
    type Value = DiagnosticsBaselineMigrationStats;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an array of schema-v1 diagnostics baseline entries")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut seen = HashSet::new();
        let mut last_by_partition: HashMap<String, DiagnosticsBaselineEntry> = HashMap::new();
        let mut migrated = 0;
        let mut skipped_unsuppressed = 0;
        while let Some(entry) = sequence.next_element::<DiagnosticsBaselineEntry>()? {
            validate_source_path(&entry.path, false).map_err(A::Error::custom)?;
            if protected(&entry.code) {
                return Err(A::Error::custom(
                    PartitionedDiagnosticsBaselineError::ProtectedDiagnostic(entry.code),
                ));
            }
            let expected =
                diagnostic_fingerprint(&entry.path, &entry.code, &entry.snippet, entry.occurrence);
            if entry.fingerprint != expected {
                return Err(A::Error::custom(
                    PartitionedDiagnosticsBaselineError::FingerprintMismatch(entry.fingerprint),
                ));
            }
            let owner = self.plan.owner_for_project_path(&entry.path).ok_or_else(|| {
                A::Error::custom(PartitionedDiagnosticsBaselineError::UnownedDiagnostic(
                    entry.path.clone(),
                ))
            })?;
            if self.plan.enabled_partition_ids.iter().any(|id| id == owner) {
                let fingerprint = parse_hash(&entry.fingerprint).map_err(A::Error::custom)?;
                if !seen.insert(fingerprint) {
                    return Err(A::Error::custom(PartitionedDiagnosticsBaselineError::Duplicate(
                        entry.fingerprint,
                    )));
                }
                // Streaming cannot sort — that is the point of streaming — so the input
                // must already be canonical. Otherwise the object written here differs
                // byte-wise from what `diagnostics_partition_json` regenerates, and
                // `create --partition` can never repair the migrated set.
                if let Some(previous) = last_by_partition.get(owner) {
                    if entry_sort(previous, &entry) == std::cmp::Ordering::Greater {
                        return Err(A::Error::custom(
                            PartitionedDiagnosticsBaselineError::UnsortedLegacyBaseline(
                                entry.path.clone(),
                            ),
                        ));
                    }
                }
                last_by_partition.insert(owner.to_owned(), entry.clone());
                (self.write_entry)(owner, &entry).map_err(A::Error::custom)?;
                migrated += 1;
            } else {
                skipped_unsuppressed += 1;
            }
        }
        Ok(DiagnosticsBaselineMigrationStats { migrated, skipped_unsuppressed })
    }
}

#[derive(Default)]
/// Interner holding ONE copy of each string.
///
/// A map keyed by `String` would store the bytes twice — once in the key, once in the
/// shared `Arc<str>` — and on a set of 1.6M distinct paths that second copy is tens of
/// megabytes that nothing ever reads, counted by the resident-memory gate.
struct StringPool(HashSet<Arc<str>>);

impl StringPool {
    fn intern(&mut self, value: String) -> Arc<str> {
        if let Some(existing) = self.0.get(value.as_str()) {
            return existing.clone();
        }
        let shared: Arc<str> = Arc::from(value);
        self.0.insert(shared.clone());
        shared
    }
}

pub fn load_diagnostics_baseline_set(
    directory: &ManagedBaselineDirectory,
    plan: &DiagnosticsBaselinePartitionPlan,
) -> Result<DiagnosticsBaselineSetSnapshot, PartitionedDiagnosticsBaselineError> {
    load_diagnostics_baseline_set_reusing(directory, plan, None, &BTreeSet::new())
        .map(|(snapshot, _)| snapshot)
}

pub fn load_diagnostics_baseline_set_reusing(
    directory: &ManagedBaselineDirectory,
    plan: &DiagnosticsBaselinePartitionPlan,
    previous: Option<&DiagnosticsBaselineSetSnapshot>,
    changed_objects: &BTreeSet<String>,
) -> Result<
    (DiagnosticsBaselineSetSnapshot, DiagnosticsBaselineLoadStats),
    PartitionedDiagnosticsBaselineError,
> {
    for attempt in 0..2 {
        match load_diagnostics_baseline_set_once(directory, plan, previous, changed_objects) {
            Err(PartitionedDiagnosticsBaselineError::ConcurrentUpdate) if attempt == 0 => {}
            result => return result,
        }
    }
    unreachable!("the bounded manifest retry always returns on its second attempt")
}

fn load_diagnostics_baseline_set_once(
    directory: &ManagedBaselineDirectory,
    plan: &DiagnosticsBaselinePartitionPlan,
    previous: Option<&DiagnosticsBaselineSetSnapshot>,
    changed_objects: &BTreeSet<String>,
) -> Result<
    (DiagnosticsBaselineSetSnapshot, DiagnosticsBaselineLoadStats),
    PartitionedDiagnosticsBaselineError,
> {
    let first_manifest = read_file(directory, "manifest.json")?;
    let loaded = (|| {
        let manifest: DiagnosticsBaselineManifest = serde_json::from_slice(&first_manifest)?;
        validate_manifest_shape(&manifest)?;
        let expected: BTreeMap<_, _> =
            plan.partitions.iter().map(|partition| (partition.id.as_str(), partition)).collect();
        let expected_ids: BTreeSet<_> =
            plan.enabled_partition_ids.iter().map(String::as_str).collect();
        let actual_ids: BTreeSet<_> =
            manifest.partitions.iter().map(|partition| partition.partition_id.as_str()).collect();
        let missing: Vec<_> =
            expected_ids.difference(&actual_ids).map(|id| (*id).to_owned()).collect();
        let orphan: Vec<_> = if plan.selection == DiagnosticsBaselineSelection::All {
            actual_ids.difference(&expected_ids).map(|id| (*id).to_owned()).collect()
        } else {
            Vec::new()
        };
        if !missing.is_empty() {
            return Err(PartitionedDiagnosticsBaselineError::MissingPartitions {
                ids: missing,
                orphan_ids: orphan,
            });
        }
        if !orphan.is_empty() {
            return Err(PartitionedDiagnosticsBaselineError::OrphanPartitions(orphan));
        }

        let reusable: BTreeSet<_> = manifest
            .partitions
            .iter()
            .filter(|entry| {
                let Some(expected_partition) = expected.get(entry.partition_id.as_str()) else {
                    return false;
                };
                if !expected_ids.contains(entry.partition_id.as_str()) {
                    return false;
                }
                previous
                    .and_then(|snapshot| snapshot.partitions.get(&entry.partition_id))
                    .is_some_and(|partition| {
                        !changed_objects.contains(&entry.file)
                            && partition.file.as_ref() == entry.file
                            && hex(&partition.hash) == entry.blake3
                            && partition.identity == expected_partition.identity
                    })
            })
            .map(|entry| entry.partition_id.as_str())
            .collect();
        let reused_partitions: Vec<_> = previous
            .into_iter()
            .flat_map(|snapshot| &snapshot.partitions)
            .filter(|(id, _)| reusable.contains(id.as_str()))
            .map(|(_, partition)| partition)
            .collect();

        let mut pool = StringPool::default();
        let mut seen_changed_fingerprints = HashSet::new();
        let mut partitions = BTreeMap::new();
        let mut stats = DiagnosticsBaselineLoadStats::default();
        for manifest_entry in &manifest.partitions {
            if !expected_ids.contains(manifest_entry.partition_id.as_str()) {
                continue;
            }
            let expected_partition = expected[manifest_entry.partition_id.as_str()];
            if reusable.contains(manifest_entry.partition_id.as_str()) {
                let partition = previous.unwrap().partitions[&manifest_entry.partition_id].clone();
                partitions.insert(manifest_entry.partition_id.clone(), partition);
                continue;
            }
            let file = match directory.open_file(&manifest_entry.file) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(PartitionedDiagnosticsBaselineError::MissingPartitions {
                        ids: vec![manifest_entry.partition_id.clone()],
                        orphan_ids: vec![],
                    });
                }
                Err(error) => return Err(error.into()),
            };
            stats.objects_read.insert(manifest_entry.file.clone());
            // The buffer belongs BETWEEN serde and the file: `from_reader` requests one
            // byte at a time, so an unbuffered file costs a syscall — and a one-byte
            // hasher update — per byte of the object.
            let mut reader = std::io::BufReader::new(HashingReader::new(file));
            let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
            let parsed = PartitionSeed { pool: &mut pool }.deserialize(&mut deserializer)?;
            stats.partitions_parsed += 1;
            deserializer.end()?;
            drop(deserializer);
            let hashing = reader.into_inner();
            stats.object_reads += hashing.reads();
            let actual_hash = hashing.finalize();
            let expected_path = partition_object_path(
                &expected_partition.id,
                &expected_partition.key,
                &manifest_entry.blake3,
            )?;
            if manifest_entry.file != expected_path {
                return Err(PartitionedDiagnosticsBaselineError::InvalidPath(
                    manifest_entry.file.clone(),
                ));
            }
            if hex(&actual_hash) != manifest_entry.blake3 {
                return Err(PartitionedDiagnosticsBaselineError::ObjectHashMismatch(
                    manifest_entry.partition_id.clone(),
                ));
            }
            if parsed.schema_version != DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION {
                return Err(PartitionedDiagnosticsBaselineError::UnsupportedPartitionSchema(
                    parsed.schema_version,
                ));
            }
            if !parsed.identity.legacy_scope_marker_is_valid() {
                return Err(PartitionedDiagnosticsBaselineError::InvalidLegacyScopeMarker);
            }
            if parsed.identity != expected_partition.identity {
                return Err(PartitionedDiagnosticsBaselineError::PartitionIdentityMismatch(
                    manifest_entry.partition_id.clone(),
                ));
            }
            for entry in &parsed.entries {
                stats.fingerprints_validated += 1;
                if plan.owner_for_project_path(&entry.path) != Some(&manifest_entry.partition_id) {
                    return Err(PartitionedDiagnosticsBaselineError::UnownedDiagnostic(
                        entry.path.to_string(),
                    ));
                }
                if !seen_changed_fingerprints.insert(entry.fingerprint)
                    || reused_partitions
                        .iter()
                        .any(|partition| partition.find(&entry.fingerprint).is_some())
                {
                    return Err(PartitionedDiagnosticsBaselineError::Duplicate(hex(
                        &entry.fingerprint
                    )));
                }
            }
            partitions.insert(
                manifest_entry.partition_id.clone(),
                Arc::new(PartitionSnapshot {
                    identity: parsed.identity,
                    file: Arc::from(manifest_entry.file.as_str()),
                    hash: actual_hash,
                    entries: parsed.entries,
                }),
            );
        }
        if plan.selection == DiagnosticsBaselineSelection::All
            && manifest.project_scope_fingerprint != plan.project_scope_fingerprint
        {
            return Err(PartitionedDiagnosticsBaselineError::ScopeMismatch);
        }
        Ok((
            DiagnosticsBaselineSetSnapshot {
                manifest,
                manifest_hash: *blake3::hash(&first_manifest).as_bytes(),
                partitions,
            },
            stats,
        ))
    })();
    match read_file(directory, "manifest.json") {
        Ok(second_manifest) if second_manifest == first_manifest => loaded,
        Ok(_) | Err(_) => Err(PartitionedDiagnosticsBaselineError::ConcurrentUpdate),
    }
}

fn read_file(
    directory: &ManagedBaselineDirectory,
    path: &str,
) -> Result<Vec<u8>, PartitionedDiagnosticsBaselineError> {
    let mut bytes = Vec::new();
    directory.open_file(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

struct HashingReader<R> {
    inner: R,
    hasher: blake3::Hasher,
    /// Reads performed on the file. `serde_json` asks its reader one byte at a
    /// time, so this counts a syscall per byte unless a buffer sits in between —
    /// hence a gate over the ratio rather than trust in the call order.
    reads: usize,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, hasher: blake3::Hasher::new(), reads: 0 }
    }

    fn finalize(&self) -> [u8; 32] {
        *self.hasher.clone().finalize().as_bytes()
    }

    fn reads(&self) -> usize {
        self.reads
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.reads += 1;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

struct ParsedPartition {
    schema_version: u32,
    identity: DiagnosticsBaselinePartitionIdentity,
    entries: Vec<CompactBaselineEntry>,
}

struct PartitionSeed<'a> {
    pool: &'a mut StringPool,
}

impl<'de> DeserializeSeed<'de> for PartitionSeed<'_> {
    type Value = ParsedPartition;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(PartitionVisitor { pool: self.pool })
    }
}

struct PartitionVisitor<'a> {
    pool: &'a mut StringPool,
}

impl<'de> Visitor<'de> for PartitionVisitor<'_> {
    type Value = ParsedPartition;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a diagnostics baseline partition")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut schema_version = None;
        let mut identity = None;
        let mut entries = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "schema_version" if schema_version.is_none() => {
                    schema_version = Some(map.next_value()?);
                }
                "partition" if identity.is_none() => identity = Some(map.next_value()?),
                "diagnostics" if entries.is_none() => {
                    entries = Some(map.next_value_seed(EntriesSeed { pool: self.pool })?);
                }
                "schema_version" | "partition" | "diagnostics" => {
                    return Err(A::Error::custom(format!("duplicate field {key}")));
                }
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                    return Err(A::Error::custom(format!("unknown field {key}")));
                }
            }
        }
        Ok(ParsedPartition {
            schema_version: schema_version
                .ok_or_else(|| A::Error::missing_field("schema_version"))?,
            identity: identity.ok_or_else(|| A::Error::missing_field("partition"))?,
            entries: entries.ok_or_else(|| A::Error::missing_field("diagnostics"))?,
        })
    }
}

struct EntriesSeed<'a> {
    pool: &'a mut StringPool,
}

impl<'de> DeserializeSeed<'de> for EntriesSeed<'_> {
    type Value = Vec<CompactBaselineEntry>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(EntriesVisitor { pool: self.pool })
    }
}

struct EntriesVisitor<'a> {
    pool: &'a mut StringPool,
}

impl<'de> Visitor<'de> for EntriesVisitor<'_> {
    type Value = Vec<CompactBaselineEntry>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an array of diagnostics baseline entries")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        let mut seen = HashSet::new();
        while let Some(entry) = sequence.next_element::<DiagnosticsBaselineEntry>()? {
            validate_source_path(&entry.path, false).map_err(A::Error::custom)?;
            if protected(&entry.code) {
                return Err(A::Error::custom(
                    PartitionedDiagnosticsBaselineError::ProtectedDiagnostic(entry.code),
                ));
            }
            let expected =
                diagnostic_fingerprint(&entry.path, &entry.code, &entry.snippet, entry.occurrence);
            if entry.fingerprint != expected {
                return Err(A::Error::custom(
                    PartitionedDiagnosticsBaselineError::FingerprintMismatch(entry.fingerprint),
                ));
            }
            let fingerprint = parse_hash(&entry.fingerprint).map_err(A::Error::custom)?;
            if !seen.insert(fingerprint) {
                return Err(A::Error::custom(PartitionedDiagnosticsBaselineError::Duplicate(
                    entry.fingerprint,
                )));
            }
            entries.push(CompactBaselineEntry {
                fingerprint,
                path: self.pool.intern(entry.path),
                code: self.pool.intern(entry.code),
                snippet: entry.snippet.into_boxed_str(),
                occurrence: entry.occurrence,
                message: entry.message.into_boxed_str(),
                severity: self.pool.intern(entry.severity),
                range: entry.range,
            });
        }
        entries.sort_unstable_by_key(|entry| entry.fingerprint);
        Ok(entries)
    }
}

fn parse_hash(value: &str) -> Result<[u8; 32], PartitionedDiagnosticsBaselineError> {
    validate_hex(value)?;
    Ok(*blake3::Hash::from_hex(value)
        .map_err(|_| PartitionedDiagnosticsBaselineError::InvalidHash(value.to_owned()))?
        .as_bytes())
}

fn hex(value: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*value).to_hex().to_string()
}

pub fn diagnostics_partition_json(
    identity: DiagnosticsBaselinePartitionIdentity,
    mut diagnostics: Vec<DiagnosticsBaselineEntry>,
) -> Result<Vec<u8>, PartitionedDiagnosticsBaselineError> {
    if !identity.legacy_scope_marker_is_valid() {
        return Err(PartitionedDiagnosticsBaselineError::InvalidLegacyScopeMarker);
    }
    diagnostics.sort_by(entry_sort);
    validate_entries(&diagnostics)?;
    let file = DiagnosticsBaselinePartitionFile {
        schema_version: DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION,
        partition: identity,
        diagnostics,
    };
    // Compact, and byte-identical to what `PartitionFileWriter` streams: a set may be
    // published by either writer, and `repair_object` compares regenerated bytes
    // against the manifest hash. A pretty object here would make every migrated set
    // unrepairable. The manifest beside it stays pretty — it is small and read by
    // people, while these objects are content-addressed and read by the tool.
    let mut bytes = serde_json::to_vec(&file)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn diagnostics_manifest(
    project_scope_fingerprint: String,
    mut partitions: Vec<DiagnosticsBaselineManifestEntry>,
) -> DiagnosticsBaselineManifest {
    partitions.sort_by(|left, right| {
        partition_id_sort_key(&left.partition_id).cmp(&partition_id_sort_key(&right.partition_id))
    });
    let generation = baseline_generation(&project_scope_fingerprint, &partitions);
    DiagnosticsBaselineManifest {
        schema_version: DIAGNOSTICS_BASELINE_MANIFEST_SCHEMA_VERSION,
        generation,
        project_scope_fingerprint,
        partitions,
    }
}

fn partition_id_sort_key(id: &str) -> (u8, &str) {
    if id == "main" {
        (0, id)
    } else if id.starts_with("group:") {
        (1, id)
    } else {
        (2, id)
    }
}

pub fn diagnostics_manifest_json(
    manifest: &DiagnosticsBaselineManifest,
) -> Result<Vec<u8>, PartitionedDiagnosticsBaselineError> {
    validate_manifest_shape(manifest)?;
    pretty_json(manifest)
}

pub fn partition_object_path(
    id: &str,
    key: &str,
    hash: &str,
) -> Result<String, PartitionedDiagnosticsBaselineError> {
    validate_hex(key)?;
    validate_hex(hash)?;
    let category = if id == "main" {
        "main"
    } else if id.starts_with("extension:") {
        "extensions"
    } else if id.starts_with("group:") {
        "groups"
    } else {
        return Err(PartitionedDiagnosticsBaselineError::InvalidPartitionId(id.to_owned()));
    };
    Ok(if id == "main" {
        format!("objects/{category}/{hash}.json")
    } else {
        format!("objects/{category}/{key}/{hash}.json")
    })
}

pub fn baseline_generation(scope: &str, partitions: &[DiagnosticsBaselineManifestEntry]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bsl-analyzer/diagnostics-baseline/generation/v1\0");
    hasher.update(scope.as_bytes());
    for partition in partitions {
        hasher.update(&[0]);
        hasher.update(partition.partition_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(partition.blake3.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn pretty_json<T: Serialize>(value: &T) -> Result<Vec<u8>, PartitionedDiagnosticsBaselineError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn entry_sort(
    left: &DiagnosticsBaselineEntry,
    right: &DiagnosticsBaselineEntry,
) -> std::cmp::Ordering {
    (&left.path, &left.code, &left.snippet, left.occurrence).cmp(&(
        &right.path,
        &right.code,
        &right.snippet,
        right.occurrence,
    ))
}

fn validate_entries(
    entries: &[DiagnosticsBaselineEntry],
) -> Result<(), PartitionedDiagnosticsBaselineError> {
    let mut fingerprints = HashSet::new();
    let mut identities = HashSet::new();
    for entry in entries {
        validate_source_path(&entry.path, false)?;
        if protected(&entry.code) {
            return Err(PartitionedDiagnosticsBaselineError::ProtectedDiagnostic(
                entry.code.clone(),
            ));
        }
        let expected =
            diagnostic_fingerprint(&entry.path, &entry.code, &entry.snippet, entry.occurrence);
        if entry.fingerprint != expected {
            return Err(PartitionedDiagnosticsBaselineError::FingerprintMismatch(
                entry.fingerprint.clone(),
            ));
        }
        if !fingerprints.insert(entry.fingerprint.as_str())
            || !identities.insert((
                entry.path.as_str(),
                entry.code.as_str(),
                entry.snippet.as_str(),
                entry.occurrence,
            ))
        {
            return Err(PartitionedDiagnosticsBaselineError::Duplicate(entry.fingerprint.clone()));
        }
    }
    Ok(())
}

fn validate_manifest_shape(
    manifest: &DiagnosticsBaselineManifest,
) -> Result<(), PartitionedDiagnosticsBaselineError> {
    if manifest.schema_version != DIAGNOSTICS_BASELINE_MANIFEST_SCHEMA_VERSION {
        return Err(PartitionedDiagnosticsBaselineError::UnsupportedManifestSchema(
            manifest.schema_version,
        ));
    }
    let mut ids = HashSet::new();
    for entry in &manifest.partitions {
        if !ids.insert(entry.partition_id.as_str()) {
            return Err(PartitionedDiagnosticsBaselineError::DuplicatePartition(
                entry.partition_id.clone(),
            ));
        }
        validate_managed_file(&entry.file)?;
        validate_hex(&entry.blake3)?;
    }
    if manifest.generation
        != baseline_generation(&manifest.project_scope_fingerprint, &manifest.partitions)
    {
        return Err(PartitionedDiagnosticsBaselineError::GenerationMismatch);
    }
    Ok(())
}

fn validate_hex(value: &str) -> Result<(), PartitionedDiagnosticsBaselineError> {
    if value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(PartitionedDiagnosticsBaselineError::InvalidHash(value.to_owned()))
    }
}

fn validate_managed_file(path: &str) -> Result<(), PartitionedDiagnosticsBaselineError> {
    if !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
    {
        Ok(())
    } else {
        Err(PartitionedDiagnosticsBaselineError::InvalidPath(path.to_owned()))
    }
}

fn validate_source_path(
    path: &str,
    allow_empty: bool,
) -> Result<(), PartitionedDiagnosticsBaselineError> {
    if (allow_empty && path.is_empty())
        || (!path.is_empty()
            && !path.starts_with('/')
            && !path.contains('\\')
            && path
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != ".."))
    {
        Ok(())
    } else {
        Err(PartitionedDiagnosticsBaselineError::InvalidPath(path.to_owned()))
    }
}

use crate::diagnostics_baseline::is_protected_diagnostic as protected;

#[derive(Debug, thiserror::Error)]
pub enum PartitionedDiagnosticsBaselineError {
    #[error("invalid partitioned diagnostics baseline JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Legacy(#[from] DiagnosticsBaselineError),
    #[error("diagnostics baseline I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported manifest schema {0}")]
    UnsupportedManifestSchema(u32),
    #[error("unsupported partition schema {0}")]
    UnsupportedPartitionSchema(u32),
    #[error("project scope does not match the manifest")]
    ScopeMismatch,
    #[error("partition identity does not match: {0}")]
    PartitionIdentityMismatch(String),
    #[error("legacy scope marker must be true and cannot accompany an explicit scope")]
    InvalidLegacyScopeMarker,
    #[error("missing diagnostics baseline partitions: {ids:?}")]
    MissingPartitions { ids: Vec<String>, orphan_ids: Vec<String> },
    #[error("orphan diagnostics baseline partitions: {0:?}")]
    OrphanPartitions(Vec<String>),
    #[error("invalid partition id: {0}")]
    InvalidPartitionId(String),
    #[error("invalid managed path: {0}")]
    InvalidPath(String),
    #[error("invalid BLAKE3 value: {0}")]
    InvalidHash(String),
    #[error("manifest generation does not match its partition hashes")]
    GenerationMismatch,
    #[error("partition object hash mismatch: {0}")]
    ObjectHashMismatch(String),
    #[error("duplicate partition: {0}")]
    DuplicatePartition(String),
    #[error("duplicate diagnostic: {0}")]
    Duplicate(String),
    #[error("diagnostic fingerprint does not match fields: {0}")]
    FingerprintMismatch(String),
    #[error(
        "legacy diagnostics baseline is not in canonical order at {0}; \
         run `diagnostics baseline update` on it before migrating"
    )]
    UnsortedLegacyBaseline(String),
    #[error("protected diagnostic cannot enter a baseline: {0}")]
    ProtectedDiagnostic(String),
    #[error("diagnostic path has no unique owner: {0}")]
    UnownedDiagnostic(String),
    #[error("diagnostics baseline changed while it was being loaded")]
    ConcurrentUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionedDiagnosticsBaselineErrorInfo<'a> {
    pub partition_id: Option<&'a str>,
    pub code: &'static str,
}

impl PartitionedDiagnosticsBaselineError {
    pub fn info(&self) -> PartitionedDiagnosticsBaselineErrorInfo<'_> {
        match self {
            Self::MissingPartitions { ids, orphan_ids: _ } => {
                PartitionedDiagnosticsBaselineErrorInfo {
                    partition_id: ids.first().map(String::as_str),
                    code: "missing_partition",
                }
            }
            Self::OrphanPartitions(ids) => PartitionedDiagnosticsBaselineErrorInfo {
                partition_id: ids.first().map(String::as_str),
                code: "orphan_partition",
            },
            Self::PartitionIdentityMismatch(id) => PartitionedDiagnosticsBaselineErrorInfo {
                partition_id: Some(id),
                code: "partition_identity_mismatch",
            },
            Self::ObjectHashMismatch(id) => PartitionedDiagnosticsBaselineErrorInfo {
                partition_id: Some(id),
                code: "object_hash_mismatch",
            },
            Self::ScopeMismatch => PartitionedDiagnosticsBaselineErrorInfo {
                partition_id: None,
                code: "scope_mismatch",
            },
            _ => {
                PartitionedDiagnosticsBaselineErrorInfo { partition_id: None, code: "invalid_set" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_model::{
        DiagnosticsBaselinePartition, DiagnosticsBaselinePartitionIdentity,
        DiagnosticsBaselineRootOwner, DiagnosticsBaselineSelection,
    };
    use std::io::Write;
    use tempfile::tempdir;

    fn entry(path: &str, code: &str) -> DiagnosticsBaselineEntry {
        let snippet = "Message(1);".to_owned();
        DiagnosticsBaselineEntry {
            fingerprint: diagnostic_fingerprint(path, code, &snippet, 0),
            path: path.to_owned(),
            code: code.to_owned(),
            snippet,
            occurrence: 0,
            message: "message".to_owned(),
            severity: "Warning".to_owned(),
            range: DiagnosticsBaselineRange {
                start_line: 1,
                start_column: 0,
                end_line: 1,
                end_column: 1,
            },
        }
    }

    fn candidate(
        partition_id: &str,
        path: &str,
        code: &str,
    ) -> PartitionedBaselineDiagnosticCandidate<()> {
        PartitionedBaselineDiagnosticCandidate {
            partition_id: partition_id.to_owned(),
            candidate: BaselineDiagnosticCandidate {
                diagnostic: (),
                path: path.to_owned(),
                code: code.to_owned(),
                snippet: Some("Message(1);".to_owned()),
                message: "message".to_owned(),
                severity: "Warning".to_owned(),
                range: DiagnosticsBaselineRange {
                    start_line: 1,
                    start_column: 0,
                    end_line: 1,
                    end_column: 1,
                },
            },
        }
    }

    fn plan() -> DiagnosticsBaselinePartitionPlan {
        let main = DiagnosticsBaselinePartitionIdentity::Main { path: "src/cf".to_owned() };
        let extension = DiagnosticsBaselinePartitionIdentity::Extension {
            name: "Ext".to_owned(),
            path: "src/cfe/Ext".to_owned(),
            scope: Some(bsl_metadata::ExtensionScope::InfoBase),
            legacy_scope: None,
            dependency_scopes: vec![],
            depends_on: vec![],
        };
        DiagnosticsBaselinePartitionPlan {
            project_scope: project_model::DiagnosticsBaselineProjectScope {
                source_root: Some("src/cf".to_owned()),
                extensions: vec![project_model::DiagnosticsBaselineProjectExtension {
                    name: "Ext".to_owned(),
                    path: "src/cfe/Ext".to_owned(),
                    scope: Some(bsl_metadata::ExtensionScope::InfoBase),
                    legacy_scope: None,
                    dependency_scopes: vec![],
                    depends_on: vec![],
                }],
                externals: vec![],
            },
            project_scope_fingerprint: "a".repeat(64),
            selection_fingerprint: "b".repeat(64),
            partitions: vec![
                DiagnosticsBaselinePartition {
                    id: "main".to_owned(),
                    key: blake3::hash(b"main").to_hex().to_string(),
                    identity: main,
                },
                DiagnosticsBaselinePartition {
                    id: "extension:Ext".to_owned(),
                    key: blake3::hash(b"extension:Ext").to_hex().to_string(),
                    identity: extension,
                },
            ],
            enabled_partition_ids: vec!["main".to_owned(), "extension:Ext".to_owned()],
            selection: DiagnosticsBaselineSelection::All,
            roots: vec![
                DiagnosticsBaselineRootOwner {
                    root: "src/cfe/Ext".to_owned(),
                    partition_id: "extension:Ext".to_owned(),
                },
                DiagnosticsBaselineRootOwner {
                    root: "src/cf".to_owned(),
                    partition_id: "main".to_owned(),
                },
            ],
        }
    }

    fn selective_plan(enabled: &[&str]) -> DiagnosticsBaselinePartitionPlan {
        let mut plan = plan();
        plan.enabled_partition_ids = enabled.iter().map(|id| (*id).to_owned()).collect();
        plan.selection = DiagnosticsBaselineSelection::Selective;
        plan.selection_fingerprint = "c".repeat(64);
        plan
    }

    fn write_set(
        directory: &ManagedBaselineDirectory,
        plan: &DiagnosticsBaselinePartitionPlan,
    ) -> DiagnosticsBaselineManifest {
        let mut files = Vec::new();
        for partition in &plan.partitions {
            let path = if partition.id == "main" {
                "src/cf/CommonModules/A/Ext/Module.bsl"
            } else {
                "src/cfe/Ext/CommonModules/B/Ext/Module.bsl"
            };
            let bytes = diagnostics_partition_json(
                partition.identity.clone(),
                vec![entry(
                    path,
                    if partition.id == "main" { "LineLength" } else { "IfElseDuplicatedCodeBlock" },
                )],
            )
            .unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let file = partition_object_path(&partition.id, &partition.key, &hash).unwrap();
            directory.create_file_new(&file).unwrap().write_all(&bytes).unwrap();
            files.push(DiagnosticsBaselineManifestEntry {
                partition_id: partition.id.clone(),
                file,
                blake3: hash,
            });
        }
        let manifest = diagnostics_manifest(plan.project_scope_fingerprint.clone(), files);
        directory
            .create_file_new("manifest.json")
            .unwrap()
            .write_all(&diagnostics_manifest_json(&manifest).unwrap())
            .unwrap();
        manifest
    }

    /// The counterexample in the partitioned form: a, b, c identical findings of one main
    /// file, the main partition holding occurrences 0 and 1. Under the `Baseline` policy c
    /// is new with occurrence 2 through any permutation and projection; under
    /// `Unsuppressed` the policy shows b and c, numbered the same way.
    #[test]
    fn finality_projection_keeps_occurrence() {
        let path = "src/cf/CommonModules/A/Ext/Module.bsl";
        let snippet = "А = А;";
        let recorded = |occurrence: u32, line: u32| DiagnosticsBaselineEntry {
            fingerprint: diagnostic_fingerprint(path, "SelfAssign", snippet, occurrence),
            path: path.to_owned(),
            code: "SelfAssign".to_owned(),
            snippet: snippet.to_owned(),
            occurrence,
            message: "message".to_owned(),
            severity: "Warning".to_owned(),
            range: DiagnosticsBaselineRange {
                start_line: line,
                start_column: 0,
                end_line: line,
                end_column: 1,
            },
        };
        let finding = |line: u32, name: &'static str| PartitionedBaselineDiagnosticCandidate {
            partition_id: "main".to_owned(),
            candidate: BaselineDiagnosticCandidate {
                diagnostic: name,
                path: path.to_owned(),
                code: "SelfAssign".to_owned(),
                snippet: Some(snippet.to_owned()),
                message: "message".to_owned(),
                severity: "Warning".to_owned(),
                range: DiagnosticsBaselineRange {
                    start_line: line,
                    start_column: 0,
                    end_line: line,
                    end_column: 1,
                },
            },
        };
        let full = || vec![finding(1, "a"), finding(2, "b"), finding(3, "c")];

        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = plan();
        let mut files = Vec::new();
        for partition in &plan.partitions {
            let entries =
                if partition.id == "main" { vec![recorded(0, 1), recorded(1, 2)] } else { vec![] };
            let bytes = diagnostics_partition_json(partition.identity.clone(), entries).unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let file = partition_object_path(&partition.id, &partition.key, &hash).unwrap();
            directory.create_file_new(&file).unwrap().write_all(&bytes).unwrap();
            files.push(DiagnosticsBaselineManifestEntry {
                partition_id: partition.id.clone(),
                file,
                blake3: hash,
            });
        }
        let manifest = diagnostics_manifest(plan.project_scope_fingerprint.clone(), files);
        directory
            .create_file_new("manifest.json")
            .unwrap()
            .write_all(&diagnostics_manifest_json(&manifest).unwrap())
            .unwrap();
        let snapshot = load_diagnostics_baseline_set(&directory, &plan).unwrap();
        let unproven = BTreeMap::new();
        let classify =
            |plan: &DiagnosticsBaselinePartitionPlan,
             current: Vec<PartitionedBaselineDiagnosticCandidate<&'static str>>| {
                classify_partitioned_diagnostics(
                    &snapshot,
                    plan,
                    "b".to_owned(),
                    current,
                    &unproven,
                )
                .unwrap()
            };

        let classified = classify(&plan, full());
        let known: Vec<_> =
            classified.known.iter().map(|item| (item.diagnostic, item.entry.occurrence)).collect();
        assert_eq!(known, vec![("a", 0), ("b", 1)]);
        assert_eq!(classified.new.len(), 1);
        let c = &classified.new[0];
        assert_eq!((c.diagnostic, c.entry.occurrence), ("c", 2));
        assert_eq!(c.entry.fingerprint, diagnostic_fingerprint(path, "SelfAssign", snippet, 2));
        assert_eq!(c.entry.range.start_line, 3);

        let mut reversed = full();
        reversed.reverse();
        let permuted = classify(&plan, reversed);
        assert_eq!(permuted.new, classified.new, "the input order must not matter");
        assert_eq!(permuted.known, classified.known);

        let visible: Vec<_> =
            classified.new.iter().filter(|item| item.entry.range.start_line >= 2).collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].entry, c.entry, "a projection must not touch the identity");

        let scoped_first: Vec<_> =
            full().into_iter().filter(|item| item.candidate.range.start_line >= 2).collect();
        let renumbered = classify(&plan, scoped_first);
        assert!(
            renumbered.new.iter().all(|item| item.diagnostic != "c"),
            "control: classifying the scoped set must lose c"
        );

        let unsuppressed_plan = selective_plan(&["extension:Ext"]);
        let shown = classify(&unsuppressed_plan, full());
        assert!(shown.known.is_empty() && shown.new.is_empty());
        let visible: Vec<_> = shown
            .unsuppressed
            .iter()
            .filter(|item| item.entry.range.start_line >= 2)
            .map(|item| (item.diagnostic, item.entry.occurrence))
            .collect();
        assert_eq!(visible, vec![("b", 1), ("c", 2)], "Unsuppressed shows b and c");
    }

    fn replace_extension_object_identity(
        root: &std::path::Path,
        directory: &ManagedBaselineDirectory,
        manifest: &DiagnosticsBaselineManifest,
        plan: &DiagnosticsBaselinePartitionPlan,
        identity: DiagnosticsBaselinePartitionIdentity,
    ) -> (String, Vec<u8>) {
        let partition =
            plan.partitions.iter().find(|partition| partition.id == "extension:Ext").unwrap();
        let bytes = serde_json::to_vec(&DiagnosticsBaselinePartitionFile {
            schema_version: DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION,
            partition: identity,
            diagnostics: vec![entry(
                "src/cfe/Ext/CommonModules/B/Ext/Module.bsl",
                "IfElseDuplicatedCodeBlock",
            )],
        })
        .unwrap();
        let mut bytes = bytes;
        bytes.push(b'\n');
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let file = partition_object_path(&partition.id, &partition.key, &hash).unwrap();
        directory.create_file_new(&file).unwrap().write_all(&bytes).unwrap();
        let replaced_file = file.clone();
        let mut files = manifest.partitions.clone();
        let extension = files.iter_mut().find(|file| file.partition_id == "extension:Ext").unwrap();
        extension.file = file;
        extension.blake3 = hash;
        let updated = diagnostics_manifest(plan.project_scope_fingerprint.clone(), files);
        std::fs::write(
            root.join("baselines/manifest.json"),
            diagnostics_manifest_json(&updated).unwrap(),
        )
        .unwrap();
        (replaced_file, bytes)
    }

    #[test]
    fn partitioned_baseline_schema_v2_generation_is_byte_deterministic() {
        let plan = plan();
        let mut files = Vec::new();
        for partition in &plan.partitions {
            let path = if partition.id == "main" {
                "src/cf/CommonModules/A/Ext/Module.bsl"
            } else {
                "src/cfe/Ext/CommonModules/B/Ext/Module.bsl"
            };
            let bytes = diagnostics_partition_json(
                partition.identity.clone(),
                vec![entry(path, "LineLength")],
            )
            .unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            files.push(DiagnosticsBaselineManifestEntry {
                partition_id: partition.id.clone(),
                file: partition_object_path(&partition.id, &partition.key, &hash).unwrap(),
                blake3: hash,
            });
        }
        let first = diagnostics_manifest(plan.project_scope_fingerprint.clone(), files.clone());
        let second = diagnostics_manifest(plan.project_scope_fingerprint, files);
        assert_eq!(
            diagnostics_manifest_json(&first).unwrap(),
            diagnostics_manifest_json(&second).unwrap()
        );
        assert_eq!(first.generation, second.generation);
    }

    #[test]
    fn contradictory_legacy_scope_markers_are_rejected_at_baseline_boundary() {
        let identity = |dependency_scopes| DiagnosticsBaselinePartitionIdentity::Extension {
            name: "Ext".to_owned(),
            path: "src/cfe/Ext".to_owned(),
            scope: Some(bsl_metadata::ExtensionScope::InfoBase),
            legacy_scope: None,
            dependency_scopes,
            depends_on: vec![],
        };
        let contradictory_root = DiagnosticsBaselinePartitionIdentity::Extension {
            name: "Ext".to_owned(),
            path: "src/cfe/Ext".to_owned(),
            scope: Some(bsl_metadata::ExtensionScope::InfoBase),
            legacy_scope: Some(true),
            dependency_scopes: vec![],
            depends_on: vec![],
        };
        assert!(matches!(
            diagnostics_partition_json(contradictory_root, vec![]),
            Err(PartitionedDiagnosticsBaselineError::InvalidLegacyScopeMarker)
        ));
        let contradictory_dependency =
            identity(vec![project_model::DiagnosticsBaselineDependencyScope {
                name: "Base".to_owned(),
                scope: Some(bsl_metadata::ExtensionScope::DataSeparation),
                legacy_scope: Some(true),
            }]);
        assert!(matches!(
            diagnostics_partition_json(contradictory_dependency, vec![]),
            Err(PartitionedDiagnosticsBaselineError::InvalidLegacyScopeMarker)
        ));
    }

    #[test]
    fn fresh_legacy_root_and_dependency_baselines_load_but_old_or_contradictory_markers_fail_read_only(
    ) {
        for legacy_dependency in [false, true] {
            let root = tempdir().unwrap();
            let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
            let mut plan = plan();
            let extension =
                plan.partitions.iter_mut().find(|part| part.id == "extension:Ext").unwrap();
            let project_extension = &mut plan.project_scope.extensions[0];
            match &mut extension.identity {
                DiagnosticsBaselinePartitionIdentity::Extension {
                    scope,
                    legacy_scope,
                    dependency_scopes,
                    ..
                } => {
                    if legacy_dependency {
                        *dependency_scopes =
                            vec![project_model::DiagnosticsBaselineDependencyScope {
                                name: "Base".to_owned(),
                                scope: None,
                                legacy_scope: Some(true),
                            }];
                        project_extension.dependency_scopes = dependency_scopes.clone();
                    } else {
                        *scope = None;
                        *legacy_scope = Some(true);
                        project_extension.scope = None;
                        project_extension.legacy_scope = Some(true);
                    }
                }
                _ => unreachable!(),
            }
            let expected_identity = extension.identity.clone();
            let manifest = write_set(&directory, &plan);
            let (accepted, _) =
                load_diagnostics_baseline_set_reusing(&directory, &plan, None, &BTreeSet::new())
                    .unwrap();
            assert_eq!(accepted.partitions["extension:Ext"].identity, expected_identity);

            let markerless = match expected_identity.clone() {
                DiagnosticsBaselinePartitionIdentity::Extension {
                    scope,
                    dependency_scopes,
                    ..
                } => DiagnosticsBaselinePartitionIdentity::Extension {
                    name: "Ext".to_owned(),
                    path: "src/cfe/Ext".to_owned(),
                    scope,
                    legacy_scope: None,
                    dependency_scopes: dependency_scopes
                        .into_iter()
                        .map(|dependency| project_model::DiagnosticsBaselineDependencyScope {
                            legacy_scope: None,
                            ..dependency
                        })
                        .collect(),
                    depends_on: vec![],
                },
                _ => unreachable!(),
            };
            let (markerless_file, markerless_bytes) = replace_extension_object_identity(
                root.path(),
                &directory,
                &manifest,
                &plan,
                markerless,
            );
            let manifest_before =
                std::fs::read(root.path().join("baselines/manifest.json")).unwrap();
            let error =
                load_diagnostics_baseline_set_reusing(&directory, &plan, None, &BTreeSet::new())
                    .unwrap_err();
            assert!(matches!(
                error,
                PartitionedDiagnosticsBaselineError::PartitionIdentityMismatch(_)
            ));
            assert_eq!(
                std::fs::read(root.path().join("baselines/manifest.json")).unwrap(),
                manifest_before
            );
            assert_eq!(
                std::fs::read(root.path().join("baselines").join(markerless_file)).unwrap(),
                markerless_bytes
            );

            let contradictory = match expected_identity {
                DiagnosticsBaselinePartitionIdentity::Extension {
                    scope,
                    mut dependency_scopes,
                    ..
                } => {
                    if legacy_dependency {
                        dependency_scopes[0].scope = Some(bsl_metadata::ExtensionScope::InfoBase);
                        dependency_scopes[0].legacy_scope = Some(true);
                    }
                    DiagnosticsBaselinePartitionIdentity::Extension {
                        name: "Ext".to_owned(),
                        path: "src/cfe/Ext".to_owned(),
                        scope: if legacy_dependency {
                            scope
                        } else {
                            Some(bsl_metadata::ExtensionScope::InfoBase)
                        },
                        legacy_scope: if legacy_dependency { None } else { Some(true) },
                        dependency_scopes,
                        depends_on: vec![],
                    }
                }
                _ => unreachable!(),
            };
            let latest: DiagnosticsBaselineManifest = serde_json::from_slice(
                &std::fs::read(root.path().join("baselines/manifest.json")).unwrap(),
            )
            .unwrap();
            replace_extension_object_identity(
                root.path(),
                &directory,
                &latest,
                &plan,
                contradictory,
            );
            let error =
                load_diagnostics_baseline_set_reusing(&directory, &plan, None, &BTreeSet::new())
                    .unwrap_err();
            assert!(
                matches!(&error, PartitionedDiagnosticsBaselineError::InvalidLegacyScopeMarker),
                "contradictory root/dependency scope should be rejected specifically: {error:?}"
            );
        }
    }

    #[test]
    fn partitioned_baseline_schema_v2_rejects_bad_schema_identity_and_paths() {
        let identity = DiagnosticsBaselinePartitionIdentity::Main { path: "src/cf".to_owned() };
        let bytes = diagnostics_partition_json(identity, vec![entry("src/cf/a.bsl", "LineLength")])
            .unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["schema_version"] = 3.into();
        let parsed: DiagnosticsBaselinePartitionFile = serde_json::from_value(value).unwrap();
        assert_ne!(parsed.schema_version, DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION);
        assert!(validate_managed_file("../escape.json").is_err());
        assert!(validate_managed_file("objects\\escape.json").is_err());
    }

    #[test]
    fn partitioned_baseline_fingerprint_contract_matches_v1() {
        let item = entry("src/cf/a.bsl", "LineLength");
        let bytes = diagnostics_partition_json(
            DiagnosticsBaselinePartitionIdentity::Main { path: "src/cf".to_owned() },
            vec![item.clone()],
        )
        .unwrap();
        let parsed: DiagnosticsBaselinePartitionFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.diagnostics[0].fingerprint, item.fingerprint);
    }

    #[test]
    fn partitioned_baseline_set_loader_streams_and_validates_all_partitions() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = plan();
        let manifest = write_set(&directory, &plan);
        let snapshot = load_diagnostics_baseline_set(&directory, &plan).unwrap();
        assert_eq!(snapshot.manifest, manifest);
        assert_eq!(
            snapshot.manifest_hash,
            *blake3::hash(&std::fs::read(root.path().join("baselines/manifest.json")).unwrap())
                .as_bytes()
        );
        assert_eq!(snapshot.partitions.len(), 2);
        assert_eq!(snapshot.partitions["main"].entries_len(), 1);
        assert_eq!(snapshot.partitions["extension:Ext"].entries_len(), 1);
    }

    #[test]
    fn partitioned_baseline_reports_topology_before_identity_and_scope() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = plan();
        let mut manifest = write_set(&directory, &plan);
        let original_manifest = manifest.clone();
        manifest.partitions[1].partition_id = "extension:Orphan".to_owned();
        manifest =
            diagnostics_manifest(manifest.project_scope_fingerprint.clone(), manifest.partitions);
        std::fs::write(
            root.path().join("baselines/manifest.json"),
            diagnostics_manifest_json(&manifest).unwrap(),
        )
        .unwrap();
        let error = load_diagnostics_baseline_set(&directory, &plan).unwrap_err();
        let info = error.info();
        assert_eq!(info.code, "missing_partition");
        assert_eq!(info.partition_id, Some("extension:Ext"));

        let mut identity_plan = plan.clone();
        identity_plan.partitions[0].identity =
            DiagnosticsBaselinePartitionIdentity::Main { path: "src/other".to_owned() };
        identity_plan.project_scope_fingerprint = "b".repeat(64);
        std::fs::write(
            root.path().join("baselines/manifest.json"),
            diagnostics_manifest_json(&original_manifest).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load_diagnostics_baseline_set(&directory, &identity_plan),
            Err(PartitionedDiagnosticsBaselineError::PartitionIdentityMismatch(id)) if id == "main"
        ));
    }

    #[test]
    fn partitioned_baseline_set_loader_reuses_unchanged_objects() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = plan();
        let first_manifest = write_set(&directory, &plan);
        let first = load_diagnostics_baseline_set(&directory, &plan).unwrap();
        let old_extension = first.partitions["extension:Ext"].clone();

        let main = &plan.partitions[0];
        let bytes = diagnostics_partition_json(
            main.identity.clone(),
            vec![entry("src/cf/Changed.bsl", "LineLength")],
        )
        .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let file = partition_object_path(&main.id, &main.key, &hash).unwrap();
        directory.create_file_new(&file).unwrap().write_all(&bytes).unwrap();
        let mut entries = first_manifest.partitions;
        let current = entries.iter_mut().find(|entry| entry.partition_id == "main").unwrap();
        current.file = file;
        current.blake3 = hash;
        let manifest = diagnostics_manifest(plan.project_scope_fingerprint.clone(), entries);
        directory
            .create_file_new("manifest.next.json")
            .unwrap()
            .write_all(&diagnostics_manifest_json(&manifest).unwrap())
            .unwrap();
        directory.replace_file("manifest.next.json", "manifest.json").unwrap();

        let (second, stats) = load_diagnostics_baseline_set_reusing(
            &directory,
            &plan,
            Some(&first),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(stats.partitions_parsed, 1);
        assert_eq!(stats.fingerprints_validated, 1);
        assert!(Arc::ptr_eq(&old_extension, &second.partitions["extension:Ext"]));
    }

    #[test]
    fn partitioned_baseline_set_loader_fails_closed_on_corruption() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = plan();
        let manifest = write_set(&directory, &plan);
        let main = manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
        let path = root.path().join("baselines").join(&main.file);
        std::fs::write(path, b"{}\n").unwrap();
        assert!(matches!(
            load_diagnostics_baseline_set(&directory, &plan),
            Err(PartitionedDiagnosticsBaselineError::Json(_))
                | Err(PartitionedDiagnosticsBaselineError::ObjectHashMismatch(_))
        ));
    }

    #[test]
    fn selective_manifest_keeps_existing_schemas_and_deterministic_effective_epoch() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = selective_plan(&["main"]);
        let mut manifest = write_set(&directory, &plan);
        let dormant =
            manifest.partitions.iter().find(|entry| entry.partition_id == "extension:Ext").unwrap();
        std::fs::remove_file(root.path().join("baselines").join(&dormant.file)).unwrap();

        let (snapshot, stats) =
            load_diagnostics_baseline_set_reusing(&directory, &plan, None, &BTreeSet::new())
                .unwrap();
        assert_eq!(snapshot.manifest.schema_version, DIAGNOSTICS_BASELINE_MANIFEST_SCHEMA_VERSION);
        assert_eq!(snapshot.partitions.len(), 1);
        assert_eq!(snapshot.partitions["main"].identity, plan.partitions[0].identity);
        assert_eq!(stats.partitions_parsed, 1);
        assert_eq!(
            stats.objects_read,
            BTreeSet::from([snapshot.partitions["main"].file.to_string()])
        );
        assert_eq!(plan.selection_fingerprint, "c".repeat(64));

        manifest.partitions.retain(|entry| entry.partition_id == "main");
        manifest =
            diagnostics_manifest(manifest.project_scope_fingerprint.clone(), manifest.partitions);
        std::fs::write(
            root.path().join("baselines/manifest.json"),
            diagnostics_manifest_json(&manifest).unwrap(),
        )
        .unwrap();
        assert!(load_diagnostics_baseline_set(&directory, &plan).is_ok());
    }

    #[test]
    fn selective_loader_fails_closed_for_every_enabled_object_error() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = selective_plan(&["main"]);
        let manifest = write_set(&directory, &plan);
        let main = manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
        std::fs::remove_file(root.path().join("baselines").join(&main.file)).unwrap();
        assert!(matches!(
            load_diagnostics_baseline_set(&directory, &plan),
            Err(PartitionedDiagnosticsBaselineError::MissingPartitions { ids, .. }) if ids == ["main"]
        ));

        let corrupt_root = tempdir().unwrap();
        let corrupt_directory =
            ManagedBaselineDirectory::open(corrupt_root.path(), "baselines", true).unwrap();
        let corrupt_manifest = write_set(&corrupt_directory, &plan);
        let main =
            corrupt_manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
        std::fs::write(corrupt_root.path().join("baselines").join(&main.file), b"{}\n").unwrap();
        assert!(load_diagnostics_baseline_set(&corrupt_directory, &plan).is_err());
    }

    #[test]
    fn selective_loader_rejects_unsafe_enabled_paths_and_links() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = selective_plan(&["main"]);
        let mut manifest = write_set(&directory, &plan);
        let main =
            manifest.partitions.iter_mut().find(|entry| entry.partition_id == "main").unwrap();
        main.file = "../escape.json".to_owned();
        manifest =
            diagnostics_manifest(manifest.project_scope_fingerprint.clone(), manifest.partitions);
        std::fs::write(
            root.path().join("baselines/manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load_diagnostics_baseline_set(&directory, &plan),
            Err(PartitionedDiagnosticsBaselineError::InvalidPath(path)) if path == "../escape.json"
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let enabled_root = tempdir().unwrap();
            let enabled_directory =
                ManagedBaselineDirectory::open(enabled_root.path(), "baselines", true).unwrap();
            let manifest = write_set(&enabled_directory, &plan);
            let main =
                manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
            let main_path = enabled_root.path().join("baselines").join(&main.file);
            std::fs::remove_file(&main_path).unwrap();
            symlink(enabled_root.path().join("outside.json"), &main_path).unwrap();
            assert!(load_diagnostics_baseline_set(&enabled_directory, &plan).is_err());

            let dormant_root = tempdir().unwrap();
            let dormant_directory =
                ManagedBaselineDirectory::open(dormant_root.path(), "baselines", true).unwrap();
            let manifest = write_set(&dormant_directory, &plan);
            let dormant = manifest
                .partitions
                .iter()
                .find(|entry| entry.partition_id == "extension:Ext")
                .unwrap();
            let dormant_path = dormant_root.path().join("baselines").join(&dormant.file);
            std::fs::remove_file(&dormant_path).unwrap();
            symlink(dormant_root.path().join("outside.json"), dormant_path).unwrap();
            assert!(load_diagnostics_baseline_set(&dormant_directory, &plan).is_ok());
        }
    }

    #[test]
    fn selective_loader_validates_enabled_identity_instead_of_global_scope() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let original = plan();
        write_set(&directory, &original);
        let mut changed = selective_plan(&["main"]);
        changed.project_scope_fingerprint = "d".repeat(64);
        changed.partitions[1].identity = DiagnosticsBaselinePartitionIdentity::Extension {
            name: "Ext".to_owned(),
            path: "src/cfe/Renamed".to_owned(),
            scope: Some(bsl_metadata::ExtensionScope::InfoBase),
            legacy_scope: None,
            dependency_scopes: vec![],
            depends_on: vec![],
        };
        let snapshot = load_diagnostics_baseline_set(&directory, &changed).unwrap();
        assert_eq!(snapshot.partitions.len(), 1);
        assert_eq!(snapshot.partitions["main"].identity, changed.partitions[0].identity);
    }

    #[test]
    fn selective_scope_ignores_changes_outside_enabled_owners() {
        selective_loader_validates_enabled_identity_instead_of_global_scope();
    }

    #[test]
    fn selective_classifier_rejects_incompatible_snapshot_and_plan_without_panic() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let full = plan();
        write_set(&directory, &full);
        let snapshot =
            load_diagnostics_baseline_set(&directory, &selective_plan(&["main"])).unwrap();
        let coverage = full
            .partitions
            .iter()
            .map(|partition| (partition.id.clone(), DiagnosticsBaselineCoverage::Full))
            .collect();
        let error = classify_partitioned_diagnostics::<()>(
            &snapshot,
            &full,
            "baselines".to_owned(),
            vec![],
            &coverage,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PartitionedDiagnosticsClassificationError::MissingEnabledPartition(ref id)
                if id == "extension:Ext"
        ));
    }

    #[test]
    fn selective_loader_defers_dormant_content_validation_until_reenable() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let full = plan();
        let mut manifest = write_set(&directory, &full);
        let extension =
            full.partitions.iter().find(|partition| partition.id == "extension:Ext").unwrap();
        let duplicate = entry("src/cfe/Ext/CommonModules/B/Ext/Module.bsl", "UsingGoto");
        let bytes = pretty_json(&DiagnosticsBaselinePartitionFile {
            schema_version: DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION,
            partition: extension.identity.clone(),
            diagnostics: vec![duplicate.clone(), duplicate],
        })
        .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let file = partition_object_path(&extension.id, &extension.key, &hash).unwrap();
        directory.create_file_new(&file).unwrap().write_all(&bytes).unwrap();
        let dormant = manifest
            .partitions
            .iter_mut()
            .find(|entry| entry.partition_id == extension.id)
            .unwrap();
        dormant.file = file;
        dormant.blake3 = hash;
        manifest =
            diagnostics_manifest(manifest.project_scope_fingerprint.clone(), manifest.partitions);
        std::fs::write(
            root.path().join("baselines/manifest.json"),
            diagnostics_manifest_json(&manifest).unwrap(),
        )
        .unwrap();

        let selective = selective_plan(&["main"]);
        assert!(load_diagnostics_baseline_set(&directory, &selective).is_ok());
        assert!(load_diagnostics_baseline_set(&directory, &full).is_err());

        let mut duplicate_manifest = manifest;
        duplicate_manifest.partitions.push(
            duplicate_manifest
                .partitions
                .iter()
                .find(|entry| entry.partition_id == "extension:Ext")
                .unwrap()
                .clone(),
        );
        duplicate_manifest = diagnostics_manifest(
            duplicate_manifest.project_scope_fingerprint.clone(),
            duplicate_manifest.partitions,
        );
        assert!(matches!(
            diagnostics_manifest_json(&duplicate_manifest),
            Err(PartitionedDiagnosticsBaselineError::DuplicatePartition(id))
                if id == "extension:Ext"
        ));
    }

    #[test]
    fn selective_baseline_manifest() {
        selective_manifest_keeps_existing_schemas_and_deterministic_effective_epoch();
        selective_loader_fails_closed_for_every_enabled_object_error();
        selective_loader_validates_enabled_identity_instead_of_global_scope();
        selective_loader_defers_dormant_content_validation_until_reenable();
    }

    /// A migrated object must be byte-identical to what `diagnostics_partition_json`
    /// regenerates, or `create --partition` can never repair the set. Streaming cannot
    /// sort, so an out-of-order legacy file is refused rather than silently written in
    /// an order no regeneration reproduces.
    #[test]
    fn migration_refuses_a_legacy_baseline_that_is_not_canonical() {
        let plan = selective_plan(&["main"]);
        let entry = |path: &str| DiagnosticsBaselineEntry {
            fingerprint: diagnostic_fingerprint(path, "LineLength", "Message(1);", 0),
            path: path.to_owned(),
            code: "LineLength".to_owned(),
            snippet: "Message(1);".to_owned(),
            occurrence: 0,
            message: "m".to_owned(),
            severity: "Warning".to_owned(),
            range: DiagnosticsBaselineRange {
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 1,
            },
        };
        let legacy = |entries: Vec<DiagnosticsBaselineEntry>| {
            serde_json::to_vec(&DiagnosticsBaseline {
                schema_version: crate::diagnostics_baseline::DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope: crate::diagnostics_baseline::DiagnosticsBaselineScope {
                    source_root: Some("src/cf".to_owned()),
                    extensions: vec![crate::diagnostics_baseline::DiagnosticsBaselineExtension {
                        name: "Ext".to_owned(),
                        path: "src/cfe/Ext".to_owned(),
                        scope: Some(bsl_metadata::ExtensionScope::InfoBase),
                        legacy_scope: None,
                        dependency_scopes: vec![],
                        depends_on: vec![],
                    }],
                },
                diagnostics: entries,
            })
            .unwrap()
        };

        let ordered = legacy(vec![entry("src/cf/a.bsl"), entry("src/cf/b.bsl")]);
        let mut written = 0;
        migrate_v1_reader(&ordered[..], &plan, |_, _| {
            written += 1;
            Ok(())
        })
        .expect("a canonical file migrates");
        assert_eq!(written, 2, "positive control: the fixture migrates when ordered");

        let reversed = legacy(vec![entry("src/cf/b.bsl"), entry("src/cf/a.bsl")]);
        let error = migrate_v1_reader(&reversed[..], &plan, |_, _| Ok(())).unwrap_err();
        assert!(
            error.to_string().contains("canonical order"),
            "an out-of-order file must be named as such: {error}"
        );
    }

    /// `serde_json` asks its reader for one byte at a time, so an object read without a
    /// buffer costs a syscall per byte — the very path the partitioned mode exists for.
    /// The ratio is the gate: the file is far larger than one buffer, so a byte-at-a-time
    /// loader could not possibly pass it.
    #[test]
    fn partition_objects_are_read_through_a_buffer() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = selective_plan(&["main", "extension:Ext"]);
        write_set(&directory, &plan);
        let (_snapshot, stats) =
            load_diagnostics_baseline_set_reusing(&directory, &plan, None, &BTreeSet::new())
                .unwrap();

        let bytes: usize = stats
            .objects_read
            .iter()
            .map(|file| {
                let mut object = directory.open_file(file).unwrap();
                let mut buffer = Vec::new();
                std::io::Read::read_to_end(&mut object, &mut buffer).unwrap();
                buffer.len()
            })
            .sum();
        assert!(bytes > 0, "the fixture must publish object bytes to read");
        assert!(
            stats.object_reads * 64 < bytes.max(64),
            "objects were read {} times for {bytes} bytes — that is byte-at-a-time reading",
            stats.object_reads
        );
    }

    /// Skipping `resolved` must change nothing about which diagnostics are active:
    /// the fixture is one that DOES have a resolved entry under `Compute`, so a
    /// silent behaviour change would show up as a differing active set here.
    #[test]
    fn skipping_resolved_keeps_the_active_classification_intact() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = selective_plan(&["main"]);
        write_set(&directory, &plan);
        let snapshot = load_diagnostics_baseline_set(&directory, &plan).unwrap();
        let coverage = BTreeMap::from([
            ("main".to_owned(), DiagnosticsBaselineCoverage::Full),
            ("extension:Ext".to_owned(), DiagnosticsBaselineCoverage::Full),
        ]);
        let current = || {
            vec![candidate(
                "extension:Ext",
                "src/cfe/Ext/CommonModules/B/Ext/Module.bsl",
                "LineLength",
            )]
        };

        let computed = classify_partitioned_diagnostics_with(
            &snapshot,
            &plan,
            "baselines".to_owned(),
            current(),
            &coverage,
            ResolvedPolicy::Compute,
        )
        .unwrap();
        assert!(
            computed.summary.resolved.is_some_and(|resolved| resolved > 0),
            "positive control: this fixture must have something to resolve"
        );

        let skipped = classify_partitioned_diagnostics_with(
            &snapshot,
            &plan,
            "baselines".to_owned(),
            current(),
            &coverage,
            ResolvedPolicy::Skip,
        )
        .unwrap();
        assert_eq!(skipped.new.len(), computed.new.len());
        assert_eq!(skipped.known.len(), computed.known.len());
        assert_eq!(skipped.unsuppressed.len(), computed.unsuppressed.len());
        assert_eq!(skipped.summary.new, computed.summary.new);
        assert_eq!(skipped.summary.known, computed.summary.known);
        assert_eq!(
            skipped.summary.resolved, None,
            "a skipped count must be absent, not a zero that reads as \"nothing was resolved\""
        );
        assert_eq!(computed.resolved.len(), computed.resolved.into_iter().count());
        assert_eq!(
            skipped.resolved.len(),
            skipped.resolved.into_iter().count(),
            "len and the iterator must agree — the type promises ExactSizeIterator"
        );
    }

    fn classify_selective(
        current: Vec<PartitionedBaselineDiagnosticCandidate<()>>,
    ) -> ClassifiedPartitionedDiagnostics<()> {
        classify_selective_with_coverage(current, DiagnosticsBaselineCoverage::Full)
    }

    fn classify_selective_with_coverage(
        current: Vec<PartitionedBaselineDiagnosticCandidate<()>>,
        extension_coverage: DiagnosticsBaselineCoverage,
    ) -> ClassifiedPartitionedDiagnostics<()> {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = selective_plan(&["main"]);
        write_set(&directory, &plan);
        let snapshot = load_diagnostics_baseline_set(&directory, &plan).unwrap();
        let coverage = BTreeMap::from([
            ("main".to_owned(), DiagnosticsBaselineCoverage::Full),
            ("extension:Ext".to_owned(), extension_coverage),
        ]);
        classify_partitioned_diagnostics(
            &snapshot,
            &plan,
            "baselines".to_owned(),
            current,
            &coverage,
        )
        .unwrap()
    }

    #[test]
    fn selective_classifier_keeps_unsuppressed_diagnostics_visible() {
        let classified = classify_selective(vec![candidate(
            "extension:Ext",
            "src/cfe/Ext/CommonModules/B/Ext/Module.bsl",
            "UsingGoto",
        )]);
        assert_eq!(classified.unsuppressed.len(), 1);
        assert!(classified.new.is_empty());
        assert!(classified.known.is_empty());
    }

    #[test]
    fn selective_classifier_never_looks_up_another_partition() {
        let classified = classify_selective(vec![candidate(
            "extension:Ext",
            "src/cf/CommonModules/A/Ext/Module.bsl",
            "LineLength",
        )]);
        assert_eq!(classified.unsuppressed.len(), 1);
        assert!(classified.known.is_empty());
    }

    #[test]
    fn selective_classifier_preserves_protected_diagnostics_for_both_policies() {
        let classified = classify_selective(vec![
            candidate("main", "src/cf/CommonModules/A/Ext/Module.bsl", "UnknownSuppressionCode"),
            candidate(
                "extension:Ext",
                "src/cfe/Ext/CommonModules/B/Ext/Module.bsl",
                "SuppressionWithoutCode",
            ),
        ]);
        assert_eq!(classified.new.len(), 2);
        assert!(classified.unsuppressed.is_empty());
        assert!(classified.known.is_empty());
    }

    #[test]
    fn selective_baseline_classification() {
        selective_classifier_keeps_unsuppressed_diagnostics_visible();
        selective_classifier_never_looks_up_another_partition();
        selective_classifier_preserves_protected_diagnostics_for_both_policies();
    }

    #[test]
    fn selective_summary_separates_enabled_counts_from_unsuppressed() {
        let classified = classify_selective(vec![
            candidate("main", "src/cf/CommonModules/A/Ext/Module.bsl", "LineLength"),
            candidate("extension:Ext", "src/cfe/Ext/CommonModules/B/Ext/Module.bsl", "UsingGoto"),
            candidate(
                "extension:Ext",
                "src/cfe/Ext/CommonModules/B/Ext/Module.bsl",
                "CyclomaticComplexity",
            ),
            candidate("extension:Ext", "src/cfe/Ext/CommonModules/B/Ext/Module.bsl", "LineLength"),
        ]);
        let summary = classified.summary;
        assert_eq!(summary.selection, Some(DiagnosticsBaselineSelection::Selective));
        assert_eq!(summary.partitions_enabled, Some(1));
        assert_eq!(summary.partitions_unsuppressed, Some(1));
        assert_eq!(summary.unsuppressed, Some(3));
        assert_eq!((summary.new, summary.known, summary.resolved), (Some(0), Some(1), Some(0)));
        assert!(summary.complete);
        assert_eq!(summary.partitions[0].id, "main");
        assert_eq!(
            summary.partitions[0].policy,
            project_model::DiagnosticsBaselinePartitionPolicy::Baseline
        );
        assert!(summary.partitions[0].path.is_some());
        assert_eq!(summary.partitions[1].id, "extension:Ext");
        assert_eq!(
            summary.partitions[1].policy,
            project_model::DiagnosticsBaselinePartitionPolicy::Unsuppressed
        );
        assert_eq!(summary.partitions[1].unsuppressed, 3);
        assert!(summary.partitions[1].path.is_none());
    }

    #[test]
    fn selective_coverage_does_not_hide_partial_unsuppressed_owner() {
        let classified = classify_selective_with_coverage(
            vec![candidate(
                "extension:Ext",
                "src/cfe/Ext/CommonModules/B/Ext/Module.bsl",
                "UsingGoto",
            )],
            DiagnosticsBaselineCoverage::Partial { completed_files: BTreeSet::new() },
        );
        assert_eq!(classified.summary.state, DiagnosticsBaselineState::Partial);
        assert!(!classified.summary.complete);
        let extension = &classified.summary.partitions[1];
        assert_eq!(extension.state, DiagnosticsBaselineState::Partial);
        assert!(!extension.complete);
        assert_eq!(extension.unsuppressed, 1);
    }

    #[test]
    fn selective_resolved_is_computed_only_for_full_enabled_partitions() {
        let classified = classify_selective(vec![]);
        assert_eq!(classified.resolved.len(), 1);
        assert_eq!(classified.summary.resolved, Some(1));
        assert_eq!(classified.summary.partitions[0].resolved, 1);
        assert_eq!(classified.summary.partitions[1].resolved, 0);
    }

    #[test]
    fn selective_baseline_coverage_and_summary() {
        selective_summary_separates_enabled_counts_from_unsuppressed();
        selective_coverage_does_not_hide_partial_unsuppressed_owner();
        selective_resolved_is_computed_only_for_full_enabled_partitions();
    }

    #[test]
    fn partitioned_baseline_classification_and_summary_routes_once() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let plan = plan();
        write_set(&directory, &plan);
        let snapshot = load_diagnostics_baseline_set(&directory, &plan).unwrap();
        let main_path = "src/cf/CommonModules/A/Ext/Module.bsl";
        let extension_path = "src/cfe/Ext/CommonModules/B/Ext/Module.bsl";
        let current = vec![
            candidate("main", main_path, "LineLength"),
            candidate("main", main_path, "UsingGoto"),
            candidate("main", main_path, "UnknownSuppressionCode"),
        ];
        let coverage = BTreeMap::from([
            ("main".to_owned(), DiagnosticsBaselineCoverage::Full),
            (
                "extension:Ext".to_owned(),
                DiagnosticsBaselineCoverage::Partial {
                    completed_files: BTreeSet::from([extension_path.to_owned()]),
                },
            ),
        ]);
        let classified = classify_partitioned_diagnostics(
            &snapshot,
            &plan,
            "baselines".to_owned(),
            current,
            &coverage,
        )
        .unwrap();
        assert_eq!(
            (classified.new.len(), classified.known.len(), classified.resolved.len()),
            (2, 1, 1)
        );
        assert_eq!(classified.summary.state, DiagnosticsBaselineState::Partial);
        assert!(!classified.summary.complete);
        assert_eq!(classified.summary.partitions.len(), 2);
        assert_eq!(classified.summary.partitions[0].id, "main");
        assert_eq!(classified.summary.partitions[0].known, 1);
        assert_eq!(classified.summary.partitions[0].new, 2);
        assert_eq!(classified.summary.partitions[1].id, "extension:Ext");
        assert_eq!(classified.summary.partitions[1].resolved, 1);
        assert!(!classified.summary.partitions[1].complete);
        assert_eq!(classified.resolved.into_iter().count(), 1);
    }

    #[test]
    fn partitioned_baseline_coverage_promotes_only_complete_owner_file_sets() {
        let plan = plan();
        let main = "src/cf/CommonModules/A/Ext/Module.bsl".to_owned();
        let extension = "src/cfe/Ext/CommonModules/B/Ext/Module.bsl".to_owned();
        let coverage = partitioned_coverage(
            &plan,
            &DiagnosticsBaselineCoverage::Partial {
                completed_files: BTreeSet::from([main.clone()]),
            },
            Some(&BTreeSet::from([main, extension.clone()])),
        )
        .unwrap();
        assert_eq!(coverage["main"], DiagnosticsBaselineCoverage::Full);
        assert_eq!(
            coverage["extension:Ext"],
            DiagnosticsBaselineCoverage::Partial { completed_files: BTreeSet::new() }
        );

        let full = partitioned_coverage(
            &plan,
            &DiagnosticsBaselineCoverage::Partial {
                completed_files: BTreeSet::from([extension.clone()]),
            },
            Some(&BTreeSet::from([extension])),
        )
        .unwrap();
        assert_eq!(
            full["main"],
            DiagnosticsBaselineCoverage::Partial { completed_files: BTreeSet::new() }
        );
        assert_eq!(full["extension:Ext"], DiagnosticsBaselineCoverage::Full);
    }

    #[test]
    fn partitioned_baseline_v1_migration_preserves_entries_without_current_diagnostics() {
        let plan = plan();
        let entries = vec![
            entry("src/cf/CommonModules/A/Ext/Module.bsl", "LineLength"),
            entry("src/cfe/Ext/CommonModules/B/Ext/Module.bsl", "IfElseDuplicatedCodeBlock"),
        ];
        let baseline = DiagnosticsBaseline {
            schema_version: crate::diagnostics_baseline::DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: crate::diagnostics_baseline::DiagnosticsBaselineScope {
                source_root: plan.project_scope.source_root.clone(),
                extensions: plan
                    .project_scope
                    .extensions
                    .iter()
                    .map(|extension| crate::diagnostics_baseline::DiagnosticsBaselineExtension {
                        name: extension.name.clone(),
                        path: extension.path.clone(),
                        scope: extension.scope,
                        legacy_scope: extension.legacy_scope,
                        dependency_scopes: extension.dependency_scopes.clone(),
                        depends_on: extension.depends_on.clone(),
                    })
                    .collect(),
            },
            diagnostics: entries.clone(),
        };
        let before = crate::diagnostics_baseline::diagnostics_baseline_json(&baseline).unwrap();
        let migrated = migrate_v1_to_partitioned(&baseline, &plan).unwrap();
        assert_eq!(migrated["main"], vec![entries[0].clone()]);
        assert_eq!(migrated["extension:Ext"], vec![entries[1].clone()]);
        assert_eq!(
            crate::diagnostics_baseline::diagnostics_baseline_json(&baseline).unwrap(),
            before
        );
    }

    #[test]
    fn v1_migration_accepts_fresh_legacy_root_and_dependency_but_rejects_old_or_contradictory_scope(
    ) {
        for legacy_root in [true, false] {
            let mut plan = plan();
            let extension = &mut plan.project_scope.extensions[0];
            if legacy_root {
                extension.scope = None;
                extension.legacy_scope = Some(true);
            } else {
                extension.dependency_scopes =
                    vec![project_model::DiagnosticsBaselineDependencyScope {
                        name: "Base".to_owned(),
                        scope: None,
                        legacy_scope: Some(true),
                    }];
                if let DiagnosticsBaselinePartitionIdentity::Extension {
                    dependency_scopes, ..
                } = &mut plan.partitions[1].identity
                {
                    *dependency_scopes = extension.dependency_scopes.clone();
                }
            }
            let scope = expected_v1_scope(&plan);
            let fresh = DiagnosticsBaseline {
                schema_version: crate::diagnostics_baseline::DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope: scope.clone(),
                diagnostics: vec![],
            };
            assert!(migrate_v1_to_partitioned(&fresh, &plan).is_ok());

            let mut markerless = fresh.clone();
            if legacy_root {
                markerless.scope.extensions[0].legacy_scope = None;
            } else {
                markerless.scope.extensions[0].dependency_scopes[0].legacy_scope = None;
            }
            assert!(matches!(
                migrate_v1_to_partitioned(&markerless, &plan),
                Err(PartitionedDiagnosticsBaselineError::ScopeMismatch)
            ));

            let mut contradictory = fresh;
            if legacy_root {
                contradictory.scope.extensions[0].scope =
                    Some(bsl_metadata::ExtensionScope::InfoBase);
            } else {
                contradictory.scope.extensions[0].dependency_scopes[0].scope =
                    Some(bsl_metadata::ExtensionScope::DataSeparation);
            }
            assert!(matches!(
                migrate_v1_to_partitioned(&contradictory, &plan),
                Err(PartitionedDiagnosticsBaselineError::Legacy(
                    DiagnosticsBaselineError::InvalidLegacyScopeMarker
                ))
            ));
        }
    }

    #[test]
    fn partitioned_baseline_v1_migration_rejects_scope_and_unowned_paths() {
        let plan = plan();
        let mut baseline = DiagnosticsBaseline {
            schema_version: crate::diagnostics_baseline::DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: crate::diagnostics_baseline::DiagnosticsBaselineScope {
                source_root: Some("wrong".to_owned()),
                extensions: vec![],
            },
            diagnostics: vec![],
        };
        assert!(matches!(
            migrate_v1_to_partitioned(&baseline, &plan),
            Err(PartitionedDiagnosticsBaselineError::ScopeMismatch)
        ));
        baseline.scope = crate::diagnostics_baseline::DiagnosticsBaselineScope {
            source_root: plan.project_scope.source_root.clone(),
            extensions: plan
                .project_scope
                .extensions
                .iter()
                .map(|extension| crate::diagnostics_baseline::DiagnosticsBaselineExtension {
                    name: extension.name.clone(),
                    path: extension.path.clone(),
                    scope: extension.scope,
                    legacy_scope: extension.legacy_scope,
                    dependency_scopes: extension.dependency_scopes.clone(),
                    depends_on: extension.depends_on.clone(),
                })
                .collect(),
        };
        baseline.diagnostics = vec![entry("outside/file.bsl", "LineLength")];
        assert!(matches!(
            migrate_v1_to_partitioned(&baseline, &plan),
            Err(PartitionedDiagnosticsBaselineError::UnownedDiagnostic(_))
        ));
    }
}
