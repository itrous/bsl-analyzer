//! Background graph build, cache adoption, and SQLite publication work.

use std::path::{Path, PathBuf};

use bsl_search::SearchEngine;

#[cfg(test)]
use crate::cache::graph_db_path;
use crate::graph_query::GraphDb;
use crate::workspace_lease::{LeaseOperationError, LeaseOperationOutcome};

#[cfg(test)]
use super::input::GRAPH_SOURCE_ROOT;
use super::scan::classify_changes;
#[cfg(test)]
use super::scan::workspace_fingerprint;
use super::snapshot::{PreparedSnapshotPool, SnapshotInstallError, SnapshotPrepareError};
use super::state::{lock_recover, GraphState, Published, ReloadState};
use super::types::GraphStatus;

/// Modules whose edges are projected per batch when building the on-disk graph.
/// 500 keeps peak RSS comfortably bounded on a 25k-module config (measured ~2.9 GB)
/// while the resident method index resolves cross-batch calls.
pub(super) const GRAPH_BUILD_BATCH: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoadFailureReason {
    TransientRefusal,
    Superseded,
    Released,
    OperationError,
}

#[derive(Debug)]
pub(super) struct LoadFailure {
    pub(super) reason: LoadFailureReason,
    pub(super) message: String,
}

impl LoadFailure {
    fn new(reason: LoadFailureReason, message: impl Into<String>) -> Self {
        Self { reason, message: message.into() }
    }

    fn operation(error: impl std::fmt::Display) -> Self {
        Self::new(LoadFailureReason::OperationError, error.to_string())
    }
}

impl std::fmt::Display for LoadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LoadFailure {}

