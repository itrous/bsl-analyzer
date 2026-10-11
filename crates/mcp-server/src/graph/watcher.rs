//! The graph's own drift owner.
//!
//! The graph used to learn about disk changes only from the search consumer, so a workspace
//! whose search never started — an init that failed, a baseline that never connected — kept
//! serving a graph nobody would ever rebuild. The watcher reads its own hub cursor, subscribed
//! before the graph's first pre-scan, and never waits for search: whatever search does, the
//! graph's drift has an owner in every mode.
//!
//! It is also the alarm for the graph's owed work. A rebuild that failed and is owed a retry,
//! and marks nobody has consumed yet, both wait for a moment in the future, and nothing else
//! wakes the graph when that moment comes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::change_hub::{ChangeEntry, CursorLease, SinkCursor, WatchReadiness, WorkspaceChangeHub};
use crate::state::{OwnerLive, OwnerStop, StandaloneNotice};

use super::{GraphState, GraphStatus};

/// The longest the watcher waits for the hub before it re-checks its stop and its alarms.
const WAIT_SLICE: Duration = Duration::from_secs(30);

/// How long one readiness wait lasts before the watcher comes up to check its stop.
const READINESS_SLICE: Duration = Duration::from_secs(1);

/// Disk events are coalesced after this quiet interval, with a hard cap per burst.
const DEBOUNCE_QUIET: Duration = Duration::from_millis(150);
const DEBOUNCE_MAX_WAIT: Duration = Duration::from_secs(1);

/// Where the watcher is in its life, for the freshness a graph answer reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchPhase {
    /// No watcher was ever started for this graph.
    Unwatched,
    /// Started, but the first observation is not complete: nothing vouches for disk yet.
    Starting,
    /// Every change the hub delivers to the graph's cursor reaches the graph.
    Running,
    /// The watcher left — stopped, superseded or released — and nobody watches any more.
    Stopped,
}

/// The advisory slot and the root it is derived from. Refreshed on every config change the
/// watcher sees; handed back as abandoned when the watcher leaves.
pub(crate) type AdvisorySlot = (PathBuf, Arc<Mutex<StandaloneNotice>>);

struct AdvisoryOwner(Option<AdvisorySlot>);

impl AdvisoryOwner {
    fn refresh(&self, graph: &GraphState, stop: &OwnerStop) {
        let Some((root, slot)) = self.0.as_ref() else { return };
        let project = match crate::project::at(root) {
            Ok(project) => project,
            Err(error) => {
                tracing::error!(%error, "workspace cache scope changed before advisory refresh");
                stop.stop_for_scope_change();
                return;
            }
        };
        if let Some(error) = graph.cache().and_then(|cache| cache.verify_project(&project).err()) {
            tracing::error!(%error, "workspace cache scope changed before advisory refresh");
            stop.stop_for_scope_change();
            return;
        }
        let notice = crate::state::standalone_notice_of(&project);
        *slot.lock().unwrap_or_else(|poison| poison.into_inner()) =
            StandaloneNotice::tracked(notice);
    }
}

impl Drop for AdvisoryOwner {
    fn drop(&mut self) {
        let Some((_, slot)) = self.0.as_ref() else { return };
        slot.lock().unwrap_or_else(|poison| poison.into_inner()).abandon();
    }
}

/// Start the watcher for `graph`. The cursor is subscribed HERE, synchronously, so a caller
/// that starts the watcher before anything builds the graph has the stream begin before the
/// first pre-scan reads disk. Returns whether the thread started.
pub(crate) fn start(
    graph: &GraphState,
    hub: &WorkspaceChangeHub,
    advisory: Option<AdvisorySlot>,
    stop: OwnerStop,
) -> bool {
    let mut lease = CursorLease::new(hub.clone());
    let Some(cursor) = lease.cursor() else {
        // No cursor, so no watcher, so nobody keeps the advisory current. The slot must say so
        // itself: it is a cached value, and a cached value with no updater that still reads
        // "tracked" tells the status tool about a project this daemon has stopped following.
        //
        // The spawn failure below needs no such branch — the `Watcher` it built is dropped
        // there, and `AdvisoryOwner::drop` abandons the slot. This path returns before the
        // owner exists at all, which is exactly why it was the one left open.
        AdvisoryOwner(advisory);
        return false;
    };
    graph.set_watch(WatchPhase::Starting, Some(cursor));
    let initial_roots = project_roots(graph);
    let initial_hashes = initial_descriptor_hashes(&initial_roots);
    let watcher = Watcher {
        graph: graph.clone(),
        hub: hub.clone(),
        cursor,
        advisory: AdvisoryOwner(advisory),
        project_roots: std::cell::RefCell::new(initial_roots),
        descriptor_hashes: std::cell::RefCell::new(initial_hashes),
        _live: stop.enter(),
        stop,
    };
    let spawned =
        std::thread::Builder::new().name("bsl-graph-watch".to_owned()).spawn(move || watcher.run());
    match spawned {
        Ok(_) => {
            lease.handed_over();
            true
        }
        Err(error) => {
            tracing::warn!("graph watcher could not start: {error}");
            graph.set_watch(WatchPhase::Stopped, None);
            false
        }
    }
}

struct Watcher {
    graph: GraphState,
    hub: WorkspaceChangeHub,
    cursor: SinkCursor,
    advisory: AdvisoryOwner,
    /// The roots the project had when a descriptor was last looked at: what a delivered
    /// `Configuration.xml` is measured against.
    project_roots: std::cell::RefCell<Vec<PathBuf>>,
    /// Semantic structure hashes of known root descriptors.
    descriptor_hashes: std::cell::RefCell<std::collections::HashMap<PathBuf, [u8; 32]>>,
    stop: OwnerStop,
    /// Last, so the owner counts as gone only after its advisory is handed back.
    _live: OwnerLive,
}

impl Watcher {
    fn run(mut self) {
        // Read BEFORE the first observation, and carried into the wait loop. Read afterwards it
        // would already count everything delivered DURING that observation, and the first wait
        // would then not fire for an edit that had arrived before the watcher ever waited —
        // the graph would serve that edit's stale answer for a whole slice while reporting
        // itself as watching.
        let generation = self.hub.generation();
        if self.observe_first() {
            self.graph.set_watch(WatchPhase::Running, Some(self.cursor));
            self.watch(generation);
        }
        self.graph.set_watch(WatchPhase::Stopped, None);
        self.hub.unsubscribe(self.cursor);
        // And the cursor the graph's own comparison keeps: the observation is over, so
        // nothing will drain it again.
        self.graph.release_scan_cursor();
    }

    fn must_leave(&self) -> bool {
        self.hub.is_closing() || self.leaves_without_asking_the_hub()
    }

    /// The half of [`Self::must_leave`] that asks nobody the hub is holding a lock for.
    ///
    /// A predicate handed to one of the hub's waits runs with the hub's accumulator lock held,
    /// so it must not read the hub back: the hub's own `closing` is checked inside the wait
    /// anyway, and asking for it here would be the same thread taking that lock twice.
    fn leaves_without_asking_the_hub(&self) -> bool {
        self.stop.is_stopped() || self.graph.lease_is_terminal()
    }

    /// The first observation: the hub settled, everything already delivered applied, and one
    /// fingerprint check made after the cursor existed. An empty result completes it too —
    /// a quiet start is exactly the case where nothing needs doing.
    fn observe_first(&mut self) -> bool {
        loop {
            if self.must_leave() {
                return false;
            }
            match self
                .hub
                .watch_readiness_or(READINESS_SLICE, || self.leaves_without_asking_the_hub())
            {
                WatchReadiness::Armed | WatchReadiness::Failed => break,
                WatchReadiness::NotYet => {}
            }
        }
        self.drain();
        // A build already under way may have taken its pre-scan before the watch armed, so it
        // is told to re-check disk at its publication. An idle graph needs nothing: the boot's
        // first build scans after this point anyway, and starting one here would steal the
        // fused boot build's claim.
        if !matches!(self.graph.status(), GraphStatus::Idle | GraphStatus::Disabled) {
            // A LEVEL, not a delivery. Nothing arrived here: this is the watcher saying where
            // the hub stood when it started watching, and a build already under way may have
            // taken its pre-scan before that. It may ask for a comparison; it may not open a
            // retry epoch, because no external work happened — and recording it as a delivery
            // handed a graph whose build had already failed a free new budget at startup.
            self.graph.observe_current_level(self.hub.seq());
        }
        self.advisory.refresh(&self.graph, &self.stop);
        !self.must_leave()
    }

