use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use ide::diagnostics_baseline::{
    parse_diagnostics_baseline, DiagnosticsBaseline, DiagnosticsBaselineError,
    DiagnosticsBaselineErrorSummary, DiagnosticsBaselineExtension, DiagnosticsBaselineScope,
    DiagnosticsBaselineState, DiagnosticsBaselineSummary,
};
use ide::partitioned_diagnostics_baseline::{
    load_diagnostics_baseline_set, load_diagnostics_baseline_set_reusing,
    DiagnosticsBaselineManifest, DiagnosticsBaselineSetSnapshot,
    PartitionedDiagnosticsBaselineError,
};

#[derive(Debug, Clone)]
pub enum DiagnosticsBaselineSnapshot {
    Disabled,
    Ready {
        baseline: DiagnosticsBaseline,
        project_path: String,
        path: PathBuf,
        epoch: String,
        ground: BaselineGround,
    },
    ReadySet {
        baseline: std::sync::Arc<DiagnosticsBaselineSetSnapshot>,
        plan: std::sync::Arc<project_model::DiagnosticsBaselinePartitionPlan>,
        project_path: String,
        path: PathBuf,
        epoch: String,
        ground: BaselineGround,
    },
    Error {
        path: Option<PathBuf>,
        /// The baseline path as the project spells it, so an error summary names the
        /// same thing a healthy one does instead of leaking an absolute machine path
        /// into CI artefacts. `None` when the configuration itself failed to resolve.
        project_path: Option<String>,
        observation_paths: Vec<PathBuf>,
        selection: Option<project_model::DiagnosticsBaselineSelection>,
        partitions_enabled: Option<usize>,
        partitions_unsuppressed: Option<usize>,
        code: String,
        detail: String,
        epoch: String,
        errors: Vec<DiagnosticsBaselineErrorSummary>,
        ground: BaselineGround,
        /// An input read as zero bytes. A truncating write passes through exactly
        /// that state between its `open` and its `write`, and nothing in the bytes
        /// tells a file caught there from one that is really empty — only a later
        /// look does. A host announcing errors can hold this one back until then.
        read_empty: bool,
    },
}

impl DiagnosticsBaselineSnapshot {
    /// Whether an input of this snapshot was read as zero bytes — the state a
    /// truncating write passes through, so the error may describe content nobody
    /// saved. Never true of a healthy snapshot.
    pub fn read_empty(&self) -> bool {
        matches!(self, Self::Error { read_empty: true, .. })
    }

    /// Load a baseline, reusing the previous snapshot's objects where they have not moved.
    pub fn load_reusing(project: &project_model::Project, previous: &Self) -> Self {
        settle(project, || Self::load_once_reusing(project, previous))
    }

    /// Load a baseline from scratch.
    pub fn load(project: &project_model::Project) -> Self {
        settle(project, || Self::load_once(project))
    }

    /// The reading of the inputs this snapshot was built from, taken before their
    /// contents were read.
    ///
    /// It is an OUTPUT of the load and never a fresh look at the disk: a reading taken
    /// after the content pairs bytes from before a concurrent write with metadata from
    /// after it, every later comparison then matches, and the write stays invisible for
    /// the life of the process.
    pub fn observation(&self) -> String {
        self.ground().observation()
    }

    /// Whether the inputs read differently now than when this snapshot was built — the
    /// one question a host asks to decide whether to load again.
    ///
    /// Answered over the names this snapshot actually recorded, through the managed
    /// directory's capability handle, so the comparison reaches the same entries the
    /// load reached and follows no link on the way.
    pub fn moved_since_load(&self, project: &project_model::Project) -> bool {
        self.ground().moved(&project.root)
    }

    fn ground(&self) -> &BaselineGround {
        match self {
            Self::Ready { ground, .. }
            | Self::ReadySet { ground, .. }
            | Self::Error { ground, .. } => ground,
            Self::Disabled => &EMPTY_GROUND,
        }
    }

    fn load_once_reusing(project: &project_model::Project, previous: &Self) -> Self {
        let Self::ReadySet { baseline: previous_set, ground, project_path, .. } = previous else {
            return Self::load_once(project);
        };
        // Reuse only while the previous snapshot describes the directory the project now
        // names. Object names are content-addressed, so a moved directory holding a copy
        // matches name for name — and the carried readings would then describe files in
        // the directory left behind while the ground pointed at the new one, a comparison
        // that cannot converge.
        if project
            .diagnostics_baseline()
            .ok()
            .flatten()
            .map(|resolved| resolved.project_path)
            .as_deref()
            != Some(project_path.as_str())
        {
            return Self::load_once(project);
        }
        let Ok(directory) =
            project_model::ManagedBaselineDirectory::open(&project.root, project_path, false)
        else {
            return Self::load_once(project);
        };
        let carried: BTreeMap<String, String> = previous_set
            .partitions
            .values()
            .map(|partition| {
                let file = partition.file.to_string();
                let reading = ground.reading(&file);
                (file, reading)
            })
            .collect();
        let changed: BTreeSet<_> = carried
            .iter()
            .filter(|(file, carried)| **carried != observe(&directory, file))
            .map(|(file, _)| file.clone())
            .collect();
        Self::load_partitioned(project, Some(previous_set), &carried, &changed)
            .unwrap_or_else(|| Self::load_once(project))
    }

    fn load_partitioned(
        project: &project_model::Project,
        previous: Option<&DiagnosticsBaselineSetSnapshot>,
        carried: &BTreeMap<String, String>,
        changed_objects: &BTreeSet<String>,
    ) -> Option<Self> {
        let resolved = project.diagnostics_baseline().ok()??;
        if !matches!(
            resolved.mode,
            project_model::DiagnosticsBaselineProjectMode::Partitioned { .. }
        ) {
            return None;
        }
        let plan = project.diagnostics_baseline_partition_plan().ok()??;
        let directory = project_model::ManagedBaselineDirectory::open(
            &project.root,
            &resolved.project_path,
            false,
        )
        .ok()?;
        let (pre_read, loaded) = observe_then_load(&directory, &plan.enabled_partition_ids, || {
            load_diagnostics_baseline_set_reusing(&directory, &plan, previous, changed_objects)
        });
        let (baseline, stats) = loaded.ok()?;
        let epoch = partitioned_epoch(&baseline, &plan);
        let ground =
            pre_read.ground_for(&resolved.project_path, &baseline, &stats.objects_read, carried);
        Some(Self::ReadySet {
            baseline: std::sync::Arc::new(baseline),
            plan: std::sync::Arc::new(plan),
            project_path: resolved.project_path,
            path: resolved.path,
            epoch,
            ground,
        })
    }