fn install_failure(
    outcome: LeaseOperationOutcome<(), SnapshotInstallError>,
) -> Result<(), LoadFailure> {
    match outcome {
        LeaseOperationOutcome::Applied(()) => Ok(()),
        LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(
            SnapshotInstallError::Changed,
        )) => Err(LoadFailure::new(
            LoadFailureReason::OperationError,
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
        SnapshotPrepareError::Open(error) => {
            LoadFailure::new(LoadFailureReason::OperationError, error.to_string())
        }
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
    pub(super) fn run_fused_cold_build(
        &self,
        engine: &mut SearchEngine,
        source_path: &Path,
        build_start_seq: i64,
    ) -> Result<(), LoadFailure> {
        let Some(workspace_root) = self.workspace_root.clone() else {
            return Err(LoadFailure::operation("fused build on a non-workspace graph"));
        };
        let generation =
            lock_recover(&self.inner).published.as_ref().map(|p| p.generation).unwrap_or(0) + 1;

        let source_path = source_path.to_path_buf();
        let mut sink = FusedChunkWriter::new(engine, source_path, self.lease.clone());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_and_publish_graph_file(&workspace_root, generation, self, Some(&mut sink))
        }));
        let built = match outcome {
            Ok(Ok(v)) => v,
            Ok(Err(failure)) => return Err(sink.failure.take().unwrap_or(failure)),
            Err(_) => return Err(LoadFailure::operation("fused graph build panicked")),
        };
        if built.force_stale {
            tracing::warn!("fused graph build straddled a disk write; snapshot marked stale");
        }
        install_failure(self.install_prepared_snapshot(
            built.prepared,
            Published {
                generation,
                fingerprint: built.fp_pre,
                stale: false,
                reload: ReloadState::Idle,
                force_stale: built.force_stale,
                search_roots: built.search_roots.clone(),
            },
            GraphStatus::Ready { files: built.files },
            // The fused build runs at boot, ahead of any forced-reload request.
            None,
        ))?;
        *lock_recover(&self.scan) = None;
        self.ensure_hub_roots(&built.scan_roots, built.fp_pre.topology);
        // The fused sink just wrote every indexed document's context from THIS
        // build — nothing persisted predates it, so no whole-collection re-render.
        self.notify_published(build_start_seq, false);
        Ok(())
    }

    /// After a successful (re)build, re-point the daemon's change hub at the build
    /// snapshot's scan roots. A topology reload that added or dropped an extension
    /// root would otherwise leave the hub watching the old universe — events in a
    /// new extension would never be delivered, and every consumer would coast on
    /// its reconcile interval. A no-op when the roots did not change.
    pub(super) fn ensure_hub_roots(&self, scan_roots: &[std::path::PathBuf], built_topology: u64) {
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
        if !hub.ensure_roots(&crate::change_hub::watch_targets_for(root, scan_roots)) {
            tracing::warn!("graph rebuild could not re-arm the change hub onto new roots");
        }
    }

    /// Build (or rebuild) the database off-thread and publish it coherently.
    /// `is_reload` distinguishes the initial load (sets `Ready`, generation 1)
    /// from a drift-triggered reload (bumps the generation, keeps the old snapshot
    /// served on failure).
    pub(super) fn run_load(&self, is_reload: bool) {
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
        // The generation this build will carry. Only one load runs at a time (the
        // initial load, then at most one reload via the claim guard), so peeking the
        // current generation without reserving it is race-free; a failed build leaves
        // it unpublished and the next attempt reuses the same number.
        let generation =
            lock_recover(&self.inner).published.as_ref().map(|p| p.generation).unwrap_or(0) + 1;

        // Capture the mark-seq at build start (before any disk read below): the post-publish
        // refresh clears only marks at or below it — drifts this build already reflects. A
        // drift stamped after this point carries a higher seq, is left for a later build, and
        // is guaranteed one by the pending-nudge machinery (every xml mark also nudges).
        let build_start_seq = self.current_mark_seq();
        let project_reload_epoch = self.project_reload_epoch();
        let force_project_reload = project_reload_epoch
            > self.completed_project_reload_epoch.load(std::sync::atomic::Ordering::SeqCst);

        // On the initial load, reuse a cached build from a previous process run if it
        // still matches the workspace — turning a multi-minute rebuild into a stat
        // walk plus an open. A reload is skipped here: it only fires once drift has
        // been detected, so the on-disk file is known stale and must be rebuilt.
        if !force_project_reload && !is_reload {
            match self.try_publish_cached(&workspace_root, build_start_seq) {
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
            match self.try_incremental_reload(&workspace_root, generation, build_start_seq) {
                PublishAttemptOutcome::Published => return,
                PublishAttemptOutcome::FallBack => {}
                PublishAttemptOutcome::Refused(failure) => {
                    self.record_load_failure(is_reload, failure);
                    return;
                }
            }
        }

        tracing::info!(?workspace_root, is_reload, generation, "graph database build started");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_and_publish_graph_file(&workspace_root, generation, self, None)
        }));

        match outcome {
            Ok(Ok(built)) => {
                let PublishedBuild {
                    files,
                    fp_pre,
                    force_stale,
                    scan_roots,
                    search_roots,
                    prepared,
                } = built;
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
                    },
                    GraphStatus::Ready { files },
                    // The only path that runs under a forced reload. The epoch was
                    // captured before the build, so a request arriving mid-build stays
                    // outstanding and claims its own follow-up reload.
                    Some(project_reload_epoch),
                )) {
                    self.record_load_failure(is_reload, error);
                    return;
                }
                #[cfg(test)]
                if let Some(hook) = &self.publish_window_hook {
                    hook();
                }
                self.ensure_hub_roots(&scan_roots, fp_pre.topology);
                self.notify_published(build_start_seq, topology_changed);
                tracing::info!(files, generation, is_reload, "graph database build complete");
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
    fn try_incremental_reload(
        &self,
        workspace_root: &Path,
        generation: u64,
        build_start_seq: i64,
    ) -> PublishAttemptOutcome {
        let db_path = self.graph_db_path().expect("workspace graph has cache layout");
        let stored_fp = read_stored_fingerprints(&db_path);
        if stored_fp.is_empty() {
            return PublishAttemptOutcome::FallBack; // older build → full rebuild
        }
        // ONE project snapshot and ONE scanned universe serve the eligibility diff,
        // the profile recompute, the pre-fingerprint and the patch, so neither a
        // config edit nor a file landing mid-operation can hand two passes two
        // different trees. Only the straddle check walks again.
        let project =
            crate::graph::ProjectSnapshot::load_excluding(workspace_root, &self.cache_exclusions());
        // A topology change re-shapes visibility for ANY module even when only
        // `.bsl` bodies drifted on disk — never body-patch across it.
        match GraphDb::open(&db_path).and_then(|g| g.freshness_token()) {
            Ok((_, stored_token, _))
                if stored_token.topology == super::scan::topology_u64(&project.configs) => {}
            _ => return PublishAttemptOutcome::FallBack,
        }
        let pre = crate::graph::universe::ScannedUniverse::scan_excluding(
            &project.scan_roots,
            &project.excluded,
        );
        // Before the diff, not inside the bracket: a diff against a short scan reads
        // hidden files as removals, and an unreadable EMPTY subtree does not move the
        // stats at all — the diff cannot see incompleteness, only the verdict can.
        if !pre.clean() {
            tracing::info!("incremental reload: incomplete workspace scan; full rebuild");
            return PublishAttemptOutcome::FallBack;
        }
        let diff = classify_changes(&stored_fp, &pre.stats);

        // Body-only shape: at least one `.bsl` modified, nothing added/removed, no
        // metadata drift (an `.xml` change can flip visibility for any module).
        if diff.is_empty()
            || !diff.added.is_empty()
            || !diff.removed.is_empty()
            || diff.touches_metadata()
        {
            return PublishAttemptOutcome::FallBack;
        }
        let modified_paths: Vec<PathBuf> = diff.modified.iter().map(PathBuf::from).collect();

        // Recompute each modified module's profile and partition into body-only
        // (signature unchanged) and signature-changed.
        let profiles =
            match crate::graph_db::recompute_module_profiles(&project, &pre.files, &modified_paths)
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("incremental reload: profile recompute failed: {e}");
                    return PublishAttemptOutcome::FallBack;
                }
            };
        let stored_sig = read_stored_sig_hashes(&db_path);
        let mut sig_changed: Vec<(String, &crate::graph_db::ModuleProfile)> = Vec::new();
        for p in &modified_paths {
            let key = p.to_string_lossy().into_owned();
            let Some(profile) = profiles.get(&key) else {
                return PublishAttemptOutcome::FallBack;
            };
            match stored_sig.get(&key) {
                Some(Some(stored)) if *stored == profile.sig_hash => {} // body-only
                Some(Some(_)) => sig_changed.push((key, profile)),      // signature changed
                _ => return PublishAttemptOutcome::FallBack,
            }
        }

        // A signature change is handled by the caller-delta path: reproject the changed
        // module PLUS its resolved callers, when caller-delta-safe (no new resolvable
        // name). Otherwise fall back to a full rebuild.
        let mut changed_paths = modified_paths.clone();
        if !sig_changed.is_empty() {
            let refs: Vec<(&str, &crate::graph_db::ModuleProfile)> =
                sig_changed.iter().map(|(f, p)| (f.as_str(), *p)).collect();
            match crate::graph_db::caller_delta_plan(&db_path, &refs) {
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
                    return PublishAttemptOutcome::FallBack;
                }
                Err(e) => {
                    tracing::warn!("incremental reload: caller-delta plan failed: {e}");
                    return PublishAttemptOutcome::FallBack;
                }
            }
            // If the caller fan-out approaches the whole config, a full rebuild (no
            // 2.6 GB copy) is cheaper than reprojecting most modules. Compare against
            // the `.bsl` module count only — `changed_paths` are modules, while
            // `stored_fp` also counts `.xml`, which would skew the threshold.
            let module_total = bsl_module_total(&stored_fp);
            if changed_paths.len() * 2 > module_total {
                tracing::info!(
                    changed = changed_paths.len(),
                    modules = module_total,
                    "incremental reload: caller-delta too broad; full rebuild"
                );
                return PublishAttemptOutcome::FallBack;
            }
        }

        // Bracket the patch with the shared pre-scan and a fresh post-scan,
        // mirroring the full build's straddle detection: a write landing after the
        // pre-scan marks the snapshot stale.
        let fp_pre = super::scan::fingerprint_of(&pre.stats, &project.configs);
        let tmp_path = db_path.with_extension(format!("db.building.{}", std::process::id()));
        let built_at = chrono::Utc::now().to_rfc3339();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let summary = crate::graph_db::update_graph_database_bodies(
                &project,
                &pre,
                &db_path,
                &tmp_path,
                &changed_paths,
                GRAPH_BUILD_BATCH,
                &crate::graph_db::GraphMeta {
                    revision: generation,
                    fingerprint: fp_pre,
                    files: 0,
                    built_at,
                },
            )
            .map_err(LoadFailure::operation)?;
            let post_project = crate::graph::ProjectSnapshot::load_excluding(
                workspace_root,
                &self.cache_exclusions(),
            );
            let post = crate::graph::universe::ScannedUniverse::scan_excluding(
                &post_project.scan_roots,
                &post_project.excluded,
            );
            let fp_post = super::scan::fingerprint_of(&post.stats, &post_project.configs);
            // `pre.clean()` is guaranteed above; it stays in the formula so the two
            // decisions cannot drift apart if the gate ever moves.
            let force_stale = publish_force_stale(fp_pre, fp_post, pre.clean(), post.clean());
            {
                let conn = rusqlite::Connection::open(&tmp_path).map_err(LoadFailure::operation)?;
                conn.execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES ('force_stale', ?1)",
                    rusqlite::params![if force_stale { "1" } else { "0" }],
                )
                .map_err(LoadFailure::operation)?;
            }
            self.publish_or_discard(&tmp_path, &db_path)?;
            let prepared = self
                .prepare_snapshot_pool(generation, fp_pre, force_stale)
                .map_err(prepare_failure)?;
            Ok::<_, LoadFailure>((summary.modules, fp_pre, force_stale, prepared))
        }));

        match outcome {
            Ok(Ok((files, fp, force_stale, prepared))) => {
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
                    },
                    GraphStatus::Ready { files },
                    // An incremental reload never runs under a forced reload.
                    None,
                )) {
                    return match error.reason {
                        LoadFailureReason::TransientRefusal
                        | LoadFailureReason::Superseded
                        | LoadFailureReason::Released => PublishAttemptOutcome::Refused(error),
                        LoadFailureReason::OperationError => PublishAttemptOutcome::FallBack,
                    };
                }
                // The body-only gate proved the stored topology unchanged.
                self.notify_published(build_start_seq, false);
                tracing::info!(
                    files,
                    generation,
                    modified = changed_paths.len(),
                    "graph incremental reload complete"
                );
                PublishAttemptOutcome::Published
            }
            Ok(Err(e)) => {
                tracing::warn!("incremental reload failed, falling back to full rebuild: {e}");
                let _ = std::fs::remove_file(&tmp_path);
                match e.reason {
                    LoadFailureReason::TransientRefusal
                    | LoadFailureReason::Superseded
                    | LoadFailureReason::Released => PublishAttemptOutcome::Refused(e),
                    LoadFailureReason::OperationError => PublishAttemptOutcome::FallBack,
                }
            }
            Err(_) => {
                tracing::error!("incremental reload panicked, falling back to full rebuild");
                let _ = std::fs::remove_file(&tmp_path);
                PublishAttemptOutcome::FallBack
            }
        }
    }

    /// Publish an existing on-disk build instead of rebuilding, when it is still a
    /// valid, current, non-straddled match for the workspace.
    pub(super) fn try_publish_cached(
        &self,
        workspace_root: &Path,
        build_start_seq: i64,
    ) -> PublishAttemptOutcome {
        if self.is_superseded() {
            return PublishAttemptOutcome::Refused(LoadFailure::new(
                LoadFailureReason::Superseded,
                super::types::SUPERSEDED_GRAPH_ERROR,
            ));
        }
        let path = self.graph_db_path().expect("workspace graph has cache layout");
        let Ok(graph) = GraphDb::open(&path) else {
            return PublishAttemptOutcome::FallBack; // missing, truncated, or stale-schema → rebuild
        };
        let Ok((revision, fingerprint, force_stale)) = graph.freshness_token() else {
            return PublishAttemptOutcome::FallBack;
        };
        let project =
            crate::graph::ProjectSnapshot::load_excluding(workspace_root, &self.cache_exclusions());
        let now = crate::graph::universe::ScannedUniverse::scan_excluding(
            &project.scan_roots,
            &project.excluded,
        );
        let fp_now = super::scan::fingerprint_of(&now.stats, &project.configs);
        if !cache_is_reusable(force_stale, fingerprint, fp_now, now.clean()) {
            return PublishAttemptOutcome::FallBack;
        }
        let files = graph.files().unwrap_or(0);
        drop(graph);
        let prepared = match self.prepare_snapshot_pool(revision, fingerprint, force_stale) {
            Ok(prepared) => prepared,
            Err(SnapshotPrepareError::Open(_)) => return PublishAttemptOutcome::FallBack,
            Err(error @ SnapshotPrepareError::Changed) => {
                let error = prepare_failure(error);
                return PublishAttemptOutcome::Refused(error);
            }
        };

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
            },
            GraphStatus::Ready { files },
            // Serving a cached build discharges no forced reload: it publishes the
            // state already on disk, not a rebuild of the newly declared configuration.
            None,
        )) {
            return PublishAttemptOutcome::Refused(error);
        }
        // Exact fingerprint match (files AND topology): the persisted search
        // contexts were rendered against this same workspace state.
        self.notify_published(build_start_seq, false);
        tracing::info!(files, revision, "reused cached graph database (workspace unchanged)");
        PublishAttemptOutcome::Published
    }

    /// Boot variant for a cached graph that no longer matches disk: publish it anyway —
    /// stale answers now beat "still indexing" for the minutes a full rebuild takes —
    /// and pre-claim the reload slot in the SAME lock hold, then let the normal reload
    /// lifecycle catch up (incrementally when eligible, else a full rebuild). The
    /// atomic Ready+Running publish keeps every existing guard honest:
    /// `freshness()`/`claim_reload_slot` stay single-flight against the pre-claimed
    /// slot, and `consume_leftover_marks` sees `drift_pending` and defers the leftover
    /// consume to the catch-up publish — unlike a fingerprint-clean cached publish,
    /// THIS snapshot does not reflect the leftover marks' causes. A snapshot from a
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
        let path = self.graph_db_path().expect("workspace graph has cache layout");
        let Ok(graph) = GraphDb::open(&path) else {
            return PublishAttemptOutcome::FallBack; // missing, truncated, or stale-schema → full rebuild
        };
        let Ok((revision, fingerprint, force_stale)) = graph.freshness_token() else {
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
        if !super::scan::graph_file_matches_live_topology(workspace_root, &graph) {
            tracing::info!(
                "cached graph database was built for another extension topology; \
                 rebuilding instead of serving it stale, and re-rendering search contexts"
            );
            self.pending_topology_refresh.store(true, std::sync::atomic::Ordering::SeqCst);
            return PublishAttemptOutcome::FallBack;
        }
        let files = graph.files().unwrap_or(0);
        drop(graph);
        let prepared = match self.prepare_snapshot_pool(revision, fingerprint, force_stale) {
            Ok(prepared) => prepared,
            Err(SnapshotPrepareError::Open(_)) => return PublishAttemptOutcome::FallBack,
            Err(error @ SnapshotPrepareError::Changed) => {
                let error = prepare_failure(error);
                return PublishAttemptOutcome::Refused(error);
            }
        };

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
            },
            GraphStatus::Ready { files },
            // A placeholder publication; the catch-up build it spawns carries whatever
            // obligation is outstanding.
            None,
        )) {
            return PublishAttemptOutcome::Refused(error);
        }
        tracing::info!(
            files,
            revision,
            "published stale cached graph database; catch-up reload starting"
        );
        self.spawn_reload();
        PublishAttemptOutcome::Published
    }

    /// Move a finished build into the shared path — unless this daemon lost the workspace
    /// while it was building. See [`publish_or_discard`].
    fn publish_or_discard(&self, tmp_path: &Path, out_path: &Path) -> Result<(), LoadFailure> {
        publish_or_discard(self, tmp_path, out_path)
    }

    /// A failed initial load surfaces as `Failed`; a failed reload keeps the
    /// previous snapshot but flags `reload="failed"` so the agent sees it. A
    /// later drift check retries the reload (the throttle bounds the retry rate).
    ///
    /// A transient ownership refusal is flagged for retry. Terminal supersession and genuine
    /// build failures stay terminal.
    pub(super) fn record_load_failure(&self, is_reload: bool, failure: LoadFailure) {
        if failure.reason == LoadFailureReason::TransientRefusal {
            let mut retry = lock_recover(&self.graph_retry);
            let window = retry.get_or_insert_with(|| {
                crate::state::retry_window::RetryWindow::new(
                    crate::state::retry_window::RetryOwner::Graph,
                )
            });
            let _ = window.refused(std::time::Instant::now(), std::time::Duration::ZERO);
        } else {
            *lock_recover(&self.graph_retry) = None;
        }
        let mut inner = lock_recover(&self.inner);
        if is_reload {
            if let Some(p) = inner.published.as_mut() {
                p.reload = ReloadState::Failed(failure.message);
            }
        } else {
            inner.status = GraphStatus::Failed(failure.message);
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
    let pre = crate::graph::universe::ScannedUniverse::scan_excluding(
        &project.scan_roots,
        &project.excluded,
    );
    build_and_publish_scanned(workspace_root, &project, &pre, generation, graph, chunk_sink)
}

/// The publication over an ALREADY-SCANNED universe — split from
/// [`build_and_publish_graph_file`] so a test can mutate the tree between the
/// pre-scan and the build and observe that the build does not see the mutation.
fn build_and_publish_scanned(
    workspace_root: &Path,
    project: &crate::graph::ProjectSnapshot,
    pre: &crate::graph::universe::ScannedUniverse,
    generation: u64,
    graph: &GraphState,
    chunk_sink: Option<&mut dyn ide::FusedChunkSink>,
) -> Result<PublishedBuild, LoadFailure> {
    let fp_pre = super::scan::fingerprint_of(&pre.stats, &project.configs);
    let out_path = graph.graph_db_path().expect("workspace graph has cache layout");
    // Pid-suffixed temp: two daemons over the same workspace (an old topology
    // generation draining while a new one starts) must not interleave writes into
    // one temp file — each builds its own and the atomic rename decides.
    let tmp_path = out_path.with_extension(format!("db.building.{}", std::process::id()));
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(LoadFailure::operation)?;
    }
    let built_at = chrono::Utc::now().to_rfc3339();
    let meta = crate::graph_db::GraphMeta {
        revision: generation,
        fingerprint: fp_pre,
        files: 0,
        built_at,
    };
    let summary = match match chunk_sink {
        Some(sink) => crate::graph_db::build_graph_database_fused(
            project,
            pre,
            &tmp_path,
            GRAPH_BUILD_BATCH,
            &meta,
            sink,
        ),
        None => {
            crate::graph_db::build_graph_database(project, pre, &tmp_path, GRAPH_BUILD_BATCH, &meta)
        }
    } {
        Ok(summary) => summary,
        Err(error) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(LoadFailure::operation(error));
        }
    };
    // The post-scan derives a FRESH project snapshot AND a fresh walk: the straddle
    // check must see the world as it is now, or a topology/root change landing
    // mid-build would compare the frozen snapshot against itself and publish clean.
    let post_project =
        crate::graph::ProjectSnapshot::load_excluding(workspace_root, &graph.cache_exclusions());
    let post = crate::graph::universe::ScannedUniverse::scan_excluding(
        &post_project.scan_roots,
        &post_project.excluded,
    );
    let fp_post = super::scan::fingerprint_of(&post.stats, &post_project.configs);
    let force_stale = publish_force_stale(fp_pre, fp_post, pre.clean(), post.clean());
    {
        let conn = rusqlite::Connection::open(&tmp_path).map_err(LoadFailure::operation)?;
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
    publish_or_discard(graph, &tmp_path, &out_path)?;
    let prepared =
        graph.prepare_snapshot_pool(generation, fp_pre, force_stale).map_err(prepare_failure)?;
    Ok(PublishedBuild {
        files: summary.modules,
        fp_pre,
        force_stale,
        scan_roots: project.scan_roots.clone(),
        search_roots: project.search_roots.clone(),
        prepared,
    })
}

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