    fn watch(&mut self, from: u64) {
        let mut generation = from;
        loop {
            if self.must_leave() {
                return;
            }
            // ACT, then wait. The first observation records its batch without deciding, so
            // entering the wait first left whatever it recorded sitting ripe for a whole
            // slice: a debt that is ripe NOW names no moment by design — it is the executor's
            // turn, not the alarm's — and the wait, reading only the alarm, has nothing to
            // shorten it. On a quiet warm boot nothing moves the hub either, so the wait runs
            // its full length while the graph reports itself behind. Acting first also covers
            // the turn where the alarms leave work still ripe.
            self.ring_alarms();
            if self.must_leave() {
                return;
            }
            // Read BEFORE the alarm counter, and that order is the whole point: a turn that
            // yielded with work still ripe left this latch, and sampling the counter first
            // would let the hand-off fall between the two and become a full slice of sleep.
            //
            // It shortens the WAIT to nothing; it does not skip the turn. Jumping back to the
            // top would step over the drain below, and the drain is how deliveries reach the
            // graph at all — a latch that kept re-arming would then starve the very work it
            // was raised to hurry.
            let continuing = self.graph.take_continuation();
            #[cfg(test)]
            self.graph.enter_latch_window();
            let alarms = self.graph.alarms.load(std::sync::atomic::Ordering::SeqCst);
            let wait = if continuing { Duration::ZERO } else { self.alarm_wait(Instant::now()) };
            let next_generation = self.hub.wait_for_change_or(generation, wait, || {
                self.graph.alarms.load(std::sync::atomic::Ordering::SeqCst) != alarms
                    // Read here too, not only above. The latch can be raised in the window
                    // between taking it and sampling the counter, and then the counter this
                    // predicate compares against ALREADY carries the producer's bump — so
                    // without this the owner sleeps out its slice over work handed to it.
                    || self.graph.continuation_pending()
                    || self.leaves_without_asking_the_hub()
            });
            if self.must_leave() {
                return;
            }
            if next_generation != generation {
                // Disk events arrived: debounce to collapse batches of file writes (150ms quiet, up to 1000ms max)
                self.debounce_quiet(next_generation);
                generation = self.hub.generation();
            } else {
                generation = next_generation;
            }
            if self.must_leave() {
                return;
            }
            self.drain();
            // Asked again between the two: a drain records, and the alarms at the top of the
            // next turn decide — which can walk the whole tree or run a probe, taking seconds.
            // A stop that lands in there is answered by leaving, or a build would start after
            // the daemon asked every owner to go and before the workspace is handed back.
        }
    }

    /// Wait for a period of silence (quiescence) after change events arrive, to coalesce
    /// rapid series of filesystem modifications (e.g. from editor saves or git checkouts).
    fn debounce_quiet(&self, mut current_gen: u64) {
        let started = Instant::now();
        while started.elapsed() < DEBOUNCE_MAX_WAIT {
            if self.must_leave() {
                return;
            }
            let remaining = DEBOUNCE_MAX_WAIT.saturating_sub(started.elapsed());
            let wait = DEBOUNCE_QUIET.min(remaining);
            // `wait_for_change_or` runs this predicate while holding the hub accumulator lock;
            // do not call `must_leave`, which asks the hub whether it is closing and deadlocks.
            let next_gen = self
                .hub
                .wait_for_change_or(current_gen, wait, || self.leaves_without_asking_the_hub());
            if next_gen == current_gen {
                break;
            }
            current_gen = next_gen;
        }
    }

    /// How long the next wait may last: the slice, or less when owed work comes due sooner.
    /// One question, asked of the one state that holds every debt.
    fn alarm_wait(&self, now: Instant) -> Duration {
        self.graph
            .wake_at(now)
            .map(|due| due.saturating_duration_since(now))
            .unwrap_or(WAIT_SLICE)
            .min(WAIT_SLICE)
    }

    /// Owed work whose moment has come. The watcher is the graph's alarm: nothing else wakes
    /// it when a backoff elapses, a probe falls due or marks nobody consumed run out of grace.
    /// What to do about each is not decided here — that is `drive`, and this is one of its
    /// three callers.
    fn ring_alarms(&self) {
        // The first build is not the watcher's to take — it belongs to the boot's fused pass,
        // one parse of the workspace for the graph and the search index together. Unless a
        // request has asked for it: the asking is a request's, the deciding is this thread's,
        // and the lease that deciding reads is a file, which is why it is read here and not on
        // the thread serving the request.
        //
        // The same entry the boot calls, with the same guards: a first build is owed to the
        // asking itself, and a failed graph is still the schedule's business — only a retry it
        // calls due starts anything.
        if self.graph.take_first_build_ask() {
            self.graph.ensure_loading();
        }
        self.graph.register_owed_context_marks();
        self.graph.drive_without_the_first_build();
    }

    /// Record the batch. The watcher RECORDS and does not decide: deciding here would take
    /// the first build of an idle graph, which belongs to the boot's fused pass — one parse of
    /// the workspace for the graph and the search index together instead of two. What the
    /// watcher does decide, it decides in [`Self::ring_alarms`], which cannot take that build.
    fn drain(&mut self) {
        let batch = self.hub.materialize(self.cursor);
        let fact = batch.fact_seq();
        if batch.rescan_required {
            self.apply_rescan(batch.loss_token(), fact);
        } else if !batch.entries.is_empty() {
            self.apply(&batch.entries, fact);
        }
        self.hub.acknowledge(&batch);
    }

    /// Lost detail: anything may have changed, the config included. A forced reload covers
    /// both — it rebuilds whatever drifted and re-reads the configuration even when the graph
    /// fingerprint compares equal — and a comparison cannot answer a loss.
    ///
    /// Recorded under the fact the reconcile stands at: the publication that observes it is
    /// what discharges the debt, whether this watcher, the search consumer or both recorded it.
    fn apply_rescan(&self, token: Option<u64>, fact: u64) {
        self.graph.record_loss_quietly(token, fact);
        self.advisory.refresh(&self.graph, &self.stop);
    }

    fn apply(&self, entries: &[ChangeEntry], fact: u64) {
        let project = entries.iter().any(|entry| self.is_project_input(entry));
        let universe = entries.iter().any(super::snapshot::entry_touches_scan_universe);
        if universe {
            self.graph.record_change_quietly(fact);
        }
        if project {
            // Root aliases and declared spellings move search ownership without moving the
            // canonical topology, so no fingerprint comparison can answer this one.
            self.graph.record_forced_quietly(fact);
            self.advisory.refresh(&self.graph, &self.stop);
        }
    }

    /// A workspace config file, or the root descriptor of the configuration or of an
    /// extension: either can move the project's roots, and neither moves a fingerprint the
    /// graph compares for every such change.
    fn is_project_input(&self, entry: &ChangeEntry) -> bool {
        if self.graph.is_workspace_config_path(&entry.canonical)
            || self.graph.is_workspace_config_path(&entry.raw)
        {
            return true;
        }
        let named_descriptor = |path: &std::path::Path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(bsl_conventions::conventional_of)
                == Some(bsl_conventions::ConventionalName::ConfigurationXml)
        };
        if !named_descriptor(&entry.canonical) && !named_descriptor(&entry.raw) {
            return false;
        }
        // The name alone says nothing: exported dumps and nested projects carry it too, and
        // editing one changes no root of this project. What does is the descriptor of a root
        // the project has — base, extension or external, declared or discovered, inside the
        // workspace or not — or one whose change moves which roots it has. The second is asked
        // of the project model itself, so no discovery rule is restated here.
        let fresh = project_roots(&self.graph);
        let known = self.project_roots.replace(fresh.clone());
        if known != fresh {
            // The structural root change itself still forces a rebuild. Seed descriptor
            // observations for the new root set now so a later Version/Comment-only save
            // on a newly-added extension can be recognized as a semantic no-op.
            *self.descriptor_hashes.borrow_mut() = initial_descriptor_hashes(&fresh);
            return true;
        }
        let parent = |path: &std::path::Path| {
            path.parent()
                .map(|dir| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
        };
        let touches_known_root = [parent(&entry.canonical), parent(&entry.raw)]
            .into_iter()
            .flatten()
            .any(|dir| known.contains(&dir) || fresh.contains(&dir));
        if !touches_known_root {
            return false;
        }

        // It touches a known Configuration.xml descriptor while roots composition remains the same.
        // Check if the semantic structure of the XML changed (ignoring <Version>, <Comment>, whitespace).
        let target_path = if entry.canonical.exists() { &entry.canonical } else { &entry.raw };
        if let Some(hash) = super::scan::xml_semantic_hash_file(target_path) {
            let mut hashes = self.descriptor_hashes.borrow_mut();
            if let Some(&old_hash) = hashes.get(target_path) {
                if old_hash == hash {
                    // Semantic no-op: version/comment edit did not change root descriptor structure.
                    return false;
                }
            }
            hashes.insert(target_path.clone(), hash);
            if let Ok(canon) = std::fs::canonicalize(target_path) {
                hashes.insert(canon, hash);
            }
        }
        true
    }
}

