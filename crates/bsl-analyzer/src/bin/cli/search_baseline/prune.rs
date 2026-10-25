use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fmt::{self, Write as _},
    io,
    ops::ControlFlow,
};

use chrono::{DateTime, Utc};

use super::{
    postgres,
    retention::{self, SnapshotRetentionStatus},
    SearchBaselineAdminPruneArgs,
};

type CliResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub(super) fn run(args: SearchBaselineAdminPruneArgs) -> CliResult<()> {
    let project = project_model::Project::new(&args.source_dir)?;
    project.warn_about_external_discovery_rejections();
    let policy = &project.config.search.baseline.workspace_code.policy;
    if !policy.is_configured() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace baseline policy is not configured in .bsl-analyzer.json",
        )
        .into());
    }
    let adapter = postgres::build_project_adapter_for_project(
        &project,
        project_model::PostgresAccessMode::Migrator,
    )?;
    perform(
        &adapter,
        policy,
        Utc::now(),
        args.execute,
        |text| print!("{text}"),
        |_| ControlFlow::Continue(()),
    )
}

/// One committed step of `--execute`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PruneStep {
    MadeStandalone { snapshot_id: String, inherited_files: usize },
    Deleted { snapshot_id: String },
    Skipped { snapshot_id: String, why: SkipReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkipReason {
    AlreadyStandalone,
    Missing,
    BecameHead,
}

/// Everything the dry-run prints, read from the database by `prepare`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PruneReport {
    /// Retention window in days, vendor heads kept, minimum snapshots per branch.
    pub(super) policy: (u32, usize, usize),
    pub(super) catalog_snapshots: usize,
    pub(super) plan: PrunePlan,
    /// File rows each snapshot of `flatten` gains when it is made standalone.
    pub(super) added_rows: BTreeMap<String, u64>,
    pub(super) footprint: bsl_search::BaselineServingFootprint,
    pub(super) estimate: PruneEstimate,
    pub(super) parent_link_enforced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TableEstimate {
    pub(super) table: String,
    pub(super) rows_deleted: u64,
    pub(super) total_rows: u64,
    pub(super) total_bytes: u64,
    pub(super) reclaim_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PruneEstimate {
    pub(super) tables: Vec<TableEstimate>,
    pub(super) reclaim_bytes: u64,
    pub(super) snapshot_files_growth: u64,
}

/// What `--execute` got through: its committed steps, and why it stopped early if it did.
#[derive(Debug)]
pub(super) struct PruneExecution {
    pub(super) steps: Vec<PruneStep>,
    pub(super) error: Option<bsl_search::SearchError>,
    /// `on_step` asked to stop.
    pub(super) stopped: bool,
}

/// Runs `admin prune`: the dry-run prints the plan and changes nothing; `--execute` prints the
/// plan it is about to carry out, then a line per committed step, then the summary.
///
/// The plan of `--execute` is built anew from the database and `now`, so a snapshot that crossed
/// the window's edge since the dry-run is in it — which is why it is printed before the first
/// step. Without the parent link nothing is planned or changed: a delete could then leave a
/// child pointing at nothing.
pub(super) fn perform(
    adapter: &bsl_search::ExternalBaselineAdapter,
    policy: &project_model::SearchBaselinePolicyConfig,
    now: DateTime<Utc>,
    execute: bool,
    mut out: impl FnMut(&str),
    mut on_step: impl FnMut(&PruneStep) -> ControlFlow<()>,
) -> CliResult<()> {
    if !execute {
        out(&render_report(&prepare(adapter, policy, now)?, false));
        return Ok(());
    }
    if !adapter.parent_link_enforced()? {
        return Err(bsl_search::SearchError::ExternalBaseline(
            "parent_link_not_enforced: the parent link is not enforced on this schema; run \
             `search baseline admin migrate` with this build first; nothing was changed"
                .into(),
        )
        .into());
    }
    let report = prepare(adapter, policy, now)?;
    out(&render_report(&report, true));
    let execution = self::execute(adapter, &report.plan, |step| {
        out(&render_step(step));
        on_step(step)
    });
    out(&render_summary(&execution));
    outcome(execution)
}

/// Carries out a plan step by step, each step its own transaction: every snapshot is made
/// standalone before anything is deleted, then deletions go children first. Every committed
/// state is therefore consistent, and a rerun plans what is left from the database.
///
/// A snapshot gone, already standalone, or become a head since planning is skipped. The first
/// error stops the run: its step changed nothing, and a later step may depend on it.
pub(super) fn execute(
    adapter: &bsl_search::ExternalBaselineAdapter,
    plan: &PrunePlan,
    mut on_step: impl FnMut(&PruneStep) -> ControlFlow<()>,
) -> PruneExecution {
    let mut execution = PruneExecution { steps: Vec::new(), error: None, stopped: false };
    let flattens = plan.flatten.iter().map(|snapshot_id| {
        adapter.flatten_snapshot(snapshot_id).map(|outcome| match outcome {
            bsl_search::FlattenOutcome::Flattened { inherited_files } => {
                PruneStep::MadeStandalone { snapshot_id: snapshot_id.clone(), inherited_files }
            }
            bsl_search::FlattenOutcome::AlreadyStandalone => PruneStep::Skipped {
                snapshot_id: snapshot_id.clone(),
                why: SkipReason::AlreadyStandalone,
            },
            bsl_search::FlattenOutcome::Missing => {
                PruneStep::Skipped { snapshot_id: snapshot_id.clone(), why: SkipReason::Missing }
            }
        })
    });
    let deletes = plan.delete.iter().map(|entry| {
        let snapshot_id = entry.snapshot_id.clone();
        adapter.delete_snapshot(&snapshot_id).map(|outcome| match outcome {
            bsl_search::DeleteOutcome::Deleted => PruneStep::Deleted { snapshot_id },
            bsl_search::DeleteOutcome::IsHead => {
                PruneStep::Skipped { snapshot_id, why: SkipReason::BecameHead }
            }
            bsl_search::DeleteOutcome::Missing => {
                PruneStep::Skipped { snapshot_id, why: SkipReason::Missing }
            }
        })
    });
    for step in flattens.chain(deletes) {
        match step {
            Ok(step) => {
                let flow = on_step(&step);
                execution.steps.push(step);
                if flow.is_break() {
                    execution.stopped = true;
                    break;
                }
            }
            Err(error) => {
                execution.error = Some(error);
                break;
            }
        }
    }
    execution
}

pub(super) fn render_step(step: &PruneStep) -> String {
    match step {
        PruneStep::MadeStandalone { snapshot_id, inherited_files } => {
            format!("made standalone {snapshot_id} (+{inherited_files} file rows)\n")
        }
        PruneStep::Deleted { snapshot_id } => format!("deleted {snapshot_id}\n"),
        PruneStep::Skipped { snapshot_id, why } => {
            let why = match why {
                SkipReason::AlreadyStandalone => "already standalone",
                SkipReason::Missing => "missing",
                SkipReason::BecameHead => "became a branch head",
            };
            format!("skipped {snapshot_id}: {why}\n")
        }
    }
}

pub(super) fn render_summary(execution: &PruneExecution) -> String {
    let count =
        |wanted: fn(&PruneStep) -> bool| execution.steps.iter().filter(|step| wanted(step)).count();
    let mut out = format!(
        "Made standalone: {}, deleted: {}, skipped: {}\n",
        count(|step| matches!(step, PruneStep::MadeStandalone { .. })),
        count(|step| matches!(step, PruneStep::Deleted { .. })),
        count(|step| matches!(step, PruneStep::Skipped { .. })),
    );
    if let Some(error) = &execution.error {
        let _ = writeln!(out, "Error: {error}");
    } else if !execution.stopped {
        out.push_str(
            "Next: search baseline admin gc (dry-run, then --execute), then VACUUM as described \
             in docs/central-postgres-search/09-operations.md\n",
        );
    }
    out
}

/// The process outcome: an execution that met an error fails, whatever it got through first.
pub(super) fn outcome(execution: PruneExecution) -> CliResult<()> {
    match execution.error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

/// Reads the catalogue, plans, and measures — reads only.
///
/// A snapshot of `flatten` that has vanished since the catalogue was read counts zero added rows;
/// execution skips it as missing.
pub(super) fn prepare(
    adapter: &bsl_search::ExternalBaselineAdapter,
    policy: &project_model::SearchBaselinePolicyConfig,
    now: DateTime<Utc>,
) -> CliResult<PruneReport> {
    let catalog = adapter.snapshot_catalog()?;
    let plan = plan(policy, &catalog, now)?;
    let footprint = adapter.serving_footprint()?;
    let own_rows = footprint.tables.iter().find(|table| table.table == "snapshot_files");
    let mut added_rows = BTreeMap::new();
    for snapshot_id in &plan.flatten {
        let visible = adapter
            .snapshot_details(snapshot_id)?
            .map(|details| details.snapshot.files as u64)
            .unwrap_or(0);
        let own = own_rows
            .and_then(|table| table.rows_by_snapshot.get(snapshot_id))
            .copied()
            .unwrap_or(0);
        added_rows.insert(snapshot_id.clone(), visible.saturating_sub(own));
    }
    let estimate = estimate(&footprint, &plan, &added_rows);
    let retention = &policy.retention;
    Ok(PruneReport {
        policy: (
            retention.develop_retention_days,
            retention.vendor_keep_heads,
            retention.min_snapshots_per_branch,
        ),
        catalog_snapshots: catalog.snapshots.len(),
        plan,
        added_rows,
        footprint,
        estimate,
        parent_link_enforced: adapter.parent_link_enforced()?,
    })
}

/// Space freed per table as the deleted snapshots' share of its rows times its size.
///
/// The share of rows rather than of snapshots: snapshots differ in size by orders of magnitude.
/// Multiplied before dividing, in `u128`, so the estimate is exact where it can be.
pub(super) fn estimate(
    footprint: &bsl_search::BaselineServingFootprint,
    plan: &PrunePlan,
    added_rows: &BTreeMap<String, u64>,
) -> PruneEstimate {
    let share = |total_bytes: u64, rows: u64, total_rows: u64| -> u64 {
        if total_rows == 0 {
            return 0;
        }
        (u128::from(total_bytes) * u128::from(rows) / u128::from(total_rows)) as u64
    };
    let deleted: HashSet<&str> =
        plan.delete.iter().map(|entry| entry.snapshot_id.as_str()).collect();
    let mut tables = Vec::new();
    let mut snapshot_files_growth = 0;
    for table in &footprint.tables {
        let total_rows = table.total_rows();
        let rows_deleted = table
            .rows_by_snapshot
            .iter()
            .filter(|(snapshot_id, _)| deleted.contains(snapshot_id.as_str()))
            .map(|(_, rows)| rows)
            .sum();
        if table.table == "snapshot_files" {
            snapshot_files_growth = share(table.total_bytes, added_rows.values().sum(), total_rows);
        }
        tables.push(TableEstimate {
            table: table.table.clone(),
            rows_deleted,
            total_rows,
            total_bytes: table.total_bytes,
            reclaim_bytes: share(table.total_bytes, rows_deleted, total_rows),
        });
    }
    PruneEstimate {
        reclaim_bytes: tables.iter().map(|table| table.reclaim_bytes).sum(),
        tables,
        snapshot_files_growth,
    }
}

/// `22020096 bytes (21.0 MiB)`: exact for the operator's arithmetic, readable at a glance.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{bytes} bytes ({value:.1} {})", UNITS[unit])
}

pub(super) fn render_report(report: &PruneReport, execute: bool) -> String {
    let mut out = String::new();
    if execute {
        out.push_str("Shared baseline prune.\n");
    } else {
        out.push_str("Shared baseline prune dry-run.\n");
        out.push_str("  Use --execute to apply.\n");
    }
    let (days, vendor_heads, min_per_branch) = report.policy;
    let plan = &report.plan;
    let _ = writeln!(out, "  Develop retention:          {days} days");
    let _ = writeln!(out, "  Vendor heads kept:          {vendor_heads}");
    let _ = writeln!(out, "  Min snapshots per branch:   {min_per_branch}");
    let _ = writeln!(out, "  Snapshots in catalog:       {}", report.catalog_snapshots);

    let mut by_reason = BTreeMap::<KeepReason, usize>::new();
    for (_, reason) in &plan.kept {
        *by_reason.entry(*reason).or_default() += 1;
    }
    let reasons: Vec<String> =
        by_reason.iter().map(|(reason, count)| format!("{}: {count}", reason.as_str())).collect();
    if reasons.is_empty() {
        let _ = writeln!(out, "  Kept:                       {}", plan.kept.len());
    } else {
        let _ = writeln!(
            out,
            "  Kept:                       {} ({})",
            plan.kept.len(),
            reasons.join(", ")
        );
    }

    let _ = writeln!(out, "  To make standalone:         {}", plan.flatten.len());
    for snapshot_id in &plan.flatten {
        let added = report.added_rows.get(snapshot_id).copied().unwrap_or(0);
        let _ = writeln!(out, "    {snapshot_id} (+{added} file rows)");
    }
    let _ = writeln!(out, "  To delete:                  {}", plan.delete.len());
    for entry in &plan.delete {
        let _ = writeln!(
            out,
            "    {}  {}  {}  {}",
            entry.snapshot_id, entry.branch, entry.created_at, entry.reason
        );
    }

    out.push_str(
        "  Space by table (rows of deleted snapshots / all rows, estimated bytes of total):\n",
    );
    for table in &report.estimate.tables {
        let _ = writeln!(
            out,
            "    {}: {}/{}, ~{} of {}",
            table.table,
            table.rows_deleted,
            table.total_rows,
            format_bytes(table.reclaim_bytes),
            format_bytes(table.total_bytes)
        );
    }
    for table in &report.footprint.missing {
        let _ = writeln!(out, "    {table}: absent");
    }
    let _ = writeln!(
        out,
        "  Estimated reclaim:          {}",
        format_bytes(report.estimate.reclaim_bytes)
    );
    let _ = writeln!(
        out,
        "  snapshot_files growth:      ~{}",
        format_bytes(report.estimate.snapshot_files_growth)
    );
    out.push_str(
        "  Not included: file objects, content objects and embeddings orphaned by the deletion \
         — run `search baseline admin gc` after --execute.\n",
    );
    if !report.parent_link_enforced {
        out.push_str(
            "  Warning: --execute requires `search baseline admin migrate` with this build first.\n",
        );
    }
    out
}

/// Why a snapshot survives compaction, in the order the reasons are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum KeepReason {
    ActiveHead,
    SafetyHead,
    WithinWindow,
    MinimumPreservation,
    BranchHead,
    OutsidePolicy,
}

impl KeepReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ActiveHead => "active-head",
            Self::SafetyHead => "safety-head",
            Self::WithinWindow => "within-window",
            Self::MinimumPreservation => "minimum-preservation",
            Self::BranchHead => "branch-head",
            Self::OutsidePolicy => "outside-policy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PruneDeletion {
    pub(super) snapshot_id: String,
    pub(super) branch: String,
    pub(super) created_at: String,
    pub(super) reason: String,
}