/// Rename a finished build into the shared path, or throw it away.
///
/// A build takes minutes, and a newer daemon generation can claim the workspace's derived
/// caches at any point during one (see [`crate::workspace_lease`]). The rename runs with
/// ownership HELD rather than merely checked: a claim landing between a check and the rename
/// would let this build clobber what the new owner just published, and "we owned it a moment
/// ago" is exactly the guarantee a minutes-long build cannot rely on. A rename that cannot go
/// ahead discards the build, temp file and all, so nothing is left behind.
///
/// The caller decides whether a classified refusal is retryable. This function owns only the
/// fenced rename and cleanup; it never mutates the load lifecycle as a side effect.
fn publish_or_discard(
    graph: &GraphState,
    tmp_path: &Path,
    out_path: &Path,
) -> Result<(), LoadFailure> {
    match graph.lease.publish_short(&mut (), |_| std::fs::rename(tmp_path, out_path)) {
        LeaseOperationOutcome::Applied(()) => Ok(()),
        LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(error))
        | LeaseOperationOutcome::OperationError(LeaseOperationError::Lease(error)) => {
            let _ = std::fs::remove_file(tmp_path);
            Err(LoadFailure::operation(error))
        }
        LeaseOperationOutcome::TransientRefusal => {
            let _ = std::fs::remove_file(tmp_path);
            Err(LoadFailure::new(
                LoadFailureReason::TransientRefusal,
                "this daemon could not establish ownership of the workspace's derived caches \
                 when the graph build finished; the build was discarded instead of published",
            ))
        }
        LeaseOperationOutcome::Superseded => {
            let _ = std::fs::remove_file(tmp_path);
            Err(LoadFailure::new(
                LoadFailureReason::Superseded,
                "workspace cache ownership was superseded before the graph build could be published",
            ))
        }
        LeaseOperationOutcome::Released => {
            let _ = std::fs::remove_file(tmp_path);
            Err(LoadFailure::new(
                LoadFailureReason::Released,
                "workspace cache ownership was released before the graph build could be published",
            ))
        }
    }
}

