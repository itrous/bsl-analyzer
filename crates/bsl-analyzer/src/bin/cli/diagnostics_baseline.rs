use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    io::{BufReader, Write},
    path::{Path, PathBuf},
};

use clap::{Args, Subcommand, ValueEnum};
use ide::diagnostics_baseline::{
    classify_diagnostics, diagnostics_baseline_json, is_protected_diagnostic,
    parse_diagnostics_baseline, BaselineDiagnosticCandidate, DiagnosticsBaseline,
    DiagnosticsBaselineCoverage, DiagnosticsBaselineEntry, DiagnosticsBaselineRange,
    DiagnosticsBaselineScope, DiagnosticsBaselineSummary, DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
};
use ide::partitioned_diagnostics_baseline::{
    classify_partitioned_diagnostics, diagnostics_manifest, diagnostics_manifest_json,
    diagnostics_partition_json, load_diagnostics_baseline_set, migrate_v1_reader,
    partition_object_path, ClassifiedPartitionedDiagnostics, DiagnosticsBaselineManifest,
    DiagnosticsBaselineManifestEntry, DiagnosticsBaselinePartitionFile,
    PartitionedBaselineDiagnosticCandidate, DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION,
};
use serde::Serialize;

use bsl_analyzer::diagnostics_baseline::transaction::{
    publish_set, repair_object, PartitionFileWriter, PreparedPartition,
};
use bsl_analyzer::reporters::FileAnalysis;

type Candidate<T> = BaselineDiagnosticCandidate<(T, usize)>;

#[derive(Debug, Subcommand)]
pub enum DiagnosticsCommand {
    Baseline {
        #[command(subcommand)]
        command: DiagnosticsBaselineCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DiagnosticsBaselineCommand {
    Create(DiagnosticsBaselineCreateArgs),
    Check(DiagnosticsBaselineSelectedArgs),
    Update(DiagnosticsBaselineSelectedArgs),
}

#[derive(Debug, Clone, Args)]
pub struct DiagnosticsBaselineArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub source_dir: PathBuf,

    #[arg(short = 'c', long = "config")]
    pub config: Option<PathBuf>,

    #[arg(long = "format", value_enum, default_value_t)]
    pub format: DiagnosticsBaselineOutputFormat,
}

#[derive(Debug, Clone, Args)]
pub struct DiagnosticsBaselineCreateArgs {
    #[command(flatten)]
    pub common: DiagnosticsBaselineArgs,

    #[arg(long, conflicts_with = "from_v1")]
    pub partition: Option<String>,

    #[arg(long = "from-v1", value_name = "PATH")]
    pub from_v1: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct DiagnosticsBaselineSelectedArgs {
    #[command(flatten)]
    pub common: DiagnosticsBaselineArgs,

    #[arg(long)]
    pub partition: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum DiagnosticsBaselineOutputFormat {
    #[default]
    Text,
    Json,
}

impl DiagnosticsBaselineCommand {
    pub fn common(&self) -> &DiagnosticsBaselineArgs {
        match self {
            Self::Create(args) => &args.common,
            Self::Check(args) | Self::Update(args) => &args.common,
        }
    }

    pub fn partition(&self) -> Option<&str> {
        match self {
            Self::Create(args) => args.partition.as_deref(),
            Self::Check(args) | Self::Update(args) => args.partition.as_deref(),
        }
    }

    /// `./legacy.json` is what a shell completes to, and the managed-path rules reject a
    /// `.` component. The prefix is stripped once, here, so preflight and the command
    /// itself see the same spelling and the safety rules stay untouched.
    pub fn migration_source(&self) -> Option<&Path> {
        let source = match self {
            Self::Create(args) => args.from_v1.as_deref(),
            Self::Check(_) | Self::Update(_) => None,
        }?;
        Some(source.strip_prefix("./").unwrap_or(source))
    }

    pub fn output_format(&self) -> DiagnosticsBaselineOutputFormat {
        self.common().format
    }

    pub fn preflight(
        &self,
        project: &project_model::Project,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let resolved = project
            .diagnostics_baseline()?
            .ok_or("[diagnostics.baseline].path or .directory is not configured")?;
        let partitioned = matches!(
            resolved.mode,
            project_model::DiagnosticsBaselineProjectMode::Partitioned { .. }
        );
        if !partitioned && (self.partition().is_some() || self.migration_source().is_some()) {
            return Err("--partition and --from-v1 require [diagnostics.baseline].directory".into());
        }
        if let Some(selected) = self.partition() {
            let plan = project
                .diagnostics_baseline_partition_plan()?
                .ok_or("--partition requires partitioned baseline mode")?;
            if !plan.partitions.iter().any(|partition| partition.id == selected) {
                return Err(format!("unknown diagnostics baseline partition: {selected}").into());
            }
            if !matches!(self, Self::Check(_))
                && plan.policy_for_partition(selected)
                    == Some(project_model::DiagnosticsBaselinePartitionPolicy::Unsuppressed)
            {
                return Err(format!("partition_unsuppressed: {selected}").into());
            }
            if matches!(self, Self::Create(_)) {
                let directory = project_model::ManagedBaselineDirectory::open(
                    &project.root,
                    &resolved.project_path,
                    false,
                );
                // Only an ABSENT manifest means "nothing published yet". A rejected
                // directory, a symlinked manifest or a permission failure are different
                // problems, and reporting them as a first run sends the user looking in
                // the wrong place.
                match directory.as_ref().map(|directory| directory.open_file("manifest.json")) {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err("the first partitioned baseline must be created in full".into());
                    }
                    Ok(Err(error)) => {
                        return Err(format!(
                            "cannot read the diagnostics baseline manifest: {error}"
                        )
                        .into())
                    }
                    Err(error) => {
                        return Err(format!(
                            "cannot open the diagnostics baseline directory: {error}"
                        )
                        .into())
                    }
                }
            }
        }
        if let Some(source) = self.migration_source() {
            // `./legacy.json` is what a shell completes to, and the managed-path rules
            // reject a `.` component. Normalising it here keeps the safety rules intact
            // while accepting the spelling users actually type.
            let source = source.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
            project_model::ManagedBaselineDirectory::open_project_root(&project.root)?
                .open_file(&source)?;
        }
        Ok(())
    }
}

pub fn run(command: DiagnosticsCommand) -> Result<(), Box<dyn Error + Send + Sync>> {
    let DiagnosticsCommand::Baseline { command } = command;
    super::analyze::diagnostics_baseline(command)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticsBaselineOperationResult {
    pub operation: &'static str,
    pub path: String,
    pub success: bool,
    pub added: usize,
    pub removed: usize,
    pub unchanged: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<project_model::DiagnosticsBaselineSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partitions_enabled: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partitions_unsuppressed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsuppressed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_unsuppressed: Option<usize>,
    pub diagnostics: Vec<DiagnosticsBaselineEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_partition: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions: Vec<ide::diagnostics_baseline::DiagnosticsBaselinePartitionSummary>,
}

impl fmt::Display for DiagnosticsBaselineOperationResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Diagnostics baseline {}: {} (added {}, removed {}, unchanged {})",
            self.operation, self.path, self.added, self.removed, self.unchanged
        )?;
        for diagnostic in &self.diagnostics {
            write!(
                f,
                "\n  {} {}:{} {}",
                diagnostic.code,
                diagnostic.path,
                diagnostic.range.start_line + 1,
                diagnostic.message,
            )?;
        }
        Ok(())
    }
}