    fn load_once(project: &project_model::Project) -> Self {
        let resolved = match project.diagnostics_baseline() {
            Ok(None) => return Self::Disabled,
            Ok(Some(resolved)) => resolved,
            Err(error) => {
                let detail = error.to_string();
                // The path an error names is only good for the summary: it may be
                // absolute, it may point outside the project, and for a non-UTF-8 name it
                // cannot be spelled at all — while the watch list this feeds insists on a
                // UTF-8 path under the root.
                let path = match &error {
                    project_model::DiagnosticsBaselineProjectError::Symlink(path)
                    | project_model::DiagnosticsBaselineProjectError::NotAFile(path) => {
                        Some(path.clone())
                    }
                    project_model::DiagnosticsBaselineProjectError::CannotResolve { .. }
                    | project_model::DiagnosticsBaselineProjectError::OutsideProject(_)
                    | project_model::DiagnosticsBaselineProjectError::NonUtf8(_)
                    | project_model::DiagnosticsBaselineProjectError::PathCollision(_)
                    | project_model::DiagnosticsBaselineProjectError::InvalidConfig(_)
                    | project_model::DiagnosticsBaselineProjectError::LegacyExtension(_)
                    | project_model::DiagnosticsBaselineProjectError::InvalidGroup(_) => None,
                };
                // The ground, though, comes from the name the CONFIGURATION spells. That
                // name is project-relative by construction and exists whatever the entry
                // behind it turned out to be — absent, a link, or a link pointing out of
                // the project — which is exactly the set of states a repair moves away
                // from. Reading it back from a resolved path cannot work: the path an
                // error carries is the one that failed to resolve.
                let ground = configured_baseline_name(project)
                    .map_or_else(BaselineGround::default, |name| {
                        entry_ground(&project.root, &name)
                    });
                return Self::error_observed(
                    path.clone(),
                    None,
                    path,
                    "invalid_configuration",
                    detail.clone().as_bytes(),
                    detail,
                    ground,
                );
            }
        };
        if matches!(
            resolved.mode,
            project_model::DiagnosticsBaselineProjectMode::Partitioned { .. }
        ) {
            let plan = match project.diagnostics_baseline_partition_plan() {
                Ok(Some(plan)) => plan,
                Ok(None) => unreachable!("partitioned mode has a plan"),
                Err(error) => {
                    let detail = error.to_string();
                    let ground = entry_ground(&project.root, &resolved.project_path);
                    return Self::error(
                        Some(resolved.path),
                        Some(resolved.project_path.clone()),
                        "invalid_configuration",
                        detail.clone().as_bytes(),
                        detail,
                        ground,
                    );
                }
            };
            let directory = match project_model::ManagedBaselineDirectory::open(
                &project.root,
                &resolved.project_path,
                false,
            ) {
                Ok(directory) => directory,
                Err(error) => {
                    let code = if error.kind() == std::io::ErrorKind::NotFound {
                        "missing"
                    } else {
                        "unreadable"
                    };
                    let detail = format!(
                        "cannot open diagnostics baseline directory {}: {error}",
                        resolved.path.display()
                    );
                    let ground = entry_ground(&project.root, &resolved.project_path);
                    return Self::error_observed(
                        Some(resolved.path.clone()),
                        Some(resolved.project_path.clone()),
                        Some(resolved.path),
                        code,
                        detail.clone().as_bytes(),
                        detail,
                        ground,
                    );
                }
            };
            let (pre_read, loaded) =
                observe_then_load(&directory, &plan.enabled_partition_ids, || {
                    load_diagnostics_baseline_set(&directory, &plan)
                });
            return match loaded {
                Ok(baseline) => {
                    let epoch = partitioned_epoch(&baseline, &plan);
                    let read_now: BTreeSet<String> = baseline
                        .partitions
                        .values()
                        .map(|partition| partition.file.to_string())
                        .collect();
                    let ground = pre_read.ground_for(
                        &resolved.project_path,
                        &baseline,
                        &read_now,
                        &BTreeMap::new(),
                    );
                    Self::ReadySet {
                        baseline: std::sync::Arc::new(baseline),
                        plan: std::sync::Arc::new(plan),
                        project_path: resolved.project_path,
                        path: resolved.path,
                        epoch,
                        ground,
                    }
                }
                Err(error) => {
                    let detail = error.to_string();
                    let observed = partitioned_error_observation(
                        &project.root,
                        &resolved.project_path,
                        &plan.enabled_partition_ids,
                    );
                    let ground = pre_read.into_ground(&resolved.project_path);
                    let mut snapshot = Self::error_observed_many(
                        Some(resolved.path),
                        Some(resolved.project_path.clone()),
                        observed.paths,
                        error.info().code,
                        &observed.fingerprint,
                        detail,
                        ground,
                    );
                    let Self::Error {
                        selection,
                        partitions_enabled,
                        partitions_unsuppressed,
                        read_empty,
                        ..
                    } = &mut snapshot
                    else {
                        unreachable!()
                    };
                    *selection = Some(plan.selection);
                    *partitions_enabled = Some(plan.enabled_partition_ids.len());
                    *partitions_unsuppressed =
                        Some(plan.partitions.len() - plan.enabled_partition_ids.len());
                    *read_empty = observed.read_empty;
                    Self::with_partition_errors(snapshot, &error)
                }
            };
        }
        // Taken before the bytes, for the reason the whole module exists: a reading taken
        // after them would describe whatever a concurrent write left behind, and the
        // stale content would then match on every later comparison.
        let ground = entry_ground(&project.root, &resolved.project_path);
        let bytes = match std::fs::read(&resolved.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                let code = if error.kind() == std::io::ErrorKind::NotFound {
                    "missing"
                } else {
                    "unreadable"
                };
                let detail = format!(
                    "cannot read diagnostics baseline {}: {error}",
                    resolved.path.display()
                );
                return Self::error(
                    Some(resolved.path),
                    Some(resolved.project_path.clone()),
                    code,
                    detail.clone().as_bytes(),
                    detail,
                    ground,
                );
            }
        };
        let scope = project_scope(&resolved.scope);
        match parse_diagnostics_baseline(&bytes, &scope) {
            Ok(baseline) => Self::Ready {
                baseline,
                project_path: resolved.project_path,
                path: resolved.path,
                epoch: blake3::hash(&bytes).to_hex().to_string(),
                ground,
            },
            Err(error) => {
                let code = match error {
                    DiagnosticsBaselineError::UnsupportedSchema { .. } => "unsupported_schema",
                    DiagnosticsBaselineError::ScopeMismatch => "scope_mismatch",
                    _ => "invalid_file",
                };
                let detail = error.to_string();
                let mut snapshot = Self::error(
                    Some(resolved.path),
                    Some(resolved.project_path.clone()),
                    code,
                    &bytes,
                    detail,
                    ground,
                );
                let Self::Error { read_empty, .. } = &mut snapshot else { unreachable!() };
                *read_empty = bytes.is_empty();
                snapshot
            }
        }
    }

    fn error(
        path: Option<PathBuf>,
        project_path: Option<String>,
        code: &str,
        bytes: &[u8],
        detail: String,
        ground: BaselineGround,
    ) -> Self {
        Self::error_observed(path.clone(), project_path, path, code, bytes, detail, ground)
    }

    fn error_observed(
        path: Option<PathBuf>,
        project_path: Option<String>,
        observation_path: Option<PathBuf>,
        code: &str,
        bytes: &[u8],
        detail: String,
        ground: BaselineGround,
    ) -> Self {
        let observation_paths = observation_path.iter().cloned().collect();
        Self::error_observed_many(
            path,
            project_path,
            observation_paths,
            code,
            bytes,
            detail,
            ground,
        )
    }

    fn error_observed_many(
        path: Option<PathBuf>,
        project_path: Option<String>,
        observation_paths: Vec<PathBuf>,
        code: &str,
        bytes: &[u8],
        detail: String,
        ground: BaselineGround,
    ) -> Self {
        let mut fingerprint = blake3::Hasher::new();
        fingerprint.update(code.as_bytes());
        fingerprint.update(&[0]);
        fingerprint.update(bytes);
        for observation_path in &observation_paths {
            fingerprint.update(&[0]);
            fingerprint.update(observation_path.to_string_lossy().as_bytes());
        }
        let epoch = fingerprint.finalize().to_hex().to_string();
        Self::Error {
            path,
            project_path,
            observation_paths,
            selection: None,
            partitions_enabled: None,
            partitions_unsuppressed: None,
            code: code.to_owned(),
            detail: detail.clone(),
            epoch: epoch.clone(),
            errors: vec![DiagnosticsBaselineErrorSummary {
                partition_id: None,
                code: code.to_owned(),
                detail,
                epoch,
            }],
            ground,
            read_empty: false,
        }
    }

    fn with_partition_errors(
        mut snapshot: Self,
        error: &PartitionedDiagnosticsBaselineError,
    ) -> Self {
        let Self::Error { epoch, errors, .. } = &mut snapshot else { unreachable!() };
        errors.clear();
        let mut push = |partition_id: Option<String>, code: &str, detail: String| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(epoch.as_bytes());
            hasher.update(&[0]);
            hasher.update(code.as_bytes());
            if let Some(partition_id) = &partition_id {
                hasher.update(&[0]);
                hasher.update(partition_id.as_bytes());
            }
            errors.push(DiagnosticsBaselineErrorSummary {
                partition_id,
                code: code.to_owned(),
                detail,
                epoch: hasher.finalize().to_hex().to_string(),
            });
        };
        match error {
            PartitionedDiagnosticsBaselineError::MissingPartitions { ids, orphan_ids } => {
                for id in ids {
                    push(Some(id.clone()), "missing_partition", format!("missing partition: {id}"));
                }
                for id in orphan_ids {
                    push(Some(id.clone()), "orphan_partition", format!("orphan partition: {id}"));
                }
            }
            PartitionedDiagnosticsBaselineError::OrphanPartitions(ids) => {
                for id in ids {
                    push(Some(id.clone()), "orphan_partition", format!("orphan partition: {id}"));
                }
            }
            _ => {
                let info = error.info();
                push(info.partition_id.map(str::to_owned), info.code, error.to_string());
            }
        }
        snapshot
    }

    pub fn epoch(&self) -> &str {
        match self {
            Self::Disabled => "disabled",
            Self::Ready { epoch, .. }
            | Self::ReadySet { epoch, .. }
            | Self::Error { epoch, .. } => epoch,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Ready { path, .. } | Self::ReadySet { path, .. } => Some(path),
            Self::Error { path, .. } => path.as_deref(),
            Self::Disabled => None,
        }
    }

    /// Whether this snapshot can change a file's active diagnostics. `Disabled` and
    /// `Error` cannot, so a caller may skip everything a classification would need —
    /// notably reading and indexing the file text.
    pub fn affects_diagnostics(&self) -> bool {
        matches!(self, Self::Ready { .. } | Self::ReadySet { .. })
    }

    /// The baseline path as the project spells it — what every summary reports, so an
    /// absolute machine path never reaches a response or a CI artefact.
    pub fn project_path(&self) -> Option<&str> {
        match self {
            Self::Ready { project_path, .. } | Self::ReadySet { project_path, .. } => {
                Some(project_path)
            }
            Self::Error { project_path, .. } => project_path.as_deref(),
            Self::Disabled => None,
        }
    }

    pub fn observation_paths(&self) -> Vec<PathBuf> {
        match self {
            Self::Ready { path, .. } => vec![path.clone()],
            Self::ReadySet { path, baseline, .. } => std::iter::once(path.join("manifest.json"))
                .chain(baseline.partitions.values().map(|partition| path.join(&*partition.file)))
                .collect(),
            Self::Error { path, observation_paths, .. } => {
                if observation_paths.is_empty() {
                    path.iter().cloned().collect()
                } else {
                    observation_paths.clone()
                }
            }
            Self::Disabled => vec![],
        }
    }

    pub fn ready(&self) -> Option<(&DiagnosticsBaseline, &str)> {
        match self {
            Self::Ready { baseline, project_path, .. } => Some((baseline, project_path)),
            _ => None,
        }
    }

    pub fn ready_set(
        &self,
    ) -> Option<(
        &DiagnosticsBaselineSetSnapshot,
        &project_model::DiagnosticsBaselinePartitionPlan,
        &str,
    )> {
        match self {
            Self::ReadySet { baseline, plan, project_path, .. } => {
                Some((baseline, plan, project_path))
            }
            _ => None,
        }
    }

    pub fn error_summary(&self) -> Option<DiagnosticsBaselineSummary> {
        let Self::Error {
            path,
            project_path,
            selection,
            partitions_enabled,
            partitions_unsuppressed,
            code,
            detail,
            errors,
            ..
        } = self
        else {
            return None;
        };
        Some(DiagnosticsBaselineSummary {
            state: DiagnosticsBaselineState::Error,
            selection: *selection,
            partitions_enabled: *partitions_enabled,
            partitions_unsuppressed: *partitions_unsuppressed,
            unsuppressed: None,
            new: None,
            known: None,
            resolved: None,
            path: project_path.clone().or_else(|| path.as_deref().map(normalize_path)),
            schema_version: None,
            manifest_schema_version: None,
            complete: false,
            error_code: Some(code.clone()),
            detail: Some(detail.clone()),
            partitions: vec![],
            errors: errors.clone(),
        })
    }

    pub fn errors(&self) -> &[DiagnosticsBaselineErrorSummary] {
        match self {
            Self::Error { errors, .. } => errors,
            _ => &[],
        }
    }
}