/// The outcome of one full build+publish pass: what was published, the identity it
/// was published under, and the scan roots of the snapshot that built it (for the
/// post-publish hub re-arm).
struct PublishedBuild {
    files: usize,
    fp_pre: crate::graph_db::GraphFp,
    force_stale: bool,
    scan_roots: Vec<PathBuf>,
    search_roots: Option<bsl_search::WorkspaceRoots>,
    prepared: PreparedSnapshotPool,
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
    /// Canonical, `/`-normalised search source root. Only used when no root table is
    /// configured — then the corpus is the configuration alone, as it always was.
    source_prefix: String,
    failure: Option<LoadFailure>,
}

impl<'e> FusedChunkWriter<'e> {
    fn new(
        engine: &'e mut SearchEngine,
        source_path: PathBuf,
        lease: crate::workspace_lease::WorkspaceLease,
    ) -> Self {
        let roots = engine.workspace_roots().cloned();
        let source_prefix =
            source_path.canonicalize().unwrap_or(source_path).to_string_lossy().replace('\\', "/");
        Self { engine, lease, roots, source_prefix, failure: None }
    }

    /// The store key of one emitted module, or `None` when it belongs to no registered root.
    fn key_of(&self, abs: &str) -> Option<bsl_search::FileKey> {
        let Some(roots) = self.roots.as_ref() else {
            let prefix = self.source_prefix.trim_end_matches('/');
            let rel = abs
                .strip_prefix(prefix)
                .filter(|rest| rest.starts_with('/'))
                .map(|s| s.trim_start_matches('/'))?;
            return (!rel.is_empty()).then(|| bsl_search::FileKey::configuration(rel));
        };
        let walked = std::path::Path::new(abs);
        let canonical = walked.canonicalize().ok()?;
        roots.root_of(walked, &canonical)
    }
}

