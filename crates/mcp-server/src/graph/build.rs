//! Background graph build, cache adoption, and SQLite publication work.

use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(test)]
use std::sync::atomic::Ordering;

use bsl_search::SearchEngine;

#[cfg(test)]
use crate::cache::graph_db_path;
use crate::graph_query::GraphDb;
use crate::workspace_lease::{LeaseOperationError, LeaseOperationOutcome};

#[cfg(test)]
use super::input::GRAPH_SOURCE_ROOT;
#[cfg(test)]
use super::scan::workspace_fingerprint;
use super::snapshot::{PreparedSnapshotPool, SnapshotInstallError, SnapshotPrepareError};
use super::state::{lock_recover, GraphState, Published, ReloadState};
use super::types::GraphStatus;
use stdx::batch::BatchBudget;

/// Modules whose edges are projected per batch when building the on-disk graph,
/// capped by count and by source bytes: a batch's syntax trees, lowered bodies and
/// inference scale with its bytes, and 500 modules of a large configuration range
/// from under a megabyte to well over a hundred, so the byte cap is what bounds
/// the build's peak while the resident method index resolves cross-batch calls.
pub(super) const GRAPH_BUILD_BATCH: BatchBudget = BatchBudget::files(500).with_bytes(32 << 20);

#[cfg(test)]
fn graph_build_path(path: &Path) -> PathBuf {
    // Backend generations can overlap within one process as well as across processes.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let build = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("db.building.{}.{build}", std::process::id()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoadFailureReason {
    TransientRefusal,
    Superseded,
    Released,
    OperationError,
}

#[derive(Clone, Debug)]
pub(super) struct LoadFailure {
    pub(super) reason: LoadFailureReason,
    pub(super) message: String,
}

impl LoadFailure {
    fn new(reason: LoadFailureReason, message: impl Into<String>) -> Self {
        Self { reason, message: message.into() }
    }

    pub(super) fn operation(error: impl std::fmt::Display) -> Self {
        Self::new(LoadFailureReason::OperationError, error.to_string())
    }

    fn lifecycle_outcome(&self) -> bsl_search::lifecycle::Outcome {
        use bsl_search::lifecycle::Outcome;
        match self.reason {
            LoadFailureReason::TransientRefusal => Outcome::Refused,
            LoadFailureReason::Superseded | LoadFailureReason::Released => Outcome::Interrupted,
            LoadFailureReason::OperationError => Outcome::Failed,
        }
    }

    pub(super) fn refused(message: impl Into<String>) -> Self {
        Self::new(LoadFailureReason::TransientRefusal, message)
    }
}

impl std::fmt::Display for LoadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LoadFailure {}

fn relative_event_path(
    path: &Path,
    workspace_root: &Path,
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> String {
    if let Ok(relative) = path.strip_prefix(workspace_root) {
        return relative.to_string_lossy().replace('\\', "/");
    }
    let Some(key) = roots.and_then(|roots| roots.key_of_path(path)) else {
        return "<unregistered>".to_owned();
    };
    let root_id = if key.root_id.is_empty() { "configuration" } else { &key.root_id };
    format!("{root_id}/{}", key.path.replace('\\', "/"))
}

fn install_failure(
    outcome: LeaseOperationOutcome<(), SnapshotInstallError>,
) -> Result<(), LoadFailure> {
    match outcome {
        LeaseOperationOutcome::Applied(()) => Ok(()),
        LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
            SnapshotInstallError::Changed,
        )) => Err(LoadFailure::new(
            LoadFailureReason::TransientRefusal,
            "graph path changed before the prepared snapshot pool could be installed",
        )),
        LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
            SnapshotInstallError::Operation(message),
        )) => Err(LoadFailure::new(LoadFailureReason::OperationError, message)),
        LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)) => {
            Err(LoadFailure::operation(error))
        }
        LeaseOperationOutcome::TransientRefusal => Err(LoadFailure::new(
            LoadFailureReason::TransientRefusal,
            "workspace cache ownership was temporarily unavailable during graph snapshot install",
        )),
        LeaseOperationOutcome::Superseded => Err(LoadFailure::new(
            LoadFailureReason::Superseded,
            "workspace cache ownership was superseded before graph snapshot install",
        )),
        LeaseOperationOutcome::Released => Err(LoadFailure::new(
            LoadFailureReason::Released,
            "workspace cache ownership was released before graph snapshot install",
        )),
    }
}

fn prepare_failure(error: SnapshotPrepareError) -> LoadFailure {
    match error {
        SnapshotPrepareError::Changed => LoadFailure::new(
            LoadFailureReason::TransientRefusal,
            "graph changed while its snapshot pool was being prepared",
        ),
        SnapshotPrepareError::Open(error) => LoadFailure::new(
            LoadFailureReason::OperationError,
            format!("preparing graph snapshot pool: {error}"),
        ),
    }
}

pub(super) enum PublishAttemptOutcome {
    Published,
    FallBack,
    Refused(LoadFailure),
}

#[cfg(test)]
thread_local! {
    static FUSED_FILE_COMMITTED_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

impl GraphState {
    /// The fused cold build, and the report of its own outcome.
    ///
    /// Reported HERE, while the ticket this build was admitted on is still carried. The grant
    /// leaves the slot the moment the build takes it, so a failure recorded after the carry has
    /// been dropped finds an empty slot and names the primary lane by default — which turned a
    /// build only the marks financed into a primary failure with a retry budget nothing bought.
    pub(super) fn run_fused_cold_build(
        &self,
        engine: &mut SearchEngine,
        source_path: &Path,
        observed_through: u64,
    ) -> Result<(), LoadFailure> {
        let ticket = self.take_claimed_ticket();
        // Carried from here to every way this build can end, its outcome included.
        let _carried = self.carry_ticket(ticket);
        let outcome = self.fused_cold_build(engine, source_path, observed_through, ticket);
        if let Err(failure) = &outcome {
            self.record_load_failure(false, failure.clone());
        }
        outcome
    }

    fn fused_cold_build(
        &self,
        engine: &mut SearchEngine,
        source_path: &Path,
        observed_through: u64,
        ticket: Option<super::debt::BuildTicket>,
    ) -> Result<(), LoadFailure> {
        self.clear_cold_build_ticker();
        let Some(workspace_root) = self.workspace_root.clone() else {
            return Err(LoadFailure::operation("fused build on a non-workspace graph"));
        };
        let generation =
            lock_recover(&self.inner).published.as_ref().map(|p| p.generation).unwrap_or(0) + 1;
        // The mandate the external claim fixed. Without it this publication discharged
        // nothing: a forced reload recorded before the boot took the slot stayed open, and the
        // drive that follows the publication started a second forced build for work this one
        // had already done.
        let observed_through = ticket.map_or(observed_through, |ticket| ticket.scan_cutoff);
        let forced_through = ticket.and_then(|ticket| ticket.forced_through);

        let source_path = source_path.to_path_buf();
        let mut sink = FusedChunkWriter::new(engine, source_path, self.lease.clone());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_and_publish_graph_file(&workspace_root, generation, self, Some(&mut sink))
        }));
        let observation_outcome = match &outcome {
            Ok(Ok(_)) => bsl_search::lifecycle::Outcome::Completed,
            Ok(Err(failure)) => sink.failure.as_ref().unwrap_or(failure).lifecycle_outcome(),
            Err(_) => bsl_search::lifecycle::Outcome::Interrupted,
        };
        sink.finish(observation_outcome);
        let built = match outcome {
            Ok(Ok(v)) => v,
            Ok(Err(failure)) => return Err(sink.failure.take().unwrap_or(failure)),
            Err(_) => return Err(LoadFailure::operation("fused graph build panicked")),
        };
        let publication_id = built.prepared.publication_id();
        let files = built.files;
        let xml_files = built.xml_files;
        if built.force_stale {
            tracing::warn!("fused graph build straddled a disk write; snapshot marked stale");
        }
        let recovery_through = ticket.map(|ticket| ticket.recovery_cutoff);
        install_failure(self.install_prepared_snapshot(
            built.prepared,
            Published {
                generation,
                fingerprint: built.fp_pre,
                stale: false,
                reload: ReloadState::Idle,
                force_stale: built.force_stale,
                search_roots: built.search_roots.clone(),
                observed_through: Some(observed_through),
            },
            GraphStatus::Ready { files: built.files },
            // Whatever its ticket was admitted to answer. A fused build that ran forced
            // discharges the demand that made it forced, exactly like any other.
            forced_through,
            recovery_through,
            built.recovery,
        ))?;
        self.finish_cold_build_ticker();
        *lock_recover(&self.scan) = None;
        self.ensure_hub_roots(&built.scan_roots, built.physical_topology, built.declaration_epoch);
        // The fused sink just wrote every indexed document's context from THIS
        // build — nothing persisted predates it, so no whole-collection re-render.
        self.notify_published(false);
        tracing::info!(
            files,
            xml_files,
            parsed_xml = xml_files,
            reprojected = files,
            affected_callers = 0,
            generation,
            fact_seq = observed_through,
            publication_id = publication_id.as_deref(),
            sponsors = ?ticket.map(|ticket| ticket.sponsors),
            "graph database build complete"
        );
        Ok(())
    }

    /// After a successful (re)build, re-point the daemon's change hub at the build
    /// snapshot's scan roots. A topology reload that added or dropped an extension
    /// root would otherwise leave the hub watching the old universe — events in a
    /// new extension would never be delivered, and every consumer would coast on
    /// its reconcile interval. A no-op when the roots did not change.
    ///
    /// `declaration_epoch` is the age of the build's own snapshot. The freshness check
    /// below skips the common case of an overtaken build before anything is sent; the
    /// epoch covers the race the check cannot — passed its re-read, this build can still
    /// be overtaken between the check and the declaration — and the hub refuses one that
    /// speaks for an older composition (github#184).
    pub(super) fn ensure_hub_roots(
        &self,
        scan_roots: &[std::path::PathBuf],
        built_topology: u64,
        declaration_epoch: u64,
    ) {
        if !self.validate_workspace_scope() {
            return;
        }
        let (Some(hub), Some(root)) = (&self.change_hub, self.workspace_root.as_deref()) else {
            return;
        };
        // A slow build finishing after a newer topology reload must not roll the
        // shared hub back onto its older root set: re-derive the live topology
        // (config parse + discovery, no tree walk) and skip when this build's
        // snapshot is already superseded — the fresher build re-arms instead.
        let live = crate::graph::ProjectSnapshot::load_excluding(root, &self.cache_exclusions());
        if super::scan::topology_u64(&live.configs) != built_topology {
            tracing::info!("skipping hub re-arm: the built snapshot's topology is superseded");
            return;
        }
        // The exclusions travel with the roots: the same roots under a new `[source].exclude`
        // are a different coverage, and the live project is the one to be followed.
        if !hub.ensure_scope(
            &crate::change_hub::watch_targets_for(root, scan_roots),
            &live.user_excluded,
            declaration_epoch,
        ) {
            tracing::warn!("graph rebuild could not re-arm the change hub onto new roots");
        }
    }

    /// Build (or rebuild) the database off-thread and publish it coherently.
    /// `is_reload` distinguishes the initial load (sets `Ready`, generation 1)
    /// from a drift-triggered reload (bumps the generation, keeps the old snapshot
    /// served on failure).
    pub(super) fn run_load(&self, is_reload: bool) {
        self.clear_cold_build_ticker();
        if self.is_superseded() {
            self.record_load_failure(
                is_reload,
                LoadFailure::new(
                    LoadFailureReason::Superseded,
                    super::types::SUPERSEDED_GRAPH_ERROR,
                ),
            );
            return;
        }
        let Some(workspace_root) = self.workspace_root.clone() else {
            return;
        };
        if !self.validate_workspace_scope() {
            self.record_load_failure(
                is_reload,
                LoadFailure::operation("workspace cache scope changed before graph preparation"),
            );
            return;
        }
        if let Some(reason) = self.ownership_refusal() {
            self.record_load_failure(
                is_reload,
                LoadFailure::new(LoadFailureReason::TransientRefusal, reason),
            );
            return;
        }
        // Nothing below opens the graph file before this process holds its access lock. The
        // wait has no deadline: the previous owner lets go once its reads are done.
        if !self.acquire_graph_access(true) {
            let failure = if self.lease_is_terminal() {
                lost_workspace_failure(self)
            } else {
                LoadFailure::new(
                    LoadFailureReason::TransientRefusal,
                    "this process does not hold the graph file's access lock",
                )
            };
            self.record_load_failure(is_reload, failure);
            return;
        }
        // An interrupted writer's journal is finished by the new holder of the file before any
        // read-only handle opens it: the process that left it may have crashed mid-write.
        if let Some(path) = self.graph_db_path() {
            if let Err(error) = super::snapshot::recover_hot_journal(&path) {
                self.record_load_failure(
                    is_reload,
                    LoadFailure::new(
                        LoadFailureReason::OperationError,
                        format!(
                            "the graph file's interrupted journal could not be recovered: {error}"
                        ),
                    ),
                );
                return;
            }
        }
        // The generation this build will carry. Only one load runs at a time (the
        // initial load, then at most one reload via the claim guard), so peeking the
        // current generation without reserving it is race-free; a failed build leaves
        // it unpublished and the next attempt reuses the same number.
        let generation =
            lock_recover(&self.inner).published.as_ref().map(|p| p.generation).unwrap_or(0) + 1;

        // The barrier a test uses to put a delivery in the window between the accepted claim
        // and the pre-scan below.
        #[cfg(test)]
        if let Some(hook) = self.post_claim_hook.clone() {
            hook(self);
        }

        // The mandate, as it was fixed at the admission. NOT re-derived here: a builder that
        // reads the current debts reconstructs a mandate nobody granted it — the facts on the
        // table now include everything delivered since the claim, and publishing a proof over
        // those retires credits this build never answered.
        //
        // A build with no ticket is one no admission point granted (a direct call in a test,
        // or the boot's own synchronous path); it falls back to reading the position now,
        // which is exactly what it was before tickets existed.
        // Every build carries one, and there is no path that reads the debts instead. A
        // caller that reached here without going through an admission point — the boot's own
        // synchronous entry, a test driving the loader directly — gets one minted here, ONCE,
        // before any disk is read. What must never exist is a later live re-read: that is the
        // whole defect the ticket removes.
        let ticket =
            self.take_claimed_ticket().unwrap_or_else(|| self.mint_direct_ticket(is_reload));
        let _carried = self.carry_ticket(Some(ticket));
        let observed_through = ticket.scan_cutoff;
        let forced_through = ticket.forced_through;
        let recovery_through = Some(ticket.recovery_cutoff);
        let force_project_reload = ticket.forced;

        // On the initial load, reuse a cached build from a previous process run if it
        // still matches the workspace — turning a multi-minute rebuild into a stat
        // walk plus an open. A reload is skipped here: it only fires once drift has
        // been detected, so the on-disk file is known stale and must be rebuilt.
        if !force_project_reload && !is_reload {
            match self.try_publish_cached(&workspace_root, observed_through) {
                PublishAttemptOutcome::Published => return,
                PublishAttemptOutcome::FallBack => {}
                PublishAttemptOutcome::Refused(failure) => {
                    self.record_load_failure(is_reload, failure);
                    return;
                }
            }
        }

        // Cached but drifted: serve the stale snapshot immediately and catch up through
        // the reload lifecycle (its failure path keeps the snapshot and flags
        // `reload="failed"`, unlike this initial load's `Failed`). The catch-up build
        // recomputes its own generation from the just-published revision.
        if !force_project_reload && !is_reload {
            match self.try_publish_stale_and_catch_up(&workspace_root) {
                PublishAttemptOutcome::Published => return,
                PublishAttemptOutcome::FallBack => {}
                PublishAttemptOutcome::Refused(failure) => {
                    self.record_load_failure(is_reload, failure);
                    return;
                }
            }
        }

        // On reload, try the body-only fast path first: if only `.bsl` bodies changed
        // (signatures intact, nothing added/removed, no `.xml` drift) reproject just
        // those modules instead of the whole config. On any ineligibility or failure
        // it returns false and we fall through to a full rebuild.
        if !force_project_reload && is_reload {
            match self.try_incremental_reload(&workspace_root, generation, observed_through) {
                PublishAttemptOutcome::Published => return,
                PublishAttemptOutcome::FallBack => {}
                PublishAttemptOutcome::Refused(failure) => {
                    self.record_load_failure(is_reload, failure);
                    return;
                }
            }
        }

        tracing::info!(
            ?workspace_root,
            is_reload,
            generation,
            fact_seq = observed_through,
            forced = force_project_reload,
            "graph database build started"
        );
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_and_publish_graph_file(&workspace_root, generation, self, None)
        }));

        match outcome {
            Ok(Ok(built)) => {
                let PublishedBuild {
                    generation,
                    files,
                    xml_files,
                    fp_pre,
                    force_stale,
                    scan_roots,
                    declaration_epoch,
                    physical_topology,
                    search_roots,
                    prepared,
                    recovery,
                } = built;
                let publication_id = prepared.publication_id();
                if force_stale {
                    tracing::warn!(
                        is_reload,
                        "graph build straddled a disk write; marking snapshot stale to force reload"
                    );
                }
                // Drop the stale scan cache *before* publishing so a concurrent
                // freshness check re-scans against the new snapshot rather than a
                // pre-reload cached fingerprint.
                *lock_recover(&self.scan) = None;
                let topology_changed;
                {
                    let inner = lock_recover(&self.inner);
                    // Only a WITNESSED transition (a previously published topology
                    // differing from this build's) requests the whole-collection
                    // re-render. `None` deliberately reads as unchanged: a cold
                    // build must keep the boot invariant that an early publish
                    // clears no pre-existing context marks — the offline-edit
                    // warm start is covered by the stale-adopt -> catch-up chain,
                    // which publishes the old topology first and transitions here.
                    topology_changed = inner
                        .published
                        .as_ref()
                        .is_some_and(|p| p.fingerprint.topology != fp_pre.topology);
                }
                if let Err(error) = install_failure(self.install_prepared_snapshot(
                    prepared,
                    Published {
                        generation,
                        fingerprint: fp_pre,
                        stale: false,
                        reload: ReloadState::Idle,
                        force_stale,
                        search_roots: search_roots.clone(),
                        observed_through: Some(observed_through),
                    },
                    GraphStatus::Ready { files },
                    // The only path that runs under a forced reload. The fact was captured
                    // before the build, so a request arriving mid-build names a later one and
                    // stays outstanding.
                    forced_through,
                    recovery_through,
                    recovery,
                )) {
                    self.record_load_failure(is_reload, error);
                    return;
                }
                self.finish_cold_build_ticker();
                #[cfg(test)]
                if let Some(hook) = &self.publish_window_hook {
                    hook();
                }
                self.ensure_hub_roots(&scan_roots, physical_topology, declaration_epoch);
                self.notify_published(topology_changed);
                tracing::info!(
                    files,
                    xml_files,
                    parsed_xml = xml_files,
                    reprojected = files,
                    affected_callers = 0,
                    generation,
                    is_reload,
                    fact_seq = observed_through,
                    publication_id = publication_id.as_deref(),
                    "graph database build complete"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!("graph database build failed: {}", e.message);
                self.record_load_failure(is_reload, e);
            }
            Err(_) => {
                tracing::error!("graph database build panicked");
                self.record_load_failure(
                    is_reload,
                    LoadFailure::new(LoadFailureReason::OperationError, "builder panicked"),
                );
            }
        }
    }

    /// The body-only fast path for a reload. Eligible only when every drifted file is
    /// a `.bsl` whose signature hash still matches its persisted value, with nothing
    /// added/removed and no `.xml` drift — then no caller's resolution can have moved,
    /// so reprojecting just those modules yields a database byte-identical to a full
    /// rebuild. Patches a copy of the published file and atomically renames it in,
    /// then publishes `generation`. Structural ineligibility falls back to a full rebuild;
    /// an ownership refusal retains its classification for the load lifecycle.
    /// What the point-patch path did and why, for a test that has to know WHICH branch ran.
    ///
    /// Observation only: it records the decision the production gates already made, and no
    /// gate consults it. A test asserting a point rewrite must be able to tell a patch from a
    /// full rebuild that quietly replaced it, or it is asserting about the wrong path.
    fn note_incremental(&self, outcome: &'static str) -> PublishAttemptOutcome {
        tracing::info!(reason_code = outcome, "incremental reload fell back to a full build");
        #[cfg(test)]
        lock_recover(&self.incremental_decisions).push(outcome);
        PublishAttemptOutcome::FallBack
    }

    fn try_incremental_reload(
        &self,
        workspace_root: &Path,
        generation: u64,
        observed_through: u64,
    ) -> PublishAttemptOutcome {
        tracing::info!(generation, fact_seq = observed_through, "graph incremental reload started");
        let db_path = self.graph_db_path().expect("workspace graph has cache layout");
        // Every read of the published build below names the generation this first one saw,
        // and none holds a handle across the scans and analysis between them.
        let wait = super::BACKGROUND_READ_WAIT;
        let Ok((base, stored_fp, stored_module_total)) = self.store.read(None, wait, |snapshot| {
            (
                snapshot.generation,
                snapshot.graph.stored_fingerprints(),
                snapshot.graph.file_count_strict(),
            )
        }) else {
            return self.note_incremental("published_graph_unavailable");
        };
        let Ok(stored_module_total) = stored_module_total else {
            return self.note_incremental("missing_published_module_total");
        };
        // Empty fingerprints are valid only for a current-schema publication that
        // explicitly records zero modules. GraphDb::open has already rejected older
        // schemas; nonempty published module totals retain the legacy-cache fallback.
        if stored_fp.is_empty() && stored_module_total != 0 {
            return self.note_incremental("no_stored_fingerprints"); // older build → full rebuild
        }
        // ONE project snapshot and ONE scanned universe serve the eligibility diff,
        // the profile recompute, the pre-fingerprint and the patch, so neither a
        // config edit nor a file landing mid-operation can hand two passes two
        // different trees. Only the straddle check walks again.
        let project =
            crate::graph::ProjectSnapshot::load_excluding(workspace_root, &self.cache_exclusions());
        // A topology change re-shapes visibility for ANY module even when only
        // `.bsl` bodies drifted on disk — never body-patch across it.
        match self.store.read(Some(base), wait, |snapshot| snapshot.graph.freshness_token()) {
            Ok(Ok((_, stored_token, _))) if stored_token.topology == project.portable_topology => {}
            Err(_) => return self.note_incremental("published_graph_moved"),
            _ => return self.note_incremental("topology_moved"),
        }
        let pre = crate::graph::universe::ScannedUniverse::scan_project(&project);
        // Before the diff, not inside the bracket: a diff against a short scan reads
        // hidden files as removals, and an unreadable EMPTY subtree does not move the
        // stats at all — the diff cannot see incompleteness, only the verdict can.
        if !pre.clean() {
            tracing::info!("incremental reload: incomplete workspace scan; full rebuild");
            return self.note_incremental("incomplete_scan");
        }
        // The ticket cutoff is the fact frontier this publication may discharge, not the
        // coherence window. Events that arrived before this authoritative pre-scan are already
        // represented by `pre`; start the ABA window before the first analyzer read instead.
        let coherence_cutoff = self.observation();
        // A symlink target outside every registered root is retained under the
        // walked spelling during a full build. The point patch receives canonical
        // paths from the analyzer, so it cannot safely reconstruct that alias map;
        // let the full writer use the exact SourceSet projection instead.
        if project.search_roots.as_ref().is_some_and(|roots| {
            pre.stats.iter().any(|stat| roots.key_of_path(&stat.canonical).is_none())
        }) {
            tracing::info!("incremental reload: external symlink target needs a full rebuild");
            return self.note_incremental("external_symlink_target");
        }
        let diff = super::scan::classify_changes_with_roots(
            &stored_fp,
            &pre.stats,
            project.search_roots.as_ref(),
        );
        let bsl_added = diff
            .added
            .iter()
            .filter(|p| !bsl_conventions::str_has_extension(p, bsl_conventions::XML_EXTENSION))
            .count();
        let bsl_removed = diff
            .removed
            .iter()
            .filter(|p| !bsl_conventions::str_has_extension(p, bsl_conventions::XML_EXTENSION))
            .count();
        let bsl_modified = diff
            .modified
            .iter()
            .filter(|p| !bsl_conventions::str_has_extension(p, bsl_conventions::XML_EXTENSION))
            .count();
        let trigger = match (bsl_added, bsl_removed, bsl_modified, diff.modified.len()) {
            (0, 0, 0, 0) if diff.added.is_empty() && diff.removed.is_empty() => {
                "touch_or_empty_diff"
            }
            (added, 0, 0, _) if added > 0 => "bsl_added",
            (0, removed, 0, _) if removed > 0 => "bsl_removed",
            (0, 0, modified, _) if modified > 0 => "bsl_modified",
            (0, 0, 0, _) => "xml_only_delta",
            _ => "mixed_source_delta",
        };
        let relative_paths = |paths: &[String]| {
            paths
                .iter()
                .map(|path| {
                    relative_event_path(
                        Path::new(path),
                        workspace_root,
                        project.search_roots.as_ref(),
                    )
                })
                .collect::<Vec<_>>()
        };
        tracing::info!(
            generation,
            fact_seq = observed_through,
            trigger,
            added_paths = ?relative_paths(&diff.added),
            removed_paths = ?relative_paths(&diff.removed),
            modified_paths = ?relative_paths(&diff.modified),
            "graph workspace changes classified"
        );

        let Ok(stored_sig) =
            self.store.read(Some(base), wait, |snapshot| snapshot.graph.stored_sig_hashes())
        else {
            return self.note_incremental("published_graph_moved");
        };

        // Filter out XML modifications whose semantic structure is unchanged (<Version>, <Comment>, etc.)
        let mut changed_bsl = Vec::new();
        let mut added_or_removed_bsl = Vec::new();
        let mut metadata_paths = Vec::new();
        let mut xml_observations = Vec::new();
        for path_str in &diff.modified {
            if bsl_conventions::str_has_extension(path_str, bsl_conventions::XML_EXTENSION) {
                let path = PathBuf::from(path_str);
                let key =
                    project.search_roots.as_ref().and_then(|roots| pre.key_for_path(roots, &path));
                let current_sig = super::scan::xml_semantic_hash_file(&path).map(|h| {
                    u64::from_le_bytes(h[..8].try_into().expect("blake3 hash >= 8 bytes"))
                });
                xml_observations.push((path.clone(), current_sig));
                let stored_s = key.as_ref().and_then(|k| stored_sig.get(k)).copied().flatten();
                if current_sig.is_some() && current_sig == stored_s {
                    // Semantic no-op: version/comment/formatting edit in XML
                } else {
                    metadata_paths.push(path);
                }
            } else {
                changed_bsl.push(path_str.clone());
            }
        }
        for path in &diff.added {
            if bsl_conventions::str_has_extension(path, bsl_conventions::XML_EXTENSION) {
                let path = PathBuf::from(path);
                let hash = super::scan::xml_semantic_hash_file(&path).map(|h| {
                    u64::from_le_bytes(h[..8].try_into().expect("blake3 hash >= 8 bytes"))
                });
                xml_observations.push((path.clone(), hash));
                metadata_paths.push(path);
            } else {
                added_or_removed_bsl.push(path.clone());
            }
        }
        for path in &diff.removed {
            if bsl_conventions::str_has_extension(path, bsl_conventions::XML_EXTENSION) {
                let path = PathBuf::from(path);
                xml_observations.push((path.clone(), None));
                metadata_paths.push(path);
            } else {
                added_or_removed_bsl.push(path.clone());
            }
        }
        let (owner_ids, form_paths) = if metadata_paths.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            match crate::graph_db::local_metadata_delta(&project, &pre, &db_path, &metadata_paths) {
                Ok(Some(delta)) => delta,
                Ok(None) => return self.note_incremental("global_or_unsupported_xml_delta"),
                Err(error) => {
                    tracing::warn!(error = %error, "incremental metadata owner lookup failed");
                    return self.note_incremental("metadata_owner_lookup_failed");
                }
            }
        };
        changed_bsl.extend(added_or_removed_bsl);
        changed_bsl.sort();
        let drifted_bsl_paths: Vec<PathBuf> = changed_bsl.iter().map(PathBuf::from).collect();
        let current_changed_paths: Vec<PathBuf> = drifted_bsl_paths
            .iter()
            .filter(|path| pre.files.iter().any(|(_, current)| current == *path))
            .cloned()
            .collect();

        // Recompute each modified module's profile and partition into body-only
        // (signature unchanged) and signature-changed.
        let profiles = if current_changed_paths.is_empty() {
            rustc_hash::FxHashMap::default()
        } else {
            match crate::graph_db::recompute_module_profiles(
                &project,
                &pre.files,
                &current_changed_paths,
            ) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("incremental reload: profile recompute failed: {e}");
                    return self.note_incremental("profile_recompute_failed");
                }
            }
        };
        let mut sig_changed: Vec<(String, &crate::graph_db::ModuleProfile)> = Vec::new();
        let mut removed_profiles = Vec::new();
        for p in &drifted_bsl_paths {
            let key = p.to_string_lossy().into_owned();
            if !current_changed_paths.contains(p) {
                removed_profiles.push((key, crate::graph_db::ModuleProfile::removed()));
                continue;
            }
            let Some(profile) = profiles.get(&key) else {
                return self.note_incremental("no_recomputed_profile");
            };
            // The diff and this lookup use the same scan's walked alias and durable key.
            let stored_key =
                project.search_roots.as_ref().and_then(|roots| pre.key_for_path(roots, p));
            match stored_key.as_ref().and_then(|key| stored_sig.get(key)) {
                Some(Some(stored)) if *stored == profile.sig_hash => {} // body-only
                Some(Some(_)) | None => sig_changed.push((key, profile)),
                // A module the last full build could not READ has no stored signature, so a
                // modification to it can never be a body-only patch.
                Some(None) => return self.note_incremental("no_stored_signature"),
            }
        }
        sig_changed.extend(removed_profiles.iter().map(|(path, profile)| (path.clone(), profile)));

        // A signature change is handled by the caller-delta path: reproject the changed
        // module PLUS its resolved callers, when caller-delta-safe (no new resolvable
        // name). Otherwise fall back to a full rebuild.
        let mut changed_paths = drifted_bsl_paths.clone();
        if !sig_changed.is_empty() {
            let refs: Vec<(&str, &crate::graph_db::ModuleProfile)> =
                sig_changed.iter().map(|(f, p)| (f.as_str(), *p)).collect();
            let plan = self
                .store
                .read(Some(base), wait, |snapshot| {
                    snapshot.graph.caller_delta_plan(&refs, project.search_roots.as_ref())
                })
                .map_err(anyhow::Error::from)
                .and_then(|plan| plan);
            match plan {
                Ok(Some(callers)) => {
                    for c in callers {
                        if !changed_paths.contains(&c) {
                            changed_paths.push(c);
                        }
                    }
                }
                Ok(None) => {
                    tracing::info!(
                        "incremental reload: signature change not caller-delta-safe; full rebuild"
                    );
                    return self.note_incremental("caller_delta_not_safe");
                }
                Err(e) => {
                    tracing::warn!("incremental reload: caller-delta plan failed: {e}");
                    return self.note_incremental("caller_delta_failed");
                }
            }
        }

        // Bracket the patch with the shared pre-scan and a fresh post-scan,
        // mirroring the full build's straddle detection: a write landing after the
        // pre-scan marks the snapshot stale.
        let Some(fp_pre) = super::scan::fingerprint_of_project(&pre.stats, &project) else {
            tracing::info!(
                "incremental reload: portable file key or content hash unavailable; full rebuild"
            );
            return self.note_incremental("incomplete_portable_fingerprint");
        };
        // The patch is written into the database on disk — not necessarily the one served, when
        // an earlier install was refused after its write — so its base is read off the file.
        // Held while the patch is computed from the live file, and let go before the write
        // waits for every use of that file to end.
        let Ok(file_use) = self.store.use_file() else {
            return PublishAttemptOutcome::Refused(LoadFailure::new(
                LoadFailureReason::Superseded,
                super::types::SUPERSEDED_GRAPH_ERROR,
            ));
        };
        let mut file_use = Some(file_use);
        let base_publication = match publication_base(&db_path) {
            Ok(base) => base,
            Err(error) => return PublishAttemptOutcome::Refused(error),
        };
        let plan = PatchPlan {
            base: base_publication.clone(),
            target: fp_pre,
            changed: {
                let mut changed = changed_paths.clone();
                changed.sort();
                changed
            },
        };
        let built_at = chrono::Utc::now().to_rfc3339();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let patch = crate::graph_db::compute_body_patch_with_metadata(
                &project,
                &pre,
                &db_path,
                &changed_paths,
                &metadata_paths,
                &owner_ids,
                &form_paths,
                &xml_observations,
                GRAPH_BUILD_BATCH,
            )
            .map_err(LoadFailure::operation)?;
            let post_project = crate::graph::ProjectSnapshot::load_excluding(
                workspace_root,
                &self.cache_exclusions(),
            );
            let post = crate::graph::universe::ScannedUniverse::scan_project(&post_project);
            let fp_post = super::scan::fingerprint_of_project(&post.stats, &post_project)
                .ok_or_else(|| {
                    LoadFailure::operation("incomplete post-scan portable fingerprint")
                })?;
            // `pre.clean()` is guaranteed above; it stays in the formula so the two decisions
            // cannot drift apart if the gate ever moves. A hub delivery after the analyzer
            // started is the ABA window: the changed bytes may have been observed and then
            // restored before the post-scan.
            let hub_moved = self.observation() > coherence_cutoff;
            let hub_unhealthy = self.change_hub.as_ref().is_some_and(|hub| {
                !matches!(
                    hub.health_for(lock_recover(&self.hub_cursor).peek()),
                    crate::change_hub::Health::Healthy
                )
            });
            let force_stale = publish_force_stale(fp_pre, fp_post, pre.clean(), post.clean())
                || hub_moved
                || hub_unhealthy;
            // The reads of the file end here: the write below holds new ones back and waits for
            // the ones in flight.
            drop(file_use.take());
            let meta = crate::graph_db::GraphMeta {
                revision: generation,
                fingerprint: fp_pre,
                files: 0,
                built_at,
                publication_id: self.next_publication_id(),
            };
            let pause = match self.store.pause_for_replacement(INSTALL_READERS_WAIT) {
                super::snapshot::Pausing::Paused(pause) => pause,
                super::snapshot::Pausing::ReadersBusy => {
                    return Err(LoadFailure::new(
                        LoadFailureReason::TransientRefusal,
                        "a graph read outlasted the installation wait; the patch is prepared \
                         again on the next reload",
                    ));
                }
                super::snapshot::Pausing::Retired => return Err(lost_workspace_failure(self)),
            };
            let (modules, pause) = write_patch_in_place(
                self,
                &project,
                &pre,
                &patch,
                &meta,
                force_stale,
                &db_path,
                &plan,
                pause,
            )?;
            let reprojected = patch.reprojected_modules();
            let prepared =
                open_installed_replacement(self, generation, fp_pre, force_stale, &db_path, pause)?;
            // A point patch re-projected exactly what it was given. It proves nothing about
            // absence and nothing about the rest of the tree: it never looked there.
            let rewritten: std::collections::HashSet<bsl_search::FileKey> = project
                .search_roots
                .as_ref()
                .map(|roots| {
                    changed_paths.iter().filter_map(|path| pre.key_for_path(roots, path)).collect()
                })
                .unwrap_or_default();
            let recovery = self.recovery_proof_with_roots(
                generation,
                prepared.declared_unread(),
                crate::graph::snapshot::RecoveryCoverage::PatchedKeys { rewritten: &rewritten },
                project.search_roots.as_ref(),
            );
            Ok::<_, LoadFailure>((modules, reprojected, fp_pre, force_stale, prepared, recovery))
        }));

        match outcome {
            Ok(Ok((files, reprojected, fp, force_stale, prepared, recovery))) => {
                let publication_id = prepared.publication_id();
                let removed = bsl_removed;
                if force_stale {
                    tracing::warn!(
                        "incremental reload straddled a disk write; marking snapshot stale"
                    );
                }
                *lock_recover(&self.scan) = None;
                if let Err(error) = install_failure(self.install_prepared_snapshot(
                    prepared,
                    Published {
                        generation,
                        fingerprint: fp,
                        stale: false,
                        reload: ReloadState::Idle,
                        force_stale,
                        search_roots: project.search_roots.clone(),
                        observed_through: Some(observed_through),
                    },
                    GraphStatus::Ready { files },
                    // An incremental reload never runs under a forced reload, so it answers
                    // no recovery cutoff either: what it proves it proves by coverage.
                    None,
                    None,
                    recovery,
                )) {
                    return match error.reason {
                        LoadFailureReason::TransientRefusal
                        | LoadFailureReason::Superseded
                        | LoadFailureReason::Released => PublishAttemptOutcome::Refused(error),
                        LoadFailureReason::OperationError => PublishAttemptOutcome::FallBack,
                    };
                }
                #[cfg(test)]
                lock_recover(&self.incremental_decisions).push("published");
                // The body-only gate proved the stored topology unchanged.
                self.notify_published(false);
                tracing::info!(
                    files,
                    generation,
                    xml_files = pre
                        .stats
                        .iter()
                        .filter(|stat| bsl_conventions::str_has_extension(
                            &stat.path,
                            bsl_conventions::XML_EXTENSION
                        ))
                        .count(),
                    parsed_xml = xml_observations.iter().filter(|(_, hash)| hash.is_some()).count(),
                    trigger,
                    reprojected,
                    added = bsl_added,
                    modified = bsl_modified,
                    affected_callers = reprojected.saturating_sub(current_changed_paths.len()),
                    removed,
                    fact_seq = observed_through,
                    publication_id = publication_id.as_deref(),
                    "graph incremental reload complete"
                );
                PublishAttemptOutcome::Published
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    reason_code = "patch_operation_failed",
                    error = %e.message,
                    "incremental reload failed, falling back to full rebuild"
                );
                match e.reason {
                    LoadFailureReason::TransientRefusal
                    | LoadFailureReason::Superseded
                    | LoadFailureReason::Released => PublishAttemptOutcome::Refused(e),
                    LoadFailureReason::OperationError => {
                        self.note_incremental("patch_operation_failed")
                    }
                }
            }
            Err(payload) => {
                let panic_message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic");
                tracing::error!(
                    reason_code = "patch_panicked",
                    panic = panic_message,
                    "incremental reload panicked, falling back to full rebuild"
                );
                self.note_incremental("patch_panicked")
            }
        }
    }

    /// Publish an existing on-disk build instead of rebuilding, when it is still a
    /// valid, current, non-straddled match for the workspace.
    pub(super) fn try_publish_cached(
        &self,
        workspace_root: &Path,
        observed_through: u64,
    ) -> PublishAttemptOutcome {
        if self.is_superseded() {
            return PublishAttemptOutcome::Refused(LoadFailure::new(
                LoadFailureReason::Superseded,
                super::types::SUPERSEDED_GRAPH_ERROR,
            ));
        }
        let inspected = match self
            .inspect_unpublished(|graph| (graph.freshness_token(), graph.files().unwrap_or(0)))
        {
            Ok(inspected) => inspected,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "cached graph database cannot be reused: missing, incompatible format, or corrupted; rebuilding"
                );
                return PublishAttemptOutcome::FallBack;
            }
        };
        let ((revision, fingerprint, force_stale), files) = match inspected {
            (Ok(token), files) => (token, files),
            (Err(error), _) => {
                tracing::warn!(
                    error = %error,
                    "cached graph database has no valid freshness token; rebuilding"
                );
                return PublishAttemptOutcome::FallBack;
            }
        };
        let project =
            crate::graph::ProjectSnapshot::load_excluding(workspace_root, &self.cache_exclusions());
        // The cached graph remembers which stat identity each of its hashes was read under; a
        // file whose identity has not moved since then is not read again to prove it.
        if let Some(roots) = project.search_roots.as_ref() {
            if let Ok(observations) =
                self.inspect_unpublished(|graph| graph.stored_observations(roots))
            {
                super::content_hash::seed(observations);
            }
        }
        let now = crate::graph::universe::ScannedUniverse::scan_project(&project);
        let Some(fp_now) = super::scan::fingerprint_of_project(&now.stats, &project) else {
            tracing::warn!(
                "cached graph database cannot be checked: project roots are unavailable or a scanned file has no registered key; rebuilding"
            );
            return PublishAttemptOutcome::FallBack;
        };
        if force_stale {
            tracing::warn!(
                "cached graph database was marked incomplete or changed during publication; rebuilding"
            );
            return PublishAttemptOutcome::FallBack;
        }
        if !now.clean() {
            tracing::warn!(
                "cached graph database cannot be declared current: source scan is incomplete or unreadable; rebuilding"
            );
            return PublishAttemptOutcome::FallBack;
        }
        if fingerprint.topology != fp_now.topology {
            tracing::warn!(
                stored = fingerprint.topology,
                current = fp_now.topology,
                "cached graph database was built for a different project composition; rebuilding"
            );
            return PublishAttemptOutcome::FallBack;
        }
        if fingerprint.files != fp_now.files {
            tracing::warn!(
                stored = fingerprint.files,
                current = fp_now.files,
                "cached graph database has changed source contents; rebuilding"
            );
            return PublishAttemptOutcome::FallBack;
        }
        if !cache_is_reusable(force_stale, fingerprint, fp_now, now.clean()) {
            tracing::warn!("cached graph database freshness check failed; rebuilding");
            return PublishAttemptOutcome::FallBack;
        }
        let prepared = match self.prepare_snapshot_pool(revision, fingerprint, force_stale) {
            Ok(prepared) => prepared,
            Err(SnapshotPrepareError::Open(error)) => {
                tracing::warn!(
                    error = %error,
                    "cached graph snapshot could not be prepared; rebuilding"
                );
                return PublishAttemptOutcome::FallBack;
            }
            Err(error @ SnapshotPrepareError::Changed) => {
                let error = prepare_failure(error);
                return PublishAttemptOutcome::Refused(error);
            }
        };
        // The grant this path may give back, read while the graph still reads as loading and no
        // other claim is legal: still in the slot on the boot's path, and nothing at all on the
        // lazy one, whose builder took its ticket and carries it.
        let own_claim = lock_recover(&self.inner).claimed.map(|ticket| ticket.claim);

        // A cache served as it stands proves nothing fresh: it read nothing, walked nothing
        // for this publication, and may retire no obligation.
        let recovery = self.recovery_proof_with_roots(
            revision,
            prepared.declared_unread(),
            crate::graph::snapshot::RecoveryCoverage::None,
            project.search_roots.as_ref(),
        );
        *lock_recover(&self.scan) = None;
        if let Err(error) = install_failure(self.install_prepared_snapshot(
            prepared,
            Published {
                generation: revision,
                fingerprint,
                stale: false,
                reload: ReloadState::Idle,
                force_stale: false,
                search_roots: project.search_roots.clone(),
                observed_through: Some(observed_through),
            },
            GraphStatus::Ready { files },
            // Serving a cached build discharges no forced reload: it publishes the
            // state already on disk, not a rebuild of the newly declared configuration.
            None,
            None,
            recovery,
        )) {
            return PublishAttemptOutcome::Refused(error);
        }
        // The claim this path was granted has done its work: the cache is published and no
        // builder will take the ticket. Left in the slot it reads as a build in flight for the
        // rest of the generation, and every decision — retry, comparison, and the recovery
        // probe this publication's own unread metadata asks for — returns early on it.
        self.release_unused_claim(own_claim);
        // Exact fingerprint match (files AND topology): the persisted search
        // contexts were rendered against this same workspace state.
        self.notify_published(false);
        tracing::info!(files, revision, "reused cached graph database (workspace unchanged)");
        PublishAttemptOutcome::Published
    }

    /// Boot variant for a cached graph that no longer matches disk: publish it anyway —
    /// stale answers now beat "still indexing" for the minutes a full rebuild takes —
    /// and pre-claim the reload slot in the SAME lock hold, then let the normal reload
    /// lifecycle catch up (incrementally when eligible, else a full rebuild). The
    /// atomic Ready+Running publish keeps every existing guard honest:
    /// `freshness()`/`try_claim_reload` stay single-flight against the pre-claimed
    /// slot, and the publication carries no observation, so no mark is consumed against
    /// it — unlike a fingerprint-clean cached publish, THIS snapshot does not reflect the
    /// leftover marks' causes. A snapshot from a
    /// straddled build (`force_stale`) was never coherent and is not served. No
    /// `notify_published`: the publish hook must only run against a build that
    /// reflects current disk.
    pub(super) fn try_publish_stale_and_catch_up(
        &self,
        workspace_root: &Path,
    ) -> PublishAttemptOutcome {
        if self.is_superseded() {
            return PublishAttemptOutcome::Refused(LoadFailure::new(
                LoadFailureReason::Superseded,
                super::types::SUPERSEDED_GRAPH_ERROR,
            ));
        }
        let project =
            crate::graph::ProjectSnapshot::load_excluding(workspace_root, &self.cache_exclusions());
        // One look at the file answers the token, the topology and the size together.
        let Ok(inspected) = self.inspect_unpublished(|graph| {
            graph.freshness_token().map(|token| {
                (token, super::scan::graph_matches_live_project(graph, &project), graph.files())
            })
        }) else {
            return PublishAttemptOutcome::FallBack; // missing, truncated, or stale-schema → full rebuild
        };
        let Ok(((revision, fingerprint, force_stale), matches_live_project, files)) = inspected
        else {
            return PublishAttemptOutcome::FallBack;
        };
        if force_stale {
            return PublishAttemptOutcome::FallBack;
        }
        // Stale on FILES is what this path exists to serve — stale on TOPOLOGY is not. A build
        // made under a different extension topology resolves names differently, so publishing it
        // would answer questions about a project shape this workspace no longer has, and every
        // later reader would compare against the foreign topology adopted here and find it
        // consistent. The clean-match path above rejects it implicitly (its fingerprint covers
        // the topology); here it has to be said.
        //
        // Not publishing it costs the transition WITNESS, though: the whole-collection context
        // re-render is normally requested by a publish that observes its predecessor's topology
        // differing from its own, and refusing to publish leaves nothing to differ from. The
        // difference is visible right here — cached file versus live configuration — so the
        // request is raised directly and the rebuild's publish carries it.
        if !matches_live_project {
            tracing::info!(
                "cached graph database was built for another extension topology; \
                 rebuilding instead of serving it stale, and re-rendering search contexts"
            );
            super::state::lock_recover(&self.debt).record_hook(super::debt::HookDebt {
                topology: true,
                roots: false,
                marks: false,
            });
            return PublishAttemptOutcome::FallBack;
        }
        let files = files.unwrap_or(0);
        let prepared = match self.prepare_snapshot_pool(revision, fingerprint, force_stale) {
            Ok(prepared) => prepared,
            Err(SnapshotPrepareError::Open(error)) => {
                tracing::warn!(
                    error = %error,
                    "stale cached graph snapshot could not be prepared; rebuilding"
                );
                return PublishAttemptOutcome::FallBack;
            }
            Err(error @ SnapshotPrepareError::Changed) => {
                let error = prepare_failure(error);
                return PublishAttemptOutcome::Refused(error);
            }
        };

        // A stale cache, adopted on purpose while the catch-up is already claimed. It read
        // nothing and walked nothing for this publication, so it retires no obligation — and
        // its own unread metadata is still what it declares.
        let recovery = self.recovery_proof_with_roots(
            revision,
            prepared.declared_unread(),
            crate::graph::snapshot::RecoveryCoverage::None,
            project.search_roots.as_ref(),
        );
        if let Err(error) = install_failure(self.install_prepared_snapshot(
            prepared,
            Published {
                generation: revision,
                fingerprint,
                stale: true,
                // Pre-claimed: the catch-up spawned below owns the one reload slot.
                reload: ReloadState::Running,
                force_stale: false,
                search_roots: None,
                observed_through: None,
            },
            GraphStatus::Ready { files },
            // A placeholder publication; the catch-up build it spawns carries whatever
            // obligation is outstanding.
            None,
            None,
            recovery,
        )) {
            return PublishAttemptOutcome::Refused(error);
        }
        tracing::info!(
            files,
            revision,
            "published stale cached graph database; catch-up reload starting"
        );
        // The catch-up owns the slot this publication pre-claimed, so it needs a mandate of
        // its own: without one the loader falls back to reading whatever the debts say when it
        // gets there, which is the live re-read the ticket exists to remove.
        self.issue_handover_ticket();
        self.spawn_reload();
        #[cfg(test)]
        if let Some(hook) = self.handover_hook.clone() {
            hook(self);
        }
        PublishAttemptOutcome::Published
    }

    /// A failed initial load surfaces as `Failed`; a failed reload keeps the
    /// previous snapshot but flags `reload="failed"` so the agent sees it. A
    /// later drift check retries the reload (the throttle bounds the retry rate).
    ///
    /// A transient refusal keeps its retry budget. An operation error stops that obligation,
    /// but fresh external work may start a new graph epoch; terminal lease outcomes never do.
    pub(super) fn record_load_failure(&self, is_reload: bool, failure: LoadFailure) {
        let kind = match failure.reason {
            LoadFailureReason::TransientRefusal => super::debt::FailureKind::Transient,
            LoadFailureReason::OperationError => super::debt::FailureKind::Operation,
            LoadFailureReason::Superseded | LoadFailureReason::Released => {
                super::debt::FailureKind::Terminal
            }
        };
        // One debt for every way a build can end badly, and the schedule that paces its
        // retries belongs to it. The drift that arrived WHILE the build ran needs nothing
        // special: it is recorded against its own fact and is still owed, because no
        // publication observed it.
        let rearmed = self.record_admission_failure(kind, |inner| {
            inner.build_ticker = None;
            if is_reload {
                if let Some(p) = inner.published.as_mut() {
                    p.reload = ReloadState::Failed(failure.message);
                }
            } else {
                inner.status = GraphStatus::Failed(failure.message);
            }
        });
        self.settle_mark_obligation();
        // Started here only when a change delivered DURING this build opened a new epoch for
        // it: nothing else will deliver that change again. An ordinary failure waits for its
        // own schedule, and the watcher is what runs it — retrying on the failing thread would
        // spend the whole budget in a tight loop.
        if rearmed {
            self.drive();
        }
    }
}

/// Build the graph into the canonical path with the full publication bracket:
/// fingerprint the workspace before and after (so a build that straddled a disk write
/// is marked `force_stale`), stamp that marker plus the file count into the file's own
/// meta, then atomically rename the temp file into place — a reader sees the previous
/// database until the swap, never a half-written one. Shared by the lazy loader
/// ([`GraphState::run_load`]) and the fused cold build; when `chunk_sink` is present,
/// the search index's chunks are streamed from the same parse pass. Returns
/// a [`PublishedBuild`].
fn build_and_publish_graph_file(
    workspace_root: &Path,
    generation: u64,
    graph: &GraphState,
    chunk_sink: Option<&mut dyn ide::FusedChunkSink>,
) -> Result<PublishedBuild, LoadFailure> {
    // ONE project snapshot and ONE scanned universe serve the pre-fingerprint,
    // the build and the persisted `files` rows: every pre-publication pass sees
    // the same tree by construction. Only the straddle check walks again.
    let project =
        crate::graph::ProjectSnapshot::load_excluding(workspace_root, &graph.cache_exclusions());
    let pre = crate::graph::universe::ScannedUniverse::scan_project(&project);
    build_and_publish_scanned_inner(workspace_root, &project, &pre, generation, graph, chunk_sink)
}

/// The publication over an ALREADY-SCANNED universe — split from
/// [`build_and_publish_graph_file`] so a test can mutate the tree between the
/// pre-scan and the build and observe that the build does not see the mutation.
/// The build's temporary database, removed on every way out of the build.
///
/// Each build takes a name nobody else will reuse, so anything that leaves without publishing —
/// an operation error, or a panic unwinding out of the builder into the loader's `catch_unwind` —
/// leaves a database the size of the workspace behind for the life of the cache directory. The
/// published build is renamed out of this path, so removing it afterwards finds nothing.
#[cfg(test)]
struct TempBuildFile(std::path::PathBuf);

#[cfg(test)]
impl Drop for TempBuildFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
fn build_and_publish_scanned(
    workspace_root: &Path,
    project: &crate::graph::ProjectSnapshot,
    pre: &crate::graph::universe::ScannedUniverse,
    generation: u64,
    graph: &GraphState,
    chunk_sink: Option<&mut dyn ide::FusedChunkSink>,
) -> Result<PublishedBuild, LoadFailure> {
    build_and_publish_scanned_inner(workspace_root, project, pre, generation, graph, chunk_sink)
}

fn build_and_publish_scanned_inner(
    workspace_root: &Path,
    project: &crate::graph::ProjectSnapshot,
    pre: &crate::graph::universe::ScannedUniverse,
    generation: u64,
    graph: &GraphState,
    chunk_sink: Option<&mut dyn ide::FusedChunkSink>,
) -> Result<PublishedBuild, LoadFailure> {
    if !graph.validate_workspace_scope() {
        return Err(LoadFailure::operation(
            "workspace cache scope changed before graph candidate preparation",
        ));
    }
    let fp_pre = super::scan::fingerprint_of_project(&pre.stats, project)
        .ok_or_else(|| LoadFailure::operation("incomplete portable pre-scan fingerprint"))?;
    let out_path = graph.graph_db_path().expect("workspace graph has cache layout");
    // Asked before the minutes of building, and asked again at the rename: a database of a
    // newer format is neither built over nor replaced.
    let base = {
        let _use = graph
            .store
            .use_file()
            .map_err(|error| LoadFailure::new(LoadFailureReason::Superseded, error.to_string()))?;
        publication_base(&out_path)?
    };
    let cache = graph.cache().expect("workspace graph has cache layout");
    let candidate = cache.graph_candidate_path();
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(LoadFailure::operation)?;
    }
    let _candidate_lock = hold_candidate(graph, &cache.graph_candidate_lock_path())?;
    // A fused build streams the search chunks from its own parse pass, so only a build that
    // streams nothing may take a candidate prepared earlier instead of building.
    let reusable = match inspect_candidate(&candidate, base.as_deref(), fp_pre, pre.clean()) {
        CandidateState::Blocked(reason) => {
            tracing::error!(path = %candidate.display(), "{reason}");
            return Err(LoadFailure::new(LoadFailureReason::OperationError, reason));
        }
        CandidateState::Reusable { modules, revision } if chunk_sink.is_none() => {
            Some((modules, revision))
        }
        CandidateState::Reusable { .. } | CandidateState::Stale | CandidateState::Absent => None,
    };
    let (modules, force_stale, generation) = match reusable {
        Some((modules, revision)) => {
            tracing::info!(
                path = %candidate.display(),
                "installing the replacement prepared earlier from the same publication and \
                 sources; not building it again"
            );
            (modules, false, revision)
        }
        None => {
            let (modules, force_stale) = build_candidate(
                workspace_root,
                project,
                pre,
                generation,
                graph,
                chunk_sink,
                &candidate,
                base.as_deref(),
                fp_pre,
            )?;
            (modules, force_stale, generation)
        }
    };
    let mut delays =
        INSTALL_RETRY_DELAYS.iter().copied().chain(std::iter::repeat(INSTALL_RETRY_LAST));
    let pause = loop {
        match publish_or_discard(graph, &candidate, &out_path, base.as_deref(), true)? {
            Replaced::Done(pause) => break pause,
            Replaced::ReadersBusy => {
                let delay = delays.next().expect("the retry schedule never ends");
                tracing::info!(
                    retry_in_secs = delay.as_secs(),
                    "a graph read outlasted the installation wait; the published graph serves \
                     on, and the prepared replacement is installed again without rebuilding it"
                );
                if graph.stop.sleep(delay) || graph.lease_is_terminal() {
                    return Err(left_before_installation(
                        graph,
                        "the prepared graph replacement was not installed before this daemon left",
                    ));
                }
            }
        }
    };
    let _ = std::fs::remove_file(candidate_marker_path(&candidate));
    let prepared =
        open_installed_replacement(graph, generation, fp_pre, force_stale, &out_path, pause)?;
    let summary = GraphBuildModules { modules };
    // Borrowed from the walk, not cloned out of it: what the coverage needs is membership, and
    // the universe already holds every address it listed.
    let enumerated: std::collections::HashSet<bsl_search::FileKey> = project
        .search_roots
        .as_ref()
        .map(|roots| pre.stats.iter().filter_map(|stat| stat.key(roots)).collect())
        .unwrap_or_default();
    let recovery = graph.recovery_proof_with_roots(
        generation,
        prepared.declared_unread(),
        crate::graph::snapshot::RecoveryCoverage::WalkedKeys {
            scope: crate::graph::snapshot::recovery_scope_of(project),
            enumerated: &enumerated,
            complete: pre.clean(),
            straddled: force_stale,
        },
        project.search_roots.as_ref(),
    );
    Ok(PublishedBuild {
        generation,
        files: summary.modules,
        xml_files: pre
            .stats
            .iter()
            .filter(|stat| {
                bsl_conventions::str_has_extension(&stat.path, bsl_conventions::XML_EXTENSION)
            })
            .count(),
        fp_pre,
        force_stale,
        scan_roots: project.scan_roots.clone(),
        declaration_epoch: project.declaration_epoch,
        physical_topology: super::scan::topology_u64(&project.configs),
        search_roots: project.search_roots.clone(),
        prepared,
        recovery,
    })
}

/// Take the replacement for this build, waiting while another builder holds it. The access lock
/// does not cover it: a superseded owner lets the graph go once its reads return, while its
/// build may still be writing the replacement by path.
fn hold_candidate(
    graph: &GraphState,
    lock_path: &Path,
) -> Result<crate::workspace_lease::ExclusiveFileLock, LoadFailure> {
    let mut warned = false;
    loop {
        if let Some(lock) = crate::workspace_lease::ExclusiveFileLock::try_acquire(lock_path)
            .map_err(LoadFailure::operation)?
        {
            return Ok(lock);
        }
        if !warned {
            warned = true;
            tracing::info!(
                path = %lock_path.display(),
                "another builder still holds the graph replacement; waiting for it to let go"
            );
        }
        if graph.stop.sleep(CANDIDATE_RETRY) || graph.lease_is_terminal() {
            return Err(left_before_installation(
                graph,
                "the graph replacement was still held by another builder when this daemon left",
            ));
        }
    }
}

/// Why a build gave up waiting: the workspace is lost for good, or the daemon is stopping.
fn left_before_installation(graph: &GraphState, stopping: &str) -> LoadFailure {
    if graph.lease_is_terminal() {
        lost_workspace_failure(graph)
    } else {
        LoadFailure::new(LoadFailureReason::TransientRefusal, stopping)
    }
}

/// How often a build waiting for the replacement another builder holds tries again.
const CANDIDATE_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

/// The module count a full publication covers, whether it was built now or earlier.
struct GraphBuildModules {
    modules: usize,
}

/// How long an installation waits for the reads in flight before it lets the old graph serve on.
const INSTALL_READERS_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// When a replacement whose installation found readers is tried again, without being rebuilt.
const INSTALL_RETRY_DELAYS: [std::time::Duration; 4] = [
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(4),
    std::time::Duration::from_secs(8),
];
const INSTALL_RETRY_LAST: std::time::Duration = std::time::Duration::from_secs(30);

/// Who a replacement next to the graph belongs to and what it was prepared from, written before
/// the replacement itself: a candidate without one is not this program's to overwrite.
#[derive(serde::Serialize, serde::Deserialize)]
struct CandidateMarker {
    format: u32,
    base: Option<String>,
    /// Set once the replacement is stamped, checked and synced; its rows alone look finished
    /// long before that.
    #[serde(default)]
    complete: bool,
}

/// Replaced by a rename, never rewritten in place: a marker torn by a crash would read as one of
/// unknown origin and block every later build.
fn write_candidate_marker(candidate: &Path, marker: &CandidateMarker) -> Result<(), LoadFailure> {
    let path = candidate_marker_path(candidate);
    let staged = path.with_extension("owner.tmp");
    std::fs::write(&staged, serde_json::to_string(marker).map_err(LoadFailure::operation)?)
        .map_err(LoadFailure::operation)?;
    std::fs::rename(&staged, &path).map_err(LoadFailure::operation)
}

fn candidate_marker_path(candidate: &Path) -> PathBuf {
    candidate.with_extension("db.owner")
}

/// What a replacement found next to the graph may be used for.
enum CandidateState {
    Absent,
    /// This program's own, complete, prepared from the publication and sources a build would
    /// start from now: installed instead of rebuilt.
    Reusable {
        modules: usize,
        revision: u64,
    },
    /// This program's own, but prepared from another publication, other sources or never
    /// finished: the next build writes over it.
    Stale,
    /// Not provably this program's, or of a format it does not write: nothing is built over it.
    Blocked(String),
}

fn inspect_candidate(
    candidate: &Path,
    base: Option<&str>,
    fp_now: crate::graph_db::GraphFp,
    clean_now: bool,
) -> CandidateState {
    if !candidate.exists() {
        return CandidateState::Absent;
    }
    let marker = std::fs::read_to_string(candidate_marker_path(candidate))
        .ok()
        .and_then(|text| serde_json::from_str::<CandidateMarker>(&text).ok());
    let Some(marker) = marker else {
        return CandidateState::Blocked(format!(
            "a graph replacement of unknown origin is at {}; it is neither used nor overwritten — \
             move it away to let the graph be built",
            candidate.display()
        ));
    };
    let newer = super::snapshot::on_disk_identity(candidate)
        .ok()
        .flatten()
        .and_then(|identity| identity.schema_version)
        .is_some_and(|version| version > crate::graph_db::SCHEMA_VERSION);
    if marker.format > crate::graph_db::SCHEMA_VERSION || newer {
        return CandidateState::Blocked(format!(
            "the graph replacement at {} was prepared by a newer program; it is neither used nor \
             overwritten",
            candidate.display()
        ));
    }
    // An equal fingerprint over a walk that could not read everything proves nothing.
    if marker.format != crate::graph_db::SCHEMA_VERSION
        || marker.base.as_deref() != base
        || !marker.complete
        || !clean_now
    {
        return CandidateState::Stale;
    }
    match GraphDb::open(candidate).and_then(|db| {
        let (revision, fingerprint, force_stale) = db.freshness_token()?;
        Ok((revision, fingerprint, force_stale, db.files()?))
    }) {
        Ok((revision, fingerprint, false, modules)) if fingerprint == fp_now => {
            CandidateState::Reusable { modules, revision }
        }
        _ => CandidateState::Stale,
    }
}

/// Build the replacement into `candidate`, stamp what the post-scan found, and make it
/// ready to install — checked, closed and on disk — before anything waits on it.
#[allow(
    clippy::too_many_arguments,
    reason = "the build's own inputs, handed through from the one caller that prepares them"
)]
fn build_candidate(
    workspace_root: &Path,
    project: &crate::graph::ProjectSnapshot,
    pre: &crate::graph::universe::ScannedUniverse,
    generation: u64,
    graph: &GraphState,
    chunk_sink: Option<&mut dyn ide::FusedChunkSink>,
    candidate: &Path,
    base: Option<&str>,
    fp_pre: crate::graph_db::GraphFp,
) -> Result<(usize, bool), LoadFailure> {
    if !graph.validate_workspace_scope() {
        return Err(LoadFailure::operation(
            "workspace cache scope changed before graph candidate write",
        ));
    }
    let mut marker = CandidateMarker {
        format: crate::graph_db::SCHEMA_VERSION,
        base: base.map(str::to_owned),
        complete: false,
    };
    write_candidate_marker(candidate, &marker)?;
    let built_at = chrono::Utc::now().to_rfc3339();
    let meta = crate::graph_db::GraphMeta {
        revision: generation,
        fingerprint: fp_pre,
        files: 0,
        built_at,
        publication_id: graph.next_publication_id(),
    };
    // The ticket's fact frontier is kept separately as `Published::observed_through` for debt
    // and marks. It is not the coherence window: a delivery after admission but before this
    // authoritative pre-scan is already represented by `pre`. Start the ABA window after that
    // scan and immediately before the analyzer reads the files for lowering.
    let coherence_cutoff = graph.observation();
    #[cfg(test)]
    graph.full_builds_started.fetch_add(1, Ordering::SeqCst);
    let ticker = Arc::new(ide::GraphBuildTicker::default());
    graph.start_cold_build_ticker(Arc::clone(&ticker));
    let summary = match crate::graph_db::build_graph_database_inner(
        project,
        pre,
        candidate,
        GRAPH_BUILD_BATCH,
        &meta,
        chunk_sink,
        Some(ticker),
    ) {
        Ok(summary) => summary,
        Err(error) => return Err(LoadFailure::operation(error)),
    };
    if !graph.validate_workspace_scope() {
        let _ = std::fs::remove_file(candidate);
        let _ = std::fs::remove_file(candidate_marker_path(candidate));
        return Err(LoadFailure::operation(
            "workspace cache scope changed during graph candidate build",
        ));
    }
    #[cfg(test)]
    graph.enter_build_candidate_hook();
    // The post-scan derives a FRESH project snapshot AND a fresh walk: the straddle
    // check must see the world as it is now, or a topology/root change landing
    // mid-build would compare the frozen snapshot against itself and publish clean.
    let post_project =
        crate::graph::ProjectSnapshot::load_excluding(workspace_root, &graph.cache_exclusions());
    let post = crate::graph::universe::ScannedUniverse::scan_project(&post_project);
    let fp_post = super::scan::fingerprint_of_project(&post.stats, &post_project)
        .ok_or_else(|| LoadFailure::operation("incomplete portable post-scan fingerprint"))?;
    if !graph.validate_workspace_scope() {
        let _ = std::fs::remove_file(candidate);
        let _ = std::fs::remove_file(candidate_marker_path(candidate));
        return Err(LoadFailure::operation(
            "workspace cache scope changed during graph candidate build",
        ));
    }
    // A delivery after the analyzer started lowering may have landed and then been reverted
    // before the post-scan (the ABA case), leaving equal fingerprints that do not describe the
    // bytes the analyzer consumed. A delivery before this boundary is represented by `pre`.
    let hub_moved = graph.observation() > coherence_cutoff;
    let hub_unhealthy = graph.change_hub.as_ref().is_some_and(|hub| {
        !matches!(
            hub.health_for(lock_recover(&graph.hub_cursor).peek()),
            crate::change_hub::Health::Healthy
        )
    });
    let force_stale = publish_force_stale(fp_pre, fp_post, pre.clean(), post.clean())
        || hub_moved
        || hub_unhealthy;
    {
        let conn = rusqlite::Connection::open(candidate).map_err(LoadFailure::operation)?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('force_stale', ?1)",
            rusqlite::params![if force_stale { "1" } else { "0" }],
        )
        .map_err(LoadFailure::operation)?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('files', ?1)",
            rusqlite::params![summary.modules.to_string()],
        )
        .map_err(LoadFailure::operation)?;
    }
    // Checked, closed and synced here, before any read is held back for it.
    GraphDb::open(candidate).and_then(|db| db.quick_check()).map_err(|error| {
        LoadFailure::operation(format!("the built graph failed its check: {error}"))
    })?;
    // Opened for writing: Windows refuses to flush a read-only handle.
    let file =
        std::fs::OpenOptions::new().write(true).open(candidate).map_err(LoadFailure::operation)?;
    file.sync_all().map_err(LoadFailure::operation)?;
    marker.complete = true;
    write_candidate_marker(candidate, &marker)?;
    crate::graph_db::record_candidate(file.metadata().map_err(LoadFailure::operation)?.len());
    Ok((summary.modules, force_stale))
}

/// Open the replacement just renamed in and check it is the publication that was built. A
/// replacement already in place is not replaced again: an open that fails is retried, and one
/// that keeps failing leaves the graph unavailable rather than served from a guess.
fn open_installed_replacement(
    graph: &GraphState,
    generation: u64,
    fp_pre: crate::graph_db::GraphFp,
    force_stale: bool,
    out_path: &Path,
    pause: super::snapshot::ReplacementPause,
) -> Result<PreparedSnapshotPool, LoadFailure> {
    let mut last = None;
    for attempt in 0..OPEN_ATTEMPTS {
        if attempt > 0 && graph.stop.sleep(std::time::Duration::from_secs(1)) {
            break;
        }
        match graph.prepare_snapshot_pool(generation, fp_pre, force_stale) {
            Ok(mut prepared) => {
                prepared.hold_reads(pause);
                return Ok(prepared);
            }
            Err(error) => last = Some(prepare_failure(error)),
        }
    }
    let failure = last.unwrap_or_else(|| LoadFailure::operation("the daemon is stopping"));
    graph.store.mark_unusable(format!(
        "graph unavailable: the replacement renamed into {} could not be opened ({}); it is \
         rebuilt, not served from a guess",
        out_path.display(),
        failure.message
    ));
    Err(failure)
}

/// How many times a replacement already renamed into place is opened before the graph is
/// declared unavailable.
const OPEN_ATTEMPTS: usize = 3;

/// Whether a finished build must be marked `force_stale` — never served as a
/// coherent snapshot. Two ways to lose the claim: the tree moved while the build
/// ran (the fingerprints differ), or either bracketing scan could not speak for
/// the whole tree (short coverage or degraded identity). The second term is what
/// a fingerprint comparison alone cannot see: an unreadable EMPTY subtree leaves
/// both fingerprints equal while hiding an unknown amount of tree.
fn publish_force_stale(
    fp_pre: crate::graph_db::GraphFp,
    fp_post: crate::graph_db::GraphFp,
    pre_clean: bool,
    post_clean: bool,
) -> bool {
    fp_pre != fp_post || !pre_clean || !post_clean
}

/// Whether an on-disk build may be adopted as FRESH. `force_stale` means it never
/// was a coherent snapshot; a fingerprint mismatch means the workspace moved since
/// it was built; an unclean scan means `fp_now` describes only the part of the
/// tree the scan could see — equality against it proves nothing, so adoption is
/// refused even when the values match.
fn cache_is_reusable(
    force_stale: bool,
    stored: crate::graph_db::GraphFp,
    fp_now: crate::graph_db::GraphFp,
    scan_clean: bool,
) -> bool {
    !force_stale && scan_clean && stored == fp_now
}

/// What identifies one attempt to patch: the publication it is written over, the fingerprint it
/// leads to and the files it rewrites. An attempt of the same plan that outlasts its SQL budget
/// twice is not tried a third time.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct PatchPlan {
    base: Option<String>,
    target: crate::graph_db::GraphFp,
    changed: Vec<PathBuf>,
}

impl GraphState {
    /// Count one overrun of `plan`'s budget; the count is of this process only and starts again
    /// when the plan changes.
    fn note_patch_overrun(&self, plan: &PatchPlan) -> u32 {
        let mut overruns = lock_recover(&self.patch_overruns);
        match overruns.as_mut() {
            Some((seen, count)) if seen == plan => {
                *count += 1;
                *count
            }
            _ => {
                *overruns = Some((plan.clone(), 1));
                1
            }
        }
    }
}

/// Leave the graph file as a rolled-back write found it: served on when it still holds the
/// publication it held before — its modification time moved, and the store is told the file is
/// the same one — and unavailable when it holds anything else or cannot be looked at.
fn settle_after_rollback(
    graph: &GraphState,
    db_path: &Path,
    base: Option<&str>,
    pause: &super::snapshot::ReplacementPause,
) {
    match super::snapshot::on_disk_identity(db_path) {
        Ok(Some(identity)) if identity.publication_id.as_deref() == base => {
            graph.store.accept_rewritten_file(db_path, pause);
        }
        other => graph.store.mark_unusable(format!(
            "graph unavailable: after a rolled-back write the file in place is not the \
             publication it held before ({})",
            match other {
                Ok(_) => "another publication or none".to_owned(),
                Err(error) => error.to_string(),
            }
        )),
    }
}

/// Write a computed patch into the graph file in one transaction and commit it under the lease.
///
/// Reads are held back by `pause` while the SQL is applied, and the transaction counts as a use
/// of the file: an owner that loses the graph waits for its return, so the rollback of an
/// unfinished transaction comes before the file is handed on. The lease is held only for the
/// commit, never for the SQL. A commit that fails is not taken for "nothing happened": the file
/// is recovered and identified by its `publication_id`, and one that is neither the old
/// publication nor the new one leaves the graph unavailable.
#[allow(clippy::too_many_arguments)] // one call site; each argument is a distinct input of the write
fn write_patch_in_place(
    graph: &GraphState,
    project: &crate::graph::ProjectSnapshot,
    universe: &crate::graph::universe::ScannedUniverse,
    patch: &crate::graph_db::BodyPatch,
    meta: &crate::graph_db::GraphMeta,
    force_stale: bool,
    db_path: &Path,
    plan: &PatchPlan,
    pause: super::snapshot::ReplacementPause,
) -> Result<(usize, super::snapshot::ReplacementPause), LoadFailure> {
    if !graph.validate_workspace_scope() {
        return Err(LoadFailure::operation(
            "workspace cache scope changed before graph patch preparation",
        ));
    }
    let base = plan.base.as_deref();
    let Ok(_file_use) = graph.store.use_file() else {
        return Err(lost_workspace_failure(graph));
    };
    super::snapshot::recover_hot_journal(db_path).map_err(LoadFailure::operation)?;
    let current = super::snapshot::on_disk_identity(db_path)
        .map_err(|error| LoadFailure::operation(error.to_string()))?;
    if let Some(version) = newer_format(current.as_ref()) {
        return Err(newer_format_failure(db_path, version));
    }
    if current.and_then(|identity| identity.publication_id).as_deref() != base {
        return Err(LoadFailure::new(
            LoadFailureReason::TransientRefusal,
            format!(
                "graph database {} was replaced while this patch was prepared from an earlier \
                 one; the patch is prepared again",
                db_path.display()
            ),
        ));
    }
    let transaction = match crate::graph_db::begin_body_patch(
        db_path,
        project,
        universe,
        patch,
        meta,
        force_stale,
        crate::graph_db::PATCH_SQL_BUDGET,
    ) {
        Ok(transaction) => transaction,
        Err(error) => {
            settle_after_rollback(graph, db_path, base, &pause);
            return Err(match error {
                crate::graph_db::PatchError::Busy => LoadFailure::new(
                    LoadFailureReason::TransientRefusal,
                    "another process holds the graph file's write lock; the patch is prepared \
                     again on the next reload",
                ),
                crate::graph_db::PatchError::Budget => {
                    let overruns = graph.note_patch_overrun(plan);
                    tracing::warn!(overruns, "applying a graph patch outlasted its budget");
                    if overruns >= 2 {
                        LoadFailure::operation(
                            "applying the same graph patch outlasted its budget twice",
                        )
                    } else {
                        LoadFailure::new(
                            LoadFailureReason::TransientRefusal,
                            "applying a graph patch outlasted its budget; it is prepared again",
                        )
                    }
                }
                crate::graph_db::PatchError::Failed(error) => LoadFailure::operation(error),
            });
        }
    };
    if !graph.validate_workspace_scope() {
        drop(transaction);
        settle_after_rollback(graph, db_path, base, &pause);
        return Err(LoadFailure::operation(
            "workspace cache scope changed before graph patch publication",
        ));
    }
    let mut held = Some(transaction);
    let outcome = graph.lease.publish_short(&mut held, |held| {
        held.take().expect("the transaction is committed once").commit()
    });
    match outcome {
        LeaseOperationOutcome::Applied(_) => Ok((patch.modules(), pause)),
        LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(error)) => {
            drop(held);
            let message =
                format!("committing the graph patch into {} failed: {error}", db_path.display());
            let after = super::snapshot::recover_hot_journal(db_path)
                .and_then(|()| super::snapshot::on_disk_identity(db_path).map_err(Into::into));
            match after {
                Ok(Some(identity))
                    if identity.publication_id.as_deref() == Some(meta.publication_id.as_str()) =>
                {
                    tracing::warn!("{message}; the patch is committed all the same");
                    Ok((patch.modules(), pause))
                }
                Ok(Some(identity)) if identity.publication_id.as_deref() == base => {
                    graph.store.accept_rewritten_file(db_path, &pause);
                    Err(LoadFailure::operation(message))
                }
                _ => {
                    graph.store.mark_unusable(format!(
                        "graph unavailable: after a failed commit ({message}) the file in place \
                         is neither the publication it patched nor the patch"
                    ));
                    Err(LoadFailure::operation(message))
                }
            }
        }
        LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)) => {
            drop(held);
            settle_after_rollback(graph, db_path, base, &pause);
            Err(LoadFailure::operation(format!(
                "publishing the graph patch lease failed for {}: {error}",
                db_path.display()
            )))
        }
        LeaseOperationOutcome::TransientRefusal => {
            drop(held);
            settle_after_rollback(graph, db_path, base, &pause);
            Err(LoadFailure::new(
                LoadFailureReason::TransientRefusal,
                "this daemon could not establish ownership of the workspace's derived caches \
                 when the graph patch was ready; it was not committed",
            ))
        }
        LeaseOperationOutcome::Superseded | LeaseOperationOutcome::Released => {
            // Rolled back here, while the transaction still counts as a use of the file.
            drop(held);
            Err(lost_workspace_failure(graph))
        }
    }
}

/// What an attempt to rename a finished build over the shared database came to.
enum Replaced {
    /// The file is in place. Reads stay held until its pool is installed.
    Done(super::snapshot::ReplacementPause),
    /// A read outlasted [`INSTALL_READERS_WAIT`]: nothing was renamed, and the old file serves on.
    ReadersBusy,
}

impl std::fmt::Debug for Replaced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Done(_) => "Done",
            Self::ReadersBusy => "ReadersBusy",
        })
    }
}

/// Rename a finished build into the shared path, or refuse to.
///
/// A build takes minutes, and a newer daemon generation can claim the workspace's derived
/// caches at any point during one (see [`crate::workspace_lease`]). The rename runs with
/// ownership HELD rather than merely checked: a claim landing between a check and the rename
/// would let this build clobber what the new owner just published.
///
/// No handle of this process is open on the shared file when it is replaced: new reads are held
/// back, the reads in flight get [`INSTALL_READERS_WAIT`] to finish, and the idle handles close.
/// A read that outlasts the wait keeps the old file serving, and nothing is renamed. A rename
/// that fails is not taken for "nothing happened": the file in place is identified, and one
/// that is neither the replacement nor what it replaces leaves the graph unavailable.
///
/// A refused build is removed, unless `keep` — the prepared replacement of a full build, which
/// is kept to be installed again or proven stale.
fn publish_or_discard(
    graph: &GraphState,
    tmp_path: &Path,
    out_path: &Path,
    base: Option<&str>,
    keep: bool,
) -> Result<Replaced, LoadFailure> {
    let discard = || {
        if !keep {
            let _ = std::fs::remove_file(tmp_path);
        }
    };
    if !graph.validate_workspace_scope() {
        discard();
        return Err(LoadFailure::operation(
            "workspace cache scope changed before graph replacement publication",
        ));
    }
    if graph.lease.is_superseded() || graph.lease.is_released() {
        discard();
        return Err(lost_workspace_failure(graph));
    }
    let pause = match graph.store.pause_for_replacement(INSTALL_READERS_WAIT) {
        super::snapshot::Pausing::Paused(pause) => pause,
        super::snapshot::Pausing::ReadersBusy => return Ok(Replaced::ReadersBusy),
        super::snapshot::Pausing::Retired => {
            discard();
            return Err(lost_workspace_failure(graph));
        }
    };
    let replacement = super::snapshot::on_disk_identity(tmp_path)
        .ok()
        .flatten()
        .and_then(|identity| identity.publication_id);
    let outcome = graph.lease.publish_short(&mut (), |_| {
        // What an interrupted writer left is finished first — never carried over to the new
        // file or deleted by hand — so the identity read next is the file's settled one.
        super::snapshot::recover_hot_journal(out_path)
            .map_err(|error| PublishRefusal::Io(std::io::Error::other(error)))?;
        // Re-read under the fence: the database this build replaces must still be the one it
        // was prepared from, and a newer format is not replaced at all.
        let current = super::snapshot::on_disk_identity(out_path).map_err(PublishRefusal::Io)?;
        if let Some(version) = newer_format(current.as_ref()) {
            return Err(PublishRefusal::NewerFormat(version));
        }
        if current.and_then(|identity| identity.publication_id).as_deref() != base {
            return Err(PublishRefusal::StaleBase);
        }
        std::fs::rename(tmp_path, out_path).map_err(PublishRefusal::Io)
    });
    match outcome {
        LeaseOperationOutcome::Applied(()) => Ok(Replaced::Done(pause)),
        LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
            PublishRefusal::NewerFormat(version),
        )) => {
            discard();
            Err(newer_format_failure(out_path, version))
        }
        LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
            PublishRefusal::StaleBase,
        )) => {
            discard();
            Err(LoadFailure::new(
                LoadFailureReason::TransientRefusal,
                format!(
                    "graph database {} was replaced while this build was prepared from an \
                     earlier one; the build is prepared again",
                    out_path.display()
                ),
            ))
        }
        LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
            PublishRefusal::Io(error),
        )) => {
            let message = format!(
                "publishing graph database {} -> {} failed: kind={:?}, raw_os_error={:?}: {error}",
                tmp_path.display(),
                out_path.display(),
                error.kind(),
                error.raw_os_error()
            );
            // A file that cannot be looked at confirms nothing either way.
            let in_place = super::snapshot::on_disk_identity(out_path)
                .map(|found| found.map(|identity| identity.publication_id));
            if replacement.is_some() && matches!(&in_place, Ok(Some(id)) if *id == replacement) {
                tracing::warn!("{message}; the replacement is in place all the same");
                return Ok(Replaced::Done(pause));
            }
            let untouched = match &in_place {
                Ok(Some(id)) => id.as_deref() == base,
                Ok(None) => base.is_none(),
                Err(_) => false,
            };
            if untouched {
                // Confirmed untouched: the old file serves on once the pause is let go.
                discard();
                return Err(LoadFailure::new(LoadFailureReason::OperationError, message));
            }
            graph.store.mark_unusable(format!(
                "graph unavailable: after a failed replacement ({message}) the file in place is \
                 neither the publication it replaced nor the replacement"
            ));
            Err(LoadFailure::new(LoadFailureReason::OperationError, message))
        }
        LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)) => {
            let message = format!(
                "publishing graph database lease failed for {} -> {}: kind={:?}, raw_os_error={:?}: {error}",
                tmp_path.display(),
                out_path.display(),
                error.kind(),
                error.raw_os_error()
            );
            discard();
            Err(LoadFailure::new(LoadFailureReason::OperationError, message))
        }
        LeaseOperationOutcome::TransientRefusal => {
            discard();
            Err(LoadFailure::new(
                LoadFailureReason::TransientRefusal,
                "this daemon could not establish ownership of the workspace's derived caches \
                 when the graph build finished; the build was not published",
            ))
        }
        LeaseOperationOutcome::Superseded | LeaseOperationOutcome::Released => {
            discard();
            Err(lost_workspace_failure(graph))
        }
    }
}

/// The refusal of a publication by a process whose workspace was taken over or handed back.
fn lost_workspace_failure(graph: &GraphState) -> LoadFailure {
    if graph.lease.is_released() {
        LoadFailure::new(
            LoadFailureReason::Released,
            "workspace cache ownership was released before the graph build could be published",
        )
    } else {
        LoadFailure::new(
            LoadFailureReason::Superseded,
            "workspace cache ownership was superseded before the graph build could be published",
        )
    }
}

/// Why a finished build was not renamed over the shared database.
enum PublishRefusal {
    /// The shared database is of a format newer than this program writes.
    NewerFormat(u32),
    /// The shared database is no longer the publication this build was prepared from.
    StaleBase,
    Io(std::io::Error),
}

/// The publication a build replaces, or a refusal when the shared database is of a newer
/// format: this program neither reads such a database nor writes over it.
fn publication_base(out_path: &Path) -> Result<Option<String>, LoadFailure> {
    let identity = super::snapshot::on_disk_identity(out_path).map_err(LoadFailure::operation)?;
    if let Some(version) = newer_format(identity.as_ref()) {
        return Err(newer_format_failure(out_path, version));
    }
    Ok(identity.and_then(|identity| identity.publication_id))
}

/// The format of a database newer than this program writes, when it is one.
fn newer_format(identity: Option<&super::snapshot::OnDiskIdentity>) -> Option<u32> {
    identity
        .and_then(|identity| identity.schema_version)
        .filter(|version| *version > crate::graph_db::SCHEMA_VERSION)
}

fn newer_format_failure(out_path: &Path, version: u32) -> LoadFailure {
    tracing::error!(
        path = %out_path.display(),
        found = version,
        supported = crate::graph_db::SCHEMA_VERSION,
        "graph database has a newer format; it is not read or overwritten — run the newer \
         program or give this one a separate --cache-dir"
    );
    LoadFailure::new(
        LoadFailureReason::OperationError,
        format!(
            "graph database {} has format {version}, newer than this program's {}; it is not \
             read or overwritten",
            out_path.display(),
            crate::graph_db::SCHEMA_VERSION
        ),
    )
}

/// The outcome of one full build+publish pass: what was published, the identity it
/// was published under, and the scan roots of the snapshot that built it (for the
/// post-publish hub re-arm).
struct PublishedBuild {
    /// The revision the publication carries: the one this build was given, or that of a
    /// replacement prepared earlier and installed as it was built.
    generation: u64,
    files: usize,
    xml_files: usize,
    fp_pre: crate::graph_db::GraphFp,
    force_stale: bool,
    scan_roots: Vec<PathBuf>,
    /// The age of the build snapshot these roots were taken from, for the hub declaration:
    /// the build and its declaration are separated by an arbitrary delay, during which a
    /// newer build may declare first (github#184).
    declaration_epoch: u64,
    physical_topology: u64,
    search_roots: Option<bsl_search::WorkspaceRoots>,
    prepared: PreparedSnapshotPool,
    /// What this build proved about the recovery obligations outstanding when it was
    /// prepared — assembled here, where the universe it walked is still in hand.
    recovery: crate::graph::debt::RecoveryPublicationProof,
}

/// Translates the graph pass's [`ide::ChunkRow`] stream into the search store for the
/// fused cold build. Filters to files under the search source root, writes each file's
/// chunks + FTS + graph context with NO embedding (filled later by
/// [`SearchEngine::embed_pending_chunks_standalone`]), and records the blake3 of the file's bytes
/// as the skip hash — matching the standalone indexer so a later run reuses unchanged
/// files.
struct FusedChunkWriter<'e> {
    engine: &'e mut SearchEngine,
    lease: crate::workspace_lease::WorkspaceLease,
    /// The engine's root table, cloned so writing through `engine` stays possible while
    /// attributing paths. Every registered root is indexed, and a file's key is decided by the
    /// same longest-prefix attribution the rest of the index uses.
    roots: Option<bsl_search::WorkspaceRoots>,
    /// Both spellings are needed when no root table is configured: the scan can emit
    /// the walked path while Windows canonicalization adds a verbatim-path prefix.
    source_root: PathBuf,
    canonical_source_root: Option<PathBuf>,
    failure: Option<LoadFailure>,
    observation: Option<bsl_search::lifecycle::Batch>,
}

impl<'e> FusedChunkWriter<'e> {
    fn new(
        engine: &'e mut SearchEngine,
        source_path: PathBuf,
        lease: crate::workspace_lease::WorkspaceLease,
    ) -> Self {
        let roots = engine.workspace_roots().cloned();
        let canonical_source_root = source_path.canonicalize().ok();
        let observation = Some(bsl_search::lifecycle::Batch::new(
            engine.store().db_path(),
            bsl_search::lifecycle::Reason::ExplicitRebuild,
        ));
        Self {
            engine,
            lease,
            roots,
            source_root: source_path,
            canonical_source_root,
            failure: None,
            observation,
        }
    }

    fn finish(&mut self, outcome: bsl_search::lifecycle::Outcome) {
        if let Some(observation) = self.observation.take() {
            observation.finish(outcome);
        }
    }

    /// The store key of one emitted module, or `None` when it belongs to no registered root.
    fn key_of(&self, disk_path: &Path) -> Option<bsl_search::FileKey> {
        let Some(roots) = self.roots.as_ref() else {
            let rel = if let Ok(rel) = disk_path.strip_prefix(&self.source_root) {
                rel.to_path_buf()
            } else {
                // A file that can no longer be canonicalized still has a key: the rows carry
                // its canonical spelling already, and the read that follows reports it.
                let canonical_root = self.canonical_source_root.as_ref()?;
                let canonical =
                    disk_path.canonicalize().unwrap_or_else(|_| disk_path.to_path_buf());
                canonical.strip_prefix(canonical_root).ok()?.to_path_buf()
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            return (!rel.is_empty()).then(|| bsl_search::FileKey::configuration(rel));
        };
        roots.key_of_path(disk_path)
    }
}

impl ide::FusedChunkSink for FusedChunkWriter<'_> {
    fn emit_chunks(
        &mut self,
        rows: &[ide::ChunkRow],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let context = self.observation.as_ref().expect("fused writer not finished").context();
        context.in_scope(|| {
            // The producer emits a module's chunks consecutively, so group consecutive
            // same-path rows into one per-file write (each module appears once per batch).
            let mut groups: Vec<(String, Vec<bsl_search::Chunk>, Vec<Option<String>>)> = Vec::new();
            for row in rows {
                if groups.last().map(|(p, _, _)| p.as_str()) != Some(row.path.as_str()) {
                    groups.push((row.path.clone(), Vec::new(), Vec::new()));
                }
                let (_, chunks, ctxs) = groups.last_mut().expect("just pushed");
                chunks.push(bsl_search::Chunk {
                    kind: row.kind,
                    name: row.symbol.clone(),
                    is_export: row.is_export,
                    annotations: row.annotations.clone(),
                    line_start: row.line_start,
                    line_end: row.line_end,
                    text: row.text.clone(),
                });
                ctxs.push(row.graph_context.clone());
            }

            for (abs, chunks, ctxs) in &groups {
                // Graph paths use `/` even on Windows. A canonical Windows path can start
                // with `\\?\`; turning that prefix into `//?/` makes it unreadable there.
                #[cfg(windows)]
                let disk_path = std::path::PathBuf::from(abs.replace('/', "\\"));
                #[cfg(not(windows))]
                let disk_path = std::path::PathBuf::from(abs);
                // A module outside every registered root is not this index's business. With a table
                // configured that means "under no declared root"; without one it means "outside the
                // configuration", which is the prefix check this used to be — a separator boundary
                // included, so `…/cf_ext` is never mistaken for a file inside `…/cf`.
                let Some(key) = self.key_of(&disk_path) else {
                    continue;
                };
                // The fused writer is one of the index-write paths, so the spelling it reached
                // this module through is recorded beside the key: a removal arriving as that
                // path finds it even when re-attribution cannot (github#192). Best effort —
                // losing the pair costs the fallback, not the build.
                if let Err(error) = self.engine.record_workspace_path_spelling(&disk_path, &key) {
                    tracing::warn!(
                        path = ?disk_path,
                        "failed to record the fused writer's path spelling: {error}"
                    );
                }
                let bytes = match std::fs::read(&disk_path) {
                    Ok(b) => b,
                    Err(_) => {
                        bsl_search::lifecycle::decision(
                            self.engine.store().db_path(),
                            &key,
                            bsl_search::lifecycle::Reason::ReadError,
                            None,
                            None,
                        );
                        continue; // unreadable now → leave for the standalone indexer
                    }
                };
                let hash = bsl_search::content_blake3(&bytes);
                // Skip a file whose content is byte-identical to what is already stored: its
                // chunks and (paid-for) embeddings are kept. Re-ingesting would DELETE+reinsert
                // them with a NULL embedding and force a needless re-embed of the whole corpus on
                // every graph rebuild — the exact cost this avoids. The graph itself still rebuilds
                // fully (its own concern); only the embeddings stay incremental.
                //
                // Trade-off: the stored graph context records a method's *outbound* edges (whom it
                // calls / which metadata it reads). If a CALLEE is renamed or removed, an unchanged
                // caller's stored context can name the old target until that caller is itself
                // touched (or a `force_stale` rebuild re-ingests it). We accept this small
                // cross-file staleness in the embedding's context rather than re-embed every caller
                // of any changed symbol — embeddings are an approximation and this self-heals on the
                // next edit of the affected file.
                let stored = self.engine.store().file_hash(&key.root_id, &key.path);
                let reason = bsl_search::lifecycle::hash_reason(
                    stored.as_ref().map(|value| value.as_deref()).map_err(|_| ()),
                    &hash,
                );
                bsl_search::lifecycle::decision(
                    self.engine.store().db_path(),
                    &key,
                    reason,
                    stored.as_ref().ok().and_then(|value| value.as_deref()),
                    Some(&hash),
                );
                if reason == bsl_search::lifecycle::Reason::Unchanged {
                    continue;
                }
                match bsl_search::lifecycle::with_reason(reason, || {
                    self.lease.publish_checkpointed(|checkpoint| {
                        self.engine
                            .ingest_fused_file_checkpointed(&key, &hash, chunks, ctxs, checkpoint)
                    })
                }) {
                    LeaseOperationOutcome::Applied(()) => {}
                    LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                        error,
                    )) => {
                        self.failure = Some(LoadFailure::operation(&error));
                        return Err(error.into());
                    }
                    LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)) => {
                        self.failure = Some(LoadFailure::operation(error));
                        return Err(std::io::Error::other("fused ingest stopped").into());
                    }
                    LeaseOperationOutcome::TransientRefusal => {
                        self.failure = Some(LoadFailure::new(
                            LoadFailureReason::TransientRefusal,
                            "workspace cache ownership was temporarily refused during fused ingest",
                        ));
                        return Err(std::io::Error::other("fused ingest stopped").into());
                    }
                    LeaseOperationOutcome::Superseded => {
                        self.failure = Some(LoadFailure::new(
                            LoadFailureReason::Superseded,
                            "workspace cache ownership was superseded during fused ingest",
                        ));
                        return Err(std::io::Error::other("fused ingest stopped").into());
                    }
                    LeaseOperationOutcome::Released => {
                        self.failure = Some(LoadFailure::new(
                            LoadFailureReason::Released,
                            "workspace cache ownership was released during fused ingest",
                        ));
                        return Err(std::io::Error::other("fused ingest stopped").into());
                    }
                }
                #[cfg(test)]
                FUSED_FILE_COMMITTED_HOOK.with(|hook| {
                    if let Some(hook) = hook.borrow_mut().take() {
                        hook();
                    }
                });
            }
            Ok(())
        })
    }
}

#[cfg(test)]
fn bsl_module_total_filekeys(
    stored_fp: &std::collections::HashMap<bsl_search::FileKey, [u8; 32]>,
) -> usize {
    stored_fp
        .keys()
        .filter(|key| bsl_conventions::str_has_extension(&key.path, bsl_conventions::BSL_EXTENSION))
        .count()
}

/// [`stored_fingerprints_in`] over a file opened by path, for a test inspecting a database
/// it built by hand.
#[cfg(test)]
pub(crate) fn read_stored_fingerprints_with_roots(
    db_path: &Path,
) -> std::collections::HashMap<bsl_search::FileKey, [u8; 32]> {
    match rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    {
        Ok(conn) => crate::graph_db::stored_fingerprints_in(&conn),
        Err(_) => std::collections::HashMap::new(),
    }
}

#[cfg(test)]
mod module_total_tests {
    use super::bsl_module_total_filekeys;
    use bsl_search::FileKey;

    #[test]
    fn the_incremental_threshold_counts_case_variant_modules() {
        let mut stored = std::collections::HashMap::new();
        stored.insert(FileKey::configuration("CommonModules/A/Ext/Module.bsl"), [1u8; 32]);
        stored.insert(FileKey::configuration("CommonModules/B/Ext/Module.BSL"), [2u8; 32]);
        stored.insert(FileKey::configuration("CommonModules/B.xml"), [3u8; 32]);
        assert_eq!(
            bsl_module_total_filekeys(&stored),
            2,
            "Module.BSL — модуль и участвует в знаменателе порога"
        );
    }
}

#[cfg(test)]
mod vector_lifecycle_tests;

#[cfg(test)]
mod tests {

    #[test]
    fn workspace_cache_scope_graph_candidate_drift_rolls_back_before_publish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let project = crate::project::at(root).unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_project(
            &project,
            Some(&cache_parent.path().join("cache")),
            cache_parent.path(),
            None,
        )
        .unwrap();
        cache.ensure().unwrap();
        let owners = crate::state::OwnerStop::default();
        let transport = tokio_util::sync::CancellationToken::new();
        let root_for_hook = root.to_path_buf();
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_owner_stop(owners.clone())
            .with_scope_transport_stop(transport.clone())
            .with_build_candidate_hook_for_test(Arc::new(move |_| {
                fs::write(
                    root_for_hook.join("bsl-analyzer.toml"),
                    "[source]\nexclude = [\"generated\"]\n",
                )
                .unwrap();
            }));
        let snapshot = crate::graph::ProjectSnapshot::load_excluding(root, &cache.exclusions(root));
        let pre = crate::graph::universe::ScannedUniverse::scan_project(&snapshot);

        let error = build_and_publish_scanned_inner(root, &snapshot, &pre, 1, &graph, None)
            .err()
            .expect("scope drift during candidate build is terminal");
        assert!(!matches!(error.reason, LoadFailureReason::TransientRefusal));
        assert!(!cache.graph_db_path().exists(), "the candidate was not renamed into the old leaf");
        assert!(!cache.graph_candidate_path().exists(), "the drifted candidate was rolled back");
        assert!(owners.is_stopped());
        assert!(transport.is_cancelled());
    }

    /// A panic in the builder unwinds past the temp database into the loader's `catch_unwind`,
    /// which logs and gives up. Nothing after that knows the name — each build takes a fresh
    /// one — so the file has to be released on the way out or it stays for good.
    #[test]
    fn a_temp_build_file_is_removed_when_the_build_unwinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bsl-graph.db.building.1.0");
        std::fs::write(&path, b"a build in progress").unwrap();

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cleanup = super::TempBuildFile(path.clone());
            panic!("the builder panicked");
        }));

        assert!(unwound.is_err(), "the panic must still reach the caller");
        assert!(!path.exists(), "the temp database outlived the build that owned it");
    }
    use super::super::input::{enumerate_bsl_files, load_workspace_db, scan_roots};
    use super::super::scan::{
        classify_changes_with_roots, scan_file_stats, scan_stats_over_roots, FileStat,
        WorkspaceDiff,
    };
    use super::super::test_support::{
        meta_string, published_report, sample_workspace, seed_cache, wait_ready, wait_until,
        wait_until_within, write, write_common_module, write_extension_config,
        write_extension_workspace,
    };
    use super::*;
    use crate::graph_db::{build_graph_database, update_graph_database_bodies};
    use ide::Analysis;
    use rusqlite::Connection;
    use std::collections::HashSet;
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};
    use walkdir::WalkDir;

    /// A copy of the published graph restamped as the next publication, for a stand that
    /// installs a replacement without building one.
    fn next_publication_beside(path: &Path) -> (PathBuf, crate::graph_db::GraphFp) {
        let candidate = path.with_file_name("bsl-graph.pending.db");
        fs::copy(path, &candidate).unwrap();
        let conn = Connection::open(&candidate).unwrap();
        conn.execute_batch(
            "UPDATE meta SET value = '8' WHERE key = 'revision';
             UPDATE meta SET value = 'test-2' WHERE key = 'publication_id';",
        )
        .unwrap();
        drop(conn);
        let fingerprint = GraphDb::open(&candidate).unwrap().freshness_token().unwrap().1;
        (candidate, fingerprint)
    }

    /// A replacement waits for the read in flight: that read keeps answering its own
    /// generation, new reads are held back meanwhile, and once it returns the file is replaced
    /// and the next generation is served — from the file itself, with no copy for the reads.
    #[test]
    fn a_replacement_waits_for_a_held_read_then_serves_the_next_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, crate::graph::scan::workspace_fingerprint(root));
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        let path = graph_db_path(root);
        let (candidate, fingerprint) = next_publication_beside(&path);

        let held = graph.snapshot().expect("a read in flight");
        let publisher = {
            let (graph, candidate, path) = (graph.clone(), candidate.clone(), path.clone());
            std::thread::spawn(move || {
                publish_or_discard(&graph, &candidate, &path, Some("test-1"), true)
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(!publisher.is_finished(), "the replacement waits for the read in flight");
        assert_eq!(
            graph.store.read(None, Duration::from_millis(50), |_| ()),
            Err(crate::graph::GraphReadError::Busy),
            "a new read is held back meanwhile"
        );
        assert_eq!(held.graph.freshness_token().unwrap().0, 7, "the old read stays whole");
        drop(held);

        let Replaced::Done(pause) = publisher.join().unwrap().unwrap() else {
            panic!("the returned read let the replacement through");
        };
        let prepared =
            open_installed_replacement(&graph, 8, fingerprint, false, &path, pause).unwrap();
        assert!(matches!(
            graph.install_prepared_snapshot(
                prepared,
                Published {
                    generation: 8,
                    fingerprint,
                    stale: false,
                    reload: ReloadState::Idle,
                    force_stale: false,
                    search_roots: None,
                    observed_through: Some(0),
                },
                GraphStatus::Ready { files: 0 },
                None,
                None,
                crate::graph::debt::RecoveryPublicationProof::without_coverage(8),
            ),
            LeaseOperationOutcome::Applied(())
        ));
        assert_eq!(graph.read(|snapshot| snapshot.generation()), Ok(8));
        assert!(!candidate.exists(), "the replacement was renamed, not copied");
    }

    /// Four reads in flight, one of them the search-context provider's, all finish against the
    /// old generation before the replacement is renamed in.
    #[test]
    fn a_replacement_waits_for_four_concurrent_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, crate::graph::scan::workspace_fingerprint(root));
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let path = graph_db_path(root);
        let (candidate, _) = next_publication_beside(&path);

        let mut held: Vec<_> =
            (0..3).map(|_| graph.snapshot().expect("a read in flight")).collect();
        let provider =
            graph.store.checkout(None, Duration::from_millis(50)).expect("provider read");
        let publisher = {
            let (graph, candidate, path) = (graph.clone(), candidate.clone(), path.clone());
            std::thread::spawn(move || {
                publish_or_discard(&graph, &candidate, &path, Some("test-1"), true)
            })
        };
        // The replacement is released only by the last of the four.
        for snapshot in held.drain(..) {
            std::thread::sleep(Duration::from_millis(150));
            assert!(!publisher.is_finished(), "the replacement waits for every read");
            assert_eq!(snapshot.graph.freshness_token().unwrap().0, 7, "the old read stays whole");
            drop(snapshot);
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(!publisher.is_finished(), "the fourth read still holds the replacement");
        assert_eq!(provider.graph.freshness_token().unwrap().0, 7);
        drop(provider);
        assert!(matches!(publisher.join().unwrap().unwrap(), Replaced::Done(_)));
    }

    /// A request arriving during the pause waits for admission about two seconds, not for the
    /// whole installation wait, and then gets `Busy` while the replacement is still pending.
    #[test]
    fn a_read_during_the_pause_is_refused_after_the_admission_wait() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, crate::graph::scan::workspace_fingerprint(root));
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let path = graph_db_path(root);
        let (candidate, _) = next_publication_beside(&path);

        let held = graph.snapshot().expect("a read in flight");
        let publisher = {
            let (graph, candidate, path) = (graph.clone(), candidate.clone(), path.clone());
            std::thread::spawn(move || {
                publish_or_discard(&graph, &candidate, &path, Some("test-1"), true)
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let outcome = graph.store.read(None, Duration::from_millis(50), |_| ());
        let waited = started.elapsed();
        assert_eq!(outcome, Err(crate::graph::GraphReadError::Busy));
        assert!(waited >= Duration::from_millis(1900), "it waited for admission: {waited:?}");
        assert!(waited < Duration::from_millis(3500), "but not for the whole wait: {waited:?}");
        drop(held);
        assert!(matches!(publisher.join().unwrap().unwrap(), Replaced::Done(_)));
    }

    /// A read that outlasts the installation wait keeps the old graph serving: nothing is
    /// renamed, the prepared replacement stays for the next attempt, and lending resumes.
    #[test]
    fn a_read_outlasting_the_wait_keeps_the_old_graph_and_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, crate::graph::scan::workspace_fingerprint(root));
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let path = graph_db_path(root);
        let (candidate, _) = next_publication_beside(&path);

        let held = graph.snapshot().expect("a read that will not finish in time");
        let started = std::time::Instant::now();
        let outcome = publish_or_discard(&graph, &candidate, &path, Some("test-1"), true).unwrap();
        assert!(matches!(outcome, Replaced::ReadersBusy), "{outcome:?}");
        assert!(started.elapsed() >= INSTALL_READERS_WAIT, "it waited out the whole bound");
        assert!(candidate.exists(), "the replacement is kept, not rebuilt later");
        assert_eq!(meta_string(&path, "revision"), "7", "nothing was renamed");
        drop(held);
        assert_eq!(graph.read(|snapshot| snapshot.generation()), Ok(7), "the old graph serves on");
    }

    /// A full build writes one replacement next to the graph and renames it in: it is counted
    /// as written, nothing is copied for it, and neither it nor its marker nor a temporary
    /// file is left behind.
    #[test]
    fn a_full_build_writes_one_counted_replacement_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let before = crate::graph_db::copy_audit();
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let after = crate::graph_db::copy_audit();
        assert!(after.candidates > before.candidates, "the replacement was counted");
        let size = fs::metadata(graph_db_path(root)).unwrap().len();
        assert!(after.candidate_bytes >= before.candidate_bytes + size);
        let leftovers: Vec<_> = fs::read_dir(graph_db_path(root).parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("pending") || name.contains(".building."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Put the seeded graph where a build would have left its replacement before installing
    /// it, with a marker naming what it was prepared from.
    fn leave_a_candidate(root: &Path, marker: Option<&str>) -> PathBuf {
        seed_cache(root, crate::graph::scan::workspace_fingerprint(root));
        let path = graph_db_path(root);
        let candidate = path.with_file_name("bsl-graph.pending.db");
        fs::rename(&path, &candidate).unwrap();
        if let Some(marker) = marker {
            fs::write(candidate_marker_path(&candidate), marker).unwrap();
        }
        candidate
    }

    /// A replacement prepared from the same publication and sources, left when the process
    /// stopped before installing it, is installed instead of built again.
    #[test]
    fn a_replacement_left_before_installation_is_installed_without_rebuilding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let format = crate::graph_db::SCHEMA_VERSION;
        leave_a_candidate(
            root,
            Some(&format!(r#"{{"format":{format},"base":null,"complete":true}}"#)),
        );
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0, "nothing was built");
        assert_eq!(graph.read(|snapshot| snapshot.generation()), Ok(7), "the one left is served");
    }

    /// A replacement another builder still holds — a superseded owner whose build outlived its
    /// hold on the graph — is not touched until that builder lets it go.
    #[test]
    fn a_replacement_held_by_another_builder_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let candidate = graph_db_path(root).with_file_name("bsl-graph.pending.db");
        fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        let held = crate::workspace_lease::ExclusiveFileLock::try_acquire(
            &candidate.with_file_name("bsl-graph.replacement.lock"),
        )
        .unwrap()
        .expect("nobody else holds the replacement");
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0, "nothing was built");
        assert!(!candidate.exists(), "the held replacement is left alone");
        drop(held);
        wait_ready(&graph);
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 1);
    }

    /// A replacement whose build stopped before it was checked and synced is built again,
    /// however finished its rows look.
    #[test]
    fn an_unfinished_replacement_is_rebuilt_over() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let format = crate::graph_db::SCHEMA_VERSION;
        leave_a_candidate(root, Some(&format!(r#"{{"format":{format},"base":null}}"#)));
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 1, "it was built again");
    }

    /// A walk that could not read everything proves nothing by an equal fingerprint, so a
    /// replacement left earlier is not installed on its word.
    #[cfg(unix)]
    #[test]
    fn a_replacement_is_not_reused_over_an_unclean_walk() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hidden = root.join("CommonModules").join("Hidden");
        fs::create_dir_all(&hidden).unwrap();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&hidden).is_ok() {
            // Privileged: nothing is unreadable to this process.
            fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let format = crate::graph_db::SCHEMA_VERSION;
        leave_a_candidate(
            root,
            Some(&format!(r#"{{"format":{format},"base":null,"complete":true}}"#)),
        );
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let built = graph.full_builds_started.load(Ordering::SeqCst);
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(built, 1, "it was built again");
    }

    /// A replacement of this program's, prepared from another publication, is written over by
    /// the next build rather than installed.
    #[test]
    fn a_stale_replacement_of_this_program_is_rebuilt_over() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let format = crate::graph_db::SCHEMA_VERSION;
        leave_a_candidate(root, Some(&format!(r#"{{"format":{format},"base":"elsewhere-1"}}"#)));
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 1, "it was built again");
    }

    /// A file at the replacement's place that cannot be shown to be this program's is neither
    /// used nor written over: the build stops and says where it is.
    #[test]
    fn a_replacement_of_unknown_origin_blocks_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let candidate = leave_a_candidate(root, None);
        let before = fs::read(&candidate).unwrap();
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_until(&graph, "the build to be refused", || {
            matches!(graph.status(), GraphStatus::Failed(_))
        });
        assert!(
            matches!(graph.status(), GraphStatus::Failed(message) if message.contains("unknown origin")),
            "{:?}",
            graph.status()
        );
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0);
        assert_eq!(fs::read(&candidate).unwrap(), before, "it is left as it was");
    }

    /// A database of a newer format belongs to a newer program: this one neither reads it, nor
    /// spends a build on replacing it, nor renames anything over it.
    #[test]
    fn a_newer_database_format_is_neither_built_over_nor_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, crate::graph::scan::workspace_fingerprint(root));
        let path = graph_db_path(root);
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                [(crate::graph_db::SCHEMA_VERSION + 1).to_string()],
            )
            .unwrap();
        let before = fs::read(&path).unwrap();

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_until(&graph, "the load to be refused", || {
            matches!(graph.status(), GraphStatus::Failed(_))
        });
        assert!(
            matches!(graph.status(), GraphStatus::Failed(message) if message.contains("newer")),
            "{:?}",
            graph.status()
        );
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0, "no build was spent");
        assert_eq!(fs::read(&path).unwrap(), before, "the newer database is untouched");

        let temp = graph_build_path(&path);
        fs::write(&temp, b"candidate").unwrap();
        let error = publish_or_discard(&graph, &temp, &path, None, false).unwrap_err();
        assert_eq!(error.reason, LoadFailureReason::OperationError);
        assert!(!temp.exists(), "the refused build is discarded");
        assert_eq!(fs::read(&path).unwrap(), before, "and nothing is renamed over it");
    }

    /// A warm cache produced by the previous projection rules cannot be adopted even
    /// when file fingerprints match: the graph is rebuilt through the existing schema gate.
    #[test]
    fn previous_projection_format_is_rebuilt_before_serving() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, crate::graph::scan::workspace_fingerprint(root));
        let path = graph_db_path(root);
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                [(crate::graph_db::SCHEMA_VERSION - 1).to_string()],
            )
            .unwrap();

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 1);
        assert_eq!(
            meta_string(&path, "schema_version"),
            crate::graph_db::SCHEMA_VERSION.to_string()
        );
    }

    /// A build replaces exactly the publication it was prepared from. Another one standing at
    /// the shared path by the rename makes the build stale, not the new answer.
    #[test]
    fn a_build_prepared_from_a_replaced_database_is_not_published() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, crate::graph::scan::workspace_fingerprint(root));
        let path = graph_db_path(root);
        let before = fs::read(&path).unwrap();
        let graph = GraphState::for_workspace(root.to_path_buf());
        let temp = graph_build_path(&path);

        fs::write(&temp, b"candidate").unwrap();
        let error = publish_or_discard(&graph, &temp, &path, Some("another-1"), false).unwrap_err();
        assert_eq!(error.reason, LoadFailureReason::TransientRefusal, "{}", error.message);
        assert!(!temp.exists());
        assert_eq!(fs::read(&path).unwrap(), before);

        fs::write(&temp, b"candidate").unwrap();
        publish_or_discard(&graph, &temp, &path, Some("test-1"), false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"candidate", "its own base is replaced");
    }

    /// Two publications of the same content are two publications; a database served again as
    /// it stands is the same one.
    #[test]
    fn every_publication_has_its_own_identity_and_a_reused_one_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let path = graph_db_path(root);
        let load = || {
            let graph = GraphState::for_workspace(root.to_path_buf());
            graph.ensure_loading();
            wait_ready(&graph);
        };

        load();
        let built = meta_string(&path, "publication_id");
        let fingerprint = meta_string(&path, "fingerprint");
        load();
        assert_eq!(meta_string(&path, "publication_id"), built, "a reused database keeps it");

        Connection::open(&path)
            .unwrap()
            .execute("UPDATE meta SET value = '1' WHERE key = 'force_stale'", [])
            .unwrap();
        load();
        assert_eq!(meta_string(&path, "fingerprint"), fingerprint, "the same content");
        assert_ne!(meta_string(&path, "publication_id"), built, "is published anew");
    }

    /// The fused pass writes the search rows itself, so it must key them the way the rest of the
    /// index does: a module of a declared extension belongs to that extension, not to the
    /// configuration and not to nowhere. Dropping it (the old behaviour) leaves the extension out
    /// of a fused cold boot entirely, and the deletion reconcile cannot put it back — it only
    /// removes.
    #[test]
    fn the_fused_writer_keys_each_module_by_its_own_root() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        let configuration = workspace.join("cf");
        let extension = dir.path().join("outside-ext");
        fs::create_dir_all(&configuration).unwrap();
        fs::create_dir_all(&extension).unwrap();
        let configuration_module = configuration.join("A.bsl");
        let extension_module = extension.join("B.bsl");
        fs::write(&configuration_module, "Процедура Первая()\nКонецПроцедуры").unwrap();
        fs::write(&extension_module, "Процедура Вторая()\nКонецПроцедуры").unwrap();
        let outsider = dir.path().join("nowhere").join("C.bsl");
        fs::create_dir_all(outsider.parent().unwrap()).unwrap();
        fs::write(&outsider, "Процедура Третья()\nКонецПроцедуры").unwrap();

        let mut engine = bsl_search::SearchEngine::fts_only(&dir.path().join("search.db")).unwrap();
        let (roots, _rejected) = bsl_search::WorkspaceRoots::build(
            &workspace,
            &configuration,
            std::slice::from_ref(&extension),
        );
        let extension_key = roots
            .root_of(&extension_module, &extension_module.canonicalize().unwrap())
            .expect("the extension's module has an owner");
        engine.set_workspace_roots(roots);

        let row = |path: &std::path::Path, symbol: &str| ide::ChunkRow {
            path: path.to_string_lossy().replace('\\', "/"),
            symbol: symbol.to_owned(),
            kind: bsl_search::ChunkKind::Procedure,
            is_export: false,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 2,
            text: format!("Процедура {symbol}()\nКонецПроцедуры"),
            graph_context: None,
        };
        {
            let mut writer = FusedChunkWriter::new(
                &mut engine,
                configuration.clone(),
                crate::workspace_lease::WorkspaceLease::unmanaged(),
            );
            ide::FusedChunkSink::emit_chunks(
                &mut writer,
                &[
                    row(&configuration_module, "Первая"),
                    row(&extension_module, "Вторая"),
                    row(&outsider, "Третья"),
                ],
            )
            .expect("the sink writes its batch");
        }

        let rows: Vec<(String, String)> = engine
            .store()
            .all_files_in_collection("code")
            .unwrap()
            .into_iter()
            .map(|(key, _hash)| (key.root_id, key.path))
            .collect();
        assert!(
            rows.contains(&(String::new(), "A.bsl".to_owned())),
            "the configuration's module keeps its key: {rows:?}",
        );
        assert!(
            rows.contains(&(extension_key.root_id.clone(), extension_key.path.clone())),
            "the extension's module is written under its own root: {rows:?}",
        );
        assert!(
            !rows.iter().any(|(_, path)| path.ends_with("C.bsl")),
            "a module under no registered root is still not this index's business: {rows:?}",
        );
        // The fused writer is one of the index-write paths: the spelling it reached each
        // module through is recorded beside the key, so a removal arriving as that path finds
        // it (github#192).
        assert_eq!(
            engine.store().path_spelling_key(&extension_module.to_string_lossy(), "code").unwrap(),
            Some(extension_key.clone()),
            "the fused writer records the spelling it reached the module through",
        );
    }

    /// BUD-02. A fused build reports the outcome of the admission it was granted, and the
    /// ticket is what says which accounts paid for it.
    ///
    /// The ticket is TAKEN out of the slot the moment the build starts and carried for as long
    /// as it runs. A failure recorded after that carry has been dropped finds an empty slot and
    /// names the primary lane by default — so a build only the marks financed came back as a
    /// primary failure and minted a retry budget nothing had bought.
    #[test]
    fn a_failed_fused_build_answers_for_the_accounts_that_paid() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        let mut engine = bsl_search::SearchEngine::fts_only(&cache.search_db_path()).unwrap();

        // Marks nobody consumed, and nothing else at all: the one lane that can pay here.
        {
            let mut debt = lock_recover(&graph.debt);
            debt.place_marks(std::time::Instant::now(), 5, 1);
            debt.settle_marks(std::time::Instant::now(), false);
        }
        assert!(graph.try_begin_external_build(), "the boot takes the claim");
        assert_eq!(
            graph.claimed_ticket().expect("the claim granted a mandate").sponsors,
            super::super::debt::Sponsors { primary: false, marks: true },
            "the stand needs an admission only the marks paid for",
        );

        // The install is refused, so the build ends the way a fenced one does.
        graph.refused_installs.store(1, std::sync::atomic::Ordering::SeqCst);
        let failure = graph
            .run_fused_cold_build(&mut engine, root, 0)
            .expect_err("a refused install ends the fused build");
        assert!(!matches!(failure.reason, LoadFailureReason::Superseded), "{failure}");

        assert!(
            matches!(graph.status(), GraphStatus::Failed(_)),
            "the build holding the mandate reported no outcome of its own: {:?}",
            graph.status(),
        );
        assert!(
            !lock_recover(&graph.debt).owes_failed(),
            "the outcome of a marks-sponsored build minted a primary retry budget",
        );
        lease.release();
    }

    /// A load attempted after the workspace was let go says the workspace was released, not
    /// that the file is momentarily held elsewhere.
    #[test]
    fn a_load_after_the_workspace_was_released_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        lease.release();
        wait_until(&graph, "the file to be let go", || graph.released());

        graph.run_load(false);

        assert!(
            matches!(graph.status(), GraphStatus::Failed(message) if message.contains("released")),
            "{:?}",
            graph.status()
        );
    }

    /// A claim nobody will build on is given back.
    ///
    /// The warm-cache path takes the external claim, publishes the database already on disk
    /// and returns — nothing takes the ticket afterwards. Left in the slot it reads as a build
    /// in flight for the rest of the generation: every decision returns early, so no retry, no
    /// comparison and, above all, no recovery probe ever runs for a publication whose own
    /// metadata says it could not read everything.
    #[test]
    fn a_warm_cache_publication_gives_its_claim_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());

        // A real build first, so there is a cache to reuse.
        graph.ensure_loading();
        wait_ready(&graph);

        // The boot of the next generation: an idle graph over that cache.
        let next = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        assert!(next.try_begin_external_build(), "the boot takes the claim");
        assert!(
            matches!(next.try_publish_cached(root, 0), super::PublishAttemptOutcome::Published),
            "the stand needs the cache to be reusable",
        );

        assert!(
            next.claimed_ticket().is_none(),
            "the finished cache path left its grant in the slot",
        );
        assert!(lock_recover(&next.inner).building.is_none(), "and nothing is carrying it either",);
        lease.release();
    }

    /// A boot whose cached publication is refused gives its claim back with the failure.
    ///
    /// The boot takes the claim itself and no builder takes the ticket afterwards. The success
    /// path gave it back; every refusal returned with it still in the slot, and the slot reads
    /// as a build in flight — so the retry the refusal scheduled could not be started by the
    /// request or the boot entry that exist to start it. Both cached branches: the fresh cache
    /// and the stale one served while a catch-up runs.
    #[test]
    fn a_refused_boot_publication_gives_its_claim_back() {
        refused_boot_publication_gives_its_claim_back(false);
    }

    /// [`a_refused_boot_publication_gives_its_claim_back`] on the stale cache the boot serves
    /// while its catch-up runs.
    #[test]
    fn a_refused_stale_boot_publication_gives_its_claim_back() {
        refused_boot_publication_gives_its_claim_back(true);
    }

    fn refused_boot_publication_gives_its_claim_back(stale: bool) {
        let which = if stale { "stale cache" } else { "fresh cache" };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let mut fingerprint = super::super::scan::workspace_fingerprint(root);
        if stale {
            fingerprint.files = fingerprint.files.wrapping_add(1);
        }
        seed_cache(root, fingerprint);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let graph = GraphState::for_workspace(root.to_path_buf());
        let mut engine = bsl_search::SearchEngine::fts_only(&cache.search_db_path()).unwrap();

        graph.refused_installs.store(1, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            graph.start_workspace_graph(&mut engine, root),
            super::super::types::FusedStartup::Standalone
        ));
        assert_eq!(
            graph.refused_installs.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{which}: the fixture needs the boot's own install to be the refused one",
        );
        assert!(
            matches!(graph.status(), GraphStatus::Failed(_)),
            "{which}: the refusal is the boot's outcome: {:?}",
            graph.status(),
        );
        assert!(!graph.build_in_flight(), "{which}: the refused boot left its grant in the slot",);

        assert_eq!(
            graph.debt_standing(std::time::Instant::now()).failed,
            Some(super::super::debt::Ripeness::Now),
            "{which}: the fixture needs the refusal's retry due now",
        );
        graph.ensure_first_build();
        wait_until(&graph, "the retry a request asked for to start", || {
            !matches!(graph.status(), GraphStatus::Failed(_))
        });
        wait_ready(&graph);
    }

    /// The point-patch path records `published` for a patch that was installed, and not for one
    /// whose install was refused.
    ///
    /// Each half on a workspace of its own: a refused patch has already renamed its database
    /// into place, so the same edit is no longer a body-only change against it.
    #[cfg(not(windows))]
    #[test]
    fn an_incremental_patch_is_recorded_published_only_once_installed() {
        let body_edited = || {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            sample_workspace(&root);
            let graph = GraphState::for_workspace(root.clone());
            graph.ensure_loading();
            wait_ready(&graph);
            write(
                &root,
                "CommonModules/Сервер/Ext/Module.bsl",
                "&НаСервере\nФункция Считать() Экспорт\nЗначение = 1;\nВозврат Значение;\nКонецФункции",
            );
            lock_recover(&graph.incremental_decisions).clear();
            (dir, root, graph)
        };

        let (_refused_dir, root, graph) = body_edited();
        graph.refused_installs.store(1, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            graph.try_incremental_reload(&root, 2, 0),
            PublishAttemptOutcome::Refused(_)
        ));
        let refused = lock_recover(&graph.incremental_decisions).clone();
        assert!(
            !refused.contains(&"published"),
            "a patch whose install was refused was recorded as published: {refused:?}",
        );

        let (_installed_dir, root, graph) = body_edited();
        assert!(matches!(
            graph.try_incremental_reload(&root, 2, 0),
            PublishAttemptOutcome::Published
        ));
        let installed = lock_recover(&graph.incremental_decisions).clone();
        assert!(
            installed.contains(&"published"),
            "an installed patch was not recorded as published: {installed:?}",
        );
    }

    #[test]
    fn superseded_fused_writer_stops_mutating() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(lease.owns_caches_now());
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        let mut engine = bsl_search::SearchEngine::fts_only(&cache.search_db_path()).unwrap();

        let newer = std::sync::Arc::new(std::sync::Mutex::new(None));
        let newer_from_hook = std::sync::Arc::clone(&newer);
        let cache_from_hook = cache.clone();
        FUSED_FILE_COMMITTED_HOOK.with(|hook| {
            hook.replace(Some(Box::new(move || {
                let claim = crate::workspace_lease::WorkspaceLease::claim_cache(&cache_from_hook);
                assert!(claim.owns_caches_now());
                *newer_from_hook.lock().unwrap() = Some(claim);
            })));
        });

        let error = graph
            .run_fused_cold_build(&mut engine, root, 0)
            .expect_err("takeover after the first file must stop fused ingest");
        assert!(error.to_string().contains("ownership was superseded"), "{error}");
        assert!(lease.is_superseded(), "the second file's fence observes the new owner");
        assert_eq!(
            engine.store().all_files_in_collection("code").unwrap().len(),
            1,
            "the first fenced file stays committed and the second is not written",
        );

        let graph_path = cache.graph_db_path();
        assert!(!graph_path.exists(), "the rejected build never replaces the canonical graph");
        assert!(
            fs::read_dir(graph_path.parent().unwrap()).unwrap().all(|entry| {
                !entry.unwrap().file_name().to_string_lossy().starts_with("bsl-graph.db.building.")
            }),
            "the rejected build removes its temp graph",
        );

        newer.lock().unwrap().take().unwrap().release();
    }

    #[test]
    fn superseded_build_is_discarded_after_owner_release() {
        struct RefusingSink;
        impl ide::FusedChunkSink for RefusingSink {
            fn emit_chunks(
                &mut self,
                _chunks: &[ide::ChunkRow],
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Err(std::io::Error::other("ownership was refused").into())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let old = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(old.clone());
        let canonical = cache.graph_db_path();
        let temp = graph_build_path(&canonical);
        let other_temp = graph_build_path(&canonical);
        assert_ne!(temp, other_temp, "same-process builds must not share a temporary database");
        fs::write(&other_temp, b"other-build-in-progress").unwrap();

        fs::write(&canonical, b"new-owner-graph").unwrap();
        fs::write(&temp, b"old-daemon-graph").unwrap();
        let newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(!old.owns_caches_now());
        assert!(old.is_superseded());
        newer.release();

        publish_or_discard(&graph, &temp, &canonical, None, false).unwrap_err();
        assert_eq!(fs::read(&canonical).unwrap(), b"new-owner-graph");
        assert!(!temp.exists(), "normal refusal removes only this build's temp file");
        assert_eq!(fs::read(&other_temp).unwrap(), b"other-build-in-progress");

        let mut sink = RefusingSink;
        assert!(build_and_publish_graph_file(root, 1, &graph, Some(&mut sink)).is_err());
        assert_eq!(fs::read(&canonical).unwrap(), b"new-owner-graph");
        assert!(
            fs::read_dir(canonical.parent().unwrap()).unwrap().all(|entry| {
                let entry = entry.unwrap();
                entry.path() == other_temp
                    || !entry.file_name().to_string_lossy().starts_with("bsl-graph.db.building.")
            }),
            "fused failure removes its temp file and leaves the other build alone",
        );
        assert_eq!(fs::read(&other_temp).unwrap(), b"other-build-in-progress");
    }

    /// A drift that arrives while a build is running is recorded as a pending nudge and never
    /// delivered again — the hub batch that carried it is already acknowledged. If the build
    /// then fails, that signal is the only thing standing between the graph and a permanent
    /// `Failed`, and it must be spent exactly once, not re-armed for ever.
    #[test]
    fn fresh_nudge_during_failed_build_rearms_graph_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        lock_recover(&graph.inner).status = GraphStatus::Loading;
        graph.record_change_quietly(graph.observation());

        graph.record_load_failure(false, LoadFailure::operation("forced"));

        assert!(
            !matches!(graph.status(), GraphStatus::Failed(_)),
            "the change recorded during the build was dropped",
        );

        // The same change must not open a second epoch: spent when it was used, a drift that
        // arrived once would otherwise re-arm the graph after every later failure.
        lock_recover(&graph.inner).status = GraphStatus::Loading;
        graph.record_load_failure(false, LoadFailure::operation("forced again"));
        assert!(
            matches!(graph.status(), GraphStatus::Failed(_)),
            "the same signal re-armed the graph a second time",
        );
        assert!(
            graph.owes_change().is_some(),
            "the change itself is still owed: only a publication that observes it answers it",
        );
    }

    #[test]
    fn original_transient_arms_withheld_build_until_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        let temp = cache.root().join("transient.building");
        let canonical = cache.root().join("transient.db");
        fs::write(&temp, b"candidate").unwrap();

        let held = lease.hold_file_lock_for_test();
        let error = publish_or_discard(&graph, &temp, &canonical, None, false).unwrap_err();
        drop(held);

        assert_eq!(error.reason, LoadFailureReason::TransientRefusal);
        graph.record_load_failure(false, error);
        assert!(graph.owes_failed());
        assert!(matches!(
            lease.publish_short(&mut (), |_| Ok::<_, std::convert::Infallible>(
                "ownership returned"
            )),
            LeaseOperationOutcome::Applied("ownership returned")
        ));
        assert!(!temp.exists());
        assert!(!canonical.exists());

        graph.ensure_loading();
        assert!(!matches!(graph.status(), GraphStatus::Failed(_)));
        wait_ready(&graph);
        assert!(!graph.owes_failed());
    }

    #[test]
    fn terminal_publish_refusals_do_not_rearm() {
        let released_dir = tempfile::tempdir().unwrap();
        let released_cache = crate::cache::WorkspaceCacheLayout::for_workspace(released_dir.path());
        released_cache.ensure().unwrap();
        let released_lease = crate::workspace_lease::WorkspaceLease::claim_cache(&released_cache);
        let released_graph = GraphState::for_workspace_with_cache(
            released_dir.path().to_path_buf(),
            released_cache.clone(),
        )
        .with_lease(released_lease.clone());
        released_lease.release();
        let released_temp = released_cache.root().join("released.building");
        fs::write(&released_temp, b"candidate").unwrap();
        let released_error = publish_or_discard(
            &released_graph,
            &released_temp,
            &released_cache.root().join("released.db"),
            None,
            false,
        )
        .unwrap_err();
        assert_eq!(released_error.reason, LoadFailureReason::Released);
        released_graph.record_load_failure(false, released_error);
        assert!(!released_graph.owes_failed());

        let superseded_dir = tempfile::tempdir().unwrap();
        let superseded_cache =
            crate::cache::WorkspaceCacheLayout::for_workspace(superseded_dir.path());
        superseded_cache.ensure().unwrap();
        let old = crate::workspace_lease::WorkspaceLease::claim_cache(&superseded_cache);
        let superseded_graph = GraphState::for_workspace_with_cache(
            superseded_dir.path().to_path_buf(),
            superseded_cache.clone(),
        )
        .with_lease(old.clone());
        let newer = crate::workspace_lease::WorkspaceLease::claim_cache(&superseded_cache);
        let superseded_temp = superseded_cache.root().join("superseded.building");
        fs::write(&superseded_temp, b"candidate").unwrap();
        let superseded_error = publish_or_discard(
            &superseded_graph,
            &superseded_temp,
            &superseded_cache.root().join("superseded.db"),
            None,
            false,
        )
        .unwrap_err();
        assert_eq!(superseded_error.reason, LoadFailureReason::Superseded);
        superseded_graph.record_load_failure(false, superseded_error);
        assert!(!superseded_graph.owes_failed());
        newer.release();
    }

    #[test]
    fn operation_error_waits_for_fresh_work_without_losing_its_origin() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        let prepared = cache.root().join("prepared.building");
        fs::write(&prepared, b"candidate").unwrap();
        lease.fail_next_restamp_for_test();
        let error =
            publish_or_discard(&graph, &prepared, &cache.root().join("output.db"), None, false)
                .expect_err("a lease restamp failure is a real operation error");
        assert_eq!(error.reason, LoadFailureReason::OperationError);

        let held = lease.hold_file_lock_for_test();
        graph.record_load_failure(false, error);
        drop(held);

        assert!(graph.owes_failed());
        graph.drive();
        assert!(
            matches!(graph.status(), GraphStatus::Failed(_)),
            "an operation error must wait for fresh work rather than retry itself",
        );

        graph.nudge_rebuild();
        assert!(
            !matches!(graph.status(), GraphStatus::Failed(_)),
            "fresh work did not open a new epoch for the stopped budget",
        );
        wait_ready(&graph);
    }

    /// Driven through the real cold-build entry point, not through the walk it happens
    /// to call.
    ///
    /// A gate that scans a hand-built snapshot proves the mechanism and nothing about
    /// the caller: it stays green while the primary build path loads a snapshot with no
    /// exclusions at all, which is exactly the shape this defect had. The file count is
    /// the observable because it is what the build persists.
    #[test]
    fn a_cold_build_does_not_take_modules_from_its_own_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone());

        let clean = build_and_publish_graph_file(root, 1, &graph, None).unwrap();
        // An uninstalled publication holds its file: it is let go before the next one replaces it.
        let clean_files = clean.files;
        drop(clean);

        // The same workspace, plus a module dropped inside the analyzer's own cache.
        crate::graph::test_support::write_common_module(
            &cache.root().join("vendor"),
            "Чужой",
            true,
            "&НаСервере\nФункция Чуж() Экспорт КонецФункции",
        );
        write(
            root,
            "CommonModules/Клиент/Ext/Module.bsl",
            "&НаКлиенте\nПроцедура Главная() Экспорт\nСервер.Считать();\nДополнительный.Extra();\nКонецПроцедуры",
        );
        let with_vendored = build_and_publish_graph_file(root, 2, &graph, None).unwrap();

        assert_eq!(
            with_vendored.files, clean_files,
            "a module under the cache entered the graph built from the workspace"
        );
    }

    #[test]
    fn fresh_and_cached_publication_paths_propagate_final_install_refusal() {
        let refuse_install = super::super::snapshot::refuse_snapshot_install_for_test;

        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh_root = fresh_dir.path();
        sample_workspace(fresh_root);
        let fresh = GraphState::for_workspace(fresh_root.to_path_buf());
        let built = build_and_publish_graph_file(fresh_root, 1, &fresh, None).unwrap();
        refuse_install();
        let fresh_install = fresh.install_prepared_snapshot(
            built.prepared,
            Published {
                generation: 1,
                fingerprint: built.fp_pre,
                stale: false,
                reload: ReloadState::Idle,
                force_stale: built.force_stale,
                search_roots: built.search_roots,
                observed_through: Some(0),
            },
            GraphStatus::Ready { files: built.files },
            None,
            None,
            built.recovery,
        );
        assert!(matches!(
            fresh_install,
            LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
                SnapshotInstallError::Changed
            ))
        ));
        assert!(fresh.snapshot().is_none(), "fresh publish never becomes ready");

        let clean_dir = tempfile::tempdir().unwrap();
        let clean_root = clean_dir.path();
        sample_workspace(clean_root);
        seed_cache(clean_root, workspace_fingerprint(clean_root));
        let clean = GraphState::for_workspace(clean_root.to_path_buf());
        refuse_install();
        let PublishAttemptOutcome::Refused(failure) = clean.try_publish_cached(clean_root, 0)
        else {
            panic!("clean adoption must preserve the final install refusal")
        };
        clean.record_load_failure(false, failure);
        assert!(clean.owes_failed());
        assert!(clean.snapshot().is_none(), "clean adoption never becomes ready");

        let stale_dir = tempfile::tempdir().unwrap();
        let stale_root = stale_dir.path();
        sample_workspace(stale_root);
        let mut stale_fingerprint = workspace_fingerprint(stale_root);
        stale_fingerprint.files = stale_fingerprint.files.wrapping_add(1);
        seed_cache(stale_root, stale_fingerprint);
        let stale = GraphState::for_workspace(stale_root.to_path_buf());
        refuse_install();
        let PublishAttemptOutcome::Refused(failure) =
            stale.try_publish_stale_and_catch_up(stale_root)
        else {
            panic!("stale adoption must preserve the final install refusal")
        };
        stale.record_load_failure(false, failure);
        assert!(stale.owes_failed());
        assert!(stale.snapshot().is_none(), "stale adoption never becomes ready");
    }

    #[cfg(not(windows))]
    #[test]
    fn incremental_publication_retries_changed_snapshot_install() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nЗначение = 1;\nВозврат Значение;\nКонецФункции",
        );
        super::super::snapshot::refuse_snapshot_install_for_test();

        assert!(matches!(
            graph.try_incremental_reload(root, 2, 0),
            PublishAttemptOutcome::Refused(LoadFailure {
                reason: LoadFailureReason::TransientRefusal,
                ..
            })
        ));
        assert_eq!(
            graph.snapshot().map(|snapshot| snapshot.generation),
            None,
            "the replaced file is not served under the generation it no longer holds"
        );
    }

    /// A graph built under a lease of its own, then one module edited: what writing a point
    /// patch into the published file takes, short of the reload that would do it.
    struct PatchFixture {
        cache: crate::cache::WorkspaceCacheLayout,
        graph: GraphState,
        db: PathBuf,
        project: crate::graph::ProjectSnapshot,
        universe: crate::graph::universe::ScannedUniverse,
        patch: crate::graph_db::BodyPatch,
        meta: crate::graph_db::GraphMeta,
        plan: PatchPlan,
    }

    fn patch_fixture(root: &Path) -> PatchFixture {
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        drop(build_and_publish_graph_file(root, 1, &graph, None).unwrap());
        let db = graph.graph_db_path().unwrap();
        let publication_id = graph.next_publication_id();
        let (project, universe, patch, meta, plan) =
            edit_and_compute_patch(root, &db, publication_id);
        PatchFixture { cache, graph, db, project, universe, patch, meta, plan }
    }

    /// Edit the sample module's body and work out the patch for it against the graph at `db`.
    fn edit_and_compute_patch(
        root: &Path,
        db: &Path,
        publication_id: String,
    ) -> (
        crate::graph::ProjectSnapshot,
        crate::graph::universe::ScannedUniverse,
        crate::graph_db::BodyPatch,
        crate::graph_db::GraphMeta,
        PatchPlan,
    ) {
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nЗначение = 1;\nВозврат Значение;\nКонецФункции",
        );
        let module = root.join("CommonModules/Сервер/Ext/Module.bsl").canonicalize().unwrap();
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        let fingerprint =
            crate::graph::scan::fingerprint_of_project(&universe.stats, &project).unwrap();
        let patch = crate::graph_db::compute_body_patch(
            &project,
            &universe,
            db,
            std::slice::from_ref(&module),
            stdx::batch::BatchBudget::files(1),
        )
        .unwrap();
        let meta = crate::graph_db::GraphMeta {
            revision: 2,
            fingerprint,
            files: 0,
            built_at: "now".into(),
            publication_id,
        };
        let plan = PatchPlan {
            base: publication_base(db).unwrap(),
            target: fingerprint,
            changed: vec![module],
        };
        (project, universe, patch, meta, plan)
    }

    fn stored_revision(db: &Path) -> u64 {
        crate::graph_query::GraphDb::open(db).unwrap().freshness_token().unwrap().0
    }

    /// A point patch is written into the published file itself: the same file holds the new
    /// publication afterwards, nothing is copied or replaced, and nothing is left beside it.
    #[test]
    fn a_point_patch_is_written_into_the_published_file_and_copies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let db = &graph_db_path(root);
        let same_file = db.with_extension("same-file");
        fs::hard_link(db, &same_file).unwrap();
        let before = fs::read(db).unwrap();
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере
Функция Считать() Экспорт
Значение = 1;
Возврат Значение;
КонецФункции",
        );

        let outcome = graph.try_incremental_reload(root, 2, 0);

        assert!(matches!(outcome, PublishAttemptOutcome::Published), "{:?}", graph.status());
        assert_eq!(stored_revision(db), 2);
        assert_ne!(
            fs::read(&same_file).unwrap(),
            before,
            "the file the graph is served from changed"
        );
        assert_eq!(fs::read(&same_file).unwrap(), fs::read(db).unwrap());
        let leftovers: Vec<_> = fs::read_dir(db.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("pending") || name.contains(".building."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The owner of the workspace changed before the commit: the unfinished transaction is
    /// rolled back, the file still holds the publication it held, and the refusal says why.
    #[test]
    fn lost_ownership_before_the_commit_rolls_the_patch_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let fixture = patch_fixture(root);
        let before = fs::read(&fixture.db).unwrap();
        let newer = crate::workspace_lease::WorkspaceLease::claim_cache(&fixture.cache);
        let super::super::snapshot::Pausing::Paused(pause) =
            fixture.graph.store.pause_for_replacement(std::time::Duration::from_secs(1))
        else {
            panic!("nothing holds the graph file");
        };

        let Err(error) = write_patch_in_place(
            &fixture.graph,
            &fixture.project,
            &fixture.universe,
            &fixture.patch,
            &fixture.meta,
            false,
            &fixture.db,
            &fixture.plan,
            pause,
        ) else {
            panic!("a patch lost to a new owner was committed");
        };

        assert_eq!(error.reason, LoadFailureReason::Superseded);
        assert_eq!(publication_base(&fixture.db).unwrap(), fixture.plan.base);
        assert_eq!(stored_revision(&fixture.db), 1);
        assert_eq!(fs::read(&fixture.db).unwrap(), before, "nothing of the patch is left");
        newer.release();
    }

    /// SQL that outlasts its budget is interrupted and rolled back, and the same plan doing it
    /// twice is not tried a third time.
    #[test]
    fn a_patch_that_outlasts_its_budget_is_rolled_back_and_the_repeat_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let fixture = patch_fixture(root);

        let refused = crate::graph_db::begin_body_patch(
            &fixture.db,
            &fixture.project,
            &fixture.universe,
            &fixture.patch,
            &fixture.meta,
            false,
            std::time::Duration::ZERO,
        );

        assert!(matches!(refused, Err(crate::graph_db::PatchError::Budget)));
        drop(refused);
        assert_eq!(publication_base(&fixture.db).unwrap(), fixture.plan.base);
        assert_eq!(stored_revision(&fixture.db), 1);
        assert_eq!(fixture.graph.note_patch_overrun(&fixture.plan), 1);
        assert_eq!(fixture.graph.note_patch_overrun(&fixture.plan), 2);
        let other = PatchPlan { base: Some("elsewhere-1".into()), ..fixture.plan.clone() };
        assert_eq!(fixture.graph.note_patch_overrun(&other), 1, "another base starts again");
    }

    /// Another process holding the file's write lock is waited for briefly, and then the patch
    /// is refused for a later attempt with nothing written.
    #[test]
    fn a_patch_does_not_wait_long_for_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let fixture = patch_fixture(root);
        let other = rusqlite::Connection::open(&fixture.db).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();

        let refused = crate::graph_db::begin_body_patch(
            &fixture.db,
            &fixture.project,
            &fixture.universe,
            &fixture.patch,
            &fixture.meta,
            false,
            crate::graph_db::PATCH_SQL_BUDGET,
        );

        assert!(matches!(refused, Err(crate::graph_db::PatchError::Busy)));
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        other.execute_batch("ROLLBACK").unwrap();
        assert_eq!(stored_revision(&fixture.db), 1);
    }

    /// What a child process of a crash test does: build the sample graph in the workspace it is
    /// given, then take the step the test names, announce it, and wait to be killed.
    fn patch_child_workspace(step: &str) -> Option<PathBuf> {
        std::env::var_os(format!("BSL_PATCH_CHILD_{step}")).map(PathBuf::from)
    }

    fn announce_and_wait(announcement: &str) {
        println!("READY {announcement}");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }

    /// Run `test` again as a child in `root` for `step`, and return it once it has announced,
    /// with what it announced.
    fn spawn_patch_child(test: &str, step: &str, root: &Path) -> (PatchChild, String) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads", "1"])
            .env(format!("BSL_PATCH_CHILD_{step}"), root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let child = PatchChild(child);
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                std::io::BufRead::read_line(&mut reader, &mut line).unwrap(),
                0,
                "the child ended before announcing"
            );
            // The harness's own "test ... " prefix is on the same line: nothing ends it first.
            if let Some((_, announcement)) = line.split_once("READY ") {
                return (child, announcement.trim().to_owned());
            }
        }
    }

    fn kill(mut child: PatchChild) {
        child.0.kill().unwrap();
    }

    /// A child of a crash test, ended and reaped however the test that started it ends.
    struct PatchChild(std::process::Child);

    impl Drop for PatchChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A process that dies with the patch written and not committed leaves the file to be
    /// opened as it was: the next open finishes what the interrupted writer left, the old
    /// publication is in place, and the same patch then applies once.
    #[test]
    fn a_crash_before_the_commit_leaves_the_old_publication_and_the_patch_applies_once() {
        const TEST: &str = "graph::build::tests::a_crash_before_the_commit_leaves_the_old_publication_and_the_patch_applies_once";
        if let Some(root) = patch_child_workspace("BEFORE") {
            let fixture = patch_fixture(&root);
            let Ok(_open) = crate::graph_db::begin_body_patch(
                &fixture.db,
                &fixture.project,
                &fixture.universe,
                &fixture.patch,
                &fixture.meta,
                false,
                crate::graph_db::PATCH_SQL_BUDGET,
            ) else {
                panic!("the child could not write the patch");
            };
            announce_and_wait(&fixture.db.display().to_string());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (child, db) = spawn_patch_child(TEST, "BEFORE", root);
        let db = PathBuf::from(db);
        kill(child);

        super::super::snapshot::recover_hot_journal(&db).unwrap();
        assert_eq!(stored_revision(&db), 1, "the old publication is in place");
        assert!(crate::graph_query::GraphDb::open(&db).unwrap().quick_check().is_ok());

        let (project, universe, patch, meta, _) =
            edit_and_compute_patch(root, &db, "after-crash-1".into());
        let Ok(open) = crate::graph_db::begin_body_patch(
            &db,
            &project,
            &universe,
            &patch,
            &meta,
            false,
            crate::graph_db::PATCH_SQL_BUDGET,
        ) else {
            panic!("the patch cannot be written after the crash");
        };
        open.commit().unwrap();
        assert_eq!(stored_revision(&db), 2);
        assert_eq!(publication_base(&db).unwrap().as_deref(), Some("after-crash-1"));
    }

    /// A process that dies with the interrupted write already spilled to the file — the journal
    /// is what holds the old pages — is recovered by the next open, without the journal being
    /// deleted by hand, and the file is the one it was before.
    #[test]
    fn a_hot_journal_is_recovered_by_the_next_open_and_the_file_is_as_it_was() {
        const TEST: &str = "graph::build::tests::a_hot_journal_is_recovered_by_the_next_open_and_the_file_is_as_it_was";
        if let Some(root) = patch_child_workspace("SPILL") {
            let fixture = patch_fixture(&root);
            let length = fs::metadata(&fixture.db).unwrap().len();
            let conn = rusqlite::Connection::open(&fixture.db).unwrap();
            conn.query_row("PRAGMA journal_mode = PERSIST", [], |r| r.get::<_, String>(0)).unwrap();
            conn.execute_batch("PRAGMA cache_size = 10; BEGIN IMMEDIATE;").unwrap();
            conn.execute_batch(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 20000)
                 INSERT INTO meta (key, value) SELECT 'junk' || x, randomblob(1000) FROM c;",
            )
            .unwrap();
            announce_and_wait(&format!("{length} {}", fixture.db.display()));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (child, announced) = spawn_patch_child(TEST, "SPILL", dir.path());
        let (length, db) = announced.split_once(' ').unwrap();
        let (length, db) = (length.parse::<u64>().unwrap(), PathBuf::from(db));
        kill(child);

        let mut journal = db.as_os_str().to_owned();
        journal.push("-journal");
        assert!(
            fs::metadata(Path::new(&journal)).is_ok_and(|journal| journal.len() > 0),
            "the crash left a journal to recover"
        );
        assert!(
            fs::metadata(&db).unwrap().len() > length,
            "the interrupted write reached the file"
        );

        super::super::snapshot::recover_hot_journal(&db).unwrap();

        assert_eq!(fs::metadata(&db).unwrap().len(), length, "the file is as long as it was");
        assert_eq!(stored_revision(&db), 1);
        let conn = rusqlite::Connection::open(&db).unwrap();
        let junk: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta WHERE key LIKE 'junk%'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(junk, 0, "nothing of the interrupted write is left");
        assert!(crate::graph_query::GraphDb::open(&db).unwrap().quick_check().is_ok());
    }

    /// The boot recovers a journal an interrupted patch left before it inspects the cached
    /// graph: the cache is served at once instead of looking unreadable.
    #[test]
    fn the_boot_recovers_a_hot_journal_before_serving_the_cache() {
        const TEST: &str =
            "graph::build::tests::the_boot_recovers_a_hot_journal_before_serving_the_cache";
        if let Some(root) = patch_child_workspace("BOOT") {
            let fixture = patch_fixture(&root);
            let conn = rusqlite::Connection::open(&fixture.db).unwrap();
            conn.query_row("PRAGMA journal_mode = PERSIST", [], |r| r.get::<_, String>(0)).unwrap();
            conn.execute_batch("PRAGMA cache_size = 10; BEGIN IMMEDIATE;").unwrap();
            conn.execute_batch(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 20000)
                 INSERT INTO meta (key, value) SELECT 'junk' || x, randomblob(1000) FROM c;",
            )
            .unwrap();
            announce_and_wait(&fixture.db.display().to_string());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (child, db) = spawn_patch_child(TEST, "BOOT", dir.path());
        kill(child);
        let mut journal = PathBuf::from(&db).into_os_string();
        journal.push("-journal");
        assert!(
            fs::metadata(Path::new(&journal)).is_ok_and(|journal| journal.len() > 0),
            "the crash left a journal to recover"
        );

        let root = dir.path();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        let mut engine = bsl_search::SearchEngine::fts_only(&cache.search_db_path()).unwrap();
        graph.start_workspace_graph(&mut engine, root);

        assert!(
            matches!(graph.status(), GraphStatus::Ready { .. }),
            "the cached graph is served by the boot itself: {:?}",
            graph.status()
        );
        assert_eq!(graph.full_builds_started.load(Ordering::SeqCst), 0, "nothing was rebuilt");
        // The catch-up the stale cache owes runs on; it ends before the workspace goes away.
        wait_until(&graph, "the catch-up to finish", || !graph.build_in_flight());
        wait_ready(&graph);
    }

    /// A process that dies after the commit leaves the new publication whole: nothing is rolled
    /// back, nothing is applied twice, and the file opens without recovery.
    #[test]
    fn a_crash_after_the_commit_leaves_the_new_publication() {
        const TEST: &str =
            "graph::build::tests::a_crash_after_the_commit_leaves_the_new_publication";
        if let Some(root) = patch_child_workspace("AFTER") {
            let fixture = patch_fixture(&root);
            let Ok(open) = crate::graph_db::begin_body_patch(
                &fixture.db,
                &fixture.project,
                &fixture.universe,
                &fixture.patch,
                &fixture.meta,
                false,
                crate::graph_db::PATCH_SQL_BUDGET,
            ) else {
                panic!("the child could not write the patch");
            };
            open.commit().unwrap();
            announce_and_wait(&format!("{} {}", fixture.meta.publication_id, fixture.db.display()));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (child, announced) = spawn_patch_child(TEST, "AFTER", dir.path());
        let (publication, db) = announced.split_once(' ').unwrap();
        let (publication, db) = (publication.to_owned(), PathBuf::from(db));
        kill(child);

        super::super::snapshot::recover_hot_journal(&db).unwrap();

        assert_eq!(stored_revision(&db), 2);
        assert_eq!(publication_base(&db).unwrap(), Some(publication));
        let graph = crate::graph_query::GraphDb::open(&db).unwrap();
        assert!(graph.quick_check().is_ok());
        assert!(!graph.freshness_token().unwrap().2, "the final force_stale was committed with it");
    }

    /// The search context is refreshed once the patch is committed and installed, against the
    /// patch's own publication, and a body-only patch does not ask for the whole collection to be
    /// rendered again.
    #[test]
    fn a_point_patch_refreshes_the_search_context_against_its_own_publication() {
        use super::super::test_support::{wait_publish_pass_within, WAIT_CEILING};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let signals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook = {
            let signals = std::sync::Arc::clone(&signals);
            std::sync::Arc::new(move |signal: crate::graph::GraphPublishSignal| {
                signals.lock().unwrap().push((signal.revision, signal.topology_changed));
                crate::graph::GraphPublishOutcome::HANDLED
            })
                as std::sync::Arc<
                    dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome
                        + Send
                        + Sync,
                >
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        wait_publish_pass_within(&graph, WAIT_CEILING, 1);
        signals.lock().unwrap().clear();
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nЗначение = 1;\nВозврат Значение;\nКонецФункции",
        );

        let outcome = graph.try_incremental_reload(root, 2, 0);

        assert!(matches!(outcome, PublishAttemptOutcome::Published));
        assert_eq!(
            *signals.lock().unwrap(),
            vec![(2, false)],
            "one refresh, for the patch's revision, without a whole-collection request"
        );
        assert_eq!(stored_revision(&graph_db_path(root)), 2);
    }

    #[test]
    fn invalid_cached_graph_falls_back_without_rearming_ownership_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let path = graph_db_path(root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not sqlite").unwrap();

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.run_load(false);

        assert!(matches!(graph.status(), GraphStatus::Ready { .. }));
        assert!(!graph.owes_failed());
        assert!(graph.snapshot().is_some(), "the invalid cache fell back to a real build");
    }

    #[test]
    fn full_reload_changed_install_serves_no_stale_generation_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        write(root, "CommonModules/Сервер.xml", "<MetaDataObject><changed/></MetaDataObject>");
        super::super::snapshot::refuse_snapshot_install_for_test();

        graph.run_load(true);

        assert_eq!(
            graph.snapshot().map(|snapshot| snapshot.generation),
            None,
            "the replaced file is not served under the generation it no longer holds"
        );
        assert!(graph.owes_failed());
        assert!(matches!(
            lock_recover(&graph.inner).published.as_ref().unwrap().reload,
            ReloadState::Failed(_)
        ));
    }

    /// A reload of a ready graph that failed is still owed: the graph says it is behind, a
    /// nudge inside the failure's backoff starts nothing, and the watcher's alarm is armed for
    /// when the backoff runs out.
    #[test]
    fn a_failed_reload_is_owed_behind_its_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // Keep a hub for the graph's observation boundary, but record this test's fact
        // directly: this test measures failed-reload backoff, not filesystem delivery.
        let hub = crate::graph::test_support::workspace_hub(root);
        let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        graph.set_watch(super::super::watcher::WatchPhase::Running, None);
        graph.ensure_loading();
        wait_ready(&graph);
        assert_eq!(published_report(&graph).stale, Some(false));
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "Функция Считать() Экспорт Возврат 2; КонецФункции",
        );
        graph.record_change_quietly(graph.observation().saturating_add(1));
        let refused =
            || LoadFailure::new(LoadFailureReason::TransientRefusal, "the lease was busy");
        // The second refusal in a row is the one with a real delay.
        graph.record_load_failure(true, refused());
        graph.record_load_failure(true, refused());

        assert_eq!(
            published_report(&graph).stale,
            Some(true),
            "a graph whose reload failed read fresh"
        );
        graph.nudge_rebuild();
        assert!(
            graph.owes_change().is_some(),
            "the backoff was stepped around instead of keeping the change owed",
        );
        assert!(matches!(
            lock_recover(&graph.inner).published.as_ref().unwrap().reload,
            ReloadState::Failed(_)
        ));
        let now = std::time::Instant::now();
        let due = graph.wake_at(now).expect("nobody owns the failed reload");
        assert!(
            due > now + std::time::Duration::from_secs(10),
            "the retry is not held off: {:?}",
            due - now
        );
    }

    /// A retry the owner could not start — its lease could not be confirmed just then — keeps
    /// the failed reload owed: the graph still reads behind and the alarm stays armed.
    #[test]
    fn a_failed_reload_whose_retry_is_held_stays_owed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let lease = crate::workspace_lease::WorkspaceLease::claim(root);
        let graph = GraphState::for_workspace(root.to_path_buf()).with_lease(lease.clone());
        graph.ensure_loading();
        wait_ready(&graph);
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "Функция Считать() Экспорт Возврат 3; КонецФункции",
        );
        graph.record_load_failure(true, LoadFailure::refused("the lease was busy"));
        // The record gone and its lock held elsewhere: the lease cannot be confirmed for now,
        // which is not a takeover.
        std::fs::remove_file(crate::cache::WorkspaceCacheLayout::for_workspace(root).lease_path())
            .unwrap();
        let held = lease.hold_file_lock_for_test();
        graph.drive();
        assert!(!lease.is_superseded(), "the probe took the workspace over");
        let now = std::time::Instant::now();
        assert!(graph.wake_at(now).is_some(), "the held retry dropped the debt");
        drop(held);
    }

    /// A first build refused by the lease is retried by its owner, like a reload is: the
    /// failure itself arms the alarm, with no drift needed to have arrived meanwhile.
    #[test]
    fn a_refused_first_build_is_owed_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.record_load_failure(false, LoadFailure::refused("the lease was busy"));
        assert!(matches!(graph.status(), GraphStatus::Failed(_)));
        assert_eq!(
            graph.debt_standing(std::time::Instant::now()).failed,
            Some(crate::graph::debt::Ripeness::Now),
            "a refused first build has no owner for its retry",
        );
    }

    /// End-to-end through `GraphState`: a first use builds the SQLite graph off
    /// the workspace and serves overview/node/neighbors from the opened handle.
    #[test]
    fn loads_workspace_and_serves_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        let snap = graph.snapshot().expect("ready graph snapshots an opened handle");
        let gdb = &snap.graph;

        let overview = gdb.overview(10, None).expect("overview");
        assert_eq!(overview.edges, 1, "Клиент.Главная → Сервер.Считать is one resolved edge");
        assert_eq!(overview.client_to_server_edges, 1);

        let node = gdb
            .node("method/common/Сервер/Считать", ide::GraphDetail::Names, None)
            .expect("query")
            .expect("durable id resolves from the on-disk graph");
        assert_eq!(node.node.name, "Считать");
        assert_eq!(node.node.dispatch, vec!["server"]);
        assert_eq!(node.node.qualified, None, "code nodes do not serve qualified");

        // Callers traversal reaches the client method via the resolved edge.
        let callers = gdb
            .neighbors(
                &ide::NeighborsParams {
                    id: "method/common/Сервер/Считать",
                    dir: ide::Direction::In,
                    depth: 1,
                    max_nodes: 50,
                    detail: ide::GraphDetail::Names,
                    provenance_filter: Vec::new(),
                    edge_kind_filter: Vec::new(),
                    call_sites: false,
                    max_call_sites: 0,
                },
                None,
            )
            .expect("query")
            .expect("neighbors resolve");
        assert!(callers.nodes.iter().any(|n| n.id == "method/common/Клиент/Главная"));
        // The root endpoint is elided from served edges (absent = root), matching
        // the in-memory serve path.
        let edge = callers.edges.iter().find(|e| e.to.is_none()).expect("edge into the root");
        assert_eq!(edge.from.as_deref(), Some("method/common/Клиент/Главная"));
    }

    #[test]
    fn explicit_cache_layout_builds_graph_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = workspace.path();
        sample_workspace(root);
        let layout = crate::cache::WorkspaceCacheLayout::from_root(cache.path().to_path_buf());

        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), layout.clone());
        graph.ensure_loading();
        wait_ready(&graph);

        assert!(layout.graph_db_path().exists());
        assert!(!root.join(".build").exists());
    }

    /// A cached build that still matches the workspace is republished as-is — no
    /// rebuild — so its `revision` and `built_at` survive the load.
    #[test]
    fn reuses_a_matching_cached_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, workspace_fingerprint(root));

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        // Reused: the served revision is the cache's (7); a rebuild would reset it to 1.
        let snap = graph.snapshot().expect("ready graph snapshots");
        assert_eq!(snap.generation, 7, "served the cached revision, not a fresh build");
        // The file was not rewritten — its build timestamp is untouched.
        assert_eq!(meta_string(&graph_db_path(root), "built_at"), "cached-build-sentinel");
    }

    #[test]
    fn fresh_generation_reuses_completed_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, workspace_fingerprint(root));
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let path = cache.graph_db_path();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        let previous = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        previous.release();
        let fresh = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(fresh.owns_caches_now(), "the new process claims the released workspace");
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache)
            .with_lease(fresh.clone());
        graph.ensure_loading();
        wait_ready(&graph);

        let snapshot = graph.snapshot().expect("the completed compatible cache is adopted");
        assert_eq!(snapshot.generation, 7, "no builder reset the cached revision");
        assert_eq!(meta_string(&path, "built_at"), "cached-build-sentinel");
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
        fresh.release();
    }

    /// Test shorthand for the profile recompute: pairs the enumeration with the
    /// snapshot it came from, as the production incremental path does.
    fn recompute_profiles_for_test(
        root: &Path,
        changed: &[std::path::PathBuf],
    ) -> anyhow::Result<rustc_hash::FxHashMap<String, crate::graph_db::ModuleProfile>> {
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        crate::graph_db::recompute_module_profiles(&project, &universe.files, changed)
    }

    /// Test shorthand for the incremental patch: ONE loaded snapshot and ONE
    /// scanned universe feed the body-only update, as production does.
    fn update_bodies_for_test(
        root: &Path,
        src: &Path,
        out: &Path,
        changed: &[std::path::PathBuf],
        batch_size: usize,
        meta: &crate::graph_db::GraphMeta,
    ) -> anyhow::Result<ide::GraphBuildSummary> {
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        update_graph_database_bodies(
            &project,
            &universe,
            src,
            out,
            changed,
            BatchBudget::files(batch_size),
            meta,
        )
    }

    /// Test shorthand for the production pairing: ONE loaded snapshot and ONE
    /// scanned universe feed a whole-config build.
    fn build_whole_graph(
        root: &Path,
        out: &Path,
        batch_size: usize,
        meta: &crate::graph_db::GraphMeta,
    ) -> anyhow::Result<ide::GraphBuildSummary> {
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        build_graph_database(&project, &universe, out, BatchBudget::files(batch_size), meta)
    }

    /// The straddle verdict is more than a fingerprint comparison: either
    /// bracketing scan failing to cover the whole tree loses the coherence claim
    /// even when the fingerprints match exactly.
    #[test]
    fn coherence_is_lost_to_an_unclean_scan_even_with_equal_fingerprints() {
        let fp = crate::graph_db::GraphFp::default();
        let moved = crate::graph_db::GraphFp { files: 1, ..Default::default() };
        assert!(!publish_force_stale(fp, fp, true, true), "clean equal brackets publish clean");
        assert!(publish_force_stale(fp, moved, true, true), "a moved tree straddles");
        assert!(publish_force_stale(fp, fp, false, true), "a short pre-scan cannot claim the tree");
        assert!(
            publish_force_stale(fp, fp, true, false),
            "a short post-scan with equal fingerprints is exactly what the comparison cannot see"
        );
    }

    /// Fresh adoption needs a clean scan behind the compared value: equality
    /// against a fingerprint that describes only part of the tree proves nothing.
    #[test]
    fn fresh_adoption_requires_a_clean_matching_scan() {
        let fp = crate::graph_db::GraphFp::default();
        let moved = crate::graph_db::GraphFp { files: 1, ..Default::default() };
        assert!(cache_is_reusable(false, fp, fp, true));
        assert!(!cache_is_reusable(true, fp, fp, true), "a straddled build is never coherent");
        assert!(!cache_is_reusable(false, fp, moved, true), "the workspace moved");
        assert!(
            !cache_is_reusable(false, fp, fp, false),
            "an unclean scan's fingerprint matching the stored one proves nothing"
        );
    }

    /// A subtree the scan cannot enter — even an EMPTY one, invisible to the
    /// fingerprint — must mark the published build `force_stale`.
    #[cfg(unix)]
    #[test]
    fn a_publication_with_a_hidden_subtree_is_marked_force_stale() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let closed = root.join("closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&closed).is_ok() {
            // Permissions do not bind this user (UID 0): the input cannot exist.
            fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        assert_eq!(
            meta_string(&graph_db_path(root), "force_stale"),
            "1",
            "an unreadable empty subtree leaves the fingerprints equal — only the \
             scan verdict can catch it"
        );
        // While the subtree stays hidden, the marker must NOT drive a rebuild
        // loop: every rebuild would come out unclean again.
        {
            let snap = graph.snapshot().expect("ready graph snapshots");
            let fresh = graph.freshness(&snap);
            assert!(fresh.stale, "a force_stale build is served as stale");
            assert_eq!(fresh.reload, "none", "an unclean scan must not chase its own tail");
        }

        // Once the tree heals, the SAME fingerprints plus a clean scan retire the
        // incoherent build with exactly one fresh rebuild.
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
        *lock_recover(&graph.scan) = None;
        let claimed = {
            let snap = graph.snapshot().expect("ready graph snapshots");
            graph.freshness(&snap).reload == "running"
        };
        assert!(claimed, "recovery must schedule the clean rebuild the marker was waiting for");
        wait_until_within(
            &graph,
            Duration::from_secs(3),
            "the recovery rebuild to publish a snapshot no longer marked force_stale",
            || meta_string(&graph_db_path(root), "force_stale") == "0",
        );
    }

    /// The positive control for the verdict wiring: a healthy tree publishes clean.
    #[test]
    fn a_publication_over_a_healthy_tree_is_not_force_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        assert_eq!(meta_string(&graph_db_path(root), "force_stale"), "0");
    }

    /// A matching cache is NOT adopted as fresh when the scan behind the comparison
    /// could not cover the whole tree: an unreadable EMPTY subtree changes no stats
    /// row, so the fingerprints still match — only the verdict refuses.
    #[cfg(unix)]
    #[test]
    fn a_cache_is_not_adopted_fresh_over_an_unclean_scan() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, workspace_fingerprint(root));
        let closed = root.join("closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&closed).is_ok() {
            fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let graph = GraphState::for_workspace(root.to_path_buf());
        let adopted = graph.try_publish_cached(root, 0);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(adopted, PublishAttemptOutcome::FallBack));
        assert!(
            matches!(graph.try_publish_cached(root, 0), PublishAttemptOutcome::Published),
            "the same cache is adopted once the scan is clean"
        );
    }

    /// The build lowers the PRE-scanned universe: a file landing between the
    /// pre-scan and the build is absent from the persisted `files` rows, and the
    /// post-scan bracket reports the straddle instead.
    #[test]
    fn the_build_does_not_see_files_added_after_the_pre_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let project = crate::graph::ProjectSnapshot::load(root);
        let pre = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        let fp_pre = crate::graph::scan::fingerprint_of_project(&pre.stats, &project).unwrap();

        write_common_module(root, "Опоздавший", true, "Процедура П() Экспорт КонецПроцедуры");

        let out = root.join(".build/graph.db");
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_graph_database(
            &project,
            &pre,
            &out,
            GRAPH_BUILD_BATCH,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: fp_pre,
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .unwrap();

        let late_rows: i64 = Connection::open(&out)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%Опоздавший%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(late_rows, 0, "the files table describes the universe the build lowered");

        // The straddle bracket is what reports the late file instead.
        let post = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        let fp_post = crate::graph::scan::fingerprint_of_project(&post.stats, &project).unwrap();
        assert!(publish_force_stale(fp_pre, fp_post, pre.clean(), post.clean()));
    }

    /// A config edit may change only the declared spelling of a root while preserving
    /// canonical graph topology. Even then the publication must carry roots from its frozen
    /// pre-build project, not reload the newer project after the build.
    #[cfg(unix)]
    #[test]
    fn published_roots_come_from_the_same_project_snapshot_as_the_graph() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let configuration = root.join("cf");
        fs::create_dir_all(&configuration).unwrap();
        fs::write(configuration.join("Configuration.xml"), "<Configuration/>").unwrap();
        sample_workspace(&configuration);
        symlink(&configuration, root.join("alias-a")).unwrap();
        symlink(&configuration, root.join("alias-b")).unwrap();
        fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \"alias-a\"\n").unwrap();

        let excluded: Vec<_> =
            crate::cache::WorkspaceCacheLayout::for_workspace(root).exclusions(root);
        let project = crate::graph::ProjectSnapshot::load_excluding(root, &excluded);
        let pre = crate::graph::universe::ScannedUniverse::scan_excluding(
            &project.scan_roots,
            &project.excluded,
        );
        fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \"alias-b\"\n").unwrap();

        let graph = GraphState::for_workspace(root.to_path_buf());
        let built = build_and_publish_scanned(root, &project, &pre, 1, &graph, None).unwrap();
        let published = built.search_roots.as_ref().unwrap();
        let live =
            crate::graph::ProjectSnapshot::load_excluding(root, &excluded).search_roots.unwrap();

        assert!(published.configuration().unwrap().ends_with("alias-a"));
        assert!(live.configuration().unwrap().ends_with("alias-b"));
        assert_eq!(
            built.fp_pre,
            crate::graph::scan::workspace_fingerprint(root),
            "the alias-only edit is deliberately invisible to GraphFp"
        );
    }

    /// One full publication is exactly TWO traversals: the shared pre-scan and the
    /// straddle bracket's post-scan. A third walk means some pass walked on its
    /// own — the regression this whole seam exists to prevent. Counted through the
    /// scanner's own per-thread counter: both scans of a publication are initiated
    /// on the calling thread, and parallel tests cannot pollute the reading.
    #[test]
    fn a_full_publication_walks_the_tree_exactly_twice() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());

        let before = project_model::source_set::scans_performed_on_thread();
        let excluded: Vec<_> =
            crate::cache::WorkspaceCacheLayout::for_workspace(root).exclusions(root);
        let project = crate::graph::ProjectSnapshot::load_excluding(root, &excluded);
        let pre = crate::graph::universe::ScannedUniverse::scan_excluding(
            &project.scan_roots,
            &project.excluded,
        );
        build_and_publish_scanned(root, &project, &pre, 1, &graph, None)
            .expect("the publication succeeds");
        let walks = project_model::source_set::scans_performed_on_thread() - before;

        assert!(walks > 0, "a zero count means the instrumentation broke, not that no walk ran");
        assert_eq!(walks, 2, "pre-scan + straddle post-scan, nothing else");
    }

    #[test]
    fn strict_context_provider_check_rejects_file_drift_and_force_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        build_and_publish_graph_file(root, 1, &graph, None).expect("the graph builds");
        let graph_path = graph.graph_db_path().expect("workspace graph has a cache path");
        let excluded: Vec<_> =
            crate::cache::WorkspaceCacheLayout::for_workspace(root).exclusions(root);
        let project = crate::graph::ProjectSnapshot::load_excluding(root, &excluded);
        let graph_db = crate::graph_query::GraphDb::open(&graph_path).expect("graph opens");
        assert!(
            crate::graph::scan::graph_matches_live_project_strict(&graph_db, &project),
            "the provider accepts the complete, current graph"
        );
        drop(graph_db);

        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nЗначение = 2;\nВозврат Значение;\nКонецФункции",
        );
        let changed_project = crate::graph::ProjectSnapshot::load_excluding(root, &excluded);
        let graph_db = crate::graph_query::GraphDb::open(&graph_path).expect("graph opens");
        assert!(
            !crate::graph::scan::graph_matches_live_project_strict(&graph_db, &changed_project),
            "a BSL edit with unchanged topology is not accepted by the provider"
        );
        drop(graph_db);

        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт КонецФункции",
        );
        let restored_project = crate::graph::ProjectSnapshot::load_excluding(root, &excluded);
        let graph_db = crate::graph_query::GraphDb::open(&graph_path).expect("graph opens");
        assert!(
            crate::graph::scan::graph_matches_live_project_strict(&graph_db, &restored_project),
            "restoring the original bytes makes the provider current again"
        );
        drop(graph_db);

        Connection::open(&graph_path)
            .expect("graph opens for metadata update")
            .execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('force_stale', '1')", [])
            .expect("force-stale metadata is writable in the fixture");
        let graph_db = crate::graph_query::GraphDb::open(&graph_path).expect("graph opens");
        assert!(
            !crate::graph::scan::graph_matches_live_project_strict(&graph_db, &restored_project),
            "force-stale metadata blocks the provider independently of topology"
        );
    }

    /// One incremental reload is also exactly TWO traversals: the shared pre-scan
    /// (eligibility diff + profiles + fingerprint + patch) and the straddle
    /// bracket's post-scan. Historically this path walked six times.
    #[test]
    fn an_incremental_reload_walks_the_tree_exactly_twice() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        // Body-only: the signature line is untouched, so the fast path is eligible.
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nЗначение = 1;\nВозврат Значение;\nКонецФункции",
        );

        let before = project_model::source_set::scans_performed_on_thread();
        let took_fast_path = graph.try_incremental_reload(root, 2, 0);
        let walks = project_model::source_set::scans_performed_on_thread() - before;

        assert!(
            matches!(took_fast_path, PublishAttemptOutcome::Published),
            "a body-only edit takes the incremental path"
        );
        assert!(walks > 0, "a zero count means the instrumentation broke");
        assert_eq!(walks, 2, "shared pre-scan + straddle post-scan, nothing else");
    }

    /// A scan that cannot cover the whole tree disables the incremental path
    /// BEFORE the eligibility diff: a diff against a short scan reads hidden
    /// files as removals, and an unreadable EMPTY subtree does not move the
    /// stats at all — only the verdict can see it.
    #[cfg(unix)]
    #[test]
    fn an_unclean_scan_disables_the_incremental_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nЗначение = 2;\nВозврат Значение;\nКонецФункции",
        );
        let closed = root.join("closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&closed).is_ok() {
            fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let refused =
            matches!(graph.try_incremental_reload(root, 2, 0), PublishAttemptOutcome::FallBack);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(refused, "an incomplete scan must fall back to the full rebuild");
        assert!(
            matches!(graph.try_incremental_reload(root, 3, 0), PublishAttemptOutcome::Published),
            "positive control: the same edit goes incremental once the scan is clean"
        );
    }

    #[test]
    fn incremental_publish_propagates_transient_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache)
            .with_lease(lease.clone());
        graph.ensure_loading();
        wait_ready(&graph);
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nЗначение = 3;\nВозврат Значение;\nКонецФункции",
        );

        let held = lease.hold_file_lock_for_test();
        let outcome = graph.try_incremental_reload(root, 2, 0);
        drop(held);

        let failure = match outcome {
            PublishAttemptOutcome::Refused(failure) => failure,
            PublishAttemptOutcome::Published => panic!(
                "an eligible incremental publication ignored the refusal; decisions: {:?}",
                lock_recover(&graph.incremental_decisions)
            ),
            PublishAttemptOutcome::FallBack => panic!(
                "an eligible incremental publication fell back before the refusal; decisions: {:?}",
                lock_recover(&graph.incremental_decisions)
            ),
        };
        assert_eq!(failure.reason, LoadFailureReason::TransientRefusal);
        graph.record_load_failure(true, failure);
        assert!(graph.owes_failed());
    }

    /// A cached build whose fingerprint no longer matches the workspace (it moved
    /// since the build) is served immediately as a stale snapshot — answers now beat
    /// "still indexing" — while the pre-claimed catch-up reload replaces it.
    #[test]
    fn serves_stale_cache_and_catches_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        seed_cache(root, {
            let mut fp = workspace_fingerprint(root);
            fp.files = fp.files.wrapping_add(1);
            fp
        });

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        // Ready right away: either the stale cache (revision 7) is being served with
        // the catch-up still running, or — on a fast machine over this tiny fixture —
        // the catch-up already published revision 8. Never a from-scratch generation 1.
        let first = graph.snapshot().expect("ready graph snapshots").generation;
        assert!(
            first == 7 || first == 8,
            "the stale cache is served (or already caught up), never rebuilt at 1: {first}"
        );

        // The catch-up publishes past the cached revision and rewrites the file.
        wait_until_within(
            &graph,
            Duration::from_secs(5),
            "the catch-up reload to publish past the cached revision",
            || graph.snapshot().map(|s| s.generation) == Some(8),
        );
        assert_ne!(meta_string(&graph_db_path(root), "built_at"), "cached-build-sentinel");
    }

    /// The event-maintained map's fold must be bit-identical to the walk's fold, or
    /// freshness would report phantom drift after every hub-patched entry.
    #[test]
    fn fp_map_fold_matches_walk_fold() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let walk = workspace_fingerprint(root);
        let excluded: Vec<_> =
            crate::cache::WorkspaceCacheLayout::for_workspace(root).exclusions(root);
        let project = crate::graph::ProjectSnapshot::load_excluding(root, &excluded);
        let stats = crate::graph::scan::scan_stats_over_roots_excluding(
            &project.scan_roots,
            &project.excluded,
        )
        .0;
        let via_map = crate::graph::scan::portable_fingerprint_of(
            &stats,
            project.search_roots.as_ref(),
            project.portable_topology,
        )
        .unwrap();
        assert_eq!(via_map, walk, "map fold == walk fold");
    }

    /// A cached build flagged `force_stale` (it straddled a disk write and was never
    /// a coherent snapshot) is never reused even if its fingerprint matches.
    #[test]
    fn rebuilds_when_cached_build_is_force_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let fp = workspace_fingerprint(root);
        seed_cache(root, fp);
        Connection::open(graph_db_path(root))
            .unwrap()
            .execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('force_stale', '1')", [])
            .unwrap();

        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);

        let snap = graph.snapshot().expect("ready graph snapshots");
        assert_eq!(snap.generation, 1, "force_stale cache rebuilt at generation 1");
        assert_ne!(meta_string(&graph_db_path(root), "built_at"), "cached-build-sentinel");
    }

    /// The graph's half of the node: it cannot PREVENT the loss (an unreadable module
    /// yields no rows to any build), so it must not be silent about it. The artefact
    /// records which modules it could not read, and a patch never clears an inherited
    /// one — only a build that rewrites the module restores its rows.
    #[test]
    fn an_unreadable_module_is_recorded_in_the_artefact() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let blind = root.join("CommonModules/Слепой/Ext/Module.bsl");
        fs::create_dir_all(blind.parent().unwrap()).unwrap();
        fs::write(&blind, [0xFF, 0xFE]).unwrap();

        let out = root.join(".build/bsl-graph.db");
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        let meta = crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        crate::graph_db::build_graph_database(
            &project,
            &universe,
            &out,
            stdx::batch::BatchBudget::files(1),
            &meta,
        )
        .expect("graph database builds");

        // The walk verdict cannot see this: `stat` needs no read permission.
        assert!(universe.clean(), "the tree walk is clean, which is exactly the trap");

        {
            // No intermediate write: the BUILDER records the key, so an artefact is
            // never at the current schema version while silently claiming zero holes.
            let conn = Connection::open(&out).unwrap();
            let stored = crate::graph_db::read_unread_paths(&conn);
            assert!(
                stored.iter().any(|p| p.path.ends_with("Слепой/Ext/Module.bsl")),
                "the builder records the module it could not read: {stored:?}"
            );
            // ONE path, not one per pass: `open_batch` is called by every pass, and a
            // counter would multiply the same file.
            assert_eq!(stored.len(), 1);
        }

        // Positive control: the same tree, readable, records nothing.
        fs::write(&blind, "&НаСервере\nПроцедура Пусто() Экспорт КонецПроцедуры").unwrap();
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        let out2 = root.join(".build/bsl-graph2.db");
        crate::graph_db::build_graph_database(
            &project,
            &universe,
            &out2,
            stdx::batch::BatchBudget::files(1),
            &meta,
        )
        .expect("graph database builds");
        let conn = Connection::open(&out2).unwrap();
        assert!(
            crate::graph_db::read_unread_paths(&conn).is_empty(),
            "control: nothing unread over a readable tree"
        );
    }

    /// A patch may only speak about the modules it rewrote. Its index pass opens the
    /// WHOLE universe, so it learns about holes it neither deleted nor replaced rows
    /// for — and recording one would claim rows are absent when they are merely stale,
    /// with no way back: the inherited set is released only for paths a later patch
    /// rewrites, and a module nobody edits is never rewritten again.
    #[test]
    fn a_patch_records_only_the_holes_whose_rows_it_rewrote() {
        let dir = tempfile::tempdir().unwrap();
        // The artefact keys rows by the walk's canonical spelling, and production
        // hands the patch paths from that same walk. A root reached through a link
        // (the temp dir is one on macOS) would have this stand comparing spellings
        // production never compares.
        let root = &dir.path().canonicalize().unwrap();
        sample_workspace(root);
        write_common_module(
            root,
            "Сосед",
            true,
            "&НаСервере\nПроцедура Соседняя() Экспорт КонецПроцедуры",
        );
        let bystander = root.join("CommonModules/Сосед/Ext/Module.bsl");

        let meta = crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let src = root.join(".build/bsl-graph.db");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        build_whole_graph(root, &src, 1, &meta).expect("the whole graph builds");
        let roots = crate::graph::ProjectSnapshot::load(root).search_roots.unwrap();
        let rows_before = node_rows_for(&src, &bystander, &roots);
        assert!(rows_before > 0, "the neighbour is in the artefact to begin with");

        // The neighbour goes dark, and someone else is edited. The patch never touches
        // the neighbour's rows — they stay exactly as the build left them.
        fs::write(&bystander, [0xFF, 0xFE]).unwrap();
        let edited = root.join("CommonModules/Сервер/Ext/Module.bsl");
        fs::write(&edited, "&НаСервере\nПроцедура Правка() Экспорт КонецПроцедуры").unwrap();
        let out = root.join(".build/bsl-graph-patched.db");
        update_bodies_for_test(root, &src, &out, &[edited], 1, &meta).expect("the patch applies");

        let conn = Connection::open(&out).unwrap();
        assert_eq!(
            node_rows_for(&out, &bystander, &roots),
            rows_before,
            "the patch left the neighbour's rows in place"
        );
        assert!(
            crate::graph_db::read_unread_paths(&conn).is_empty(),
            "so it must not report the neighbour as a module the artefact is missing: {:?}",
            crate::graph_db::read_unread_paths(&conn)
        );
        drop(conn);

        // Positive control, without which the filter above could be silently swallowing
        // every hole: the SAME unreadable module, this time inside the patch. Its rows
        // are deleted and nothing replaces them, so now the artefact owes the record.
        let dark = root.join(".build/bsl-graph-dark.db");
        update_bodies_for_test(root, &out, &dark, std::slice::from_ref(&bystander), 1, &meta)
            .expect("the patch applies over an unreadable module");
        let conn = Connection::open(&dark).unwrap();
        assert_eq!(node_rows_for(&dark, &bystander, &roots), 0, "its rows went with the patch");
        assert!(
            crate::graph_db::read_unread_paths(&conn)
                .iter()
                .any(|p| p.path.ends_with("Сосед/Ext/Module.bsl")),
            "and a module the patch could not lower IS recorded"
        );
        drop(conn);

        // And the record is released by the pass that restores the rows, not before.
        fs::write(&bystander, "&НаСервере\nПроцедура Снова() Экспорт КонецПроцедуры").unwrap();
        let healed = root.join(".build/bsl-graph-healed.db");
        update_bodies_for_test(root, &dark, &healed, std::slice::from_ref(&bystander), 1, &meta)
            .expect("the patch applies over the restored module");
        let conn = Connection::open(&healed).unwrap();
        assert!(node_rows_for(&healed, &bystander, &roots) > 0, "the rows are back");
        assert!(
            crate::graph_db::read_unread_paths(&conn).is_empty(),
            "so the record goes with them"
        );
    }

    /// Node rows the artefact holds for one module, keyed exactly as the builder stores it.
    fn node_rows_for(db: &Path, module: &Path, roots: &bsl_search::WorkspaceRoots) -> i64 {
        let key = roots.key_of_path(module).expect("the module belongs to the workspace");
        let conn = Connection::open(db).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE file_root_id = ?1 AND file_path = ?2",
            rusqlite::params![key.root_id, key.path],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// The streaming SQLite build must reproduce the in-memory graph: identical
    /// node-kind tallies, edge counts, durable ids, dispatch and in-degree.
    #[test]
    fn sqlite_build_matches_in_memory_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (db, files) = load_workspace_db(root).expect("workspace loads");
        let analysis = Analysis::from_database(db.clone());
        let overview =
            analysis.graph_overview(GRAPH_SOURCE_ROOT, Some(&ide::StripRoot::resolve(root)), 10);

        let out = root.join(".build/bsl-graph.db");
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        let summary = build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        assert_eq!(summary.edges, overview.edges);

        let conn = Connection::open(&out).unwrap();
        let count = |sql: &str| -> usize {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as usize
        };

        assert_eq!(count("SELECT COUNT(*) FROM nodes"), overview.nodes);
        assert_eq!(count("SELECT COUNT(*) FROM nodes WHERE kind='method'"), overview.methods);
        // `overview.modules` is the true distinct-module population (every module that owns a
        // method, plus any persisted module-body node), so it is >= the module rows actually
        // stored — module nodes are synthesized on demand, not generally persisted.
        let stored_module_rows = count("SELECT COUNT(*) FROM nodes WHERE kind='module'");
        assert!(
            overview.modules >= stored_module_rows,
            "reported modules {} >= stored module rows {stored_module_rows}",
            overview.modules,
        );
        assert!(overview.modules > 0, "the sample workspace has code modules");
        assert_eq!(count("SELECT COUNT(*) FROM nodes WHERE kind='mdo'"), overview.mdos);
        assert_eq!(count("SELECT COUNT(*) FROM nodes WHERE kind='attribute'"), overview.attributes);
        assert_eq!(count("SELECT COUNT(*) FROM edges"), overview.edges);
        assert_eq!(
            count("SELECT COUNT(*) FROM edges WHERE crosses=1"),
            overview.client_to_server_edges
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM edges WHERE provenance='resolved'"),
            *overview.edge_provenance.get("resolved").unwrap_or(&0)
        );

        let (name, dispatch): (String, String) = conn
            .query_row(
                "SELECT name, dispatch FROM nodes WHERE id = ?1",
                rusqlite::params!["method/common/Сервер/Считать"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((name.as_str(), dispatch.as_str()), ("Считать", "server"));

        let in_degree: i64 = conn
            .query_row(
                "SELECT degree FROM in_degree WHERE id = 'method/common/Сервер/Считать'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_degree, 1, "Сервер.Считать is called once");
    }

    /// `edge_kinds` narrows a neighbours query to the requested edge kinds: a method with
    /// both a `call` and a `query_ref` out-edge returns both unfiltered, only the query_ref
    /// edge under `edge_kinds=["query_ref"]`.
    #[test]
    fn neighbors_edge_kinds_filter_isolates_one_kind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_common_module(
            root,
            "Бета",
            true,
            "&НаСервере\nПроцедура ШагБ() Экспорт КонецПроцедуры",
        );
        write_common_module(
            root,
            "Альфа",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\nБета.ШагБ();\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );

        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");

        let mk = |kinds: Vec<String>| ide::NeighborsParams {
            id: "method/common/Альфа/ШагА",
            dir: ide::Direction::Out,
            depth: 1,
            max_nodes: 50,
            detail: ide::GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: kinds,
            call_sites: false,
            max_call_sites: 0,
        };

        // Unfiltered: both the call to Бета.ШагБ and the query_ref to Номенклатура.
        let all = gdb.neighbors(&mk(Vec::new()), None).unwrap().unwrap();
        let all_kinds: Vec<&str> = all.edges.iter().map(|e| e.kind).collect();
        assert!(all_kinds.contains(&"call"), "kinds: {all_kinds:?}");
        assert!(all_kinds.contains(&"query_ref"), "kinds: {all_kinds:?}");
        // Grouped distribution mirrors the edges; nothing was capped here.
        assert_eq!(all.by_kind.get("call"), Some(&1), "by_kind: {:?}", all.by_kind);
        assert_eq!(all.by_kind.get("query_ref"), Some(&1), "by_kind: {:?}", all.by_kind);
        assert_eq!(all.by_provenance.values().sum::<usize>(), all.edges.len());
        assert!(!all.connectors_dropped, "no nodes capped, so no connectors dropped");

        // Out-direction traversal reports its callees and no callers.
        assert_eq!(all.out_total, Some(2), "two callees (Бета.ШагБ + Номенклатура query)");
        assert_eq!(all.in_total, None, "dir=out reports no caller count");

        // dir=both surfaces directional fan-out: 2 callees, 0 callers of ШагА.
        let both = gdb
            .neighbors(
                &ide::NeighborsParams {
                    id: "method/common/Альфа/ШагА",
                    dir: ide::Direction::Both,
                    depth: 1,
                    max_nodes: 50,
                    detail: ide::GraphDetail::Names,
                    provenance_filter: Vec::new(),
                    edge_kind_filter: Vec::new(),
                    call_sites: false,
                    max_call_sites: 0,
                },
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(both.out_total, Some(2), "both: callees counted");
        assert_eq!(both.in_total, Some(0), "both: no callers of ШагА");

        // edge_kinds=["query_ref"] keeps only the query_ref edge.
        let qr = gdb.neighbors(&mk(vec!["query_ref".to_owned()]), None).unwrap().unwrap();
        assert!(!qr.edges.is_empty(), "query_ref edge present");
        assert!(qr.edges.iter().all(|e| e.kind == "query_ref"), "edges: {:?}", qr.edges);
    }

    /// `node(detail=bodies)` caps its source output at `max_output_tokens`: a tiny budget
    /// truncates the body and flags `budget_exhausted`, a generous budget leaves it whole.
    /// Every node a tool serves says WHERE it is, or why it cannot — silence would read as
    /// "this thing has no place", which is false for a method and true for a metadata object.
    /// The three actions are checked separately because each has its own path to `node_ref`,
    /// and covering only `node` is how `overview` would ship without a location at all.
    #[test]
    fn served_nodes_carry_a_location_or_a_machine_reason() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (_db, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");
        let project = crate::project::at(root).expect("the fixture is a project");
        let (roots, _rejected) = crate::project::workspace_roots(&project, &[]);

        let id = "method/common/Сервер/Считать";
        let node = gdb
            .node(id, ide::GraphDetail::Names, Some(&roots))
            .unwrap()
            .expect("the method resolves")
            .node;
        let location = node.location.as_ref().expect("a method has a place");
        assert_eq!(location["root_id"], "");
        assert!(
            location["path"].as_str().unwrap().ends_with("CommonModules/Сервер/Ext/Module.bsl"),
            "{location}",
        );
        // The name range must be the NAME: the row stores where the header ends, and
        // publishing that would put the parameter list inside the field.
        assert_eq!(location["range"]["start_line"], location["range"]["end_line"]);
        assert!(location["enclosing_range"]["end_line"].as_u64().unwrap() >= 1);

        // Without the root table there is no pair — the node says so instead of going quiet.
        let rootless =
            gdb.node(id, ide::GraphDetail::Names, None).unwrap().expect("the method resolves").node;
        assert!(rootless.location.is_none());
        assert_eq!(rootless.location_unavailable, Some("roots_unavailable"));

        // `overview` reaches `node_ref` by its own path; a fix applied only to `node` and
        // `neighbors` leaves its methods with neither key, and this is what catches it.
        let overview = gdb.overview(10, Some(&roots)).expect("overview");
        let served: Vec<_> = overview
            .top_by_centrality
            .iter()
            .filter(|n| matches!(n.kind, "method" | "module"))
            .collect();
        assert!(!served.is_empty(), "the fixture has methods in the centrality list");
        for node in served {
            assert!(
                node.location.is_some() ^ node.location_unavailable.is_some(),
                "exactly one of the two keys, got {node:?}",
            );
            assert!(node.location.is_some(), "with a root table it must be the location");
        }
    }

    /// Offsets live in the artefact, text lives on disk, and between a build and its
    /// catch-up reload they disagree. An offset that stayed inside the file still points at
    /// the wrong bytes, so a range built from it is plausible and wrong — the worst kind for
    /// a consumer that cuts text with it. The name is verifiable by slicing, and it gates
    /// BOTH ranges; the pair itself stays, because the file is still that file.
    #[test]
    fn a_drifted_file_loses_its_ranges_but_keeps_its_pair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (_db, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");
        let project = crate::project::at(root).expect("the fixture is a project");
        let (roots, _rejected) = crate::project::workspace_roots(&project, &[]);
        let id = "method/common/Сервер/Считать";

        // Control: before the drift the node has both ranges.
        let before =
            gdb.node(id, ide::GraphDetail::Names, Some(&roots)).unwrap().expect("resolves").node;
        let before = before.location.expect("a method has a place");
        assert!(before.get("range").is_some(), "{before}");
        assert!(before.get("enclosing_range").is_some(), "{before}");

        // Insert a line ABOVE the method: every stored offset now points that much earlier.
        let module = root.join("CommonModules/Сервер/Ext/Module.bsl");
        let text = fs::read_to_string(&module).unwrap();
        fs::write(&module, format!("// шапка\n{text}")).unwrap();

        let after = GraphDb::open(&out)
            .expect("graph database opens")
            .node(id, ide::GraphDetail::Names, Some(&roots))
            .unwrap()
            .expect("resolves")
            .node;
        let after = after.location.expect("the pair survives: it is still that file");
        assert_eq!(after["path"], before["path"]);
        assert!(
            after.get("range").is_none() && after.get("enclosing_range").is_none(),
            "an unverifiable place is published as the file alone: {after}",
        );
    }

    /// An edit INSIDE the body leaves the declared name exactly where it was, so the name
    /// check passes and cannot notice anything — yet the stored end offset now lands in the
    /// middle of the new text. The end of a declaration is a keyword, so that is what the
    /// stored end is required to land on; without it the answer carries a range that cuts
    /// the wrong bytes.
    #[test]
    fn a_body_edit_drops_the_enclosing_range_while_the_name_survives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (_db, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let project = crate::project::at(root).expect("the fixture is a project");
        let (roots, _rejected) = crate::project::workspace_roots(&project, &[]);
        let id = "method/common/Сервер/Считать";

        // Grow the BODY: the name keeps its offset, the closing keyword does not.
        let module = root.join("CommonModules/Сервер/Ext/Module.bsl");
        fs::write(
            &module,
            "&НаСервере\nФункция Считать() Экспорт\n\tА = 1;\n\tВозврат А;\nКонецФункции\n",
        )
        .unwrap();

        let node = GraphDb::open(&out)
            .expect("graph database opens")
            .node(id, ide::GraphDetail::Names, Some(&roots))
            .unwrap()
            .expect("resolves")
            .node;
        let location = node.location.expect("the pair survives");

        assert!(
            location.get("range").is_some(),
            "the name is where it was, so its range is still true: {location}",
        );
        assert!(
            location.get("enclosing_range").is_none(),
            "the stored end no longer lands on the closing keyword: {location}",
        );
    }

    /// A projection reads a node's file only where the bytes are actually used, and the
    /// answers that cannot use them must not pay for a read.
    ///
    /// The two shapes that cannot: `usages` walks its callers with NO root table (so no place
    /// can be built) at `names` (so no signature and no body are asked for), and a `module`
    /// row carries no offsets (so it gets the pair alone) while being projected at `bodies`,
    /// which is what `overview` does for every module it lists.
    ///
    /// A wasted read is invisible in the answer — same JSON either way — so this measures the
    /// read itself: every module file is replaced by a FIFO, whose open blocks until someone
    /// writes. A projection that reads returns nothing within the timeout; one that does not
    /// answers at once. That also makes the check sensitive in the only direction that
    /// matters: it fails when a read comes back, and passes only when none does.
    #[cfg(unix)]
    #[test]
    fn a_projection_that_cannot_use_a_file_does_not_open_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (_db, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let project = crate::project::at(root).expect("the fixture is a project");
        let (roots, _rejected) = crate::project::workspace_roots(&project, &[]);

        // Everything the graph knows about is read from the database from here on; the
        // sources exist only as a trap.
        for module in ["Клиент", "Сервер"] {
            let path = root.join(format!("CommonModules/{module}/Ext/Module.bsl"));
            fs::remove_file(&path).unwrap();
            let made = std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .expect("mkfifo runs on this platform");
            assert!(made.success(), "a FIFO stands in for {module}'s module");
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let out_in_thread = out.clone();
        std::thread::spawn(move || {
            let gdb = GraphDb::open(&out_in_thread).expect("graph database opens");
            // The callers of a method, summarized for `symbol_info`: no root table, `names`.
            let usages = gdb
                .usages("method/common/Сервер/Считать", 5)
                .expect("usages reads the database")
                .expect("the method is in the graph");
            // A module projected at `bodies` — the shape `overview` takes for every module.
            let module = gdb
                .node("module/common/Сервер", ide::GraphDetail::Bodies, Some(&roots))
                .expect("node reads the database")
                .expect("the module resolves")
                .node;
            let _ = tx.send((usages.count, module.location.is_some(), module.source.is_none()));
        });

        let (callers, module_placed, module_without_source) = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("no projection here can use the bytes, so none may block on reading them");
        assert_eq!(callers, 1, "the fixture has exactly one caller of Считать");
        assert!(module_placed, "the pair costs no I/O and is served without it");
        assert!(module_without_source, "a module row has no offsets to cut a body with");
    }

    #[test]
    fn node_bodies_respect_output_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (_db, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");
        let project = crate::project::at(root).expect("the fixture is a project");
        let (roots, _rejected) = crate::project::workspace_roots(&project, &[]);

        let id = "method/common/Сервер/Считать";
        // Tiny budget (1 token ≈ 4 chars) truncates the body and flags exhaustion.
        let (tight, tight_completeness) =
            crate::tools::graph::node(&gdb, id, ide::GraphDetail::Bodies, 1, Some(&roots));
        assert_eq!(tight["budget_exhausted"], serde_json::json!(true));
        assert!(tight["node"]["source"].as_str().unwrap().len() <= 4, "{tight:?}");
        // The same fact reaches the envelope as a machine reason, not only as the flag.
        assert_eq!(tight_completeness.to_value()["reasons"][0]["code"], "output_budget");
        // A generous budget keeps the whole body and sets no exhaustion flag.
        let (loose, loose_completeness) =
            crate::tools::graph::node(&gdb, id, ide::GraphDetail::Bodies, 10_000, Some(&roots));
        assert!(loose.get("budget_exhausted").is_none(), "{loose:?}");
        assert!(loose["node"]["source"].as_str().unwrap().contains("Считать"), "{loose:?}");
        assert_eq!(loose_completeness.to_value()["status"], "complete");
    }

    /// A common module with no module-level edge has no stored `module` row, yet
    /// `node(module/common/X)` resolves on demand and lists the module's members; a module
    /// with no methods reports `not_found`.
    #[test]
    fn module_node_resolves_on_demand_and_lists_members() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (_db, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");

        // The module is NOT a stored node (no module-level edge in the fixture)...
        let stored_module_rows: i64 = Connection::open(&out)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE id = 'module/common/Сервер'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_module_rows, 0, "module has no stored row");

        // ...yet node(module/common/Сервер) resolves on demand and lists its members.
        let resolved = gdb
            .node("module/common/Сервер", ide::GraphDetail::Names, None)
            .unwrap()
            .expect("resolves");
        assert_eq!(resolved.node.kind, "module");
        let methods = resolved.node.methods.expect("module node carries its methods");
        assert!(
            methods.iter().any(|m| m.id == "method/common/Сервер/Считать" && m.name == "Считать"),
            "members listed: {methods:?}"
        );

        // A module with no methods cannot be synthesized → not_found.
        let missing = gdb.node("module/common/НетТакого", ide::GraphDetail::Names, None).unwrap();
        assert!(missing.is_err(), "module with no members is not_found");
    }

    /// A metadata object reached by a manager call in one module and by an SDBL
    /// query in another, across separate batches (`batch_size = 1`), must get the
    /// SAME durable `Mdo` node id from the streaming build as the in-memory fold.
    /// The build runs call edges across all batches before query edges, mirroring
    /// the fold's Pass-2-then-Pass-3 order, so the first-seen (canonical) spelling —
    /// and thus the id — cannot diverge even when the call and query sites differ in
    /// case.
    #[test]
    fn cross_batch_mdo_node_id_matches_fold() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write(
            root,
            "Catalogs/Номенклатура.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Номенклатура</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
        );
        // One module creates via the manager (canonical case), another reads it in a
        // query (upper case). Their batch order is fixed by walk order; the build's
        // global call-before-query order decides the canonical spelling regardless.
        write(
            root,
            "CommonModules/Менеджер/Ext/Module.bsl",
            "Процедура Создать() Экспорт\nСправочники.Номенклатура.СоздатьЭлемент();\nКонецПроцедуры",
        );
        write(
            root,
            "CommonModules/Отчет/Ext/Module.bsl",
            "Процедура Читать() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.НОМЕНКЛАТУРА\";\nКонецПроцедуры",
        );

        let (db, files) = load_workspace_db(root).expect("workspace loads");
        let analysis = Analysis::from_database(db);
        let fold =
            analysis.graph_overview(GRAPH_SOURCE_ROOT, Some(&ide::StripRoot::resolve(root)), 50);
        let fold_mdo: Vec<&str> = fold
            .top_by_centrality
            .iter()
            .filter(|n| n.kind == "mdo")
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(fold_mdo.len(), 1, "exactly one catalog Mdo node in the fold: {fold_mdo:?}");
        let fold_id = fold_mdo[0];

        let out = root.join(".build/bsl-graph.db");
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");

        let conn = Connection::open(&out).unwrap();
        let sqlite_mdo: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id FROM nodes WHERE kind='mdo'").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(sqlite_mdo.len(), 1, "exactly one catalog Mdo node in SQLite: {sqlite_mdo:?}");
        assert_eq!(
            sqlite_mdo[0], fold_id,
            "cross-batch Mdo node id must be byte-identical to the in-memory fold's"
        );
    }

    /// Serving overview/node/neighbors/source from the SQLite store must produce
    /// JSON byte-identical to the in-memory `ide::Analysis::graph_*` path it
    /// replaces — same fields, signatures, bodies, edges and budget behaviour.
    #[test]
    fn sqlite_serving_matches_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (db, files) = load_workspace_db(root).expect("workspace loads");
        let analysis = Analysis::from_database(db);

        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens and validates");
        let project = crate::project::at(root).expect("the fixture is a project");
        let (roots, _rejected) = crate::project::workspace_roots(&project, &[]);

        let id = "method/common/Сервер/Считать";

        let mem_overview = serde_json::to_value(analysis.graph_overview(
            GRAPH_SOURCE_ROOT,
            Some(&ide::StripRoot::resolve(root)),
            10,
        ))
        .unwrap();
        let mut sql_overview =
            serde_json::to_value(gdb.overview(10, Some(&roots)).unwrap()).unwrap();
        for node in sql_overview["top_by_centrality"].as_array_mut().unwrap() {
            if !node["location"].is_null() {
                node.as_object_mut().unwrap().remove("location");
                node["location_unavailable"] = serde_json::json!("roots_unavailable");
            }
        }
        assert_eq!(mem_overview, sql_overview, "overview JSON");

        let mem_node = serde_json::to_value(
            analysis
                .graph_node(
                    GRAPH_SOURCE_ROOT,
                    Some(&ide::StripRoot::resolve(root)),
                    id,
                    ide::GraphDetail::Bodies,
                )
                .unwrap(),
        )
        .unwrap();
        // Source text is read through the current root table. The in-memory graph
        // projection predates persisted locations and therefore carries the explicit
        // roots-unavailable marker; normalize only those location fields for this parity
        // assertion, while keeping the body/signature comparison exact.
        let mut sql_node = serde_json::to_value(
            gdb.node(id, ide::GraphDetail::Bodies, Some(&roots)).unwrap().unwrap(),
        )
        .unwrap();
        sql_node["node"].as_object_mut().unwrap().remove("location");
        sql_node["node"]["location_unavailable"] = serde_json::json!("roots_unavailable");
        assert_eq!(mem_node, sql_node, "node JSON (bodies detail)");

        let params = ide::NeighborsParams {
            id,
            dir: ide::Direction::In,
            depth: 1,
            max_nodes: 50,
            detail: ide::GraphDetail::Signatures,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            call_sites: false,
            max_call_sites: 0,
        };
        let mem_nb = serde_json::to_value(
            analysis
                .graph_neighbors(GRAPH_SOURCE_ROOT, Some(&ide::StripRoot::resolve(root)), &params)
                .unwrap(),
        )
        .unwrap();
        let mut sql_nb =
            serde_json::to_value(gdb.neighbors(&params, Some(&roots)).unwrap().unwrap()).unwrap();
        // Signature detail reads the source through the answering root table. The in-memory
        // projection carries the explicit roots-unavailable marker for locations, so normalize
        // only those fields on the SQLite nodes and keep the signature/edge JSON strict.
        let normalize_location = |node: &mut serde_json::Value| {
            if !node["location"].is_null() {
                node.as_object_mut().unwrap().remove("location");
                node["location_unavailable"] = serde_json::json!("roots_unavailable");
            }
        };
        normalize_location(&mut sql_nb["root"]);
        for node in sql_nb["nodes"].as_array_mut().unwrap() {
            normalize_location(node);
        }
        assert_eq!(mem_nb, sql_nb, "neighbors JSON");

        // Asking for places is a separate rootless projection. Names detail keeps this check
        // focused on the call-site contract, so neither side needs source bytes for signatures.
        let with_places = ide::NeighborsParams {
            detail: ide::GraphDetail::Names,
            call_sites: true,
            max_call_sites: 20,
            ..params
        };
        let mem_sites = serde_json::to_value(
            analysis
                .graph_neighbors(
                    GRAPH_SOURCE_ROOT,
                    Some(&ide::StripRoot::resolve(root)),
                    &with_places,
                )
                .unwrap(),
        )
        .unwrap();
        let sql_sites =
            serde_json::to_value(gdb.neighbors(&with_places, None).unwrap().unwrap()).unwrap();
        assert_eq!(mem_sites, sql_sites, "neighbors JSON with call sites");
        assert_eq!(
            mem_sites["edges"][0]["call_sites_unavailable"], "roots_unavailable",
            "without a root table a recorded span has no address to publish: {mem_sites}"
        );

        let ids = [id.to_string()];
        let mem_src = serde_json::to_value(analysis.graph_source(
            GRAPH_SOURCE_ROOT,
            Some(&ide::StripRoot::resolve(root)),
            &ids,
            4000,
        ))
        .unwrap();
        let sql_src = serde_json::to_value(gdb.source(&ids, 4000, Some(&roots)).unwrap()).unwrap();
        assert_eq!(mem_src, sql_src, "source JSON");

        // A malformed/unknown id reports NotFound, not an infra error.
        let missing = gdb.node("method/common/Нет/Метод", ide::GraphDetail::Names, None).unwrap();
        assert!(missing.is_err(), "unknown id resolves to a GraphError");
    }

    /// `GraphDb::graph_context` renders a method's outbound facts (dispatch, signature,
    /// calls, metadata reads) from the stored graph — the production source for
    /// embedding enrichment. Reuses `ide::GraphContext::render`, so it is byte-identical
    /// to the in-memory renderer for the same facts.
    #[test]
    fn graph_context_renders_method_outbound_facts_from_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A client method that calls a server method and reads a catalog via a manager.
        write_common_module(
            root,
            "Вызыватель",
            false,
            "Процедура Делать() Экспорт\n\
             Сервер.Считать();\n\
             Справочники.Контрагенты.НайтиПоКоду();\n\
             КонецПроцедуры",
        );
        write_common_module(root, "Сервер", true, "Функция Считать() Экспорт КонецФункции");

        let (_db, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");
        let project = crate::project::at(root).expect("the fixture is a project");
        let (roots, _rejected) = crate::project::workspace_roots(&project, &[]);

        // The calling method carries its signature, its call, and its metadata read.
        let ctx = gdb
            .graph_context("method/common/Вызыватель/Делать", Some(&roots))
            .unwrap()
            .expect("method has graph context");
        assert!(ctx.starts_with("Dispatch: "), "{ctx}");
        assert!(ctx.contains("\nSignature: Процедура Делать() Экспорт\n"), "{ctx}");
        assert!(ctx.contains("\nCalls: Считать\n"), "{ctx}");
        assert!(ctx.contains("\nReads: Справочник.Контрагенты\n"), "{ctx}");

        // A leaf method keeps its signature/dispatch but lists no calls or reads.
        let leaf = gdb
            .graph_context("method/common/Сервер/Считать", Some(&roots))
            .unwrap()
            .expect("leaf context");
        assert!(leaf.contains("Signature: Функция Считать() Экспорт"), "{leaf}");
        assert!(!leaf.contains("Calls:"), "{leaf}");
        assert!(!leaf.contains("Reads:"), "{leaf}");

        // Non-method ids have no graph context.
        assert_eq!(gdb.graph_context("mdo/Catalog/Контрагенты", Some(&roots)).unwrap(), None);

        // The graph-DB-backed provider resolves a chunk (path, symbol) to the same text.
        let generation = gdb.freshness_token().unwrap().0;
        drop(gdb);
        let store = crate::graph::GraphStore::serving_file_for_test(&out, None).unwrap();
        let provider =
            crate::graph_query::GraphDbContextProvider::new(store, generation, Some(&roots), None);
        let via_provider = bsl_search::GraphContextProvider::graph_context(
            &provider,
            "CommonModules/Вызыватель/Ext/Module.bsl",
            "Делать",
            "procedure",
        )
        .expect("provider resolves the method");
        assert!(via_provider.contains("\nCalls: Считать\n"), "{via_provider}");
    }

    /// The fused build streams the search index's chunks from the same parse pass that
    /// produces the graph, attaching each method's graph context. That context must be
    /// byte-identical to `GraphDb::graph_context` for the stored graph (so a chunk
    /// enriched by the fused path keys the same embedding as the round-trip path), and
    /// module-header chunks must carry no context.
    #[test]
    fn fused_chunks_carry_graph_context_matching_stored_graph() {
        #[derive(Default)]
        struct CollectingSink {
            rows: Vec<ide::ChunkRow>,
        }
        impl ide::FusedChunkSink for CollectingSink {
            fn emit_chunks(
                &mut self,
                chunks: &[ide::ChunkRow],
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                self.rows.extend_from_slice(chunks);
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(
            root,
            "Вызыватель",
            false,
            "Процедура Делать() Экспорт\n\
             Сервер.Считать();\n\
             Справочники.Контрагенты.НайтиПоКоду();\n\
             КонецПроцедуры",
        );
        write_common_module(root, "Сервер", true, "Функция Считать() Экспорт КонецФункции");

        let (_db, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        let mut sink = CollectingSink::default();
        let fused_project = crate::graph::ProjectSnapshot::load(root);
        let fused_universe =
            crate::graph::universe::ScannedUniverse::scan(&fused_project.scan_roots);
        crate::graph_db::build_graph_database_fused(
            &fused_project,
            &fused_universe,
            &out,
            stdx::batch::BatchBudget::files(1),
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
            &mut sink,
        )
        .expect("fused graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");

        let canon_root = root.canonicalize().unwrap().to_string_lossy().replace('\\', "/");
        let mut methods_checked = 0;
        for row in &sink.rows {
            match row.kind {
                bsl_search::ChunkKind::Procedure | bsl_search::ChunkKind::Function => {
                    let rel = row.path.strip_prefix(&canon_root).unwrap().trim_start_matches('/');
                    let id = ide::method_id_for_path(rel, &row.symbol).expect("durable id");
                    let expected =
                        gdb.graph_context(&id, fused_project.search_roots.as_ref()).unwrap();
                    assert_eq!(
                        row.graph_context, expected,
                        "fused context for {} diverges from the stored graph",
                        row.symbol
                    );
                    methods_checked += 1;
                }
                bsl_search::ChunkKind::ModuleHeader => {
                    assert_eq!(row.graph_context, None, "header chunk must have no context");
                }
            }
        }
        assert_eq!(methods_checked, 2, "both methods should be chunked and checked");

        // The calling method's context carries its call and metadata read.
        let caller = sink.rows.iter().find(|r| r.symbol == "Делать").unwrap();
        let ctx = caller.graph_context.as_deref().expect("caller has context");
        assert!(ctx.contains("\nCalls: Считать\n"), "{ctx}");
        assert!(ctx.contains("\nReads: Справочник.Контрагенты\n"), "{ctx}");
    }

    /// Resume/incremental contract for the fused embedding pass. Re-running the fused
    /// writer over an UNCHANGED file must not wipe its already-computed embedding — a
    /// restart resumes instead of paying to re-embed the whole corpus on every graph
    /// rebuild. A CHANGED file must be re-ingested back to a pending (NULL) embedding so
    /// only the change is recomputed.
    #[test]
    fn fused_writer_preserves_embeddings_for_unchanged_files() {
        use ide::FusedChunkSink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path();
        let file = source.join("CommonModule.bsl");
        fs::write(&file, "Процедура Делать() Экспорт\nКонецПроцедуры").unwrap();

        let db_path = source.join("bsl-search.db");
        let mut engine = bsl_search::SearchEngine::fts_only(&db_path).unwrap();

        let abs = file.canonicalize().unwrap().to_string_lossy().replace('\\', "/");
        let row = ide::ChunkRow {
            path: abs,
            symbol: "Делать".to_owned(),
            kind: bsl_search::ChunkKind::Procedure,
            is_export: true,
            annotations: Vec::new(),
            line_start: 1,
            line_end: 2,
            text: "Процедура Делать() Экспорт\nКонецПроцедуры".to_owned(),
            graph_context: None,
        };

        {
            let mut writer = FusedChunkWriter::new(
                &mut engine,
                source.to_path_buf(),
                crate::workspace_lease::WorkspaceLease::unmanaged(),
            );
            writer.emit_chunks(std::slice::from_ref(&row)).unwrap();
        }

        // One chunk written; its embedding is still NULL, so it is pending.
        let pending = engine.store().load_pending_embedding_documents("code").unwrap();
        assert_eq!(pending.len(), 1, "the freshly ingested chunk is pending");
        let chunk_id = pending[0].0;

        // Pay for its embedding, then confirm nothing is pending.
        engine.store().set_chunk_embedding(chunk_id, &vec![0.1_f32; 1024]).unwrap();
        assert!(
            engine.store().load_pending_embedding_documents("code").unwrap().is_empty(),
            "after embedding, nothing is pending"
        );

        // Re-run the fused writer over the UNCHANGED file: the embedding must survive.
        {
            let mut writer = FusedChunkWriter::new(
                &mut engine,
                source.to_path_buf(),
                crate::workspace_lease::WorkspaceLease::unmanaged(),
            );
            writer.emit_chunks(std::slice::from_ref(&row)).unwrap();
        }
        assert!(
            engine.store().load_pending_embedding_documents("code").unwrap().is_empty(),
            "an unchanged file keeps its embedding across a fused rebuild (resume, not re-embed)"
        );
        assert_eq!(engine.chunk_count().unwrap(), 1, "no duplicate chunk");

        // Change the file on disk: the next fused pass re-ingests it to a pending
        // embedding, so only the changed file is recomputed.
        fs::write(&file, "Процедура Делать() Экспорт\nВыполнить();\nКонецПроцедуры").unwrap();
        {
            let mut writer = FusedChunkWriter::new(
                &mut engine,
                source.to_path_buf(),
                crate::workspace_lease::WorkspaceLease::unmanaged(),
            );
            writer.emit_chunks(std::slice::from_ref(&row)).unwrap();
        }
        assert_eq!(
            engine.store().load_pending_embedding_documents("code").unwrap().len(),
            1,
            "a changed file is re-ingested back to a pending embedding"
        );
    }

    /// The build parallelises per-module resolution within a batch. A batch holding
    /// several modules that call each other and touch the same metadata object must
    /// still produce the fold's graph exactly — same edges, and the shared `Mdo`
    /// node spelled by whichever module the deterministic (file-order) projection
    /// sees first. Built with a batch large enough to hold every module at once, so
    /// the concurrent `map_with` path is exercised, not the one-module-per-batch case.
    #[test]
    fn parallel_multi_module_batch_matches_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write(
            root,
            "Catalogs/Номенклатура.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Номенклатура</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
        );
        // Both modules touch the catalog through both edge passes — a manager call
        // (Pass 2) and a query (Pass 3) — so the parallel collection of call summaries
        // AND of SDBL query refs is exercised across multiple modules in one batch.
        write_common_module(
            root,
            "Альфа",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\nБета.ШагБ();\nСправочники.Номенклатура.СоздатьЭлемент();\nЗапрос = \"ВЫБРАТЬ Код ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );
        write_common_module(
            root,
            "Бета",
            true,
            "&НаСервере\nПроцедура ШагБ() Экспорт\nЗапрос = \"ВЫБРАТЬ Наименование ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );

        let (db, files) = load_workspace_db(root).expect("workspace loads");
        let analysis = Analysis::from_database(db);

        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        // A batch_size far above the module count puts every module in one batch.
        build_whole_graph(
            root,
            &out,
            100,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");
        let project = crate::graph::ProjectSnapshot::load(root);
        let roots = project.search_roots.expect("workspace roots");

        // Overview parity covers node/edge tallies, provenance, and the
        // centrality ranking (whose nodes carry the canonical Mdo spelling).
        let mem_overview = serde_json::to_value(analysis.graph_overview(
            GRAPH_SOURCE_ROOT,
            Some(&ide::StripRoot::resolve(root)),
            10,
        ))
        .unwrap();
        let mut sql_overview =
            serde_json::to_value(gdb.overview(10, Some(&roots)).unwrap()).unwrap();
        for node in sql_overview["top_by_centrality"].as_array_mut().unwrap() {
            if !node["location"].is_null() {
                node.as_object_mut().unwrap().remove("location");
                node["location_unavailable"] = serde_json::json!("roots_unavailable");
            }
        }
        assert_eq!(mem_overview, sql_overview, "overview JSON from a multi-module batch");
        // The module count is the true distinct-module population (both common modules
        // own methods), not just the module nodes that happen to be edge endpoints.
        assert_eq!(sql_overview["modules"], 2, "both common modules counted: {sql_overview}");

        // `resolve` parity: a bare method name yields the same candidates from both paths.
        let mem_resolve = serde_json::to_value(analysis.graph_resolve(
            GRAPH_SOURCE_ROOT,
            Some(&ide::StripRoot::resolve(root)),
            "ШагБ",
            10,
        ))
        .unwrap();
        let sql_resolve = serde_json::to_value(gdb.resolve("ШагБ", 10).unwrap()).unwrap();
        assert_eq!(mem_resolve, sql_resolve, "resolve candidates from a multi-module batch");
        assert!(
            sql_resolve["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["id"] == "method/common/Бета/ШагБ" && c["match"] == "name"),
            "ШагБ resolves to its durable id by name: {sql_resolve}"
        );
        // Guard the coverage: the query pass really produced edges across the batch,
        // so the parallel SDBL collection path is genuinely exercised, not vacuous.
        assert!(
            sql_overview["edge_provenance"]["inferred"].as_u64().unwrap_or(0) >= 2,
            "both modules' queries yield inferred query_ref edges: {sql_overview}"
        );

        // The single catalog Mdo node is reached identically from both modules.
        let mdo_id = "mdo/Catalog/Номенклатура";
        let params = ide::NeighborsParams {
            id: mdo_id,
            dir: ide::Direction::In,
            depth: 1,
            max_nodes: 50,
            detail: ide::GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            call_sites: false,
            max_call_sites: 0,
        };
        let mem_nb = serde_json::to_value(
            analysis
                .graph_neighbors(GRAPH_SOURCE_ROOT, Some(&ide::StripRoot::resolve(root)), &params)
                .unwrap(),
        )
        .unwrap();
        let sql_nb = serde_json::to_value(gdb.neighbors(&params, None).unwrap().unwrap()).unwrap();
        assert_eq!(mem_nb, sql_nb, "Mdo neighbours from a multi-module batch");
    }

    /// When `max_nodes` cuts through a set of equal-centrality neighbours, the
    /// in-memory and SQLite paths must keep/drop the *same* nodes — both rank by
    /// `(in_degree desc, durable id asc)`. Guards the tie-break parity.
    #[test]
    fn neighbors_tie_break_matches_across_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(root, "Ядро", true, "&НаСервере\nФункция Цель() Экспорт КонецФункции");
        // Three callers, each with in-degree 0 — a three-way centrality tie.
        write_common_module(
            root,
            "Вызовы",
            true,
            "&НаСервере\n\
             Процедура А() Экспорт Ядро.Цель(); КонецПроцедуры\n\
             Процедура Б() Экспорт Ядро.Цель(); КонецПроцедуры\n\
             Процедура В() Экспорт Ядро.Цель(); КонецПроцедуры",
        );

        let (db, files) = load_workspace_db(root).expect("workspace loads");
        let analysis = Analysis::from_database(db);

        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");

        let params = ide::NeighborsParams {
            id: "method/common/Ядро/Цель",
            dir: ide::Direction::In,
            depth: 1,
            max_nodes: 1,
            detail: ide::GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            call_sites: false,
            max_call_sites: 0,
        };
        let mem = analysis
            .graph_neighbors(GRAPH_SOURCE_ROOT, Some(&ide::StripRoot::resolve(root)), &params)
            .unwrap();
        let sql = gdb.neighbors(&params, None).unwrap().unwrap();

        assert_eq!(mem.total, 3, "all three tied callers counted");
        assert_eq!(mem.nodes.len(), 1);
        assert_eq!(mem.dropped.len(), 2);
        // Explicit counts: returned matches nodes, dropped_count = total - returned.
        assert_eq!(mem.returned, 1);
        assert_eq!(mem.dropped_count, 2);
        assert_eq!(mem.dropped_count, mem.total - mem.returned);
        // The cut resolves identically on both paths, not just by count.
        assert_eq!(
            serde_json::to_value(&mem).unwrap(),
            serde_json::to_value(&sql).unwrap(),
            "tie-break keeps/drops the same nodes on both paths"
        );
    }

    /// The SQLite reader must keep the in-memory resolver's id semantics: a
    /// malformed id is `BadId` (not `NotFound`), and a metadata id resolves
    /// case-insensitively on its type and object name.
    #[test]
    fn sqlite_serving_bad_id_and_case_insensitive_mdo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write(
            root,
            "Catalogs/Номенклатура.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Номенклатура</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#,
        );
        write(
            root,
            "CommonModules/Менеджер/Ext/Module.bsl",
            "Процедура Создать() Экспорт\nСправочники.Номенклатура.СоздатьЭлемент();\nКонецПроцедуры",
        );

        let files = enumerate_bsl_files(&crate::graph::ProjectSnapshot::load(root)).len();
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("opens");

        let canonical = gdb
            .overview(50, None)
            .unwrap()
            .top_by_centrality
            .iter()
            .find(|n| n.kind == "mdo")
            .map(|n| n.id.clone())
            .expect("a catalog Mdo node");
        assert_eq!(canonical, "mdo/Catalog/Номенклатура");

        // Case-insensitive on the object name and ASCII type segment, and accepting
        // a localized type spelling (Справочник → Catalog).
        for variant in
            ["mdo/Catalog/НОМЕНКЛАТУРА", "mdo/catalog/номенклатура", "mdo/Справочник/Номенклатура"]
        {
            let r = gdb
                .node(variant, ide::GraphDetail::Names, None)
                .unwrap()
                .unwrap_or_else(|e| panic!("{variant} should resolve, got {e:?}"));
            assert_eq!(r.node.id, canonical, "{variant} resolves to the canonical node");
        }

        // Malformed ids are BadId, not NotFound.
        for garbage in ["garbage", "mdo/NoSuchType/X", "method/file/x"] {
            assert!(
                matches!(
                    gdb.node(garbage, ide::GraphDetail::Names, None).unwrap(),
                    Err(ide::GraphError::BadId { .. })
                ),
                "{garbage} must be BadId"
            );
        }
        // Well-formed but absent → NotFound.
        assert!(matches!(
            gdb.node("method/common/Нет/М", ide::GraphDetail::Names, None).unwrap(),
            Err(ide::GraphError::NotFound { .. })
        ));
    }

    #[test]
    fn fingerprint_changes_on_bsl_edit_and_xml_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let base = workspace_fingerprint(root);

        // A `.bsl` body edit (different length) shifts the fingerprint.
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции",
        );
        let after_bsl = workspace_fingerprint(root);
        assert_ne!(base, after_bsl, "a .bsl edit must change the fingerprint");

        // A `.xml` metadata edit must also shift it — graph resolution depends on
        // configuration metadata, not only module text.
        write(root, "CommonModules/Сервер.xml", "<MetaDataObject/>");
        let after_xml = workspace_fingerprint(root);
        assert_ne!(after_bsl, after_xml, "a .xml metadata edit must change the fingerprint");
    }

    /// A `dependsOn`-only config edit touches no file the stats fold sees, so the
    /// topology component is the ONLY channel that can report it. If the fold were
    /// files-only, this drift would be invisible forever.
    #[test]
    fn fingerprint_topology_component_tracks_a_depends_on_only_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_extension_workspace(root, false);
        let base = workspace_fingerprint(root);

        write_extension_config(root, true);
        let after = workspace_fingerprint(root);
        assert_eq!(base.files, after.files, "no scanned file moved");
        assert_ne!(base.topology, after.topology, "the dependency edge changed the topology");
    }

    /// An extension appearing through zero-config auto-discovery (no analyzer config
    /// file exists at all) must flow into the topology component too — visibility
    /// re-shapes without a single config-file stat to observe.
    #[test]
    fn an_auto_discovered_extension_changes_the_topology_component() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let base = workspace_fingerprint(root);

        write(root, "src/cfe/NewExt/Configuration.xml", "<Configuration/>");
        let after = workspace_fingerprint(root);
        assert_ne!(base.topology, after.topology, "discovery must reshape the topology");
    }

    /// The offline-edit warm start (daemon down while `dependsOn` changed): the
    /// stale cache is served, and the catch-up publish must hand its hook
    /// `topology_changed = true` — that request is what re-renders persisted
    /// search contexts built under the old topology. A files-only drift must NOT
    /// raise it.
    #[test]
    fn a_topology_only_warm_start_requests_a_whole_collection_context_refresh() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_extension_workspace(root, false);
        seed_cache(root, workspace_fingerprint(root));
        write_extension_config(root, true); // offline dependsOn edit

        let requested = Arc::new(AtomicBool::new(false));
        let hook = {
            let requested = Arc::clone(&requested);
            Arc::new(move |signal: crate::graph::GraphPublishSignal| {
                if signal.topology_changed {
                    requested.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                crate::graph::GraphPublishOutcome::HANDLED
            })
                as Arc<
                    dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome
                        + Send
                        + Sync,
                >
        };
        let graph = GraphState::for_workspace(root.to_path_buf()).with_publish_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);

        wait_until(
            &graph,
            "the catch-up publish after a topology-only warm start to request the refresh",
            || requested.load(std::sync::atomic::Ordering::SeqCst),
        );
    }

    /// Serving a stale cache is the right trade when the workspace's FILES moved — stale
    /// answers beat "still indexing" for the minutes a rebuild takes. It is the wrong trade
    /// when the extension TOPOLOGY moved: that build resolves names against a project shape
    /// this workspace no longer has, and once adopted every later freshness check compares
    /// against the foreign topology and finds it consistent. Drop the topology check in
    /// `try_publish_stale_and_catch_up` and the foreign build is published as this
    /// workspace's answer.
    #[test]
    fn a_stale_cache_from_another_topology_is_not_published() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_extension_workspace(root, false);
        seed_cache(root, workspace_fingerprint(root));
        write_extension_config(root, true); // offline dependsOn edit

        let graph = GraphState::for_workspace(root.to_path_buf());
        assert!(
            matches!(graph.try_publish_stale_and_catch_up(root), PublishAttemptOutcome::FallBack),
            "a build made under another topology is not served, however stale-tolerant we are",
        );
        assert!(
            graph.hook_debt().topology,
            "and the whole-collection context re-render is still requested",
        );
    }

    /// A cached on-disk graph built under one dependency graph is dead the moment the
    /// declared topology changes, even though not one indexed file moved.
    #[test]
    fn cached_build_is_not_reused_after_a_topology_only_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_extension_workspace(root, false);
        seed_cache(root, workspace_fingerprint(root));

        let graph = GraphState::for_workspace(root.to_path_buf());
        assert!(matches!(graph.try_publish_cached(root, 0), PublishAttemptOutcome::Published));

        write_extension_config(root, true);
        let graph = GraphState::for_workspace(root.to_path_buf());
        assert!(
            matches!(graph.try_publish_cached(root, 0), PublishAttemptOutcome::FallBack),
            "a dependsOn-only edit must invalidate the cached graph"
        );
    }

    /// A build persists a per-file fingerprint for every `.bsl` AND `.xml` file, so
    /// a later reload can classify drift granularly. `sig_hash` is NULL for now.
    #[test]
    fn build_persists_per_file_fingerprints_for_bsl_and_xml() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");

        let conn = Connection::open(&out).unwrap();
        let bsl: i64 = conn
            .query_row("SELECT COUNT(*) FROM files WHERE path LIKE '%.bsl'", [], |r| r.get(0))
            .unwrap();
        let xml: i64 = conn
            .query_row("SELECT COUNT(*) FROM files WHERE path LIKE '%.xml'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bsl, 2, "both common-module bodies are fingerprinted");
        assert_eq!(xml, 2, "both common-module descriptors are fingerprinted");

        // The stored fingerprints match a fresh stat-scan: an unchanged workspace
        // classifies as an empty diff.
        let project = crate::graph::ProjectSnapshot::load(root);
        let stored = read_stored_fingerprints_with_roots(&out);
        assert_eq!(stored.len(), 4);
        let diff = classify_changes_with_roots(
            &stored,
            &scan_file_stats(root),
            project.search_roots.as_ref(),
        );
        assert!(
            diff.is_empty(),
            "unchanged workspace ⇒ empty diff: {:?}",
            (&diff.added, &diff.removed, &diff.modified)
        );

        // Every `.bsl` module carries a signature hash; `.xml` descriptors stay NULL.
        let bsl_sigs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%.bsl' AND sig_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let xml_sigs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%.xml' AND sig_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bsl_sigs, 2, "both module bodies get a signature hash");
        assert_eq!(xml_sigs, 2, "both XML descriptors carry semantic hashes");
    }

    /// The persisted signature hash is stable across a body-only edit (same method
    /// names/exports/dispatch) but changes when a signature does — the exact property
    /// the body-only fast path relies on.
    #[test]
    fn sig_hash_stable_across_body_edit_changes_on_signature_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать(Знач А) Экспорт КонецФункции",
        );
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let server_sig = |out: &Path| -> i64 {
            Connection::open(out)
                .unwrap()
                .query_row(
                    "SELECT sig_hash FROM files WHERE path LIKE '%Сервер/Ext/Module.bsl'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };

        build_whole_graph(root, &out, 1, &meta()).expect("builds");
        let base = server_sig(&out);

        // Body-only edit: same signature including its parameter list, new body.
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать(Знач А) Экспорт\nБ = А + 1; Возврат Б;\nКонецФункции",
        );
        build_whole_graph(root, &out, 1, &meta()).expect("rebuilds");
        assert_eq!(server_sig(&out), base, "a body-only edit leaves the signature hash unchanged");

        // Parameter composition changes resolution semantics even when name/export stay fixed.
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать(Знач А, Б) Экспорт КонецФункции",
        );
        build_whole_graph(root, &out, 1, &meta()).expect("rebuilds");
        assert_ne!(
            server_sig(&out),
            base,
            "changing the exported parameter list reopens caller resolution"
        );

        // Signature edit: rename the function. The hash must move.
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать2(Знач А) Экспорт КонецФункции",
        );
        build_whole_graph(root, &out, 1, &meta()).expect("rebuilds");
        assert_ne!(server_sig(&out), base, "renaming a method changes the signature hash");
    }

    // ---- call sites on edges ------------------------------------------------------

    /// Build the artefact for `root` and open it together with the workspace's own root
    /// table, which is what turns a recorded span into an addressable place.
    fn built_with_roots(root: &Path) -> (GraphDb, bsl_search::WorkspaceRoots) {
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens and validates");
        let (roots, _rejected) = bsl_search::WorkspaceRoots::build(root, root, &[]);
        (gdb, roots)
    }

    fn asking_for_call_sites(id: &str, dir: ide::Direction) -> ide::NeighborsParams<'_> {
        ide::NeighborsParams {
            id,
            dir,
            depth: 1,
            max_nodes: 50,
            detail: ide::GraphDetail::Names,
            provenance_filter: Vec::new(),
            edge_kind_filter: Vec::new(),
            call_sites: true,
            max_call_sites: crate::tools::graph::DEFAULT_CALL_SITE_CAP,
        }
    }

    /// The text a place cuts, read back through the published UTF-16 positions — so an
    /// assertion is about the source a consumer would get, not about numbers agreeing with
    /// themselves.
    fn cut(text: &str, place: &serde_json::Value, key: &str) -> String {
        let range = &place[key];
        let index = line_index::LineIndex::new(text);
        let offset = |line: &str, ch: &str| -> usize {
            let line = range[line].as_u64().expect("a published line") as u32;
            let utf16_col = range[ch].as_u64().expect("a published character") as u32;
            let byte_col = index
                .utf16_col_to_byte_col(text, line, utf16_col)
                .expect("the published column is inside its line");
            let start = index.try_line_start(line).expect("the published line is inside the file");
            u32::from(start) as usize + byte_col as usize
        };
        text[offset("start_line", "start_character")..offset("end_line", "end_character")]
            .to_string()
    }

    /// Two calls to one method from one body are ONE edge with TWO places, and each place
    /// cuts the call it stands for.
    ///
    /// The positive control is the second caller: it calls once, so a projection that
    /// reported a fixed number of places, or one that lost the multiplicity by deduplicating
    /// rows, would disagree with one of the two edges in the same answer.
    #[test]
    fn an_edge_carries_one_place_per_call_and_each_place_cuts_that_call() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(
            root,
            "Сервер",
            true,
            "&НаСервере\nФункция Считать() Экспорт КонецФункции",
        );
        write_common_module(
            root,
            "Дважды",
            true,
            "&НаСервере\nПроцедура Оба() Экспорт\nСервер.Считать();\nСервер.Считать();\nКонецПроцедуры",
        );
        write_common_module(
            root,
            "Однажды",
            true,
            "&НаСервере\nПроцедура Раз() Экспорт\nСервер.Считать();\nКонецПроцедуры",
        );

        let (gdb, roots) = built_with_roots(root);
        let params = asking_for_call_sites("method/common/Сервер/Считать", ide::Direction::In);
        let result = gdb.neighbors(&params, Some(&roots)).unwrap().unwrap();

        let by_caller = |module: &str| {
            result
                .edges
                .iter()
                .find(|e| e.from.as_deref() == Some(module))
                .unwrap_or_else(|| panic!("an edge from {module}: {:?}", result.edges))
        };
        let twice = by_caller("method/common/Дважды/Оба");
        let once = by_caller("method/common/Однажды/Раз");

        assert_eq!(twice.call_sites_total, Some(2), "two calls, two recorded places");
        assert_eq!(once.call_sites_total, Some(1), "the control caller calls once");
        assert!(twice.call_sites_unavailable.is_none() && once.call_sites_unavailable.is_none());

        let places = twice.call_sites.as_ref().expect("places");
        assert_eq!(places.len(), 2);
        assert!(!twice.call_sites_truncated, "nothing was cut, so nothing may claim it was");

        let text = fs::read_to_string(root.join("CommonModules/Дважды/Ext/Module.bsl")).unwrap();
        for place in places {
            // The first thing the task asks for is that this be the SAME object the other
            // tools publish. Nothing else here would notice a place that quietly grew its
            // own shape, so the contract's own type is what accepts it — `deny_unknown_fields`
            // included.
            let parsed: crate::tools::location::WireLocation =
                serde_json::from_value(place.clone()).unwrap_or_else(|e| {
                    panic!("a call site is a location contract v1 place: {e}: {place}")
                });
            assert!(parsed.range.is_some() && parsed.enclosing_range.is_some());

            assert_eq!(place["path"], "CommonModules/Дважды/Ext/Module.bsl");
            assert_eq!(place["root_id"], "");
            assert_eq!(place["position_encoding"], "utf-16");
            assert_eq!(place["schema_version"], "1");
            assert_eq!(cut(&text, place, "range"), "Сервер.Считать()");
            let enclosing = cut(&text, place, "enclosing_range");
            assert!(
                enclosing.contains("Процедура Оба()") && enclosing.ends_with("КонецПроцедуры"),
                "the enclosing range is the whole calling declaration, got {enclosing:?}"
            );
            assert!(
                enclosing.contains(&cut(&text, place, "range")),
                "the enclosing range contains the call it encloses"
            );
        }
        // Ordered by position, not by whatever order the store walked the rows in.
        let first = &places[0]["range"];
        let second = &places[1]["range"];
        assert!(
            first["start_line"].as_u64() < second["start_line"].as_u64(),
            "places are ordered by position: {places:?}"
        );
    }

    /// An edge nobody asked about carries no `call_site*` key at all — which is what makes
    /// "no place" a different answer from "not asked".
    ///
    /// Both halves run over the SAME artefact and the same edge, so the difference is the
    /// request and nothing else.
    #[test]
    fn an_edge_not_asked_about_is_silent_and_an_edge_without_a_place_names_why() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_common_module(
            root,
            "Читатель",
            true,
            "&НаСервере\nПроцедура Читать() Экспорт\nЗапрос = \"ВЫБРАТЬ Код ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );

        let (gdb, roots) = built_with_roots(root);
        let id = "method/common/Читатель/Читать";

        let mut silent = asking_for_call_sites(id, ide::Direction::Out);
        silent.call_sites = false;
        let unasked = gdb.neighbors(&silent, Some(&roots)).unwrap().unwrap();
        let quiet = unasked.edges.first().expect("the query read is an edge");
        assert_eq!(quiet.kind, "query_ref");
        assert!(
            quiet.call_sites.is_none()
                && quiet.call_sites_total.is_none()
                && quiet.call_sites_unavailable.is_none()
                && !quiet.call_sites_truncated,
            "an unasked edge says nothing about places: {quiet:?}"
        );

        let asked = gdb
            .neighbors(&asking_for_call_sites(id, ide::Direction::Out), Some(&roots))
            .unwrap()
            .unwrap();
        let named = asked.edges.first().expect("the same edge");
        assert_eq!(named.kind, "query_ref");
        // The read IS written in the module; this build keeps no span for it. Saying
        // `no_call_site` here would teach the consumer to stop expecting one.
        assert_eq!(named.call_sites_unavailable, Some(ide::CALL_SITE_NOT_RECORDED));
        assert!(named.call_sites.is_none() && named.call_sites_total.is_none());
    }

    /// A structural edge — one derived from metadata rather than from code — says there is
    /// no call site at all, and says it with the other code.
    #[test]
    fn a_metadata_derived_edge_says_it_has_no_call_site() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_catalog_form(
            root,
            "Номенклатура",
            "ФормаЭлемента",
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nКонецПроцедуры",
        );

        let (gdb, roots) = built_with_roots(root);
        let params =
            asking_for_call_sites("form/Catalog/Номенклатура/ФормаЭлемента", ide::Direction::Out);
        let result = gdb.neighbors(&params, Some(&roots)).unwrap().unwrap();

        let contains: Vec<_> = result.edges.iter().filter(|e| e.kind == "contains").collect();
        assert!(!contains.is_empty(), "a form contains its items: {:?}", result.edges);
        for edge in contains {
            assert_eq!(edge.call_sites_unavailable, Some(ide::NO_CALL_SITE));
            assert!(edge.call_sites.is_none());
        }
    }

    /// A file that moved under ONE of an edge's recorded spans takes the whole list with it.
    ///
    /// The positive control is the same artefact answering before the edit: without it the
    /// test could not tell "the drift was caught" from "this edge never had places". The
    /// edit is chosen so the FIRST span still lands on its call — a per-span drop would
    /// leave one place and a `call_sites_total` of two, which reads as an undeclared
    /// truncation, and that is the answer this rule exists to forbid.
    #[test]
    fn one_drifted_span_takes_the_whole_place_list_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(
            root,
            "Сервер",
            true,
            "&НаСервере\nФункция Считать() Экспорт КонецФункции",
        );
        let module = "CommonModules/Дважды/Ext/Module.bsl";
        write_common_module(
            root,
            "Дважды",
            true,
            "&НаСервере\nПроцедура Оба() Экспорт\nСервер.Считать();\nСервер.Считать();\nКонецПроцедуры",
        );

        let (gdb, roots) = built_with_roots(root);
        let params = asking_for_call_sites("method/common/Сервер/Считать", ide::Direction::In);

        let before = gdb.neighbors(&params, Some(&roots)).unwrap().unwrap();
        let edge = before.edges.first().expect("one caller");
        assert_eq!(edge.call_sites.as_ref().map(Vec::len), Some(2), "the control: both places");

        // Rewrite only the SECOND call. Everything before it keeps its offsets, so span one
        // still cuts its call and span two now cuts something else.
        write(
            root,
            module,
            "&НаСервере\nПроцедура Оба() Экспорт\nСервер.Считать();\nСервер.Иное();\nКонецПроцедуры",
        );

        let after = gdb.neighbors(&params, Some(&roots)).unwrap().unwrap();
        let edge = after.edges.first().expect("the edge is still in the artefact");
        assert_eq!(edge.call_sites_unavailable, Some(ide::SOURCE_DRIFTED));
        assert!(
            edge.call_sites.is_none() && edge.call_sites_total.is_none(),
            "a drifted edge publishes no partial list: {edge:?}"
        );
    }

    /// The cap shortens the shown list and says so; `call_sites_total` keeps counting what
    /// the artefact records, so the two numbers stay comparable.
    ///
    /// The positive control is the same graph served with a cap above the number of places:
    /// an implementation that counted `total` after truncating, or that trimmed silently,
    /// passes every other gate here and fails this pair.
    #[test]
    fn a_capped_place_list_is_declared_and_still_counts_what_it_did_not_show() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(
            root,
            "Сервер",
            true,
            "&НаСервере\nФункция Считать() Экспорт КонецФункции",
        );
        let calls = "Сервер.Считать();\n".repeat(5);
        write_common_module(
            root,
            "Пять",
            true,
            &format!("&НаСервере\nПроцедура Много() Экспорт\n{calls}КонецПроцедуры"),
        );

        let (gdb, roots) = built_with_roots(root);
        let mut params = asking_for_call_sites("method/common/Сервер/Считать", ide::Direction::In);

        params.max_call_sites = 2;
        let (capped, completeness) =
            crate::tools::graph::neighbors(&gdb, &params, 6000, Some(&roots));
        let edge = &capped["edges"][0];
        assert_eq!(edge["call_sites"].as_array().map(Vec::len), Some(2), "the cap shortened it");
        assert_eq!(edge["call_sites_total"], 5, "the total counts what the artefact records");
        assert_eq!(edge["call_sites_truncated"], true);
        let reasons = completeness.to_value();
        assert!(
            reasons["reasons"].as_array().unwrap().iter().any(|r| r["code"] == "result_cap"
                && r["detail"].as_str().unwrap().contains("call sites")),
            "the cap is named in the envelope: {reasons}"
        );

        params.max_call_sites = 10;
        let (whole, completeness) =
            crate::tools::graph::neighbors(&gdb, &params, 6000, Some(&roots));
        let edge = &whole["edges"][0];
        assert_eq!(edge["call_sites"].as_array().map(Vec::len), Some(5));
        assert_eq!(edge["call_sites_total"], 5);
        assert!(edge.get("call_sites_truncated").is_none(), "nothing was cut: {edge}");
        assert!(
            completeness.is_complete(),
            "an uncut answer is complete: {}",
            completeness.to_value()
        );
    }

    /// Every edge kind a body produces has a recorded span, so none of them may answer
    /// `no_call_site` — and none may answer `source_drifted` on a freshly built artefact.
    ///
    /// The kinds here are the ones whose `EdgeKind` is assigned during RESOLUTION rather
    /// than during extraction: a rule that decided "has a place" by kind would send exactly
    /// these to "there is no place, and there never will be" while their spans sat in the
    /// artefact. `source_drifted` is asserted absent because a name check that no legitimate
    /// call satisfies would degrade this whole class into a false drift, silently.
    #[test]
    fn every_body_derived_edge_kind_publishes_its_place_on_a_fresh_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_common_module(
            root,
            "Обработчики",
            false,
            "&НаКлиенте\nПроцедура ПослеЗакрытия(Результат, Параметры) Экспорт\nКонецПроцедуры",
        );
        write_common_module(
            root,
            "Трогает",
            false,
            "&НаКлиенте\n\
             Процедура Всё() Экспорт\n\
             Справочники.Номенклатура.СоздатьЭлемент();\n\
             Справочники.Номенклатура.НайтиПоКоду();\n\
             Оповещение = Новый ОписаниеОповещения(\"ПослеЗакрытия\", Обработчики);\n\
             КонецПроцедуры",
        );

        let (gdb, roots) = built_with_roots(root);
        let params = asking_for_call_sites("method/common/Трогает/Всё", ide::Direction::Out);
        let result = gdb.neighbors(&params, Some(&roots)).unwrap().unwrap();

        let body_derived: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, "manager_creates" | "manager_access" | "notify_ref"))
            .collect();
        assert_eq!(
            body_derived.len(),
            3,
            "one edge of each body-derived kind under test: {:?}",
            result.edges
        );
        let text = fs::read_to_string(root.join("CommonModules/Трогает/Ext/Module.bsl")).unwrap();
        for edge in body_derived {
            assert!(
                edge.call_sites_unavailable.is_none(),
                "{} has a recorded span, so it may not name an absence ({:?})",
                edge.kind,
                edge.call_sites_unavailable
            );
            let places = edge.call_sites.as_ref().expect("places");
            assert_eq!(places.len(), 1, "{} is written once", edge.kind);
            let call = cut(&text, &places[0], "range");
            assert!(
                call.contains('(') && call.ends_with(')'),
                "{} cuts its call expression, got {call:?}",
                edge.kind
            );
        }
    }

    /// A span that slid onto a call to a DIFFERENT method whose name merely CONTAINS the
    /// old one is drift, not a confirmation.
    ///
    /// This is the class a substring check cannot see: `Считать` sits inside `СчитатьИное`,
    /// so the moved span certifies itself and the published place cuts someone else's call.
    /// The neighbouring test only covers the name disappearing outright, which every
    /// weakening of this check still catches — so without this one the check is graded on
    /// the input it cannot fail.
    #[test]
    fn a_span_that_slid_onto_a_longer_name_is_drift_and_not_a_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(
            root,
            "Сервер",
            true,
            "&НаСервере\nФункция Считать() Экспорт КонецФункции\n\
             &НаСервере\nФункция СчитатьИное() Экспорт КонецФункции",
        );
        let module = "CommonModules/Дважды/Ext/Module.bsl";
        write_common_module(
            root,
            "Дважды",
            true,
            "&НаСервере\nПроцедура Оба() Экспорт\nСервер.Считать();\nСервер.Считать();\nКонецПроцедуры",
        );

        let (gdb, roots) = built_with_roots(root);
        let params = asking_for_call_sites("method/common/Сервер/Считать", ide::Direction::In);

        let before = gdb.neighbors(&params, Some(&roots)).unwrap().unwrap();
        assert_eq!(
            before.edges.first().and_then(|e| e.call_sites.as_ref()).map(Vec::len),
            Some(2),
            "the control: the unedited file confirms both spans"
        );

        // Only the second call changes, and it changes into a name that CONTAINS the old
        // one — so the moved span still reads `Считать` as a prefix.
        write(
            root,
            module,
            "&НаСервере\nПроцедура Оба() Экспорт\nСервер.Считать();\nСервер.СчитатьИное();\nКонецПроцедуры",
        );

        let after = gdb.neighbors(&params, Some(&roots)).unwrap().unwrap();
        let edge = after.edges.first().expect("the edge is still in the artefact");
        assert_eq!(
            edge.call_sites_unavailable,
            Some(ide::SOURCE_DRIFTED),
            "a span reading another method's name is not a confirmed place: {edge:?}"
        );
        assert!(edge.call_sites.is_none(), "and it publishes nothing: {edge:?}");
    }

    fn write_catalog(root: &Path, name: &str, id: u8) {
        write(
            root,
            &format!("Catalogs/{name}.xml"),
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-0000000000{id:02}">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#
            ),
        );
    }

    /// A catalog with one top-level attribute (`ИНН`) and a tabular section (`Товары`)
    /// carrying one column (`Цена`) — exercises the metadata-catalog pass:
    /// `mdo -> attribute`, `mdo -> tabular_section`, `tabular_section -> attribute`.
    fn write_catalog_with_attributes(root: &Path, name: &str, id: u8) {
        write(
            root,
            &format!("Catalogs/{name}.xml"),
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-0000000000{id:02}">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
        <ChildObjects>
            <Attribute uuid="00000000-0000-0000-0000-0000000010{id:02}">
                <Properties><Name>ИНН</Name><Type><Type>xs:string</Type></Type></Properties>
            </Attribute>
            <TabularSection uuid="00000000-0000-0000-0000-0000000020{id:02}">
                <Properties><Name>Товары</Name></Properties>
                <ChildObjects>
                    <Attribute uuid="00000000-0000-0000-0000-0000000030{id:02}">
                        <Properties><Name>Цена</Name><Type><Type>xs:string</Type></Type></Properties>
                    </Attribute>
                </ChildObjects>
            </TabularSection>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#
            ),
        );
    }

    /// Write a managed form for catalog `obj`: the `Ext/Form.xml` (two named input
    /// fields) plus the form module `Ext/Form/Module.bsl`. `module_metadata.form` is
    /// loaded from the XML by path, so the form pass sees the two elements.
    fn write_catalog_form(root: &Path, obj: &str, form: &str, module_body: &str) {
        let base = format!("Catalogs/{obj}/Forms/{form}/Ext");
        write(
            root,
            &format!("{base}/Form.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <ChildItems>
        <InputField name="ПолеКод" id="1"><DataPath>Объект.Код</DataPath></InputField>
        <InputField name="ПолеНаименование" id="2"><DataPath>Объект.Наименование</DataPath></InputField>
    </ChildItems>
</Form>"#,
        );
        write(root, &format!("{base}/Form/Module.bsl"), module_body);
    }

    /// A form with a nested group (`Группа` → `ПолеВложенное`), a root field, and two
    /// form attributes — exercises the `form_item → form_item` hierarchy and the
    /// `form → form_attribute` edges.
    fn write_catalog_form_rich(root: &Path, obj: &str, form: &str, module_body: &str) {
        let base = format!("Catalogs/{obj}/Forms/{form}/Ext");
        write(
            root,
            &format!("{base}/Form.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <ChildItems>
        <InputField name="ПолеКод" id="1"><DataPath>Объект.Код</DataPath></InputField>
        <UsualGroup name="Группа" id="10">
            <ChildItems>
                <InputField name="ПолеВложенное" id="11"><DataPath>Объект.Наименование</DataPath></InputField>
            </ChildItems>
        </UsualGroup>
    </ChildItems>
    <Attributes>
        <Attribute name="Объект"/>
        <Attribute name="СписокЗначений"/>
    </Attributes>
</Form>"#,
        );
        write(root, &format!("{base}/Form/Module.bsl"), module_body);
    }

    /// A form for object `obj` whose main attribute `Объект` is typed
    /// `CatalogObject.{obj}` (a `Ref`), with UI fields bound to: a real object
    /// attribute (`Объект.ИНН`), a tabular-section column (`Объект.Товары.Цена`), a
    /// platform standard attribute (`Объект.Код` — must NOT link, excluded from the
    /// catalog), and a broken path (`~Объект.Нет` — must be skipped). Exercises the
    /// `data_binding` cross-links. Pair with `write_catalog_with_attributes(obj)`.
    fn write_catalog_form_databinding(root: &Path, obj: &str, form: &str, module_body: &str) {
        let base = format!("Catalogs/{obj}/Forms/{form}/Ext");
        write(
            root,
            &format!("{base}/Form.xml"),
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <ChildItems>
        <InputField name="ПолеИНН" id="1"><DataPath>Объект.ИНН</DataPath></InputField>
        <InputField name="ПолеЦена" id="2"><DataPath>Объект.Товары.Цена</DataPath></InputField>
        <InputField name="ПолеКод" id="3"><DataPath>Объект.Код</DataPath></InputField>
        <InputField name="ПолеБитый" id="4"><DataPath>~Объект.Нет</DataPath></InputField>
        <InputField name="ПолеГлубокий" id="5"><DataPath>Объект.Товары.Цена.Лишнее</DataPath></InputField>
        <InputField name="ПолеПрочее" id="6"><DataPath>Прочее.Что</DataPath></InputField>
    </ChildItems>
    <Attributes>
        <Attribute name="Объект">
            <Type><v8:Type>cfg:CatalogObject.{obj}</v8:Type></Type>
            <MainAttribute>true</MainAttribute>
        </Attribute>
        <Attribute name="Прочее">
            <Type><v8:Type>xs:string</v8:Type></Type>
        </Attribute>
    </Attributes>
</Form>"#
            ),
        );
        write(root, &format!("{base}/Form/Module.bsl"), module_body);
    }

    /// Dump the data tables in a stable order so two databases can be compared for
    /// logical (byte-identical) equality independent of physical row order. Returns
    /// `(nodes, edges, in_degree, unresolved_calls)`.
    fn dump_data(path: &Path) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
        let conn = Connection::open(path).unwrap();
        let collect = |sql: &str, cols: usize| -> Vec<String> {
            let mut stmt = conn.prepare(sql).unwrap();
            let rows = stmt
                .query_map([], |r| {
                    let mut parts = Vec::with_capacity(cols);
                    for i in 0..cols {
                        parts
                            .push(r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}"))?);
                    }
                    Ok(parts.join("|"))
                })
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        let nodes = collect(
            "SELECT id, kind, name, qualified, module, file_root_id, file_path, name_offset, \
             sig_end, src_start, src_end, dispatch, is_export, addressable \
             FROM nodes ORDER BY id",
            14,
        );
        let edges = collect(
            "SELECT from_id, to_id, kind, provenance, crosses FROM edges \
             ORDER BY from_id, to_id, kind, provenance, crosses",
            5,
        );
        let in_degree = collect("SELECT id, degree FROM in_degree ORDER BY id", 2);
        let unresolved = collect(
            "SELECT target_scope, method_lower, caller_root_id, caller_path FROM unresolved_calls \
             ORDER BY target_scope, method_lower, caller_root_id, caller_path",
            4,
        );
        (nodes, edges, in_degree, unresolved)
    }

    /// The body-only fast path must produce a database byte-identical to a full
    /// rebuild of the edited tree: same nodes (incl. aux GC of an orphaned object),
    /// edges, in-degree, and meta counts. The edit changes a module's edge set (drops
    /// a manager-create that orphans one catalog, adds a query to another already
    /// referenced elsewhere) without touching any signature.
    #[test]
    fn incremental_update_matches_full_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_catalog(root, "Контрагенты", 2);
        write_common_module(
            root,
            "Альфа",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\nБета.ШагБ();\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );
        write_common_module(
            root,
            "Бета",
            true,
            "&НаСервере\nПроцедура ШагБ() Экспорт\nСправочники.Контрагенты.СоздатьЭлемент();\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // Body-only edit of Бета: same signature `Процедура ШагБ() Экспорт`. Drops the
        // Контрагенты manager-create (orphaning that catalog's Mdo node) and adds a
        // query to Номенклатура (already referenced by Альфа → existing spelling).
        write(
            root,
            "CommonModules/Бета/Ext/Module.bsl",
            "&НаСервере\nПроцедура ШагБ() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Наименование ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );
        let changed = vec![root.join("CommonModules/Бета/Ext/Module.bsl").canonicalize().unwrap()];

        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("incremental update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild of edited tree");

        let (inc_nodes, inc_edges, inc_indeg, inc_unres) = dump_data(&db_inc);
        let (full_nodes, full_edges, full_indeg, full_unres) = dump_data(&db_full);
        assert_eq!(inc_nodes, full_nodes, "nodes (incl. orphan-GC) must match a full rebuild");
        assert_eq!(inc_edges, full_edges, "edges must match a full rebuild");
        assert_eq!(inc_indeg, full_indeg, "in-degree must match a full rebuild");
        assert_eq!(inc_unres, full_unres, "unresolved_calls must match a full rebuild");

        // The orphaned Контрагенты Mdo node is gone in both.
        assert!(
            !inc_nodes.iter().any(|n| n.contains("mdo/Catalog/Контрагенты")),
            "orphaned Контрагенты Mdo node GC'd: {inc_nodes:?}"
        );

        let meta_count = |path: &Path, key: &str| -> String {
            Connection::open(path)
                .unwrap()
                .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(meta_count(&db_inc, "nodes"), meta_count(&db_full, "nodes"), "meta node count");
        assert_eq!(meta_count(&db_inc, "edges"), meta_count(&db_full, "edges"), "meta edge count");
    }

    /// The full build's form pass emits `form`/`form_item` nodes and `contains`
    /// edges (`mdo → form`, `form → form_item`) into SQLite, and the SQL serving path
    /// counts and resolves them (case-insensitively, localized type accepted).
    #[test]
    fn sqlite_build_includes_form_nodes_and_contains_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_catalog_form(
            root,
            "Номенклатура",
            "ФормаЭлемента",
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nКонецПроцедуры",
        );

        let (_, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");

        let conn = Connection::open(&out).unwrap();
        let count = |sql: &str| -> usize {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as usize
        };
        assert_eq!(count("SELECT COUNT(*) FROM nodes WHERE kind='form'"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM nodes WHERE kind='form_item'"), 2);
        // mdo → form containment.
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM edges WHERE kind='contains' \
                 AND from_id='mdo/Catalog/Номенклатура' \
                 AND to_id='form/Catalog/Номенклатура/ФормаЭлемента'"
            ),
            1,
            "mdo → form contains edge"
        );
        // form → form_item containment (one per declared element).
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM edges WHERE kind='contains' \
                 AND from_id='form/Catalog/Номенклатура/ФормаЭлемента'"
            ),
            2,
            "form → form_item contains edges"
        );

        let gdb = GraphDb::open(&out).expect("graph database opens");
        let overview = gdb.overview(10, None).unwrap();
        assert_eq!(overview.forms, 1);
        assert_eq!(overview.form_items, 2);

        // Form node resolves with a localized type segment and mixed casing.
        let node = gdb
            .node("form/Справочник/номенклатура/ФОРМАЭЛЕМЕНТА", ide::GraphDetail::Names, None)
            .unwrap()
            .expect("form node resolves case-insensitively");
        assert_eq!(node.node.id, "form/Catalog/Номенклатура/ФормаЭлемента");
        assert_eq!(node.node.kind, "form");
    }

    /// A body-only edit to a form module's `.bsl` must leave the form's structural
    /// nodes/edges byte-identical to a full rebuild: form structure comes from form
    /// XML, not the body, and the incremental reprojection never re-derives it.
    #[test]
    fn incremental_body_edit_preserves_form_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_catalog_form(
            root,
            "Номенклатура",
            "ФормаЭлемента",
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nСообщить(\"a\");\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // Body-only edit of the form module: same handler signature, different body.
        let module_rel = "Catalogs/Номенклатура/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
        write(
            root,
            module_rel,
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nСообщить(\"b\");\nКонецПроцедуры",
        );
        let changed = vec![root.join(module_rel).canonicalize().unwrap()];

        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("incremental update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild");

        let (inc_nodes, inc_edges, inc_indeg, inc_unres) = dump_data(&db_inc);
        let (full_nodes, full_edges, full_indeg, full_unres) = dump_data(&db_full);
        assert_eq!(inc_nodes, full_nodes, "nodes (incl. form/form_item) must match a full rebuild");
        assert_eq!(inc_edges, full_edges, "edges (incl. contains) must match a full rebuild");
        assert_eq!(inc_indeg, full_indeg, "in-degree must match a full rebuild");
        assert_eq!(inc_unres, full_unres, "unresolved_calls must match a full rebuild");

        // The form structure survived the body edit in the incremental path.
        assert!(
            inc_nodes.iter().any(|n| n.contains("form/Catalog/Номенклатура/ФормаЭлемента")),
            "form node preserved: {inc_nodes:?}"
        );
        assert_eq!(
            inc_edges.iter().filter(|e| e.contains("contains")).count(),
            3,
            "1 mdo→form + 2 form→form_item contains edges preserved: {inc_edges:?}"
        );
    }

    /// Form-item group hierarchy (`FormElement.parent_id`) and `Form.attributes`
    /// become graph structure: a nested element hangs off its parent group, root
    /// elements off the form, and each form attribute off the form.
    #[test]
    fn sqlite_build_models_form_hierarchy_and_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_catalog_form_rich(
            root,
            "Номенклатура",
            "ФормаЭлемента",
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nКонецПроцедуры",
        );

        let (_, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");

        let conn = Connection::open(&out).unwrap();
        let count = |sql: &str| -> usize {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as usize
        };
        let edge = |from: &str, to: &str| -> usize {
            count(&format!(
                "SELECT COUNT(*) FROM edges WHERE kind='contains' \
                 AND from_id='{from}' AND to_id='{to}'"
            ))
        };
        let form = "form/Catalog/Номенклатура/ФормаЭлемента";
        let item = |name: &str| format!("form_item/Catalog/Номенклатура/ФормаЭлемента/{name}");

        // 3 UI elements, 2 form attributes.
        assert_eq!(count("SELECT COUNT(*) FROM nodes WHERE kind='form_item'"), 3);
        assert_eq!(count("SELECT COUNT(*) FROM nodes WHERE kind='form_attribute'"), 2);

        // Roots hang off the form; the nested field hangs off its group, NOT the form.
        assert_eq!(edge(form, &item("ПолеКод")), 1, "root field → form");
        assert_eq!(edge(form, &item("Группа")), 1, "group → form");
        assert_eq!(edge(form, &item("ПолеВложенное")), 0, "nested field is NOT a form root");
        assert_eq!(
            edge(&item("Группа"), &item("ПолеВложенное")),
            1,
            "nested field → its parent group"
        );

        // Each form attribute hangs off the form.
        assert_eq!(
            edge(form, "form_attr/Catalog/Номенклатура/ФормаЭлемента/Объект"),
            1,
            "form → form_attribute Объект"
        );
        assert_eq!(
            edge(form, "form_attr/Catalog/Номенклатура/ФормаЭлемента/СписокЗначений"),
            1,
            "form → form_attribute СписокЗначений"
        );

        let gdb = GraphDb::open(&out).expect("graph database opens");
        assert_eq!(gdb.overview(10, None).unwrap().form_attributes, 2);
        // A form attribute resolves with a localized type segment and mixed casing.
        let node = gdb
            .node(
                "form_attr/Справочник/номенклатура/ФормаЭлемента/объект",
                ide::GraphDetail::Names,
                None,
            )
            .unwrap()
            .expect("form attribute resolves case-insensitively");
        assert_eq!(node.node.id, "form_attr/Catalog/Номенклатура/ФормаЭлемента/Объект");
        assert_eq!(node.node.kind, "form_attribute");

        // Served edges out of the form carry the `contains` kind (not mislabelled
        // `call`), and reach both UI items and form attributes.
        let neighbors = gdb
            .neighbors(
                &ide::NeighborsParams {
                    id: form,
                    dir: ide::Direction::Out,
                    depth: 1,
                    max_nodes: 50,
                    detail: ide::GraphDetail::Names,
                    provenance_filter: Vec::new(),
                    edge_kind_filter: Vec::new(),
                    call_sites: false,
                    max_call_sites: 0,
                },
                None,
            )
            .unwrap()
            .expect("form node resolves");
        assert!(
            !neighbors.edges.is_empty() && neighbors.edges.iter().all(|e| e.kind == "contains"),
            "all edges out of a form are `contains`: {:?}",
            neighbors.edges.iter().map(|e| e.kind).collect::<Vec<_>>()
        );
    }

    /// A body-only edit to a form module's `.bsl` must leave the form hierarchy and
    /// attribute nodes/edges byte-identical to a full rebuild (build-only structure,
    /// never re-derived by the incremental reprojection).
    #[test]
    fn incremental_body_edit_preserves_form_hierarchy_and_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_catalog_form_rich(
            root,
            "Номенклатура",
            "ФормаЭлемента",
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nСообщить(\"a\");\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        let module_rel = "Catalogs/Номенклатура/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
        write(
            root,
            module_rel,
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nСообщить(\"b\");\nКонецПроцедуры",
        );
        let changed = vec![root.join(module_rel).canonicalize().unwrap()];

        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("incremental update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild");

        let (inc_nodes, inc_edges, ..) = dump_data(&db_inc);
        let (full_nodes, full_edges, ..) = dump_data(&db_full);
        assert_eq!(inc_nodes, full_nodes, "nodes (incl. form_attribute) must match a full rebuild");
        assert_eq!(
            inc_edges, full_edges,
            "edges (incl. form_item hierarchy + form_attribute) must match a full rebuild"
        );
        // The group-hierarchy edge and the form-attribute edges survived the body edit.
        assert!(inc_edges
            .iter()
            .any(|e| e.contains("/ФормаЭлемента/Группа")
                && e.contains("/ФормаЭлемента/ПолеВложенное")));
        assert_eq!(
            inc_edges.iter().filter(|e| e.contains("form_attr/")).count(),
            2,
            "two form_attribute edges preserved: {inc_edges:?}"
        );
    }

    /// The metadata-catalog pass materialises every object's declared structure as
    /// `contains` edges, INDEPENDENT of whether code references the object. A catalog
    /// touched by no code still gets its attribute / tabular-section / column nodes.
    #[test]
    fn sqlite_build_includes_mdo_attribute_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        // Контрагенты has attributes + a tabular section but is referenced by NO code.
        write_catalog_with_attributes(root, "Контрагенты", 1);
        // A module exists only so the build has a batch to iterate (and to prove the
        // catalog object needs no code reference to appear).
        write_common_module(root, "Альфа", true, "Процедура П() Экспорт КонецПроцедуры");

        let (_, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");

        let conn = Connection::open(&out).unwrap();
        let count = |sql: &str| -> usize {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as usize
        };
        let edge = |from: &str, to: &str| -> usize {
            count(&format!(
                "SELECT COUNT(*) FROM edges WHERE kind='contains' \
                 AND from_id='{from}' AND to_id='{to}'"
            ))
        };
        let mdo = "mdo/Catalog/Контрагенты";
        // The object node exists though no code references it.
        assert_eq!(count(&format!("SELECT COUNT(*) FROM nodes WHERE id='{mdo}'")), 1);
        // mdo -> top-level attribute.
        assert_eq!(
            edge(mdo, "attribute/Catalog/Контрагенты/ИНН"),
            1,
            "mdo -> attribute (top-level)"
        );
        // mdo -> tabular_section -> column.
        assert_eq!(
            edge(mdo, "tabular_section/Catalog/Контрагенты/Товары"),
            1,
            "mdo -> tabular_section"
        );
        assert_eq!(
            edge(
                "tabular_section/Catalog/Контрагенты/Товары",
                "ts_attr/Catalog/Контрагенты/Товары/Цена"
            ),
            1,
            "tabular_section -> column"
        );
        assert_eq!(count("SELECT COUNT(*) FROM nodes WHERE kind='tabular_section'"), 1);

        let gdb = GraphDb::open(&out).expect("graph database opens");
        let overview = gdb.overview(10, None).unwrap();
        assert_eq!(overview.tabular_sections, 1);
        // ИНН + Цена both stored as `attribute`-kind nodes.
        assert_eq!(overview.attributes, 2);

        // The tabular-section column resolves with a localized type + mixed casing.
        let node = gdb
            .node("ts_attr/Справочник/контрагенты/товары/цена", ide::GraphDetail::Names, None)
            .unwrap()
            .expect("ts column resolves case-insensitively");
        assert_eq!(node.node.id, "ts_attr/Catalog/Контрагенты/Товары/Цена");
        assert_eq!(node.node.kind, "attribute");
        // And the tabular-section node itself.
        let ts = gdb
            .node("tabular_section/Справочник/Контрагенты/Товары", ide::GraphDetail::Names, None)
            .unwrap()
            .expect("tabular section resolves");
        assert_eq!(ts.node.kind, "tabular_section");
    }

    /// A body-only edit leaves the whole metadata catalog (attributes, tabular
    /// sections, columns) byte-identical to a full rebuild — it is build-only, never
    /// re-derived incrementally, and the catalog is stable under body edits.
    #[test]
    fn incremental_body_edit_preserves_mdo_attribute_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog_with_attributes(root, "Контрагенты", 1);
        write_common_module(
            root,
            "Альфа",
            true,
            "&НаСервере\nПроцедура П() Экспорт\nСообщить(\"a\");\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        let module_rel = "CommonModules/Альфа/Ext/Module.bsl";
        write(
            root,
            module_rel,
            "&НаСервере\nПроцедура П() Экспорт\nСообщить(\"b\");\nКонецПроцедуры",
        );
        let changed = vec![root.join(module_rel).canonicalize().unwrap()];

        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("incremental update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild");

        let (inc_nodes, inc_edges, inc_indeg, ..) = dump_data(&db_inc);
        let (full_nodes, full_edges, full_indeg, ..) = dump_data(&db_full);
        assert_eq!(inc_nodes, full_nodes, "catalog nodes must match a full rebuild");
        assert_eq!(inc_edges, full_edges, "catalog contains edges must match a full rebuild");
        assert_eq!(inc_indeg, full_indeg, "in-degree must match a full rebuild");
        // The catalog structure is present and survived the body edit.
        assert!(inc_nodes.iter().any(|n| n.contains("tabular_section/Catalog/Контрагенты/Товары")));
        assert!(inc_edges.iter().any(|e| e.contains("ts_attr/Catalog/Контрагенты/Товары/Цена")));
    }

    /// The form's data model links to the object structure it mirrors: a UI field's
    /// data path → the object attribute / tabular-section column it shows, and a
    /// Ref-typed form attribute → its backing object. A standard attribute and a broken
    /// path produce no edge (no dangling).
    #[test]
    fn sqlite_build_links_form_data_to_object_fields() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog_with_attributes(root, "Контрагенты", 1);
        write_catalog_form_databinding(
            root,
            "Контрагенты",
            "ФормаЭлемента",
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nКонецПроцедуры",
        );

        let (_, files) = load_workspace_db(root).expect("workspace loads");
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");

        let conn = Connection::open(&out).unwrap();
        let count = |sql: &str| -> usize {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap() as usize
        };
        let bind = |from: &str, to: &str| -> usize {
            count(&format!(
                "SELECT COUNT(*) FROM edges WHERE kind='data_binding' \
                 AND from_id='{from}' AND to_id='{to}'"
            ))
        };
        let item = |name: &str| format!("form_item/Catalog/Контрагенты/ФормаЭлемента/{name}");

        // UI field → object attribute, and → tabular-section column.
        assert_eq!(
            bind(&item("ПолеИНН"), "attribute/Catalog/Контрагенты/ИНН"),
            1,
            "field ПолеИНН shows Контрагенты.ИНН"
        );
        assert_eq!(
            bind(&item("ПолеЦена"), "ts_attr/Catalog/Контрагенты/Товары/Цена"),
            1,
            "field ПолеЦена shows the Товары.Цена column"
        );
        // Ref-typed form attribute → its backing object.
        assert_eq!(
            bind("form_attr/Catalog/Контрагенты/ФормаЭлемента/Объект", "mdo/Catalog/Контрагенты"),
            1,
            "form attribute Объект is backed by Контрагенты"
        );

        // A platform standard attribute is not in the catalog → no edge; a `~` path is
        // skipped. Neither dangles.
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM edges WHERE kind='data_binding' \
                   AND to_id LIKE '%/Контрагенты/Код'"
            ),
            0,
            "standard attribute Код is not linked"
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM edges e WHERE e.kind='data_binding' \
             AND e.from_id='{}'",
                item("ПолеБитый")
            )),
            0,
            "broken ~ path produces no binding"
        );
        // A path through a non-Ref form attribute (`Прочее.Что`) and one deeper than a
        // tabular-section column (`Объект.Товары.Цена.Лишнее`) both resolve to nothing.
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM edges WHERE kind='data_binding' \
                 AND from_id='{}'",
                item("ПолеПрочее")
            )),
            0,
            "data path through a non-Ref attribute is not linked"
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM edges WHERE kind='data_binding' \
                 AND from_id='{}'",
                item("ПолеГлубокий")
            )),
            0,
            "data path deeper than a tabular-section column is not linked"
        );
        // Exactly three data_binding edges total (ИНН, Цена, Объект).
        assert_eq!(count("SELECT COUNT(*) FROM edges WHERE kind='data_binding'"), 3);
        // Every data_binding endpoint resolves to a real node (no dangling).
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM edges e WHERE e.kind='data_binding' \
                 AND (e.from_id NOT IN (SELECT id FROM nodes) \
                   OR e.to_id NOT IN (SELECT id FROM nodes))"
            ),
            0,
            "no dangling data_binding endpoints"
        );

        // Served via SQLite: the edge carries the `data_binding` kind, and an inbound
        // query answers "which forms show this object field".
        let gdb = GraphDb::open(&out).expect("graph database opens");
        let neighbors = gdb
            .neighbors(
                &ide::NeighborsParams {
                    id: "attribute/Catalog/Контрагенты/ИНН",
                    dir: ide::Direction::In,
                    depth: 1,
                    max_nodes: 50,
                    detail: ide::GraphDetail::Names,
                    provenance_filter: Vec::new(),
                    edge_kind_filter: Vec::new(),
                    call_sites: false,
                    max_call_sites: 0,
                },
                None,
            )
            .unwrap()
            .expect("attribute node resolves");
        assert!(
            neighbors.edges.iter().any(|e| e.kind == "data_binding"),
            "the field's inbound edges include a data_binding from the form item: {:?}",
            neighbors.edges.iter().map(|e| e.kind).collect::<Vec<_>>()
        );
    }

    /// A body-only edit to a form module's `.bsl` leaves the `data_binding` cross-links
    /// byte-identical to a full rebuild — build-only, never re-derived incrementally.
    #[test]
    fn incremental_body_edit_preserves_data_binding_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog_with_attributes(root, "Контрагенты", 1);
        write_catalog_form_databinding(
            root,
            "Контрагенты",
            "ФормаЭлемента",
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nСообщить(\"a\");\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        let module_rel = "Catalogs/Контрагенты/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
        write(
            root,
            module_rel,
            "&НаКлиенте\nПроцедура ПриОткрытии(Отказ)\nСообщить(\"b\");\nКонецПроцедуры",
        );
        let changed = vec![root.join(module_rel).canonicalize().unwrap()];

        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("incremental update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild");

        let (inc_nodes, inc_edges, ..) = dump_data(&db_inc);
        let (full_nodes, full_edges, ..) = dump_data(&db_full);
        assert_eq!(inc_nodes, full_nodes, "nodes must match a full rebuild");
        assert_eq!(inc_edges, full_edges, "data_binding edges must match a full rebuild");
        assert_eq!(
            inc_edges.iter().filter(|e| e.contains("data_binding")).count(),
            3,
            "three data_binding edges preserved: {inc_edges:?}"
        );
    }

    /// A changed module referencing an existing object with a different casing must
    /// bail to a full rebuild (it may be the object's first-seen owner, whose new
    /// spelling a full rebuild would adopt but the DB-pinned fast path cannot).
    #[test]
    fn incremental_update_bails_on_aux_casing_drift() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        write_common_module(
            root,
            "Альфа",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );
        write_common_module(
            root,
            "Бета",
            true,
            "&НаСервере\nПроцедура ШагБ() Экспорт\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // Бета references the SAME catalog with a different spelling.
        write(
            root,
            "CommonModules/Бета/Ext/Module.bsl",
            "&НаСервере\nПроцедура ШагБ() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.НОМЕНКЛАТУРА\";\nКонецПроцедуры",
        );
        let changed = vec![root.join("CommonModules/Бета/Ext/Module.bsl").canonicalize().unwrap()];
        let db_inc = root.join(".build/inc.db");
        let result = update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta());
        assert!(result.is_err(), "casing drift must bail to full rebuild, got {result:?}");
    }

    /// A changed module dropping its last reference to an object that survives via an
    /// unchanged module must bail (the surviving module could re-own the object with a
    /// different canonical spelling on a full rebuild).
    #[test]
    fn incremental_update_bails_on_dropped_shared_aux() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        let body = "&НаСервере\nПроцедура {m}() Экспорт\n\
                    Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.Номенклатура\";\nКонецПроцедуры";
        write_common_module(root, "Альфа", true, &body.replace("{m}", "ШагА"));
        write_common_module(root, "Бета", true, &body.replace("{m}", "ШагБ"));

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // Бета drops its query; Альфа still references Номенклатура (it survives).
        write(
            root,
            "CommonModules/Бета/Ext/Module.bsl",
            "&НаСервере\nПроцедура ШагБ() Экспорт\nКонецПроцедуры",
        );
        let changed = vec![root.join("CommonModules/Бета/Ext/Module.bsl").canonicalize().unwrap()];
        let db_inc = root.join(".build/inc.db");
        let result = update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta());
        assert!(result.is_err(), "dropping a shared aux ref must bail, got {result:?}");
    }

    /// When two modules reference one object with inconsistent casing, the full build
    /// records it as a casing variant, and a body-only edit of a module touching that
    /// object bails to a full rebuild — even though the edit itself keeps the casing
    /// consistent (the fast path cannot reconstruct cross-module first-seen ordering).
    #[test]
    fn incremental_update_bails_on_recorded_casing_variant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Номенклатура", 1);
        // Альфа (earlier file-id) and Гамма spell the same catalog differently.
        write_common_module(
            root,
            "Альфа",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );
        write_common_module(
            root,
            "Гамма",
            true,
            "&НаСервере\nПроцедура ШагГ() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.НОМЕНКЛАТУРА\";\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // The build recorded the inconsistent casing.
        let variants: String = Connection::open(&db_pre)
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key='casing_variants'", [], |r| r.get(0))
            .unwrap();
        assert!(
            variants.lines().any(|k| k == "catalog/номенклатура"),
            "build records the casing variant: {variants:?}"
        );

        // Body-only edit of Альфа keeping its consistent casing — still bails, because
        // Альфа touches the variant object.
        write(
            root,
            "CommonModules/Альфа/Ext/Module.bsl",
            "&НаСервере\nПроцедура ШагА() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Наименование ИЗ Справочник.Номенклатура\";\nКонецПроцедуры",
        );
        let changed = vec![root.join("CommonModules/Альфа/Ext/Module.bsl").canonicalize().unwrap()];
        let db_inc = root.join(".build/inc.db");
        let result = update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta());
        assert!(result.is_err(), "touching a recorded casing variant must bail, got {result:?}");
    }

    /// A multi-file body-only edit that introduces a NEW inconsistently-cased object
    /// (one not referenced before) succeeds on the fast path AND records the variant,
    /// so a later single-module reload refuses the fast path for it.
    #[test]
    fn incremental_update_records_newly_introduced_casing_variant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_catalog(root, "Товары", 1);
        // Neither module references Товары yet.
        write_common_module(
            root,
            "Альфа",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\nКонецПроцедуры",
        );
        write_common_module(
            root,
            "Бета",
            true,
            "&НаСервере\nПроцедура ШагБ() Экспорт\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // Both modules now reference Товары with inconsistent casing.
        write(
            root,
            "CommonModules/Альфа/Ext/Module.bsl",
            "&НаСервере\nПроцедура ШагА() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.Товары\";\nКонецПроцедуры",
        );
        write(
            root,
            "CommonModules/Бета/Ext/Module.bsl",
            "&НаСервере\nПроцедура ШагБ() Экспорт\n\
             Запрос = \"ВЫБРАТЬ Код ИЗ Справочник.ТОВАРЫ\";\nКонецПроцедуры",
        );
        let changed = vec![
            root.join("CommonModules/Альфа/Ext/Module.bsl").canonicalize().unwrap(),
            root.join("CommonModules/Бета/Ext/Module.bsl").canonicalize().unwrap(),
        ];
        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("multi-file body-only update succeeds (current result is still correct)");

        // The newly-introduced inconsistency is now persisted, so a later reload bails.
        let variants: String = Connection::open(&db_inc)
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key='casing_variants'", [], |r| r.get(0))
            .unwrap();
        assert!(
            variants.lines().any(|k| k == "catalog/товары"),
            "incremental update records the introduced casing variant: {variants:?}"
        );

        // And the incremental DB is still byte-identical to a full rebuild of this tree.
        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild");
        let (inc_nodes, inc_edges, _, inc_unres) = dump_data(&db_inc);
        let (full_nodes, full_edges, _, full_unres) = dump_data(&db_full);
        assert_eq!(inc_nodes, full_nodes, "nodes match a full rebuild");
        assert_eq!(inc_edges, full_edges, "edges match a full rebuild");
        assert_eq!(inc_unres, full_unres, "unresolved_calls match a full rebuild");

        // The persisted variant set is byte-identical too (both sides sort).
        let variants_meta = |path: &Path| -> String {
            Connection::open(path)
                .unwrap()
                .query_row("SELECT value FROM meta WHERE key='casing_variants'", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            variants_meta(&db_inc),
            variants_meta(&db_full),
            "casing_variants meta row matches a full rebuild byte-for-byte"
        );
    }

    /// Caller-delta path: removing an exported method from B must update B's resolved
    /// callers (their edge to the removed method vanishes) byte-identically to a full
    /// rebuild. The reprojection set is the one `caller_delta_plan` derives.
    #[test]
    fn caller_delta_update_matches_full_rebuild_on_method_removal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(
            root,
            "Ядро",
            true,
            "&НаСервере\nПроцедура М() Экспорт КонецПроцедуры\nПроцедура Н() Экспорт КонецПроцедуры",
        );
        write_common_module(
            root,
            "Алиса",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\nЯдро.М();\nКонецПроцедуры",
        );
        write_common_module(
            root,
            "Вера",
            true,
            "&НаСервере\nПроцедура ШагВ() Экспорт\nЯдро.Н();\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // Remove Ядро.М (keep Н) — a signature change that only shrinks the resolvable
        // surface, so it is caller-delta-safe.
        write(
            root,
            "CommonModules/Ядро/Ext/Module.bsl",
            "&НаСервере\nПроцедура Н() Экспорт КонецПроцедуры",
        );
        let core_path = root.join("CommonModules/Ядро/Ext/Module.bsl").canonicalize().unwrap();
        let core_key = core_path.to_string_lossy().into_owned();

        let profiles = recompute_profiles_for_test(root, std::slice::from_ref(&core_path)).unwrap();
        let profile = profiles.get(&core_key).expect("profiled Ядро");
        let project = crate::graph::ProjectSnapshot::load(root);
        let callers = crate::graph_db::caller_delta_plan(
            &db_pre,
            &[(core_key.as_str(), profile)],
            project.search_roots.as_ref(),
        )
        .unwrap()
        .expect("method removal is caller-delta-safe");
        // Both Алиса (called the removed М) and Вера (called Н) are resolved callers.
        assert_eq!(callers.len(), 2, "both callers discovered: {callers:?}");

        let mut changed = vec![core_path];
        changed.extend(callers);
        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("caller-delta update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild");
        let (inc_nodes, inc_edges, inc_indeg, inc_unres) = dump_data(&db_inc);
        let (full_nodes, full_edges, full_indeg, full_unres) = dump_data(&db_full);
        assert_eq!(inc_nodes, full_nodes, "nodes match a full rebuild");
        assert_eq!(inc_edges, full_edges, "edges match a full rebuild");
        assert_eq!(inc_indeg, full_indeg, "in-degree matches a full rebuild");
        assert_eq!(inc_unres, full_unres, "unresolved_calls match a full rebuild");
        assert!(
            !inc_nodes.iter().any(|n| n.contains("method/common/Ядро/М")),
            "removed method node gone: {inc_nodes:?}"
        );
    }

    /// IB-3b: ADDING an exported method must reproject the callers whose previously-
    /// unresolved `Ядро.Новый()` now resolves — found via the `unresolved_calls`
    /// reverse index, not `edges_to`. Byte-identical to a full rebuild.
    #[test]
    fn caller_delta_update_matches_full_rebuild_on_method_addition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(root, "Ядро", true, "&НаСервере\nПроцедура М() Экспорт КонецПроцедуры");
        // Алиса calls Ядро.Новый, which does not exist yet → unresolved (no stored edge).
        write_common_module(
            root,
            "Алиса",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\nЯдро.Новый();\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // The build recorded Алиса's unresolved call to Ядро.Новый, and stored no edge.
        let (_, pre_edges, _, pre_unres) = dump_data(&db_pre);
        assert!(
            pre_unres.iter().any(|u| u.contains("common/Ядро") && u.contains("новый")),
            "unresolved call recorded: {pre_unres:?}"
        );
        assert!(
            !pre_edges.iter().any(|e| e.contains("method/common/Ядро/Новый")),
            "no edge to the not-yet-existing method"
        );

        // Add Ядро.Новый exported.
        write(
            root,
            "CommonModules/Ядро/Ext/Module.bsl",
            "&НаСервере\nПроцедура М() Экспорт КонецПроцедуры\nПроцедура Новый() Экспорт КонецПроцедуры",
        );
        let core_path = root.join("CommonModules/Ядро/Ext/Module.bsl").canonicalize().unwrap();
        let core_key = core_path.to_string_lossy().into_owned();
        let profiles = recompute_profiles_for_test(root, std::slice::from_ref(&core_path)).unwrap();
        let profile = profiles.get(&core_key).unwrap();
        let project = crate::graph::ProjectSnapshot::load(root);
        let callers = crate::graph_db::caller_delta_plan(
            &db_pre,
            &[(core_key.as_str(), profile)],
            project.search_roots.as_ref(),
        )
        .unwrap()
        .expect("addition is eligible via the unresolved index");
        // Алиса is found through the reverse index (it has no stored edge into Ядро).
        assert_eq!(callers.len(), 1, "the unresolved caller is discovered: {callers:?}");

        let mut changed = vec![core_path];
        changed.extend(callers);
        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("caller-delta update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild");
        let (inc_nodes, inc_edges, inc_indeg, inc_unres) = dump_data(&db_inc);
        let (full_nodes, full_edges, full_indeg, full_unres) = dump_data(&db_full);
        assert_eq!(inc_nodes, full_nodes, "nodes match a full rebuild");
        assert_eq!(inc_edges, full_edges, "edges match a full rebuild");
        assert_eq!(inc_indeg, full_indeg, "in-degree matches a full rebuild");
        assert_eq!(inc_unres, full_unres, "unresolved_calls match a full rebuild");
        assert!(
            inc_edges.iter().any(|e| e.contains("method/common/Ядро/Новый")),
            "the newly-resolving caller's edge appears: {inc_edges:?}"
        );
        assert!(
            !inc_unres.iter().any(|u| u.contains("common/Ядро") && u.contains("новый")),
            "the resolved call is no longer in the unresolved index: {inc_unres:?}"
        );
    }

    /// The published graph must not depend on how the build was batched. A body whose
    /// bytes could not be read is registered as unreadable only by the batch that read
    /// it, so a caller in another batch asks a database that never heard of that file
    /// and is told "readable" — after which a lower-priority body answers for the one
    /// nobody could read. The barrier therefore has to travel with the index, which
    /// every batch shares, not with the per-batch database.
    ///
    /// Batch size 1 puts every module in its own database, which is the whole point:
    /// with the caller and the unread base together the case hides.
    #[test]
    fn a_cross_batch_unread_base_body_still_bars_the_extension_from_answering() {
        fn build_and_dump(base_body_unreadable: bool) -> Vec<String> {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
            write_common_module(
                root,
                "Сервер",
                true,
                "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры",
            );

            // The extension adopts Сервер (a second body of the same module) and calls
            // it from a module of its own — common modules are extension-private, so
            // the caller has to live inside the extension to see both bodies.
            let ext = root.join("cfe/Расш");
            std::fs::create_dir_all(&ext).unwrap();
            std::fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
            write_common_module(
                &ext,
                "Сервер",
                true,
                "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры",
            );
            write_common_module(
                &ext,
                "Вызов",
                true,
                "&НаСервере\nПроцедура Т() Экспорт\nСервер.П();\nКонецПроцедуры",
            );

            if base_body_unreadable {
                fs::write(root.join("CommonModules/Сервер/Ext/Module.bsl"), [0xff, 0xfe]).unwrap();
            }

            let meta = crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            };
            let db = root.join(".build/graph.db");
            fs::create_dir_all(db.parent().unwrap()).unwrap();
            build_whole_graph(root, &db, 1, &meta).expect("build");
            let (_, edges, _, _) = dump_data(&db);
            edges
        }

        // Control: with every body readable the call resolves, so the absence below is
        // the barrier speaking and not a fixture that never resolved anything.
        let control = build_and_dump(false);
        assert!(
            control.iter().any(|e| e.contains("method/common/Сервер/П")),
            "control: a readable base body must resolve the call: {control:?}"
        );

        let unread = build_and_dump(true);
        assert!(
            !unread.iter().any(|e| e.contains("method/common/Сервер/П")),
            "a body behind an unread one must not answer for it, whatever the batching: {unread:?}"
        );
    }

    /// A body crossing the readable↔unread barrier changes how OTHER modules' calls
    /// resolve — calls that resolve into a SIBLING body of the same common module, in
    /// another file entirely. Nothing in the stored graph ties those callers to this
    /// file, so the body-only fast path cannot widen its delta to reach them and must
    /// decline outright. Its own signature hash has to move too, or the transition is
    /// never even offered to the plan: an empty readable body and an unread one declare
    /// exactly the same nothing.
    #[test]
    fn an_incremental_unread_transition_declines_the_body_only_fast_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        // Readable but empty: the call falls through to the extension body.
        write_common_module(root, "Сервер", true, "");

        let ext = root.join("cfe/Расш");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(
            &ext,
            "Сервер",
            true,
            "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры",
        );
        write_common_module(
            &ext,
            "Вызов",
            true,
            "&НаСервере\nПроцедура Т() Экспорт\nСервер.П();\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 2, &meta()).expect("pre build");
        let (_, pre_edges, _, _) = dump_data(&db_pre);
        assert!(
            pre_edges.iter().any(|e| e.contains("method/common/Сервер/П")),
            "control: while the base body is readable the call resolves: {pre_edges:?}"
        );

        // The base body stops being readable. Its declarations do not change — it had
        // none — so only the barrier moves.
        let base_body = root.join("CommonModules/Сервер/Ext/Module.bsl");
        fs::write(&base_body, [0xff, 0xfe]).unwrap();

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 2, &meta()).expect("full rebuild");
        let (_, full_edges, _, _) = dump_data(&db_full);
        assert!(
            !full_edges.iter().any(|e| e.contains("method/common/Сервер/П")),
            "control: a full rebuild bars the call once the base body is unread: {full_edges:?}"
        );

        // The caller that must change is `Вызов`, whose edge points at the EXTENSION
        // body's node — nothing in the stored graph connects it to this file, so the
        // body-only fast path has no way to widen its delta and must decline.
        let canonical = base_body.canonicalize().unwrap();
        let key = canonical.to_string_lossy().into_owned();
        let profiles = recompute_profiles_for_test(root, std::slice::from_ref(&canonical)).unwrap();
        let project = crate::graph::ProjectSnapshot::load(root);
        let plan = crate::graph_db::caller_delta_plan(
            &db_pre,
            &[(key.as_str(), profiles.get(&key).unwrap())],
            project.search_roots.as_ref(),
        )
        .unwrap();
        assert!(
            plan.is_none(),
            "a body crossing the unread barrier is not eligible for the body-only path: {plan:?}"
        );
    }

    /// Healing a body lifts the barrier, and the calls that were barred resolve into a
    /// SIBLING body of the same common module — so the healed file's own declarations
    /// say nothing about who has to be reprojected. Looking for callers by the names
    /// this file newly exports finds none of them when the disputed method is declared
    /// next door; they have to be found by the module SCOPE they were barred from.
    #[test]
    fn healing_a_body_reprojects_callers_barred_into_a_sibling_body() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(root, "Сервер", true, "");

        let ext = root.join("cfe/Расш");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(
            &ext,
            "Сервер",
            true,
            "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры",
        );
        write_common_module(
            &ext,
            "Вызов",
            true,
            "&НаСервере\nПроцедура Т() Экспорт\nСервер.П();\nКонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let base_body = root.join("CommonModules/Сервер/Ext/Module.bsl");
        fs::write(&base_body, [0xff, 0xfe]).unwrap();

        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 2, &meta()).expect("pre build");
        let (_, pre_edges, _, _) = dump_data(&db_pre);
        assert!(
            !pre_edges.iter().any(|e| e.contains("method/common/Сервер/П")),
            "control: the barrier holds while the base body is unread: {pre_edges:?}"
        );

        // Healed, and it declares nothing at all — least of all the disputed П.
        fs::write(&base_body, "").unwrap();
        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 2, &meta()).expect("full rebuild");
        let (_, full_edges, _, _) = dump_data(&db_full);
        assert!(
            full_edges.iter().any(|e| e.contains("method/common/Сервер/П")),
            "control: a full rebuild resolves the call once the barrier lifts: {full_edges:?}"
        );

        let canonical = base_body.canonicalize().unwrap();
        let key = canonical.to_string_lossy().into_owned();
        let expected_file_key = crate::graph::ProjectSnapshot::load(root)
            .search_roots
            .as_ref()
            .and_then(|roots| roots.key_of_path(&canonical))
            .expect("the test project assigns the unread module to a root");

        // The plan matches the recorded unread paths against these very keys, verbatim.
        // Pin that they are one spelling: the comparison is only sound because both
        // sides come from the same scanned canonical path, and this is the assertion
        // that would notice either producer starting to normalise.
        let recorded = {
            let conn = rusqlite::Connection::open(&db_pre).unwrap();
            crate::graph_db::read_unread_paths(&conn)
        };
        assert!(
            recorded.contains(&expected_file_key),
            "the artefact records the unread body under the same spelling the plan is keyed by: \
             {recorded:?} vs {key}"
        );

        let profiles = recompute_profiles_for_test(root, std::slice::from_ref(&canonical)).unwrap();
        let project = crate::graph::ProjectSnapshot::load(root);
        let plan = crate::graph_db::caller_delta_plan(
            &db_pre,
            &[(key.as_str(), profiles.get(&key).unwrap())],
            project.search_roots.as_ref(),
        )
        .unwrap();
        // Insisting on `Some` is the point. A full rebuild (`None`) would also publish
        // the right graph, so accepting it would let the scope lookup rot away unnoticed
        // — the gate would pass on a build that simply gave up.
        let callers = plan.expect("healing keeps the body-only fast path, it does not decline it");
        assert!(
            callers.iter().any(|p| p.to_string_lossy().contains("Вызов")),
            "the caller barred into the sibling body must be reprojected: {callers:?}"
        );
    }

    /// A target whose body could not be read at build time still owes its callers a
    /// reverse reference. Name resolution refuses to conclude anything about an unread
    /// module — correctly — but if that refusal also erases the reference, then healing
    /// the body reprojects nobody: `caller_delta_plan` finds no callers, and the
    /// incremental graph is published as current while missing an edge the full rebuild
    /// has. The batch size is 2 so the target shares its database with the caller;
    /// across batches an unregistered file answers "readable" and the case hides.
    #[test]
    fn caller_delta_update_heals_a_target_that_was_unread_at_build_time() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(root, "Ядро", true, "&НаСервере\nПроцедура М() Экспорт КонецПроцедуры");
        write_common_module(
            root,
            "Алиса",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт\nЯдро.Новый();\nКонецПроцедуры",
        );
        // Two bytes `read_to_string` refuses under any UID — the stand does not depend
        // on permissions, which root ignores.
        fs::write(root.join("CommonModules/Ядро/Ext/Module.bsl"), [0xff, 0xfe]).unwrap();

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 2, &meta()).expect("pre build");

        // Heal the body, exporting the method the caller was already asking for.
        write(
            root,
            "CommonModules/Ядро/Ext/Module.bsl",
            "&НаСервере\nПроцедура М() Экспорт КонецПроцедуры\nПроцедура Новый() Экспорт КонецПроцедуры",
        );
        let core_path = root.join("CommonModules/Ядро/Ext/Module.bsl").canonicalize().unwrap();
        let core_key = core_path.to_string_lossy().into_owned();
        let profiles = recompute_profiles_for_test(root, std::slice::from_ref(&core_path)).unwrap();
        let profile = profiles.get(&core_key).unwrap();
        let project = crate::graph::ProjectSnapshot::load(root);
        let callers = crate::graph_db::caller_delta_plan(
            &db_pre,
            &[(core_key.as_str(), profile)],
            project.search_roots.as_ref(),
        )
        .unwrap()
        .expect("addition is eligible via the unresolved index");
        assert_eq!(callers.len(), 1, "the caller of the healed body is discovered: {callers:?}");

        let mut changed = vec![core_path];
        changed.extend(callers);
        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 2, &meta())
            .expect("caller-delta update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 2, &meta()).expect("full rebuild");
        let (_, inc_edges, _, _) = dump_data(&db_inc);
        let (_, full_edges, _, _) = dump_data(&db_full);
        assert_eq!(inc_edges, full_edges, "edges match a full rebuild");
        assert!(
            inc_edges.iter().any(|e| e.contains("method/common/Ядро/Новый")),
            "the edge into the healed body appears: {inc_edges:?}"
        );
    }

    /// A body-only edit that ADDS an unresolved call must refresh the reverse index
    /// (so a later addition of that method finds this caller), byte-identically to a
    /// full rebuild.
    #[test]
    fn incremental_body_edit_refreshes_unresolved_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(root, "Ядро", true, "&НаСервере\nПроцедура М() Экспорт КонецПроцедуры");
        write_common_module(
            root,
            "Алиса",
            true,
            "&НаСервере\nПроцедура ШагА() Экспорт КонецПроцедуры",
        );

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
            publication_id: "test-1".to_owned(),
        };
        let db_pre = root.join(".build/pre.db");
        fs::create_dir_all(db_pre.parent().unwrap()).unwrap();
        build_whole_graph(root, &db_pre, 1, &meta()).expect("pre build");

        // Body-only edit (ШагА signature unchanged): add a call to the missing Ядро.Завтра.
        write(
            root,
            "CommonModules/Алиса/Ext/Module.bsl",
            "&НаСервере\nПроцедура ШагА() Экспорт\nЯдро.Завтра();\nКонецПроцедуры",
        );
        let changed = vec![root.join("CommonModules/Алиса/Ext/Module.bsl").canonicalize().unwrap()];
        let db_inc = root.join(".build/inc.db");
        update_bodies_for_test(root, &db_pre, &db_inc, &changed, 1, &meta())
            .expect("body-only update");

        let db_full = root.join(".build/full.db");
        build_whole_graph(root, &db_full, 1, &meta()).expect("full rebuild");
        let (_, _, _, inc_unres) = dump_data(&db_inc);
        let (_, _, _, full_unres) = dump_data(&db_full);
        assert!(
            inc_unres.iter().any(|u| u.contains("common/Ядро") && u.contains("завтра")),
            "the newly-added unresolved call is indexed: {inc_unres:?}"
        );
        assert_eq!(inc_unres, full_unres, "unresolved_calls match a full rebuild");
    }

    /// `classify_changes` sorts each modified/added/removed file into the right
    /// bucket, and `.xml` drift is flagged for the (forced) full-rebuild path.
    #[test]
    fn classify_changes_buckets_add_remove_modify_and_flags_xml() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_whole_graph(
            root,
            &out,
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .expect("graph database builds");
        let project = crate::graph::ProjectSnapshot::load(root);
        let stored = read_stored_fingerprints_with_roots(&out);

        // Modify one body, add a new module, remove an existing one.
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции",
        );
        write_common_module(
            root,
            "Новый",
            true,
            "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры",
        );
        fs::remove_file(root.join("CommonModules/Клиент/Ext/Module.bsl")).unwrap();

        let diff = classify_changes_with_roots(
            &stored,
            &scan_file_stats(root),
            project.search_roots.as_ref(),
        );
        assert!(!diff.is_empty());

        let ends = |v: &[String], suffix: &str| v.iter().filter(|p| p.ends_with(suffix)).count();
        assert_eq!(ends(&diff.modified, "Сервер/Ext/Module.bsl"), 1, "edited body is modified");
        assert_eq!(ends(&diff.added, "Новый/Ext/Module.bsl"), 1, "new body is added");
        // The new module also drops a new `.xml` descriptor → metadata drift.
        assert_eq!(ends(&diff.added, "Новый.xml"), 1, "new descriptor is added");
        assert_eq!(ends(&diff.removed, "Клиент/Ext/Module.bsl"), 1, "deleted body is removed");
        assert!(diff.touches_metadata(), "an added .xml descriptor forces the full-rebuild path");

        // A modified-only `.bsl` (no add/remove, no `.xml`) does NOT flag metadata.
        let body_only = WorkspaceDiff {
            added: vec![],
            removed: vec![],
            modified: vec!["/cfg/SomeModule/Ext/Module.bsl".to_string()],
        };
        assert!(!body_only.touches_metadata(), "a body-only change does not touch metadata");
    }

    #[test]
    fn external_change_paths_log_with_root_identity_and_relative_file_path() {
        let workspace = tempfile::tempdir().unwrap();
        let configuration = workspace.path().join("src/cf");
        fs::create_dir_all(&configuration).unwrap();
        let external = tempfile::tempdir().unwrap();
        let external_file = external.path().join("CommonModules/Ext/Ext/Module.bsl");
        fs::create_dir_all(external_file.parent().unwrap()).unwrap();
        fs::write(&external_file, "&НаСервере\nПроцедура Внешняя() КонецПроцедуры").unwrap();
        let (roots, rejected) = bsl_search::WorkspaceRoots::build(
            workspace.path(),
            &configuration,
            &[external.path().to_path_buf()],
        );
        assert!(rejected.is_empty());

        let external_path = external_file.to_string_lossy().into_owned();
        let before = FileStat::for_test(&external_path, 1, 1);
        let key = before.key(&roots).expect("registered external root key");
        let stored =
            std::collections::HashMap::from([(key.clone(), before.persisted_content_hash())]);
        let changed = FileStat::for_test(&external_path, 2, 1);
        let modified = classify_changes_with_roots(&stored, &[changed], Some(&roots));
        let removed = classify_changes_with_roots(&stored, &[], Some(&roots));
        assert_eq!(modified.modified.as_slice(), std::slice::from_ref(&external_path));
        assert_eq!(removed.removed, [external_path]);

        let expected = format!("{}/{}", key.root_id, key.path);
        assert_eq!(
            relative_event_path(Path::new(&modified.modified[0]), workspace.path(), Some(&roots)),
            expected
        );
        assert_eq!(
            relative_event_path(Path::new(&removed.removed[0]), workspace.path(), Some(&roots)),
            expected
        );
        assert_eq!(
            relative_event_path(
                &configuration.join("Catalogs/Товары.xml"),
                workspace.path(),
                Some(&roots)
            ),
            "src/cf/Catalogs/Товары.xml",
            "paths under the workspace keep their existing relative spelling"
        );
        let unregistered = tempfile::tempdir().unwrap();
        assert_eq!(
            relative_event_path(
                &unregistered.path().join("secret/Module.bsl"),
                workspace.path(),
                Some(&roots)
            ),
            "<unregistered>",
            "unknown event paths never fall back to their absolute spelling"
        );
    }

    /// End-to-end: a signature change (method removal) drifts the workspace, and the
    /// reload takes the caller-delta path — bumping the generation and serving a graph
    /// where the removed method (and its caller's edge) is gone.
    #[test]
    fn drift_with_signature_change_reloads_via_caller_delta() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(
            root,
            "Ядро",
            true,
            "&НаСервере\nФункция Цель() Экспорт КонецФункции\nФункция Прочее() Экспорт КонецФункции",
        );
        write_common_module(
            root,
            "Вызов",
            true,
            "&НаСервере\nПроцедура Звать() Экспорт\nЯдро.Цель();\nКонецПроцедуры",
        );

        let mut graph = GraphState::for_workspace(root.to_path_buf());
        graph.drift_interval = Duration::ZERO;
        graph.ensure_loading();
        wait_ready(&graph);

        let snap1 = graph.snapshot().expect("ready");
        assert!(snap1
            .graph
            .node("method/common/Ядро/Цель", ide::GraphDetail::Names, None)
            .unwrap()
            .is_ok());

        // Remove Ядро.Цель — a caller-delta-safe signature change.
        write(
            root,
            "CommonModules/Ядро/Ext/Module.bsl",
            "&НаСервере\nФункция Прочее() Экспорт КонецФункции",
        );
        let drifted = graph.freshness(&snap1);
        assert!(drifted.stale, "removal drifts the workspace");
        // Returned, so the reload's installation need not wait for it.
        drop(snap1);

        // The caller-delta reload publishes generation 2 with the method gone.
        wait_until_within(
            &graph,
            Duration::from_secs(2),
            "the caller-delta reload to publish generation 2",
            || graph.snapshot().is_some_and(|snap| snap.generation == 2),
        );
        let snap2 = graph.snapshot().expect("the caller-delta reload published");
        assert!(
            snap2
                .graph
                .node("method/common/Ядро/Цель", ide::GraphDetail::Names, None)
                .unwrap()
                .is_err(),
            "removed method no longer resolves after caller-delta reload"
        );
        // The caller's edge into the removed method is gone (Вызов has no out-edges now).
        let overview = snap2.graph.overview(10, None).expect("overview");
        assert_eq!(overview.edges, 0, "the caller's edge to the removed method vanished");
    }

    /// The straightforward sequential scan the parallel per-directory version replaces:
    /// canonicalise every file individually, dedup, in walk order. Kept as the parity
    /// oracle so the optimisation cannot silently change the file universe. Takes explicit
    /// roots (each a dir or a file) so a file-root case can be exercised too.
    #[cfg(test)]
    fn scan_stats_over_roots_reference(roots: &[PathBuf]) -> Vec<FileStat> {
        let mut stats: Vec<FileStat> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for root in roots {
            for entry in WalkDir::new(root).follow_links(true) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                match entry.path().extension().and_then(|e| e.to_str()) {
                    Some("bsl") | Some("xml") => {}
                    _ => continue,
                }
                let path =
                    entry.path().canonicalize().unwrap_or_else(|_| entry.path().to_path_buf());
                if !seen.insert(path.clone()) {
                    continue;
                }
                let (mtime, len) = entry
                    .metadata()
                    .ok()
                    .map(|m| {
                        let mtime = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        (mtime, m.len())
                    })
                    .unwrap_or((0, 0));
                let content_hash =
                    std::fs::read(&path).ok().map(|bytes| *blake3::hash(&bytes).as_bytes());
                stats.push(FileStat {
                    path: path.to_string_lossy().into_owned(),
                    canonical: path,
                    walked: entry.path().to_path_buf(),
                    mtime,
                    len,
                    content_hash,
                    stat: crate::graph::content_hash::StatIdentity {
                        len,
                        mtime_ns: mtime,
                        change: None,
                    },
                    observed_at_ns: None,
                });
            }
        }
        stats
    }

    /// The parallel, per-directory-canonical scan yields the same `(canonical path,
    /// fingerprint)` set as the sequential reference — through nested directories, a
    /// symlinked subtree, and a file symlink (all canonicalise to the same targets, so
    /// dedup collapses the duplicate reachable paths identically).
    #[test]
    fn scan_file_stats_matches_reference() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(root, "Сервер", true, "&НаСервере\nФункция Ч() Экспорт КонецФункции");
        write_common_module(
            root,
            "Клиент",
            false,
            "&НаКлиенте\nПроцедура П() Экспорт КонецПроцедуры",
        );
        // A deeper nested directory.
        write(
            root,
            "Documents/Док/Forms/Форма/Ext/Form/Module.bsl",
            "Процедура Р() КонецПроцедуры",
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // A real subtree reachable BOTH directly and through a directory symlink.
            write(root, "_real/Sub/File.bsl", "Процедура С() КонецПроцедуры");
            symlink(root.join("_real"), root.join("Linked")).unwrap();
            // A file that is itself a symlink to a real `.bsl`.
            symlink(root.join("CommonModules/Сервер/Ext/Module.bsl"), root.join("Alias.bsl"))
                .unwrap();
        }

        // A scan-root that is itself a FILE (a misconfigured extension path), which the
        // partitioning must still stat rather than silently drop. It lives OUTSIDE the
        // directory roots so it is reachable ONLY as an explicit file-root.
        let ext_dir = tempfile::tempdir().unwrap();
        let file_root = ext_dir.path().join("Standalone.xml");
        std::fs::write(&file_root, "<Configuration/>").unwrap();
        let mut roots = scan_roots(root);
        roots.push(file_root.clone());

        let key = |s: &FileStat| (s.path.clone(), s.fingerprint());
        let mut got: Vec<_> = scan_stats_over_roots(&roots).0.iter().map(key).collect();
        let mut want: Vec<_> = scan_stats_over_roots_reference(&roots).iter().map(key).collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "parallel scan must match the sequential reference byte-for-byte");
        assert!(!got.is_empty(), "the fixture produced files");
        let file_root_canonical =
            file_root.canonicalize().unwrap_or(file_root).to_string_lossy().into_owned();
        assert!(
            got.iter().any(|(p, _)| *p == file_root_canonical),
            "a file scan-root must be stat'd, not dropped",
        );
    }
}

#[cfg(test)]
mod form_twin_tests {
    use super::super::state::lock_recover;
    use super::super::test_support::{
        sample_workspace, wait_ready, write, write_extension_workspace,
    };
    use super::super::GraphState;
    use super::PublishAttemptOutcome;
    use super::GRAPH_BUILD_BATCH;
    use crate::graph_db::build_graph_database;
    use rusqlite::Connection;
    use std::path::Path;

    fn wait_for_build_to_settle(graph: &GraphState) {
        let deadline = std::time::Instant::now() + super::super::test_support::WAIT_CEILING;
        while graph.build_in_flight() {
            assert!(std::time::Instant::now() < deadline, "initial graph build did not settle");
            std::thread::yield_now();
        }
    }

    fn build(root: &Path, out: &Path) {
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        build_graph_database(
            &project,
            &universe,
            out,
            GRAPH_BUILD_BATCH,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: 0,
                built_at: "t".to_string(),
                publication_id: "test-1".to_owned(),
            },
        )
        .unwrap();
    }

    fn shape(out: &Path) -> (i64, i64, Vec<(String, String)>) {
        let conn = Connection::open(out).unwrap();
        let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();
        let edges: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0)).unwrap();
        let mut stmt = conn
            .prepare("SELECT name, qualified FROM nodes WHERE name = 'ПриОткрытии' ORDER BY id")
            .unwrap();
        let form_nodes = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        (nodes, edges, form_nodes)
    }

    fn full_projection(out: &Path) -> (Vec<String>, Vec<String>) {
        let conn = Connection::open(out).unwrap();
        let collect = |sql: &str, columns: usize| {
            let mut stmt = conn.prepare(sql).unwrap();
            stmt.query_map([], |row| {
                (0..columns)
                    .map(|index| {
                        row.get::<_, rusqlite::types::Value>(index)
                            .map(|value| format!("{value:?}"))
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(|parts| parts.join("|"))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
        };
        (
            collect(
                "SELECT id,kind,name,qualified,module,file_root_id,file_path,name_offset,sig_end,src_start,src_end,dispatch,is_export,addressable FROM nodes ORDER BY id",
                14,
            ),
            collect(
                "SELECT from_id,to_id,kind,provenance,call_start,call_end,call_absent,crosses FROM edges ORDER BY from_id,to_id,kind,provenance,call_start,call_end,call_absent,crosses",
                8,
            ),
        )
    }

    fn write_catalog_attribute(root: &Path, name: &str, attribute: &str) {
        let id = if name == "Контрагенты" { 2 } else { 1 };
        write(
            root,
            &format!("Catalogs/{name}.xml"),
            &format!(
                r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><Catalog uuid="00000000-0000-0000-0000-0000000000{id:02}"><Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties><ChildObjects><Attribute uuid="00000000-0000-0000-0000-0000000010{id:02}"><Properties><Name>{attribute}</Name><Type><Type>xs:string</Type></Type></Properties></Attribute></ChildObjects></Catalog></MetaDataObject>"#
            ),
        );
    }

    /// Дерево с форменным модулем `Module.BSL` собирается в тот же граф, что его
    /// нижнерегистровый близнец: узлы, рёбра и квалифицированное имя форменного
    /// обработчика совпадают.
    #[test]
    fn a_case_variant_form_module_builds_the_same_graph_shape() {
        let body = "&НаКлиенте\nПроцедура ПриОткрытии() Сервер.Считать();\nКонецПроцедуры";
        let lower = tempfile::tempdir().unwrap();
        sample_workspace(lower.path());
        write(lower.path(), "Catalogs/C/Forms/F/Ext/Form/Module.bsl", body);
        let lower_out = lower.path().join("out/graph.db");
        build(lower.path(), &lower_out);

        let upper = tempfile::tempdir().unwrap();
        sample_workspace(upper.path());
        write(upper.path(), "Catalogs/C/Forms/F/Ext/Form/Module.BSL", body);
        let upper_out = upper.path().join("out/graph.db");
        build(upper.path(), &upper_out);

        let (lower_nodes, lower_edges, lower_form) = shape(&lower_out);
        let (upper_nodes, upper_edges, upper_form) = shape(&upper_out);

        // Положительный контроль: форменный обработчик действительно в графе.
        assert!(!lower_form.is_empty(), "обработчик формы обязан быть узлом графа");
        assert_eq!(lower_nodes, upper_nodes, "число узлов");
        assert_eq!(lower_edges, upper_edges, "число рёбер");
        // Квалификация здесь путевая (метаданных в фикстуре нет), а написание
        // самого файла у близнецов и ДОЛЖНО отличаться — сравниваем структуру
        // с точностью до ASCII-регистра пути.
        let fold = |v: Vec<(String, String)>| -> Vec<(String, String)> {
            v.into_iter().map(|(n, q)| (n, q.to_ascii_lowercase())).collect()
        };
        assert_eq!(fold(lower_form), fold(upper_form), "квалификация форменного обработчика");
    }

    #[test]
    fn local_mdo_attribute_and_form_xml_delta_matches_cold_projection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write_catalog_attribute(root, "Товары", "ИНН");
        write_catalog_attribute(root, "Контрагенты", "Код");
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nЗапрос = \"ВЫБРАТЬ КПП ИЗ Справочник.Товары\";\nСправочники.Товары.СоздатьЭлемент();\nВозврат 1;\nКонецФункции",
        );
        write(
            root,
            "Catalogs/Товары/Forms/Карточка/Ext/Form.xml",
            r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10"><ChildItems><InputField name="ПолеКПП" id="1"><DataPath>Объект.КПП</DataPath><Events><Event name="OnChange">ПриИзменении</Event></Events></InputField></ChildItems><Attributes><Attribute name="Объект"><Type><v8:Type>cfg:CatalogObject.Товары</v8:Type></Type><MainAttribute>true</MainAttribute></Attribute></Attributes></Form>"#,
        );
        write(
            root,
            "Catalogs/Товары/Forms/Карточка/Ext/Form/Module.bsl",
            "&НаКлиенте\nПроцедура ПриИзменении(Элемент)\nКонецПроцедуры",
        );
        write(
            root,
            "Catalogs/Контрагенты/Forms/Связь/Ext/Form.xml",
            r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10"><ChildItems><InputField name="ПолеКПП" id="1"><DataPath>Товар.КПП</DataPath></InputField></ChildItems><Attributes><Attribute name="Товар"><Type><v8:Type>cfg:CatalogObject.Товары</v8:Type></Type></Attribute></Attributes></Form>"#,
        );
        write(
            root,
            "Catalogs/Контрагенты/Forms/Связь/Ext/Form/Module.bsl",
            "&НаКлиенте\nПроцедура ПриОткрытии(Элемент)\nКонецПроцедуры",
        );

        let graph = GraphState::for_workspace_with_cache(
            root.to_path_buf(),
            crate::cache::WorkspaceCacheLayout::for_workspace(root),
        );
        graph.ensure_loading();
        wait_ready(&graph);
        wait_for_build_to_settle(&graph);

        // First prove the fixture itself represents both owners and their structural
        // form edges before the metadata-only patch is allowed to touch either row.
        let cold = root.join("cold-owner.db");
        build(root, &cold);
        let initial = full_projection(&crate::cache::graph_db_path(root));
        assert_eq!(initial, full_projection(&cold), "initial owner graph equals cold build");
        assert!(initial.0.iter().any(|row| row.contains("mdo/Catalog/Контрагенты")));
        assert!(initial.0.iter().any(|row| row.contains("form/Catalog/Контрагенты/Связь")));
        assert!(initial.1.iter().any(|row| {
            row.contains("mdo/Catalog/Контрагенты")
                && row.contains("form/Catalog/Контрагенты/Связь")
                && row.contains("contains")
        }));

        write_catalog_attribute(root, "Товары", "КПП");
        build(root, &cold);
        let post_edit_cold = full_projection(&cold);
        assert!(post_edit_cold.0.iter().any(|row| row.contains("mdo/Catalog/Контрагенты")));
        assert!(post_edit_cold.0.iter().any(|row| row.contains("form/Catalog/Контрагенты/Связь")));
        assert!(post_edit_cold.1.iter().any(|row| {
            row.contains("mdo/Catalog/Контрагенты")
                && row.contains("form/Catalog/Контрагенты/Связь")
                && row.contains("contains")
        }));
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let project = crate::graph::ProjectSnapshot::load_excluding(root, &cache.exclusions(root));
        let universe = crate::graph::universe::ScannedUniverse::scan_project(&project);
        let metadata_path = root.join("Catalogs/Товары.xml");
        let (owners, forms) = crate::graph_db::local_metadata_delta(
            &project,
            &universe,
            &crate::cache::graph_db_path(root),
            std::slice::from_ref(&metadata_path),
        )
        .unwrap()
        .expect("existing catalog attribute edit is a local owner delta");
        let semantic_hash = crate::graph::scan::xml_semantic_hash_file(&metadata_path)
            .map(|hash| u64::from_le_bytes(hash[..8].try_into().expect("blake3 hash >= 8 bytes")));
        let patch = crate::graph_db::compute_body_patch_with_metadata(
            &project,
            &universe,
            &crate::cache::graph_db_path(root),
            &[],
            std::slice::from_ref(&metadata_path),
            &owners,
            &forms,
            &[(metadata_path.clone(), semantic_hash)],
            GRAPH_BUILD_BATCH,
        )
        .unwrap();
        assert!(
            patch.reprojected_modules() > 0,
            "an XML-only owner delta also reprojects its unchanged BSL consumers"
        );
        let projected = patch.rows_for_test();
        assert!(projected.nodes.iter().any(|node| node.id == "mdo/Catalog/Контрагенты"));
        assert!(projected.nodes.iter().any(|node| node.id == "form/Catalog/Контрагенты/Связь"));
        assert!(projected.edges.iter().any(|edge| {
            edge.from_id == "mdo/Catalog/Контрагенты"
                && edge.to_id == "form/Catalog/Контрагенты/Связь"
                && edge.kind == "contains"
        }));
        assert!(!graph.build_in_flight(), "metadata patch starts after the initial build settles");
        assert!(matches!(
            graph.try_incremental_reload(root, 2, 0),
            PublishAttemptOutcome::Published
        ));
        assert_eq!(
            full_projection(&cold),
            post_edit_cold,
            "incremental publish leaves cold baseline untouched"
        );

        let compare_projection = |message: &str| {
            let actual = full_projection(&crate::cache::graph_db_path(root));
            let expected = full_projection(&cold);
            let difference = |actual: &[String], expected: &[String]| {
                let mut missing = expected.to_vec();
                let mut extra = Vec::new();
                for row in actual {
                    if let Some(position) = missing.iter().position(|candidate| candidate == row) {
                        missing.remove(position);
                    } else {
                        extra.push(row.clone());
                    }
                }
                (missing, extra)
            };
            let (missing_nodes, extra_nodes) = difference(&actual.0, &expected.0);
            let (missing_edges, extra_edges) = difference(&actual.1, &expected.1);
            assert!(
                missing_nodes.is_empty()
                    && extra_nodes.is_empty()
                    && missing_edges.is_empty()
                    && extra_edges.is_empty(),
                "{message}; missing nodes={missing_nodes:?}; extra nodes={extra_nodes:?}; missing edges={missing_edges:?}; extra edges={extra_edges:?}"
            );
        };
        compare_projection("metadata-only patch preserves incoming query, manager and form edges");
        let query_and_manager: i64 = Connection::open(crate::cache::graph_db_path(root))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE kind IN ('query_ref','manager_creates') AND from_id = 'method/common/Сервер/Считать' AND to_id = 'mdo/Catalog/Товары'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            query_and_manager >= 2,
            "unchanged BSL's incoming owner edges were retained: {query_and_manager}"
        );
        let incoming_edges: i64 = Connection::open(crate::cache::graph_db_path(root))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE (kind = 'query_ref' AND from_id = 'method/common/Сервер/Считать' AND to_id = 'attribute/Catalog/Товары/КПП') OR (kind = 'data_binding' AND to_id = 'attribute/Catalog/Товары/КПП')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            incoming_edges, 3,
            "new query and both owner-local and foreign form consumers resolve the new attribute"
        );

        write(
            root,
            "Catalogs/Товары/Forms/Карточка/Ext/Form.xml",
            r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform"><ChildItems><InputField name="ПолеКПП" id="1"><DataPath>Объект.КПП</DataPath><Events><Event name="OnChange">ПриИзменении</Event></Events></InputField></ChildItems></Form>"#,
        );
        assert!(matches!(
            graph.try_incremental_reload(root, 3, 0),
            PublishAttemptOutcome::Published
        ));

        build(root, &cold);
        compare_projection("local form replacement keeps the neighboring owner and graph edges");
        assert!(full_projection(&cold).0.iter().any(|row| row.contains("mdo/Catalog/Контрагенты")));
        assert!(full_projection(&cold).0.iter().any(|row| row.contains("ПолеКПП")));
    }

    #[test]
    fn adding_and_removing_form_module_with_existing_xml_matches_cold_projection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write(
            root,
            "Catalogs/Товары/Forms/Карточка/Ext/Form.xml",
            r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform"><ChildItems><InputField name="ПолеИНН" id="1"><DataPath>Объект.Код</DataPath></InputField></ChildItems></Form>"#,
        );
        let graph = GraphState::for_workspace_with_cache(
            root.to_path_buf(),
            crate::cache::WorkspaceCacheLayout::for_workspace(root),
        );
        graph.ensure_loading();
        wait_ready(&graph);
        let cold = root.join("cold-form-membership.db");
        let compare_cold = || {
            build(root, &cold);
            assert_eq!(
                full_projection(&crate::cache::graph_db_path(root)),
                full_projection(&cold),
                "form owner membership must match a cold graph"
            );
        };

        let module = root.join("Catalogs/Товары/Forms/Карточка/Ext/Form/Module.bsl");
        write(
            root,
            "Catalogs/Товары/Forms/Карточка/Ext/Form/Module.bsl",
            "&НаКлиенте\nПроцедура ПриОткрытии(Элемент)\nКонецПроцедуры",
        );
        assert!(matches!(
            graph.try_incremental_reload(root, 2, 0),
            PublishAttemptOutcome::Published
        ));
        compare_cold();
        assert!(full_projection(&crate::cache::graph_db_path(root))
            .0
            .iter()
            .any(|row| row.contains("form/Catalog/Товары/Карточка")));

        std::fs::remove_file(module).unwrap();
        assert!(matches!(
            graph.try_incremental_reload(root, 3, 0),
            PublishAttemptOutcome::Published
        ));
        compare_cold();
        assert!(!full_projection(&crate::cache::graph_db_path(root))
            .0
            .iter()
            .any(|row| row.contains("form/Catalog/Товары/Карточка")));
    }

    fn apply_one_module_patch(root: &Path, source_db: &Path, changed_path: &Path, serial: u64) {
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let project = crate::graph::ProjectSnapshot::load_excluding(root, &cache.exclusions(root));
        let universe = crate::graph::universe::ScannedUniverse::scan_project(&project);
        let patched = root.join(format!("patched-{serial}.db"));
        crate::graph_db::update_graph_database_bodies(
            &project,
            &universe,
            source_db,
            &patched,
            &[changed_path.to_path_buf()],
            GRAPH_BUILD_BATCH,
            &crate::graph_db::GraphMeta {
                revision: serial,
                fingerprint: crate::graph_db::GraphFp::default(),
                files: universe.files.len(),
                built_at: "test".to_owned(),
                publication_id: format!("patch-{serial}"),
            },
        )
        .unwrap();
        std::fs::copy(patched, source_db).unwrap();
    }

    fn assert_last_module_roundtrip(root: &Path, module_rel: &str, body: &str) {
        let module = root.join(module_rel);
        let canonical_module = module.canonicalize().unwrap();
        let graph_db = crate::cache::graph_db_path(root);
        build(root, &graph_db);
        let cold = root.join("cold-last-module.db");
        let compare_cold = |step: &str| {
            build(root, &cold);
            assert_eq!(
                full_projection(&graph_db),
                full_projection(&cold),
                "{step}: zero/one module graph must match a cold build"
            );
        };
        compare_cold("initial module");
        let initial = full_projection(&graph_db);
        let mdo = "mdo/Catalog/Товары";
        let attribute = "attribute/Catalog/Товары/ИНН";
        assert!(initial.0.iter().any(|row| row.contains(mdo)), "custom MDO row present");
        assert!(
            initial.0.iter().any(|row| row.contains(attribute)),
            "custom attribute row present"
        );
        assert!(
            initial.1.iter().any(|row| row.contains(mdo)
                && row.contains(attribute)
                && row.contains("contains")),
            "custom catalog containment edge present"
        );

        std::fs::remove_file(&module).unwrap();
        apply_one_module_patch(root, &graph_db, &canonical_module, 2);
        compare_cold("after deleting the only module");
        assert!(
            full_projection(&graph_db).0.is_empty(),
            "zero-module cold projection has no nodes"
        );
        assert!(
            full_projection(&graph_db).1.is_empty(),
            "zero-module cold projection has no edges"
        );

        write(root, module_rel, body);
        let restored_path = module.canonicalize().unwrap();
        apply_one_module_patch(root, &graph_db, &restored_path, 3);
        compare_cold("after restoring the first module");
        let restored = full_projection(&graph_db);
        assert!(restored.0.iter().any(|row| row.contains(attribute)));
        assert!(restored.1.iter().any(|row| row.contains(mdo) && row.contains(attribute)));
    }

    fn assert_public_last_module_roundtrip(root: &Path, module_rel: &str, body: &str) {
        let module = root.join(module_rel);
        let graph = GraphState::for_workspace_with_cache(
            root.to_path_buf(),
            crate::cache::WorkspaceCacheLayout::for_workspace(root),
        );
        graph.ensure_loading();
        wait_ready(&graph);
        wait_for_build_to_settle(&graph);
        let graph_db = crate::cache::graph_db_path(root);
        let cold = root.join("cold-public-last-module.db");
        let compare_cold = |step: &str| {
            build(root, &cold);
            assert_eq!(
                full_projection(&graph_db),
                full_projection(&cold),
                "{step}: public reload must match a cold build"
            );
        };
        compare_cold("initial module");
        let initial = full_projection(&graph_db);
        let mdo = "mdo/Catalog/Товары";
        let attribute = "attribute/Catalog/Товары/ИНН";
        assert!(initial.0.iter().any(|row| row.contains(attribute)));
        assert!(initial.1.iter().any(|row| {
            row.contains(mdo) && row.contains(attribute) && row.contains("contains")
        }));
        let full_builds = graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst);

        std::fs::remove_file(&module).unwrap();
        assert!(matches!(
            graph.try_incremental_reload(root, 2, 0),
            PublishAttemptOutcome::Published
        ));
        compare_cold("after deleting the only module");
        assert!(full_projection(&graph_db).0.is_empty());
        assert!(full_projection(&graph_db).1.is_empty());
        assert_eq!(
            graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst),
            full_builds,
            "deleting the sole module used the public incremental path"
        );

        write(root, module_rel, body);
        assert!(matches!(
            graph.try_incremental_reload(root, 3, 0),
            PublishAttemptOutcome::Published
        ));
        compare_cold("after restoring the first module");
        let restored = full_projection(&graph_db);
        assert!(restored.0.iter().any(|row| row.contains(attribute)));
        assert!(restored.1.iter().any(|row| {
            row.contains(mdo) && row.contains(attribute) && row.contains("contains")
        }));
        assert_eq!(
            graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst),
            full_builds,
            "restoring the first module used the public incremental path"
        );
    }

    fn setup_custom_catalog_workspace(root: &Path) {
        sample_workspace(root);
        std::fs::remove_file(root.join("CommonModules/Клиент/Ext/Module.bsl")).unwrap();
        std::fs::remove_file(root.join("CommonModules/Сервер/Ext/Module.bsl")).unwrap();
        write_catalog_attribute(root, "Товары", "ИНН");
        write(
            root,
            "Catalogs/Товары/Forms/Карточка/Ext/Form.xml",
            r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform"><ChildItems><InputField name="ПолеИНН" id="1"><DataPath>Объект.ИНН</DataPath></InputField></ChildItems></Form>"#,
        );
    }

    #[test]
    fn removing_and_restoring_the_sole_form_module_matches_cold_projection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        std::fs::remove_file(root.join("CommonModules/Клиент/Ext/Module.bsl")).unwrap();
        std::fs::remove_file(root.join("CommonModules/Сервер/Ext/Module.bsl")).unwrap();
        write_catalog_attribute(root, "Товары", "ИНН");
        write(
            root,
            "Catalogs/Товары/Forms/Карточка/Ext/Form.xml",
            r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform"><ChildItems><InputField name="ПолеИНН" id="1"><DataPath>Объект.ИНН</DataPath></InputField></ChildItems></Form>"#,
        );
        let module_rel = "Catalogs/Товары/Forms/Карточка/Ext/Form/Module.bsl";
        let body = "&НаКлиенте\nПроцедура ПриОткрытии(Элемент)\nКонецПроцедуры";
        write(root, module_rel, body);
        assert_last_module_roundtrip(root, module_rel, body);
    }

    #[test]
    fn removing_and_restoring_the_sole_ordinary_module_matches_cold_projection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        std::fs::remove_file(root.join("CommonModules/Клиент/Ext/Module.bsl")).unwrap();
        std::fs::remove_file(root.join("CommonModules/Сервер/Ext/Module.bsl")).unwrap();
        write_catalog_attribute(root, "Товары", "ИНН");
        let module_rel = "CommonModules/Единственный/Ext/Module.bsl";
        let body = "&НаСервере\nФункция M() Экспорт\nВозврат 1;\nКонецФункции";
        crate::graph::test_support::write_common_module(root, "Единственный", true, body);
        assert_last_module_roundtrip(root, module_rel, body);
    }

    #[test]
    fn public_reload_roundtrips_the_sole_form_module_with_custom_catalog_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        setup_custom_catalog_workspace(root);
        let module_rel = "Catalogs/Товары/Forms/Карточка/Ext/Form/Module.bsl";
        let body = "&НаКлиенте\nПроцедура ПриОткрытии(Элемент)\nКонецПроцедуры";
        write(root, module_rel, body);
        assert_public_last_module_roundtrip(root, module_rel, body);
    }

    #[test]
    fn public_reload_roundtrips_the_sole_ordinary_module_with_custom_catalog_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        setup_custom_catalog_workspace(root);
        let module_rel = "CommonModules/Единственный/Ext/Module.bsl";
        let body = "&НаСервере\nФункция M() Экспорт\nВозврат 1;\nКонецФункции";
        crate::graph::test_support::write_common_module(root, "Единственный", true, body);
        assert_public_last_module_roundtrip(root, module_rel, body);
    }

    #[test]
    fn public_reload_accepts_empty_fingerprints_for_current_schema_zero_module_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        std::fs::remove_file(root.join("CommonModules/Клиент/Ext/Module.bsl")).unwrap();
        std::fs::remove_file(root.join("CommonModules/Клиент.xml")).unwrap();
        std::fs::remove_file(root.join("CommonModules/Сервер/Ext/Module.bsl")).unwrap();
        std::fs::remove_file(root.join("CommonModules/Сервер.xml")).unwrap();
        let module_rel = "CommonModules/Единственный/Ext/Module.bsl";
        let body = "Процедура M()\nКонецПроцедуры";
        write(root, module_rel, body);
        let module = root.join(module_rel);
        let graph = GraphState::for_workspace_with_cache(
            root.to_path_buf(),
            crate::cache::WorkspaceCacheLayout::for_workspace(root),
        );
        graph.ensure_loading();
        wait_ready(&graph);
        wait_for_build_to_settle(&graph);
        let graph_db = crate::cache::graph_db_path(root);
        let full_builds = graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst);

        std::fs::remove_file(&module).unwrap();
        assert!(matches!(
            graph.try_incremental_reload(root, 2, 0),
            PublishAttemptOutcome::Published
        ));
        assert_eq!(full_projection(&graph_db), (Vec::new(), Vec::new()));
        let stored_empty =
            crate::graph_db::stored_fingerprints_in(&Connection::open(&graph_db).unwrap());
        assert!(stored_empty.is_empty(), "plain-BSL zero graph has no fingerprint rows");

        write(root, module_rel, body);
        assert!(matches!(
            graph.try_incremental_reload(root, 3, 0),
            PublishAttemptOutcome::Published
        ));
        let cold = root.join("cold-no-xml-first-module.db");
        build(root, &cold);
        assert_eq!(full_projection(&graph_db), full_projection(&cold));
        assert_eq!(
            graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst),
            full_builds,
            "current-schema empty publications remain eligible for incremental reload"
        );
    }

    #[test]
    fn adding_and_removing_a_bsl_module_matches_a_cold_projection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        crate::graph::test_support::write_common_module(
            root,
            "Дополнительный",
            true,
            "&НаСервере\nФункция Extra() Экспорт\nВозврат 1;\nКонецФункции",
        );
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nВозврат Дополнительный.Extra();\nКонецФункции",
        );
        let extra = root.join("CommonModules/Дополнительный/Ext/Module.bsl");
        std::fs::remove_file(&extra).unwrap();

        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache);
        graph.ensure_loading();
        wait_ready(&graph);

        let initial = Connection::open(crate::cache::graph_db_path(root)).unwrap();
        let unresolved_rows: Vec<(String, String, String)> = initial
            .prepare("SELECT target_scope, method_lower, caller_path FROM unresolved_calls ORDER BY target_scope, method_lower")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            unresolved_rows.iter().any(|(_, method, path)| method == "extra"
                && path.ends_with("CommonModules/Сервер/Ext/Module.bsl")),
            "cold graph must record the unresolved caller reverse index: {unresolved_rows:?}"
        );

        let cold = root.join("cold.db");
        let projection = |path: &Path| {
            let conn = Connection::open(path).unwrap();
            let read = |sql: &str, count: usize| {
                let mut stmt = conn.prepare(sql).unwrap();
                stmt.query_map([], |row| {
                    (0..count)
                        .map(|i| row.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}")))
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|parts| parts.join("|"))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
            };
            (
                read(
                    "SELECT id,kind,name,qualified,module,file_root_id,file_path,name_offset,sig_end,src_start,src_end,dispatch,is_export,addressable FROM nodes ORDER BY id",
                    14,
                ),
                read(
                    "SELECT from_id,to_id,kind,provenance,call_start,call_end,call_absent,crosses FROM edges ORDER BY from_id,to_id,kind,provenance,call_start,call_end,call_absent,crosses",
                    8,
                ),
            )
        };
        let compare_cold = || {
            build(root, &cold);
            assert_eq!(projection(&crate::cache::graph_db_path(root)), projection(&cold));
        };

        std::fs::create_dir_all(extra.parent().unwrap()).unwrap();
        std::fs::write(&extra, "&НаСервере\nФункция Extra() Экспорт\nВозврат 1;\nКонецФункции")
            .unwrap();
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        let current_modules = universe
            .files
            .iter()
            .filter(|(_, path)| {
                bsl_conventions::str_has_extension(
                    path.to_string_lossy().as_ref(),
                    bsl_conventions::BSL_EXTENSION,
                )
            })
            .count();
        let profile = crate::graph_db::recompute_module_profiles(
            &project,
            &universe.files,
            std::slice::from_ref(&extra),
        )
        .unwrap();
        let profile = profile.get(&extra.to_string_lossy().to_string()).unwrap();
        let roots = project.search_roots.as_ref().unwrap();
        let planned = crate::graph_db::caller_delta_plan(
            &crate::cache::graph_db_path(root),
            &[(&extra.to_string_lossy(), profile)],
            Some(roots),
        )
        .unwrap();
        assert!(
            planned.as_ref().is_some_and(|callers| callers
                .iter()
                .any(|path| path.ends_with("CommonModules/Сервер/Ext/Module.bsl"))),
            "caller delta must select the unresolved caller; sig={}, exports={:?}, refs={unresolved_rows:?}, plan={planned:?}",
            profile.sig_hash,
            profile.exported_lower
        );
        assert_eq!(planned.as_ref().unwrap().len(), 1, "fixture has one dependent caller");
        let affected_modules = 1 + planned.as_ref().unwrap().len();
        assert_eq!(
            current_modules, 3,
            "fixture currently has the new module and two existing modules"
        );
        assert_eq!(affected_modules, 2, "the delta is the new module plus its proven caller");
        assert!(
            affected_modules * 2 > current_modules,
            "affected/current modules = {affected_modules}/{current_modules}; safe caller closure exceeds half"
        );
        assert!(matches!(
            graph.try_incremental_reload(root, 2, 0),
            PublishAttemptOutcome::Published
        ));
        compare_cold();

        std::fs::remove_file(&extra).unwrap();
        let removed = graph.try_incremental_reload(root, 3, 0);
        let (was_published, outcome) = match removed {
            PublishAttemptOutcome::Published => (true, "published".to_owned()),
            PublishAttemptOutcome::FallBack => (false, "full-build fallback".to_owned()),
            PublishAttemptOutcome::Refused(failure) => (false, failure.message),
        };
        assert!(
            was_published,
            "delete should publish incrementally ({outcome}); status={:?}; decisions={:?}",
            graph.status(),
            lock_recover(&graph.incremental_decisions)
        );
        compare_cold();
    }

    #[test]
    fn metadata_version_bump_and_touch_publish_without_bsl_reprojection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_extension_workspace(root, false);
        let config_xml_v1 = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <Configuration uuid="00000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Тестовая</Name>
            <Version>1.0.0.1</Version>
            <Comment>Базовая версия</Comment>
        </Properties>
    </Configuration>
</MetaDataObject>"#;
        write(root, "ext/a/Configuration.xml", config_xml_v1);
        let module_xml = root.join("CommonModules/Сервер.xml");
        let module_v1 = std::fs::read_to_string(&module_xml).unwrap().replace(
            "<Name>Сервер</Name>",
            "<Name>Сервер</Name><Comment>Исходный комментарий</Comment>",
        );
        std::fs::write(&module_xml, module_v1).unwrap();

        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache);
        graph.ensure_loading();
        wait_ready(&graph);

        // Edit extension Configuration.xml and a module comment only: BSL bytes stay unchanged.
        let config_xml_v2 = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <Configuration uuid="00000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Тестовая</Name>
            <Version>1.0.0.2</Version>
            <Comment>Обновленная версия</Comment>
        </Properties>
    </Configuration>
</MetaDataObject>"#;
        write(root, "ext/a/Configuration.xml", config_xml_v2);
        let module_v2 = std::fs::read_to_string(&module_xml)
            .unwrap()
            .replace("Исходный комментарий", "Обновленный комментарий");
        std::fs::write(&module_xml, module_v2).unwrap();
        let full_builds_before =
            graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst);

        let outcome = graph.try_incremental_reload(root, 2, 0);
        assert!(
            matches!(outcome, PublishAttemptOutcome::Published),
            "version bump and comment edit must not fall back to full rebuild; decisions: {:?}",
            lock_recover(&graph.incremental_decisions)
        );

        // Rewriting identical metadata bytes models a touch-only event: the scan diff is empty.
        write(root, "ext/a/Configuration.xml", config_xml_v2);
        let outcome = graph.try_incremental_reload(root, 3, 0);
        assert!(
            matches!(outcome, PublishAttemptOutcome::Published),
            "an empty content diff must acknowledge the event without a full rebuild; decisions: {:?}",
            lock_recover(&graph.incremental_decisions)
        );
        assert_eq!(
            lock_recover(&graph.incremental_decisions).as_slice(),
            ["published", "published"],
            "both no-op publications must avoid the full-build fallback"
        );
        assert_eq!(
            graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst),
            full_builds_before,
            "extension version/comment changes publish without a full graph build"
        );
    }
}