pub fn apply(
    project: &project_model::Project,
    files: &[FileAnalysis],
    proof: &super::analyze::CoverageProof,
    command: DiagnosticsBaselineCommand,
) -> Result<DiagnosticsBaselineOperationResult, Box<dyn Error + Send + Sync>> {
    // Migration reads the v1 file and nothing else — no current diagnostic takes part —
    // so a partial analysis cannot make its result wrong. Demanding full coverage there
    // would block the format change for every project that uses `diff_base` or
    // `ignored_authors`, which is the reason those projects have a large baseline to
    // migrate in the first place.
    let migrating =
        matches!(&command, DiagnosticsBaselineCommand::Create(args) if args.from_v1.is_some());
    if !migrating {
        // Named reason first: an active filter always makes coverage partial too, so
        // `require_full` would answer with a `CoverageProof` dump and the actionable
        // message below would never be seen.
        if project.config.analysis.diff_base.is_some()
            || !project.config.analysis.ignored_authors.is_empty()
        {
            return Err("full diagnostics coverage required: [analysis].diff_base or \
                        [analysis].ignored_authors is active, so the analysis is narrowed"
                .into());
        }
        proof.require_full()?;
    }

    let resolved =
        project.diagnostics_baseline()?.ok_or("[diagnostics.baseline].path is not configured")?;
    if matches!(resolved.mode, project_model::DiagnosticsBaselineProjectMode::Partitioned { .. }) {
        return apply_partitioned(project, files, command, &resolved);
    }
    let scope = scope(&resolved.scope);
    let candidates = candidates(project, files.iter().enumerate())?;

    match command {
        DiagnosticsBaselineCommand::Create(_) => {
            if resolved.path.exists() {
                return Err(format!(
                    "diagnostics baseline already exists: {}",
                    resolved.path.display()
                )
                .into());
            }
            let empty = DiagnosticsBaseline {
                schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope: scope.clone(),
                diagnostics: Vec::new(),
            };
            let classified = classify_diagnostics(
                &empty,
                resolved.project_path.clone(),
                candidates,
                &DiagnosticsBaselineCoverage::Full,
            )?;
            let diagnostics = recordable(classified.new.into_iter().map(|item| item.entry));
            let baseline = DiagnosticsBaseline {
                schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope,
                diagnostics,
            };
            let bytes = diagnostics_baseline_json(&baseline)?;
            publish_new(&resolved.path, &bytes)?;
            Ok(DiagnosticsBaselineOperationResult {
                operation: "created",
                path: resolved.project_path,
                success: true,
                added: baseline.diagnostics.len(),
                removed: 0,
                unchanged: 0,
                selection: None,
                partitions_enabled: None,
                partitions_unsuppressed: None,
                unsuppressed: None,
                skipped_unsuppressed: None,
                diagnostics: baseline.diagnostics,
                generation: None,
                selected_partition: None,
                partitions: vec![],
            })
        }
        DiagnosticsBaselineCommand::Check(_) => {
            let baseline = read_baseline(&resolved.path, &scope)?;
            let classified = classify_diagnostics(
                &baseline,
                resolved.project_path.clone(),
                candidates,
                &DiagnosticsBaselineCoverage::Full,
            )?;
            let added = classified.new.len();
            let removed = classified.resolved.len();
            let mut diagnostics: Vec<_> = classified
                .new
                .into_iter()
                .map(|item| item.entry)
                .chain(classified.resolved)
                .collect();
            diagnostics.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
            Ok(DiagnosticsBaselineOperationResult {
                operation: "checked",
                path: resolved.project_path,
                success: added == 0 && removed == 0,
                added,
                removed,
                unchanged: classified.known.len(),
                selection: None,
                partitions_enabled: None,
                partitions_unsuppressed: None,
                unsuppressed: None,
                skipped_unsuppressed: None,
                diagnostics,
                generation: None,
                selected_partition: None,
                partitions: vec![],
            })
        }
        DiagnosticsBaselineCommand::Update(_) => {
            let baseline = read_baseline(&resolved.path, &scope)?;
            let classified = classify_diagnostics(
                &baseline,
                resolved.project_path.clone(),
                candidates,
                &DiagnosticsBaselineCoverage::Full,
            )?;
            let added = classified.new.len();
            let removed = classified.resolved.len();
            let unchanged = classified.known.len();
            let mut diagnostics: Vec<_> = recordable(
                classified.new.into_iter().chain(classified.known).map(|item| item.entry),
            );
            let bytes = diagnostics_baseline_json(&DiagnosticsBaseline {
                schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
                scope,
                diagnostics: diagnostics.clone(),
            })?;
            replace(&resolved.path, &bytes)?;
            diagnostics.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
            Ok(DiagnosticsBaselineOperationResult {
                operation: "updated",
                path: resolved.project_path,
                success: true,
                added,
                removed,
                unchanged,
                selection: None,
                partitions_enabled: None,
                partitions_unsuppressed: None,
                unsuppressed: None,
                skipped_unsuppressed: None,
                diagnostics,
                generation: None,
                selected_partition: None,
                partitions: vec![],
            })
        }
    }
}