fn project_scope(
    scope: &project_model::DiagnosticsBaselineProjectScope,
) -> DiagnosticsBaselineScope {
    DiagnosticsBaselineScope {
        source_root: scope.source_root.clone(),
        extensions: scope
            .extensions
            .iter()
            .map(|extension| DiagnosticsBaselineExtension {
                name: extension.name.clone(),
                path: extension.path.clone(),
                depends_on: extension.depends_on.clone(),
            })
            .collect(),
    }
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
}

/// Recorded where no reading could be taken for a name at all. It equals no reading
/// [`observe`] can produce, so the next load re-reads the object instead of trusting a
/// record whose age is unknown.
const UNOBSERVED: &str = "unobserved";

/// Recorded for a name the managed directory would not describe — absent, or behind a
/// component it refuses to follow. Equals no reading of a real entry, so the name coming
/// back reads as a change.
const MISSING: &str = "missing";

/// The number of times a load re-reads a baseline that moved while it was being read.
///
/// Bounded because a baseline rewritten in a loop would otherwise hold the load open. On
/// giving up, the snapshot still carries a reading OLDER than the file, which the next
/// comparison reports as moved — the load degrades to a stale answer that announces
/// itself, never to a fresh-looking one that lies.
const SETTLE_ATTEMPTS: usize = 4;

static EMPTY_GROUND: BaselineGround = BaselineGround { directory: None, readings: BTreeMap::new() };

/// The readings a snapshot was built against, kept with the handle they were taken
/// through so the same readings can be taken again the same way.
///
/// Keeping the directory rather than absolute paths is what makes the second reading
/// reach the same entries as the first: a path re-assembled from the project root and a
/// name out of the manifest walks whatever links have appeared under it since.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BaselineGround {
    /// The managed directory the names below live in, as the project spells it.
    /// `None` when the baseline is one entry addressed from the project root.
    directory: Option<String>,
    /// Name inside that directory → the reading taken when its content was read.
    readings: BTreeMap<String, String>,
}

impl BaselineGround {
    fn reading(&self, name: &str) -> String {
        self.readings.get(name).cloned().unwrap_or_else(|| UNOBSERVED.to_owned())
    }

    fn open(
        &self,
        project_root: &Path,
    ) -> std::io::Result<project_model::ManagedBaselineDirectory> {
        match &self.directory {
            Some(directory) => {
                project_model::ManagedBaselineDirectory::open(project_root, directory, false)
            }
            None => project_model::ManagedBaselineDirectory::open_project_root(project_root),
        }
    }