/// What compaction does to a catalogue: which snapshots become standalone, which go, and in
/// which order — every child before its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrunePlan {
    pub(super) kept: Vec<(String, KeepReason)>,
    pub(super) flatten: Vec<String>,
    pub(super) delete: Vec<PruneDeletion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrunePlanError {
    pub(super) problems: Vec<String>,
}

impl fmt::Display for PrunePlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the snapshot catalog cannot be compacted: {}", self.problems.join("; "))
    }
}

impl Error for PrunePlanError {}

/// Decides compaction from the catalogue alone.
///
/// Refuses a damaged catalogue outright rather than planning around the damage: a parent that is
/// not there or a cycle means the chains are not what the plan would assume, and a `created_at`
/// that does not parse makes `analyze` call a snapshot of the window expired.
pub(super) fn plan(
    policy: &project_model::SearchBaselinePolicyConfig,
    catalog: &bsl_search::BaselineSnapshotCatalog,
    now: DateTime<Utc>,
) -> Result<PrunePlan, PrunePlanError> {
    let problems = catalog_problems(catalog);
    if !problems.is_empty() {
        return Err(PrunePlanError { problems });
    }

    let records = retention::records_from_catalog(catalog, None);
    let assessments = retention::analyze(policy, &records, now);
    let heads: HashSet<&str> = catalog.heads.iter().map(|head| head.snapshot_id.as_str()).collect();
    let mut delete = Vec::new();
    let mut kept_by_analysis = HashMap::new();
    for assessment in &assessments {
        let preserved =
            assessment.protections.iter().any(|protection| protection == "minimum-preservation");
        let reason = match assessment.status {
            SnapshotRetentionStatus::ActiveHead => KeepReason::ActiveHead,
            SnapshotRetentionStatus::SafetyHead => KeepReason::SafetyHead,
            SnapshotRetentionStatus::WithinWindow => KeepReason::WithinWindow,
            SnapshotRetentionStatus::ExpiredCandidate if preserved => {
                KeepReason::MinimumPreservation
            }
            SnapshotRetentionStatus::ExpiredCandidate
                if heads.contains(assessment.snapshot_id.as_str()) =>
            {
                KeepReason::BranchHead
            }
            SnapshotRetentionStatus::ExpiredCandidate => {
                delete.push(PruneDeletion {
                    snapshot_id: assessment.snapshot_id.clone(),
                    branch: assessment.branch.clone(),
                    created_at: assessment.created_at.clone(),
                    reason: assessment.reason.clone(),
                });
                continue;
            }
        };
        kept_by_analysis.insert(assessment.snapshot_id.as_str(), reason);
    }

    let deleted: HashSet<&str> = delete.iter().map(|entry| entry.snapshot_id.as_str()).collect();
    let mut kept = Vec::new();
    let mut flatten = Vec::new();
    for snapshot in &catalog.snapshots {
        if deleted.contains(snapshot.snapshot_id.as_str()) {
            continue;
        }
        let reason = kept_by_analysis
            .get(snapshot.snapshot_id.as_str())
            .copied()
            .unwrap_or(KeepReason::OutsidePolicy);
        kept.push((snapshot.snapshot_id.clone(), reason));
        if snapshot.parent_snapshot_id.as_deref().is_some_and(|parent| deleted.contains(parent)) {
            flatten.push(snapshot.snapshot_id.clone());
        }
    }
    kept.sort();
    flatten.sort();

    Ok(PrunePlan { kept, flatten, delete: children_before_parents(catalog, delete) })
}