fn apply_partitioned(
    project: &project_model::Project,
    files: &[FileAnalysis],
    command: DiagnosticsBaselineCommand,
    resolved: &project_model::ResolvedDiagnosticsBaseline,
) -> Result<DiagnosticsBaselineOperationResult, Box<dyn Error + Send + Sync>> {
    command.preflight(project)?;
    let plan = project
        .diagnostics_baseline_partition_plan()?
        .ok_or("partitioned diagnostics baseline plan is unavailable")?;
    let create = matches!(command, DiagnosticsBaselineCommand::Create(_));
    let directory = project_model::ManagedBaselineDirectory::open(
        &project.root,
        &resolved.project_path,
        create || matches!(command, DiagnosticsBaselineCommand::Update(_)),
    )?;
    let selected = command.partition().map(str::to_owned);
    let coverage: std::collections::BTreeMap<_, _> = plan
        .partitions
        .iter()
        .map(|partition| (partition.id.clone(), DiagnosticsBaselineCoverage::Full))
        .collect();

    match command {
        DiagnosticsBaselineCommand::Create(args) => {
            let existing_manifest = read_partitioned_manifest(&directory)?;
            if let Some(selected) = args.partition {
                let current = partitioned_candidates(project, &plan, files.iter().enumerate())?;
                let manifest = existing_manifest
                    .ok_or("the first partitioned baseline must be created in full")?;
                let partition = plan
                    .partitions
                    .iter()
                    .find(|partition| partition.id == selected)
                    .expect("selector was checked during preflight");
                let mut buckets = current_entries_by_partition(&plan, current)?;
                let diagnostics = buckets.remove(&selected).unwrap();
                let bytes =
                    diagnostics_partition_json(partition.identity.clone(), diagnostics.clone())?;
                let repairing =
                    manifest.partitions.iter().any(|entry| entry.partition_id == selected);
                let (added, generation) = if let Some(expected) =
                    manifest.partitions.iter().find(|entry| entry.partition_id == selected)
                {
                    validate_set_for_repair(&directory, &plan, &manifest, &selected, &bytes, true)?;
                    repair_object(&directory, expected, &bytes)?;
                    (0, manifest.generation.clone())
                } else {
                    diagnostics_manifest_json(&manifest)?;
                    if manifest.project_scope_fingerprint != plan.project_scope_fingerprint {
                        return Err(
                            "diagnostics baseline scope does not match the current project".into(),
                        );
                    }
                    let hash = blake3::hash(&bytes).to_hex().to_string();
                    let mut validation_entries = manifest.partitions.clone();
                    validation_entries.push(DiagnosticsBaselineManifestEntry {
                        partition_id: selected.clone(),
                        file: partition_object_path(&partition.id, &partition.key, &hash)?,
                        blake3: hash,
                    });
                    let validation_manifest = diagnostics_manifest(
                        plan.project_scope_fingerprint.clone(),
                        validation_entries,
                    );
                    validate_set_for_repair(
                        &directory,
                        &plan,
                        &validation_manifest,
                        &selected,
                        &bytes,
                        false,
                    )?;
                    let mut prepared = plan
                        .partitions
                        .iter()
                        .filter(|candidate| plan.enabled_partition_ids.contains(&candidate.id))
                        .map(|candidate| {
                            if candidate.id == selected {
                                Ok(PreparedPartition::Write {
                                    id: candidate.id.clone(),
                                    key: candidate.key.clone(),
                                    bytes: bytes.clone(),
                                })
                            } else {
                                let entry = manifest
                                    .partitions
                                    .iter()
                                    .find(|entry| entry.partition_id == candidate.id)
                                    .ok_or("diagnostics baseline enabled partition is missing")?
                                    .clone();
                                Ok(PreparedPartition::Reuse {
                                    id: candidate.id.clone(),
                                    key: candidate.key.clone(),
                                    entry,
                                })
                            }
                        })
                        .collect::<Result<Vec<_>, Box<dyn Error + Send + Sync>>>()?;
                    carry_dormant(&plan, &manifest, &mut prepared)?;
                    let published = publish_set(
                        &directory,
                        plan.project_scope_fingerprint.clone(),
                        prepared,
                        Some(&manifest.generation),
                    )?;
                    (diagnostics.len(), published.manifest.generation)
                };
                let snapshot = load_diagnostics_baseline_set(&directory, &plan)?;
                let current = partitioned_candidates(project, &plan, files.iter().enumerate())?;
                let classified = classify_partitioned_diagnostics(
                    &snapshot,
                    &plan,
                    resolved.project_path.clone(),
                    current,
                    &coverage,
                )?;
                let (_, _, unchanged, partitions) = selected_counts(&classified, Some(&selected));
                // The reclassification above sees the entries this very command just
                // published, so for a partition created from scratch they are the SAME
                // records as `added`. Reporting both would say the partition holds twice
                // what it does; a repair publishes nothing new and keeps its count.
                let unchanged = if repairing { unchanged } else { 0 };
                return Ok(DiagnosticsBaselineOperationResult {
                    operation: "created",
                    path: resolved.project_path.clone(),
                    success: true,
                    added,
                    removed: 0,
                    unchanged,
                    selection: Some(plan.selection),
                    partitions_enabled: Some(plan.enabled_partition_ids.len()),
                    partitions_unsuppressed: Some(
                        plan.partitions.len() - plan.enabled_partition_ids.len(),
                    ),
                    // Not measured on this path: a zero would read as "no partition is
                    // left unsuppressed", which the command never checked.
                    unsuppressed: None,
                    skipped_unsuppressed: None,
                    diagnostics,
                    generation: Some(generation),
                    selected_partition: Some(selected),
                    partitions,
                });
            }
            if existing_manifest.is_some() {
                return Err("diagnostics baseline manifest already exists".into());
            }
            if let Some(source) = args.from_v1.as_deref() {
                let source = source
                    .strip_prefix("./")
                    .unwrap_or(source)
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let project_root =
                    project_model::ManagedBaselineDirectory::open_project_root(&project.root)?;
                let mut writers: BTreeMap<_, _> = plan
                    .partitions
                    .iter()
                    .filter(|partition| plan.enabled_partition_ids.contains(&partition.id))
                    .map(|partition| {
                        Ok((partition.id.clone(), PartitionFileWriter::new(&directory, partition)?))
                    })
                    .collect::<Result<
                        _,
                        ide::partitioned_diagnostics_baseline::PartitionedDiagnosticsBaselineError,
                    >>()?;
                let migration = migrate_v1_reader(
                    BufReader::new(project_root.open_file(&source)?),
                    &plan,
                    |owner, entry| writers.get_mut(owner).unwrap().write_entry(entry),
                )?;
                let mut prepared = Vec::with_capacity(writers.len());
                for writer in writers.into_values() {
                    match writer.finish() {
                        Ok((partition, _)) => prepared.push(partition),
                        Err(error) => {
                            cleanup_staged_partitions(&directory, &prepared);
                            return Err(error.into());
                        }
                    }
                }
                let published = publish_set(
                    &directory,
                    plan.project_scope_fingerprint.clone(),
                    prepared,
                    None,
                )?;
                return Ok(DiagnosticsBaselineOperationResult {
                    operation: "created",
                    path: resolved.project_path.clone(),
                    success: true,
                    added: migration.migrated,
                    removed: 0,
                    unchanged: 0,
                    selection: Some(plan.selection),
                    partitions_enabled: Some(plan.enabled_partition_ids.len()),
                    partitions_unsuppressed: Some(
                        plan.partitions.len() - plan.enabled_partition_ids.len(),
                    ),
                    unsuppressed: None,
                    skipped_unsuppressed: Some(migration.skipped_unsuppressed),
                    diagnostics: vec![],
                    generation: Some(published.manifest.generation),
                    selected_partition: None,
                    partitions: vec![],
                });
            }
            let current = partitioned_candidates(project, &plan, files.iter().enumerate())?;
            let buckets = current_entries_by_partition(&plan, current)?;
            let unsuppressed = buckets
                .iter()
                .filter(|(id, _)| !plan.enabled_partition_ids.contains(id))
                .map(|(_, entries)| entries.len())
                .sum();
            let prepared = prepare_all(&plan, buckets)?;
            let published =
                publish_set(&directory, plan.project_scope_fingerprint.clone(), prepared, None)?;
            let diagnostics = load_diagnostics_baseline_set(&directory, &plan)?
                .partitions
                .values()
                .flat_map(|partition| partition.owned_entries())
                .collect::<Vec<_>>();
            Ok(DiagnosticsBaselineOperationResult {
                operation: "created",
                path: resolved.project_path.clone(),
                success: true,
                added: diagnostics.len(),
                removed: 0,
                unchanged: 0,
                selection: Some(plan.selection),
                partitions_enabled: Some(plan.enabled_partition_ids.len()),
                partitions_unsuppressed: Some(
                    plan.partitions.len() - plan.enabled_partition_ids.len(),
                ),
                unsuppressed: Some(unsuppressed),
                skipped_unsuppressed: None,
                diagnostics,
                generation: Some(published.manifest.generation),
                selected_partition: None,
                partitions: vec![],
            })
        }
        DiagnosticsBaselineCommand::Check(_) => {
            let current = partitioned_candidates(project, &plan, files.iter().enumerate())?;
            let snapshot = load_diagnostics_baseline_set(&directory, &plan)?;
            let classified = classify_partitioned_diagnostics(
                &snapshot,
                &plan,
                resolved.project_path.clone(),
                current,
                &coverage,
            )?;
            operation_from_classified(
                "checked",
                resolved.project_path.clone(),
                snapshot.manifest.generation,
                selected,
                classified,
                &plan,
            )
        }
        DiagnosticsBaselineCommand::Update(_) => {
            let current = partitioned_candidates(project, &plan, files.iter().enumerate())?;
            let snapshot = match load_diagnostics_baseline_set(&directory, &plan) {
                Ok(snapshot) => snapshot,
                Err(error) if selected.is_none() && matches!(
                    error,
                    ide::partitioned_diagnostics_baseline::PartitionedDiagnosticsBaselineError::ScopeMismatch
                        | ide::partitioned_diagnostics_baseline::PartitionedDiagnosticsBaselineError::MissingPartitions { .. }
                        | ide::partitioned_diagnostics_baseline::PartitionedDiagnosticsBaselineError::OrphanPartitions(_)
                        | ide::partitioned_diagnostics_baseline::PartitionedDiagnosticsBaselineError::PartitionIdentityMismatch(_)
                ) => {
                    let old = read_partitioned_manifest(&directory)?
                        .ok_or("diagnostics baseline manifest is missing")?;
                    let buckets = current_entries_by_partition(&plan, current)?;
                    let unsuppressed = buckets
                        .iter()
                        .filter(|(id, _)| !plan.enabled_partition_ids.contains(id))
                        .map(|(_, entries)| entries.len())
                        .sum();
                    let diagnostics = buckets
                        .iter()
                        .filter(|(id, _)| plan.enabled_partition_ids.contains(id))
                        .flat_map(|(_, entries)| entries.iter().cloned())
                        .collect::<Vec<_>>();
                    // Carry the dormant partitions whenever the project scope is
                    // unchanged, because that is exactly when `publish_set` deletes the
                    // objects a new manifest omits — a repair must not take partitions
                    // outside `include` with it. A changed scope prunes dormant metadata
                    // on purpose, and leaves the objects themselves on disk.
                    let mut prepared = prepare_all(&plan, buckets)?;
                    if old.project_scope_fingerprint == plan.project_scope_fingerprint {
                        carry_dormant(&plan, &old, &mut prepared)?;
                    }
                    let published = publish_set(
                        &directory,
                        plan.project_scope_fingerprint.clone(),
                        prepared,
                        Some(&old.generation),
                    )?;
                    return Ok(DiagnosticsBaselineOperationResult {
                        // Not "updated": the previous set could not be loaded, which is
                        // why this branch exists, so nothing can be compared against it.
                        // `added` counts the records the rebuilt set holds — calling that
                        // an update would report 1.6M new findings for a repair.
                        operation: "rebuilt",
                        path: resolved.project_path.clone(),
                        success: true,
                        added: diagnostics.len(),
                        removed: 0,
                        unchanged: 0,
                        selection: Some(plan.selection),
                        partitions_enabled: Some(plan.enabled_partition_ids.len()),
                        partitions_unsuppressed: Some(
                            plan.partitions.len() - plan.enabled_partition_ids.len(),
                        ),
                        unsuppressed: Some(unsuppressed),
                        skipped_unsuppressed: None,
                        diagnostics,
                        generation: Some(published.manifest.generation),
                        selected_partition: None,
                        partitions: vec![],
                    });
                }
                Err(error) => return Err(error.into()),
            };
            let generation = snapshot.manifest.generation.clone();
            if selected.is_some()
                && snapshot.manifest.project_scope_fingerprint != plan.project_scope_fingerprint
            {
                return Err("diagnostics baseline scope does not match the current project".into());
            }
            let classified = classify_partitioned_diagnostics(
                &snapshot,
                &plan,
                resolved.project_path.clone(),
                current,
                &coverage,
            )?;
            let (added, removed, unchanged, summaries) =
                selected_counts(&classified, selected.as_deref());
            let mut buckets: std::collections::BTreeMap<String, Vec<DiagnosticsBaselineEntry>> =
                plan.partitions.iter().map(|partition| (partition.id.clone(), vec![])).collect();
            for entry in recordable(
                classified.new.iter().chain(&classified.known).map(|item| item.entry.clone()),
            ) {
                let owner =
                    plan.owner_for_project_path(&entry.path).ok_or("unowned current diagnostic")?;
                buckets.get_mut(owner).unwrap().push(entry);
            }
            let mut prepared = Vec::new();
            for partition in plan
                .partitions
                .iter()
                .filter(|partition| plan.enabled_partition_ids.contains(&partition.id))
            {
                if selected.as_deref().is_none_or(|id| id == partition.id) {
                    prepared.push(PreparedPartition::Write {
                        id: partition.id.clone(),
                        key: partition.key.clone(),
                        bytes: diagnostics_partition_json(
                            partition.identity.clone(),
                            buckets.remove(&partition.id).unwrap(),
                        )?,
                    });
                } else {
                    let entry = snapshot
                        .manifest
                        .partitions
                        .iter()
                        .find(|entry| entry.partition_id == partition.id)
                        .unwrap()
                        .clone();
                    prepared.push(PreparedPartition::Reuse {
                        id: partition.id.clone(),
                        key: partition.key.clone(),
                        entry,
                    });
                }
            }
            if snapshot.manifest.project_scope_fingerprint == plan.project_scope_fingerprint {
                carry_dormant(&plan, &snapshot.manifest, &mut prepared)?;
            }
            let published = publish_set(
                &directory,
                plan.project_scope_fingerprint.clone(),
                prepared,
                Some(&generation),
            )?;
            let diagnostics = classified
                .new
                .into_iter()
                .chain(classified.known)
                .filter(|item| {
                    selected
                        .as_deref()
                        .is_none_or(|id| plan.owner_for_project_path(&item.entry.path) == Some(id))
                })
                .map(|item| item.entry)
                .collect();
            Ok(DiagnosticsBaselineOperationResult {
                operation: "updated",
                path: resolved.project_path.clone(),
                success: true,
                added,
                removed,
                unchanged,
                selection: Some(plan.selection),
                partitions_enabled: Some(plan.enabled_partition_ids.len()),
                partitions_unsuppressed: Some(
                    plan.partitions.len() - plan.enabled_partition_ids.len(),
                ),
                // Scoped to the selected partition, as `check --partition` reports it:
                // answering a question about one partition with the project-wide number
                // makes the two commands disagree about the same run.
                unsuppressed: Some(match selected.as_deref() {
                    Some(id) => summaries
                        .iter()
                        .find(|partition| partition.id == id)
                        .map_or(0, |partition| partition.unsuppressed),
                    None => classified.summary.unsuppressed.unwrap_or_default(),
                }),
                skipped_unsuppressed: None,
                diagnostics,
                generation: Some(published.manifest.generation),
                selected_partition: selected,
                partitions: summaries,
            })
        }
    }
}