impl ide::FusedChunkSink for FusedChunkWriter<'_> {
    fn emit_chunks(
        &mut self,
        rows: &[ide::ChunkRow],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            // A module outside every registered root is not this index's business. With a table
            // configured that means "under no declared root"; without one it means "outside the
            // configuration", which is the prefix check this used to be — a separator boundary
            // included, so `…/cf_ext` is never mistaken for a file inside `…/cf`.
            let Some(key) = self.key_of(abs) else {
                continue;
            };
            let bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(_) => continue, // unreadable now → leave for the standalone indexer
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
            if self.engine.store().file_hash(&key.root_id, &key.path).ok().flatten().as_deref()
                == Some(hash.as_slice())
            {
                continue;
            }
            match self.lease.publish_checkpointed(|checkpoint| {
                self.engine.ingest_fused_file_checkpointed(&key, &hash, chunks, ctxs, checkpoint)
            }) {
                LeaseOperationOutcome::Applied(()) => {}
                LeaseOperationOutcome::OperationError(LeaseOperationError::Operation(error)) => {
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
    }
}

/// Read the stored per-file fingerprints from a built graph's `files` table. Any
/// open/query failure (missing file, older schema without the table) yields an empty
/// map, which classifies every current file as `added` → conservative full rebuild.
/// The `.bsl` module count of a stored fingerprint map — the denominator of the
/// incremental-reload breadth threshold (`.xml` rows would skew it).
fn bsl_module_total(stored_fp: &std::collections::HashMap<String, u64>) -> usize {
    stored_fp
        .keys()
        .filter(|p| bsl_conventions::str_has_extension(p, bsl_conventions::BSL_EXTENSION))
        .count()
}

pub(crate) fn read_stored_fingerprints(db_path: &Path) -> std::collections::HashMap<String, u64> {
    let mut map = std::collections::HashMap::new();
    // Read-only open: never create the file as a side effect. A missing/older DB
    // errors here and yields an empty map → every current file classified `added`.
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return map;
    };
    let Ok(mut stmt) = conn.prepare("SELECT path, fingerprint FROM files") else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))
    else {
        return map;
    };
    for row in rows.flatten() {
        map.insert(row.0, row.1);
    }
    map
}

/// Read the stored per-file signature hashes (`None` for `.xml`, and for `.bsl` built
/// before signature persistence). Read-only open; an open/query failure yields an
/// empty map → the body-only fast path treats every module as ineligible (full
/// rebuild). Separate from [`read_stored_fingerprints`] so the eligibility check can
/// distinguish "no stored signature" (NULL) from "signature present but differs".
pub(crate) fn read_stored_sig_hashes(
    db_path: &Path,
) -> std::collections::HashMap<String, Option<u64>> {
    let mut map = std::collections::HashMap::new();
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return map;
    };
    let Ok(mut stmt) = conn.prepare("SELECT path, sig_hash FROM files") else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?.map(|v| v as u64)))
    }) else {
        return map;
    };
    for row in rows.flatten() {
        map.insert(row.0, row.1);
    }
    map
}

#[cfg(test)]
mod module_total_tests {
    use super::bsl_module_total;