fn catalog_problems(catalog: &bsl_search::BaselineSnapshotCatalog) -> Vec<String> {
    let parents: HashMap<&str, Option<&str>> = catalog
        .snapshots
        .iter()
        .map(|snapshot| (snapshot.snapshot_id.as_str(), snapshot.parent_snapshot_id.as_deref()))
        .collect();
    let mut problems = Vec::new();
    let mut ids: Vec<&str> = parents.keys().copied().collect();
    ids.sort_unstable();

    for id in &ids {
        if let Some(parent) = parents[id].filter(|parent| !parents.contains_key(parent)) {
            problems.push(format!(
                "snapshot '{id}' names parent '{parent}', which is not in the catalog"
            ));
        }
    }

    // Each chain is walked once; a walk that meets its own path has found a cycle, reported once
    // by its members whichever of them the walk started from.
    let mut settled = HashSet::new();
    let mut cycles = BTreeSet::new();
    for id in &ids {
        let mut path = Vec::new();
        let mut current = Some(*id);
        while let Some(node) = current.filter(|node| !settled.contains(node)) {
            if let Some(start) = path.iter().position(|seen| *seen == node) {
                let mut members: Vec<&str> = path[start..].to_vec();
                members.sort_unstable();
                cycles.insert(members);
                break;
            }
            path.push(node);
            current = parents.get(node).copied().flatten();
        }
        settled.extend(path);
    }
    for members in cycles {
        let members: Vec<String> = members.iter().map(|id| format!("'{id}'")).collect();
        problems.push(format!("the parent chain cycles through {}", members.join(", ")));
    }

    for snapshot in &catalog.snapshots {
        let assessed = snapshot.corpus == bsl_search::CorpusId::WorkspaceCode.as_str()
            && snapshot.branch.as_deref().is_some_and(|branch| !branch.trim().is_empty());
        if assessed && project_model::parse_timestamp_utc(&snapshot.created_at).is_none() {
            problems.push(format!(
                "snapshot '{}' has created_at '{}', which cannot be read as a time",
                snapshot.snapshot_id, snapshot.created_at
            ));
        }
    }
    problems
}

/// Kahn's order over the edges child → parent inside `delete`: a snapshot goes only once none of
/// its children is left to delete, the newest first among those that may go.
fn children_before_parents(
    catalog: &bsl_search::BaselineSnapshotCatalog,
    delete: Vec<PruneDeletion>,
) -> Vec<PruneDeletion> {
    let parent_of: HashMap<&str, &str> = catalog
        .snapshots
        .iter()
        .filter_map(|snapshot| {
            snapshot
                .parent_snapshot_id
                .as_deref()
                .map(|parent| (snapshot.snapshot_id.as_str(), parent))
        })
        .collect();
    let ids: HashSet<String> = delete.iter().map(|entry| entry.snapshot_id.clone()).collect();
    let mut children_left: HashMap<String, usize> = HashMap::new();
    for id in &ids {
        if let Some(parent) = parent_of.get(id.as_str()).filter(|parent| ids.contains(**parent)) {
            *children_left.entry((*parent).to_owned()).or_default() += 1;
        }
    }

    let key = |entry: &PruneDeletion| {
        (Reverse(project_model::parse_timestamp_utc(&entry.created_at)), entry.snapshot_id.clone())
    };
    let mut pending = delete;
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let Some(next) = pending
            .iter()
            .enumerate()
            .filter(|(_, entry)| children_left.get(&entry.snapshot_id).copied().unwrap_or(0) == 0)
            .min_by_key(|(_, entry)| key(entry))
            .map(|(index, _)| index)
        else {
            break;
        };
        let entry = pending.swap_remove(next);
        if let Some(parent) = parent_of.get(entry.snapshot_id.as_str()) {
            if let Some(count) = children_left.get_mut(*parent) {
                *count -= 1;
            }
        }
        ordered.push(entry);
    }
    ordered
}

#[cfg(test)]
mod tests {
    use bsl_search::{BaselineCatalogSnapshot, BaselineHeadRecord, BaselineSnapshotCatalog};
    use chrono::{DateTime, Duration, TimeZone, Utc};

    use super::{
        estimate, execute, outcome, perform, plan, prepare, render_report, render_step,
        render_summary, KeepReason, PruneDeletion, PruneExecution, PrunePlan, PruneReport,
        PruneStep, SkipReason,
    };
    use bsl_search::{
        BaselineRef, BaselineServingFootprint, BaselineTableFootprint, CorpusId,
        ExternalBaselineAdapter, ExternalBaselineConfig, IndexedDocument, SearchError, Snapshot,
        SnapshotCatalog, SnapshotContentStore, SnapshotPublishMetadata, SnapshotPublisher,
    };
    use std::collections::BTreeMap;
    use std::ops::ControlFlow;

    fn policy(min_snapshots_per_branch: usize) -> project_model::SearchBaselinePolicyConfig {
        serde_json::from_value(serde_json::json!({
            "publishBranches": ["vendor", "develop"],
            "branches": [{ "match": "*", "selectBranch": "develop", "fallbackBranch": "vendor" }],
            "retention": {
                "developRetentionDays": 30,
                "vendorKeepHeads": 2,
                "minSnapshotsPerBranch": min_snapshots_per_branch
            }
        }))
        .unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap()
    }