    /// One value standing for every reading, so a host can hold a baseline's state in a
    /// single comparable string.
    fn observation(&self) -> String {
        if self.readings.is_empty() {
            return "disabled".to_owned();
        }
        let mut hasher = blake3::Hasher::new();
        for (name, reading) in &self.readings {
            hasher.update(name.as_bytes());
            hasher.update(&[0]);
            hasher.update(reading.as_bytes());
            hasher.update(&[0]);
        }
        hasher.finalize().to_hex().to_string()
    }

    fn moved(&self, project_root: &Path) -> bool {
        if self.readings.is_empty() {
            return false;
        }
        // A directory that no longer opens took every name in it with it.
        let Ok(directory) = self.open(project_root) else { return true };
        self.readings.iter().any(|(name, recorded)| observe(&directory, name) != *recorded)
    }
}

/// The reading of one managed name, taken through the capability handle.
///
/// Nothing on the way to the entry is followed and the entry itself is described rather
/// than resolved, so a name that has become a link reads differently from the file it
/// replaced instead of reporting on the file it points at.
fn observe(directory: &project_model::ManagedBaselineDirectory, name: &str) -> String {
    let Ok(reading) = directory.entry_reading(name) else { return MISSING.to_owned() };
    let mut hasher = blake3::Hasher::new();
    hasher.update(&reading.len.to_le_bytes());
    hasher.update(&[u8::from(reading.is_regular_file)]);
    match reading.mode {
        Some(mode) => {
            hasher.update(&[1]);
            hasher.update(&mode.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    match reading.modified_nanos {
        Some(nanos) => {
            hasher.update(&[1]);
            hasher.update(&nanos.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    match reading.identity {
        Some((device, inode)) => {
            hasher.update(&[1]);
            hasher.update(&device.to_le_bytes());
            hasher.update(&inode.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// The reading of the one entry the configuration names outright — the file in legacy
/// mode, the directory itself when its contents could not be reached.
///
/// It is the one name the project spells for itself, so it is addressable from the
/// project root's own handle without the manifest having been read.
fn entry_ground(project_root: &Path, project_path: &str) -> BaselineGround {
    let reading = project_model::ManagedBaselineDirectory::open_project_root(project_root)
        .map_or_else(|_| MISSING.to_owned(), |root| observe(&root, project_path));
    BaselineGround {
        directory: None,
        readings: BTreeMap::from([(project_path.to_owned(), reading)]),
    }
}

/// The name the configuration spells for the baseline, whatever it turned out to be.
///
/// A managed name, so it can be read back through the project root's own handle; and it
/// survives every resolution failure, because it is what was asked for rather than what
/// was found.
fn configured_baseline_name(project: &project_model::Project) -> Option<String> {
    let configured = project.config.diagnostics.baseline.as_ref()?;
    configured.path.clone().or_else(|| configured.directory.clone())
}

/// Load until the reading the load produced still describes what is on disk.
///
/// A snapshot is never published with a reading its own loader can already see is stale:
/// the ground moving during a read is exactly the case that used to be absorbed into a
/// reading taken afterwards, and absorbing it is what made the change invisible for the
/// life of the process.
fn settle(
    project: &project_model::Project,
    load: impl Fn() -> DiagnosticsBaselineSnapshot,
) -> DiagnosticsBaselineSnapshot {
    for _ in 1..SETTLE_ATTEMPTS {
        let snapshot = load();
        if !snapshot.moved_since_load(project) {
            return snapshot;
        }
    }
    let snapshot = load();
    if snapshot.moved_since_load(project) {
        tracing::warn!(
            attempts = SETTLE_ATTEMPTS,
            "the diagnostics baseline moved under every read; the snapshot describes an \
             older state than the one on disk and the next comparison will say so"
        );
    }
    snapshot
}

/// Every reading taken before a load read anything: the entry point, then the enabled
/// objects the entry names.
struct PreRead {
    manifest: String,
    objects: BTreeMap<String, String>,
}

impl PreRead {
    /// The ground of a snapshot that failed to load: the entry and whatever objects were
    /// reachable, which is what a later comparison must watch for a repair.
    fn into_ground(mut self, project_path: &str) -> BaselineGround {
        self.objects.insert(MANIFEST.to_owned(), self.manifest);
        BaselineGround { directory: Some(project_path.to_owned()), readings: self.objects }
    }

    /// The ground of a freshly built snapshot.
    ///
    /// A reading must be no younger than the content it describes. For an object read
    /// during this load that is the one taken before the read; for an object carried over
    /// from the previous snapshot it is that snapshot's own reading, taken when its
    /// content was read. Reading the file here instead pairs content from before a
    /// concurrent write with metadata from after it, and every later comparison then
    /// matches — leaving the change invisible for the life of the process.
    fn ground_for(
        &self,
        project_path: &str,
        baseline: &DiagnosticsBaselineSetSnapshot,
        objects_read: &BTreeSet<String>,
        carried: &BTreeMap<String, String>,
    ) -> BaselineGround {
        let mut readings: BTreeMap<String, String> = baseline
            .partitions
            .values()
            .map(|partition| {
                let file = partition.file.to_string();
                let source = if objects_read.contains(&file) { &self.objects } else { carried };
                let reading = source.get(&file).cloned().unwrap_or_else(|| UNOBSERVED.to_owned());
                (file, reading)
            })
            .collect();
        readings.insert(MANIFEST.to_owned(), self.manifest.clone());
        BaselineGround { directory: Some(project_path.to_owned()), readings }
    }
}

/// The name a partitioned baseline's entry point carries inside its managed directory.
const MANIFEST: &str = "manifest.json";

/// Read the objects, then load them, in that order and no other.
///
/// The order is the whole point, so it lives in one place rather than at each call site:
/// a reading taken after the load pairs content from before a concurrent write with
/// metadata from after it, every later comparison then matches, and the change stays
/// invisible for the life of the process.
fn observe_then_load<T>(
    directory: &project_model::ManagedBaselineDirectory,
    enabled_partition_ids: &[String],
    load: impl FnOnce() -> T,
) -> (PreRead, T) {
    let observations = pre_read_observations(directory, enabled_partition_ids);
    (observations, load())
}

/// Read the entry point and every enabled object it lists, before any of their contents
/// are read.
///
/// Every name reaches the filesystem through the managed directory, which refuses
/// absolute paths, `..`, and any link on the way. A dormant partition is skipped for the
/// same reason the loader never opens one: a selective baseline must not touch what it
/// was told to leave alone.
///
/// A name the directory would not describe is recorded all the same, as [`MISSING`]: an
/// absent object is exactly the state a repair moves away from, and a name left out of
/// the record is a name no later comparison can watch. The value is not a reading of
/// anything, so a name that comes back reads as a change.
///
/// An unreadable or unparseable manifest is not a failure to report here: it is the
/// loader's own error to raise, and leaving the objects unrecorded only costs the next
/// load one re-read.
fn pre_read_observations(
    directory: &project_model::ManagedBaselineDirectory,
    enabled_partition_ids: &[String],
) -> PreRead {
    // Before the bytes below, not after them: the manifest decides the composition of
    // everything else, so a reading younger than the bytes it describes would let a
    // replaced manifest match for the life of the process.
    let manifest = observe(directory, MANIFEST);
    let mut pre_read = PreRead { manifest, objects: BTreeMap::new() };
    let Ok(mut file) = directory.open_file(MANIFEST) else {
        return pre_read;
    };
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return pre_read;
    }
    let Ok(manifest) = serde_json::from_slice::<DiagnosticsBaselineManifest>(&bytes) else {
        return pre_read;
    };
    pre_read.objects = manifest
        .partitions
        .into_iter()
        .filter(|entry| enabled_partition_ids.contains(&entry.partition_id))
        .map(|entry| {
            let observation = observe(directory, &entry.file);
            (entry.file, observation)
        })
        .collect();
    pre_read
}

fn partitioned_epoch(
    baseline: &DiagnosticsBaselineSetSnapshot,
    plan: &project_model::DiagnosticsBaselinePartitionPlan,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bsl-analyzer/diagnostics-baseline/effective-epoch/v1\0");
    hasher.update(&baseline.manifest_hash);
    hasher.update(plan.selection_fingerprint.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// What a failed partitioned load saw of its inputs: the names to watch for a repair,
/// a fingerprint of their bytes for the error's epoch, and whether any of them read as
/// zero bytes. One flag for the whole set is enough because a host only HOLDS an
/// announcement on it and looks again: an object caught inside a write beside a really
/// broken one costs that look's delay, never the broken one's announcement.
struct ErrorObservation {
    paths: Vec<PathBuf>,
    fingerprint: [u8; 32],
    read_empty: bool,
}

fn partitioned_error_observation(
    project_root: &Path,
    project_path: &str,
    enabled_partition_ids: &[String],
) -> ErrorObservation {
    let directory = project_root.join(project_path);
    let manifest_path = directory.join("manifest.json");
    let mut hasher = blake3::Hasher::new();
    let mut read_empty = false;
    let Ok(managed) =
        project_model::ManagedBaselineDirectory::open(project_root, project_path, false)
    else {
        return ErrorObservation {
            paths: vec![],
            fingerprint: *hasher.finalize().as_bytes(),
            read_empty,
        };
    };
    let mut paths = vec![manifest_path];
    let Ok(mut manifest_file) = managed.open_file("manifest.json") else {
        return ErrorObservation { paths, fingerprint: *hasher.finalize().as_bytes(), read_empty };
    };
    let mut bytes = Vec::new();
    if manifest_file.read_to_end(&mut bytes).is_err() {
        return ErrorObservation { paths, fingerprint: *hasher.finalize().as_bytes(), read_empty };
    }
    read_empty |= bytes.is_empty();
    hasher.update(&bytes);
    if let Ok(manifest) = serde_json::from_slice::<
        ide::partitioned_diagnostics_baseline::DiagnosticsBaselineManifest,
    >(&bytes)
    {
        let mut buffer = [0u8; 64 * 1024];
        for entry in manifest
            .partitions
            .into_iter()
            .filter(|entry| enabled_partition_ids.contains(&entry.partition_id))
        {
            let Ok(relative) = managed.validated_relative_path(&entry.file) else { continue };
            paths.push(project_root.join(relative));
            let Ok(mut file) = managed.open_file(&entry.file) else { continue };
            hasher.update(entry.file.as_bytes());
            let mut total = 0usize;
            loop {
                match file.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        total += read;
                        hasher.update(&buffer[..read]);
                    }
                }
            }
            read_empty |= total == 0;
        }
    }
    ErrorObservation { paths, fingerprint: *hasher.finalize().as_bytes(), read_empty }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ide::partitioned_diagnostics_baseline::{
        diagnostics_manifest, diagnostics_manifest_json, diagnostics_partition_json,
        partition_object_path, DiagnosticsBaselineManifestEntry,
    };
    use std::io::Write;
    use std::os::unix::fs::symlink;

    fn selective_project(root: &Path) -> project_model::Project {
        std::fs::write(
            root.join("bsl-analyzer.toml"),
            r#"
[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/Ext" }]

[diagnostics.baseline]
directory = "baselines"
include = ["main"]
"#,
        )
        .unwrap();
        let config = project_model::ProjectConfig::load(root).unwrap().unwrap();
        project_model::Project::with_config(root, config).unwrap()
    }

    fn write_selective_set(
        root: &Path,
        plan: &project_model::DiagnosticsBaselinePartitionPlan,
    ) -> ide::partitioned_diagnostics_baseline::DiagnosticsBaselineManifest {
        let directory =
            project_model::ManagedBaselineDirectory::open(root, "baselines", true).unwrap();
        let mut entries = Vec::new();
        for partition in &plan.partitions {
            let bytes = diagnostics_partition_json(partition.identity.clone(), vec![]).unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let file = partition_object_path(&partition.id, &partition.key, &hash).unwrap();
            directory.create_file_new(&file).unwrap().write_all(&bytes).unwrap();
            entries.push(DiagnosticsBaselineManifestEntry {
                partition_id: partition.id.clone(),
                file,
                blake3: hash,
            });
        }
        let manifest = diagnostics_manifest(plan.project_scope_fingerprint.clone(), entries);
        directory
            .create_file_new("manifest.json")
            .unwrap()
            .write_all(&diagnostics_manifest_json(&manifest).unwrap())
            .unwrap();
        manifest
    }

    /// Every summary reports the project's own spelling of the path. An error summary
    /// leaking an absolute path would put the developer's home directory into CI output
    /// and break a consumer that joins the value with the project root.
    #[test]
    fn an_error_summary_reports_the_project_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/cf")).unwrap();
        std::fs::write(dir.path().join("src/cf/Configuration.xml"), "<xml/>").unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\n\n[diagnostics.baseline]\npath = \"baseline.json\"\n",
        )
        .unwrap();
        let config = project_model::ProjectConfig::load(dir.path()).unwrap().unwrap();
        let project = project_model::Project::with_config(dir.path(), config).unwrap();

        std::fs::write(dir.path().join("baseline.json"), b"{broken").unwrap();
        let snapshot = DiagnosticsBaselineSnapshot::load(&project);
        let summary = snapshot.error_summary().expect("a broken file is an error state");
        assert_eq!(summary.path.as_deref(), Some("baseline.json"), "{summary:?}");
    }

    fn selective_stand() -> (tempfile::TempDir, project_model::Project) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/cf")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/cfe/Ext")).unwrap();
        std::fs::write(dir.path().join("src/cf/Configuration.xml"), "<xml/>").unwrap();
        std::fs::write(dir.path().join("src/cfe/Ext/Configuration.xml"), "<xml/>").unwrap();
        let project = selective_project(dir.path());
        (dir, project)
    }

    #[test]
    fn selective_baseline_reload() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let manifest = write_selective_set(dir.path(), &plan);

        let first = DiagnosticsBaselineSnapshot::load(&project);
        let DiagnosticsBaselineSnapshot::ReadySet {
            baseline: first_set,
            ground,
            epoch: first_epoch,
            ..
        } = &first
        else {
            panic!("expected ready selective baseline")
        };
        let main_file = first_set.partitions["main"].file.to_string();
        assert_eq!(
            ground.readings.keys().collect::<Vec<_>>(),
            vec![&MANIFEST.to_owned(), &main_file],
            "the enabled object and the entry point, and nothing else",
        );
        assert_eq!(first.observation_paths().len(), 2);
        let main = first_set.partitions["main"].clone();
        let dormant =
            manifest.partitions.iter().find(|entry| entry.partition_id == "extension:Ext").unwrap();
        assert!(!first
            .observation_paths()
            .contains(&dir.path().join("baselines").join(&dormant.file)));
        std::fs::write(dir.path().join("baselines").join(&dormant.file), b"dormant changed")
            .unwrap();
        assert!(
            !first.moved_since_load(&project),
            "a dormant object is not part of the ground a selective baseline stands on"
        );

        let second = DiagnosticsBaselineSnapshot::load_reusing(&project, &first);
        let DiagnosticsBaselineSnapshot::ReadySet {
            baseline: second_set,
            ground: second_ground,
            epoch: second_epoch,
            ..
        } = second
        else {
            panic!("dormant change must not invalidate selective baseline")
        };
        assert!(std::sync::Arc::ptr_eq(&main, &second_set.partitions["main"]));
        assert_eq!(second_ground.readings.len(), 2);
        assert_eq!(second_epoch, *first_epoch);
    }

    #[test]
    fn selective_loader_never_reads_or_watches_unsuppressed_objects() {
        selective_baseline_reload();
    }

    /// The reading a snapshot publishes describes what that snapshot LOADED. A reading
    /// that went back to the disk would absorb a write it never read, and every later
    /// comparison would then match — the write staying invisible for the life of the
    /// process, which is the whole defect this module exists to prevent.
    #[test]
    fn the_published_reading_is_an_output_of_the_load_not_a_later_look_at_the_disk() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let manifest = write_selective_set(dir.path(), &plan);
        let main = manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();

        let snapshot = DiagnosticsBaselineSnapshot::load(&project);
        let published = snapshot.observation();
        std::fs::write(
            dir.path().join("baselines").join(&main.file),
            b"{rewritten under the same name",
        )
        .unwrap();

        assert_eq!(
            snapshot.observation(),
            published,
            "the reading names the content the snapshot holds, not whatever is on disk now"
        );
    }

    /// The guard covers every enabled object, not only the entry point. An object
    /// rewritten in place under the same name leaves the manifest untouched, so a guard
    /// that compared the entry point alone would see a still baseline and keep answering
    /// from a snapshot that no longer matches the disk.
    #[test]
    fn an_object_rewritten_in_place_moves_the_ground_the_entry_point_alone_would_miss() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let manifest = write_selective_set(dir.path(), &plan);
        let main = manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
        let entry_point = dir.path().join("baselines/manifest.json");
        let entry_before = std::fs::symlink_metadata(&entry_point).unwrap();

        let snapshot = DiagnosticsBaselineSnapshot::load(&project);
        assert!(!snapshot.moved_since_load(&project), "a still baseline reads the same");

        std::fs::write(
            dir.path().join("baselines").join(&main.file),
            b"{rewritten under the same name",
        )
        .unwrap();

        let entry_after = std::fs::symlink_metadata(&entry_point).unwrap();
        assert_eq!(
            (entry_before.len(), entry_before.modified().unwrap()),
            (entry_after.len(), entry_after.modified().unwrap()),
            "the entry point must be untouched, or this stand is not the case it names"
        );
        assert!(
            snapshot.moved_since_load(&project),
            "the object moved, so the ground the snapshot stands on moved with it"
        );
    }

    /// A load never hands back a snapshot whose own reading it can already see is stale.
    /// The write that lands while the content is being read is exactly the case a reading
    /// taken afterwards used to absorb; here the load reads again instead.
    #[test]
    fn a_load_reads_again_when_the_ground_moved_under_it() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let manifest = write_selective_set(dir.path(), &plan);
        let main = manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
        let object = dir.path().join("baselines").join(&main.file);

        let attempts = std::cell::Cell::new(0usize);
        let snapshot = settle(&project, || {
            let snapshot = DiagnosticsBaselineSnapshot::load_once(&project);
            // Stands in for a write landing while the load reads: what the first attempt
            // returns is already behind the disk by the time it returns it.
            if attempts.get() == 0 {
                std::fs::write(&object, b"{written while the load was running").unwrap();
            }
            attempts.set(attempts.get() + 1);
            snapshot
        });

        assert_eq!(attempts.get(), 2, "a load whose ground moved under it must read again");
        assert!(
            !snapshot.moved_since_load(&project),
            "the snapshot that is published describes the file that is actually there"
        );
    }

    /// A baseline rewritten under every read cannot hold the load open, and what comes
    /// back says so: its reading is older than the file, which is what a comparison
    /// reports as moved. The alternative — a reading taken at the end — would look fresh
    /// and be wrong.
    #[test]
    fn a_load_that_never_settles_returns_a_reading_that_admits_it() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let manifest = write_selective_set(dir.path(), &plan);
        let main = manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
        let object = dir.path().join("baselines").join(&main.file);

        let attempts = std::cell::Cell::new(0usize);
        let snapshot = settle(&project, || {
            let snapshot = DiagnosticsBaselineSnapshot::load_once(&project);
            attempts.set(attempts.get() + 1);
            // Each rewrite is a different LENGTH. A same-length rewrite in place moves
            // neither the inode nor, on a platform whose timestamps come from a coarse
            // clock, the modification time — and would read as a file that never moved.
            std::fs::write(&object, "{".repeat(attempts.get())).unwrap();
            snapshot
        });

        assert_eq!(attempts.get(), SETTLE_ATTEMPTS, "the load gives up rather than spinning");
        assert!(
            snapshot.moved_since_load(&project),
            "a snapshot that gave up must not read as though it had settled"
        );
    }

    /// An object the load could not read at all is the state a repair moves away from, so
    /// it has to be part of the ground. Leaving an unreadable name out of the record would
    /// make its return invisible: the manifest never moved, so nothing else reports it, and
    /// the resident would answer from the failed snapshot for the life of the process.
    #[test]
    fn a_restored_object_reaches_the_host() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let manifest = write_selective_set(dir.path(), &plan);
        let main = manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
        let object = dir.path().join("baselines").join(&main.file);
        let bytes = std::fs::read(&object).unwrap();
        std::fs::remove_file(&object).unwrap();

        let broken = DiagnosticsBaselineSnapshot::load(&project);
        assert!(matches!(broken, DiagnosticsBaselineSnapshot::Error { .. }));
        assert!(!broken.moved_since_load(&project), "an absent object stays absent");

        std::fs::write(&object, &bytes).unwrap();

        assert!(broken.moved_since_load(&project), "the repair has to reach the host");
        assert!(DiagnosticsBaselineSnapshot::load(&project).ready_set().is_some());
    }

    /// A managed directory that is a plain file resolves to an error naming that entry,
    /// and putting a real directory there is a repair the host has to see. Recording the
    /// ground only for a link or a wrong type would leave every other resolvable state
    /// answering from the same error for the life of the process.
    #[test]
    fn a_repaired_baseline_directory_moves_the_ground() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/cf")).unwrap();
        std::fs::write(dir.path().join("src/cf/Configuration.xml"), "<xml/>").unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\n\n[diagnostics.baseline]\ndirectory = \"baselines\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("baselines"), b"not a directory").unwrap();
        let config = project_model::ProjectConfig::load(dir.path()).unwrap().unwrap();
        let project = project_model::Project::with_config(dir.path(), config).unwrap();

        let broken = DiagnosticsBaselineSnapshot::load(&project);
        assert!(matches!(broken, DiagnosticsBaselineSnapshot::Error { .. }));
        assert!(!broken.moved_since_load(&project), "an untouched entry reads the same");

        std::fs::remove_file(dir.path().join("baselines")).unwrap();
        std::fs::create_dir(dir.path().join("baselines")).unwrap();

        assert!(broken.moved_since_load(&project), "the repair has to reach the host");
    }

    /// A legacy baseline whose parent directory does not exist yet resolves to an error
    /// naming a path that cannot be canonicalised. Creating the directory and the file is
    /// a repair, and it has to reach the host: nothing else reports it.
    #[test]
    fn a_repaired_legacy_baseline_under_a_new_parent_moves_the_ground() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/cf")).unwrap();
        std::fs::write(dir.path().join("src/cf/Configuration.xml"), "<xml/>").unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\n\n[diagnostics.baseline]\npath = \"state/baseline.json\"\n",
        )
        .unwrap();
        let config = project_model::ProjectConfig::load(dir.path()).unwrap().unwrap();
        let project = project_model::Project::with_config(dir.path(), config).unwrap();

        let broken = DiagnosticsBaselineSnapshot::load(&project);
        assert!(matches!(broken, DiagnosticsBaselineSnapshot::Error { .. }));
        assert!(!broken.moved_since_load(&project), "an absent parent stays absent");

        std::fs::create_dir(dir.path().join("state")).unwrap();
        let baseline = DiagnosticsBaseline {
            schema_version: ide::diagnostics_baseline::DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
            diagnostics: vec![],
        };
        std::fs::write(
            dir.path().join("state/baseline.json"),
            ide::diagnostics_baseline::diagnostics_baseline_json(&baseline).unwrap(),
        )
        .unwrap();

        assert!(broken.moved_since_load(&project), "the repair has to reach the host");
    }

    /// A baseline behind a link that leaves the project resolves to an error naming a
    /// path outside it — from which the project's own name for the entry cannot be read
    /// back. The configuration still names it, and replacing the link with a real
    /// directory is a repair the host has to see.
    #[test]
    fn a_repaired_baseline_behind_an_escaping_link_moves_the_ground() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/cf")).unwrap();
        std::fs::write(dir.path().join("src/cf/Configuration.xml"), "<xml/>").unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\n\n[diagnostics.baseline]\npath = \"state/baseline.json\"\n",
        )
        .unwrap();
        std::fs::write(outside.path().join("baseline.json"), "{}").unwrap();
        symlink(outside.path(), dir.path().join("state")).unwrap();
        let config = project_model::ProjectConfig::load(dir.path()).unwrap().unwrap();
        let project = project_model::Project::with_config(dir.path(), config).unwrap();

        let broken = DiagnosticsBaselineSnapshot::load(&project);
        assert!(matches!(broken, DiagnosticsBaselineSnapshot::Error { .. }));
        assert!(!broken.moved_since_load(&project), "an untouched link reads the same");

        std::fs::remove_file(dir.path().join("state")).unwrap();
        std::fs::create_dir(dir.path().join("state")).unwrap();
        std::fs::write(dir.path().join("state/baseline.json"), "{}").unwrap();

        assert!(broken.moved_since_load(&project), "the repair has to reach the host");
    }

    /// An entry that could not be opened for its mode reads the same as one that could on
    /// every other field. Granting access is a repair, and for a snapshot that failed the
    /// reading is the only thing that can report one.
    #[cfg(unix)]
    #[test]
    fn granting_access_to_the_baseline_moves_the_ground() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/cf")).unwrap();
        std::fs::write(dir.path().join("src/cf/Configuration.xml"), "<xml/>").unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\n\n[diagnostics.baseline]\npath = \"baseline.json\"\n",
        )
        .unwrap();
        let baseline = dir.path().join("baseline.json");
        std::fs::write(&baseline, "{}").unwrap();
        std::fs::set_permissions(&baseline, std::fs::Permissions::from_mode(0o000)).unwrap();
        let config = project_model::ProjectConfig::load(dir.path()).unwrap().unwrap();
        let project = project_model::Project::with_config(dir.path(), config).unwrap();

        let unreadable = DiagnosticsBaselineSnapshot::load(&project);
        assert!(!unreadable.moved_since_load(&project), "an untouched file reads the same");

        std::fs::set_permissions(&baseline, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(unreadable.moved_since_load(&project), "the repair has to reach the host");
    }

    /// Object names are content-addressed, so a baseline directory copied elsewhere
    /// matches the previous snapshot name for name. Reusing across that move would pair
    /// readings taken in the directory left behind with a ground pointing at the new one
    /// — a comparison that reports a move on every read and never converges.
    #[test]
    fn a_moved_baseline_directory_is_not_reused_across_the_move() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        write_selective_set(dir.path(), &plan);
        let first = DiagnosticsBaselineSnapshot::load(&project);
        assert!(first.ready_set().is_some());

        let moved = dir.path().join("baselines-moved");
        std::fs::create_dir_all(&moved).unwrap();
        copy_tree(&dir.path().join("baselines"), &moved);
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            r#"
[source]
root = "src/cf"
extensions = [{ name = "Ext", path = "src/cfe/Ext" }]