fn initial_descriptor_hashes(roots: &[PathBuf]) -> std::collections::HashMap<PathBuf, [u8; 32]> {
    let mut map = std::collections::HashMap::new();
    for root in roots {
        let desc = root.join(bsl_conventions::ConventionalName::ConfigurationXml.canonical());
        if let Some(hash) = super::scan::xml_semantic_hash_file(&desc) {
            if let Ok(canon) = std::fs::canonicalize(&desc) {
                map.insert(canon, hash);
            }
            map.insert(desc, hash);
        }
    }
    map
}

/// The roots the project at the graph's workspace declares right now, canonical and sorted.
fn project_roots(graph: &GraphState) -> Vec<PathBuf> {
    let Some(root) = graph.workspace_root.as_deref() else { return Vec::new() };
    let mut roots: Vec<PathBuf> = crate::project::at(root)
        .map(|project| project.source_roots())
        .unwrap_or_default()
        .into_iter()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(test)]
mod the_watcher_records_and_does_not_decide {
    /// The watcher's recording paths are the QUIET ones, everywhere.
    ///
    /// `record_change` and `record_forced` each drive the executor from inside themselves, so
    /// a watcher that calls either decides wherever it records — including inside its first
    /// drain, before the boot's fused pass has claimed the build, and inside every later drain
    /// while the graph is still idle. What the watcher may decide it decides in `ring_alarms`,
    /// which drives the form that cannot take the first build.
    ///
    /// Counted rather than left to review, because the next recording call added here will be
    /// added by someone who has not read this. The needles are assembled at run time: spelled
    /// out, they would match this gate's own source and pass for the wrong reason.
    #[test]
    fn the_watcher_never_calls_a_recording_that_decides() {
        let source = include_str!("watcher.rs");
        // A CRLF checkout (core.autocrlf on Windows, no .gitattributes pinning LF) gives this
        // file "\r\n" endings, and a needle anchored on "\n" would then match nothing — the
        // gate would fail on the line endings rather than on the code, in the very CI step that
        // runs it by name. Normalised first, so the gate is about the source and not the
        // checkout.
        let source = &source.replace("\r\n", "\n");
        let cut = ["\n#[cfg(test)]\n", "mod the_watcher_records_and_does_not_decide {"].concat();
        assert_eq!(
            source.matches(&cut).count(),
            1,
            "the production/test cut moved; this gate scans only what it can prove it scanned",
        );
        let production = source.split(&cut).next().unwrap_or(source);
        let quiet_change = ["record_change", "_quietly("].concat();
        let quiet_forced = ["record_forced", "_quietly("].concat();
        let deciding_change = ["record_change", "("].concat();
        let deciding_forced = ["record_forced", "("].concat();

        assert!(
            production.contains(&quiet_change) && production.contains(&quiet_forced),
            "the watcher no longer records at all; this gate has lost its subject",
        );
        // The deciding names carry their own parenthesis, which the quiet ones do not: a call
        // of either is a call of the form that drives the executor where it records.
        assert_eq!(
            production.matches(&deciding_change).count(),
            0,
            "the watcher records a change through the form that decides where it records",
        );
        assert_eq!(
            production.matches(&deciding_forced).count(),
            0,
            "the watcher records a forced reload through the form that decides where it records",
        );
        // And the one place it does decide is the alarm, through the restricted executor.
        let restricted = ["drive_without", "_the_first_build()"].concat();
        assert!(
            production.contains(&restricted),
            "the watcher's alarm drives the executor that may take the first build",
        );
    }
}

#[cfg(test)]
mod tests {

    /// The watcher's first observation is a LEVEL, not a delivery. It says where the hub stood
    /// when watching began; nothing external happened at that instant.
    ///
    /// Recording it as a delivery handed a graph whose build had already failed a brand new
    /// retry budget at startup — reachable whenever the first observation loses the race with
    /// an initialisation that has already failed, and no second `start` is needed for it.
    #[test]
    fn initial_observation_is_not_a_new_failure_credit() {
        use super::super::debt::FailureKind;
        use super::super::state::lock_recover;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::graph::test_support::sample_workspace(root);
        let graph = crate::graph::state::GraphState::for_workspace(root.to_path_buf());

        // A build that has already failed for good, with its credit spent on the admission.
        lock_recover(&graph.debt).charge_admission(7, std::time::Instant::now(), false);
        graph.record_failure(FailureKind::Operation);
        lock_recover(&graph.inner).status =
            crate::graph::types::GraphStatus::Failed("operation".to_owned());
        let spent = graph.debt_standing(std::time::Instant::now()).failed;
        assert!(
            matches!(spent, Some(super::super::debt::Ripeness::Exhausted(_))),
            "the stand needs a spent budget: {spent:?}",
        );

        // The hub has moved ON — a write outside this graph's scan roots advances the shared
        // sequence without ever being delivered here — so the level the first observation
        // reports stands ABOVE what was spent. That gap is the whole exposure: a delivery
        // there is external work and legitimately reopens the budget, while an observation of
        // the same number is only a reading of the stream.
        graph.observe_current_level(12);

        let after = graph.debt_standing(std::time::Instant::now()).failed;
        assert!(
            matches!(after, Some(super::super::debt::Ripeness::Exhausted(_))),
            "an observation reopened a spent budget with no external work: {after:?}",
        );

        // The positive control, so the check above cannot pass by forbidding revival outright:
        // a fact actually DELIVERED at that level is work, and it does reopen the budget.
        let delivered = crate::graph::state::GraphState::for_workspace(root.to_path_buf());
        lock_recover(&delivered.debt).charge_admission(7, std::time::Instant::now(), false);
        delivered.record_failure(FailureKind::Operation);
        lock_recover(&delivered.inner).status =
            crate::graph::types::GraphStatus::Failed("operation".to_owned());
        lock_recover(&delivered.debt).record_change(std::time::Instant::now(), 12);
        assert!(
            !matches!(
                delivered.debt_standing(std::time::Instant::now()).failed,
                Some(super::super::debt::Ripeness::Exhausted(_))
            ),
            "a real delivery above the frontier must reopen the budget",
        );
    }
    use super::super::test_support::{published_report, sample_workspace, wait_ready, wait_until};
    use super::*;

    #[test]
    fn debounce_window_matches_the_disk_event_contract() {
        assert_eq!(DEBOUNCE_QUIET, Duration::from_millis(150));
        assert_eq!(DEBOUNCE_MAX_WAIT, Duration::from_secs(1));
    }
    use crate::change_hub::test_support::eventually;
    use std::sync::{Arc, Mutex};

    fn watched_graph(root: &std::path::Path) -> (GraphState, WorkspaceChangeHub, OwnerStop) {
        let hub = super::super::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        (graph, hub, OwnerStop::default())
    }

    /// A watcher that never began leaves the advisory saying nobody keeps it current.
    ///
    /// The slot is a CACHED project advisory, and caching it is only honest while an updater
    /// exists. Two ways for one never to exist: the cursor cannot be leased, and the thread
    /// cannot be spawned. The second is covered by the owner's own drop — the `Watcher` built
    /// for the thread is dropped when the spawn fails — and this asserts that mechanism, which
    /// is the reachable one. The first cannot be reached today (`WorkspaceChangeHub::subscribe`
    /// always yields a cursor) and is guarded in `start` so the invariant does not rest on that
    /// staying true.
    #[test]
    fn an_advisory_nobody_keeps_current_says_so() {
        let slot = Arc::new(Mutex::new(crate::state::StandaloneNotice::tracked(Some(
            "анализируется без основной конфигурации".to_owned(),
        ))));
        let before = slot.lock().unwrap().clone();

        drop(AdvisoryOwner(Some((std::path::PathBuf::from("/тест"), Arc::clone(&slot)))));

        let after = slot.lock().unwrap().clone();
        assert_ne!(after, before, "the slot still claims someone keeps it current");
    }