    fn days_ago(days: i64) -> String {
        (now() - Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    }

    fn snapshot(
        id: &str,
        corpus: &str,
        branch: Option<&str>,
        parent: Option<&str>,
        created_at: String,
    ) -> BaselineCatalogSnapshot {
        BaselineCatalogSnapshot {
            snapshot_id: id.to_owned(),
            corpus: corpus.to_owned(),
            branch: branch.map(ToOwned::to_owned),
            parent_snapshot_id: parent.map(ToOwned::to_owned),
            created_at,
        }
    }

    fn develop(id: &str, parent: Option<&str>, age_days: i64) -> BaselineCatalogSnapshot {
        snapshot(id, "workspace-code", Some("develop"), parent, days_ago(age_days))
    }

    fn head(branch: &str, snapshot_id: &str) -> BaselineHeadRecord {
        BaselineHeadRecord {
            corpus: "workspace-code".to_owned(),
            branch: branch.to_owned(),
            snapshot_id: snapshot_id.to_owned(),
        }
    }

    /// `s1←s2←…←s6`: `s1..s3` are older than the window, `s4..s6` inside it, `s6` the newest.
    fn six_link_develop() -> Vec<BaselineCatalogSnapshot> {
        vec![
            develop("s1", None, 60),
            develop("s2", Some("s1"), 50),
            develop("s3", Some("s2"), 40),
            develop("s4", Some("s3"), 20),
            develop("s5", Some("s4"), 10),
            develop("s6", Some("s5"), 1),
        ]
    }

    fn deleted(plan: &PrunePlan) -> Vec<&str> {
        plan.delete.iter().map(|entry| entry.snapshot_id.as_str()).collect()
    }

    fn reason_of(plan: &PrunePlan, id: &str) -> Option<KeepReason> {
        plan.kept.iter().find(|(kept, _)| kept == id).map(|(_, reason)| *reason)
    }

    /// No kept snapshot points at a deleted one unless it is made standalone first.
    fn assert_no_kept_to_deleted_edge(catalog: &BaselineSnapshotCatalog, plan: &PrunePlan) {
        let deleted = deleted(plan);
        for snapshot in &catalog.snapshots {
            if deleted.contains(&snapshot.snapshot_id.as_str()) {
                continue;
            }
            let Some(parent) = snapshot.parent_snapshot_id.as_deref() else { continue };
            assert!(
                !deleted.contains(&parent) || plan.flatten.contains(&snapshot.snapshot_id),
                "{} would be left pointing at deleted {parent}",
                snapshot.snapshot_id
            );
        }
    }

    #[test]
    fn plan_flattens_the_oldest_retained_snapshot() {
        let catalog = BaselineSnapshotCatalog { snapshots: six_link_develop(), heads: vec![] };
        let plan = plan(&policy(1), &catalog, now()).unwrap();

        assert_eq!(plan.flatten, vec!["s4"]);
        assert_eq!(deleted(&plan), vec!["s3", "s2", "s1"]);
        assert_eq!(reason_of(&plan, "s6"), Some(KeepReason::ActiveHead));
        assert_eq!(reason_of(&plan, "s5"), Some(KeepReason::WithinWindow));
        assert_eq!(reason_of(&plan, "s4"), Some(KeepReason::WithinWindow));
        assert_no_kept_to_deleted_edge(&catalog, &plan);
    }

    #[test]
    fn plan_keeps_heads_unanalyzed_and_cross_branch_children() {
        // (b) a head row on the old s2 keeps it, so both s2 and s4 lose their parents.
        let catalog = BaselineSnapshotCatalog {
            snapshots: six_link_develop(),
            heads: vec![head("develop", "s2")],
        };
        let headed = plan(&policy(1), &catalog, now()).unwrap();
        assert_eq!(reason_of(&headed, "s2"), Some(KeepReason::BranchHead));
        assert_eq!(headed.flatten, vec!["s2", "s4"]);
        assert_eq!(deleted(&headed), vec!["s3", "s1"]);
        assert_no_kept_to_deleted_edge(&catalog, &headed);

        // (c) a snapshot without a branch is not the policy's to judge, but its parent still goes.
        let mut snapshots = six_link_develop();
        snapshots.push(snapshot("b", "workspace-code", None, Some("s1"), days_ago(70)));
        let catalog = BaselineSnapshotCatalog { snapshots, heads: vec![] };
        let unbranched = plan(&policy(1), &catalog, now()).unwrap();
        assert_eq!(reason_of(&unbranched, "b"), Some(KeepReason::OutsidePolicy));
        assert_eq!(unbranched.flatten, vec!["b", "s4"]);
        assert_eq!(deleted(&unbranched), vec!["s3", "s2", "s1"]);
        assert_no_kept_to_deleted_edge(&catalog, &unbranched);

        // (d) a develop snapshot whose parent is the third vendor head.
        let vendor = |id: &str, age: i64| {
            snapshot(id, "workspace-code", Some("vendor"), None, days_ago(age))
        };
        let catalog = BaselineSnapshotCatalog {
            snapshots: vec![
                vendor("v1", 5),
                vendor("v2", 15),
                vendor("v3", 25),
                develop("k", Some("v3"), 3),
            ],
            heads: vec![],
        };
        let cross = plan(&policy(1), &catalog, now()).unwrap();
        assert_eq!(deleted(&cross), vec!["v3"]);
        assert_eq!(cross.flatten, vec!["k"]);
        assert_eq!(reason_of(&cross, "v2"), Some(KeepReason::SafetyHead));
        assert_no_kept_to_deleted_edge(&catalog, &cross);
    }

    #[test]
    fn plan_deletes_only_unprotected_expired_candidates() {
        let catalog = BaselineSnapshotCatalog {
            snapshots: vec![
                develop("n", None, 1),
                develop("o2", None, 40),
                develop("o3", None, 50),
                develop("o4", None, 60),
                develop("o5", None, 70),
                snapshot("ref", "reference", Some("develop"), None, days_ago(90)),
                snapshot("nb", "workspace-code", None, None, days_ago(90)),
            ],
            heads: vec![head("develop", "o4")],
        };
        let plan = plan(&policy(3), &catalog, now()).unwrap();

        assert_eq!(deleted(&plan), vec!["o5"], "only the unprotected expired candidate goes");
        assert_eq!(reason_of(&plan, "o2"), Some(KeepReason::MinimumPreservation));
        assert_eq!(reason_of(&plan, "o3"), Some(KeepReason::MinimumPreservation));
        assert_eq!(reason_of(&plan, "o4"), Some(KeepReason::BranchHead));
        assert_eq!(reason_of(&plan, "ref"), Some(KeepReason::OutsidePolicy));
        assert_eq!(reason_of(&plan, "nb"), Some(KeepReason::OutsidePolicy));
        assert_eq!(plan.delete[0].branch, "develop");
        assert!(plan.delete[0].reason.contains("outside branch retention policy"));
    }

    #[test]
    fn plan_deletes_children_before_parents() {
        // The child is OLDER than its parent: ordering by age alone would delete the parent first
        // and trip the parent link.
        let catalog = BaselineSnapshotCatalog {
            snapshots: vec![
                develop("d1", None, 50),
                develop("d2", Some("d1"), 60),
                develop("h", Some("d2"), 1),
            ],
            heads: vec![],
        };
        let plan = plan(&policy(1), &catalog, now()).unwrap();
        assert_eq!(deleted(&plan), vec!["d2", "d1"]);
        assert_eq!(plan.flatten, vec!["h"]);
    }

    #[test]
    fn plan_refuses_a_corrupt_or_unreadable_catalog() {
        let unreadable = "2026-10-16 12:00:00.1+00";
        let catalog = BaselineSnapshotCatalog {
            snapshots: vec![
                develop("orphan", Some("ghost"), 40),
                develop("cyc", Some("cyc"), 40),
                develop("a", Some("b"), 40),
                develop("b", Some("a"), 40),
                develop("fresh", None, 1),
                snapshot("odd", "workspace-code", Some("develop"), None, unreadable.to_owned()),
            ],
            heads: vec![],
        };
        let error = plan(&policy(1), &catalog, now()).expect_err("damage must refuse the plan");
        assert_eq!(error.problems.len(), 4, "{error}");
        for needle in ["'orphan'", "'ghost'", "'cyc'", "'a', 'b'", "'odd'", unreadable] {
            assert!(error.to_string().contains(needle), "{needle} missing from: {error}");
        }

        // Control: the unreadable time on a snapshot of the window is what makes `analyze` call it
        // expired, so a planner without the check would delete it.
        let records = super::retention::records_from_catalog(
            &BaselineSnapshotCatalog {
                snapshots: vec![
                    develop("fresh", None, 1),
                    snapshot("odd", "workspace-code", Some("develop"), None, unreadable.to_owned()),
                ],
                heads: vec![],
            },
            None,
        );
        let odd = super::retention::analyze(&policy(1), &records, now())
            .into_iter()
            .find(|assessment| assessment.snapshot_id == "odd")
            .unwrap();
        assert_eq!(odd.status, super::SnapshotRetentionStatus::ExpiredCandidate);
    }

    fn table(name: &str, total_bytes: u64, rows: &[(&str, u64)]) -> BaselineTableFootprint {
        BaselineTableFootprint {
            table: name.to_owned(),
            total_bytes,
            rows_by_snapshot: rows.iter().map(|(id, count)| ((*id).to_owned(), *count)).collect(),
        }
    }

    fn deletion(id: &str, created_at: &str) -> PruneDeletion {
        PruneDeletion {
            snapshot_id: id.to_owned(),
            branch: "develop".to_owned(),
            created_at: created_at.to_owned(),
            reason: "outside branch retention policy (develop-like window: 30 days)".to_owned(),
        }
    }

    #[test]
    fn estimate_uses_the_row_share() {
        // Two snapshots, 30 rows against 70: the share of snapshots would say 500 bytes.
        let footprint = BaselineServingFootprint {
            tables: vec![
                table("serving_lexical", 1000, &[("gone", 30), ("kept", 70)]),
                table("snapshot_files", 1000, &[("gone", 40), ("kept", 60)]),
            ],
            missing: vec![],
        };
        let plan = PrunePlan {
            kept: vec![("kept".to_owned(), KeepReason::ActiveHead)],
            flatten: vec!["kept".to_owned()],
            delete: vec![deletion("gone", "2026-01-01T00:00:00.000000Z")],
        };
        let added = BTreeMap::from([("kept".to_owned(), 10u64)]);
        let estimate = estimate(&footprint, &plan, &added);

        assert_eq!(estimate.tables[0].reclaim_bytes, 300);
        assert_eq!(estimate.tables[0].rows_deleted, 30);
        assert_eq!(estimate.tables[0].total_rows, 100);
        assert_eq!(estimate.tables[1].reclaim_bytes, 400);
        assert_eq!(estimate.reclaim_bytes, 700);
        assert_eq!(estimate.snapshot_files_growth, 100, "10 added rows of a 100-row table");

        let empty = BaselineServingFootprint {
            tables: vec![table("snapshot_deletions", 8192, &[])],
            missing: vec![],
        };
        assert_eq!(estimate_of_empty(&empty, &plan), 0, "an empty table frees nothing");
    }

    fn estimate_of_empty(footprint: &BaselineServingFootprint, plan: &PrunePlan) -> u64 {
        estimate(footprint, plan, &BTreeMap::new()).reclaim_bytes
    }

    fn sample_report(parent_link_enforced: bool) -> PruneReport {
        let plan = PrunePlan {
            kept: vec![
                ("h".to_owned(), KeepReason::ActiveHead),
                ("k1".to_owned(), KeepReason::WithinWindow),
                ("v1".to_owned(), KeepReason::SafetyHead),
                ("v2".to_owned(), KeepReason::ActiveHead),
            ],
            flatten: vec!["k1".to_owned()],
            delete: vec![
                deletion("d2", "2026-09-05T10:00:00.000000Z"),
                deletion("d1", "2026-08-26T10:00:00.000000Z"),
            ],
        };
        let footprint = BaselineServingFootprint {
            tables: vec![
                table("serving_lexical", 4096, &[("d1", 11), ("d2", 13), ("h", 17)]),
                table("snapshot_files", 2048, &[("d1", 3), ("d2", 2), ("k1", 1)]),
                table("snapshot_deletions", 1024, &[("d2", 1)]),
            ],
            missing: vec!["serving_semantic".to_owned()],
        };
        let added_rows = BTreeMap::from([("k1".to_owned(), 4u64)]);
        PruneReport {
            policy: (31, 3, 2),
            catalog_snapshots: 7,
            estimate: estimate(&footprint, &plan, &added_rows),
            plan,
            added_rows,
            footprint,
            parent_link_enforced,
        }
    }

    #[test]
    fn report_prints_every_field() {
        let expected = "\
Shared baseline prune dry-run.
  Use --execute to apply.
  Develop retention:          31 days
  Vendor heads kept:          3
  Min snapshots per branch:   2
  Snapshots in catalog:       7
  Kept:                       4 (active-head: 2, safety-head: 1, within-window: 1)
  To make standalone:         1
    k1 (+4 file rows)
  To delete:                  2
    d2  develop  2026-09-05T10:00:00.000000Z  outside branch retention policy (develop-like window: 30 days)
    d1  develop  2026-08-26T10:00:00.000000Z  outside branch retention policy (develop-like window: 30 days)
  Space by table (rows of deleted snapshots / all rows, estimated bytes of total):
    serving_lexical: 24/41, ~2397 bytes (2.3 KiB) of 4096 bytes (4.0 KiB)
    snapshot_files: 5/6, ~1706 bytes (1.7 KiB) of 2048 bytes (2.0 KiB)
    snapshot_deletions: 1/1, ~1024 bytes (1.0 KiB) of 1024 bytes (1.0 KiB)
    serving_semantic: absent
  Estimated reclaim:          5127 bytes (5.0 KiB)
  snapshot_files growth:      ~1365 bytes (1.3 KiB)
  Not included: file objects, content objects and embeddings orphaned by the deletion — run `search baseline admin gc` after --execute.
";
        let unenforced = render_report(&sample_report(false), false);
        assert_eq!(
            unenforced,
            format!(
                "{expected}  Warning: --execute requires `search baseline admin migrate` with this build first.\n"
            )
        );
        assert_eq!(render_report(&sample_report(true), false), expected, "no warning with the key");
        let executing = render_report(&sample_report(true), true);
        assert!(
            executing.starts_with("Shared baseline prune.\n  Develop retention:"),
            "{executing}"
        );
    }

    /// A throwaway migrated schema, dropped when the value goes.
    struct Live {
        adapter: ExternalBaselineAdapter,
        schema: String,
    }

    impl Drop for Live {
        fn drop(&mut self) {
            let _ = self
                .adapter
                .execute_sql_for_test(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema));
        }
    }