    #[test]
    fn the_incremental_threshold_counts_case_variant_modules() {
        let mut stored = std::collections::HashMap::new();
        stored.insert("cfg/CommonModules/A/Ext/Module.bsl".to_string(), 1u64);
        stored.insert("cfg/CommonModules/B/Ext/Module.BSL".to_string(), 2u64);
        stored.insert("cfg/CommonModules/B.xml".to_string(), 3u64);
        assert_eq!(
            bsl_module_total(&stored),
            2,
            "Module.BSL — модуль и участвует в знаменателе порога"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::input::{enumerate_bsl_files, load_workspace_db, scan_roots};
    use super::super::scan::{scan_file_stats, scan_stats_over_roots, FileStat, WorkspaceDiff};
    use super::super::snapshot::fold_fingerprint_entries;
    use super::super::test_support::{
        meta_string, sample_workspace, seed_cache, wait_ready, wait_until, wait_until_within,
        write, write_common_module, write_extension_config, write_extension_workspace,
    };
    use super::*;
    use crate::graph_db::{build_graph_database, update_graph_database_bodies};
    use ide::Analysis;
    use rusqlite::Connection;
    use std::collections::HashSet;
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};
    use walkdir::WalkDir;

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
        let tmp_path = graph_path.with_extension(format!("db.building.{}", std::process::id()));
        assert!(!graph_path.exists(), "the rejected build never replaces the canonical graph");
        assert!(!tmp_path.exists(), "the rejected build removes only its current temp graph");

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
        let temp = canonical.with_extension(format!("db.building.{}", std::process::id()));

        fs::write(&canonical, b"new-owner-graph").unwrap();
        fs::write(&temp, b"old-daemon-graph").unwrap();
        let newer = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        assert!(!old.owns_caches_now());
        assert!(old.is_superseded());
        newer.release();

        publish_or_discard(&graph, &temp, &canonical).unwrap_err();
        assert_eq!(fs::read(&canonical).unwrap(), b"new-owner-graph");
        assert!(!temp.exists(), "normal refusal removes only this build's temp file");

        let mut sink = RefusingSink;
        assert!(build_and_publish_graph_file(root, 1, &graph, Some(&mut sink)).is_err());
        assert_eq!(fs::read(&canonical).unwrap(), b"new-owner-graph");
        assert!(!temp.exists(), "fused failure before publication removes its temp file");
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
        let error = publish_or_discard(&graph, &temp, &canonical).unwrap_err();
        drop(held);

        assert_eq!(error.reason, LoadFailureReason::TransientRefusal);
        graph.record_load_failure(false, error);
        assert!(lock_recover(&graph.graph_retry).is_some());
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
        assert!(lock_recover(&graph.graph_retry).is_none());
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
        )
        .unwrap_err();
        assert_eq!(released_error.reason, LoadFailureReason::Released);
        released_graph.record_load_failure(false, released_error);
        assert!(lock_recover(&released_graph.graph_retry).is_none());

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
        )
        .unwrap_err();
        assert_eq!(superseded_error.reason, LoadFailureReason::Superseded);
        superseded_graph.record_load_failure(false, superseded_error);
        assert!(lock_recover(&superseded_graph.graph_retry).is_none());
        newer.release();
    }

    #[test]
    fn operation_error_is_not_reclassified_by_later_probe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
        cache.ensure().unwrap();
        let lease = crate::workspace_lease::WorkspaceLease::claim_cache(&cache);
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone())
            .with_lease(lease.clone());
        let missing = cache.root().join("missing.building");
        let error = publish_or_discard(&graph, &missing, &cache.root().join("output.db"))
            .expect_err("an admitted rename of a missing file is a real operation error");
        assert_eq!(error.reason, LoadFailureReason::OperationError);

        let held = lease.hold_file_lock_for_test();
        graph.record_load_failure(false, error);
        drop(held);

        assert!(lock_recover(&graph.graph_retry).is_none());
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

        // The same workspace, plus a module dropped inside the analyzer's own cache.
        crate::graph::test_support::write_common_module(
            &cache.root().join("vendor"),
            "Чужой",
            true,
            "&НаСервере\nФункция Чуж() Экспорт КонецФункции",
        );
        let with_vendored = build_and_publish_graph_file(root, 2, &graph, None).unwrap();

        assert_eq!(
            with_vendored.files, clean.files,
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
            },
            GraphStatus::Ready { files: built.files },
            None,
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
        assert!(lock_recover(&clean.graph_retry).is_none());
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
        assert!(lock_recover(&stale.graph_retry).is_none());
        assert!(stale.snapshot().is_none(), "stale adoption never becomes ready");
    }

    #[cfg(not(windows))]
    #[test]
    fn incremental_publication_propagates_final_install_refusal() {
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
            PublishAttemptOutcome::FallBack
        ));
        assert_eq!(
            graph.snapshot().map(|snapshot| snapshot.generation),
            Some(1),
            "failed reload keeps serving its old pool"
        );
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
        assert!(lock_recover(&graph.graph_retry).is_none());
        assert!(graph.snapshot().is_some(), "the invalid cache fell back to a real build");
    }

    #[test]
    fn full_reload_install_failure_keeps_the_old_snapshot_without_transient_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        graph.ensure_loading();
        wait_ready(&graph);
        write(root, "CommonModules/Сервер.xml", "<MetaDataObject><changed/></MetaDataObject>");
        super::super::snapshot::refuse_snapshot_install_for_test();

        graph.run_load(true);

        assert_eq!(graph.snapshot().map(|snapshot| snapshot.generation), Some(1));
        assert!(lock_recover(&graph.graph_retry).is_none());
        assert!(matches!(
            lock_recover(&graph.inner).published.as_ref().unwrap().reload,
            ReloadState::Failed(_)
        ));
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
        update_graph_database_bodies(&project, &universe, src, out, changed, batch_size, meta)
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
        build_graph_database(&project, &universe, out, batch_size, meta)
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
        let fp_pre = crate::graph::scan::fingerprint_of(&pre.stats, &project.configs);

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
        let fp_post = crate::graph::scan::fingerprint_of(&post.stats, &project.configs);
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

        let project = crate::graph::ProjectSnapshot::load(root);
        let pre = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        fs::write(root.join("bsl-analyzer.toml"), "[source]\nroot = \"alias-b\"\n").unwrap();

        let graph = GraphState::for_workspace(root.to_path_buf());
        let built = build_and_publish_scanned(root, &project, &pre, 1, &graph, None).unwrap();
        let published = built.search_roots.as_ref().unwrap();
        let live = crate::graph::ProjectSnapshot::load(root).search_roots.unwrap();

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
        let project = crate::graph::ProjectSnapshot::load(root);
        let pre = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        build_and_publish_scanned(root, &project, &pre, 1, &graph, None)
            .expect("the publication succeeds");
        let walks = project_model::source_set::scans_performed_on_thread() - before;

        assert!(walks > 0, "a zero count means the instrumentation broke, not that no walk ran");
        assert_eq!(walks, 2, "pre-scan + straddle post-scan, nothing else");
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

        let PublishAttemptOutcome::Refused(failure) = outcome else {
            panic!("an eligible incremental publication must retain the refusal")
        };
        assert_eq!(failure.reason, LoadFailureReason::TransientRefusal);
        graph.record_load_failure(true, failure);
        assert!(lock_recover(&graph.graph_retry).is_some());
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
        let project = crate::graph::ProjectSnapshot::load(root);
        let mut entries: Vec<(String, u128, u64)> = scan_stats_over_roots(&project.scan_roots)
            .0
            .into_iter()
            .map(|s| (s.path, s.mtime, s.len))
            .collect();
        entries.sort();
        let map: std::collections::BTreeMap<String, (u128, u64)> =
            entries.into_iter().map(|(p, m, l)| (p, (m, l))).collect();
        let via_map: Vec<(String, u128, u64)> =
            map.iter().map(|(p, (m, l))| (p.clone(), *m, *l)).collect();
        assert_eq!(fold_fingerprint_entries(&via_map), walk.files, "map fold == walk fold");
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
        };
        let project = crate::graph::ProjectSnapshot::load(root);
        let universe = crate::graph::universe::ScannedUniverse::scan(&project.scan_roots);
        crate::graph_db::build_graph_database(&project, &universe, &out, 1, &meta)
            .expect("graph database builds");

        // The walk verdict cannot see this: `stat` needs no read permission.
        assert!(universe.clean(), "the tree walk is clean, which is exactly the trap");

        {
            // No intermediate write: the BUILDER records the key, so an artefact is
            // never at the current schema version while silently claiming zero holes.
            let conn = Connection::open(&out).unwrap();
            let stored = crate::graph_db::read_unread_paths(&conn);
            assert!(
                stored.iter().any(|p| p.ends_with("Слепой/Ext/Module.bsl")),
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
        crate::graph_db::build_graph_database(&project, &universe, &out2, 1, &meta)
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
        let root = dir.path();
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
        };
        let src = root.join(".build/bsl-graph.db");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        build_whole_graph(root, &src, 1, &meta).expect("the whole graph builds");
        let rows_before = node_rows_for(&src, &bystander);
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
            node_rows_for(&out, &bystander),
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
        assert_eq!(node_rows_for(&dark, &bystander), 0, "its rows went with the patch");
        assert!(
            crate::graph_db::read_unread_paths(&conn)
                .iter()
                .any(|p| p.ends_with("Сосед/Ext/Module.bsl")),
            "and a module the patch could not lower IS recorded"
        );
        drop(conn);

        // And the record is released by the pass that restores the rows, not before.
        fs::write(&bystander, "&НаСервере\nПроцедура Снова() Экспорт КонецПроцедуры").unwrap();
        let healed = root.join(".build/bsl-graph-healed.db");
        update_bodies_for_test(root, &dark, &healed, std::slice::from_ref(&bystander), 1, &meta)
            .expect("the patch applies over the restored module");
        let conn = Connection::open(&healed).unwrap();
        assert!(node_rows_for(&healed, &bystander) > 0, "the rows are back");
        assert!(
            crate::graph_db::read_unread_paths(&conn).is_empty(),
            "so the record goes with them"
        );
    }

    /// Node rows the artefact holds for one module. `nodes.file` is the absolute path
    /// with separators normalised, the spelling the encoder writes.
    fn node_rows_for(db: &Path, module: &Path) -> i64 {
        let key = module.to_string_lossy().replace('\\', "/");
        let conn = Connection::open(db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM nodes WHERE file = ?1", [key], |r| r.get(0)).unwrap()
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
        let overview = analysis.graph_overview(GRAPH_SOURCE_ROOT, Some(root), 10);

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
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");

        let id = "method/common/Сервер/Считать";
        // Tiny budget (1 token ≈ 4 chars) truncates the body and flags exhaustion.
        let (tight, tight_completeness) =
            crate::tools::graph::node(&gdb, id, ide::GraphDetail::Bodies, 1, None);
        assert_eq!(tight["budget_exhausted"], serde_json::json!(true));
        assert!(tight["node"]["source"].as_str().unwrap().len() <= 4, "{tight:?}");
        // The same fact reaches the envelope as a machine reason, not only as the flag.
        assert_eq!(tight_completeness.to_value()["reasons"][0]["code"], "output_budget");
        // A generous budget keeps the whole body and sets no exhaustion flag.
        let (loose, loose_completeness) =
            crate::tools::graph::node(&gdb, id, ide::GraphDetail::Bodies, 10_000, None);
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
        let fold = analysis.graph_overview(GRAPH_SOURCE_ROOT, Some(root), 50);
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
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens and validates");

        let id = "method/common/Сервер/Считать";

        let mem_overview =
            serde_json::to_value(analysis.graph_overview(GRAPH_SOURCE_ROOT, Some(root), 10))
                .unwrap();
        let sql_overview = serde_json::to_value(gdb.overview(10, None).unwrap()).unwrap();
        assert_eq!(mem_overview, sql_overview, "overview JSON");

        let mem_node = serde_json::to_value(
            analysis
                .graph_node(GRAPH_SOURCE_ROOT, Some(root), id, ide::GraphDetail::Bodies)
                .unwrap(),
        )
        .unwrap();
        let sql_node =
            serde_json::to_value(gdb.node(id, ide::GraphDetail::Bodies, None).unwrap().unwrap())
                .unwrap();
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
            analysis.graph_neighbors(GRAPH_SOURCE_ROOT, Some(root), &params).unwrap(),
        )
        .unwrap();
        let sql_nb = serde_json::to_value(gdb.neighbors(&params, None).unwrap().unwrap()).unwrap();
        assert_eq!(mem_nb, sql_nb, "neighbors JSON");

        // Asking for places must not split the two projections either. Neither holds a root
        // table here, so both must answer `roots_unavailable` for the same edges — and a
        // projection that grew the fields on one side only would diverge right here.
        let with_places = ide::NeighborsParams { call_sites: true, max_call_sites: 20, ..params };
        let mem_sites = serde_json::to_value(
            analysis.graph_neighbors(GRAPH_SOURCE_ROOT, Some(root), &with_places).unwrap(),
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
        let mem_src =
            serde_json::to_value(analysis.graph_source(GRAPH_SOURCE_ROOT, Some(root), &ids, 4000))
                .unwrap();
        let sql_src = serde_json::to_value(gdb.source(&ids, 4000).unwrap()).unwrap();
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
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");

        // The calling method carries its signature, its call, and its metadata read.
        let ctx = gdb
            .graph_context("method/common/Вызыватель/Делать")
            .unwrap()
            .expect("method has graph context");
        assert!(ctx.starts_with("Dispatch: "), "{ctx}");
        assert!(ctx.contains("\nSignature: Процедура Делать() Экспорт\n"), "{ctx}");
        assert!(ctx.contains("\nCalls: Считать\n"), "{ctx}");
        assert!(ctx.contains("\nReads: Справочник.Контрагенты\n"), "{ctx}");

        // A leaf method keeps its signature/dispatch but lists no calls or reads.
        let leaf =
            gdb.graph_context("method/common/Сервер/Считать").unwrap().expect("leaf context");
        assert!(leaf.contains("Signature: Функция Считать() Экспорт"), "{leaf}");
        assert!(!leaf.contains("Calls:"), "{leaf}");
        assert!(!leaf.contains("Reads:"), "{leaf}");

        // Non-method ids have no graph context.
        assert_eq!(gdb.graph_context("mdo/Catalog/Контрагенты").unwrap(), None);

        // The graph-DB-backed provider resolves a chunk (path, symbol) to the same text.
        let provider = crate::graph_query::GraphDbContextProvider::new(gdb);
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
            1,
            &crate::graph_db::GraphMeta {
                revision: 1,
                fingerprint: crate::graph_db::GraphFp::default(),
                files,
                built_at: "t".to_string(),
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
                    let expected = gdb.graph_context(&id).unwrap();
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
            },
        )
        .expect("graph database builds");
        let gdb = GraphDb::open(&out).expect("graph database opens");

        // Overview parity covers node/edge tallies, provenance, and the
        // centrality ranking (whose nodes carry the canonical Mdo spelling).
        let mem_overview =
            serde_json::to_value(analysis.graph_overview(GRAPH_SOURCE_ROOT, Some(root), 10))
                .unwrap();
        let sql_overview = serde_json::to_value(gdb.overview(10, None).unwrap()).unwrap();
        assert_eq!(mem_overview, sql_overview, "overview JSON from a multi-module batch");
        // The module count is the true distinct-module population (both common modules
        // own methods), not just the module nodes that happen to be edge endpoints.
        assert_eq!(sql_overview["modules"], 2, "both common modules counted: {sql_overview}");

        // `resolve` parity: a bare method name yields the same candidates from both paths.
        let mem_resolve =
            serde_json::to_value(analysis.graph_resolve(GRAPH_SOURCE_ROOT, Some(root), "ШагБ", 10))
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
            analysis.graph_neighbors(GRAPH_SOURCE_ROOT, Some(root), &params).unwrap(),
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
        let mem = analysis.graph_neighbors(GRAPH_SOURCE_ROOT, Some(root), &params).unwrap();
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
            graph.pending_topology_refresh.load(std::sync::atomic::Ordering::SeqCst),
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
        let stored = read_stored_fingerprints(&out);
        assert_eq!(stored.len(), 4);
        let diff = classify_changes(&stored, &scan_file_stats(root));
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
        assert_eq!(xml_sigs, 0, ".xml descriptors have no signature hash");
    }

    /// The persisted signature hash is stable across a body-only edit (same method
    /// names/exports/dispatch) but changes when a signature does — the exact property
    /// the body-only fast path relies on.
    #[test]
    fn sig_hash_stable_across_body_edit_changes_on_signature_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let out = graph_db_path(root);
        fs::create_dir_all(out.parent().unwrap()).unwrap();

        let meta = || crate::graph_db::GraphMeta {
            revision: 1,
            fingerprint: crate::graph_db::GraphFp::default(),
            files: 0,
            built_at: "t".to_string(),
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

        // Body-only edit: same signature `Функция Считать() Экспорт`, new body.
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт\nА = 1; Возврат А;\nКонецФункции",
        );
        build_whole_graph(root, &out, 1, &meta()).expect("rebuilds");
        assert_eq!(server_sig(&out), base, "a body-only edit leaves the signature hash unchanged");

        // Signature edit: rename the function. The hash must move.
        write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать2() Экспорт КонецФункции",
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
            "SELECT id, kind, name, qualified, module, file, name_offset, sig_end, src_start, \
             src_end, dispatch, is_export, addressable FROM nodes ORDER BY id",
            13,
        );
        let edges = collect(
            "SELECT from_id, to_id, kind, provenance, crosses FROM edges \
             ORDER BY from_id, to_id, kind, provenance, crosses",
            5,
        );
        let in_degree = collect("SELECT id, degree FROM in_degree ORDER BY id", 2);
        let unresolved = collect(
            "SELECT target_scope, method_lower, caller_file FROM unresolved_calls \
             ORDER BY target_scope, method_lower, caller_file",
            3,
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
        write_common_module(root, "Ядро", true, "&НаСервере\nПроцедура М() Экспорт КонецПроцедуры\nПроцедура Н() Экспорт КонецПроцедуры");
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
        let callers = crate::graph_db::caller_delta_plan(&db_pre, &[(core_key.as_str(), profile)])
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
        write(root, "CommonModules/Ядро/Ext/Module.bsl", "&НаСервере\nПроцедура М() Экспорт КонецПроцедуры\nПроцедура Новый() Экспорт КонецПроцедуры");
        let core_path = root.join("CommonModules/Ядро/Ext/Module.bsl").canonicalize().unwrap();
        let core_key = core_path.to_string_lossy().into_owned();
        let profiles = recompute_profiles_for_test(root, std::slice::from_ref(&core_path)).unwrap();
        let profile = profiles.get(&core_key).unwrap();
        let callers = crate::graph_db::caller_delta_plan(&db_pre, &[(core_key.as_str(), profile)])
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
        let plan = crate::graph_db::caller_delta_plan(
            &db_pre,
            &[(key.as_str(), profiles.get(&key).unwrap())],
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

        // The plan matches the recorded unread paths against these very keys, verbatim.
        // Pin that they are one spelling: the comparison is only sound because both
        // sides come from the same scanned canonical path, and this is the assertion
        // that would notice either producer starting to normalise.
        let recorded = {
            let conn = rusqlite::Connection::open(&db_pre).unwrap();
            crate::graph_db::read_unread_paths(&conn)
        };
        assert!(
            recorded.contains(&key),
            "the artefact records the unread body under the same spelling the plan is keyed by: \
             {recorded:?} vs {key}"
        );

        let profiles = recompute_profiles_for_test(root, std::slice::from_ref(&canonical)).unwrap();
        let plan = crate::graph_db::caller_delta_plan(
            &db_pre,
            &[(key.as_str(), profiles.get(&key).unwrap())],
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
        let callers = crate::graph_db::caller_delta_plan(&db_pre, &[(core_key.as_str(), profile)])
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
            },
        )
        .expect("graph database builds");
        let stored = read_stored_fingerprints(&out);

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

        let diff = classify_changes(&stored, &scan_file_stats(root));
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

    /// End-to-end: a signature change (method removal) drifts the workspace, and the
    /// reload takes the caller-delta path — bumping the generation and serving a graph
    /// where the removed method (and its caller's edge) is gone.
    #[test]
    fn drift_with_signature_change_reloads_via_caller_delta() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(root, "Ядро", true, "&НаСервере\nФункция Цель() Экспорт КонецФункции\nФункция Прочее() Экспорт КонецФункции");
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
                stats.push(FileStat { path: path.to_string_lossy().into_owned(), mtime, len });
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
    use super::super::test_support::{sample_workspace, write};
    use super::GRAPH_BUILD_BATCH;
    use crate::graph_db::build_graph_database;
    use rusqlite::Connection;
    use std::path::Path;

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
}