    #[test]
    fn advisory_refresh_before_scope_guard_preserves_the_boot_project_on_drift() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (name, purpose) in [
            ("cf", ""),
            ("ext", "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("Configuration.xml"),
                format!("<MetaDataObject><Configuration><Properties>{purpose}</Properties></Configuration></MetaDataObject>"),
            )
            .unwrap();
        }
        let config = root.join("bsl-analyzer.toml");
        std::fs::write(&config, "[source]\nroot = \"ext\"\nextensions = []\n").unwrap();
        let boot_project = crate::project::at(root).unwrap();
        let cache = crate::cache::WorkspaceCacheLayout::for_project(
            &boot_project,
            Some(&root.join("cache")),
            root,
            None,
        )
        .unwrap();
        let graph = GraphState::for_workspace_with_cache(root.to_path_buf(), cache.clone());
        let owners = OwnerStop::default();
        let transport_stop = tokio_util::sync::CancellationToken::new();
        owners.set_scope_transport_stop(transport_stop.clone());
        let boot_notice = crate::state::StandaloneNotice::tracked(
            crate::state::standalone_notice_of(&boot_project),
        );
        assert!(crate::state::standalone_notice_of(&boot_project).is_some());
        let slot = Arc::new(Mutex::new(boot_notice.clone()));
        let advisory = AdvisoryOwner(Some((root.to_path_buf(), Arc::clone(&slot))));

        std::fs::write(&config, "[source]\nroot = \"cf\"\nextensions = []\n").unwrap();
        // This simulates the graph watcher handling the event before the independent scope guard.
        advisory.refresh(&graph, &owners);