    impl Live {
        fn new(prefix: &str) -> Self {
            let url = std::env::var("BSL_TEST_PG_URL")
                .expect("BSL_TEST_PG_URL must point to a live Postgres to run this test");
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
            let schema =
                format!("bsl_{prefix}_{:02x}{:08x}", std::process::id() % 256, nanos as u32);
            let adapter = ExternalBaselineAdapter::new(
                ExternalBaselineConfig::postgres(url).with_schema(schema.clone()),
            )
            .unwrap();
            let live = Self { adapter, schema };
            live.adapter.migrate_storage().unwrap();
            live
        }

        fn sql(&self, sql: &str) {
            self.adapter.execute_sql_for_test(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }

        fn rows(&self, sql: &str) -> Vec<Vec<String>> {
            self.adapter
                .query_text_for_test(sql)
                .unwrap_or_else(|error| panic!("{sql}: {error}"))
                .into_iter()
                .map(|row| row.into_iter().map(|value| value.unwrap_or_default()).collect())
                .collect()
        }

        fn number(&self, sql: &str) -> u64 {
            self.rows(sql)[0][0].parse().unwrap()
        }

        /// Row count of every table of the schema and the version of every snapshot row.
        fn inert_state(&self) -> (Vec<(String, u64)>, Vec<Vec<String>>) {
            let tables = self.rows(&format!(
                "SELECT table_name::TEXT FROM information_schema.tables
                  WHERE table_schema = '{}' ORDER BY 1",
                self.schema
            ));
            let counts = tables
                .into_iter()
                .map(|row| {
                    let count = self
                        .number(&format!("SELECT count(*)::TEXT FROM {}.{}", self.schema, row[0]));
                    (row[0].clone(), count)
                })
                .collect();
            let versions = self
                .rows(&format!("SELECT id, xmin::TEXT FROM {}.snapshots ORDER BY id", self.schema));
            (counts, versions)
        }

        fn dangling_parents(&self) -> u64 {
            self.number(&format!(
                "SELECT count(*)::TEXT FROM {0}.snapshots s
                  WHERE s.parent_snapshot_id IS NOT NULL
                    AND NOT EXISTS (SELECT 1 FROM {0}.snapshots p WHERE p.id = s.parent_snapshot_id)",
                self.schema
            ))
        }
    }