fn cleanup_staged_partitions(
    directory: &project_model::ManagedBaselineDirectory,
    partitions: &[PreparedPartition],
) {
    for partition in partitions {
        let PreparedPartition::Staged { path, .. } = partition else {
            continue;
        };
        match directory.remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%error, %path, "staged baseline cleanup failed"),
        }
    }
}

fn carry_dormant(
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
    manifest: &DiagnosticsBaselineManifest,
    prepared: &mut Vec<PreparedPartition>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for partition in plan
        .partitions
        .iter()
        .filter(|partition| !plan.enabled_partition_ids.contains(&partition.id))
    {
        let Some(entry) =
            manifest.partitions.iter().find(|entry| entry.partition_id == partition.id).cloned()
        else {
            continue;
        };
        if entry.file != partition_object_path(&partition.id, &partition.key, &entry.blake3)? {
            return Err(format!("invalid diagnostics baseline object path: {}", entry.file).into());
        }
        prepared.push(PreparedPartition::Carry {
            id: partition.id.clone(),
            key: partition.key.clone(),
            entry,
        });
    }
    Ok(())
}

fn read_partitioned_manifest(
    directory: &project_model::ManagedBaselineDirectory,
) -> Result<
    Option<ide::partitioned_diagnostics_baseline::DiagnosticsBaselineManifest>,
    Box<dyn Error + Send + Sync>,
> {
    use std::io::Read;
    match directory.open_file("manifest.json") {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn validate_set_for_repair(
    directory: &project_model::ManagedBaselineDirectory,
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
    manifest: &DiagnosticsBaselineManifest,
    selected: &str,
    replacement: &[u8],
    require_repairable: bool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    diagnostics_manifest_json(manifest)?;
    if manifest.project_scope_fingerprint != plan.project_scope_fingerprint {
        return Err("diagnostics baseline scope does not match the current project".into());
    }
    if plan
        .enabled_partition_ids
        .iter()
        .any(|id| !manifest.partitions.iter().any(|entry| entry.partition_id == *id))
    {
        return Err("diagnostics baseline partition set does not match the current project".into());
    }

    let selected_entry = manifest
        .partitions
        .iter()
        .find(|entry| entry.partition_id == selected)
        .ok_or("selected partition is absent from manifest")?;
    let repairable = match read_managed_file(directory, &selected_entry.file) {
        Ok(bytes) => blake3::hash(&bytes).to_hex().as_str() != selected_entry.blake3,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };

    let mut fingerprints = std::collections::HashSet::new();
    for partition in plan
        .partitions
        .iter()
        .filter(|partition| plan.enabled_partition_ids.contains(&partition.id))
    {
        let entry = manifest
            .partitions
            .iter()
            .find(|entry| entry.partition_id == partition.id)
            .expect("enabled entries were checked above");
        if entry.file != partition_object_path(&partition.id, &partition.key, &entry.blake3)? {
            return Err(format!("invalid diagnostics baseline object path: {}", entry.file).into());
        }
        let bytes = if entry.partition_id == selected {
            replacement.to_vec()
        } else {
            read_managed_file(directory, &entry.file)?
        };
        if blake3::hash(&bytes).to_hex().as_str() != entry.blake3 {
            return Err(format!("diagnostics baseline object is corrupt: {}", entry.file).into());
        }
        let file: DiagnosticsBaselinePartitionFile = serde_json::from_slice(&bytes)?;
        if file.schema_version != DIAGNOSTICS_BASELINE_PARTITION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported diagnostics baseline partition schema: {}",
                file.schema_version
            )
            .into());
        }
        if file.partition != partition.identity {
            return Err(format!(
                "diagnostics baseline partition identity mismatch: {}",
                entry.partition_id
            )
            .into());
        }
        diagnostics_partition_json(file.partition, file.diagnostics.clone())?;
        for diagnostic in file.diagnostics {
            if plan.owner_for_project_path(&diagnostic.path) != Some(entry.partition_id.as_str()) {
                return Err(format!(
                    "diagnostics baseline entry has the wrong owner: {}",
                    diagnostic.path
                )
                .into());
            }
            if !fingerprints.insert(diagnostic.fingerprint.clone()) {
                return Err(format!(
                    "duplicate diagnostics baseline fingerprint: {}",
                    diagnostic.fingerprint
                )
                .into());
            }
        }
    }
    if require_repairable && !repairable {
        return Err("selected diagnostics baseline object is already valid".into());
    }
    Ok(())
}