        assert_eq!(*slot.lock().unwrap(), boot_notice, "new-scope advisory is not published");
        assert!(owners.is_stopped(), "the mismatching Project stops background owners");
        assert!(transport_stop.is_cancelled(), "the serving transport is retired");
        assert!(!cache.root().exists(), "advisory validation does not create cache storage");
    }

    #[test]
    fn advisory_project_error_stops_owners_without_cache_layout() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphState::disabled();
        assert!(graph.cache().is_none());
        let owners = OwnerStop::default();
        let transport_stop = tokio_util::sync::CancellationToken::new();
        owners.set_scope_transport_stop(transport_stop.clone());
        let boot_notice = StandaloneNotice::tracked(Some("boot notice".to_owned()));
        let slot = Arc::new(Mutex::new(boot_notice.clone()));
        let advisory = AdvisoryOwner(Some((dir.path().to_path_buf(), Arc::clone(&slot))));
        std::fs::write(dir.path().join("bsl-analyzer.toml"), "[source\n").unwrap();

        advisory.refresh(&graph, &owners);

        assert_eq!(*slot.lock().unwrap(), boot_notice);
        assert!(owners.is_stopped());
        assert!(transport_stop.is_cancelled());
    }

    /// The watcher RECORDS an idle graph's debt and never takes its first build.
    ///
    /// The watcher's cursor is subscribed before anything builds the graph, so it is draining
    /// while the graph is still `Idle` and the boot's fused pass has yet to claim the build.
    /// Taking it there parses the workspace once for the graph and again for the search index
    /// instead of once for both. Both doors are closed here: the drain records without
    /// deciding, AND the alarm drives an executor that may not take that build.
    #[test]
    fn the_watcher_records_an_idle_graphs_debt_without_taking_its_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let pending = |graph: &GraphState, hub: &WorkspaceChangeHub, stop: &OwnerStop| {
            let cursor = hub.subscribe();
            std::fs::write(root.join("Новый.bsl"), "Процедура П() КонецПроцедуры").unwrap();
            assert!(
                eventually(Duration::from_secs(10), || !hub.materialize(cursor).entries.is_empty()),
                "the hub never delivered the write the observation must find waiting",
            );
            Watcher {
                graph: graph.clone(),
                hub: hub.clone(),
                cursor,
                advisory: AdvisoryOwner(None),
                project_roots: std::cell::RefCell::new(super::project_roots(graph)),
                descriptor_hashes: std::cell::RefCell::new(std::collections::HashMap::new()),
                _live: stop.enter(),
                stop: stop.clone(),
            }
        };

        let (graph, hub, stop) = watched_graph(root);
        let mut watcher = pending(&graph, &hub, &stop);

        // The drain records and does not decide.
        watcher.drain();
        assert_eq!(
            graph.status(),
            GraphStatus::Idle,
            "the drain took the build the boot's fused pass was on its way to claim",
        );
        assert!(
            graph.owes_change().is_some() || graph.owes_forced().is_some(),
            "the drain recorded no debt at all",
        );

        // And the alarm does not take it either — which is the half that matters, because a
        // debt ripe NOW names no moment, so the watcher's wait falls back to its slice and
        // rings anyway. Deferring alone would only move the theft later.
        watcher.ring_alarms();
        assert_eq!(
            graph.status(),
            GraphStatus::Idle,
            "the watcher's alarm took the first build of an idle graph",
        );

        // The control: the unrestricted executor DOES take it, so the two assertions above are
        // about the restriction and not about a graph with nothing to decide.
        graph.drive();
        assert_ne!(
            graph.status(),
            GraphStatus::Idle,
            "the boot's own executor left the idle graph alone, so the restriction proves nothing",
        );
        hub.unsubscribe(watcher.cursor);
        stop.stop();
    }

    /// And it DOES take it once a request has asked for it.
    ///
    /// The request may not decide — deciding reads the lifecycle facts, and those include a
    /// lease file — so what it leaves is the ask, and this owner answers it on its own thread.
    /// Without that, "a workspace whose user only ever asks for the graph gets its first build
    /// from the asking" would simply have stopped being true.
    #[test]
    fn the_watcher_answers_a_first_build_a_request_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, hub, stop) = watched_graph(root);
        // The real owner, started the way the boot starts it.
        assert!(super::start(&graph, &hub, None, stop.clone()), "the watcher must start");
        wait_until(&graph, "the watcher to run", || graph.watch_state().0 == WatchPhase::Running);
        assert_eq!(
            graph.status(),
            GraphStatus::Idle,
            "the stand needs a graph nobody has built yet",
        );

        graph.ensure_first_build();
        let kicks = graph.first_build_kicks.load(std::sync::atomic::Ordering::SeqCst);

        wait_ready(&graph);
        assert_eq!(
            kicks, 0,
            "the watcher was running, so nothing else should have been started to ask it",
        );
        hub.shutdown();
        stop.stop();
    }

    /// The same ask where no watcher will ever come for it: one short-lived kick carries it,
    /// single-flight, and the lifecycle it calls is the same one the boot calls.
    #[test]
    fn a_watcherless_graph_still_builds_from_the_asking() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let graph = GraphState::for_workspace(root.to_path_buf());
        assert_eq!(graph.watch_state().0, WatchPhase::Unwatched, "the stand needs no watcher");

        for _ in 0..5 {
            graph.ensure_first_build();
        }

        wait_ready(&graph);
        let kicks = graph.first_build_kicks.load(std::sync::atomic::Ordering::SeqCst);
        assert!(kicks >= 1, "nothing carried the ask");
        assert!(kicks <= 5, "a burst of requests started a worker each: {kicks}");
    }

    /// The comparison keeps a cursor of its own — subscribed lazily, the first time the graph
    /// asks what is on disk — and it is the graph's, not the watcher's. When the observation
    /// ends it has to go with it: a cursor nobody drains makes the hub hold every entry for a
    /// consumer that has left, for as long as the hub lives.
    #[test]
    fn the_observation_takes_the_comparison_cursor_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, hub, stop) = watched_graph(root);
        let before = hub.active_cursor_count();

        // The first comparison subscribes the graph's own cursor.
        graph.current_disk_fp();
        assert_eq!(
            hub.active_cursor_count(),
            before + 1,
            "control: the comparison keeps a cursor of its own"
        );

        // Wired as the boot wires it: the stop releases the hub's waits too.
        let woken = hub.clone();
        stop.wakes(move || woken.interrupt_waiters());
        assert!(start(&graph, &hub, None, stop.clone()));
        wait_until(&graph, "the watcher to run", || graph.watch_state().0 == WatchPhase::Running);
        stop.stop();

        assert!(
            eventually(Duration::from_secs(5), || hub.active_cursor_count() == before),
            "the graph left a cursor behind: {} of {before}",
            hub.active_cursor_count()
        );
        hub.shutdown();
    }

    /// A failed graph that is owed a rebuild waits for its backoff, and when the backoff runs
    /// out nothing else calls back into it: no request walks disk, and the drift that armed it
    /// was delivered once. The watcher is the alarm, with no new event needed.
    #[test]
    fn a_failed_graph_is_retried_when_its_backoff_runs_out() {
        let dir = tempfile::tempdir().unwrap();
        sample_workspace(dir.path());
        let (graph, hub, stop) = watched_graph(dir.path());
        let due = Instant::now() + Duration::from_millis(700);
        graph.fail_with_retry_held_until(due);

        assert!(start(&graph, &hub, None, stop.clone()));
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            matches!(graph.status(), GraphStatus::Failed(_)),
            "the retry started before its backoff elapsed"
        );
        wait_until(&graph, "the owed rebuild to publish", || {
            matches!(graph.status(), GraphStatus::Ready { .. })
        });
        assert!(Instant::now() >= due);
        stop.stop();
        hub.interrupt_waiters();
        assert!(eventually(Duration::from_secs(1), || stop.live() == 0));
    }

    /// Work handed over inside the sampling window is not slept through.
    ///
    /// The owner takes the latch, samples the alarm counter, and waits on both. A turn that
    /// yields with work still ripe raises the latch — and the bump it makes on the way is
    /// already inside the sample the wait is about to compare against, so a predicate watching
    /// only the counter has nothing left to notice. The hand-off then costs a whole slice of
    /// sleep over work already handed over.
    ///
    /// A barrier, not a sleep-for-luck: the producer runs ON the owner's thread at exactly
    /// that point — `latch_window_hook` — so the window is hit every turn, by construction.
    /// The workspace is quiet — nothing writes to it after the graph is ready — so the wait
    /// the hand-off shortens is the full slice, and the control below says so by watching what
    /// happens once the hand-offs stop.
    #[test]
    fn a_latch_raised_inside_the_sampling_window_is_not_slept_through() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hub = super::super::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let stop = OwnerStop::default();

        let turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook = {
            let turns = Arc::clone(&turns);
            Arc::new(move |graph: &GraphState| {
                // Three hand-offs, then quiet: the fourth turn is the control below.
                if turns.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 {
                    graph.latch_continuation();
                }
            }) as Arc<dyn Fn(&GraphState) + Send + Sync>
        };
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_change_hub(hub.clone())
            .with_latch_window_hook(hook);
        graph.ensure_loading();
        wait_ready(&graph);
        // Start from a quiet latch, so the first wait is a real one and the turns counted
        // below are the hand-off's doing.
        graph.take_continuation();

        let woken = hub.clone();
        stop.wakes(move || woken.interrupt_waiters());
        assert!(start(&graph, &hub, None, stop.clone()));

        // The window is entered once per turn, so the count IS the number of turns taken.
        assert!(
            eventually(Duration::from_secs(5), || turns.load(std::sync::atomic::Ordering::SeqCst)
                >= 4),
            "the owner slept out its slice over work handed to it inside the sampling window: \
             {} turns",
            turns.load(std::sync::atomic::Ordering::SeqCst),
        );
        // The control. The window is entered at the TOP of a turn, so one more entry follows
        // the last hand-off and then the wait is a full slice again: the turns above came from
        // the hand-off and not from an owner that never sleeps. Bounded rather than exact —
        // the hub may deliver something of its own — but a slice of thirty seconds against a
        // handful of turns in two is the difference being asserted.
        let settled = turns.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            !eventually(Duration::from_secs(2), || turns.load(std::sync::atomic::Ordering::SeqCst)
                > settled + 2),
            "the owner turns over with nothing handed to it; the assertion above proves nothing",
        );

        stop.stop();
        hub.interrupt_waiters();
        wait_until(&graph, "the watcher to stop", || graph.watch_state().0 == WatchPhase::Stopped);
        hub.shutdown();
    }

    /// A quiet start completes the first observation: no event is needed for the watcher to
    /// vouch that it is watching.
    #[test]
    fn a_quiet_start_completes_the_first_observation() {
        let dir = tempfile::tempdir().unwrap();
        sample_workspace(dir.path());
        let (graph, hub, stop) = watched_graph(dir.path());
        assert!(start(&graph, &hub, None, stop.clone()));
        wait_until(&graph, "the watcher to run", || graph.watch_state().0 == WatchPhase::Running);
        stop.stop();
        hub.interrupt_waiters();
        wait_until(&graph, "the watcher to stop", || graph.watch_state().0 == WatchPhase::Stopped);
        assert_eq!(hub.active_cursor_count(), 0, "the watcher left its cursor behind");
    }

    /// Search init hangs while edits keep coming: its cursor is subscribed and never drained.
    /// The hub holds no more than its capacity — the stalled cursor is moved past what it pins
    /// and owes a reconcile — the watcher keeps the graph current by itself, and the consumer
    /// that finally starts reconciles once, not once per lost batch.
    #[test]
    fn a_stalled_search_cursor_costs_the_hub_its_capacity_at_most_and_the_graph_nothing() {
        const CAP: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let hub = WorkspaceChangeHub::start_with_capacity(
            vec![crate::change_hub::WatchTarget::recursive(root.to_path_buf())],
            CAP,
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let stalled = hub.subscribe();
        let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        let stop = OwnerStop::default();
        assert!(start(&graph, &hub, None, stop.clone()));
        graph.ensure_loading();
        wait_ready(&graph);
        let GraphStatus::Ready { files: before } = graph.status() else { unreachable!() };

        let edits = CAP * 4;
        let mut most = 0;
        for i in 0..edits {
            super::super::test_support::write(
                root,
                &format!("CommonModules/Поток{i}/Ext/Module.bsl"),
                "Процедура П() КонецПроцедуры",
            );
            most = most.max(hub.undrained_paths());
        }
        wait_until(
            &graph,
            "the graph to hold every edit",
            || matches!(graph.status(), GraphStatus::Ready { files } if files == before + edits),
        );
        assert!(most <= CAP && hub.undrained_paths() <= CAP, "the hub held {most} paths");

        let first = hub.drain(stalled);
        assert!(first.rescan_required, "the stalled cursor lost facts: it must reconcile");
        let second = hub.drain(first.cursor);
        assert!(!second.rescan_required, "one reconcile answers every lost batch");
        hub.unsubscribe(second.cursor);
        stop.stop();
        hub.interrupt_waiters();
    }

    /// A reconcile tells the watcher that anything may have changed, the configuration
    /// included, whatever the graph is doing: the next build it gets is a project reload.
    #[test]
    fn a_reconcile_owes_a_project_reload_in_every_live_state() {
        let dir = tempfile::tempdir().unwrap();
        sample_workspace(dir.path());
        let (graph, hub, stop) = watched_graph(dir.path());
        for (iteration, status) in
            [GraphStatus::Loading, GraphStatus::Failed("down".to_owned())].into_iter().enumerate()
        {
            super::super::state::lock_recover(&graph.inner).status = status.clone();
            let watcher = Watcher {
                graph: graph.clone(),
                hub: hub.clone(),
                cursor: hub.subscribe(),
                advisory: AdvisoryOwner(None),
                project_roots: std::cell::RefCell::new(super::project_roots(&graph)),
                descriptor_hashes: std::cell::RefCell::new(std::collections::HashMap::new()),
                _live: stop.enter(),
                stop: stop.clone(),
            };
            watcher.apply_rescan(Some(iteration as u64), hub.seq());
            assert!(
                graph.owes_forced().is_some() || graph.drift_pending(),
                "{status:?}: no project reload owed",
            );
            hub.unsubscribe(watcher.cursor);
        }
    }

    /// A reload that failed is retried by the watcher when its backoff runs out, with no
    /// further event: nothing else calls back into a graph whose request path reads no disk.
    #[test]
    fn a_failed_reload_is_retried_without_another_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, hub, stop) = super::super::test_support::watched_graph(root);
        graph.ensure_loading();
        wait_ready(&graph);
        let before = published_report(&graph).revision.unwrap();
        graph.refused_installs.store(1, std::sync::atomic::Ordering::SeqCst);
        super::super::test_support::write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт Возврат 7; КонецФункции",
        );
        wait_until(&graph, "the failed reload to be retried", || {
            graph.status_report().revision.is_some_and(|revision| revision > before)
        });
        assert_eq!(graph.refused_installs.load(std::sync::atomic::Ordering::SeqCst), 0);
        stop.stop();
        hub.interrupt_waiters();
    }

    /// The watcher does not wait for search: it alone carries a body edit to the graph.
    #[test]
    fn a_body_edit_reloads_the_graph_with_no_other_consumer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, hub, stop) = watched_graph(root);
        assert!(start(&graph, &hub, None, stop.clone()));
        graph.ensure_loading();
        wait_ready(&graph);
        let before = published_report(&graph).revision.unwrap();
        let builds_before = graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst);
        super::super::test_support::write(
            root,
            "CommonModules/Сервер/Ext/Module.bsl",
            "&НаСервере\nФункция Считать() Экспорт Возврат 3; КонецФункции",
        );
        wait_until(&graph, "the edit to reach the graph", || {
            graph.status_report().revision.is_some_and(|revision| revision > before)
        });
        wait_until(&graph, "the event's build to settle", || {
            !graph.drift_pending() && !graph.build_in_flight()
        });
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            graph.status_report().revision,
            Some(before + 1),
            "one save should coalesce filesystem notifications into one graph publication"
        );
        assert_eq!(
            graph.full_builds_started.load(std::sync::atomic::Ordering::SeqCst),
            builds_before,
            "body-only save stays incremental"
        );
        stop.stop();
        hub.interrupt_waiters();
    }

    fn recording_watched_graph(
        root: &std::path::Path,
    ) -> (GraphState, WorkspaceChangeHub, OwnerStop, Arc<Mutex<Vec<i64>>>) {
        let hub = super::super::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let bounds = Arc::new(Mutex::new(Vec::new()));
        let hook = {
            let bounds = Arc::clone(&bounds);
            Arc::new(move |signal: crate::graph::GraphPublishSignal| {
                bounds.lock().unwrap().push(signal.mark_bound);
                crate::graph::GraphPublishOutcome::HANDLED
            })
                as Arc<
                    dyn Fn(crate::graph::GraphPublishSignal) -> crate::graph::GraphPublishOutcome
                        + Send
                        + Sync,
                >
        };
        let graph = GraphState::for_workspace(root.to_path_buf())
            .with_change_hub(hub.clone())
            .with_publish_hook(hook);
        (graph, hub, OwnerStop::default(), bounds)
    }

    /// The graph caught up with a metadata edit on its own — search had not attached yet —
    /// and the search consumer places the edit's marks only afterwards. The publication that
    /// observed the edit consumes them the moment they are placed; nobody waits for another.
    #[test]
    fn marks_placed_after_the_graph_caught_up_on_its_own_are_consumed_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, hub, stop, bounds) = recording_watched_graph(root);
        assert!(start(&graph, &hub, None, stop.clone()));
        graph.ensure_loading();
        wait_ready(&graph);
        let before = published_report(&graph).revision.unwrap();

        // Every reload this edit causes starts after the edit's first fact reached the hub.
        let first_fact = hub.seq() + 1;
        super::super::test_support::write_common_module(
            root,
            "Сервер",
            true,
            "&НаСервере\nФункция Считать() Экспорт Возврат 5; КонецФункции",
        );
        wait_until(&graph, "the watcher to carry the edit to the graph", || {
            graph.status_report().revision.is_some_and(|revision| revision > before)
                && !graph.drift_pending()
        });
        let observed = graph.consuming_observation().expect("a clean publication");
        assert!(observed >= first_fact, "the reload the edit caused did not observe it");

        graph.marks_placed(77, first_fact);
        assert!(bounds.lock().unwrap().contains(&77), "the observing publication did not consume");
        assert!(!graph.marks_pending());
        stop.stop();
        hub.interrupt_waiters();
    }

    /// Marks whose fact no publication observed, on a graph whose fingerprint still matches
    /// disk: an ordinary nudge would schedule nothing. The watcher runs the owed build as a
    /// forced reload, and its publication consumes the marks.
    #[test]
    fn owed_marks_force_a_rebuild_of_an_unchanged_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let (graph, hub, stop, bounds) = recording_watched_graph(root);
        assert!(start(&graph, &hub, None, stop.clone()));
        graph.ensure_loading();
        wait_ready(&graph);
        wait_until(&graph, "the boot nudge to settle", || !graph.drift_pending());
        let before = published_report(&graph).revision.unwrap();

        // A fact the published build did not observe, which changes no fingerprint input.
        let seq_before = hub.seq();
        std::fs::write(root.join("notes.txt"), "not a source").unwrap();
        wait_until(&graph, "the hub to take the fact in", || hub.seq() > seq_before);
        let fact = hub.seq();
        graph.marks_placed(42, fact);
        assert!(!bounds.lock().unwrap().contains(&42), "consumed by a graph that missed the fact");

        wait_until(&graph, "the owed build to publish and consume the marks", || {
            !graph.marks_pending()
        });
        assert!(bounds.lock().unwrap().contains(&42));
        assert!(published_report(&graph).revision.unwrap() > before, "no rebuild ran");
        stop.stop();
        hub.interrupt_waiters();
    }

    /// A delivered `Configuration.xml` forces a project reload only where it is a descriptor
    /// of this project: the base root, an extension or external root, or a position whose
    /// descriptor changes which roots the project has. A file of that name anywhere else — an
    /// exported dump, a nested project — is ordinary XML, and forcing a full reload on every
    /// save of it answers nothing a comparison cannot.
    fn delivery_forces_a_reload(
        root: &std::path::Path,
        change: &str,
        after_watch: impl FnOnce(),
    ) -> bool {
        let hub = super::super::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        let stop = OwnerStop::default();
        let watcher = Watcher {
            graph: graph.clone(),
            hub: hub.clone(),
            cursor: hub.subscribe(),
            advisory: AdvisoryOwner(None),
            project_roots: std::cell::RefCell::new(super::project_roots(&graph)),
            descriptor_hashes: std::cell::RefCell::new(std::collections::HashMap::new()),
            _live: stop.enter(),
            stop: stop.clone(),
        };
        after_watch();
        // A real delivery of the change, so the fact it is recorded under is one no publication
        // has answered.
        let floor = hub.seq();
        let raw = root.join(change);
        let bytes = std::fs::read(&raw).expect("the delivered descriptor exists");
        std::fs::write(&raw, [bytes.as_slice(), b" "].concat()).unwrap();
        let fact = crate::graph::test_support::wait_for_hub_seq_above(&hub, floor);
        let entry = ChangeEntry {
            canonical: std::fs::canonicalize(&raw).unwrap_or_else(|_| raw.clone()),
            raw,
            kind: crate::change_hub::ChangeKind::MaybeChanged,
            seq: fact,
        };
        watcher.apply(&[entry], fact);
        let forced = graph.owes_forced().is_some();
        hub.unsubscribe(watcher.cursor);
        hub.shutdown();
        forced
    }

    #[test]
    fn only_a_descriptor_of_this_project_forces_a_reload() {
        use crate::graph::test_support::{sample_workspace, write, write_extension_workspace};
        // Declared extensions, and a dump nobody declared.
        let declared = tempfile::tempdir().unwrap();
        let root = declared.path();
        write_extension_workspace(root, false);
        write(root, "dump/Configuration.xml", "<Configuration/>");
        assert!(
            !delivery_forces_a_reload(root, "dump/Configuration.xml", || {}),
            "an exported dump's descriptor forced a project reload",
        );
        assert!(
            delivery_forces_a_reload(root, "Configuration.xml", || {}),
            "the base root's descriptor must force a reload",
        );
        assert!(
            delivery_forces_a_reload(root, "ext/a/Configuration.xml", || {}),
            "a declared extension's descriptor must force a reload",
        );

        // Discovered extensions, including one that appears while the watcher runs.
        let discovered = tempfile::tempdir().unwrap();
        let root = discovered.path();
        sample_workspace(root);
        write(root, "Configuration.xml", "<Configuration/>");
        write(root, "cfe/Existing/Configuration.xml", "<Configuration/>");
        assert!(
            delivery_forces_a_reload(root, "cfe/Existing/Configuration.xml", || {}),
            "a discovered extension's descriptor must force a reload",
        );
        let appearing = root.to_path_buf();
        assert!(
            delivery_forces_a_reload(root, "cfe/New/Configuration.xml", move || {
                write(&appearing, "cfe/New/Configuration.xml", "<Configuration/>");
            }),
            "a descriptor that turns a directory into an extension must force a reload",
        );
    }

    #[test]
    fn a_new_extension_descriptor_is_seeded_before_its_version_only_edit() {
        use crate::graph::test_support::{sample_workspace, write};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write(root, "Configuration.xml", "<Configuration/>");
        let hub = super::super::test_support::workspace_hub(root);
        assert!(hub.wait_until_watching(Duration::from_secs(5)));
        let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        let stop = OwnerStop::default();
        let cursor = hub.subscribe();
        let roots = project_roots(&graph);
        let watcher = Watcher {
            graph: graph.clone(),
            hub: hub.clone(),
            cursor,
            advisory: AdvisoryOwner(None),
            project_roots: std::cell::RefCell::new(roots.clone()),
            descriptor_hashes: std::cell::RefCell::new(initial_descriptor_hashes(&roots)),
            _live: stop.enter(),
            stop: stop.clone(),
        };

        let descriptor = root.join("cfe/New/Configuration.xml");
        write(
            root,
            "cfe/New/Configuration.xml",
            "<Configuration><Properties><Version>1.0</Version></Properties></Configuration>",
        );
        let event = |path: &std::path::Path| ChangeEntry {
            canonical: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
            raw: path.to_path_buf(),
            kind: crate::change_hub::ChangeKind::MaybeChanged,
            seq: hub.seq().saturating_add(1),
        };
        assert!(
            watcher.is_project_input(&event(&descriptor)),
            "new extension root forces topology reload"
        );

        write(
            root,
            "cfe/New/Configuration.xml",
            "<Configuration><Properties><Version>2.0</Version></Properties></Configuration>",
        );
        assert!(
            !watcher.is_project_input(&event(&descriptor)),
            "the new root descriptor hash was seeded by the structural event"
        );

        hub.unsubscribe(cursor);
        hub.shutdown();
        stop.stop();
    }

    /// The two consumers that feed ONE graph ledger from one hub: the search sink's cursor,
    /// whose dispatch the stand performs through the graph's own loss entry, and the graph
    /// watcher, drained through its production `drain`. Cap 2, so three distinct paths cut a
    /// cursor out of detail it had not drained.
    struct TwoConsumers {
        _dir: tempfile::TempDir,
        root: PathBuf,
        hub: WorkspaceChangeHub,
        graph: GraphState,
        search: SinkCursor,
        watcher: Watcher,
        paths: usize,
        marks: i64,
        _stop: OwnerStop,
    }

    const LEDGER_FACTS: super::super::debt::Facts = super::super::debt::Facts {
        ready: true,
        failed: false,
        idle: false,
        in_flight: false,
        owns: true,
        terminal: false,
        stopping: false,
    };

    impl TwoConsumers {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let hub = WorkspaceChangeHub::start_with_capacity(
                vec![crate::change_hub::WatchTarget::recursive(root.clone())],
                2,
                Duration::from_secs(3600),
            );
            assert!(hub.wait_until_watching(Duration::from_secs(5)));
            let graph = GraphState::for_workspace(root.clone()).with_change_hub(hub.clone());
            let search = hub.subscribe();
            let stop = OwnerStop::default();
            let watcher = Watcher {
                graph: graph.clone(),
                hub: hub.clone(),
                cursor: hub.subscribe(),
                advisory: AdvisoryOwner(None),
                project_roots: std::cell::RefCell::new(super::project_roots(&graph)),
                descriptor_hashes: std::cell::RefCell::new(std::collections::HashMap::new()),
                _live: stop.enter(),
                stop: stop.clone(),
            };
            Self { _dir: dir, root, hub, graph, search, watcher, paths: 0, marks: 0, _stop: stop }
        }

        /// A distinct path, recorded exactly as the backend reports a removal.
        fn one_more_path(&mut self) {
            self.paths += 1;
            self.hub
                .deliver_vanished_for_test(&self.root.join(format!("Модуль{}.bsl", self.paths)));
        }

        /// What search does with a reconcile batch it took: its marks, then the loss, recorded
        /// into the graph's ledger. Acknowledged separately, as the sink does, after the apply.
        fn search_records(&mut self, batch: &crate::change_hub::DrainBatch) {
            self.owe_marks(batch.fact_seq());
            self.graph.record_loss_quietly(batch.loss_token(), batch.fact_seq());
        }

        /// Marks placed anew for `fact`, and the owner's settlement that makes them owed.
        fn owe_marks(&mut self, fact: u64) {
            self.marks += 1;
            let now = Instant::now();
            let mut debt = super::super::state::lock_recover(&self.graph.debt);
            debt.place_marks(now, self.marks, fact);
            debt.settle_marks(now, false);
        }

        fn search_acknowledges(&mut self, batch: &crate::change_hub::DrainBatch) {
            self.hub.acknowledge(batch);
            self.search = batch.cursor;
        }

        /// The build this debt paid for ran and failed with an Operation: both accounts that
        /// sponsored it are closed until fresh work arrives.
        fn spend_and_stop(&self, fact: u64) {
            let now = Instant::now();
            let mut debt = super::super::state::lock_recover(&self.graph.debt);
            let sponsors = debt.charge_admission(fact, now, true);
            assert!(sponsors.primary && sponsors.marks, "the stand needs both accounts to pay");
            debt.record_failure(now, super::super::debt::FailureKind::Operation, sponsors);
        }

        fn failed(&self) -> Option<super::super::debt::Ripeness> {
            super::super::state::lock_recover(&self.graph.debt)
                .standing(Instant::now(), LEDGER_FACTS)
                .failed
        }

        fn assert_spent(&self, what: &str) {
            let now = Instant::now();
            let debt = super::super::state::lock_recover(&self.graph.debt);
            assert_eq!(
                debt.standing(now, LEDGER_FACTS).failed,
                Some(super::super::debt::Ripeness::Exhausted(
                    super::super::debt::Revival::FreshFact
                )),
                "{what}: the primary retry account was reopened",
            );
            assert!(!debt.marks_are_eligible(now), "{what}: the marks account was reopened");
            assert_eq!(debt.decide(now, LEDGER_FACTS).start, None, "{what}: a build was admitted");
        }

        fn is_level(&self) -> bool {
            !self.hub.drain_peek(self.search) && !self.hub.drain_peek(self.watcher.cursor)
        }
    }

    impl Drop for TwoConsumers {
        fn drop(&mut self) {
            self.hub.shutdown();
        }
    }

    /// One shared loss reaches the graph through two cursors, and a private loss of one of
    /// them lands in between: W through search, L through search, then W through the watcher.
    /// The last is the same event as the first, and it may not buy what the first already
    /// bought — however many other losses were recorded between the two.
    ///
    /// The accounts are spent and stopped after L, before W comes back: over a budget still
    /// open a repeat buys nothing anyway, and that would prove nothing about the repeat.
    #[test]
    fn a_shared_loss_arriving_again_after_a_private_one_buys_nothing() {
        let mut stand = TwoConsumers::new();
        stand.hub.deliver_backend_error_for_test();
        let w = stand.hub.materialize(stand.search);
        assert!(w.rescan_required, "the stand needs a shared window");
        stand.search_records(&w);
        stand.search_acknowledges(&w);
        stand.spend_and_stop(w.fact_seq());
        stand.assert_spent("the first W");

        // Three distinct paths past a cap of two: search, level with the stream and owing
        // nothing, is cut out and issued a loss of its own; the watcher still owes W.
        for _ in 0..3 {
            stand.one_more_path();
        }
        let l = stand.hub.materialize(stand.search);
        assert!(l.rescan_required, "the stand needs a private loss for search");
        assert_ne!(l.loss_token(), w.loss_token(), "a private loss has an identity of its own");
        stand.search_records(&l);
        assert!(
            stand.failed() == Some(super::super::debt::Ripeness::Now),
            "the stand needs L to be fresh work: {:?}",
            stand.failed(),
        );
        stand.search_acknowledges(&l);
        stand.spend_and_stop(l.fact_seq());
        stand.assert_spent("after L");
        // A delivery of the same fact is not what could reopen anything below.
        super::super::state::lock_recover(&stand.graph.debt)
            .record_change(Instant::now(), l.fact_seq());
        stand.assert_spent("the same fact delivered again");

        assert_eq!(
            stand.hub.materialize(stand.watcher.cursor).loss_token(),
            w.loss_token(),
            "the stand needs the watcher to still owe W",
        );
        stand.watcher.drain();
        stand.assert_spent("W arriving through the watcher after L");
        assert!(stand.is_level(), "the two cursors did not settle once quiet");
    }

    /// The same repeat after a publication has answered L: it reintroduced a forced reload
    /// nobody asked for again, and reset the recovery probe's earned pace to its floor.
    #[test]
    fn a_shared_loss_arriving_again_after_its_answer_reopens_no_reload_and_keeps_the_probe_pace() {
        use super::super::debt::{
            Capability, Level, ProbeReceipt, ProbeResult, RecoveryPublicationProof,
            RECOVERY_PROBE_CAP,
        };
        let mut stand = TwoConsumers::new();
        stand.hub.deliver_backend_error_for_test();
        let w = stand.hub.materialize(stand.search);
        assert!(w.rescan_required, "the stand needs a shared window");
        stand.graph.record_loss_quietly(w.loss_token(), w.fact_seq());
        stand.search_acknowledges(&w);
        for _ in 0..3 {
            stand.one_more_path();
        }
        let l = stand.hub.materialize(stand.search);
        assert!(l.rescan_required, "the stand needs a private loss for search");
        stand.graph.record_loss_quietly(l.loss_token(), l.fact_seq());
        stand.search_acknowledges(&l);

        let unread = stand.root.join("Недоступный.bsl").to_string_lossy().into_owned();
        let mut now = Instant::now();
        {
            let mut debt = super::super::state::lock_recover(&stand.graph.debt);
            let captured_seq = debt.outstanding_recovery().captured_seq;
            debt.record_publication(
                now,
                Some(l.fact_seq()),
                true,
                None,
                RecoveryPublicationProof {
                    generation: 1,
                    captured_seq,
                    declared_unread: Some(vec![unread.clone()]),
                    scan_complete: Some(true),
                    ..Default::default()
                },
            );
            assert_eq!(debt.decide(now, LEDGER_FACTS).start, None, "the publication answered L");
            for _ in 0..3 {
                now += RECOVERY_PROBE_CAP;
                let plan = debt.reserve_probe().expect("the unread path is watched");
                let result = debt.finish_probe(
                    now,
                    ProbeReceipt {
                        token: plan.token,
                        basis: plan.basis,
                        levels: vec![(Capability::Open(unread.clone()), Level::Denied)],
                        scope: None,
                    },
                );
                assert_eq!(result, ProbeResult::NoNewEvidence);
            }
            assert_eq!(
                debt.probe_interval(),
                Some(RECOVERY_PROBE_CAP),
                "the stand needs an earned pace"
            );
        }

        stand.watcher.drain();
        let debt = super::super::state::lock_recover(&stand.graph.debt);
        assert_eq!(
            debt.probe_interval(),
            Some(RECOVERY_PROBE_CAP),
            "W arriving again reset the pace the probe had earned",
        );
        assert_eq!(
            debt.decide(now, LEDGER_FACTS).start,
            None,
            "W arriving again reopened a reload the publication had answered",
        );
        drop(debt);
        assert!(stand.is_level(), "the two cursors did not settle once quiet");
    }

    /// A consumer still holding W when a newer window opens: the hub has renamed its debt, and
    /// the W in its hands is still the W the other cursor already delivered.
    #[test]
    fn a_shared_loss_in_a_consumers_hands_when_a_newer_window_opens_is_still_one_event() {
        let mut stand = TwoConsumers::new();
        stand.hub.deliver_backend_error_for_test();
        let w = stand.hub.materialize(stand.search);
        assert!(w.rescan_required, "the stand needs a shared window");
        stand.search_records(&w);
        stand.search_acknowledges(&w);
        stand.spend_and_stop(w.fact_seq());

        // The watcher takes its batch — the first step of its drain — and is held there.
        let held = stand.hub.materialize(stand.watcher.cursor);
        assert_eq!(held.loss_token(), w.loss_token(), "the stand needs the watcher holding W");

        stand.hub.deliver_backend_error_for_test();
        let x = stand.hub.materialize(stand.search);
        assert!(x.rescan_required, "the stand needs a second window");
        assert_ne!(x.loss_token(), w.loss_token(), "a second window is a second loss");
        stand.search_records(&x);
        assert_eq!(
            stand.failed(),
            Some(super::super::debt::Ripeness::Now),
            "a newer window is fresh work",
        );
        stand.search_acknowledges(&x);
        stand.spend_and_stop(x.fact_seq());
        stand.assert_spent("after X");

        // The rest of the watcher's drain, over what it holds.
        stand.watcher.apply_rescan(held.loss_token(), held.fact_seq());
        stand.hub.acknowledge(&held);
        stand.assert_spent("W, taken before X opened, recorded after it");
        stand.watcher.drain();
        stand.assert_spent("X through the watcher");
        assert!(stand.is_level(), "the two cursors did not settle once quiet");
    }

    /// Countercontrols, so the stands above cannot pass by refusing losses outright.
    ///
    /// A loss the graph has never seen revives a spent account whatever its number: a private
    /// loss issued BEFORE one already recorded, delivered after it, is still news — which is
    /// why a high-water mark of the numbers is not an identity. A repeat of the loss just
    /// recorded buys nothing, and a new shared window revives again.
    #[test]
    fn a_loss_the_graph_has_not_seen_revives_whatever_its_number() {
        let mut stand = TwoConsumers::new();
        // Search stalls while the watcher keeps up: search is cut out, loss L1.
        for _ in 0..3 {
            stand.one_more_path();
            stand.watcher.drain();
        }
        let l1 = stand.hub.materialize(stand.search);
        assert!(l1.rescan_required, "the stand needs a private loss for search");

        // Search holds L1 while the watcher stalls in turn and is cut out itself: loss L2.
        let mut cut = false;
        for _ in 0..32 {
            stand.one_more_path();
            if stand.hub.drain_peek(stand.watcher.cursor) {
                cut = true;
                break;
            }
        }
        assert!(cut, "the stand needs the watcher cut out");
        let l2 = stand.hub.materialize(stand.watcher.cursor);
        assert!(
            l2.loss_token() > l1.loss_token(),
            "the stand needs the later loss delivered first: {:?} then {:?}",
            l2.loss_token(),
            l1.loss_token(),
        );
        stand.watcher.drain();
        stand.owe_marks(l2.fact_seq());
        stand.spend_and_stop(l2.fact_seq());
        stand.assert_spent("after L2");

        stand.search_records(&l1);
        assert_eq!(
            stand.failed(),
            Some(super::super::debt::Ripeness::Now),
            "an older loss the graph had never seen was taken for a repeat",
        );
        stand.search_acknowledges(&l1);
        stand.spend_and_stop(l1.fact_seq().max(l2.fact_seq()));
        stand.assert_spent("after L1");

        // The acknowledgement above was taken against a position the cut had moved: search
        // still owes L1, and takes it again. The same loss, recorded again, buys nothing.
        let again = stand.hub.materialize(stand.search);
        if again.rescan_required {
            assert_eq!(again.loss_token(), l1.loss_token());
            stand.graph.record_loss_quietly(again.loss_token(), again.fact_seq());
            stand.assert_spent("L1 recorded twice in a row");
            stand.search_acknowledges(&again);
        }

        stand.hub.deliver_backend_error_for_test();
        stand.watcher.drain();
        assert_eq!(
            stand.failed(),
            Some(super::super::debt::Ripeness::Now),
            "a new shared window did not revive the spent account",
        );
    }

    /// What the ledger remembers of the losses it acted on stays within what the hub can still
    /// deliver, however long one consumer stays inside an old window while the other is issued
    /// loss after loss — and the old window is still one event when it finally arrives.
    #[test]
    fn remembered_losses_stay_within_what_the_hub_can_still_deliver() {
        let mut stand = TwoConsumers::new();
        stand.hub.deliver_backend_error_for_test();
        let w = stand.hub.materialize(stand.search);
        stand.search_records(&w);
        stand.search_acknowledges(&w);
        let mut most = 0;
        for round in 0..24 {
            for _ in 0..3 {
                stand.one_more_path();
            }
            let private = stand.hub.materialize(stand.search);
            assert!(private.rescan_required, "round {round}: the stand needs a private loss");
            stand.search_records(&private);
            let remembered =
                super::super::state::lock_recover(&stand.graph.debt).losses_remembered();
            let live = stand.hub.loss_horizon().live.len();
            assert!(remembered <= live + 1, "round {round}: {remembered} remembered, {live} live");
            most = most.max(remembered);
            stand.search_acknowledges(&private);
        }
        stand.spend_and_stop(stand.hub.seq());
        stand.assert_spent("after the last private loss");
        assert_eq!(
            stand.hub.materialize(stand.watcher.cursor).loss_token(),
            w.loss_token(),
            "the stand needs the watcher still inside the first window",
        );
        stand.watcher.drain();
        stand.assert_spent("the first window, arriving after 24 private losses");
        assert!(most <= 3, "the ledger remembered up to {most} losses");
        assert!(stand.is_level(), "the two cursors did not settle once quiet");
    }

    /// A retry whose window is still open keeps its schedule: neither a new loss nor a repeat
    /// of an old one resets its attempts or pulls its next attempt forward.
    #[test]
    fn an_open_retry_window_keeps_its_schedule_across_losses_and_repeats() {
        let mut stand = TwoConsumers::new();
        stand.hub.deliver_backend_error_for_test();
        let w = stand.hub.materialize(stand.search);
        stand.graph.record_loss_quietly(w.loss_token(), w.fact_seq());
        stand.search_acknowledges(&w);
        {
            let now = Instant::now();
            let mut debt = super::super::state::lock_recover(&stand.graph.debt);
            let sponsors = debt.charge_admission(w.fact_seq(), now, false);
            debt.record_failure(now, super::super::debt::FailureKind::Transient, sponsors);
            debt.record_failure(now, super::super::debt::FailureKind::Transient, sponsors);
        }
        let scheduled = stand.failed();
        assert!(
            matches!(scheduled, Some(super::super::debt::Ripeness::At(_))),
            "the stand needs a retry paced into the future: {scheduled:?}",
        );
        for _ in 0..3 {
            stand.one_more_path();
        }
        let l = stand.hub.materialize(stand.search);
        assert!(l.rescan_required, "the stand needs a private loss for search");
        stand.graph.record_loss_quietly(l.loss_token(), l.fact_seq());
        stand.search_acknowledges(&l);
        assert_eq!(stand.failed(), scheduled, "a loss reset an open retry window");
        stand.watcher.drain();
        assert_eq!(stand.failed(), scheduled, "a repeat reset an open retry window");
        assert!(stand.is_level(), "the two cursors did not settle once quiet");
    }
}