    fn indexed_document(root_id: &str, path: &str, content: &str) -> IndexedDocument {
        IndexedDocument {
            collection: "code".to_owned(),
            root_id: root_id.to_owned(),
            path: path.to_owned(),
            symbol_name: "Proc".to_owned(),
            kind: "procedure".to_owned(),
            line_start: 1,
            line_end: 2,
            text: format!("text {content}"),
            content_hash: format!("hash-{content}"),
            graph_context: None,
            source_span: None,
        }
    }

    /// `d1←d2←k1←h` on `develop` and `v1←v2` on `vendor`, with semantics, dated from `t0`.
    ///
    /// `d2` changes `B` and deletes `C` of `d1`, `k1` changes `A`, `h` adds `D`, and `X` lives
    /// under two roots — so `k1`, made standalone, inherits three files and must not resurrect
    /// `C`.
    fn publish_fixture(live: &Live, t0: DateTime<Utc>) {
        let doc = |path: &str, content: &str| indexed_document("", path, content);
        let ext = indexed_document("Расш", "src/X.bsl", "x-ext");
        let x = doc("src/X.bsl", "x-cfg");
        let chain: [(&str, Option<&str>, &str, Vec<IndexedDocument>); 6] = [
            (
                "d1",
                None,
                "develop",
                vec![
                    doc("src/A.bsl", "a1"),
                    doc("src/B.bsl", "b1"),
                    doc("src/C.bsl", "c1"),
                    x.clone(),
                    ext.clone(),
                ],
            ),
            (
                "d2",
                Some("d1"),
                "develop",
                vec![doc("src/A.bsl", "a1"), doc("src/B.bsl", "b2"), x.clone(), ext.clone()],
            ),
            (
                "k1",
                Some("d2"),
                "develop",
                vec![doc("src/A.bsl", "a2"), doc("src/B.bsl", "b2"), x.clone(), ext.clone()],
            ),
            (
                "h",
                Some("k1"),
                "develop",
                vec![
                    doc("src/A.bsl", "a2"),
                    doc("src/B.bsl", "b2"),
                    x.clone(),
                    ext.clone(),
                    doc("src/D.bsl", "d1"),
                ],
            ),
            ("v1", None, "vendor", vec![doc("src/V.bsl", "v1")]),
            ("v2", Some("v1"), "vendor", vec![doc("src/V.bsl", "v2"), doc("src/W.bsl", "w1")]),
        ];
        let mut embeddings = Vec::new();
        for (_, _, _, documents) in &chain {
            for document in documents {
                let key = bsl_search::semantic_key_for_indexed_document(document);
                if !embeddings.iter().any(|(known, _): &(String, Vec<f32>)| *known == key) {
                    let seed = embeddings.len() as f32;
                    embeddings.push((key, vec![1.0, seed, 0.5, -seed]));
                }
            }
        }
        live.adapter.store_embeddings("model", 4, &embeddings).unwrap();
        live.adapter.ensure_embedding_identity("model", 4).unwrap();
        for (id, parent, branch, documents) in &chain {
            let mut snapshot = Snapshot::new(*id, CorpusId::WorkspaceCode);
            if let Some(parent) = parent {
                snapshot = snapshot.with_parent(*parent);
            }
            live.adapter
                .publish_snapshot(
                    &snapshot,
                    &SnapshotPublishMetadata { branch: Some((*branch).to_owned()), commit: None },
                    documents,
                )
                .unwrap();
            live.adapter.populate_serving_semantic(id, "model", 4).unwrap();
        }
        let at = |days: i64| {
            (t0 - chrono::Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        };
        live.sql(&format!(
            "UPDATE {}.snapshots SET created_at = CASE id
                 WHEN 'd1' THEN '{}'::TIMESTAMPTZ WHEN 'd2' THEN '{}'::TIMESTAMPTZ
                 WHEN 'k1' THEN '{}'::TIMESTAMPTZ WHEN 'h' THEN '{}'::TIMESTAMPTZ
                 WHEN 'v1' THEN '{}'::TIMESTAMPTZ WHEN 'v2' THEN '{}'::TIMESTAMPTZ END",
            live.schema,
            at(60),
            at(50),
            at(10),
            at(1),
            at(40),
            at(20)
        ));
    }

    fn fixture_policy() -> project_model::SearchBaselinePolicyConfig {
        policy(1)
    }

    fn dry_run(live: &Live, t0: DateTime<Utc>) -> String {
        let mut output = String::new();
        perform(
            &live.adapter,
            &fixture_policy(),
            t0,
            false,
            |text| output.push_str(text),
            |_| ControlFlow::Continue(()),
        )
        .unwrap();
        output
    }

    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_prune_dry_run_changes_nothing() {
        let live = Live::new("prunedry");
        let t0 = Utc::now();
        publish_fixture(&live, t0);

        let before = live.inert_state();
        let output = dry_run(&live, t0);
        let report = prepare(&live.adapter, &fixture_policy(), t0).unwrap();
        assert_eq!(live.inert_state(), before, "a dry-run must not write");
        assert!(output.contains("  To delete:                  2\n"), "{output}");
        assert!(output.contains("  To make standalone:         1\n"), "{output}");
        assert!(output.contains("    k1 (+3 file rows)\n"), "{output}");
        assert_eq!(report.plan.flatten, vec!["k1"]);

        let deleted = ["d1", "d2"];
        for table in &report.footprint.tables {
            let qualified = format!("{}.{}", live.schema, table.table);
            assert_eq!(
                table.total_bytes,
                live.number(&format!("SELECT pg_total_relation_size('{qualified}')::TEXT")),
                "{qualified}"
            );
            assert_eq!(
                table.total_rows(),
                live.number(&format!("SELECT count(*)::TEXT FROM {qualified}")),
                "{qualified}"
            );
            let estimated =
                report.estimate.tables.iter().find(|row| row.table == table.table).unwrap();
            assert_eq!(
                estimated.rows_deleted,
                live.number(&format!(
                    "SELECT count(*)::TEXT FROM {qualified} WHERE snapshot_id IN ('d1', 'd2')"
                )),
                "{qualified}: rows of {deleted:?}"
            );
        }
        assert!(report.footprint.missing.is_empty(), "the fixture has semantics");
        // Control: the indexes are freed with the rows, so the size without them would understate.
        let lexical = format!("{}.serving_lexical", live.schema);
        assert!(
            live.number(&format!("SELECT pg_relation_size('{lexical}')::TEXT"))
                < live.number(&format!("SELECT pg_total_relation_size('{lexical}')::TEXT"))
        );

        // The same state and `now`, executed: the plan it prints before its first step is the
        // dry-run's, and the rows it says it adds are the rows it adds.
        let mut executed = String::new();
        let mut steps = Vec::new();
        perform(
            &live.adapter,
            &fixture_policy(),
            t0,
            true,
            |text| executed.push_str(text),
            |step| {
                steps.push(step.clone());
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        let plan_block = |output: &str| -> Vec<String> {
            output
                .lines()
                .skip_while(|line| !line.starts_with("  Snapshots in catalog:"))
                .take_while(|line| !line.starts_with("  Space by table"))
                .map(ToOwned::to_owned)
                .collect()
        };
        assert_eq!(plan_block(&executed), plan_block(&output), "{executed}");
        let first_step = executed.find("made standalone").expect("a step line");
        assert!(executed.find("  To delete:").unwrap() < first_step, "the plan comes first");
        for snapshot_id in &report.plan.flatten {
            let inherited = steps.iter().find_map(|step| match step {
                PruneStep::MadeStandalone { snapshot_id: done, inherited_files }
                    if done == snapshot_id =>
                {
                    Some(*inherited_files as u64)
                }
                _ => None,
            });
            assert_eq!(inherited, report.added_rows.get(snapshot_id).copied(), "{snapshot_id}");
        }
        assert_ne!(live.inert_state(), before, "control: execution is seen by the same comparison");
    }

    fn observe(live: &Live, snapshot_id: &str) -> Vec<String> {
        let adapter = &live.adapter;
        let sorted = |mut items: Vec<String>| {
            items.sort();
            items
        };
        let documents = adapter
            .load_snapshot_documents(&Snapshot::new(snapshot_id, CorpusId::WorkspaceCode))
            .unwrap();
        let manifest = adapter.load_baseline_manifest(snapshot_id).unwrap();
        let lexical = adapter.lexical_search_baseline(snapshot_id, "text", None, 100).unwrap();
        let semantic = adapter
            .semantic_search_baseline(snapshot_id, &[1.0, 0.0, 0.5, 0.0], "model", 4, None, 100)
            .unwrap();
        assert!(!lexical.is_empty() && !semantic.is_empty(), "{snapshot_id} must be searchable");
        let listed = adapter
            .list_snapshots(None, None, None, 200)
            .unwrap()
            .into_iter()
            .find(|record| record.snapshot_id == snapshot_id)
            .map(|record| (record.files, record.documents));
        vec![
            format!("{:?}", sorted(documents.iter().map(|d| format!("{d:?}")).collect())),
            format!(
                "{:?} {:?}",
                manifest.snapshot_fingerprint,
                sorted(manifest.files.iter().map(|f| format!("{f:?}")).collect())
            ),
            format!("{:?}", sorted(lexical.iter().map(|h| format!("{h:?}")).collect())),
            format!("{:?}", sorted(semantic.iter().map(|h| format!("{h:?}")).collect())),
            format!("{listed:?}"),
        ]
    }

    const RETAINED: [&str; 4] = ["k1", "h", "v1", "v2"];

    /// What the retained snapshots look like to every reader, plus where the branches resolve.
    fn observe_retained(live: &Live) -> Vec<Vec<String>> {
        let mut observed: Vec<Vec<String>> = RETAINED.iter().map(|id| observe(live, id)).collect();
        let resolved: Vec<String> = ["develop", "vendor"]
            .iter()
            .map(|branch| {
                let baseline = BaselineRef {
                    corpus: CorpusId::WorkspaceCode,
                    snapshot_id: None,
                    branch: Some((*branch).to_owned()),
                    commit: None,
                };
                format!("{:?}", live.adapter.resolve_baseline(&baseline).unwrap().map(|s| s.id))
            })
            .collect();
        observed.push(resolved);
        observed.push(heads(live));
        observed
    }

    fn heads(live: &Live) -> Vec<String> {
        live.rows(&format!(
            "SELECT corpus || ':' || branch || '->' || snapshot_id FROM {}.snapshot_heads ORDER BY 1",
            live.schema
        ))
        .into_iter()
        .map(|row| row[0].clone())
        .collect()
    }

    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn prune_keeps_every_retained_snapshot_observably_equal() {
        let live = Live::new("prunekeep");
        let t0 = Utc::now();
        publish_fixture(&live, t0);
        let before = observe_retained(&live);

        let mut output = String::new();
        let mut dangling_after_steps = Vec::new();
        perform(
            &live.adapter,
            &fixture_policy(),
            t0,
            true,
            |text| output.push_str(text),
            |_| {
                dangling_after_steps.push(live.dangling_parents());
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(dangling_after_steps, vec![0, 0, 0], "every committed step is consistent");
        for line in
            ["made standalone k1 (+3 file rows)\n", "deleted d2\n", "deleted d1\n", "Next: "]
        {
            assert!(output.contains(line), "{line:?} missing from:\n{output}");
        }
        assert_eq!(observe_retained(&live), before, "the retained snapshots must not change");

        let again = dry_run(&live, t0);
        assert!(again.contains("  To make standalone:         0\n"), "{again}");
        assert!(again.contains("  To delete:                  0\n"), "{again}");

        // Control: one inherited row of k1 gone is visible to the same comparison.
        live.sql(&format!(
            "DELETE FROM {}.snapshot_files WHERE snapshot_id = 'k1' AND path = 'src/B.bsl'",
            live.schema
        ));
        assert_ne!(observe_retained(&live), before);
    }

    /// The state compared between an interrupted run and an uninterrupted one.
    fn end_state(live: &Live) -> Vec<String> {
        let mut state: Vec<String> = live
            .rows(&format!(
                "SELECT id || ' ' || coalesce(parent_snapshot_id, '-') || ' '
                        || coalesce(branch, '-') || ' ' || created_at::TEXT
                   FROM {}.snapshots ORDER BY id",
                live.schema
            ))
            .into_iter()
            .map(|row| row[0].clone())
            .collect();
        state.extend(heads(live));
        for table in live.adapter.serving_footprint().unwrap().tables {
            state.push(format!("{} {:?}", table.table, table.rows_by_snapshot));
        }
        let catalog = live.adapter.snapshot_catalog().unwrap();
        for snapshot in &catalog.snapshots {
            state.push(observe(live, &snapshot.snapshot_id)[0].clone());
        }
        state
    }

    fn assert_consistent(live: &Live, before: &[Vec<String>]) {
        assert_eq!(live.dangling_parents(), 0);
        assert_eq!(observe_retained(live), before);
    }

    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn an_interrupted_prune_is_consistent_and_a_rerun_finishes_it() {
        let t0 = Utc::now();
        let interrupted = Live::new("pruneint");
        let straight = Live::new("prunestr");
        publish_fixture(&interrupted, t0);
        publish_fixture(&straight, t0);
        let before = observe_retained(&interrupted);

        let first = prepare(&interrupted.adapter, &fixture_policy(), t0).unwrap();
        let run = execute(&interrupted.adapter, &first.plan, |step| match step {
            PruneStep::MadeStandalone { .. } => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        });
        assert!(run.stopped && run.error.is_none());
        assert_eq!(run.steps.len(), 1, "{:?}", run.steps);
        assert_consistent(&interrupted, &before);

        let second = prepare(&interrupted.adapter, &fixture_policy(), t0).unwrap();
        assert!(second.plan.flatten.is_empty(), "k1 is standalone already");
        let run = execute(&interrupted.adapter, &second.plan, |step| match step {
            PruneStep::Deleted { .. } => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        });
        assert!(run.stopped && run.error.is_none());
        assert_eq!(run.steps, vec![PruneStep::Deleted { snapshot_id: "d2".to_owned() }]);
        assert_consistent(&interrupted, &before);

        let third = prepare(&interrupted.adapter, &fixture_policy(), t0).unwrap();
        let run = execute(&interrupted.adapter, &third.plan, |_| ControlFlow::Continue(()));
        assert!(!run.stopped && run.error.is_none());
        assert_eq!(run.steps, vec![PruneStep::Deleted { snapshot_id: "d1".to_owned() }]);
        assert_consistent(&interrupted, &before);

        perform(
            &straight.adapter,
            &fixture_policy(),
            t0,
            true,
            |_| {},
            |_| ControlFlow::Continue(()),
        )
        .unwrap();
        assert_eq!(end_state(&interrupted), end_state(&straight));
    }

    /// A child published onto a snapshot the plan deletes stops the run by name, and the child
    /// survives — the plan was already stale, a fresh one would have kept its parent.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_child_published_before_the_delete_stops_the_prune() {
        let live = Live::new("prunechild");
        let t0 = Utc::now();
        publish_fixture(&live, t0);
        let report = prepare(&live.adapter, &fixture_policy(), t0).unwrap();
        assert_eq!(report.plan.delete[0].snapshot_id, "d2");
        let child = indexed_document("", "src/E.bsl", "e1");
        live.adapter
            .publish_snapshot(
                &Snapshot::new("c", CorpusId::WorkspaceCode).with_parent("d2"),
                &SnapshotPublishMetadata::default(),
                std::slice::from_ref(&child),
            )
            .unwrap();

        let run = execute(&live.adapter, &report.plan, |_| ControlFlow::Continue(()));
        let error = run.error.expect("the delete of a live parent must fail");
        assert_eq!(error.reason_code(), Some("snapshot_still_referenced"), "{error}");
        assert_eq!(
            run.steps,
            vec![PruneStep::MadeStandalone { snapshot_id: "k1".to_owned(), inherited_files: 3 }],
            "nothing after the failed step"
        );
        let resolved = live
            .adapter
            .resolve_baseline(&BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "c"))
            .unwrap()
            .expect("the child resolves");
        assert!(!live.adapter.load_snapshot_documents(&resolved).unwrap().is_empty());
        assert_eq!(live.dangling_parents(), 0);

        // Control: without the key the same delete goes through and leaves the child dangling.
        live.sql(&format!(
            "ALTER TABLE {}.snapshots DROP CONSTRAINT snapshots_parent_fk",
            live.schema
        ));
        assert_eq!(live.adapter.delete_snapshot("d2").unwrap(), bsl_search::DeleteOutcome::Deleted);
        assert_eq!(live.dangling_parents(), 1);
    }

    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn prune_execute_refuses_without_the_parent_link() {
        let live = Live::new("prunenokey");
        let t0 = Utc::now();
        publish_fixture(&live, t0);
        live.sql(&format!(
            "ALTER TABLE {}.snapshots DROP CONSTRAINT snapshots_parent_fk",
            live.schema
        ));

        assert!(dry_run(&live, t0)
            .contains("  Warning: --execute requires `search baseline admin migrate`"));
        let before = live.inert_state();
        let mut output = String::new();
        let error = perform(
            &live.adapter,
            &fixture_policy(),
            t0,
            true,
            |text| output.push_str(text),
            |_| ControlFlow::Continue(()),
        )
        .expect_err("--execute without the parent link must refuse");
        let error = error.downcast_ref::<SearchError>().expect("a named storage refusal");
        assert_eq!(error.reason_code(), Some("parent_link_not_enforced"), "{error}");
        assert!(output.is_empty(), "nothing is planned or printed: {output}");
        assert_eq!(live.inert_state(), before);

        // Control: with the key back the same call changes the database.
        live.adapter.migrate_storage().unwrap();
        perform(
            &live.adapter,
            &fixture_policy(),
            t0,
            true,
            |text| output.push_str(text),
            |_| ControlFlow::Continue(()),
        )
        .unwrap();
        assert!(output.contains("deleted d2\n"), "{output}");
    }

    /// A plan carried out and then replayed — as when a second operator runs the same plan — has
    /// nothing left to do: every step is skipped by name and the database is left as it was.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn replaying_a_finished_plan_skips_every_step() {
        let live = Live::new("prunereplay");
        let t0 = Utc::now();
        publish_fixture(&live, t0);
        let before = observe_retained(&live);
        let report = prepare(&live.adapter, &fixture_policy(), t0).unwrap();

        let first = execute(&live.adapter, &report.plan, |_| ControlFlow::Continue(()));
        assert!(first.error.is_none() && !first.stopped);
        assert_eq!(
            first.steps,
            vec![
                PruneStep::MadeStandalone { snapshot_id: "k1".to_owned(), inherited_files: 3 },
                PruneStep::Deleted { snapshot_id: "d2".to_owned() },
                PruneStep::Deleted { snapshot_id: "d1".to_owned() },
            ],
            "control: the first run does the work the replay must not repeat"
        );
        let after = live.inert_state();

        let replay = execute(&live.adapter, &report.plan, |_| ControlFlow::Continue(()));
        assert!(replay.error.is_none() && !replay.stopped, "{:?}", replay.error);
        assert_eq!(
            replay.steps,
            vec![
                PruneStep::Skipped {
                    snapshot_id: "k1".to_owned(),
                    why: SkipReason::AlreadyStandalone
                },
                PruneStep::Skipped { snapshot_id: "d2".to_owned(), why: SkipReason::Missing },
                PruneStep::Skipped { snapshot_id: "d1".to_owned(), why: SkipReason::Missing },
            ]
        );
        assert_eq!(live.inert_state(), after, "a replay writes nothing");
        assert_consistent(&live, &before);
    }

    /// A head that lands on a snapshot of the plan after it was made keeps the snapshot, so its
    /// parent — which the head still points through — cannot go either: the run stops there by
    /// name, with the data whole, and a rerun plans the head as kept and finishes the job.
    #[test]
    #[ignore = "requires a live Postgres; set BSL_TEST_PG_URL and run with --ignored"]
    fn a_snapshot_that_became_a_head_is_skipped_and_its_parent_stops_the_run() {
        let live = Live::new("pruneahead");
        let t0 = Utc::now();
        publish_fixture(&live, t0);
        let before = observe_retained(&live);
        let report = prepare(&live.adapter, &fixture_policy(), t0).unwrap();
        assert_eq!(report.plan.flatten, vec!["k1"]);
        assert_eq!(
            report.plan.delete.iter().map(|entry| entry.snapshot_id.as_str()).collect::<Vec<_>>(),
            vec!["d2", "d1"]
        );
        live.sql(&format!(
            "UPDATE {}.snapshot_heads SET snapshot_id = 'd2'
              WHERE corpus = 'workspace-code' AND branch = 'develop'",
            live.schema
        ));

        let run = execute(&live.adapter, &report.plan, |_| ControlFlow::Continue(()));
        assert_eq!(
            run.steps,
            vec![
                PruneStep::MadeStandalone { snapshot_id: "k1".to_owned(), inherited_files: 3 },
                PruneStep::Skipped { snapshot_id: "d2".to_owned(), why: SkipReason::BecameHead },
            ],
            "nothing after the failed step"
        );
        let error = run.error.expect("the parent of a kept snapshot must not be deletable");
        assert_eq!(error.reason_code(), Some("snapshot_still_referenced"), "{error}");
        assert_eq!(live.dangling_parents(), 0);
        let rows = live.rows(&format!(
            "SELECT id, coalesce(parent_snapshot_id, '-') FROM {}.snapshots
              WHERE id IN ('d1', 'd2') ORDER BY id",
            live.schema
        ));
        assert_eq!(rows, vec![vec!["d1", "-"], vec!["d2", "d1"]], "both still there, still linked");

        let second = prepare(&live.adapter, &fixture_policy(), t0).unwrap();
        assert!(second.plan.kept.contains(&("d2".to_owned(), KeepReason::BranchHead)));
        assert_eq!(second.plan.flatten, vec!["d2"]);
        assert_eq!(
            second.plan.delete.iter().map(|entry| entry.snapshot_id.as_str()).collect::<Vec<_>>(),
            vec!["d1"]
        );
        let rerun = execute(&live.adapter, &second.plan, |_| ControlFlow::Continue(()));
        assert!(rerun.error.is_none(), "{:?}", rerun.error);
        assert_eq!(
            rerun.steps,
            vec![
                PruneStep::MadeStandalone { snapshot_id: "d2".to_owned(), inherited_files: 3 },
                PruneStep::Deleted { snapshot_id: "d1".to_owned() },
            ]
        );
        assert_eq!(live.dangling_parents(), 0);
        assert_eq!(
            observe_retained(&live)[..RETAINED.len()],
            before[..RETAINED.len()],
            "the retained snapshots read as they did"
        );
    }

    #[test]
    fn execution_report_prints_steps_and_next() {
        let steps = vec![
            PruneStep::MadeStandalone { snapshot_id: "k1".to_owned(), inherited_files: 3 },
            PruneStep::Skipped { snapshot_id: "k2".to_owned(), why: SkipReason::AlreadyStandalone },
            PruneStep::Deleted { snapshot_id: "d2".to_owned() },
            PruneStep::Skipped { snapshot_id: "d3".to_owned(), why: SkipReason::Missing },
            PruneStep::Skipped { snapshot_id: "d4".to_owned(), why: SkipReason::BecameHead },
        ];
        let lines: Vec<String> = steps.iter().map(render_step).collect();
        assert_eq!(
            lines,
            vec![
                "made standalone k1 (+3 file rows)\n",
                "skipped k2: already standalone\n",
                "deleted d2\n",
                "skipped d3: missing\n",
                "skipped d4: became a branch head\n",
            ]
        );

        let finished = PruneExecution { steps: steps.clone(), error: None, stopped: false };
        let summary = render_summary(&finished);
        assert!(summary.starts_with("Made standalone: 1, deleted: 1, skipped: 3\n"), "{summary}");
        assert!(summary.contains("Next: search baseline admin gc (dry-run, then --execute)"));
        assert!(!summary.contains("Error:"));
        assert!(outcome(finished).is_ok());

        let failed = PruneExecution {
            steps: steps.clone(),
            error: Some(SearchError::ExternalBaseline(
                "snapshot_still_referenced: snapshot 'd1' is still the parent".to_owned(),
            )),
            stopped: false,
        };
        let summary = render_summary(&failed);
        assert!(
            summary.contains("Error: external baseline error: snapshot_still_referenced"),
            "{summary}"
        );
        assert!(!summary.contains("Next:"), "{summary}");
        assert!(outcome(failed).is_err());

        let stopped = PruneExecution { steps, error: None, stopped: true };
        assert!(!render_summary(&stopped).contains("Next:"));
        assert!(outcome(stopped).is_ok());
    }
}