fn read_managed_file(
    directory: &project_model::ManagedBaselineDirectory,
    path: &str,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    directory.open_file(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

type IndexedPartitionedCandidate<T> = PartitionedBaselineDiagnosticCandidate<(T, usize, String)>;

fn partitioned_candidates<'a, T: Clone + 'a>(
    project: &project_model::Project,
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
    files: impl IntoIterator<Item = (T, &'a FileAnalysis)>,
) -> Result<Vec<IndexedPartitionedCandidate<T>>, Box<dyn Error + Send + Sync>> {
    #[cfg(test)]
    CANDIDATE_PREPARATIONS.with(|count| count.set(count.get() + 1));
    let root = project.root.canonicalize()?;
    let mut result = Vec::new();
    for (file_id, file) in files {
        let path = file
            .path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let owner = plan
            .owner_for_project_path(&path)
            .ok_or_else(|| format!("diagnostics file has no partition owner: {path}"))?
            .to_owned();
        for (index, diagnostic) in file.diagnostics.iter().enumerate() {
            result.push(PartitionedBaselineDiagnosticCandidate {
                partition_id: owner.clone(),
                candidate: BaselineDiagnosticCandidate {
                    diagnostic: (file_id.clone(), index, owner.clone()),
                    path: path.clone(),
                    code: diagnostic.code.clone(),
                    snippet: file.line_snippets.get(index).cloned(),
                    message: diagnostic.message.clone(),
                    severity: diagnostic.severity.clone(),
                    range: DiagnosticsBaselineRange {
                        start_line: diagnostic.start_line.try_into()?,
                        start_column: diagnostic.start_column.try_into()?,
                        end_line: diagnostic.end_line.try_into()?,
                        end_column: diagnostic.end_column.try_into()?,
                    },
                },
            });
        }
    }
    Ok(result)
}

fn current_entries_by_partition<T>(
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
    current: Vec<PartitionedBaselineDiagnosticCandidate<T>>,
) -> Result<
    std::collections::BTreeMap<String, Vec<DiagnosticsBaselineEntry>>,
    Box<dyn Error + Send + Sync>,
> {
    let mut buckets: std::collections::BTreeMap<_, Vec<_>> =
        plan.partitions.iter().map(|partition| (partition.id.clone(), vec![])).collect();
    let empty = DiagnosticsBaseline {
        schema_version: DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
        scope: scope_from_plan(plan),
        diagnostics: vec![],
    };
    let candidates = current
        .into_iter()
        .map(|item| BaselineDiagnosticCandidate {
            diagnostic: item.partition_id,
            path: item.candidate.path,
            code: item.candidate.code,
            snippet: item.candidate.snippet,
            message: item.candidate.message,
            severity: item.candidate.severity,
            range: item.candidate.range,
        })
        .collect();
    let classified = classify_diagnostics(
        &empty,
        String::new(),
        candidates,
        &DiagnosticsBaselineCoverage::Full,
    )?;
    for item in classified.new {
        if is_protected_diagnostic(&item.entry.code) {
            continue;
        }
        buckets.get_mut(&item.diagnostic).unwrap().push(item.entry);
    }
    Ok(buckets)
}

fn scope_from_plan(
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
) -> DiagnosticsBaselineScope {
    DiagnosticsBaselineScope {
        source_root: plan.project_scope.source_root.clone(),
        extensions: plan.project_scope.extensions.clone(),
    }
}

/// Entries a baseline file may hold. A protected diagnostic stays active and is
/// never recorded, so every write path filters here instead of each rediscovering
/// the rule — the validator rejects such an entry and would fail the whole command.
fn recordable(
    entries: impl IntoIterator<Item = DiagnosticsBaselineEntry>,
) -> Vec<DiagnosticsBaselineEntry> {
    entries.into_iter().filter(|entry| !is_protected_diagnostic(&entry.code)).collect()
}

fn prepare_all(
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
    mut buckets: std::collections::BTreeMap<String, Vec<DiagnosticsBaselineEntry>>,
) -> Result<Vec<PreparedPartition>, Box<dyn Error + Send + Sync>> {
    plan.partitions
        .iter()
        .filter(|partition| plan.enabled_partition_ids.contains(&partition.id))
        .map(|partition| {
            Ok(PreparedPartition::Write {
                id: partition.id.clone(),
                key: partition.key.clone(),
                bytes: diagnostics_partition_json(
                    partition.identity.clone(),
                    buckets.remove(&partition.id).unwrap_or_default(),
                )?,
            })
        })
        .collect()
}

fn selected_counts<T>(
    classified: &ClassifiedPartitionedDiagnostics<T>,
    selected: Option<&str>,
) -> (usize, usize, usize, Vec<ide::diagnostics_baseline::DiagnosticsBaselinePartitionSummary>) {
    let summaries = classified.summary.partitions.clone();
    if let Some(selected) = selected {
        let summary = summaries.iter().find(|summary| summary.id == selected).unwrap();
        (summary.new, summary.resolved, summary.known, vec![summary.clone()])
    } else {
        (
            classified.summary.new.unwrap_or_default(),
            classified.summary.resolved.unwrap_or_default(),
            classified.summary.known.unwrap_or_default(),
            summaries,
        )
    }
}

fn operation_from_classified<T>(
    operation: &'static str,
    path: String,
    generation: String,
    selected: Option<String>,
    classified: ClassifiedPartitionedDiagnostics<T>,
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
) -> Result<DiagnosticsBaselineOperationResult, Box<dyn Error + Send + Sync>> {
    let (added, removed, unchanged, partitions) = selected_counts(&classified, selected.as_deref());
    let unsuppressed = selected
        .as_deref()
        .and_then(|id| partitions.iter().find(|partition| partition.id == id))
        .map_or_else(
            || classified.summary.unsuppressed.unwrap_or_default(),
            |partition| partition.unsuppressed,
        );
    let selected_id = selected.as_deref();
    let resolved = match selected_id {
        Some(id) => classified.resolved.retain_partition(id),
        None => classified.resolved,
    };
    // `diagnostics` lists the records this run TOUCHED. Findings of partitions outside
    // `include` are merely active — they are counted in `unsuppressed`, and listing them
    // here made a run with `added: 0, removed: 0` dump the whole excluded set.
    let mut diagnostics: Vec<_> = classified
        .new
        .into_iter()
        .map(|item| item.entry)
        .chain(resolved)
        .filter(|entry| {
            selected_id.is_none_or(|id| plan.owner_for_project_path(&entry.path) == Some(id))
        })
        .collect();
    diagnostics.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
    Ok(DiagnosticsBaselineOperationResult {
        operation,
        path,
        success: added == 0 && removed == 0,
        added,
        removed,
        unchanged,
        selection: Some(plan.selection),
        partitions_enabled: Some(plan.enabled_partition_ids.len()),
        partitions_unsuppressed: Some(plan.partitions.len() - plan.enabled_partition_ids.len()),
        unsuppressed: Some(unsuppressed),
        skipped_unsuppressed: None,
        diagnostics,
        generation: Some(generation),
        selected_partition: selected,
        partitions,
    })
}

fn scope(scope: &project_model::DiagnosticsBaselineProjectScope) -> DiagnosticsBaselineScope {
    DiagnosticsBaselineScope {
        source_root: scope.source_root.clone(),
        extensions: scope.extensions.clone(),
    }
}

fn candidates<'a, T: Clone + 'a>(
    project: &project_model::Project,
    files: impl IntoIterator<Item = (T, &'a FileAnalysis)>,
) -> Result<Vec<Candidate<T>>, Box<dyn Error + Send + Sync>> {
    #[cfg(test)]
    CANDIDATE_PREPARATIONS.with(|count| count.set(count.get() + 1));
    let root = project.root.canonicalize()?;
    let mut candidates = Vec::new();
    for (file_id, file) in files {
        let path = file
            .path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        for (index, diagnostic) in file.diagnostics.iter().enumerate() {
            let snippet = file.line_snippets.get(index).cloned();
            candidates.push(BaselineDiagnosticCandidate {
                diagnostic: (file_id.clone(), index),
                path: path.clone(),
                code: diagnostic.code.clone(),
                snippet,
                message: diagnostic.message.clone(),
                severity: diagnostic.severity.clone(),
                range: DiagnosticsBaselineRange {
                    start_line: diagnostic.start_line.try_into()?,
                    start_column: diagnostic.start_column.try_into()?,
                    end_line: diagnostic.end_line.try_into()?,
                    end_column: diagnostic.end_column.try_into()?,
                },
            });
        }
    }
    Ok(candidates)
}

#[cfg(test)]
pub fn classify_files(
    project: &project_model::Project,
    files: &mut [Option<FileAnalysis>],
    coverage: DiagnosticsBaselineCoverage,
) -> Result<DiagnosticsBaselineSummary, Box<dyn Error + Send + Sync>> {
    let loaded = ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load(project);
    let root = project.root.canonicalize()?;
    let all_project_files = files
        .iter()
        .flatten()
        .map(|file| {
            file.path
                .strip_prefix(&root)
                .map(|path| path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
        })
        .collect::<Result<_, _>>()?;
    classify_files_with_loaded(project, files, coverage, Some(&all_project_files), &loaded, None)
}

/// Classifies every finding of `files` against the loaded baseline and keeps the active
/// ones, giving each survivor the occurrence the classification gave it.
///
/// `files` hold each analysed file's whole finalized set. Under a scope whose line gate the
/// analysis deferred, `scope_masks` (index-aligned with `files`, and within a file with its
/// diagnostics) says which findings the scope admits; they are dropped only after the
/// classification, together with the known ones, so a finding cut by the scope still takes
/// its place in the numbering. The summary therefore counts the whole classified set, which
/// can exceed what the report shows.
pub fn classify_files_with_loaded(
    project: &project_model::Project,
    files: &mut [Option<FileAnalysis>],
    coverage: DiagnosticsBaselineCoverage,
    all_project_files: Option<&std::collections::BTreeSet<String>>,
    loaded: &ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot,
    scope_masks: Option<&[Option<Vec<bool>>]>,
) -> Result<DiagnosticsBaselineSummary, Box<dyn Error + Send + Sync>> {
    use ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot;
    let (baseline, project_path) = match loaded {
        DiagnosticsBaselineSnapshot::Disabled => return Ok(DiagnosticsBaselineSummary::disabled()),
        DiagnosticsBaselineSnapshot::Ready { baseline, project_path, .. } => {
            (baseline, project_path)
        }
        DiagnosticsBaselineSnapshot::ReadySet { baseline, plan, project_path, .. } => {
            let current = partitioned_candidates(
                project,
                plan,
                files
                    .iter()
                    .enumerate()
                    .filter_map(|(index, file)| file.as_ref().map(|file| (index, file))),
            )?;
            let per_partition_coverage =
                ide::partitioned_diagnostics_baseline::partitioned_coverage(
                    plan,
                    &coverage,
                    all_project_files,
                )?;
            let classified = classify_partitioned_diagnostics(
                baseline,
                plan,
                project_path.clone(),
                current,
                &per_partition_coverage,
            )?;
            let known: std::collections::HashSet<_> = classified
                .known
                .iter()
                .map(|item| (item.diagnostic.0, item.diagnostic.1))
                .collect();
            let occurrences = classified
                .new
                .iter()
                .chain(&classified.known)
                .chain(&classified.unsuppressed)
                .map(|item| ((item.diagnostic.0, item.diagnostic.1), item.entry.occurrence))
                .collect();
            keep_active_files(files, &occurrences, &known, scope_masks);
            return Ok(classified.summary);
        }
        DiagnosticsBaselineSnapshot::Error { detail, .. } => return Err(detail.clone().into()),
    };
    let current = candidates(
        project,
        files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| file.as_ref().map(|file| (index, file))),
    )?;
    let classified = classify_diagnostics(baseline, project_path.clone(), current, &coverage)?;
    let known: std::collections::HashSet<_> =
        classified.known.iter().map(|item| item.diagnostic).collect();
    let occurrences = classified
        .new
        .iter()
        .chain(&classified.known)
        .map(|item| (item.diagnostic, item.entry.occurrence))
        .collect();
    keep_active_files(files, &occurrences, &known, scope_masks);
    Ok(classified.summary)
}

/// Writes the classification's occurrences into `files`, then drops the known findings and
/// those the scope mask refuses in one pass. Both steps address findings by their index in
/// the whole set the classifier saw, which is why neither may run after a removal.
fn keep_active_files(
    files: &mut [Option<FileAnalysis>],
    occurrences: &std::collections::HashMap<(usize, usize), u32>,
    known: &std::collections::HashSet<(usize, usize)>,
    scope_masks: Option<&[Option<Vec<bool>>]>,
) {
    for (file_index, file) in files.iter_mut().enumerate() {
        let Some(analysis) = file else { continue };
        let classified: Option<Vec<u32>> = (0..analysis.diagnostics.len())
            .map(|index| occurrences.get(&(file_index, index)).copied())
            .collect();
        if let Some(classified) = classified {
            analysis.occurrences = classified;
        }
        let mask = scope_masks.and_then(|masks| masks.get(file_index)).and_then(Option::as_ref);
        analysis.retain_findings(|index| {
            !known.contains(&(file_index, index))
                && mask.is_none_or(|mask| mask.get(index).copied().unwrap_or(true))
        });
        if analysis.diagnostics.is_empty() {
            *file = None;
        }
    }
}

// Counts the classifications that went past the early `Disabled` return and prepared
// candidates, so a test tells the fast path from the work by the count, not the clock.
#[cfg(test)]
thread_local! {
    pub(crate) static CANDIDATE_PREPARATIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

fn read_baseline(
    path: &Path,
    scope: &DiagnosticsBaselineScope,
) -> Result<DiagnosticsBaseline, Box<dyn Error + Send + Sync>> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read diagnostics baseline {}: {error}", path.display()))?;
    Ok(parse_diagnostics_baseline(&bytes, scope)?)
}

fn synced_temp(path: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile, std::io::Error> {
    let parent = path.parent().ok_or_else(|| std::io::Error::other("missing parent directory"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    Ok(temp)
}

fn publish_new(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
    publish_new_with(path, bytes, |source, target| std::fs::hard_link(source, target))
}

fn publish_new_with(
    path: &Path,
    bytes: &[u8],
    link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let temp = synced_temp(path, bytes)?;
    link(temp.path(), path).map_err(|error| {
        format!("cannot atomically create diagnostics baseline {}: {error}", path.display())
    })?;
    Ok(())
}

fn replace(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
    replace_with(path, bytes, |temp, target| {
        temp.persist(target)
            .map(|_| ())
            .map_err(|error| std::io::Error::new(error.error.kind(), error.error.to_string()))
    })
}

fn replace_with(
    path: &Path,
    bytes: &[u8],
    persist: impl FnOnce(tempfile::NamedTempFile, &Path) -> std::io::Result<()>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let temp = synced_temp(path, bytes)?;
    persist(temp, path).map_err(|error| {
        format!("cannot atomically replace diagnostics baseline {}: {error}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide::DiagnosticOutput;
    use project_model::{DiagnosticsBaselineConfig, Project, ProjectConfig};

    fn args() -> DiagnosticsBaselineArgs {
        DiagnosticsBaselineArgs {
            source_dir: PathBuf::from("."),
            config: None,
            format: DiagnosticsBaselineOutputFormat::Text,
        }
    }

    fn create_args() -> DiagnosticsBaselineCreateArgs {
        DiagnosticsBaselineCreateArgs { common: args(), partition: None, from_v1: None }
    }

    fn selected_args() -> DiagnosticsBaselineSelectedArgs {
        DiagnosticsBaselineSelectedArgs { common: args(), partition: None }
    }

    fn project(root: &Path, configured: bool) -> Project {
        let mut config = ProjectConfig::default();
        if configured {
            config.diagnostics.baseline = Some(DiagnosticsBaselineConfig {
                path: Some("baseline.json".to_owned()),
                ..Default::default()
            });
        }
        Project::with_config(root, config).unwrap()
    }

    fn file(root: &Path) -> FileAnalysis {
        let path = root.join("module.bsl");
        std::fs::write(&path, "x = 1;\n").unwrap();
        let path = path.canonicalize().unwrap();
        FileAnalysis {
            path: path.clone(),
            relative_path: PathBuf::from("module.bsl"),
            diagnostics: vec![DiagnosticOutput {
                code: "UnusedLocalVariable".to_owned(),
                message: "unused".to_owned(),
                severity: "Warning".to_owned(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 1,
                tags: Vec::new(),
            }],
            line_snippets: vec!["x = 1;".to_owned()],
            occurrences: vec![0],
        }
    }

    fn full() -> super::super::analyze::CoverageProof {
        super::super::analyze::CoverageProof { total: 1, analyzed: 1, ..Default::default() }
    }

    #[test]
    fn diagnostics_baseline_create_reports_text_and_machine_result() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(dir.path(), true);
        let result = apply(
            &project,
            &[file(dir.path())],
            &full(),
            DiagnosticsBaselineCommand::Create(create_args()),
        )
        .unwrap();
        assert_eq!((result.added, result.removed, result.unchanged), (1, 0, 0));
        assert_eq!(result.diagnostics[0].path, "module.bsl");
        assert!(result.to_string().contains("added 1"));
        assert!(result.to_string().contains("UnusedLocalVariable module.bsl:1"));
        assert_eq!(serde_json::to_value(&result).unwrap()["added"], 1);
        assert!(dir.path().join("baseline.json").is_file());
    }

    #[test]
    fn diagnostics_baseline_create_requires_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let error = apply(
            &project(dir.path(), false),
            &[],
            &full(),
            DiagnosticsBaselineCommand::Create(create_args()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("not configured"));
    }

    #[test]
    fn diagnostics_baseline_create_rejects_missing_snippet_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(dir.path(), true);
        let mut file = file(dir.path());
        file.line_snippets.clear();

        let error =
            apply(&project, &[file], &full(), DiagnosticsBaselineCommand::Create(create_args()))
                .unwrap_err();

        assert!(error.to_string().contains("snippet"));
        assert!(!dir.path().join("baseline.json").exists());
    }

    #[test]
    fn diagnostics_baseline_create_never_overwrites_a_race_winner() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("baseline.json");
        publish_new(&target, b"winner").unwrap();
        assert!(publish_new(&target, b"loser").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"winner");
    }

    #[test]
    fn diagnostics_baseline_create_cleans_temp_after_unsupported_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("baseline.json");
        let error = publish_new_with(&target, b"value", |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "hard links unsupported"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("hard links unsupported"));
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    fn create_baseline(dir: &Path) -> (Project, Vec<FileAnalysis>, Vec<u8>) {
        let project = project(dir, true);
        let files = vec![file(dir)];
        apply(&project, &files, &full(), DiagnosticsBaselineCommand::Create(create_args()))
            .unwrap();
        let bytes = std::fs::read(dir.join("baseline.json")).unwrap();
        (project, files, bytes)
    }

    fn assert_check_keeps_bytes(project: &Project, files: &[FileAnalysis], expected_ok: bool) {
        let path = project.root.join("baseline.json");
        let before = std::fs::read(&path).unwrap();
        let result =
            apply(project, files, &full(), DiagnosticsBaselineCommand::Check(selected_args()));
        match result {
            Ok(result) => assert_eq!(result.success, expected_ok, "{result:?}"),
            Err(_) => assert!(!expected_ok),
        }
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn diagnostics_baseline_check_is_read_only_for_every_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let (project, files, valid) = create_baseline(dir.path());
        assert_check_keeps_bytes(&project, &files, true);

        let mut new = files.clone();
        new[0].diagnostics[0].code = "MagicNumber".to_owned();
        assert_check_keeps_bytes(&project, &new, false);
        assert_check_keeps_bytes(&project, &[], false);

        let mut protected = files.clone();
        protected[0].diagnostics[0].code = "UnknownSuppressionCode".to_owned();
        assert_check_keeps_bytes(&project, &protected, false);

        std::fs::write(dir.path().join("baseline.json"), b"not json").unwrap();
        assert_check_keeps_bytes(&project, &files, false);
        assert_ne!(std::fs::read(dir.path().join("baseline.json")).unwrap(), valid);
    }

    #[test]
    fn diagnostics_baseline_update_refreshes_fields_and_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let (project, mut files, _) = create_baseline(dir.path());
        files[0].diagnostics[0].message = "refreshed".to_owned();
        files[0].diagnostics[0].start_line = 7;
        files[0].diagnostics[0].end_line = 7;
        let result =
            apply(&project, &files, &full(), DiagnosticsBaselineCommand::Update(selected_args()))
                .unwrap();
        assert_eq!((result.added, result.removed, result.unchanged), (0, 0, 1));

        let resolved = project.diagnostics_baseline().unwrap().unwrap();
        let baseline = read_baseline(&resolved.path, &scope(&resolved.scope)).unwrap();
        assert_eq!(baseline.diagnostics[0].message, "refreshed");
        assert_eq!(baseline.diagnostics[0].range.start_line, 7);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);

        files[0].diagnostics[0].code = "MagicNumber".to_owned();
        let result =
            apply(&project, &files, &full(), DiagnosticsBaselineCommand::Update(selected_args()))
                .unwrap();
        assert_eq!((result.added, result.removed, result.unchanged), (1, 1, 0));
    }

    #[test]
    fn diagnostics_baseline_update_preserves_old_bytes_when_replace_fails() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("baseline.json");
        std::fs::write(&target, b"old").unwrap();
        let error = replace_with(&target, b"new", |_, _| {
            Err(std::io::Error::other("injected replacement failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected replacement failure"));
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn analyze_diagnostics_baseline_filters_known_after_existing_suppressions() {
        let dir = tempfile::tempdir().unwrap();
        let (project, mut files, _) = create_baseline(dir.path());
        let mut active = files[0].diagnostics[0].clone();
        active.code = "MagicNumber".to_owned();
        active.message = "new".to_owned();
        let mut protected = active.clone();
        protected.code = "SuppressionWithoutCode".to_owned();
        files[0].diagnostics.extend([active, protected]);
        files[0].line_snippets.extend(["y = 2;".to_owned(), "z = 3;".to_owned()]);
        let mut results = vec![Some(files.remove(0))];

        let summary =
            classify_files(&project, &mut results, DiagnosticsBaselineCoverage::Full).unwrap();
        assert_eq!((summary.new, summary.known, summary.resolved), (Some(2), Some(1), Some(0)));
        let active = &results[0].as_ref().unwrap().diagnostics;
        assert_eq!(active.len(), 2);
        assert!(active.iter().any(|diagnostic| diagnostic.code == "MagicNumber"));
        assert!(active.iter().any(|diagnostic| diagnostic.code == "SuppressionWithoutCode"));

        // Diagnostics disabled by project rules or source directives never enter this
        // post-suppression list, so they cannot become baseline entries.
        assert!(!active.iter().any(|diagnostic| diagnostic.code == "DisabledByRule"));
        assert!(!active.iter().any(|diagnostic| diagnostic.code == "SuppressedInSource"));

        let disabled_project = self::project(dir.path(), false);
        let mut untouched = results.clone();
        let disabled =
            classify_files(&disabled_project, &mut untouched, DiagnosticsBaselineCoverage::Full)
                .unwrap();
        assert_eq!(disabled.state, ide::diagnostics_baseline::DiagnosticsBaselineState::Disabled);
        assert_eq!(
            untouched[0].as_ref().unwrap().diagnostics,
            results[0].as_ref().unwrap().diagnostics
        );
    }

    #[test]
    fn analyze_baseline_partial_scope_resolves_only_completed_files() {
        let dir = tempfile::tempdir().unwrap();
        // The project resolves its root through the filesystem, so a root reached by a symlink
        // — every temporary directory on macOS — has to be resolved here too, or the relative
        // paths below are cut against a prefix the project never holds.
        let root = dir.path().canonicalize().unwrap();
        let project = project(&root, true);
        let first = file(&root);
        let mut second = first.clone();
        second.path = root.join("second.bsl");
        second.relative_path = PathBuf::from("second.bsl");
        std::fs::write(&second.path, "x = 1;\n").unwrap();
        apply(
            &project,
            &[first, second],
            &super::super::analyze::CoverageProof { total: 2, analyzed: 2, ..Default::default() },
            DiagnosticsBaselineCommand::Create(create_args()),
        )
        .unwrap();

        let mut no_current_diagnostics = Vec::<Option<FileAnalysis>>::new();
        let summary = classify_files(
            &project,
            &mut no_current_diagnostics,
            DiagnosticsBaselineCoverage::Partial {
                completed_files: std::collections::BTreeSet::from(["module.bsl".to_owned()]),
            },
        )
        .unwrap();
        assert_eq!(summary.state, ide::diagnostics_baseline::DiagnosticsBaselineState::Partial);
        assert_eq!(summary.resolved, Some(1));
    }

    fn partitioned_project(root: &Path) -> Project {
        let mut config = ProjectConfig::default();
        config.diagnostics.baseline = Some(DiagnosticsBaselineConfig {
            directory: Some("baselines".to_owned()),
            ..Default::default()
        });
        Project::with_config(root, config).unwrap()
    }

    /// The counterexample as the analysis hands it over: the first `count` of three
    /// identical self-assignments a, b, c on 0-based lines 1, 2 and 3 of one module.
    fn counterexample(root: &Path, count: usize) -> FileAnalysis {
        let path = root.join("module.bsl");
        std::fs::write(
            &path,
            "Процедура Т()\n    А = А;\n    А = А;\n    А = А;\nКонецПроцедуры\n",
        )
        .unwrap();
        let path = path.canonicalize().unwrap();
        FileAnalysis {
            path,
            relative_path: PathBuf::from("module.bsl"),
            diagnostics: (1..=count)
                .map(|line| DiagnosticOutput {
                    code: "SelfAssign".to_owned(),
                    message: "self".to_owned(),
                    severity: "Warning".to_owned(),
                    start_line: line,
                    start_column: 4,
                    end_line: line,
                    end_column: 9,
                    tags: Vec::new(),
                })
                .collect(),
            line_snippets: vec!["А = А;".to_owned(); count],
            occurrences: (0..count as u32).collect(),
        }
    }

    fn unanalysed(root: &Path) -> FileAnalysis {
        let mut other = file(root);
        let path = root.join("other.bsl");
        std::fs::write(&path, "x = 1;\n").unwrap();
        other.path = path.canonicalize().unwrap();
        other.relative_path = PathBuf::from("other.bsl");
        other
    }

    fn lines_of(files: &[Option<FileAnalysis>]) -> Vec<(usize, u32)> {
        files
            .iter()
            .flatten()
            .flat_map(|file| {
                file.diagnostics.iter().zip(&file.occurrences).map(|(d, o)| (d.start_line, *o))
            })
            .collect()
    }

    /// Under a scope the summary counts the whole classified set of the analysed files —
    /// a and b known, c new — while only c is shown; the run is partial, so the entry of
    /// a file nobody analysed is not resolved. The unscoped full run beside it does
    /// resolve that entry, so the zero is the coverage at work, not an empty fixture.
    #[test]
    fn finality_scoped_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = project(&root, true);
        let proof =
            super::super::analyze::CoverageProof { total: 2, analyzed: 2, ..Default::default() };
        apply(
            &project,
            &[counterexample(&root, 2), unanalysed(&root)],
            &proof,
            DiagnosticsBaselineCommand::Create(create_args()),
        )
        .unwrap();
        let loaded =
            ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load(&project);

        let mut files = vec![Some(counterexample(&root, 3))];
        let masks = vec![Some(vec![false, true, true])];
        let summary = classify_files_with_loaded(
            &project,
            &mut files,
            DiagnosticsBaselineCoverage::Partial { completed_files: Default::default() },
            None,
            &loaded,
            Some(&masks),
        )
        .unwrap();
        assert_eq!((summary.known, summary.new, summary.resolved), (Some(2), Some(1), Some(0)));
        assert!(!summary.complete);
        assert_eq!(lines_of(&files), vec![(3, 2)], "only c is shown, with occurrence 2");

        // A scope of a and b cuts the one new finding: nothing is shown, and the summary
        // still counts c.
        let mut files = vec![Some(counterexample(&root, 3))];
        let masks = vec![Some(vec![true, true, false])];
        let summary = classify_files_with_loaded(
            &project,
            &mut files,
            DiagnosticsBaselineCoverage::Partial { completed_files: Default::default() },
            None,
            &loaded,
            Some(&masks),
        )
        .unwrap();
        assert_eq!((summary.known, summary.new), (Some(2), Some(1)));
        assert!(lines_of(&files).is_empty(), "c is outside the scope: {:?}", lines_of(&files));

        let mut files = vec![Some(counterexample(&root, 3))];
        let full = classify_files_with_loaded(
            &project,
            &mut files,
            DiagnosticsBaselineCoverage::Full,
            None,
            &loaded,
            None,
        )
        .unwrap();
        assert_eq!(full.resolved, Some(1), "control: a full run resolves the unanalysed entry");
    }

    /// `Disabled` returns before preparing candidates — counted, not timed — and leaves the
    /// already scoped input as it is; a snapshot that cannot change the set never defers
    /// the gate. A malformed baseline still fails the run. `Ready` prepares for real.
    #[test]
    fn finality_disabled_keeps_fast_path() {
        use ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let prepared = || CANDIDATE_PREPARATIONS.with(std::cell::Cell::get);

        let disabled_project = project(&root, false);
        let disabled = DiagnosticsBaselineSnapshot::load(&disabled_project);
        assert!(!disabled.affects_diagnostics(), "Disabled must not defer the gate");
        let mut files = vec![Some(counterexample(&root, 2))];
        let before = prepared();
        let summary = classify_files_with_loaded(
            &disabled_project,
            &mut files,
            DiagnosticsBaselineCoverage::Full,
            None,
            &disabled,
            None,
        )
        .unwrap();
        assert_eq!(summary.state, ide::diagnostics_baseline::DiagnosticsBaselineState::Disabled);
        assert_eq!(prepared(), before, "Disabled must not prepare candidates");
        assert_eq!(lines_of(&files), vec![(1, 0), (2, 1)], "the input stays as it was");

        let project = project(&root, true);
        apply(
            &project,
            &[counterexample(&root, 2)],
            &full(),
            DiagnosticsBaselineCommand::Create(create_args()),
        )
        .unwrap();
        let ready = DiagnosticsBaselineSnapshot::load(&project);
        let before = prepared();
        classify_files_with_loaded(
            &project,
            &mut [Some(counterexample(&root, 3))],
            DiagnosticsBaselineCoverage::Full,
            None,
            &ready,
            None,
        )
        .unwrap();
        assert_eq!(prepared(), before + 1, "control: Ready prepares candidates");

        std::fs::write(root.join("baseline.json"), b"not json").unwrap();
        let broken = DiagnosticsBaselineSnapshot::load(&project);
        assert!(
            classify_files_with_loaded(
                &project,
                &mut [Some(counterexample(&root, 3))],
                DiagnosticsBaselineCoverage::Full,
                None,
                &broken,
                None,
            )
            .is_err(),
            "a malformed baseline keeps failing the run"
        );
    }

    /// The baseline files are read, never rewritten, and the identity the classification
    /// gives a survivor is the one of the full input: c keeps occurrence 2 in both forms,
    /// and after a further author-style retain every survivor still pairs its own snippet
    /// and occurrence with its diagnostic.
    #[test]
    fn finality_existing_baseline_and_reporter_identity() {
        for partitioned in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let project =
                if partitioned { partitioned_project(&root) } else { project(&root, true) };
            apply(
                &project,
                &[counterexample(&root, 1)],
                &full(),
                DiagnosticsBaselineCommand::Create(create_args()),
            )
            .unwrap();
            let stored = |root: &Path| -> Vec<(PathBuf, Vec<u8>)> {
                let mut stored: Vec<_> = walkdir::WalkDir::new(root)
                    .into_iter()
                    .flatten()
                    .filter(|entry| entry.file_type().is_file())
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                    .map(|entry| (entry.path().to_owned(), std::fs::read(entry.path()).unwrap()))
                    .collect();
                stored.sort();
                stored
            };
            let before = stored(&root);
            assert!(!before.is_empty(), "partitioned={partitioned}: the baseline must exist");
            let loaded =
                ide_host_core::diagnostics_baseline::DiagnosticsBaselineSnapshot::load(&project);

            // Without the producer's numbers, so the occurrences shown can only be the
            // classification's.
            let unnumbered = || {
                let mut file = counterexample(&root, 3);
                file.occurrences.clear();
                Some(file)
            };
            let mut full_run = vec![unnumbered()];
            classify_files_with_loaded(
                &project,
                &mut full_run,
                DiagnosticsBaselineCoverage::Full,
                None,
                &loaded,
                None,
            )
            .unwrap();
            assert_eq!(lines_of(&full_run), vec![(2, 1), (3, 2)], "partitioned={partitioned}");

            let mut scoped = vec![unnumbered()];
            let masks = vec![Some(vec![true, true, true])];
            classify_files_with_loaded(
                &project,
                &mut scoped,
                DiagnosticsBaselineCoverage::Partial { completed_files: Default::default() },
                None,
                &loaded,
                Some(&masks),
            )
            .unwrap();
            let survivor = scoped[0].as_mut().unwrap();
            survivor.retain_findings(|index| index == 1);
            assert_eq!(lines_of(&scoped), vec![(3, 2)], "partitioned={partitioned}: c keeps 2");
            let survivor = scoped[0].as_ref().unwrap();
            assert_eq!(survivor.line_snippets, vec!["А = А;".to_owned()]);
            assert_eq!(stored(&root), before, "partitioned={partitioned}: no baseline write");
        }
    }
}