[diagnostics.baseline]
directory = "baselines-moved"
include = ["main"]
"#,
        )
        .unwrap();
        let config = project_model::ProjectConfig::load(dir.path()).unwrap().unwrap();
        let project = project_model::Project::with_config(dir.path(), config).unwrap();

        let second = DiagnosticsBaselineSnapshot::load_reusing(&project, &first);

        assert!(second.ready_set().is_some());
        assert!(
            !second.moved_since_load(&project),
            "the snapshot must describe the directory it now stands in"
        );
    }

    fn copy_tree(from: &Path, to: &Path) {
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    #[test]
    fn a_recorded_reading_is_never_younger_than_the_content_it_describes() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        write_selective_set(dir.path(), &plan);
        let directory =
            project_model::ManagedBaselineDirectory::open(dir.path(), "baselines", false).unwrap();

        let snapshot = DiagnosticsBaselineSnapshot::load(&project);
        let DiagnosticsBaselineSnapshot::ReadySet { baseline, .. } = &snapshot else {
            panic!("expected ready selective baseline")
        };
        let file = baseline.partitions["main"].file.to_string();
        let carried = BTreeMap::from([(file.clone(), "when-the-content-was-read".to_owned())]);
        let pre_read = PreRead {
            manifest: "the-entry-point".to_owned(),
            objects: BTreeMap::from([(file.clone(), "before-this-load".to_owned())]),
        };

        // Carried over: the content is the previous snapshot's, and so is its reading. A
        // reading taken now would describe a file this snapshot never read.
        let reused = pre_read.ground_for("baselines", baseline, &BTreeSet::new(), &carried);
        assert_eq!(reused.readings[&file], "when-the-content-was-read");

        // Read during this load: the reading taken before the file was opened, so a write
        // racing the read leaves the record older than the file rather than newer.
        let read_now = BTreeSet::from([file.clone()]);
        let reread = pre_read.ground_for("baselines", baseline, &read_now, &carried);
        assert_eq!(reread.readings[&file], "before-this-load");

        // Neither source has one: nothing here may be trusted, so the next load re-reads.
        let empty = PreRead { manifest: "the-entry-point".to_owned(), objects: BTreeMap::new() };
        let unknown = empty.ground_for("baselines", baseline, &BTreeSet::new(), &BTreeMap::new());
        assert_eq!(unknown.readings[&file], UNOBSERVED);
        assert_ne!(observe(&directory, &file), UNOBSERVED);
    }

    #[test]
    fn the_entry_point_reading_moves_when_the_manifest_is_replaced() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        write_selective_set(dir.path(), &plan);

        let snapshot = DiagnosticsBaselineSnapshot::load(&project);
        assert!(!snapshot.moved_since_load(&project), "a still file reads the same");

        // What a host has to be able to see: the manifest replaced under it, atomically,
        // exactly as the writer replaces it.
        let manifest = dir.path().join("baselines/manifest.json");
        let bytes = std::fs::read(&manifest).unwrap();
        let temp = dir.path().join("baselines/manifest.next.json");
        std::fs::write(&temp, [bytes.as_slice(), b"\n"].concat()).unwrap();
        std::fs::rename(&temp, &manifest).unwrap();

        assert!(
            snapshot.moved_since_load(&project),
            "a replaced manifest must read differently, or the guard built on this cannot fire"
        );
    }

    #[test]
    fn the_reading_is_taken_before_the_load_runs() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let manifest = write_selective_set(dir.path(), &plan);
        let directory =
            project_model::ManagedBaselineDirectory::open(dir.path(), "baselines", false).unwrap();
        let main = manifest.partitions.iter().find(|entry| entry.partition_id == "main").unwrap();
        let object = dir.path().join("baselines").join(&main.file);
        let before_the_load = observe(&directory, &main.file);

        // The closure stands in for a write that lands while the load is reading: what the
        // load returns describes the file as it was, so the recorded reading must too.
        let (observed, ()) = observe_then_load(&directory, &plan.enabled_partition_ids, || {
            std::fs::write(&object, b"{written while the load was running").unwrap();
        });

        assert_eq!(
            observed.objects[&main.file], before_the_load,
            "the reading describes the file the load saw, not the one the write left"
        );
        assert_ne!(
            observed.objects[&main.file],
            observe(&directory, &main.file),
            "a reading equal to the file as it is now would make the write invisible for good"
        );
    }

    #[test]
    fn the_pre_read_stays_inside_the_managed_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("baselines")).unwrap();
        let directory =
            project_model::ManagedBaselineDirectory::open(dir.path(), "baselines", true).unwrap();
        let enabled = ["main".to_owned()];
        for escape in ["/etc/passwd", "../../../../etc/passwd"] {
            std::fs::write(
                dir.path().join("baselines/manifest.json"),
                format!(
                    r#"{{"schema_version":1,"generation":"x","project_scope_fingerprint":"x","partitions":[{{"partition_id":"main","file":"{escape}","blake3":"x"}}]}}"#
                ),
            )
            .unwrap();
            assert_eq!(
                pre_read_observations(&directory, &enabled).objects[escape],
                MISSING,
                "a name the managed directory rejects must never reach the filesystem: {escape}"
            );
        }
    }

    /// The manifest's names are untrusted, and the lexical check they pass says nothing
    /// about what the path WALKS. Reading an object by re-assembling an absolute path and
    /// stat'ing that follows every link planted between the managed directory and the
    /// name, which is exactly the traversal the capability handle exists to refuse.
    #[test]
    fn the_pre_read_does_not_follow_a_link_on_the_way_to_an_object() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("baselines/objects")).unwrap();
        std::fs::write(outside.path().join("stolen.json"), b"outside this project").unwrap();
        symlink(outside.path(), dir.path().join("baselines/objects/ab")).unwrap();
        std::fs::write(
            dir.path().join("baselines/manifest.json"),
            r#"{"schema_version":1,"generation":"x","project_scope_fingerprint":"x","partitions":[{"partition_id":"main","file":"objects/ab/stolen.json","blake3":"x"}]}"#,
        )
        .unwrap();
        let directory =
            project_model::ManagedBaselineDirectory::open(dir.path(), "baselines", false).unwrap();

        let observed = pre_read_observations(&directory, &["main".to_owned()]);

        assert_eq!(
            observed.objects["objects/ab/stolen.json"], MISSING,
            "the name is lexically valid, so only the handle can refuse the link it walks"
        );
    }

    #[test]
    fn the_pre_read_leaves_dormant_partitions_alone() {
        let (dir, project) = selective_stand();
        let plan = project.diagnostics_baseline_partition_plan().unwrap().unwrap();
        let manifest = write_selective_set(dir.path(), &plan);
        let directory =
            project_model::ManagedBaselineDirectory::open(dir.path(), "baselines", false).unwrap();

        let observed = pre_read_observations(&directory, &plan.enabled_partition_ids);

        for entry in &manifest.partitions {
            let enabled = plan.enabled_partition_ids.contains(&entry.partition_id);
            assert_eq!(
                observed.objects.contains_key(&entry.file),
                enabled,
                "the selective boundary holds before the read as it does during it: {}",
                entry.partition_id
            );
        }
        assert!(!observed.objects.is_empty(), "the enabled partition is observed");
    }

    #[test]
    fn partitioned_error_observation_rejects_manifest_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("baselines")).unwrap();
        std::fs::write(
            dir.path().join("baselines/manifest.json"),
            r#"{"schema_version":1,"generation":"x","project_scope_fingerprint":"x","partitions":[{"partition_id":"main","file":"../../outside","blake3":"x"}]}"#,
        )
        .unwrap();
        assert_eq!(
            partitioned_error_observation(dir.path(), "baselines", &[]).paths,
            vec![dir.path().join("baselines/manifest.json")]
        );
    }

    #[test]
    fn partitioned_error_observation_never_enters_symlinked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("manifest.json"), b"secret").unwrap();
        symlink(outside.path(), dir.path().join("baselines")).unwrap();
        assert!(partitioned_error_observation(dir.path(), "baselines", &[]).paths.is_empty());
    }

    #[test]
    fn partitioned_error_observation_hashes_manifest_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("baselines")).unwrap();
        let manifest = dir.path().join("baselines/manifest.json");
        std::fs::write(&manifest, b"broken-a").unwrap();
        let first = partitioned_error_observation(dir.path(), "baselines", &[]).fingerprint;
        std::fs::write(&manifest, b"broken-b").unwrap();
        assert_ne!(partitioned_error_observation(dir.path(), "baselines", &[]).fingerprint, first);
    }

    #[test]
    fn partitioned_error_summary_preserves_deterministic_error_count() {
        let base = DiagnosticsBaselineSnapshot::error_observed_many(
            None,
            None,
            vec![],
            "missing_partition",
            b"missing",
            "missing partitions".to_owned(),
            BaselineGround::default(),
        );
        let snapshot = DiagnosticsBaselineSnapshot::with_partition_errors(
            base,
            &PartitionedDiagnosticsBaselineError::MissingPartitions {
                ids: vec!["extension:A".to_owned(), "extension:B".to_owned()],
                orphan_ids: vec!["extension:Old".to_owned()],
            },
        );
        let summary = snapshot.error_summary().unwrap();
        assert_eq!(summary.errors.len(), 3);
        assert_eq!(summary.errors[0].code, "missing_partition");
        assert_eq!(summary.errors[2].code, "orphan_partition");
    }

    /// A baseline that could not be resolved at all still has to be watched: the ground
    /// it stands on is the entry the configuration names, and a repair of that entry has
    /// to reach the host that refused it.
    #[test]
    fn a_broken_entry_is_still_ground_a_repair_moves() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.json"), "{}").unwrap();
        symlink("real.json", dir.path().join("baseline.json")).unwrap();
        let config_path = dir.path().join("bsl-analyzer.json");
        std::fs::write(
            &config_path,
            r#"{"diagnostics":{"baseline":{"path":"baseline.json"}},"extensions":[]}"#,
        )
        .unwrap();
        let config = project_model::ProjectConfig::load_from_file(&config_path).unwrap();
        let project = project_model::Project::with_config(dir.path(), config).unwrap();
        let broken = DiagnosticsBaselineSnapshot::load(&project);
        assert!(matches!(broken, DiagnosticsBaselineSnapshot::Error { .. }));
        assert!(!broken.moved_since_load(&project), "an untouched link reads the same");

        std::fs::remove_file(dir.path().join("baseline.json")).unwrap();
        let baseline = DiagnosticsBaseline {
            schema_version: ide::diagnostics_baseline::DIAGNOSTICS_BASELINE_SCHEMA_VERSION,
            scope: DiagnosticsBaselineScope { source_root: None, extensions: vec![] },
            diagnostics: vec![],
        };
        std::fs::write(
            dir.path().join("baseline.json"),
            ide::diagnostics_baseline::diagnostics_baseline_json(&baseline).unwrap(),
        )
        .unwrap();

        assert!(broken.moved_since_load(&project), "the repair has to reach the host");
        assert!(matches!(
            DiagnosticsBaselineSnapshot::load(&project),
            DiagnosticsBaselineSnapshot::Ready { .. }
        ));
    }
}
