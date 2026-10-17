use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::change_hub::{CarriedDebt, ChangeEntry, Health, SinkCursor, WorkspaceChangeHub};
use crate::graph::scan::{classify_changes, FileStat, WorkspaceDiff};
use crate::graph::universe::ScanVerdict;

use super::lifecycle::{lock_recover, DiagnosticsState, Inner};
use super::resident::apply_resident_changes;
use super::types::{DiagnosticsStatus, Freshness, ReloadState};

pub(super) const DRIFT_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// How often the drift poll re-resolves the scope's git refs (base, HEAD) to
/// notice ref-only movement. A couple of `rev-parse`s per minute is free.
pub(super) const SCOPE_REF_CHECK_INTERVAL: Duration = Duration::from_secs(60);

pub(super) const FORCE_RESCAN_FLOOR: Duration = Duration::from_millis(250);

pub(super) const RECONCILE_INTERVAL: Duration = Duration::from_secs(90);

pub(super) const MIN_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

pub(super) fn reconcile_interval() -> Duration {
    clamp_reconcile_interval(
        std::env::var("BSL_MCP_RECONCILE_SECS").ok().and_then(|s| s.parse::<u64>().ok()),
    )
}

pub(super) fn clamp_reconcile_interval(secs: Option<u64>) -> Duration {
    match secs {
        Some(0) | None => RECONCILE_INTERVAL,
        Some(secs) => Duration::from_secs(secs).max(MIN_RECONCILE_INTERVAL),
    }
}

pub(super) struct ScanCache {
    pub(super) at: Instant,
    pub(super) stats: Vec<FileStat>,
    pub(super) config_fp: u64,
    /// The baseline this snapshot is comparable against — see `Inner::baseline_epoch`.
    pub(super) baseline_epoch: u64,
    /// What the walk behind `stats` may speak for — see [`ScanVerdict`]. Cached with
    /// the rows it describes: a verdict paired with another walk's rows would vouch
    /// for a tree these rows never came from.
    pub(super) verdict: ScanVerdict,
}

impl DiagnosticsState {
    #[cfg(test)]
    pub(super) fn scan_count(&self) -> usize {
        self.scan_count.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(super) fn forced_rescans(&self) -> usize {
        self.forced_rescans.load(Ordering::SeqCst)
    }

    /// `pub(crate)`, unlike its neighbours: the handler-level gates live in `lib.rs`.
    #[cfg(test)]
    pub(crate) fn stale_miss_consultations(&self) -> usize {
        self.stale_miss_consultations.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(super) fn note_stale_miss_consultation(&self) {
        self.stale_miss_consultations.fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(super) fn has_hub_cursor(&self) -> bool {
        lock_recover(&self.hub_cursor).is_some()
    }

    #[cfg(test)]
    pub(super) fn drain_and_discard_cursor(&self) {
        let cursor = *lock_recover(&self.hub_cursor);
        if let (Some(hub), Some(cursor)) = (&self.change_hub, cursor) {
            let batch = hub.drain(cursor);
            self.commit_drained_cursor(cursor, batch.cursor);
        }
    }

    #[cfg(test)]
    pub(super) fn set_reconcile_probe(&self, f: impl FnOnce() + Send + 'static) {
        *lock_recover(&self.reconcile_probe) = Some(Box::new(f));
    }

    #[cfg(test)]
    pub(super) fn set_post_scan_probe(&self, f: impl FnOnce() + Send + 'static) {
        *lock_recover(&self.post_scan_probe) = Some(Box::new(f));
    }

    #[cfg(test)]
    pub(super) fn set_pre_drain_probe(&self, f: impl FnOnce() + Send + 'static) {
        *lock_recover(&self.pre_drain_probe) = Some(Box::new(f));
    }

    #[cfg(test)]
    pub(super) fn set_pre_force_probe(&self, f: impl FnOnce() + Send + 'static) {
        *lock_recover(&self.pre_force_probe) = Some(Box::new(f));
    }

    #[cfg(test)]
    pub(crate) fn set_failure_probe(&self, f: impl FnOnce() + Send + 'static) {
        *lock_recover(&self.failure_probe) = Some(Box::new(f));
    }

    pub(super) fn poll_drift(&self) {
        let Some(root) = self.workspace_root.clone() else {
            return;
        };
        if !matches!(self.status(), DiagnosticsStatus::Ready { .. }) {
            return;
        }

        // Ref-only git movement (fetch/rebase/reset) changes what the scope
        // should be without any watched-file event; this is the only place
        // that can notice it.
        self.check_scope_ref_drift(&root);

        // Holes are retried on EVERY window, ahead of every classifier — including the
        // forced-scan escape hatch below, which returns early. Neither classifier can
        // find them: the regular drift fingerprint tracks the observed file bytes, so a file that came
        // back readable without a delivered event is still handled by its hole list, and with a healthy hub
        // there is no scan to re-add it either. Their own list is the only mechanism
        // that heals them, so skipping it in the window that asked for the MOST
        // thorough disk reconciliation would be exactly backwards.
        self.retry_holes();

        // The `metadata object` miss escape hatch forces a scan regardless of hub health.
        if self.force_scan.swap(false, Ordering::SeqCst) {
            self.poll_drift_via_scan(&root);
            return;
        }

        // Healthy hub → event-driven drain (O(change), no scan on the hot path).
        // No hub, or a degraded one, → today's throttled scan-on-read (parity).
        //
        // Health is asked about OUR cursor: another consumer that stopped draining owes
        // its own reconcile, and answering that debt here would put these diagnostics on
        // a full scan for as long as the other one stays silent.
        let cursor = *lock_recover(&self.hub_cursor);
        match &self.change_hub {
            Some(hub)
                if self.hub_armed.load(Ordering::SeqCst)
                    && matches!(hub.health_for(cursor), Health::Healthy) =>
            {
                self.poll_drift_via_drain(hub, &root);
            }
            _ => {
                self.poll_drift_via_scan(&root);
            }
        }
    }

    /// Re-read every held hole; serve the ones that came back and drop the ones that
    /// vanished. Cheap in the normal case — the list is empty.
    fn retry_holes(&self) {
        self.retry_holes_in(None);
    }

    /// Re-read the ONE hole a request just asked about, whatever the window says.
    ///
    /// The pacing below exists because a sweep reads every hole whole; a client that repaired
    /// one file and asked for it by name pays for that file alone, and telling it for another
    /// window that the bytes cannot be read would answer about a state that has ended.
    pub(super) fn retry_hole_named(&self, key: &str) {
        {
            // Paced per file, for the reason the sweep is paced at all: a read is the WHOLE
            // file, taken under the resident's write lock, and a file that stays unreadable
            // is the steady state a hole exists for. One re-read per window per file answers
            // a repair at once and costs a client that polls an unreadable file nothing.
            let mut last = lock_recover(&self.last_named_hole_retry);
            if last.get(key).is_some_and(|at| at.elapsed() < self.drift_interval) {
                return;
            }
            // Claimed under the SAME lock that checked, because checking and then stamping
            // after the read is not one decision: two requests naming the same file in one
            // window both passed the check and both read the whole file under the resident's
            // write lock. Withdrawn below when the rebuild refuses, so the other half of the
            // rule still holds — a window is spent only on an attempt that happened.
            last.insert(key.to_owned(), Instant::now());
            // Bounded by the hole list: a path that is no longer a hole never asks again.
            last.retain(|_, at| at.elapsed() < self.drift_interval * 4);
        }
        // A retry the rebuild refuses — `reload == Running`, no resident, the path no longer a
        // hole — must not spend the window, or every later request for that file inside it is
        // skipped in silence: the one input the named retry exists for gets the answer it was
        // added to prevent.
        if !self.retry_holes_in(Some(key)) {
            lock_recover(&self.last_named_hole_retry).remove(key);
        }
    }

    /// Heal the hole a file request is about, if it is one, before the answer is built.
    ///
    /// A file held out of service answers "its bytes could not be read" and carries no
    /// findings, so a client that fixed the file and asked again would be told the same
    /// thing until the sweep's next window — about a state that has already ended. Bounded
    /// by what was asked for: at most the one file named.
    pub(crate) fn retry_unread_request_path(&self, root_id: Option<&str>, path: &Path) {
        let Some(key) = self.unread_key_for(root_id, path) else { return };
        self.retry_hole_named(&key);
    }

    /// The hole key this request names, and only when it IS a hole right now.
    ///
    /// Through the root table, exactly as the answer resolves it. Against the workspace root
    /// alone — which is what a raw relative path resolves to — a file addressed by its own
    /// root, in an extension or a nested source root, named a path nobody holds: no hole was
    /// found, nothing was re-read, and the request waited out the sweep it was meant to skip.
    fn unread_key_for(&self, root_id: Option<&str>, path: &Path) -> Option<String> {
        let inner = lock_recover(&self.inner);
        let resident = inner.resident.as_ref()?;
        let resolved = resident.resolve_rooted_path(root_id, path).ok()?;
        resident.hole_key_of(&resolved)
    }

    /// Says whether a re-read was ATTEMPTED, so a caller pacing itself pays for a retry that
    /// happened rather than for one that was refused before it began.
    fn retry_holes_in(&self, only: Option<&str>) -> bool {
        let admits = {
            let inner = lock_recover(&self.inner);
            let Some(resident) = inner.resident.as_ref() else { return false };
            if resident.holes.is_empty() {
                return false;
            }
            if only.is_some_and(|key| !resident.holes.contains_key(key)) {
                return false;
            }
            resident.holes.values().any(|o| *o == super::resident::HoleOrigin::Pending)
        };
        // Throttled on the same interval as the scan beside it, and for the same
        // reason. A retry is NOT cheap: `read_disk_text` is `read_to_string`, so an
        // unreadable file is read WHOLE into memory before its bytes fail the UTF-8
        // check, and the cost is paid per hole. `poll_drift` runs on every request
        // (`diagnostics file`, `workspace`, `symbol_info`, the search prefetch's
        // `catch_up`), so an unthrottled retry would put a full re-read of every hole
        // on the hot path of a workspace whose normal state, for this node, is holes.
        // Healing is not urgent — the next window is soon enough.
        if only.is_none() {
            let last = lock_recover(&self.last_hole_retry);
            if last.is_some_and(|at| at.elapsed() < self.drift_interval) {
                return false;
            }
        }
        // Asked BEFORE the lock, like the drain does: `resident_config_is_current`
        // takes the same mutex, and asking it under `apply` would deadlock rather
        // than refuse. Asked ONLY when something can be admitted — it parses the
        // config and rediscovers extensions, and `poll_drift` runs on every read
        // (the scan beside it is throttled precisely because of this cost).
        // Returning a file whose admission was already asserted needs no gate.
        //
        // "Nothing to admit" therefore answers NO, not yes. The lock is released
        // between the sample and the apply, so a concurrent request can open a
        // `Pending` hole in the gap — and a gate that was never asked must not let it
        // through. Admitted holes ignore this answer, so they still heal; a newborn
        // waits for the next window, which does ask.
        let config_is_current = admits && self.resident_config_is_current();

        let mut rescope: Option<(PathBuf, String)> = None;
        {
            let mut inner = lock_recover(&self.inner);
            if inner.reload == ReloadState::Running {
                // Refused before any file was read, so nothing is paced: see the caller.
                return false;
            }
            let Inner {
                resident: Some(resident), stats, generation, baseline_epoch, status, ..
            } = &mut *inner
            else {
                return false;
            };
            // Past EVERY refusal, the resident included: the lock is released between the
            // sample above and this one, so the idle sweeper can evict the resident in the
            // gap. Stamped before this check, that eviction spent the window for a re-read
            // that never happened, and every request inside it was skipped in silence.
            if only.is_none() {
                *lock_recover(&self.last_hole_retry) = Some(Instant::now());
            }
            let (healed, vanished) =
                super::resident::retry_resident_holes(resident, config_is_current, only);
            if healed.is_empty() && vanished.is_empty() {
                // Read and found nothing changed: an attempt all the same, and the pacing it
                // earns is the whole point — an unreadable file is the steady state.
                return true;
            }
            // Healing changed the bytes this file serves, so its baseline entry moves
            // with it — to the fingerprint taken BEFORE the read, the one describing
            // the text actually applied.
            for (key, fp) in &healed {
                match fp {
                    Some(fp) => {
                        stats.insert(key.clone(), *fp);
                    }
                    None => {
                        stats.remove(key);
                    }
                }
            }
            // Gone: the key goes UNCONDITIONALLY. Re-stating it here would restore an
            // entry for a file just taken out of service — and a recreation matching
            // it would count as neither added nor modified, never coming back.
            for key in &vanished {
                stats.remove(key);
            }
            // Both transitions move the served file universe exactly as add/remove do,
            // so the observable count moves with them.
            *status = DiagnosticsStatus::Ready { files: resident.by_path.len() };
            // `stats` moved, so the epoch moves with it — the documented contract of
            // the field. Without it a scan snapshot taken before this point still
            // passes the epoch check and overwrites the baseline back to its
            // pre-heal state, re-reading the same file every window.
            *baseline_epoch += 1;
            *generation += 1;
            // The vendor-diff scope was computed against the working copy this heal
            // just changed; both existing appliers recompute for the same reason.
            if let Some(base) = resident.diff_base.clone() {
                rescope = Some((resident.workspace_root().to_path_buf(), base));
            }
        }
        // Held a stale view of the disk; the next scan must not be compared against it.
        *lock_recover(&self.scan) = None;
        if let Some((root, base)) = rescope {
            self.rescope_out_of_lock(&root, &base);
        }
        true
    }

    fn poll_drift_via_scan(&self, root: &Path) -> bool {
        let Some(scan) = self.throttled_scan(root) else {
            return false;
        };

        // Diff under a short read lock against the last-applied stats.
        let (changes, config_changed) = {
            let inner = lock_recover(&self.inner);
            scan_drift(&inner, &scan)
        };
        self.apply_scan_drift(&changes, config_changed, &scan)
    }

    /// Refuse a snapshot the baseline has outrun. Says whether it was refused.
    ///
    /// A scan is a snapshot of disk PAIRED with the baseline it was meant to be compared
    /// against; once that baseline moves, the two describe different worlds and their diff
    /// runs backwards — a file added since the snapshot was taken reads as a deletion (Ф12).
    /// Checked before ANY of the snapshot's drift is interpreted, config edits included: the
    /// rebuild a stale `config_changed` would ask for is built over a tree this walk never
    /// read, so answering it here would be acting on a world that no longer exists.
    ///
    /// The refusal RE-ARMS rather than merely dropping. The reason this scan ran — a
    /// reconcile debt the drain already cleared, a consumed force — is spent by then, and a
    /// healthy cursor sends every following read back down the drain path; without re-arming,
    /// the drift the hub lost would sit unapplied, and unreported, until the watchdog tick.
    /// The snapshot itself goes too: `throttled_scan` caches unconditionally, so one taken
    /// before the move would otherwise be handed to every read until it cools, each one
    /// refusing it again — and the epoch only grows, so it can never become applicable.
    fn refuse_scan_if_stale(&self, scan: &OwnedScan) -> bool {
        let baseline = lock_recover(&self.inner).baseline_epoch;
        if scan.baseline_epoch == baseline {
            return false;
        }
        tracing::debug!(
            snapshot = scan.baseline_epoch,
            baseline,
            "dropping a scan whose baseline moved under it; the next poll rescans"
        );
        self.force_scan.store(true, Ordering::SeqCst);
        *lock_recover(&self.scan) = None;
        true
    }

    fn apply_scan_drift(
        &self,
        changes: &WorkspaceDiff,
        config_changed: bool,
        scan: &OwnedScan,
    ) -> bool {
        if changes.is_empty() && !config_changed {
            return false;
        }

        // A snapshot the baseline has outrun speaks for another world, so none of its drift
        // — the config edit included — can be applied to this one.
        if self.refuse_scan_if_stale(scan) {
            return true;
        }

        // Only an analyzer-config edit forces a full rebuild (it can change the
        // extension set and the effective diagnostics config). Everything else — any
        // `.xml` add/remove/edit, `.bsl` body edits, AND `.bsl` files appearing or
        // vanishing — is reconciled into the live resident in place. A removed subtree
        // needs no special case here: the scan diff enumerates every descendant.
        // The same question the drain asks before admitting a file, answered here from the
        // snapshot already in hand: `config_changed` IS "the configuration the resident
        // serves is not the one on disk". Deriving it in both places, rather than raising a
        // flag in one and reading it in the other, is what keeps the two appliers agreeing
        // about a state neither of them owns.
        let config_pending = config_changed && !scan.verdict.clean();
        if config_changed {
            // The rebuild this asks for would be built over the same tree this scan just
            // failed to read whole, and `run_reload` declines to swap such a build in. Not
            // asking for it at all is what keeps that refusal cheap: the config edit stays
            // on disk, so every window would otherwise pay a full resident build only to
            // throw it away. The edit lands on the first window whose scan reads whole.
            //
            // Postponing the rebuild does NOT postpone the rest of this scan's drift: the
            // usual reason to return here is that the rebuild answers everything, and a
            // rebuild that will not happen answers nothing. Body edits keep landing.
            if scan.verdict.clean() {
                self.kick_full_reload();
                return true;
            }
            tracing::debug!(
                "postponing the config rebuild: this scan could not read the tree whole"
            );
        }

        // XML drift spans all three buckets: an added/removed object is a structural
        // listing change, an edited one a content change — the substrate refresh handles
        // all of them by re-discovery + re-read of changed/new composing files.
        let xml_paths: Vec<PathBuf> = changes
            .added
            .iter()
            .filter(|_| !config_pending)
            .chain(&changes.removed)
            .chain(&changes.modified)
            .filter(|p| bsl_conventions::str_has_extension(p, bsl_conventions::XML_EXTENSION))
            .map(PathBuf::from)
            .collect();
        // A file is ADMITTED to the resident by an addition, and admission is a claim that
        // this file belongs to the configuration the resident serves. While the rebuild
        // that would adopt the edited configuration is postponed, that claim cannot be
        // made: the path may be new precisely because the postponed edit added the root it
        // lies in. Refreshing files the resident already holds is a different act and
        // continues. Nothing is lost by waiting — an unapplied addition is not recorded in
        // the baseline, so it is offered again by every later scan.
        let added_bsl: Vec<String> = if config_pending {
            Vec::new()
        } else {
            changes
                .added
                .iter()
                .filter(|p| !bsl_conventions::str_has_extension(p, bsl_conventions::XML_EXTENSION))
                .cloned()
                .collect()
        };
        let modified_bsl: Vec<String> = changes
            .modified
            .iter()
            .filter(|p| !bsl_conventions::str_has_extension(p, bsl_conventions::XML_EXTENSION))
            .cloned()
            .collect();
        let removed_bsl: Vec<String> = changes
            .removed
            .iter()
            .filter(|p| !bsl_conventions::str_has_extension(p, bsl_conventions::XML_EXTENSION))
            .cloned()
            .collect();
        self.apply_metadata_and_body_drift(
            &xml_paths,
            &added_bsl,
            &modified_bsl,
            &removed_bsl,
            scan,
        );
        true
    }

    fn poll_drift_via_drain(&self, hub: &WorkspaceChangeHub, root: &Path) {
        // A full rebuild in flight will publish a fresh resident whose baseline scan already
        // reflects disk, and `apply_drained_resident` defers to it (bails on `Running`).
        // Draining now would advance the cursor past events the apply then drops — the
        // resident would miss the whole rebuild window. Leave them pending: the reload
        // re-subscribes a fresh cursor at its start, and the next poll after it finishes
        // drains that window onto the new resident. Mirrors the scan path, which bails
        // without rebasing its baseline so the drift is re-detected.
        if lock_recover(&self.inner).reload == ReloadState::Running {
            return;
        }
        let Some(cursor) = *lock_recover(&self.hub_cursor) else {
            // Ready but no cursor yet (a read racing the build's subscribe): reconcile via
            // scan this once; the next poll uses the cursor.
            self.poll_drift_via_scan(root);
            return;
        };
        #[cfg(test)]
        self.fire_pre_drain_probe();

        let batch = hub.drain(cursor);
        self.commit_drained_cursor(cursor, batch.cursor);
        // A debt raised between the health decision above and this drain: the scan covers
        // what the hub dropped, and the entries cover what the scan's cache is too old to
        // see — this path does not force a fresh walk, so a cache younger than the drift
        // interval answers with a snapshot taken before these very entries.
        if batch.rescan_required {
            self.poll_drift_via_scan(root);
            if lock_recover(&self.inner).reload == ReloadState::Running {
                return;
            }
        }
        if batch.entries.is_empty() {
            return;
        }
        self.apply_drained_entries(&batch.entries);
    }

    pub(super) fn apply_drained_entries(&self, entries: &[ChangeEntry]) {
        let baseline: HashMap<String, u64> = lock_recover(&self.inner).stats.clone();
        // The analyzer config files fingerprinted by `config_files_fingerprint` — canonicalised
        // to match the drained key spelling. Only a file at THIS exact location is config
        // drift; an identically-named file elsewhere in the tree is not (parity with the
        // scan path, which fingerprints only `root.join(name)`).
        let config_paths = self.config_file_paths();

        let class = crate::drift_classify::classify_drift(entries, &config_paths, Some(&baseline));

        // A config edit changes the effective diagnostics/extension setup and a removed
        // subtree hides descendants the drain could not enumerate — neither is
        // expressible in place. Everything else (xml, and `.bsl` added / modified /
        // removed) reconciles into the live resident.
        if class.config_changed || class.structural_rescan {
            self.kick_full_reload();
            return;
        }
        if class.xml_paths.is_empty()
            && class.bsl_modified.is_empty()
            && class.bsl_added.is_empty()
            && class.bsl_removed.is_empty()
        {
            return;
        }
        let mut added_bsl: Vec<String> = class.bsl_added.iter().map(|d| d.key.clone()).collect();
        let modified_bsl: Vec<String> = class.bsl_modified.iter().map(|d| d.key.clone()).collect();
        let removed_bsl: Vec<String> = class.bsl_removed.iter().map(|d| d.key.clone()).collect();
        let mut new_fp = class.new_fp;
        // The drain reports `.xml` drift in one bucket whatever its direction, so a
        // descriptor being ADMITTED is told from one being edited or removed the only way
        // available here: by whether the baseline ever held its key.
        let mut xml_keys: Vec<&str> = class.xml_paths.iter().map(|d| d.key.as_str()).collect();
        let admits_a_descriptor = xml_keys.iter().any(|key| !baseline.contains_key(*key));
        // Admitting a file asserts it belongs to the configuration this resident serves,
        // and the question is asked here rather than remembered from wherever a rebuild
        // was last postponed: this path never learns that one was. Costs a config parse
        // and an extension discovery — no tree walk — and only when there is something to
        // admit. Their fingerprints go with them: recording a key we did not apply would
        // turn the wait into a cancellation.
        if (!added_bsl.is_empty() || admits_a_descriptor) && !self.resident_config_is_current() {
            tracing::debug!(
                bodies = added_bsl.len(),
                "holding new files back: the resident's configuration is out of date"
            );
            for key in &added_bsl {
                new_fp.remove(key);
            }
            added_bsl.clear();
            xml_keys.retain(|key| baseline.contains_key(*key));

            new_fp.retain(|key, _| baseline.contains_key(key) || modified_bsl.contains(key));
        }
        let xml_paths: Vec<PathBuf> = xml_keys.into_iter().map(PathBuf::from).collect();
        self.apply_drained_resident(
            &xml_paths,
            &added_bsl,
            &modified_bsl,
            &removed_bsl,
            &class.removed_keys,
            &new_fp,
        );
        // The baseline just moved, which leaves every cached snapshot describing the world
        // before the move. Dropping the cache is what stops the next scan within the drift
        // interval from being compared against a baseline it predates.
        *lock_recover(&self.scan) = None;
    }

    /// Whether the resident is serving the configuration that is on disk right now.
    ///
    /// Asked, never remembered. A postponed rebuild leaves no mark anywhere, and a flag
    /// raised where it was postponed would have to be lowered by every path that later
    /// adopts a configuration — including the ones that never learn a postponement
    /// happened. Derived, the answer cannot go stale and cannot differ between the two
    /// appliers.
    fn resident_config_is_current(&self) -> bool {
        let Some(root) = self.workspace_root.as_deref() else {
            return true;
        };
        let live = config_identity_now(root);
        lock_recover(&self.inner).config_fp == live
    }

    fn apply_drained_resident(
        &self,
        xml_paths: &[PathBuf],
        added_bsl: &[String],
        modified_bsl: &[String],
        removed_bsl: &[String],
        removed_keys: &[String],
        new_fp: &HashMap<String, u64>,
    ) {
        let mut needs_rebuild = false;
        let mut rescope: Option<(PathBuf, String)> = None;
        {
            let mut inner = lock_recover(&self.inner);
            if inner.reload == ReloadState::Running {
                return;
            }
            let Inner {
                resident: Some(resident), stats, generation, status, baseline_epoch, ..
            } = &mut *inner
            else {
                return;
            };
            let (rebuild, moved) = apply_resident_changes(
                resident,
                xml_paths,
                added_bsl,
                modified_bsl,
                removed_bsl,
                |p| new_fp.get(p).copied(),
                stats,
            );
            if rebuild {
                needs_rebuild = true;
            } else {
                // Body drift moved the working copy the vendor-diff scope was
                // computed against (save, checkout, pull). The git diff can take
                // seconds, so only capture the inputs here — the recompute runs
                // after this lock is released. No-op without a configured base.
                if !added_bsl.is_empty() || !modified_bsl.is_empty() || !removed_bsl.is_empty() {
                    if let Some(base) = resident.diff_base.clone() {
                        rescope = Some((resident.workspace_root().to_path_buf(), base));
                    }
                }
                for (key, fp) in new_fp {
                    stats.insert(key.clone(), *fp);
                }
                for key in removed_keys {
                    stats.remove(key);
                }
                *baseline_epoch += 1;
                if moved {
                    *generation += 1;
                    // An add/remove changed the served file universe; keep the
                    // observable `Ready { files }` count truthful.
                    *status = DiagnosticsStatus::Ready { files: resident.by_path.len() };
                    tracing::info!(
                        xml = xml_paths.len(),
                        added = added_bsl.len(),
                        bodies = modified_bsl.len(),
                        removed = removed_bsl.len(),
                        generation = *generation,
                        "diagnostics event-driven drift refresh",
                    );
                }
            }
        }
        if needs_rebuild {
            self.kick_full_reload();
        } else if let Some((root, base)) = rescope {
            self.rescope_out_of_lock(&root, &base);
        }
    }

    fn apply_metadata_and_body_drift(
        &self,
        xml_paths: &[PathBuf],
        added_bsl: &[String],
        modified_bsl: &[String],
        removed_bsl: &[String],
        scan: &OwnedScan,
    ) {
        let new_fp: HashMap<&str, u64> =
            scan.stats.iter().map(|s| (s.path.as_str(), s.fingerprint())).collect();

        let mut needs_rebuild = false;
        let mut rescope: Option<(PathBuf, String)> = None;
        {
            let mut inner = lock_recover(&self.inner);
            // A full rebuild already in flight will publish a fresh resident; defer to it
            // rather than mutating a resident that is about to be replaced.
            if inner.reload == ReloadState::Running {
                return;
            }
            // Another caller may have reconciled this exact scan already (both passed the
            // throttle, then serialised here); bail so we neither re-walk the roots nor
            // double-bump the generation.
            let (residual, config_moved) = scan_drift(&inner, scan);
            if residual.is_empty() && !config_moved {
                return;
            }
            // The baseline moved after this snapshot was taken, so the two describe
            // different worlds and their diff runs backwards (Ф12): a file added since
            // would be applied as a deletion. Checked HERE, under the lock that does the
            // applying — a check at the caller leaves the window between the two open, and
            // there is more than one caller.
            if scan.baseline_epoch != inner.baseline_epoch {
                tracing::debug!(
                    snapshot = scan.baseline_epoch,
                    baseline = inner.baseline_epoch,
                    "dropping a scan whose baseline moved under it; the next poll rescans"
                );
                // The reason this scan ran is one-shot — a reconcile debt cleared by the
                // drain that raised it, or a forced rescan already consumed — and refusing
                // the snapshot answers none of it. Without re-arming, a healthy cursor
                // sends every following read back down the drain path and the drift the hub
                // lost stays unapplied until the watchdog tick, with `stale` reading false.
                self.force_scan.store(true, Ordering::SeqCst);
                // And drop the snapshot itself: `throttled_scan` caches unconditionally, so
                // a snapshot taken before the move is cached after it and would be handed
                // to every read until it cools — each one refusing it again. The epoch only
                // grows, so this one can never become applicable.
                drop(inner);
                *lock_recover(&self.scan) = None;
                return;
            }
            let Inner {
                resident: Some(resident), stats, generation, status, baseline_epoch, ..
            } = &mut *inner
            else {
                return;
            };
            let (rebuild, moved) = apply_resident_changes(
                resident,
                xml_paths,
                added_bsl,
                modified_bsl,
                removed_bsl,
                |p| new_fp.get(p).copied(),
                stats,
            );
            if rebuild {
                needs_rebuild = true;
            } else {
                // Body drift moved the working copy the vendor-diff scope was
                // computed against (save, checkout, pull). The git diff can take
                // seconds, so only capture the inputs here — the recompute runs
                // after this lock is released. No-op without a configured base.
                if !added_bsl.is_empty() || !modified_bsl.is_empty() || !removed_bsl.is_empty() {
                    if let Some(base) = resident.diff_base.clone() {
                        rescope = Some((resident.workspace_root().to_path_buf(), base));
                    }
                }
                // Advance the drift baseline to the scan we reconciled against: every
                // applied body and every XML add/remove/edit is now reflected in the
                // resident, so its state equals `scan`. Rebasing even when nothing moved
                // (a pure mtime touch with unchanged content) stops us re-scanning it
                // every window.
                //
                // Only a scan that may speak for the whole tree replaces the baseline
                // outright; one that may not merges into it, so the keys it was not
                // entitled to call gone survive to be compared against the next scan.
                let baseline_moved = if scan.verdict.clean() {
                    *stats = scan.stats.iter().map(|s| (s.path.clone(), s.fingerprint())).collect();
                    true
                } else {
                    let applied = xml_paths
                        .iter()
                        .filter_map(|p| p.to_str())
                        .chain(added_bsl.iter().map(String::as_str))
                        .chain(modified_bsl.iter().map(String::as_str));
                    rebase_over_incomplete_scan(stats, applied, &new_fp)
                };
                if baseline_moved {
                    *baseline_epoch += 1;
                }
                if moved {
                    *generation += 1;
                    // An add/remove changed the served file universe; keep the
                    // observable `Ready { files }` count truthful.
                    *status = DiagnosticsStatus::Ready { files: resident.by_path.len() };
                    tracing::info!(
                        xml = xml_paths.len(),
                        added = added_bsl.len(),
                        bodies = modified_bsl.len(),
                        removed = removed_bsl.len(),
                        generation = *generation,
                        "diagnostics metadata drift refresh",
                    );
                }
            }
        }
        if needs_rebuild {
            self.kick_full_reload();
        } else if let Some((root, base)) = rescope {
            self.rescope_out_of_lock(&root, &base);
        }
    }

    /// Recompute the vendor-diff scope OUTSIDE the resident lock (the git diff
    /// can take seconds on a large working copy) and publish it under a short
    /// lock — unless a full reload started meanwhile: the rebuilt resident
    /// computes its own fresh scope.
    fn rescope_out_of_lock(&self, root: &Path, base: &str) {
        let (scope, identity) = super::resident::build_scope(root, base);
        let mut inner = lock_recover(&self.inner);
        if inner.reload == ReloadState::Running {
            return;
        }
        if let Some(resident) = inner.resident.as_mut() {
            resident.config.scope = scope;
            resident.scope_identity = identity;
        }
    }

    /// Compare the scope's resolved (base, HEAD) OIDs — and the author
    /// filter's pinned HEAD — against the live refs, throttled off the request
    /// hot path, and rebuild whichever went stale.
    fn check_scope_ref_drift(&self, root: &Path) {
        {
            let mut at = lock_recover(&self.scope_ref_check_at);
            if at.is_some_and(|t| t.elapsed() < SCOPE_REF_CHECK_INTERVAL) {
                return;
            }
            *at = Some(Instant::now());
        }
        let (base, stored, author_identity, ignored_authors, generation) = {
            let inner = lock_recover(&self.inner);
            let Some(resident) = inner.resident.as_ref() else { return };
            (
                resident.diff_base.clone(),
                resident.scope_identity.clone(),
                resident.author_filter.as_ref().map(|f| (f.head_identity(), f.mailmap_fp())),
                resident.ignored_authors.clone(),
                inner.generation,
            )
        };

        if let Some(base) = base {
            if let Ok(identity) = vcs::scope_ref_identity(root, &base) {
                if stored.as_ref().is_some_and(|s| s != &identity) {
                    tracing::info!(
                        "scope refs moved without file events; rebuilding vendor-diff scope"
                    );
                    self.rescope_out_of_lock(root, &base);
                }
            }
        }

        if author_filter_rebuild_needed(
            &ignored_authors,
            author_identity.as_ref().map(|(head, mm)| (head.as_str(), *mm)),
            vcs::head_identity(root).ok().as_deref(),
            vcs::mailmap_fingerprint(root),
        ) {
            tracing::info!("attribution inputs moved; rebuilding the ignored-authors filter");
            // Rebuild outside the lock (repo open + ref resolve), publish
            // under a short one — unless the resident changed meanwhile (a
            // reload or drift apply bumped the generation): the fresh resident
            // carries or will recompute its own filter, and a stale publish
            // here could overwrite it.
            let filter = super::resident::build_author_filter(root, &ignored_authors);
            let mut inner = lock_recover(&self.inner);
            if inner.reload == ReloadState::Running || inner.generation != generation {
                return;
            }
            if let Some(resident) = inner.resident.as_mut() {
                resident.author_filter = filter;
            }
        }
    }

    /// Reports what the storm guard owes: `None` when the force armed, `Some(remaining)`
    /// when the floor declined it and by how much it is still short.
    ///
    /// The number is the whole difference between "the scan was asked for" and "the scan
    /// happened". A caller that only wants the next poll routed through the disk may drop
    /// it; a caller whose ANSWER depends on the walk cannot, because a declined force
    /// leaves the read exactly as blind as it was — see
    /// [`ResidentSession::read_retrying_a_stale_miss`](super::session::ResidentSession::read_retrying_a_stale_miss).
    pub(crate) fn force_rescan(&self) -> Option<Duration> {
        #[cfg(test)]
        self.forced_rescans.fetch_add(1, Ordering::SeqCst);
        let mut cache = lock_recover(&self.scan);
        let owed = cache
            .as_ref()
            .and_then(|c| FORCE_RESCAN_FLOOR.checked_sub(c.at.elapsed()))
            .filter(|remaining| !remaining.is_zero());
        if owed.is_some() {
            return owed;
        }
        *cache = None;
        // Route the next poll through the scan even when the hub is healthy: the
        // event path would not re-observe an object the caller thinks it just added.
        self.force_scan.store(true, Ordering::SeqCst);
        None
    }

    /// Re-subscribe this state's hub cursor, dropping any prior one, and hand back the debt
    /// the old cursor's re-subscription carried onto the new one — the token a successful
    /// rebuild hands to [`Self::settle_rebuild_debt`].
    ///
    /// `None` when there is no hub, or when the cursor owed nothing to carry.
    pub(super) fn resubscribe_cursor(&self) -> Option<(SinkCursor, CarriedDebt)> {
        let hub = self.change_hub.as_ref()?;
        // Whether the stream was already covering the tree when this resident is about to
        // read it. A resident built while the watch is still arming derives its state from
        // a tree nobody was watching, and no event carries the window in between: until one
        // scan runs under an armed watch, this one keeps its own eyes on disk.
        self.hub_armed.store(hub.is_watching(), Ordering::SeqCst);
        let mut slot = lock_recover(&self.hub_cursor);
        let (cursor, carried) = match slot.take() {
            // The rebuild this precedes can fail, and a failed rebuild leaves the OLD
            // resident serving. Its outstanding reconcile has to survive the swap, or it
            // would be settled against a baseline that was never taken.
            Some(old) => hub.resubscribe(old),
            None => (hub.subscribe(), None),
        };
        *slot = Some(cursor);
        carried.map(|carried| (cursor, carried))
    }

    /// Settle the debt a SUCCESSFUL rebuild answered, under the pair it was carried with —
    /// and only against the cursor that rebuild itself took.
    ///
    /// Between the re-subscription and this call the slot can have moved: an eviction drops
    /// the cursor, and a newer rebuild replaces it. Cursor ids are never reused, so identity
    /// is the whole check — the same rule [`Self::commit_drained_cursor`] applies to a
    /// drain. Settling under a cursor that is no longer there would be answering a reconcile
    /// on behalf of a subscription nobody holds. The pair travels as the carry captured it:
    /// the hub refuses the settle if the cursor's loss was renamed or its position was cut in
    /// the meantime, and nothing here may widen that refusal into a settle.
    ///
    /// Called BEFORE [`Self::ensure_hub_roots`]: a re-arm can open a fresh window, and that
    /// window is not the one this rebuild answered.
    pub(super) fn settle_rebuild_debt(&self, carried: Option<(SinkCursor, CarriedDebt)>) {
        let Some((cursor, carried)) = carried else {
            return;
        };
        let Some(hub) = &self.change_hub else {
            return;
        };
        if *lock_recover(&self.hub_cursor) != Some(cursor) {
            return;
        }
        // A refusal is not an error and must not be widened into one: the hub pays only the
        // exact pair the carry captured, and by the time the rebuild succeeded the cursor may
        // have been renamed by a newer window or cut by `make_room`. The debt survives and the
        // next guard pass answers it; logging the pair here is what keeps a refusal that
        // silently reverts a read to the slow path from being invisible in the log.
        if !hub.settle_carried_debt(cursor, &carried) {
            tracing::debug!(
                ?carried,
                "rebuild did not settle the carried debt; the cursor no longer holds the pair",
            );
        }
    }

    fn config_file_paths(&self) -> std::collections::HashSet<PathBuf> {
        let Some(root) = self.workspace_root.as_deref() else {
            return std::collections::HashSet::new();
        };
        project_model::PROJECT_INPUT_FILE_NAMES
            .iter()
            .map(|name| {
                let path = root.join(name);
                path.canonicalize().unwrap_or(path)
            })
            .collect()
    }

    /// Write a drained cursor back only if the slot still holds the SAME subscription.
    ///
    /// A drain reads the cursor, advances it at the hub and stores it again, and an
    /// eviction or a rebuild's resubscribe can land in between. An unconditional store
    /// would then undo them, leaving an `Idle` resident holding a handle to a
    /// subscription that no longer exists. Cursor ids are never reused, so identity is
    /// the whole check.
    fn commit_drained_cursor(&self, drained: SinkCursor, advanced: SinkCursor) {
        let mut slot = lock_recover(&self.hub_cursor);
        if *slot == Some(drained) {
            *slot = Some(advanced);
        }
    }

    pub(super) fn drop_cursor(&self) {
        let Some(hub) = &self.change_hub else {
            return;
        };
        let mut slot = lock_recover(&self.hub_cursor);
        if let Some(old) = slot.take() {
            hub.unsubscribe(old);
        }
    }

    fn drain_delivered_paths(&self, hub: &WorkspaceChangeHub, root: &Path) -> DeliveredPaths {
        let cursor = *lock_recover(&self.hub_cursor);
        let Some(cursor) = cursor else {
            return DeliveredPaths::default();
        };
        let batch = hub.drain(cursor);
        self.commit_drained_cursor(cursor, batch.cursor);
        // A demand to reconcile is not a retraction of the paths in the same batch: the hub
        // keeps its entries on every input but a channel overflow, and a path it handed over
        // was handed over whatever else it also asked for. The scan still runs — the entries
        // are a SUBSET of the truth, not all of it.
        if batch.rescan_required {
            self.poll_drift_via_scan(root);
        }
        let delivered = DeliveredPaths::of(&batch.entries);
        if !batch.entries.is_empty() {
            // The scan above compares against a cache that may predate this batch, so it can
            // leave a delivered path unapplied; applying the entries covers exactly those.
            self.apply_drained_entries(&batch.entries);
        }
        delivered
    }

    pub(super) fn reconcile_tick(&self) {
        let Some(root) = self.workspace_root.clone() else {
            return;
        };
        let Some(hub) = self.change_hub.clone() else {
            return;
        };
        if !matches!(self.status(), DiagnosticsStatus::Ready { .. }) {
            return;
        }

        // 0. Retry the holes, ahead of every classifier, exactly as `poll_drift` does.
        //    A hole is invisible to both of them — the regular fingerprint only records
        //    bytes for files that were readable, so a file that came back readable without
        //    an event is still handled by its hole list — and
        //    this is the only path a daemon nobody queries ever walks. Without it a
        //    client that polls `diagnostics status` alone (which reads the state
        //    directly and never drifts) would watch `unread_files` stay at 1 forever.
        self.retry_holes();

        // 1. Apply everything the hub delivered so far. Draining also clears this cursor's
        //    reconcile flag, so a degraded hub recovers once the scan below is clean. The
        //    delivered paths are remembered so they are not later mistaken for a miss.
        let mut delivered = self.drain_delivered_paths(&hub, &root);

        // A delivered structural change kicked a full rebuild that will re-baseline the
        // whole workspace and re-subscribe a fresh cursor; nothing more to reconcile here.
        if lock_recover(&self.inner).reload == ReloadState::Running {
            return;
        }

        #[cfg(test)]
        self.fire_reconcile_probe();

        // 2. Fresh scan: classify the drift the events did not cover (do not apply yet).
        *lock_recover(&self.scan) = None;
        let Some(scan) = self.throttled_scan(&root) else {
            return;
        };
        let (changes, config_changed) = {
            let inner = lock_recover(&self.inner);
            scan_drift(&inner, &scan)
        };
        if changes.is_empty() && !config_changed {
            return;
        }

        #[cfg(test)]
        self.fire_post_scan_probe();

        // 3. A legitimate edit may have landed AFTER step 1's drain but DURING the scan
        //    above. Drain once more: the paths it now delivers were merely late, not missed.
        delivered.extend(self.drain_delivered_paths(&hub, &root));

        // 4. Apply the residual drift (the just-delivered paths now match and are skipped).
        self.apply_scan_drift(&changes, config_changed, &scan);

        // 5. Degrade only if a FILE change (bsl/xml) was genuinely undelivered. Config drift
        //    is already fully rebuilt above and is expected to reach the reconciler in nested
        //    layouts (the config file sits above the watched root), so it is not a miss.
        let missed = has_undelivered_drift(&changes, &delivered);
        if missed {
            tracing::warn!(
                "diagnostics reconciler found drift the change hub did not deliver; \
                 degrading to scan-on-read until the watcher recovers"
            );
            hub.degrade_external();
        }
    }

    #[cfg(test)]
    fn fire_reconcile_probe(&self) {
        let probe = lock_recover(&self.reconcile_probe).take();
        if let Some(probe) = probe {
            probe();
        }
    }

    #[cfg(test)]
    fn fire_post_scan_probe(&self) {
        let probe = lock_recover(&self.post_scan_probe).take();
        if let Some(probe) = probe {
            probe();
        }
    }

    #[cfg(test)]
    pub(super) fn fire_pre_force_probe(&self) {
        let probe = lock_recover(&self.pre_force_probe).take();
        if let Some(probe) = probe {
            probe();
        }
    }

    /// Fire the failure snapshot's one-shot seam, if a test armed one — see
    /// [`Inner::failure_probe`].
    #[cfg(test)]
    pub(super) fn fire_failure_probe(&self) {
        let probe = lock_recover(&self.failure_probe).take();
        if let Some(probe) = probe {
            probe();
        }
    }

    #[cfg(test)]
    pub(super) fn set_load_start_probe(&self, f: impl FnOnce() + Send + 'static) {
        *lock_recover(&self.load_start_probe) = Some(Box::new(f));
    }

    /// Fire the loader's start seam, if a test armed one — see [`Inner::load_start_probe`].
    #[cfg(test)]
    pub(super) fn fire_load_start_probe(&self) {
        let probe = lock_recover(&self.load_start_probe).take();
        if let Some(probe) = probe {
            probe();
        }
    }

    #[cfg(test)]
    fn fire_pre_drain_probe(&self) {
        let probe = lock_recover(&self.pre_drain_probe).take();
        if let Some(probe) = probe {
            probe();
        }
    }

    pub(super) fn throttled_scan(&self, root: &Path) -> Option<OwnedScan> {
        let mut cache = lock_recover(&self.scan);
        if let Some(c) = cache.as_ref() {
            if c.at.elapsed() < self.drift_interval {
                return Some(OwnedScan {
                    stats: c.stats.clone(),
                    config_fp: c.config_fp,
                    baseline_epoch: c.baseline_epoch,
                    verdict: c.verdict,
                });
            }
        }
        // Decided here, where "this call walked" is a fact rather than an inference: the scan
        // counter is shared, so a caller that compared it around this call could be reading
        // ANOTHER thread's walk while itself receiving the cached snapshot above — a snapshot
        // that may predate the arming this flag is about.
        let armed_before_the_walk =
            self.change_hub.as_ref().is_some_and(WorkspaceChangeHub::is_watching);
        // Read BEFORE the walk, never after. Too old is safe — the snapshot is refused and
        // the next poll walks again; too new would let a snapshot that predates a baseline
        // move claim it was taken after one.
        let baseline_epoch = lock_recover(&self.inner).baseline_epoch;
        self.scan_count.fetch_add(1, Ordering::SeqCst);
        // One project load per scan: the stat universe and the config identity must
        // describe the same project state, mirroring how the build derives its
        // baseline — otherwise the comparison could pair one state's files with
        // another's topology and mask (or fabricate) drift.
        let config_files_fp = config_files_fingerprint(root);
        let project = crate::graph::input::ProjectSnapshot::load_excluding(root, &self.excluded);
        let (stats, verdict) = crate::graph::scan::scan_stats_over_project(&project);
        let config_fp = config_identity(config_files_fp, &project.configs);
        if armed_before_the_walk {
            // This call read the tree itself, with the watch already holding it: the walk
            // covers everything up to it and the stream everything after, so from here the
            // drain alone is enough.
            self.hub_armed.store(true, Ordering::SeqCst);
        }
        *cache = Some(ScanCache {
            at: Instant::now(),
            stats: stats.clone(),
            config_fp,
            baseline_epoch,
            verdict,
        });
        Some(OwnedScan { stats, config_fp, baseline_epoch, verdict })
    }
}

/// What a tick's drains handed over, in the two shapes the hub can name it: exact paths,
/// and directories whose disappearance stands for descendants no drain can enumerate.
///
/// The distinction is not cosmetic. A vanished directory arrives as ONE entry while the scan
/// lists every file that was under it, so comparing by equality alone would call each of
/// those a miss — and a miss is answered by charging every consumer of the hub a full
/// reconcile.
#[derive(Default)]
pub(super) struct DeliveredPaths {
    exact: std::collections::HashSet<String>,
    subtrees: Vec<PathBuf>,
}

impl DeliveredPaths {
    fn of(entries: &[ChangeEntry]) -> Self {
        let mut delivered = Self::default();
        for entry in entries {
            // EVERY reported removal stands for what was under it. The hub names a vanished
            // path a subtree by the absence of an extension — the only thing knowable about
            // a path that is gone — so a directory called `Dir.v1` arrives as an ordinary
            // removal. Taking only the explicit kind turns its descendants, which the scan
            // does list, into a miss, and a miss costs every consumer a full reconcile.
            // A path that really was a file loses nothing by this: no scan path lies under
            // it, so it answers for itself alone anyway.
            if matches!(
                entry.kind,
                crate::change_hub::ChangeKind::SubtreeRemoved
                    | crate::change_hub::ChangeKind::MaybeRemoved
            ) {
                delivered.subtrees.push(entry.canonical.clone());
            }
            delivered.exact.insert(entry.canonical.to_string_lossy().into_owned());
        }
        delivered
    }

    fn extend(&mut self, other: Self) {
        self.exact.extend(other.exact);
        self.subtrees.extend(other.subtrees);
    }

    /// Whether this path was delivered by name.
    fn covers(&self, path: &str) -> bool {
        self.exact.contains(path)
    }

    /// Whether this path's DISAPPEARANCE was delivered — by name, or as part of a directory
    /// that vanished. Containment is by whole components, so `Dir` never answers for `Dir2`.
    ///
    /// Separate from [`Self::covers`] because an event carries a direction. "This directory
    /// is gone" accounts for the files that were under it and for nothing else: a file that
    /// appeared after the directory was recreated is a change nobody reported, and counting
    /// the removal as its delivery would hide a watcher that stopped seeing that subtree.
    fn covers_removal(&self, path: &str) -> bool {
        self.covers(path) || self.subtrees.iter().any(|dir| Path::new(path).starts_with(dir))
    }
}

/// Whether the scan found drift the hub never handed over — the question a degrade answers,
/// and the reason the two directions are not interchangeable.
///
/// A disappearance may be accounted for by the directory that vanished with it. An
/// appearance or an edit may not: the entry that reported the directory gone says nothing
/// about what showed up there afterwards, and treating it as delivery would silently forgive
/// a watcher that stopped seeing that subtree.
fn has_undelivered_drift(changes: &WorkspaceDiff, delivered: &DeliveredPaths) -> bool {
    changes.removed.iter().any(|p| !delivered.covers_removal(p))
        || changes.added.iter().chain(&changes.modified).any(|p| !delivered.covers(p))
}

pub(super) struct OwnedScan {
    pub(super) stats: Vec<FileStat>,
    pub(super) config_fp: u64,
    pub(super) baseline_epoch: u64,
    pub(super) verdict: ScanVerdict,
}

/// The one place a scan becomes drift: classify it against the live baseline, then keep
/// only what the walk behind it is entitled to assert.
///
/// Every consumer of the scan goes through here — the apply, the reconciler's miss
/// verdict and the freshness answer — because a rule that decides which files exist
/// cannot live in three procedures and stay one rule. The config comparison rides along
/// for the same reason: it is answered from the same snapshot, never re-sampled.
fn scan_drift(inner: &Inner, scan: &OwnedScan) -> (WorkspaceDiff, bool) {
    let changes = classify_changes(&inner.stats, &scan.stats);
    (drift_the_scan_may_assert(changes, scan.verdict), inner.config_fp != scan.config_fp)
}

/// Narrow a scan's drift to what its walk can actually vouch for.
///
/// A short file list is not a list of deletions, and a degraded key is not a new file.
/// The two counters are answered separately because they invalidate different
/// directions:
///
/// - identity degraded (`canonical_fallbacks`): a file whose key fell back to the walked
///   spelling arrives as a removal of its canonical key AND an addition of the walked
///   one. Applying either half is wrong in its own way — the removal unregisters a live
///   file, the addition registers the same physical file a second time — so neither is
///   asserted. What stays are the keys present on both sides: those matched, so no
///   spelling moved under them;
/// - coverage incomplete (`unreadable`): a file the walk did list was seen, keys and all,
///   so additions and edits stand. Only absences fall: the walk cannot tell "gone" from
///   "behind a door it could not open".
///
/// Both at once is the identity case: a walk that also lost coverage has no stronger
/// claim than one that only lost identity.
///
/// Incomplete coverage also drops every path that would RESTATE a config root's
/// structure rather than describe one file. `refresh_metadata_substrate` re-discovers
/// each affected root by walking it and publishes what it found, so a single permitted
/// `.xml` edit would hand a half-read tree the authority to say which objects that root
/// contains — and the ones behind the closed door would be dropped from the listing.
/// A `.bsl` body added under a listing that names it (`CommonModules` and the service
/// families) reaches the same re-discovery, so it goes with them. Plain body text is
/// unaffected: re-reading one file states nothing about the tree around it.
fn drift_the_scan_may_assert(mut changes: WorkspaceDiff, verdict: ScanVerdict) -> WorkspaceDiff {
    if !verdict.identity_exact() {
        changes.added.clear();
        changes.removed.clear();
    } else if !verdict.coverage_complete() {
        changes.removed.clear();
    }
    if !verdict.coverage_complete() {
        changes.added.retain(|p| !restates_a_root_when_added(p));
        // Only an `.xml` EDIT reaches the re-discovery. A body edit re-reads that one
        // file: the listing already names it, and its text says nothing about which
        // objects the root contains.
        changes.modified.retain(|p| !is_metadata_xml(p));
    }
    changes
}

/// Whether this path's APPEARANCE would re-discover a whole config root instead of
/// registering one file — see [`drift_the_scan_may_assert`]. A descriptor obviously
/// restates its root; a body under a listing that names it (`CommonModules` and the
/// service families) does too, because its `module_file` back-link is published by the
/// same re-discovery.
fn restates_a_root_when_added(path: &str) -> bool {
    is_metadata_xml(path) || project_model::is_substrate_listed_body_path(Path::new(path))
}

fn is_metadata_xml(path: &str) -> bool {
    bsl_conventions::str_has_extension(path, bsl_conventions::XML_EXTENSION)
}

/// Advance the drift baseline over a scan the walk could not fully vouch for: the
/// baseline moves for the keys whose drift was APPLIED, and for nothing else.
///
/// The wholesale rebase the clean path does would drop every key the scan did not list —
/// exactly the keys whose absence is unproven. Losing them is worse than the phantom
/// removal it replaces: the file stays registered in the resident while the baseline
/// forgets it ever existed, so its REAL removal later has nothing left to compare
/// against and never lands.
///
/// Recording a key whose drift was REFUSED is the mirror-image mistake: the baseline
/// would then agree with disk, the next scan would see no drift, and the refusal — meant
/// to postpone the change until the tree reads whole — would have cancelled it instead.
///
/// Returns whether the baseline actually moved, because the caller stamps an epoch on
/// that answer and a stamp over a no-op is not free: it declares every snapshot taken
/// before it incomparable, starting with the scan's own cache.
fn rebase_over_incomplete_scan<'a>(
    stats: &mut HashMap<String, u64>,
    applied: impl Iterator<Item = &'a str>,
    new_fp: &HashMap<&str, u64>,
) -> bool {
    let mut moved = false;
    for key in applied {
        if let Some(&fp) = new_fp.get(key) {
            moved |= stats.insert(key.to_string(), fp) != Some(fp);
        }
    }
    moved
}

pub(super) fn compute_freshness(inner: &Inner, scan: Option<&OwnedScan>) -> Freshness {
    let drifted = match scan {
        Some(s) => {
            let (changes, config_changed) = scan_drift(inner, s);
            config_changed || !changes.is_empty()
        }
        None => false,
    };
    Freshness {
        revision: inner.generation,
        // A held hole makes the answer incomplete even over a tree that has not
        // drifted at all: some workspace file is not being analysed. Saying `stale`
        // is what keeps that from being silent — it does not schedule a rebuild.
        stale: drifted
            || inner.reload == ReloadState::Running
            || inner.resident.as_ref().is_some_and(|r| r.unread_count() > 0),
        reload: inner.reload.label(),
        topology: inner.topology,
    }
}

/// The resident's "config drift" identity: the config-file stat fold PLUS the
/// extension-topology hash of `configs`. Folding the topology in covers changes no
/// config-file stat can see — an auto-discovered extension appearing or vanishing
/// re-shapes visibility with `bsl-analyzer.toml` untouched (or absent entirely).
pub(super) fn config_identity(
    config_files_fp: u64,
    configs: &ide::WorkspaceConfigsSnapshot,
) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    (config_files_fp, crate::graph::scan::topology_u64(configs)).hash(&mut hasher);
    hasher.finish()
}

/// [`config_identity`] against a freshly derived project state — the cheap
/// (config-file stat + config parse + extension discovery, no tree walk) probe
/// the post-publication re-check and the throttled scan use. The stat is taken
/// BEFORE the project load, pairing with how the build captures its baseline.
pub(super) fn config_identity_now(root: &Path) -> u64 {
    let config_files_fp = config_files_fingerprint(root);
    // No exclusions, and none are needed: this snapshot is read for its configs alone
    // and never walked, so what the tree contains does not reach the answer.
    config_identity(
        config_files_fp,
        &crate::graph::input::ProjectSnapshot::load_excluding(root, &[]).configs,
    )
}

/// Whether the ignored-authors filter must be (re)built. Fail-open logic:
/// a filter attributing against inputs that no longer exist or moved is
/// rebuilt (an unreadable repository then yields `None` = no suppression),
/// and a missing filter is retried once the repository becomes usable again.
fn author_filter_rebuild_needed(
    configured_authors: &[String],
    stored: Option<(&str, u64)>,
    live_head: Option<&str>,
    live_mailmap: Option<u64>,
) -> bool {
    if configured_authors.is_empty() {
        return false;
    }
    match (stored, live_head) {
        // Attribution inputs moved under an active filter.
        (Some((stored_head, stored_mm)), Some(live)) => {
            stored_head != live || live_mailmap != Some(stored_mm)
        }
        // Active filter over a repository that can no longer resolve HEAD:
        // rebuild (and fail open) rather than keep suppressing against
        // obsolete history.
        (Some(_), None) => true,
        // No filter was built but the repository is usable now — retry.
        (None, Some(_)) => true,
        (None, None) => false,
    }
}

pub(super) fn config_files_fingerprint(root: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::UNIX_EPOCH;

    let mut entries: Vec<(String, u64, u128)> = Vec::new();
    for name in project_model::PROJECT_INPUT_FILE_NAMES {
        let path = root.join(name);
        if let Ok(meta) = std::fs::metadata(&path) {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            entries.push((name.to_string(), meta.len(), mtime));
        }
    }
    entries.sort();
    let mut hasher = DefaultHasher::new();
    entries.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use crate::change_hub::ChangeEntry;
    use crate::diagnostics_state::ResidentOutcome;

    use super::super::drift::{
        author_filter_rebuild_needed, clamp_reconcile_interval, ScanCache, FORCE_RESCAN_FLOOR,
        MIN_RECONCILE_INTERVAL, RECONCILE_INTERVAL,
    };
    use super::super::test_support::*;
    use crate::change_hub::test_support::overflow_the_cap;
    use crate::change_hub::Health;

    #[test]
    fn author_filter_rebuild_decision_covers_every_transition() {
        let authors = vec!["Фирма 1С".to_string()];
        let none: &[String] = &[];

        // Not configured: never rebuild, whatever the repo state.
        assert!(!author_filter_rebuild_needed(none, None, Some("h1"), Some(1)));
        assert!(!author_filter_rebuild_needed(none, Some(("h1", 1)), None, None));

        // Steady state: same HEAD, same mailmap.
        assert!(!author_filter_rebuild_needed(&authors, Some(("h1", 1)), Some("h1"), Some(1)));
        // HEAD moved, mailmap edited, or mailmap unreadable → rebuild.
        assert!(author_filter_rebuild_needed(&authors, Some(("h1", 1)), Some("h2"), Some(1)));
        assert!(author_filter_rebuild_needed(&authors, Some(("h1", 1)), Some("h1"), Some(2)));
        assert!(author_filter_rebuild_needed(&authors, Some(("h1", 1)), Some("h1"), None));
        // Active filter over a repo that lost HEAD → rebuild (fails open).
        assert!(author_filter_rebuild_needed(&authors, Some(("h1", 1)), None, None));
        // Filter missing, repo became usable → retry; still broken → wait.
        assert!(author_filter_rebuild_needed(&authors, None, Some("h1"), Some(1)));
        assert!(!author_filter_rebuild_needed(&authors, None, None, None));
    }
    use super::*;
    use crate::change_hub::ChangeKind;
    use ide::DiagnosticsConfig;
    use std::fs;
    use std::sync::{Arc, Mutex};

    /// The resident's config identity must move on a `dependsOn`-only edit while the
    /// per-file stat channel stays silent — that identity is the only trigger a full
    /// rebuild (with re-derived closures) has for such a change.
    /// `.env` carries the shared-configuration root, so the resident is derived
    /// from it. Both drift channels must see it: the scan channel through the
    /// config identity, and the event channel through the watched path set.
    /// The analyzer's own config file is the control on each.
    #[test]
    fn drift_treats_the_env_file_as_a_project_input() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let env_path = root.join(project_model::PROJECT_ENV_FILE_NAME);

        // Scan channel. The two values differ in LENGTH, not just content: the
        // fingerprint is (name, len, mtime), and same-length values inside one
        // mtime tick would leave it unchanged for reasons unrelated to the rule.
        fs::write(&env_path, "ONEC_CONFIGURATIONS_ROOT=/opt/a\n").unwrap();
        let before = config_identity_now(root);
        fs::write(&env_path, "ONEC_CONFIGURATIONS_ROOT=/opt/bbbbbbbbbb\n").unwrap();
        assert_ne!(
            before,
            config_identity_now(root),
            "a .env edit must move the resident's config identity"
        );

        // Event channel: the watched set decides whether a drained entry counts as
        // config drift at all.
        let (state, _hub) = state_with_hub(root);
        let watched = state.config_file_paths();
        let canonical = |p: std::path::PathBuf| p.canonicalize().unwrap_or(p);
        assert!(
            watched.contains(&canonical(root.join(project_model::CONFIG_FILE_NAMES[0]))),
            "control: the analyzer config file is watched for drift"
        );
        assert!(
            watched.contains(&canonical(env_path)),
            "the .env file must be watched for drift as well"
        );
    }

    #[test]
    fn config_identity_tracks_a_depends_on_only_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        fs::create_dir_all(root.join("ext/a")).unwrap();
        fs::create_dir_all(root.join("ext/b")).unwrap();
        let config = |deps: &str| {
            format!(
                "[source]\nroot = \".\"\nextensions = [\n  \
                 {{ name = \"a\", path = \"ext/a\", scope = \"InfoBase\" }},\n  \
                 {{ name = \"b\", path = \"ext/b\", scope = \"InfoBase\"{deps} }},\n]\n"
            )
        };
        fs::write(root.join("bsl-analyzer.toml"), config("")).unwrap();
        let before = config_identity_now(root);

        fs::write(root.join("bsl-analyzer.toml"), config(", dependsOn = [\"a\"]")).unwrap();
        assert_ne!(
            before,
            config_identity_now(root),
            "the dependency edge must change the resident's config identity"
        );
    }

    /// With no analyzer config-file stat to observe, a scoped extension added
    /// to the project snapshot must still change identity through topology.
    #[test]
    fn config_identity_sees_a_scoped_extension_without_a_config_file_stat() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let project_configs = |include_extension: bool| {
            let extensions = if include_extension {
                vec![project_model::ExtensionDecl::Structured(
                    project_model::StructuredExtensionDecl {
                        name: "NewExt".to_owned(),
                        path: "src/cfe/NewExt".to_owned(),
                        depends_on: Vec::new(),
                        scope: Some(bsl_metadata::ExtensionScope::InfoBase),
                    },
                )]
            } else {
                Vec::new()
            };
            let project = project_model::Project::with_config(
                root,
                project_model::ProjectConfig {
                    configuration_root: Some(".".to_owned()),
                    extensions: Some(extensions),
                    ..project_model::ProjectConfig::default()
                },
            )
            .unwrap();
            ide::WorkspaceConfigsSnapshot::from_project(&project)
        };
        let config_files_fp = config_files_fingerprint(root);
        let before = config_identity(config_files_fp, &project_configs(false));

        write(root, "src/cfe/NewExt/Configuration.xml", "<Configuration/>");
        let after = config_identity(config_files_fp, &project_configs(true));
        assert_ne!(
            before, after,
            "the scoped topology must reshape identity without changing config-file stats"
        );
    }

    /// Editing `bsl-analyzer.toml` is structural drift: the resident fully reloads and
    /// re-derives its effective config, so a later `file`/`workspace` sees the new
    /// settings — the same single source LSP and CLI would pick up.
    #[test]
    fn config_edit_triggers_reload_with_new_config() {
        use ide::DiagnosticCode;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0); // scan every read
        state.ensure_loading();
        wait_ready(&state);

        let typo0 = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
        assert!(matches!(typo0, ResidentOutcome::Ready(true, _)), "initial toml disables Typo");

        // Flip the config; mtime/len change is what config drift keys on.
        edit_moving_mtime(&root.join("bsl-analyzer.toml"), || {
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n")
        });

        // A read sees config drift → full reload (off-thread); poll until it lands.
        wait_until("config edit reloads the resident with the updated diagnostics config", || {
            let disabled = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
            if matches!(disabled, ResidentOutcome::Ready(false, _)) {
                Ok(())
            } else {
                Err("the resident does not serve the edited config yet")
            }
        });
    }

    /// A disabled handle never loads and reads degrade to `Disabled`.
    #[test]
    fn disabled_handle_does_not_load() {
        let state = DiagnosticsState::disabled();
        state.ensure_loading();
        assert_eq!(state.status(), DiagnosticsStatus::Disabled);
        let out = state.read(|_, _| 1usize);
        assert!(matches!(out, ResidentOutcome::Disabled));
    }

    /// Two bytes that `read_to_string` refuses under any UID, so the stand does not
    /// depend on file permissions (which root ignores).
    const UNREADABLE: &[u8] = &[0xFF, 0xFE];

    /// How many bodies of a module the substrate would let a resolver read. Counting
    /// them is a statement about the substrate, which is what these tests measure; a
    /// resolver instead walks them through `CommonModuleBodies::search`, which stops at
    /// the first unread body rather than skipping it.
    fn readable_body_count(bodies: &ide::CommonModuleBodies) -> usize {
        bodies.all_for_reference().count() - bodies.unread().count()
    }

    fn mtime_of(path: &Path) -> std::time::SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn restore_mtime(path: &Path, want: std::time::SystemTime) {
        fs::File::options().write(true).open(path).unwrap().set_modified(want).unwrap();
    }

    /// Run `edit` over the existing file `path` and set its mtime a second past the one it
    /// had, so the stat the drift scan keys on moves however coarse the filesystem clock and
    /// however soon after the last write the edit lands.
    fn edit_moving_mtime(path: &Path, edit: impl FnOnce()) {
        let before = mtime_of(path);
        edit();
        restore_mtime(path, before + Duration::from_secs(1));
    }

    #[test]
    fn an_unreadable_module_leaves_service_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();
        let module = module_path(root, "Сервер");

        std::thread::sleep(Duration::from_millis(10));
        fs::write(&module, UNREADABLE).unwrap();
        wait_for_apply(&state, gen0, "the unreadable body is a drift move");

        let filters = crate::tools::diagnostics::FileFilters {
            min_severity: ide::SeverityBucket::Hint,
            codes: Vec::new(),
            range: None,
            max_findings: 50,
            max_output_tokens: None,
            detailed: false,
        };
        let out = state.read(|resident, _| {
            (
                resident.file_id_for(&module),
                resident.is_unread(&module),
                resident.unread_count(),
                crate::tools::diagnostics::file_findings(
                    resident,
                    &resident.analysis(),
                    None,
                    &module,
                    &filters,
                    0,
                ),
            )
        });
        let ResidentOutcome::Ready((file_id, is_unread, unread, findings), fresh) = out else {
            panic!("expected Ready")
        };
        assert!(file_id.is_none(), "an unreadable module is not served");
        assert!(is_unread, "it is known, though");
        assert_eq!(unread, 1);
        // The distinction the node exists for: not "clean", not "not in workspace".
        assert_eq!(findings.0["error"], "unreadable");
        assert!(fresh.stale, "an unanalysed workspace file makes the answer stale");
        // The envelope must agree with the body: this file is not analysed at all, so the
        // answer about it cannot be whole.
        assert_eq!(
            findings.1.to_value()["reasons"][0]["code"],
            "unreadable_files",
            "the branch that KNOWS the file is a hole must say so: {}",
            findings.1.to_value(),
        );

        // Positive control: the same tree with a readable body answers the opposite.
        // The generation is sampled BEFORE the write — `generation()` polls a window
        // itself, so sampling after it would already include the heal.
        let gen1 = state.generation();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&module, "&НаСервере\nФункция Считать() Экспорт КонецФункции\n").unwrap();
        wait_for_apply(&state, gen1, "the readable body applies");
        let out = state
            .read(|resident, _| (resident.file_id_for(&module).is_some(), resident.unread_count()));
        let ResidentOutcome::Ready((served, unread), _) = out else { panic!("expected Ready") };
        assert!(served, "control: a readable module is served");
        assert_eq!(unread, 0, "control: no holes");
    }

    /// The named retry is paced per file, like the sweep it sits beside and for the same
    /// reason: it reads the WHOLE file under the resident's write lock, and a file that stays
    /// unreadable is the steady state a hole exists for. An editor polling such a file would
    /// otherwise pay that read on every request and block every other reader while it ran.
    #[test]
    fn a_named_hole_retry_is_paced_like_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();
        let module = module_path(root, "Сервер");

        std::thread::sleep(Duration::from_millis(10));
        fs::write(&module, UNREADABLE).unwrap();
        wait_for_apply(&state, gen0, "the unreadable body is a drift move");

        state.drift_interval = Duration::from_secs(3600);
        let ResidentOutcome::Ready(key, _) =
            state.read(|resident, _| resident.hole_key_of(&module))
        else {
            panic!("expected Ready")
        };
        let key = key.expect("the file is a hole");

        // The file stays unreadable, so every retry that runs pays the whole read. The
        // moment recorded for it moves only when one does.
        state.retry_hole_named(&key);
        let first = *lock_recover(&state.last_named_hole_retry)
            .get(&key)
            .expect("the first request re-read the file it asked about");
        std::thread::sleep(Duration::from_millis(5));
        for _ in 0..4 {
            state.retry_hole_named(&key);
        }
        let last = *lock_recover(&state.last_named_hole_retry).get(&key).expect("still recorded");
        assert_eq!(first, last, "a request inside the window re-read the file again");
        let ResidentOutcome::Ready(still_a_hole, _) =
            state.read(|resident, _| resident.is_unread(&module))
        else {
            panic!("expected Ready")
        };
        assert!(still_a_hole, "control: the file is still held out of service");
        assert_eq!(
            lock_recover(&state.last_named_hole_retry).len(),
            1,
            "the pacing record grew per call instead of per file"
        );
    }

    /// The flag that hands the drift over to the event stream is set by the call that walked
    /// the tree, and only by it. The scan counter is shared, so a caller that compared it
    /// around the throttled scan could be reading another thread's walk while itself
    /// receiving the cached snapshot — one taken before the watch armed, which proves nothing
    /// about the window that follows it.
    #[test]
    fn only_the_call_that_walked_hands_the_drift_to_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_secs(3600);
        state.ensure_loading();
        wait_ready(&state);

        // No hub at all: nothing can arm, so no walk may hand the drift over.
        let walks = state.scan_count.load(Ordering::SeqCst);
        state.throttled_scan(root).expect("the first call walks");
        assert!(
            state.scan_count.load(Ordering::SeqCst) > walks,
            "control: the first call after the window really does walk"
        );
        assert!(
            !state.hub_armed.load(Ordering::SeqCst),
            "a walk with no armed watch behind it handed the drift to a stream that carries \
             nothing"
        );

        // A second call inside the window returns the cached snapshot and walks nothing —
        // and must not hand anything over either, however many walks others have made.
        let walks = state.scan_count.load(Ordering::SeqCst);
        state.throttled_scan(root).expect("the cached snapshot is served");
        assert_eq!(
            state.scan_count.load(Ordering::SeqCst),
            walks,
            "control: the second call inside the window did not walk"
        );
        assert!(!state.hub_armed.load(Ordering::SeqCst));
    }

    /// A file held out of service answers "unreadable" and carries no findings, and the
    /// sweep that re-reads holes is paced — it reads every hole whole, so it may not run on
    /// every request. A client that repaired one file and asked for it by name would be told
    /// about a state that has already ended, for as long as that window lasts. The file it
    /// asked for is re-read for that request, and nothing else is.
    #[test]
    fn a_request_for_a_repaired_file_does_not_wait_out_the_sweep_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();
        let module = module_path(root, "Сервер");

        std::thread::sleep(Duration::from_millis(10));
        fs::write(&module, UNREADABLE).unwrap();
        wait_for_apply(&state, gen0, "the unreadable body is a drift move");
        let held = state.read(|resident, _| resident.is_unread(&module));
        let ResidentOutcome::Ready(true, _) = held else { panic!("the file is not a hole") };

        // Paced as in production from here on, and repaired on disk.
        state.drift_interval = Duration::from_secs(3600);
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&module, "&НаСервере\nФункция Считать() Экспорт КонецФункции\n").unwrap();

        // Control: an ordinary read inside the window heals nothing — which is what makes
        // the answer below the request's doing rather than the sweep's.
        let paced = state.read(|resident, _| resident.is_unread(&module));
        let ResidentOutcome::Ready(true, _) = paced else {
            panic!("the sweep healed inside its own window; this test proves nothing")
        };

        state.retry_unread_request_path(None, &module);

        let asked =
            state.read(|resident, _| (resident.is_unread(&module), resident.unread_count()));
        let ResidentOutcome::Ready((still_a_hole, unread), _) = asked else {
            panic!("expected Ready")
        };
        assert!(!still_a_hole, "the file the request named is still held out of service");
        assert_eq!(unread, 0);
    }

    /// The accelerated heal resolves the file the way the ANSWER does.
    ///
    /// A request names a file as a root id and a path inside it; the answer resolves that pair
    /// through the root table. The heal in front of it resolved the raw path against the
    /// workspace root alone, so for anything not physically under that root — an extension, a
    /// nested source root — it looked for a hole under a path nobody holds, found none, and
    /// healed nothing. The periodic sweep still gets there, so the file is not lost; what the
    /// request pays for and does not get is the re-read it asked for.
    #[test]
    fn a_rooted_request_heals_the_hole_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cf = root.join("src/cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(&cf, "Сервер", true, "&НаСервере\nФункция Ч() Экспорт КонецФункции");
        let ext = root.join("src/cfe/Расш");
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(
            &ext,
            "РасшМодуль",
            true,
            "&НаСервере\nФункция Р() Экспорт КонецФункции",
        );
        fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [{ name = \"Расш\", path = \"src/cfe/Расш\", scope = \"InfoBase\" }]\n",
        )
        .unwrap();

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let ext_module = ext.join("CommonModules/РасшМодуль/Ext/Module.bsl");
        let gen0 = state.generation();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&ext_module, UNREADABLE).unwrap();
        wait_for_apply(&state, gen0, "the unreadable extension body is a drift move");
        let held = state.read(|resident, _| resident.is_unread(&ext_module));
        let ResidentOutcome::Ready(true, _) = held else { panic!("the file is not a hole") };

        // Paced as in production, and repaired on disk.
        state.drift_interval = Duration::from_secs(3600);
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&ext_module, "&НаСервере\nФункция Р() Экспорт КонецФункции\n").unwrap();
        let paced = state.read(|resident, _| resident.is_unread(&ext_module));
        let ResidentOutcome::Ready(true, _) = paced else {
            panic!("the sweep healed inside its own window; this test proves nothing")
        };

        // Addressed the way a client addresses a file in a root that is not the workspace
        // root: the id of the root, and the path inside it.
        let key = state.read(|resident, _| resident.workspace_roots().key_of_path(&ext_module));
        let ResidentOutcome::Ready(Some(key), _) = key else {
            panic!("the extension module belongs to a registered root")
        };
        assert_ne!(key.root_id, "", "the stand needs a root that is not the workspace root");
        let relative = std::path::PathBuf::from(&key.path);

        state.retry_unread_request_path(Some(&key.root_id), &relative);

        let asked = state.read(|resident, _| resident.is_unread(&ext_module));
        let ResidentOutcome::Ready(still_a_hole, _) = asked else { panic!("expected Ready") };
        assert!(
            !still_a_hole,
            "the request named the file through its root and the heal looked somewhere else",
        );
    }

    /// A body that becomes unreadable WHILE SERVING must lose its `module_file`
    /// back-link, exactly as one that was unreadable at build time. Otherwise the same
    /// disk state answers differently depending on when it went bad, and consumers of
    /// a non-empty back-link over an empty symbol tree (the subscription and scheduled
    /// job handlers) emit blocking "create the procedure" findings against innocent
    /// files, where an empty back-link makes them return silently.
    #[test]
    fn a_body_that_becomes_unreadable_loses_its_back_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(
            root,
            "Сервер",
            true,
            "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры",
        );
        write_common_module(root, "Клиент", false, "Процедура К() КонецПроцедуры");

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let anchor_path = module_path(root, "Клиент");
        let out = state.read(|r, _| {
            let anchor = r.file_id_for(&anchor_path).expect("the healthy neighbour serves");
            readable_body_count(&r.db().resolve_common_module_files_for_file(anchor, "Сервер"))
        });
        let ResidentOutcome::Ready(before, _) = out else { panic!("expected Ready") };
        assert_eq!(before, 1, "control: a readable body HAS a back-link");

        let gen0 = state.generation();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(module_path(root, "Сервер"), UNREADABLE).unwrap();
        wait_for_apply(&state, gen0, "the unreadable body applies as drift");

        let out = state.read(|r, _| {
            let anchor = r.file_id_for(&anchor_path).expect("the neighbour keeps serving");
            readable_body_count(&r.db().resolve_common_module_files_for_file(anchor, "Сервер"))
        });
        let ResidentOutcome::Ready(after, _) = out else { panic!("expected Ready") };
        assert_eq!(after, 0, "a body nobody could read gets no back-link");
    }

    /// A file created ALREADY unreadable is not registered at all — and "at all" has
    /// to include the VFS interner, which is checked directly. A weaker assertion (no
    /// back-link, not served) passes just as well against a build that interns the
    /// path and only then fails to read it: the back-link is blanked by the unread
    /// filter, not by the absence of an id. That state is forbidden because the id
    /// would have no file-set entry, and the first query through it panics.
    #[test]
    fn a_file_born_unreadable_is_never_interned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();

        std::thread::sleep(Duration::from_millis(10));
        write_common_module_xml(root, "Новый", true);
        let born_bad = module_path(root, "Новый");
        fs::create_dir_all(born_bad.parent().unwrap()).unwrap();
        fs::write(&born_bad, UNREADABLE).unwrap();
        wait_for_apply(&state, gen0, "the new unreadable file is drift");

        let out = state.read(|r, _| {
            let interned = r.vfs_file_id_for_test(&born_bad);
            (interned, r.file_id_for(&born_bad).is_some(), r.unread_count())
        });
        let ResidentOutcome::Ready((interned, served, unread), _) = out else {
            panic!("expected Ready")
        };
        assert!(interned.is_none(), "the VFS must not hold an id for a file never read");
        assert!(!served, "and it is not served");
        assert_eq!(unread, 1, "but it IS counted, not silently absent");
    }

    /// `diagnostics status` reads the state directly and drifts nothing, so a client
    /// that polls only it depends entirely on the background reconciler to notice a
    /// hole that healed. Neither classifier can: the fingerprint is `(mtime, len)`,
    /// and a file that came back readable under the same stat is no drift at all.
    #[test]
    fn the_background_reconciler_notices_a_healed_hole() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = module_path(root, "Сервер");
        let readable = "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры";
        let dark = vec![0xFFu8; readable.len()];

        fs::write(&module, &dark).unwrap();
        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);
        assert_eq!(state.status_report().unread_files, Some(1), "it loads as a hole");

        // Same length, same mtime: nothing a classifier compares has moved.
        let mtime = mtime_of(&module);
        fs::write(&module, readable).unwrap();
        restore_mtime(&module, mtime);

        // Only the two calls a silent daemon makes — never `read`.
        let mut healed = false;
        for _ in 0..300 {
            state.reconcile_tick();
            if state.status_report().unread_files == Some(0) {
                healed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(healed, "the reconciler must retire the hole without a single request");
        assert_eq!(state.status_report().files, Some(1), "and hand the module back to service");
    }

    /// A hole with no registration behind it is workspace state all the same: it is
    /// `unread_files`, it is half of `files_total`, and a non-empty list is what makes
    /// the answer stale. Retiring it therefore moves the resident exactly as creating
    /// it did — otherwise one `result_id` answers both "unreadable" and "not a
    /// workspace file".
    #[test]
    fn retiring_a_hole_that_never_served_still_moves_the_resident() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_secs(30);
        state.ensure_loading();
        wait_ready(&state);

        // Deliberately NOT a listing-backed body: for `CommonModules/…/Ext/Module.bsl`
        // the substrate refresh moves the resident by itself and hides the question.
        let newborn = root.join("Catalogs/Товары/Ext/ObjectModule.bsl");
        fs::create_dir_all(newborn.parent().unwrap()).unwrap();
        std::thread::sleep(FORCE_RESCAN_FLOOR + Duration::from_millis(10));
        fs::write(&newborn, UNREADABLE).unwrap();
        state.force_rescan();
        let out = state.read(|r, _| r.unread_count());
        assert!(matches!(out, ResidentOutcome::Ready(1, _)), "it is a hole that never served");

        // Arm the retry throttle, so the classifier carries the removal, not the list.
        let _ = state.read(|_, _| ());

        let before = raw_generation(&state);
        std::thread::sleep(FORCE_RESCAN_FLOOR + Duration::from_millis(10));
        fs::remove_file(&newborn).unwrap();
        state.force_rescan();
        let out = state.read(|r, _| r.unread_count());
        assert!(matches!(out, ResidentOutcome::Ready(0, _)), "and it stops being counted");
        assert!(raw_generation(&state) > before, "the answer changed, so the revision must too");
    }

    /// The same removal reaches the resident by a SECOND route, and that route is not
    /// the retry list: with the retry window closed, the drift classifier carries the
    /// deletion into the ordinary removal branch — which keys off `by_path`, the one
    /// map a hole left when it stopped serving.
    #[test]
    fn a_hole_the_classifier_removes_is_removed_completely() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(
            root,
            "Сервер",
            true,
            "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры",
        );
        write_common_module(root, "Клиент", false, "Процедура К() КонецПроцедуры");

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        // Wide enough that the retry list stays throttled off for the rest of the test.
        // The scan beside it is re-armed explicitly, so the classifier still runs.
        state.drift_interval = Duration::from_secs(30);
        state.ensure_loading();
        wait_ready(&state);

        let victim = module_path(root, "Сервер");
        let out = state.read(|r, _| r.file_id_for(&victim));
        let ResidentOutcome::Ready(Some(victim_id), _) = out else { panic!("serves first") };

        std::thread::sleep(FORCE_RESCAN_FLOOR + Duration::from_millis(10));
        fs::write(&victim, UNREADABLE).unwrap();
        state.force_rescan();
        let out = state.read(|r, _| r.is_unread(&victim));
        assert!(matches!(out, ResidentOutcome::Ready(true, _)), "it becomes a hole first");

        // Arm the retry throttle. The poll above ran the retry list while `holes` was
        // still empty, and an empty list leaves no timestamp behind.
        let _ = state.read(|_, _| ());

        std::thread::sleep(FORCE_RESCAN_FLOOR + Duration::from_millis(10));
        fs::remove_file(&victim).unwrap();
        state.force_rescan();
        let out = state.read(|r, _| (r.unread_count(), r.file_set_has_for_test(victim_id)));
        let ResidentOutcome::Ready((unread, in_file_set), _) = out else {
            panic!("expected Ready")
        };
        assert_eq!(unread, 0, "a file that is gone is not an unreadable file");
        assert!(!in_file_set, "the file set must not still map the vanished file's id");
    }

    /// A hole that disappears from disk owes the full removal, not just its own
    /// retirement: the file set entry and the metadata back-link outlive the hole
    /// list otherwise, and the back-link then points at a tombstoned FileId.
    #[test]
    fn a_vanished_hole_is_removed_completely() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(
            root,
            "Сервер",
            true,
            "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры",
        );
        write_common_module(root, "Клиент", false, "Процедура К() КонецПроцедуры");

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let victim = module_path(root, "Сервер");
        let anchor = module_path(root, "Клиент");
        // Captured while it still serves: after the removal the interner still answers
        // for this path, so only the file set can say whether the removal completed.
        let out = state.read(|r, _| r.file_id_for(&victim));
        let ResidentOutcome::Ready(Some(victim_id), _) = out else { panic!("serves first") };
        let gen0 = state.generation();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&victim, UNREADABLE).unwrap();
        wait_for_apply(&state, gen0, "it becomes a hole first");

        let gen1 = state.generation();
        std::thread::sleep(Duration::from_millis(10));
        fs::remove_file(&victim).unwrap();
        wait_for_apply(&state, gen1, "removing it moves the resident again");

        let out = state.read(|r, _| {
            let anchor_id = r.file_id_for(&anchor).expect("the neighbour keeps serving");
            (
                r.unread_count(),
                r.is_unread(&victim),
                readable_body_count(
                    &r.db().resolve_common_module_files_for_file(anchor_id, "Сервер"),
                ),
                r.file_set_has_for_test(victim_id),
            )
        });
        let ResidentOutcome::Ready((unread, still_hole, links, in_file_set), _) = out else {
            panic!("expected Ready")
        };
        assert_eq!(unread, 0, "a file that is gone is not an unreadable file");
        assert!(!still_hole, "and the retry list stops probing it");
        assert_eq!(links, 0, "its back-link is gone with it");
        // The load-bearing assertion: the other three hold even if the removal stops
        // half-way, because a deleted file leaves metadata discovery on its own.
        assert!(!in_file_set, "the file set must not still map the vanished file's id");
    }

    /// The single test that tells the retry list apart from ordinary drift
    /// classification: the fingerprint is `(mtime, len)` only, so a heal that keeps
    /// both is invisible to BOTH classifiers. Nothing but the hole list can notice it.
    #[test]
    fn a_hole_heals_under_an_unchanged_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        let module = module_path(root, "Сервер");
        // Exactly two bytes either way, so only the CONTENT differs.
        fs::write(&module, UNREADABLE).unwrap();

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let out = state.read(|r, _| (r.file_id_for(&module).is_some(), r.unread_count()));
        let ResidentOutcome::Ready((served, unread), _) = out else { panic!("expected Ready") };
        assert!(!served, "a module unreadable at build time is not served");
        assert_eq!(unread, 1);

        // Heal with the SAME length and the SAME mtime: no drift exists to be found.
        let before = mtime_of(&module);
        fs::write(&module, b"//").unwrap();
        restore_mtime(&module, before);
        assert_eq!(mtime_of(&module), before, "the stand must not move the fingerprint");

        let mut healed = false;
        for _ in 0..100 {
            let out = state.read(|r, _| (r.file_id_for(&module).is_some(), r.unread_count()));
            if let ResidentOutcome::Ready((true, 0), fresh) = out {
                assert!(!fresh.stale, "with no holes left the answer is fresh again");
                healed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(healed, "the retry list must heal a hole no fingerprint change reveals");
    }

    /// Losing the back-link when a body goes unreadable is only half the contract: the
    /// substrate is rebuilt FROM the database, so a rebuild ordered before the mark is
    /// cleared files the body as unread for good, and every call into the module stays
    /// silently undiagnosed. That outcome is worse than the false finding this route
    /// exists to prevent, and neither `served` nor the hole count shows it.
    #[test]
    fn a_healed_body_becomes_readable_again_in_the_substrate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let body = "&НаСервере\nПроцедура П() Экспорт КонецПроцедуры";
        write_common_module(root, "Сервер", true, body);
        write_common_module(root, "Клиент", false, "Процедура К() КонецПроцедуры");

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let anchor_path = module_path(root, "Клиент");
        let composition = |state: &DiagnosticsState| {
            let out = state.read(|r, _| {
                let anchor = r.file_id_for(&anchor_path).expect("the neighbour serves throughout");
                let bodies = r.db().resolve_common_module_files_for_file(anchor, "Сервер");
                (readable_body_count(&bodies), bodies.unread().count())
            });
            let ResidentOutcome::Ready(counts, _) = out else { panic!("expected Ready") };
            counts
        };

        assert_eq!(composition(&state), (1, 0), "control: a readable body starts readable");

        let gen0 = state.generation();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(module_path(root, "Сервер"), UNREADABLE).unwrap();
        wait_for_apply(&state, gen0, "the unreadable body applies as drift");
        assert_eq!(composition(&state), (0, 1), "unreadable is recorded, not merely dropped");

        let gen1 = state.generation();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(module_path(root, "Сервер"), body).unwrap();
        wait_for_apply(&state, gen1, "the restored body applies as drift");
        assert_eq!(composition(&state), (1, 0), "and the mark is gone once it reads again");
    }

    /// Editing a `.bsl` body drifts the workspace; the next read applies an
    /// incremental `set_file_text` and bumps the generation, with no full rebuild.
    #[test]
    fn incremental_reload_on_body_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0); // scan every read
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();

        // Modify the body; mtime/len change is what the drift scan keys on.
        std::thread::sleep(Duration::from_millis(10));
        fs::write(
            module_path(root, "Сервер"),
            "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции\n",
        )
        .unwrap();

        // A read triggers drift handling; the edited text must be resident afterwards.
        let _ = state.read(|_, _| ());
        // Give a beat in case the apply raced; then re-read.
        for _ in 0..50 {
            if state.generation() > gen0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            let _ = state.read(|_, _| ());
        }
        assert!(state.generation() > gen0, "incremental apply should bump the generation");
        assert!(
            matches!(state.status(), DiagnosticsStatus::Ready { .. }),
            "incremental apply stays Ready, no rebuild churn"
        );

        let text = state.read(|resident, _gen| {
            let file_id = resident.file_id_for(&module_path(root, "Сервер")).unwrap();
            resident.analysis().file_text(file_id)
        });
        match text {
            ResidentOutcome::Ready(t, _) => {
                assert!(t.contains("Возврат 1"), "edited text resident")
            }
            _ => panic!("expected Ready"),
        }
    }

    /// A brand-new common module (descriptor + body) registers into the live resident:
    /// no rebuild (pre-existing FileIds stay stable — a re-enumeration would shift them,
    /// the new name sorts first), and the substrate lists the module (its module-level
    /// diagnostic fires, which requires the `module_file` back-link).
    #[test]
    fn incremental_add_of_new_common_module() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();
        let existing = module_path(root, "Сервер");
        let existing_id_before = match state.read(|r, _| r.file_id_for(&existing)) {
            ResidentOutcome::Ready(Some(id), _) => id,
            _ => panic!("existing module resolves before the add"),
        };

        // The name sorts before "Сервер", so a full re-enumeration would renumber it.
        std::thread::sleep(Duration::from_millis(10));
        write_common_module(root, "ААльфа", true, "Процедура Внутренняя()\nКонецПроцедуры");

        wait_for_apply(&state, gen0, "the add applies in place");
        assert!(
            matches!(state.status(), DiagnosticsStatus::Ready { .. }),
            "incremental add stays Ready"
        );

        let added = module_path(root, "ААльфа");
        let out = state.read(|resident, _| {
            let existing_id_after =
                resident.file_id_for(&existing).expect("existing module still resolves");
            let new_id = resident.file_id_for(&added).expect("new module resolves");
            let findings = resident.analysis().diagnostics(new_id, &DiagnosticsConfig::default());
            (existing_id_after, findings)
        });
        let ResidentOutcome::Ready((existing_id_after, findings), _) = out else {
            panic!("expected Ready")
        };
        assert_eq!(
            existing_id_after, existing_id_before,
            "pre-existing FileIds survive an incremental add (a rebuild would renumber)"
        );
        assert!(
            findings.iter().any(|d| d.code.as_str() == "CommonModuleMissingAPI"),
            "the module-level diagnostic fires — the substrate listed the new module: {:?}",
            findings.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
    }

    /// A new body with no metadata descriptor still registers (readable, findings served);
    /// it just carries no substrate listing until its `.xml` lands.
    #[test]
    fn incremental_add_of_bare_body_without_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();

        std::thread::sleep(Duration::from_millis(10));
        let body = module_path(root, "БезОписания");
        fs::create_dir_all(body.parent().unwrap()).unwrap();
        fs::write(&body, "Процедура Тест()\n    Перем Неиспользуемая;\nКонецПроцедуры").unwrap();

        wait_for_apply(&state, gen0, "the bare add applies in place");
        let out = state.read(|resident, _| {
            let id = resident.file_id_for(&body).expect("bare body resolves");
            resident.analysis().diagnostics(id, &DiagnosticsConfig::default()).len()
        });
        assert!(matches!(out, ResidentOutcome::Ready(n, _) if n > 0), "findings served");
    }

    /// A deleted body unregisters in place: the path stops resolving, the survivor keeps
    /// its FileId, and the state stays Ready without a rebuild.
    #[test]
    fn incremental_remove_of_module_body() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write_common_module(root, "Удаляемый", true, "Процедура У()\nКонецПроцедуры");

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();
        let survivor = module_path(root, "Сервер");
        let survivor_id_before = match state.read(|r, _| r.file_id_for(&survivor)) {
            ResidentOutcome::Ready(Some(id), _) => id,
            _ => panic!("survivor resolves before the removal"),
        };

        std::thread::sleep(Duration::from_millis(10));
        let doomed = module_path(root, "Удаляемый");
        fs::remove_file(&doomed).unwrap();

        wait_for_apply(&state, gen0, "the removal applies in place");
        assert!(
            matches!(state.status(), DiagnosticsStatus::Ready { .. }),
            "incremental removal stays Ready"
        );
        let out = state
            .read(|resident, _| (resident.file_id_for(&doomed), resident.file_id_for(&survivor)));
        let ResidentOutcome::Ready((gone, kept), _) = out else { panic!("expected Ready") };
        assert!(gone.is_none(), "the removed body no longer resolves");
        assert_eq!(kept, Some(survivor_id_before), "the survivor keeps its FileId");
    }

    /// Idle eviction drops the resident db back to `Idle` after the quiet period, and
    /// a later read rebuilds it.
    ///
    /// This is the one place where the sweeper's clock IS the subject, so it stays
    /// raised — and therefore nothing here may assert on a state the sweeper is free to
    /// take away. Every assertion is about `generation` instead: it is bumped on each
    /// publication and eviction never rolls it back, so the sweeper may fire whenever it
    /// likes and what was read stays true. Waiting to observe `Ready` would be the
    /// opposite: the resident is entitled to vanish before the observer reaches it, and
    /// `Idle` is not a state the wait can end on.
    #[test]
    fn idle_eviction_drops_and_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.eviction_after = Duration::from_millis(50);
        state.ensure_loading();

        // No reads for longer than the eviction window → the sweeper drops what it built.
        wait_until("the sweeper evicts the resident it built", || {
            match (state.generation(), state.status()) {
                (built, DiagnosticsStatus::Idle) if built >= 1 => Ok(()),
                observed => Err(observed),
            }
        });

        // A later use rebuilds: a second publication, not merely a second `Ready`.
        state.ensure_loading();
        wait_until("a later use rebuilds the resident", || match state.generation() {
            rebuilt if rebuilt >= 2 => Ok(()),
            rebuilt => Err(format!("only {rebuilt} publications so far")),
        });
    }

    /// `status_report` reflects the lifecycle: `idle` before load, `ready` with the file
    /// count and a bumped generation after, and `reload = none` when not reloading.
    #[test]
    fn status_report_tracks_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        let before = state.status_report();
        assert_eq!(before.state, "idle");
        assert_eq!(before.generation, 0);
        assert_eq!(before.files, None);

        state.ensure_loading();
        wait_ready(&state);

        let after = state.status_report();
        assert_eq!(after.state, "ready");
        assert!(after.generation >= 1, "generation bumped on build");
        assert_eq!(after.files, Some(1), "one resident .bsl");
        assert_eq!(after.reload, "none");
        assert!(after.error.is_none());
        // elapsed_ms is cleared once ready.
        assert!(after.elapsed_ms.is_none());
    }

    /// The production `catch_build` fold: an `Ok` build passes through, an `Err` becomes a
    /// message, and a PANIC is folded into `Err` (so the caller publishes `Failed` instead
    /// of leaving a dead thread with the status pinned at `Loading`).
    #[test]
    fn catch_build_folds_ok_err_and_panic() {
        let ok: Result<i32, String> = DiagnosticsState::catch_build(|| Ok(42));
        assert_eq!(ok, Ok(42));

        let err = DiagnosticsState::catch_build(|| -> anyhow::Result<i32> {
            anyhow::bail!("plain build error")
        });
        assert_eq!(err, Err("plain build error".to_owned()));

        let panicked = DiagnosticsState::catch_build(|| -> anyhow::Result<i32> {
            panic!("synthetic build panic")
        });
        let msg = panicked.unwrap_err();
        assert!(msg.contains("panicked") && msg.contains("synthetic build panic"), "{msg}");
    }

    /// End-to-end: a loader that publishes via `catch_build`'s `Err` path lands in
    /// `Failed` with `loading_since` cleared (no stale `elapsed_ms`), never stuck `Loading`.
    #[test]
    fn failed_build_clears_loading_since_and_is_visible() {
        let err = DiagnosticsState::catch_build(|| -> anyhow::Result<()> { panic!("boom") });
        // Simulate run_load's Err arm publishing the failure.
        let state = DiagnosticsState::for_workspace(std::env::temp_dir());
        {
            let mut inner = lock_recover(&state.inner);
            inner.loading_since = Some(Instant::now());
            inner.status = DiagnosticsStatus::Loading;
            // The exact publication run_load performs on Err.
            inner.loading_since = None;
            inner.status = DiagnosticsStatus::Failed(err.unwrap_err());
        }
        let report = state.status_report();
        assert_eq!(report.state, "failed");
        assert!(report.error.as_deref().unwrap().contains("boom"));
        assert!(report.elapsed_ms.is_none(), "loading_since cleared on failure");
    }

    /// Adding a metadata `.xml` point-refreshes the substrate in place: the new object
    /// resolves without a full db rebuild (no reload kicked), and the generation bumps.
    #[test]
    fn xml_add_point_refreshes_substrate_without_full_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();

        let module = module_path(root, "Сервер");
        assert!(!catalog_resolves(&state, &module), "catalog absent before the add");

        std::thread::sleep(Duration::from_millis(10));
        write_catalog(root, "Товары", 9);

        assert!(catalog_resolves(&state, &module), "added catalog resolves after point-refresh");
        assert_eq!(
            state.status_report().reload,
            "none",
            "no full rebuild was kicked for an XML add"
        );
        assert!(state.generation() > gen0, "the point-refresh bumps the generation");
        assert!(matches!(state.status(), DiagnosticsStatus::Ready { .. }), "stays Ready, no churn");
    }

    /// Removing a metadata `.xml` tombstones the object through a point-refresh — it no
    /// longer resolves — with no full rebuild.
    #[test]
    fn xml_remove_point_refreshes_substrate_without_full_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write_catalog(root, "Товары", 9);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();

        let module = module_path(root, "Сервер");
        assert!(catalog_resolves(&state, &module), "catalog present before the remove");

        std::thread::sleep(Duration::from_millis(10));
        std::fs::remove_file(root.join("Catalogs/Товары.xml")).unwrap();

        assert!(
            !catalog_resolves(&state, &module),
            "removed catalog tombstoned after point-refresh"
        );
        assert_eq!(
            state.status_report().reload,
            "none",
            "no full rebuild was kicked for an XML remove"
        );
        assert!(state.generation() > gen0, "the point-refresh bumps the generation");
    }

    /// Editing a metadata `.xml` re-reads only that object; the resident stays in place
    /// (no full rebuild) and its diagnostics equal a cold build over the mutated tree.
    #[test]
    fn xml_edit_point_refreshes_and_matches_fresh_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write_catalog(root, "Товары", 9);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();

        let module = module_path(root, "Сервер");
        std::thread::sleep(Duration::from_millis(10));
        write_catalog(root, "Товары", 12); // content edit → the object's revision moves

        // A read triggers the synchronous point-refresh.
        let _ = state.read(|_, _| ());
        assert_eq!(
            state.status_report().reload,
            "none",
            "no full rebuild was kicked for an XML edit"
        );
        assert!(state.generation() > gen0, "the edit is detected and applied in place");

        // A cold resident over the same on-disk tree must agree diagnostic-for-diagnostic.
        let fresh = DiagnosticsState::for_workspace(root.to_path_buf());
        fresh.ensure_loading();
        wait_ready(&fresh);
        assert_eq!(
            module_diag_fingerprint(&state, &module),
            module_diag_fingerprint(&fresh, &module),
            "point-refreshed diagnostics must equal a cold build over the mutated tree"
        );
    }

    /// An analyzer-config edit is NOT a metadata point-refresh: it still forces a full
    /// rebuild, re-deriving the effective config (something only a rebuild does). Proven
    /// by the resident picking up the flipped `Typo` setting after the edit.
    #[test]
    fn analyzer_config_edit_still_full_rebuilds() {
        use ide::DiagnosticCode;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let disabled0 = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
        assert!(matches!(disabled0, ResidentOutcome::Ready(true, _)), "initial toml disables Typo");

        edit_moving_mtime(&root.join("bsl-analyzer.toml"), || {
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n")
        });

        // The reload runs off-thread; poll until the re-derived config lands.
        wait_until("a config edit full-rebuilds and re-derives the effective config", || {
            let disabled = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
            if matches!(disabled, ResidentOutcome::Ready(false, _)) {
                Ok(())
            } else {
                Err("the resident does not serve the edited config yet")
            }
        });
    }

    /// A metadata `.xml` that `discover_*` does NOT enroll as a composing file
    /// (`Configuration.xml` here — a whole-config `load_from_directory` would re-read it)
    /// must still invalidate the coarse Channel-2 `load_configuration` memo via an
    /// unconditional config-revision bump, without a full rebuild. Observed directly
    /// through the config-root revision the `load_configuration` query keys on.
    #[test]
    fn non_enrolled_xml_edit_bumps_channel2_without_full_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        // Config roots are registered under their resolved spelling, and the revision is
        // asked for by the root a path falls under: through a symlinked component the
        // question would land on no root at all and be answered by the global fallback,
        // which this edit has no reason to move.
        let root = &dir.path().canonicalize().unwrap();
        sample_workspace(root);
        write(root, "Configuration.xml", "<Configuration><Name>Конфа</Name></Configuration>");

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let module = module_path(root, "Сервер");
        let rev0 = state.read(|r, _| r.db.config_root_revision_for_path(&module));
        let ResidentOutcome::Ready(rev0, _) = rev0 else { panic!("expected Ready") };

        std::thread::sleep(Duration::from_millis(10));
        write(root, "Configuration.xml", "<Configuration><Name>Другая</Name></Configuration>");

        // A read triggers the synchronous point-refresh; the non-enrolled edit still bumps
        // the config revision even though no per-MDO composing file moved.
        let rev1 = state.read(|r, _| r.db.config_root_revision_for_path(&module));
        let ResidentOutcome::Ready(rev1, _) = rev1 else { panic!("expected Ready") };

        assert_eq!(
            state.status_report().reload,
            "none",
            "a non-enrolled XML edit is a point-refresh, not a full rebuild"
        );
        assert!(rev1 > rev0, "the config-root revision the Channel-2 memo keys on must bump");
    }

    /// A metadata subtree that is a symlink to a directory OUTSIDE the config root: the
    /// canonical (scan) path of its XML resolves outside the root, so the point-refresh
    /// cannot express the drift. Editing such an XML must still reach the resident — the
    /// pre-classification routes it to a full rebuild instead of silently forgetting it.
    #[cfg(unix)]
    #[test]
    fn symlinked_subtree_outside_root_xml_edit_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let root = base.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        // Real common-module content lives OUTSIDE the workspace root, reached only via a
        // symlinked `CommonModules` directory.
        let real = base.join("real");
        write_common_module(&real, "Сервер", true, "&НаСервере\nФункция Ч() Экспорт КонецФункции");
        std::os::unix::fs::symlink(real.join("CommonModules"), root.join("CommonModules")).unwrap();

        let mut state = DiagnosticsState::for_workspace(root.clone());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let module = root.join("CommonModules/Сервер/Ext/Module.bsl");
        assert_eq!(module_is_server(&state, &module), Some(true), "starts server-side");

        // Flip Server→false via the descriptor XML only (no body edit). Its canonical path
        // is outside the root, so the point-refresh cannot own it.
        edit_moving_mtime(&real.join("CommonModules/Сервер.xml"), || {
            write_common_module_xml(&real, "Сервер", false)
        });

        // The full rebuild is async; poll until the edit lands.
        wait_until("an XML edit under a symlinked-outside subtree must not be lost", || {
            let _ = state.read(|_, _| ());
            match module_is_server(&state, &module) {
                Some(false) => Ok(()),
                server => Err(server),
            }
        });
    }

    /// A metadata subtree that is a symlink to another directory INSIDE the config root:
    /// the canonical path stays under the root (so the point-refresh owns it), but the
    /// discovery join keeps the symlink unresolved. Editing the XML must re-read the file
    /// in place (via `enroll_refresh`'s canonicalise-on-miss) — a point-refresh, not a
    /// full rebuild.
    #[cfg(unix)]
    #[test]
    fn symlinked_subtree_inside_root_xml_edit_point_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Real modules live under `root/RealCM`; `root/CommonModules` is a symlink to it —
        // both inside the root, so canonical paths stay under the root.
        let realcm = root.join("RealCM");
        std::fs::create_dir_all(&realcm).unwrap();
        write_common_module(
            &realcm,
            "Сервер",
            true,
            "&НаСервере\nФункция Ч() Экспорт КонецФункции",
        );
        std::os::unix::fs::symlink(realcm.join("CommonModules"), root.join("CommonModules"))
            .unwrap();

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let module = root.join("CommonModules/Сервер/Ext/Module.bsl");
        assert_eq!(module_is_server(&state, &module), Some(true), "starts server-side");

        std::thread::sleep(Duration::from_millis(10));
        write_common_module_xml(&realcm, "Сервер", false);

        // A read triggers the synchronous point-refresh; no full rebuild.
        let _ = state.read(|_, _| ());
        assert_eq!(
            state.status_report().reload,
            "none",
            "an in-root symlinked XML edit is a point-refresh, not a full rebuild"
        );
        assert_eq!(
            module_is_server(&state, &module),
            Some(false),
            "enroll_refresh must re-read the edited XML through the canonicalise-on-miss path"
        );
    }

    /// The `metadata object` tool path (`object_from_db` over the resident substrate) sees
    /// a newly-added catalog through the point-refresh — no full db rebuild, generation
    /// bumped — and never loads the whole configuration (the resolver is substrate-only).
    #[test]
    fn metadata_object_finds_added_catalog_without_full_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        let gen0 = state.generation();

        let found = |state: &DiagnosticsState| {
            let out = state.read(|r, _| {
                crate::tools::metadata::object_from_db(r.db(), "Catalog", "Товары").is_ok()
            });
            match out {
                ResidentOutcome::Ready(v, _) => v,
                _ => panic!("expected Ready"),
            }
        };

        assert!(!found(&state), "catalog absent before the add");

        std::thread::sleep(Duration::from_millis(10));
        write_catalog(root, "Товары", 9);

        assert!(found(&state), "the metadata object tool finds the added catalog");
        assert_eq!(state.status_report().reload, "none", "no full db rebuild for an object add");
        assert!(state.generation() > gen0, "the point-refresh bumped the generation");
    }

    /// The idle-eviction contract for metadata reads: after the resident is evicted, a read
    /// re-triggers the build and degrades to a "loading" outcome (or Ready once rebuilt) —
    /// NEVER a hard `Disabled`/`Failed` error. The tool maps `Loading` to a retry envelope.
    #[test]
    fn metadata_read_after_eviction_is_loading_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        // The sweeper is not raised: the subject is what a read does AFTER an eviction,
        // so the eviction is driven by the very step the sweeper takes. A sweeper armed
        // before the resident exists would let it vanish between the publication of
        // `Ready` and the observation of it, and the wait for `Ready` would then never
        // end.
        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.without_idle_sweeper();
        state.eviction_after = Duration::ZERO;
        state.ensure_loading();
        wait_ready(&state);

        assert!(state.evict_if_idle(), "an untouched ready resident is evicted");
        assert_eq!(state.status(), DiagnosticsStatus::Idle, "resident evicted after idle");

        // A metadata read after eviction: re-trigger the build and read. It is Loading (still
        // rebuilding) or Ready (rebuilt fast) — the tool turns Loading into a retry envelope,
        // never surfacing a hard "not loaded" error.
        state.ensure_loading();
        let out = state
            .read(|r, _| crate::tools::metadata::object_from_db(r.db(), "Catalog", "X").is_ok());
        assert!(
            matches!(out, ResidentOutcome::Loading | ResidentOutcome::Ready(_, _)),
            "an evicted metadata read must be loading or ready, never a hard error",
        );
    }

    /// The `metadata object` miss retry drops the throttle cache to force a re-scan, but
    /// only when the last scan is older than [`FORCE_RESCAN_FLOOR`] — so a loop of
    /// genuinely-absent lookups cannot stat-walk the workspace faster than that floor
    /// (the retired MetadataCache's storm guard). Exercised with a synthetic past
    /// `Instant`, so it is deterministic and needs no real sleep.
    ///
    /// A decline is also REPORTED, and reported as the time still owed to the floor: a
    /// caller whose answer depends on the walk waits exactly that long instead of taking
    /// the swallowed force for a verdict. A guard that declined silently would be
    /// indistinguishable from one that armed.
    #[test]
    fn force_rescan_is_storm_guarded_by_the_floor() {
        let state = DiagnosticsState::for_workspace(std::env::temp_dir());

        // A fresh scan (just now) must NOT be force-cleared — the storm guard.
        *lock_recover(&state.scan) = Some(ScanCache {
            at: Instant::now(),
            stats: Vec::new(),
            config_fp: 0,
            baseline_epoch: 0,
            verdict: ScanVerdict::for_test(0, 0),
        });
        let owed = state.force_rescan();
        assert!(
            lock_recover(&state.scan).is_some(),
            "a scan within the floor is kept, so repeated misses cannot hammer the FS",
        );
        let owed = owed.expect("a declined force says so");
        assert!(
            owed <= FORCE_RESCAN_FLOOR && !owed.is_zero(),
            "the decline is reported as the time the floor still owes, and a scan taken \
             just now owes nearly the whole floor: {owed:?}",
        );

        // A scan older than the floor IS cleared, so the next read re-scans and can pick up
        // a just-added object.
        let stale_at = Instant::now()
            .checked_sub(FORCE_RESCAN_FLOOR + Duration::from_millis(50))
            .expect("a valid past instant");
        *lock_recover(&state.scan) = Some(ScanCache {
            at: stale_at,
            stats: Vec::new(),
            config_fp: 0,
            baseline_epoch: 0,
            verdict: ScanVerdict::for_test(0, 0),
        });
        assert_eq!(
            state.force_rescan(),
            None,
            "a force that armed owes nothing, or a caller would wait for a walk it got",
        );
        assert!(
            lock_recover(&state.scan).is_none(),
            "a scan older than the floor is force-cleared so the retry re-scans",
        );
    }

    // --- Event-driven drift (W2): the change hub feeds a drain-on-read path. ---

    /// A `.bsl` body edit reaches the resident through the hub drain, and the healthy hot
    /// path performs NO workspace scan — the whole point of the event-driven path.
    #[test]
    fn event_driven_body_edit_lands_via_drain_without_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);
        assert_eq!(state.scan_count(), 0, "the cold build does not go through the throttled scan");

        let gen0 = raw_generation(&state);
        std::thread::sleep(Duration::from_millis(10));
        fs::write(
            module_path(root, "Сервер"),
            "&НаСервере\nФункция Считать() Экспорт Возврат 1; КонецФункции\n",
        )
        .unwrap();

        wait_for_apply(&state, gen0, "the body edit must be applied via drain");
        assert_eq!(state.scan_count(), 0, "the event-driven hot path performs no scan");

        let text = state.read(|resident, _| {
            let fid = resident.file_id_for(&module_path(root, "Сервер")).unwrap();
            resident.analysis().file_text(fid)
        });
        match text {
            ResidentOutcome::Ready(t, _) => {
                assert!(t.contains("Возврат 1"), "edited text resident")
            }
            _ => panic!("expected Ready"),
        }
    }

    /// A metadata `.xml` edit is delivered through the drain and point-refreshes the
    /// substrate in place (no full rebuild), again with no scan on the hot path.
    #[test]
    fn event_driven_xml_edit_lands_via_drain() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let gen0 = raw_generation(&state);
        std::thread::sleep(Duration::from_millis(10));
        // Flip the common module's server flag: a pure `.xml` edit (no body change).
        write_common_module_xml(root, "Сервер", false);

        wait_for_apply(&state, gen0, "the xml edit must be applied via drain");
        assert_eq!(
            state.status_report().reload,
            "none",
            "an xml edit is a point-refresh, not a full rebuild"
        );
        assert_eq!(state.scan_count(), 0, "the event-driven hot path performs no scan");
    }

    /// A degraded hub falls back to exactly today's throttled scan path: the edit is still
    /// applied, but through a scan (parity with the pre-hub behaviour).
    #[test]
    fn degraded_hub_reconciles_via_scan_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);
        // Force the scan fallback.
        hub.degrade_external();
        assert!(matches!(hub.health(), Health::Degraded(_)));

        let gen0 = raw_generation(&state);
        std::thread::sleep(Duration::from_millis(10));
        fs::write(
            module_path(root, "Сервер"),
            "&НаСервере\nФункция Считать() Экспорт Возврат 2; КонецФункции\n",
        )
        .unwrap();

        wait_for_apply(&state, gen0, "a degraded hub still applies the edit via scan");
        assert!(state.scan_count() > 0, "the degraded path uses the scan, matching today");
    }

    /// A consumer that stopped draining owes its own reconcile, and it used to be charged
    /// to everyone: the shared verdict stayed degraded for as long as that cursor was
    /// silent, so these diagnostics answered every read with a full workspace scan for the
    /// rest of the daemon's life.
    ///
    /// The counter covers BOTH health questions on this path — the drain/scan choice and
    /// the freshness fallback — because either one scanning shows up in it. An
    /// implementation that moved only one of them off the shared verdict fails here.
    #[test]
    fn a_foreign_cursors_debt_does_not_cost_diagnostics_a_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        // A stranger subscribes and never drains; everyone is asked to reconcile. These
        // diagnostics answer for THEMSELVES — that debt is genuinely theirs — and the
        // stranger's stays outstanding for ever after.
        let _stranger = hub.subscribe();
        hub.degrade_external();
        // Paid through the reconcile tick, not through a read: on the scan path a read
        // never drains, so the debt would otherwise outlive the reason for it.
        state.reconcile_tick();

        let scans = state.scan_count();
        let _ = state.read(|_resident, _| ());
        assert_eq!(
            state.scan_count(),
            scans,
            "somebody else's outstanding reconcile is not these diagnostics' to pay for"
        );
    }

    /// The other half, and the one that keeps the first honest: a hub that cannot deliver
    /// at all leaves nothing to trust, however clean this consumer's own cursor is.
    /// Without this leg, an unconditional fast path passes the test above while quietly
    /// serving whatever the event stream never delivered.
    #[test]
    fn a_hub_that_cannot_deliver_still_sends_diagnostics_to_a_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let hub = WorkspaceChangeHub::start_with_unstartable_thread(vec![
            crate::change_hub::WatchTarget::recursive(root.to_path_buf()),
        ]);
        let mut state = DiagnosticsState::for_workspace(root.to_path_buf()).with_change_hub(hub);
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let scans = state.scan_count();
        let _ = state.read(|_resident, _| ());
        assert!(
            state.scan_count() > scans,
            "a hub that will never deliver leaves the diagnostics nothing to trust"
        );
    }

    /// The reconciler/watchdog: a change the event stream failed to deliver (simulated by
    /// draining the cursor without applying) is caught by the periodic scan, which applies
    /// the drift AND degrades the hub so reads revert to scanning until it recovers.
    #[test]
    fn reconciler_catches_undelivered_drift_and_degrades() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let mut observer = hub.subscribe();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(
            module_path(root, "Сервер"),
            "&НаСервере\nФункция Считать() Экспорт Возврат 3; КонецФункции\n",
        )
        .unwrap();
        // Confirm the hub delivered the change (so the diagnostics cursor has it too)...
        wait_for_delivery(&hub, &mut observer, "Module.bsl", "hub delivered the edit");
        // ...then simulate a lossy sink dropping it: consume the cursor without applying.
        // Consume to quiet, not once: one write is several raw events, and any the hub
        // has yet to record would be read as a delivery by the tick — the opposite of
        // the loss this test sets up.
        discard_until_quiet(&state, &hub, 3, "the hub went quiet before the tick");
        // The sink stays lossy for the whole tick, and that has to be arranged rather
        // than hoped for. One write is not one event: the watcher may deliver the same
        // file again while the reconciler scans, and step 3 of the tick then counts the
        // path as late rather than missed — no degrade, and the assertion below fails
        // for a reason that has nothing to do with the rule it checks.
        state.set_post_scan_probe(cursor_discarder(&state));

        let gen0 = raw_generation(&state);
        assert_eq!(hub.health(), Health::Healthy, "still healthy before the reconcile");
        state.reconcile_tick();

        assert!(raw_generation(&state) > gen0, "the reconciler applied the missed drift");
        assert_eq!(
            hub.health().label(),
            "degraded:reconcile-miss",
            "a delivered-but-undrained miss degrades the hub to the scan fallback",
        );
    }

    /// An analyzer-config edit delivered through the drain is structural: it forces a full
    /// rebuild that re-derives the effective config, exactly like the scan path.
    #[test]
    fn event_driven_config_edit_full_rebuilds() {
        use ide::DiagnosticCode;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);
        let disabled0 = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
        assert!(matches!(disabled0, ResidentOutcome::Ready(true, _)), "initial toml disables Typo");

        write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n");

        wait_until("a config edit via drain full-rebuilds and re-derives the config", || {
            let disabled = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
            if matches!(disabled, ResidentOutcome::Ready(false, _)) {
                Ok(())
            } else {
                Err("the resident does not serve the edited config yet")
            }
        });
    }

    /// After a full rebuild the cursor is re-subscribed, so a change landing AFTER the
    /// rebuild is applied to the fresh resident (the drain path survives a rebuild).
    #[test]
    fn events_after_rebuild_apply_to_the_new_resident() {
        use ide::DiagnosticCode;

        let dir = tempfile::tempdir().unwrap();
        // Resolved, because the wait below asks the drift baseline about this file and the
        // baseline is keyed the way the scan spells its paths — resolved. An unresolved
        // link component in a temporary directory would make the question address nothing.
        let root = &dir.path().canonicalize().unwrap();
        sample_workspace(root);
        write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        // Force a full rebuild via a config edit and wait for the fresh resident.
        write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n");
        wait_until("the config rebuild completed", || {
            let disabled = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
            if matches!(disabled, ResidentOutcome::Ready(false, _)) {
                Ok(())
            } else {
                Err("the resident does not serve the edited config yet")
            }
        });

        // A body edit AFTER the rebuild must reach the freshly-built resident via drain.
        let module = module_path(root, "Сервер");
        fs::write(&module, "&НаСервере\nФункция Считать() Экспорт Возврат 42; КонецФункции\n")
            .unwrap();
        // Waited for by THIS file's own baseline row, not by a generation move: the config
        // edit is seen twice — by the scan and, later, by the hub — and the second sighting
        // moves the generation on its own. Reading the text on that move asks the resident
        // for a file whose recorded revision disk has since left behind, which it refuses
        // outright, so the proxy fails the test for something that is not its subject.
        let on_disk =
            crate::graph::scan::file_fingerprint(&module).expect("the edited body is on disk");
        let key = module.to_string_lossy().into_owned();
        wait_until("post-rebuild edits apply to the new resident", || {
            let _ = state.read(|_, _| ());
            match lock_recover(&state.inner).stats.get(&key).copied() {
                Some(fp) if fp == on_disk => Ok(()),
                other => Err(format!("baseline holds {other:?}, waiting for {on_disk}")),
            }
        });
        let text = state.read(|r, _| {
            let fid = r.file_id_for(&module).unwrap();
            r.analysis().file_text(fid)
        });
        match text {
            ResidentOutcome::Ready(t, _) => {
                assert!(t.contains("Возврат 42"), "new resident edited")
            }
            _ => panic!("expected Ready"),
        }
    }

    /// A rebuild starts by taking a fresh cursor, and the rebuild can fail — in which case
    /// the OLD resident keeps serving requests. So an outstanding reconcile has to survive
    /// the swap: settling it against a baseline that was never taken would leave the stale
    /// resident looking healthy, with the events it missed already reclaimed.
    #[test]
    fn a_rebuild_that_takes_a_fresh_cursor_keeps_the_reconcile_it_owed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);
        assert!(state.has_hub_cursor(), "a built resident holds a cursor");

        hub.degrade_external();
        state.resubscribe_cursor();

        let cursor = lock_recover(&state.hub_cursor).expect("the resident still holds a cursor");
        assert!(
            hub.drain(cursor).rescan_required,
            "the debt belongs to the resident, not to the cursor it happened to hold",
        );
    }

    /// #188: a successful rebuild HAS answered the reconcile its fresh cursor carried. Left
    /// unsettled, the resident goes on owing a debt the build already paid, and every read
    /// takes the scan path — a full tree walk per window for a loss nobody can act on — until
    /// the watchdog tick a minute and a half later.
    #[test]
    fn a_successful_rebuild_settles_the_debt_it_carried() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        hub.degrade_external();
        // The baseline this fix moves: while the debt is owed, a read walks the tree. The
        // stand uses a zero drift interval, so a read on the scan path never reuses a cache.
        let owed = state.scan_count();
        let _ = state.read(|_, _| ());
        assert!(
            state.scan_count() > owed,
            "an owed debt does not put reads on the scan path — nothing is measured here",
        );

        state.kick_full_reload();
        wait_until("the rebuild finishes", || match lock_recover(&state.inner).reload {
            ReloadState::Running => Err("the rebuild is still running"),
            ReloadState::Failed(_) => panic!("the rebuild failed"),
            ReloadState::Idle => Ok(()),
        });

        let cursor = lock_recover(&state.hub_cursor).expect("the rebuilt resident holds a cursor");
        assert_eq!(
            hub.health_for(Some(cursor)),
            Health::Healthy,
            "the successful rebuild paid the debt its fresh cursor carried",
        );

        let scans = state.scan_count();
        let _ = state.read(|_, _| ());
        let _ = state.read(|_, _| ());
        assert_eq!(state.scan_count(), scans, "a settled debt keeps reads off the scan path");
    }

    /// #188: settling moves the debt and nothing else. An edit that lands after the fresh
    /// subscription — the window `edit_during_rebuild_applies_after_it` models — is still
    /// delivered by the drain and applied, even though the rebuild that settles the debt read
    /// its baseline before the edit existed. Settling by draining would have carried the
    /// cursor's position away with the debt and dropped that edit.
    #[test]
    fn an_edit_from_the_rebuild_window_survives_the_settling_of_its_debt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        // A debt to carry, and the fresh cursor a rebuild takes at its start.
        hub.degrade_external();
        let carried = state.resubscribe_cursor();
        // The rebuild in flight, as `edit_during_rebuild_applies_after_it` models it.
        lock_recover(&state.inner).reload = ReloadState::Running;

        let mut observer = hub.subscribe();
        std::thread::sleep(Duration::from_millis(10));
        let module = module_path(root, "Сервер");
        fs::write(&module, "&НаСервере\nФункция Считать() Экспорт Возврат 11; КонецФункции\n")
            .unwrap();
        wait_for_delivery(&hub, &mut observer, "Module.bsl", "the hub delivered the edit");
        hub.unsubscribe(observer);

        let gen0 = raw_generation(&state);
        // The rebuild finishes and settles the debt it carried.
        lock_recover(&state.inner).reload = ReloadState::Idle;
        state.settle_rebuild_debt(carried);

        let cursor = lock_recover(&state.hub_cursor).expect("the resident holds its cursor");
        assert_eq!(hub.health_for(Some(cursor)), Health::Healthy, "the debt is settled");

        // The edit from the window is DELIVERED, not SCANNED. With the debt settled, reads
        // stay on the drain path, so the generation moves while the scan counter does not. A
        // test that only read the file text would pass even had the settle been skipped and a
        // scan found the edit — this half is what separates delivery from a walk.
        let scans = state.scan_count();
        wait_until("the window's edit applies by delivery", || {
            let _ = state.read(|_, _| ());
            match raw_generation(&state) {
                now if now > gen0 => Ok(()),
                now => Err(format!("generation still {now}")),
            }
        });
        assert_eq!(
            state.scan_count(),
            scans,
            "the edit arrived by the drain; a scan means the settled debt was still owed",
        );
        let text = state.read(|r, _| {
            let fid = r.file_id_for(&module).unwrap();
            r.analysis().file_text(fid)
        });
        match text {
            ResidentOutcome::Ready(t, _) => {
                assert!(t.contains("Возврат 11"), "the edit from the rebuild window was lost");
            }
            _ => panic!("expected Ready"),
        }
    }

    /// #188, the cut: an overflow during the rebuild window cuts the resident's FRESH cursor
    /// out of entries its baseline scan never read, and `make_room` does NOT rename the debt
    /// that cursor already owed — one reconcile covers the cut too. So the loss alone still
    /// names the carried window while the position has moved: the pair is what refuses the
    /// settle. Without it, the successful rebuild would answer a reconcile it never performed
    /// and the cut would vanish with the resident reading as fresh.
    #[test]
    fn an_overflow_during_the_rebuild_window_is_not_settled_by_the_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        // The debt the rebuild carries, and the fresh cursor it takes at its start.
        hub.degrade_external();
        let carried = state.resubscribe_cursor().expect("the window's debt was carried");
        // The window is open, the rebuild in flight, and nobody drains the fresh cursor.
        lock_recover(&state.inner).reload = ReloadState::Running;
        overflow_the_cap(&hub);

        // The rebuild succeeds and tries to settle what it carried.
        lock_recover(&state.inner).reload = ReloadState::Idle;
        state.settle_rebuild_debt(Some(carried));

        let cursor = lock_recover(&state.hub_cursor).expect("the resident holds its cursor");
        assert!(
            matches!(hub.health_for(Some(cursor)), Health::Degraded(_)),
            "the cut must not be settled into silence",
        );

        // And the resident reads the cut: an owed reconcile puts the next read on the scan
        // path, where the baseline covers what the hub dropped.
        let scans = state.scan_count();
        let _ = state.read(|_, _| ());
        assert!(
            state.scan_count() > scans,
            "an owed reconcile after the cut has to make reads reconcile",
        );
    }

    /// Idle eviction releases the hub cursor so an evicted resident does not pin the
    /// accumulator against reclamation.
    ///
    /// The eviction is driven by calling `evict_if_idle` — the very step the sweeper
    /// takes — instead of raising the sweeper and waiting out its clock. What is under
    /// test is the release, not the schedule; and a sweeper armed before the resident
    /// exists makes the assertion BEFORE the eviction race the eviction itself, since
    /// the resident may vanish between the publication of `Ready` and the observation
    /// of it.
    #[test]
    fn eviction_releases_hub_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (mut state, _hub) = state_with_hub(root);
        state.without_idle_sweeper();
        state.eviction_after = Duration::ZERO;
        state.ensure_loading();
        wait_ready(&state);
        assert!(state.has_hub_cursor(), "a built resident holds a cursor");

        assert!(state.evict_if_idle(), "an untouched ready resident is evicted");
        assert_eq!(state.status(), DiagnosticsStatus::Idle, "eviction drops the resident");
        assert!(!state.has_hub_cursor(), "eviction drops the cursor");
    }

    /// An eviction that lands while a drain is in flight must not be undone by that drain.
    ///
    /// The drain takes the cursor, advances it at the hub and writes it back; if the
    /// eviction released the cursor in between, an unconditional write-back puts a handle
    /// to a dead subscription back on a resident that no longer exists.
    ///
    /// The release is driven by calling `drop_cursor` — the very call the idle sweeper
    /// makes — instead of waiting out `eviction_after`. Waiting would put the test at the
    /// mercy of the scheduler: it needs the drain parked BEFORE the release, and under a
    /// loaded machine the sweeper wins that race, evicts first, and the drain then finds
    /// no cursor to hold. What is under test is the write-back, not the sweeper's clock.
    #[test]
    fn a_drain_in_flight_never_resurrects_the_evicted_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);
        assert!(state.has_hub_cursor(), "a built resident holds a cursor");

        // Park the drain exactly where it holds a cursor it has not yet written back.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        state.set_pre_drain_probe(move || {
            entered_tx.send(()).expect("the test is still waiting");
            let _ = release_rx.recv();
        });

        // The drain path is entered directly. Going through `poll_pending_drift` would
        // make the test depend on the hub reporting healthy, and a degraded hub — which a
        // loaded machine running many watchers produces on its own — silently routes the
        // poll to the scan instead, where no drain is in flight and nothing is tested.
        let drainer = {
            let state = state.clone();
            let hub = hub.clone();
            let root = root.to_path_buf();
            std::thread::spawn(move || state.poll_drift_via_drain(&hub, &root))
        };
        entered_rx
            .recv_timeout(crate::change_hub::test_support::STATE_CEILING)
            .expect("the drain reaches the point where it holds the cursor");

        state.drop_cursor();
        assert!(!state.has_hub_cursor(), "the release lands while the drain is parked");

        release_tx.send(()).expect("the drain is still parked");
        drainer.join().expect("the drain finishes");
        assert!(!state.has_hub_cursor(), "an in-flight drain does not resurrect the cursor");
        // What the write-back can resurrect is the state's own handle, not the hub's
        // registration: `unsubscribe` removed the subscription and draining a cursor the
        // hub no longer knows never re-creates it. Asserted so a drain that starts doing
        // so — which WOULD pin the accumulator on an evicted resident — is caught here.
        assert_eq!(hub.active_cursor_count(), 0, "the release leaves no subscription behind");
    }

    /// `status_report` surfaces the hub view so an agent can tell an event-driven serve
    /// from a scan fallback.
    #[test]
    fn status_report_exposes_watch_mode() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let watch = state.status_report().watch.expect("a hub-backed profile reports watch");
        assert_eq!(watch.mode, "event-driven");
        assert_eq!(watch.health, "healthy");

        hub.degrade_external();
        let watch = state.status_report().watch.expect("watch report present");
        assert_eq!(watch.mode, "scan-fallback", "a degraded hub reports the scan fallback");
    }

    /// An edit that lands WHILE a full rebuild is in flight must not be dropped: the drain
    /// leaves it pending (rather than draining-then-bailing on the reload) and applies it to
    /// the fresh resident once the rebuild finishes — without waiting for the reconciler.
    /// With the old drain-before-reload-check order this test fails (the edit is lost).
    #[test]
    fn edit_during_rebuild_applies_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        // Simulate a full rebuild in flight.
        lock_recover(&state.inner).reload = ReloadState::Running;

        let mut observer = hub.subscribe();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(
            module_path(root, "Сервер"),
            "&НаСервере\nФункция Считать() Экспорт Возврат 11; КонецФункции\n",
        )
        .unwrap();
        wait_for_delivery(&hub, &mut observer, "Module.bsl", "hub delivered the edit");

        // A read during the rebuild must NOT drain/apply (else the edit is lost).
        let gen0 = raw_generation(&state);
        let _ = state.read(|_, _| ());
        assert_eq!(raw_generation(&state), gen0, "no apply while a rebuild is in flight");

        // The rebuild finishes.
        lock_recover(&state.inner).reload = ReloadState::Idle;

        // The still-pending edit now applies to the (current) resident on the next read.
        wait_for_apply(&state, gen0, "the pending edit applies once the rebuild ends");
        let text = state.read(|r, _| {
            let fid = r.file_id_for(&module_path(root, "Сервер")).unwrap();
            r.analysis().file_text(fid)
        });
        match text {
            ResidentOutcome::Ready(t, _) => assert!(t.contains("Возврат 11"), "edit applied"),
            _ => panic!("expected Ready"),
        }
    }

    /// A legitimate edit that lands DURING the reconciler's scan (delivered to the cursor,
    /// just after its first drain) must NOT be counted as a lossy-backend miss: the second
    /// drain covers it, so the hub stays Healthy. With the old single-drain reconciler this
    /// fails (the scan sees drift and degrades).
    #[test]
    fn reconciler_does_not_degrade_a_late_delivered_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        // The probe fires between the reconciler's first drain and its scan: it writes the
        // edit and waits for the hub to deliver it into the accumulator (so the diagnostics
        // cursor holds it for the reconciler's second drain).
        let probe_root = root.to_path_buf();
        let probe_hub = hub.clone();
        state.set_reconcile_probe(move || {
            // Subscribed before the write, so the delivery cannot pass ahead of the cursor.
            let mut obs = probe_hub.subscribe();
            fs::write(
                probe_root.join("CommonModules/Сервер/Ext/Module.bsl"),
                "&НаСервере\nФункция Считать() Экспорт Возврат 13; КонецФункции\n",
            )
            .unwrap();
            wait_until("the hub delivers the probe's write", || {
                let batch = probe_hub.drain(obs);
                obs = batch.cursor;
                if batch.entries.iter().any(|e| e.raw.to_string_lossy().contains("Module.bsl")) {
                    Ok(())
                } else {
                    Err("the write is not delivered yet")
                }
            });
        });

        let gen0 = raw_generation(&state);
        state.reconcile_tick();

        assert!(raw_generation(&state) > gen0, "the late edit is still applied");
        assert_eq!(
            hub.health(),
            Health::Healthy,
            "an edit delivered during the scan is not a miss and must not degrade",
        );
    }

    /// The containment rule the miss check leans on, pinned where it lives: an end-to-end
    /// deletion cannot pin it, because a directory removed through the filesystem also
    /// delivers an event per file, and those exact paths would answer for the descendants
    /// even if containment did nothing.
    mod delivered_coverage {
        use super::*;

        fn subtree_removal(path: &str) -> ChangeEntry {
            ChangeEntry {
                canonical: PathBuf::from(path),
                raw: PathBuf::from(path),
                kind: ChangeKind::SubtreeRemoved,
                seq: 1,
            }
        }

        #[test]
        fn a_vanished_directory_answers_for_its_descendants() {
            let delivered = DeliveredPaths::of(&[subtree_removal("/w/Dir")]);
            assert!(delivered.covers_removal("/w/Dir/A.bsl"));
            assert!(delivered.covers_removal("/w/Dir/Deep/B.bsl"));
            assert!(delivered.covers_removal("/w/Dir"), "and for itself");
        }

        #[test]
        fn a_namesake_directory_is_not_covered() {
            let delivered = DeliveredPaths::of(&[subtree_removal("/w/Dir")]);
            assert!(!delivered.covers_removal("/w/Dir2/A.bsl"));
        }

        /// The hub cannot tell a vanished directory from a vanished file when the name has a
        /// dot in it, so `Dir.v1` arrives as an ordinary removal while the scan still lists
        /// every file that was inside. Counting only the explicit kind would call all of
        /// them missed and charge every consumer of the hub a reconcile for it.
        #[test]
        fn a_dotted_directory_still_answers_for_its_descendants() {
            let delivered = DeliveredPaths::of(&[ChangeEntry {
                canonical: PathBuf::from("/w/Dir.v1"),
                raw: PathBuf::from("/w/Dir.v1"),
                kind: ChangeKind::MaybeRemoved,
                seq: 1,
            }]);
            let gone = WorkspaceDiff {
                added: vec![],
                removed: vec!["/w/Dir.v1/A.bsl".to_owned(), "/w/Dir.v1/B.bsl".to_owned()],
                modified: vec![],
            };
            assert!(
                !has_undelivered_drift(&gone, &delivered),
                "the removal of a dotted directory accounts for the files under it",
            );
        }

        /// An event has a direction. A directory reported gone accounts for what was under
        /// it disappearing — not for a file that appeared there afterwards, whose own event
        /// the watcher may well have lost. Checked through the miss decision itself, so it
        /// pins which rule that decision applies, not merely that both rules exist.
        #[test]
        fn a_vanished_directory_does_not_answer_for_what_appears_after_it() {
            let delivered = DeliveredPaths::of(&[subtree_removal("/w/Dir")]);
            let gone = WorkspaceDiff {
                added: vec![],
                removed: vec!["/w/Dir/A.bsl".to_owned()],
                modified: vec![],
            };
            assert!(
                !has_undelivered_drift(&gone, &delivered),
                "the vanished directory accounts for its descendants disappearing",
            );

            let reborn = WorkspaceDiff {
                added: vec!["/w/Dir/New.bsl".to_owned()],
                removed: vec![],
                modified: vec![],
            };
            assert!(
                has_undelivered_drift(&reborn, &delivered),
                "but not for a file that appeared there afterwards",
            );
        }

        /// Only a vanished DIRECTORY stands for what is under it. An ordinary change to a
        /// path covers that path alone — otherwise an edit inside a directory would vouch
        /// for files nobody reported.
        #[test]
        fn an_ordinary_entry_covers_only_itself() {
            let delivered = DeliveredPaths::of(&[ChangeEntry {
                canonical: PathBuf::from("/w/Dir"),
                raw: PathBuf::from("/w/Dir"),
                kind: ChangeKind::MaybeChanged,
                seq: 1,
            }]);
            assert!(delivered.covers("/w/Dir"));
            assert!(!delivered.covers("/w/Dir/A.bsl"));
        }
    }

    /// A batch that also demands a full reconcile still DELIVERED its paths, and the hub
    /// keeps them on every input but a channel overflow. Counting them as missed answers a
    /// reconcile with another one — charged to every consumer of the hub, not just this one.
    #[test]
    fn a_path_delivered_alongside_a_rescan_is_not_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        // The probe fires between the reconciler's first drain and its scan: it edits a
        // module, waits for the hub to hold it, then opens a reconcile window that does NOT
        // clear entries — the shape nine of the hub's ten inputs have.
        let probe_root = root.to_path_buf();
        let probe_hub = hub.clone();
        let after_probe = Arc::new(Mutex::new(0u64));
        let probe_seen = Arc::clone(&after_probe);
        state.set_reconcile_probe(move || {
            // Subscribed before the write, so the delivery cannot pass ahead of the cursor.
            let mut obs = probe_hub.subscribe();
            fs::write(
                probe_root.join("CommonModules/Сервер/Ext/Module.bsl"),
                "&НаСервере\nФункция Считать() Экспорт Возврат 17; КонецФункции\n",
            )
            .unwrap();
            wait_until("the hub delivers the probe's write", || {
                let batch = probe_hub.drain(obs);
                obs = batch.cursor;
                if batch.entries.iter().any(|e| e.raw.to_string_lossy().contains("Module.bsl")) {
                    Ok(())
                } else {
                    Err("the write is not delivered yet")
                }
            });
            probe_hub.unsubscribe(obs);
            probe_hub.degrade_external();
            *lock_recover(&probe_seen) = probe_hub.rescan_request_count();
        });

        state.reconcile_tick();

        assert_eq!(
            hub.rescan_request_count(),
            *lock_recover(&after_probe),
            "the reconciler asked for no reconcile of its own: the path was delivered",
        );
    }

    /// A snapshot and the baseline it is compared against must describe the same world.
    /// Once the baseline moves, their diff runs BACKWARDS: a file added since the snapshot
    /// was taken is absent from it, so it reads as a deletion and is applied as one.
    #[test]
    fn a_scan_taken_before_the_baseline_moved_is_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        // Resolved once, because the entry below stands for one the hub would deliver and
        // the hub spells its canonical key canonically. A temporary directory can be
        // reached through a symlinked component, and an entry keyed by the unresolved
        // spelling addresses a file the resident has never heard of.
        let root = &dir.path().canonicalize().unwrap();
        sample_workspace(root);

        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        // The snapshot, taken while the new module does not exist yet.
        let snapshot = state.throttled_scan(root).expect("a scan");

        // The baseline moves: the module is created and applied from the event stream.
        write_common_module(root, "Новый", true, "&НаСервере\nФункция Н() Экспорт КонецФункции");
        let added = module_path(root, "Новый");
        state.apply_drained_entries(&[ChangeEntry {
            canonical: added.clone(),
            raw: added.clone(),
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        }]);
        assert!(
            matches!(
                state.read(|r, _| r.file_id_for(&added).is_some()),
                ResidentOutcome::Ready(true, _)
            ),
            "the new module is in the resident before the stale scan is applied",
        );

        // The stale snapshot now describes a world without that module.
        let changes = {
            let inner = lock_recover(&state.inner);
            classify_changes(&inner.stats, &snapshot.stats)
        };
        assert!(
            changes.removed.iter().any(|p| p.contains("Новый")),
            "the stale snapshot really does read the new module as deleted",
        );
        state.apply_scan_drift(&changes, false, &snapshot);

        assert!(
            matches!(
                state.read(|r, _| r.file_id_for(&added).is_some()),
                ResidentOutcome::Ready(true, _)
            ),
            "a snapshot older than the baseline is refused, not applied backwards",
        );
    }

    /// Refusing a snapshot answers nothing: whatever made this scan run — a reconcile debt
    /// the drain already cleared, a consumed force — is spent. A healthy cursor then sends
    /// every following read down the drain path, so the drift the hub lost would sit
    /// unapplied, and unreported, until the watchdog tick.
    #[test]
    fn a_refused_scan_re_arms_the_obligation_to_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let snapshot = state.throttled_scan(root).expect("a scan");
        write_common_module(
            root,
            "Двигатель",
            true,
            "&НаСервере\nФункция Д() Экспорт КонецФункции",
        );
        let moved = module_path(root, "Двигатель");
        state.apply_drained_entries(&[ChangeEntry {
            canonical: moved.clone(),
            raw: moved,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        }]);

        let changes = {
            let inner = lock_recover(&state.inner);
            classify_changes(&inner.stats, &snapshot.stats)
        };
        state.apply_scan_drift(&changes, false, &snapshot);

        let scans = state.scan_count();
        let _ = state.read(|_, _| ());
        assert!(
            state.scan_count() > scans,
            "the refused snapshot leaves the obligation to scan standing",
        );
    }

    /// Re-arming alone does not make the next poll walk. `throttled_scan` caches whatever it
    /// produced, so a walk that STARTED before the baseline moved lands its snapshot in the
    /// cache after it — and every read inside the drift interval is then handed the same
    /// snapshot and refuses it again. The epoch only grows, so it can never become
    /// applicable: the refusal has to drop it.
    #[test]
    fn a_refused_snapshot_does_not_stay_in_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (mut state, _hub) = state_with_hub(root);
        state.drift_interval = Duration::from_secs(60);
        state.ensure_loading();
        wait_ready(&state);

        let snapshot = state.throttled_scan(root).expect("a scan");
        write_common_module(root, "Обгон", true, "&НаСервере\nФункция О() Экспорт КонецФункции");
        let overtaken = module_path(root, "Обгон");
        state.apply_drained_entries(&[ChangeEntry {
            canonical: overtaken.clone(),
            raw: overtaken,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        }]);

        // A walk that began before the move finishing after it: its snapshot is cached
        // carrying the older epoch.
        *lock_recover(&state.scan) = Some(ScanCache {
            at: Instant::now(),
            stats: snapshot.stats.clone(),
            config_fp: snapshot.config_fp,
            baseline_epoch: snapshot.baseline_epoch,
            verdict: snapshot.verdict,
        });

        // Set directly rather than through `force_rescan`, whose storm guard declines to arm
        // anything while the cache is fresh — which is exactly the state under test.
        state.force_scan.store(true, Ordering::SeqCst);
        let _ = state.read(|_, _| ());
        let scans = state.scan_count();
        let _ = state.read(|_, _| ());

        assert!(
            state.scan_count() > scans,
            "the refused snapshot is gone, so the re-armed scan actually walks",
        );
    }

    /// The largest baseline move there is: a rebuild replaces `stats` wholesale. A snapshot
    /// from before it would rebase the freshly built baseline back onto its own older view.
    #[test]
    fn a_scan_taken_before_a_rebuild_is_not_applied_to_the_new_resident() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let snapshot = state.throttled_scan(root).expect("a scan");
        write_common_module(root, "После", true, "&НаСервере\nФункция П() Экспорт КонецФункции");

        state.kick_full_reload();
        wait_until("the kicked rebuild finishes", || match lock_recover(&state.inner).reload {
            ReloadState::Running => Err("running"),
            _ => Ok(()),
        });
        wait_ready(&state);
        let built = module_path(root, "После");
        assert!(
            matches!(
                state.read(|r, _| r.file_id_for(&built).is_some()),
                ResidentOutcome::Ready(true, _)
            ),
            "the rebuild picked the module up",
        );

        let changes = {
            let inner = lock_recover(&state.inner);
            classify_changes(&inner.stats, &snapshot.stats)
        };
        state.apply_scan_drift(&changes, false, &snapshot);

        assert!(
            matches!(
                state.read(|r, _| r.file_id_for(&built).is_some()),
                ResidentOutcome::Ready(true, _)
            ),
            "a rebuild's baseline is not rebased onto a snapshot that predates it",
        );
    }

    /// A snapshot the baseline has outrun describes another world, and no part of its drift —
    /// a config edit included — may be applied to this one. The rebuild a stale
    /// `config_changed` would ask for is built over a tree that walk never read, and arrives
    /// on evidence that has already been invalidated: the flag comes from a comparison
    /// against a baseline that has since moved.
    ///
    /// Postponing it is not losing it. The refusal re-arms the scan and drops the snapshot, so
    /// the next walk reads the edit against the CURRENT baseline and asks for the rebuild it
    /// really owes — the control below.
    #[test]
    fn a_stale_config_snapshot_does_not_rebuild_but_re_arms_the_scan() {
        use ide::DiagnosticCode;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (mut state, _hub) = state_with_hub(root);
        // A real drift interval, so a snapshot LEFT in the cache would be served: the
        // assertion below that the refusal drops it is about behaviour, not a field that a
        // zero interval would never read anyway.
        state.drift_interval = Duration::from_secs(60);
        state.ensure_loading();
        wait_ready(&state);

        // The control needs a diagnostic ENABLED under the config the resident serves, so
        // its landing is evidence that the rebuild happened at all.
        assert!(
            matches!(
                state
                    .read(|r, _| r.config().is_disabled(DiagnosticCode::CanonicalSpellingKeywords)),
                ResidentOutcome::Ready(false, _)
            ),
            "the stand needs a diagnostic that is enabled by default",
        );

        let snapshot = state.throttled_scan(root).expect("a scan");
        // Move the baseline out from under the snapshot.
        write_common_module(root, "Сдвиг", true, "&НаСервере\nФункция С() Экспорт КонецФункции");
        let moved = module_path(root, "Сдвиг");
        state.apply_drained_entries(&[ChangeEntry {
            canonical: moved.clone(),
            raw: moved,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        }]);
        lock_recover(&state.inner).reload = ReloadState::Idle;
        // A walk that began before the move finishing after it: its snapshot is cached
        // carrying the older epoch, exactly as `throttled_scan` caches unconditionally.
        *lock_recover(&state.scan) = Some(ScanCache {
            at: Instant::now(),
            stats: snapshot.stats.clone(),
            config_fp: snapshot.config_fp,
            baseline_epoch: snapshot.baseline_epoch,
            verdict: snapshot.verdict,
        });

        let rebuilds = state.rebuilds_started.load(Ordering::SeqCst);
        let no_file_drift =
            WorkspaceDiff { added: Vec::new(), removed: Vec::new(), modified: Vec::new() };
        state.apply_scan_drift(&no_file_drift, true, &snapshot);

        assert_eq!(
            lock_recover(&state.inner).reload,
            ReloadState::Idle,
            "a stale snapshot's config drift may not start a rebuild",
        );
        assert_eq!(
            state.rebuilds_started.load(Ordering::SeqCst),
            rebuilds,
            "and no rebuild was started at all",
        );
        assert!(
            state.force_scan.load(Ordering::SeqCst),
            "the refusal has to re-arm the scan: nothing else still remembers the obligation",
        );
        assert!(
            lock_recover(&state.scan).is_none(),
            "the refused snapshot does not stay in the cache",
        );

        // Positive control: with a real config edit on disk the re-armed scan asks for the
        // rebuild. The config drift was postponed, never lost.
        write(
            root,
            "bsl-analyzer.toml",
            "[diagnostics.parameters]\nCanonicalSpellingKeywords = false\n",
        );
        let scans = state.scan_count();
        wait_until("the re-armed scan kicks the config rebuild", || {
            match state
                .read(|r, _| r.config().is_disabled(DiagnosticCode::CanonicalSpellingKeywords))
            {
                ResidentOutcome::Ready(true, _) => Ok(()),
                _ => Err("the config edit has not landed"),
            }
        });
        assert!(
            state.scan_count() > scans,
            "the re-armed obligation was honoured by a real walk, not a cache hit",
        );
        assert!(
            state.rebuilds_started.load(Ordering::SeqCst) > rebuilds,
            "the fresh config scan kicks the rebuild the stale one was refused",
        );
    }

    /// Applying entries moves the baseline, which leaves every cached snapshot describing
    /// the world before the move. The cache must go with it, or the next scan inside the
    /// drift interval is served a snapshot the baseline has already outrun.
    #[test]
    fn applying_entries_drops_the_scan_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (mut state, _hub) = state_with_hub(root);
        state.drift_interval = Duration::from_secs(60);
        state.ensure_loading();
        wait_ready(&state);
        state.throttled_scan(root).expect("a scan warms the cache");

        write_common_module(root, "Свежий", true, "&НаСервере\nФункция Св() Экспорт КонецФункции");
        let fresh = module_path(root, "Свежий");
        let scans = state.scan_count();
        state.apply_drained_entries(&[ChangeEntry {
            canonical: fresh.clone(),
            raw: fresh,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        }]);
        state.throttled_scan(root).expect("a scan");

        assert!(
            state.scan_count() > scans,
            "the next scan walks instead of reusing a snapshot the baseline outran",
        );
    }

    /// A read whose drain comes back demanding a reconcile answers with a scan — but that
    /// scan is served from a cache up to `drift_interval` old, which can predate the very
    /// entries the batch delivered. Applying them is what keeps the read from waiting for
    /// the cache to cool.
    #[test]
    fn a_read_applies_the_entries_delivered_with_a_rescan_demand() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (mut state, hub) = state_with_hub(root);
        state.drift_interval = Duration::from_secs(60);
        state.ensure_loading();
        wait_ready(&state);
        // Warm the scan cache, so the scan the rescan branch performs is served from a
        // snapshot older than the file the probe is about to create.
        state.force_rescan();
        let _ = state.read(|_, _| ());

        // The debt is raised INSIDE the window between the health decision and the drain:
        // raised any earlier, the read would take the scan path and never drain at all.
        let probe_root = root.to_path_buf();
        let probe_hub = hub.clone();
        state.set_pre_drain_probe(move || {
            // Subscribed before the write, so the delivery cannot pass ahead of the cursor.
            let mut obs = probe_hub.subscribe();
            write_common_module(
                &probe_root,
                "Внезапный",
                true,
                "&НаСервере\nФункция Внезапная() Экспорт КонецФункции",
            );
            wait_until("the hub delivers the probe's write", || {
                let batch = probe_hub.drain(obs);
                obs = batch.cursor;
                if batch.entries.iter().any(|e| e.raw.to_string_lossy().contains("Внезапный"))
                {
                    Ok(())
                } else {
                    Err("the write is not delivered yet")
                }
            });
            probe_hub.unsubscribe(obs);
            probe_hub.degrade_external();
        });

        let _ = state.read(|_, _| ());

        let sudden = module_path(root, "Внезапный");
        let found = state.read(|resident, _| resident.file_id_for(&sudden).is_some());
        assert!(
            matches!(found, ResidentOutcome::Ready(true, _)),
            "the entries delivered with the rescan demand are applied, not dropped",
        );
    }

    /// The scan is what covers whatever the hub DROPPED, so it runs whether or not the
    /// batch also carried entries. An empty batch is the case where nothing else would.
    #[test]
    fn a_read_still_scans_when_the_rescan_demand_carries_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let probe_hub = hub.clone();
        state.set_pre_drain_probe(move || probe_hub.degrade_external());

        let scans = state.scan_count();
        let _ = state.read(|_, _| ());

        assert!(
            state.scan_count() > scans,
            "a reconcile demand is answered by a walk even with nothing to apply",
        );
    }

    /// The tick clears its scan cache ONCE, and the snapshot it then takes immediately
    /// becomes the warm cache the second drain's scan is served from. An event delivered
    /// after that snapshot is therefore in neither the snapshot nor step 4's diff: without
    /// applying the batch's own entries it waits for the cache to cool.
    #[test]
    fn an_event_delivered_after_the_scan_is_applied_in_the_same_tick() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (mut state, hub) = state_with_hub(root);
        // A warm cache is the whole point: at the test harness's zero interval the second
        // drain's scan would be a fresh walk that sees the late file by itself.
        state.drift_interval = Duration::from_secs(60);
        state.ensure_loading();
        wait_ready(&state);

        // Drift that exists BEFORE the scan, so the tick does not return on an empty diff
        // and never reaches the second drain at all.
        lock_recover(&state.inner).stats.insert(root.join("Vanished.bsl").display().to_string(), 1);

        let probe_root = root.to_path_buf();
        let probe_hub = hub.clone();
        state.set_post_scan_probe(move || {
            // Subscribed before the write, so the delivery cannot pass ahead of the cursor.
            let mut obs = probe_hub.subscribe();
            write_common_module(
                &probe_root,
                "Поздний",
                true,
                "&НаСервере\nФункция Поздняя() Экспорт КонецФункции",
            );
            wait_until("the hub delivers the probe's write", || {
                let batch = probe_hub.drain(obs);
                obs = batch.cursor;
                if batch.entries.iter().any(|e| e.raw.to_string_lossy().contains("Поздний"))
                {
                    Ok(())
                } else {
                    Err("the write is not delivered yet")
                }
            });
            probe_hub.unsubscribe(obs);
            probe_hub.degrade_external();
        });

        state.reconcile_tick();

        let late = module_path(root, "Поздний");
        // The BASELINE is the observable for this path, not the resident. Step 4 applies the
        // diff computed at step 2, and removals from the resident come only from that diff's
        // own list — the late file is not in it, so the resident keeps it either way. What
        // step 4 does move is the baseline, wholesale, onto its pre-move snapshot.
        assert!(
            lock_recover(&state.inner).stats.keys().any(|p| p.contains("Поздний")),
            "the late file is in the baseline: the stale snapshot did not rebase it away",
        );
        let found = state.read(|resident, _| resident.file_id_for(&late).is_some());
        assert!(
            matches!(found, ResidentOutcome::Ready(true, _)),
            "and it is in the resident, not waiting for the cache to cool",
        );
    }

    /// The drain names a vanished directory and nothing else — the descendants it stood for
    /// are exactly what can no longer be enumerated, while the scan lists every one of them.
    /// Matching delivered paths by equality alone therefore calls every descendant a miss.
    #[test]
    fn the_descendants_of_a_delivered_vanished_directory_are_not_missed() {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, because the drain entry this stand looks for carries the resolved
        // spelling — that is the key the hub guarantees against the scan universe — and a
        // temporary directory can be reached through a link component.
        let root = &dir.path().canonicalize().unwrap();
        write_common_module(root, "Первый", true, "&НаСервере\nФункция А() Экспорт КонецФункции");
        write_common_module(root, "Второй", true, "&НаСервере\nФункция Б() Экспорт КонецФункции");

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let probe_root = root.to_path_buf();
        let probe_hub = hub.clone();
        let after_probe = Arc::new(Mutex::new(0u64));
        let probe_seen = Arc::clone(&after_probe);
        state.set_reconcile_probe(move || {
            let vanished = probe_root.join("CommonModules");
            let obs = probe_hub.subscribe();
            fs::remove_dir_all(&vanished).unwrap();
            // The input this stand is about, stated rather than waited for: the drain names
            // the vanished DIRECTORY and nothing else. Waiting for the backend to name it
            // is what makes the stand measure the platform — under load FSEvents has been
            // measured naming `Первый`, `Первый/Ext` and their bodies while never naming
            // `CommonModules` above them, and never naming `Первый.xml` either. The
            // reconciler is then right to charge a miss (that `.xml` removal really was not
            // delivered), so the stand fails over an input it never meant to have. Whether
            // the backend reports a vanished directory at all is `change_hub`'s own subject.
            probe_hub.deliver_vanished_for_test(&vanished);
            let batch = probe_hub.drain(obs);
            let named = batch
                .entries
                .iter()
                .any(|e| e.kind == ChangeKind::SubtreeRemoved && e.canonical == vanished);
            probe_hub.unsubscribe(batch.cursor);
            assert!(named, "the drain names the vanished directory: {:?}", batch.entries);
            probe_hub.degrade_external();
            *lock_recover(&probe_seen) = probe_hub.rescan_request_count();
        });

        state.reconcile_tick();

        assert_eq!(
            hub.rescan_request_count(),
            *lock_recover(&after_probe),
            "the vanished directory covers the files that went with it",
        );
    }

    /// A `bsl-analyzer.toml` in a SUBDIRECTORY is not the analyzer config (which lives at the
    /// workspace root): the drain must ignore it, matching the scan path's `config_files_fingerprint`
    /// which only fingerprints `root.join(name)`. A subtree toml edit is not a rebuild trigger.
    #[test]
    fn subdir_config_file_is_not_config_drift() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);
        // A toml deep in the tree, NOT the root analyzer config.
        write(root, "CommonModules/Сервер/bsl-analyzer.toml", "[diagnostics]\n");

        let (state, _hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let subdir_toml = root.join("CommonModules/Сервер/bsl-analyzer.toml");
        let canonical = subdir_toml.canonicalize().unwrap_or(subdir_toml);
        let entry = ChangeEntry {
            canonical: canonical.clone(),
            raw: canonical,
            kind: ChangeKind::MaybeChanged,
            seq: 1,
        };

        let gen0 = raw_generation(&state);
        // Feeding the subdir toml as drift must NOT kick a full rebuild.
        state.apply_drained_entries(&[entry]);
        assert_eq!(
            state.status_report().reload,
            "none",
            "a toml outside the workspace root is not analyzer-config drift",
        );
        assert_eq!(raw_generation(&state), gen0, "no structural rebuild for a subtree toml");
    }

    /// A `.bsl` edit in an EXTENSION root (disjoint from the config source root) is delivered
    /// through the drain, because the hub watches every scan root — not just the source one.
    /// Without extension coverage this drift would be invisible until the 90s reconciler.
    #[test]
    fn extension_root_edit_lands_via_drain() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Nested layout: config source under `src/cf`, with an explicitly-scoped extension
        // beside it so the test exercises the drain path with a validated Project.
        let cf = root.join("src/cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(&cf, "Сервер", true, "&НаСервере\nФункция Ч() Экспорт КонецФункции");
        let ext = root.join("src/cfe/Расш");
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(
            &ext,
            "РасшМодуль",
            true,
            "&НаСервере\nФункция Р() Экспорт КонецФункции",
        );
        fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [{ name = \"Расш\", path = \"src/cfe/Расш\", scope = \"InfoBase\" }]\n",
        )
        .unwrap();

        // Build the hub over the SAME targets production arms (source + extensions,
        // plus the workspace root non-recursively) — a narrower boot set would be
        // upgraded by the resident's post-publish re-arm, and that one-time rescan
        // debt is exactly what this zero-scan assertion must not see.
        let project = project_model::Project::new(root).expect("valid test project");
        let roots = project.source_roots();
        assert!(roots.len() >= 2, "the scoped extension root must be declared: {roots:?}");
        let hub =
            WorkspaceChangeHub::start_targets(crate::change_hub::watch_targets_for(root, &roots));
        assert!(hub.wait_until_watching(), "the hub must arm");

        let mut state =
            DiagnosticsState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);

        let ext_module = ext.join("CommonModules/РасшМодуль/Ext/Module.bsl");
        let resident = state.read(|r, _| r.file_id_for(&ext_module).is_some());
        assert!(
            matches!(resident, ResidentOutcome::Ready(true, _)),
            "the extension module must be resident",
        );

        let gen0 = raw_generation(&state);
        std::thread::sleep(Duration::from_millis(10));
        fs::write(&ext_module, "&НаСервере\nФункция Р() Экспорт Возврат 9; КонецФункции\n")
            .unwrap();

        wait_for_apply(&state, gen0, "an extension-root edit is delivered via drain");
        assert_eq!(state.scan_count(), 0, "the event-driven path performs no scan");
    }

    /// `BSL_MCP_RECONCILE_SECS` clamping: `0` and garbage fall back to the default; small
    /// positive values are floored so the sweeper cannot busy-loop; valid values pass through.
    #[test]
    fn reconcile_interval_clamps_bad_env() {
        assert_eq!(clamp_reconcile_interval(None), RECONCILE_INTERVAL, "unset/garbage → default");
        assert_eq!(clamp_reconcile_interval(Some(0)), RECONCILE_INTERVAL, "zero → default");
        assert_eq!(clamp_reconcile_interval(Some(1)), MIN_RECONCILE_INTERVAL, "floored");
        assert_eq!(clamp_reconcile_interval(Some(4)), MIN_RECONCILE_INTERVAL, "floored");
        assert_eq!(clamp_reconcile_interval(Some(5)), Duration::from_secs(5), "at the floor");
        assert_eq!(clamp_reconcile_interval(Some(120)), Duration::from_secs(120), "passthrough");
        // Unparseable env text becomes `None` before clamping.
        assert_eq!("nonsense".parse::<u64>().ok(), None);
    }

    /// A delivered file outside the scan universe (an editor temp file) is a no-op for the
    /// diagnostics drain: it touches no resident input and triggers no scan on the healthy
    /// hot path. `apply_drained_entries` already ignores non-`.bsl`/`.xml`/config paths.
    #[test]
    fn non_scan_file_delivery_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let (state, hub) = state_with_hub(root);
        state.ensure_loading();
        wait_ready(&state);

        let mut observer = hub.subscribe();
        std::thread::sleep(Duration::from_millis(10));
        fs::write(root.join("CommonModules/Сервер/Ext/Module.bsl.tmp"), "editor swap").unwrap();
        wait_for_delivery(&hub, &mut observer, ".tmp", "hub delivered the temp file");

        let gen0 = raw_generation(&state);
        // A read drains the diagnostics cursor (which holds the .tmp), and must apply nothing.
        let _ = state.read(|_, _| ());
        assert_eq!(raw_generation(&state), gen0, "a non-scan file does not move the resident");
        assert_eq!(state.scan_count(), 0, "and triggers no scan on the healthy path");
    }

    /// What a scan may assert, per verdict. The clean row is the positive control: a
    /// narrowing that refused everything would pass the other three.
    ///
    /// The combined row is not implied by the two single-counter ones: an implementation
    /// asking about coverage FIRST answers the axes correctly and still lets an addition
    /// through when identity is degraded too. No filesystem stand can catch that — none
    /// of them produces a canonicalisation fallback.
    #[test]
    fn a_scan_asserts_only_what_its_walk_can_vouch_for() {
        let drift = || WorkspaceDiff {
            added: vec!["Новый.bsl".to_string()],
            removed: vec!["Ушедший.bsl".to_string()],
            modified: vec!["Правленый.bsl".to_string()],
        };
        let buckets = |d: WorkspaceDiff| (d.added.len(), d.removed.len(), d.modified.len());
        let verdict = ScanVerdict::for_test;

        assert_eq!(
            buckets(drift_the_scan_may_assert(drift(), verdict(0, 0))),
            (1, 1, 1),
            "a complete walk with exact keys speaks for the whole tree",
        );
        assert_eq!(
            buckets(drift_the_scan_may_assert(drift(), verdict(1, 0))),
            (1, 0, 1),
            "a file it did not reach is not a file that is gone",
        );
        assert_eq!(
            buckets(drift_the_scan_may_assert(drift(), verdict(0, 1))),
            (0, 0, 1),
            "a shifted key arrives as a removal and an addition; neither half is true",
        );
        assert_eq!(
            buckets(drift_the_scan_may_assert(drift(), verdict(1, 1))),
            (0, 0, 1),
            "losing coverage on top of identity grants no stronger claim",
        );
    }

    /// Incomplete coverage additionally drops whatever would restate a root's structure,
    /// because the applier behind those paths re-discovers the whole root by walking it.
    /// Both members of that class are checked here — a descriptor and a body under a
    /// listing that names it — and so is the line between them: a body EDIT reaches no
    /// re-discovery and must survive, or the resident would freeze on every edit while
    /// one subtree is unreadable.
    #[test]
    fn an_incomplete_walk_may_change_text_but_not_restate_a_root() {
        let drift = || WorkspaceDiff {
            added: vec![
                "cf/Catalogs/Товары.xml".to_string(),
                "cf/CommonModules/Новый/Ext/Module.bsl".to_string(),
                "cf/Catalogs/Товары/Ext/ObjectModule.bsl".to_string(),
            ],
            removed: Vec::new(),
            modified: vec![
                "cf/Catalogs/Склады.xml".to_string(),
                "cf/CommonModules/Сервер/Ext/Module.bsl".to_string(),
            ],
        };

        let complete = drift_the_scan_may_assert(drift(), ScanVerdict::for_test(0, 0));
        assert_eq!(complete.added.len(), 3, "a complete walk restates whatever it likes");
        assert_eq!(complete.modified.len(), 2);

        let short = drift_the_scan_may_assert(drift(), ScanVerdict::for_test(1, 0));
        assert_eq!(
            short.added,
            vec!["cf/Catalogs/Товары/Ext/ObjectModule.bsl".to_string()],
            "a descriptor and a listed body would re-discover their root; an object \
             module is registered without touching the listing",
        );
        assert_eq!(
            short.modified,
            vec!["cf/CommonModules/Сервер/Ext/Module.bsl".to_string()],
            "an edited descriptor re-discovers its root, an edited body re-reads itself",
        );
    }

    /// The baseline of a scan that may not speak for everything moves only where the scan
    /// had something to say. Keys it never listed keep their fingerprints — they are the
    /// only record that those files were ever there.
    #[test]
    fn an_incomplete_scan_moves_the_baseline_only_where_it_applied() {
        let baseline = || {
            HashMap::from([
                ("Правленый.bsl".to_string(), 1u64),
                ("Скрыт.bsl".to_string(), 2u64),
                ("Отложен.xml".to_string(), 3u64),
            ])
        };
        let edited = FileStat::for_test("Правленый.bsl", 77, 5);
        let postponed = FileStat::for_test("Отложен.xml", 88, 6);
        let new_fp: HashMap<&str, u64> = [
            (edited.path.as_str(), edited.fingerprint()),
            (postponed.path.as_str(), postponed.fingerprint()),
        ]
        .into_iter()
        .collect();

        let mut stats = baseline();
        rebase_over_incomplete_scan(&mut stats, ["Правленый.bsl"].into_iter(), &new_fp);
        assert_eq!(
            stats.get("Правленый.bsl"),
            Some(&edited.fingerprint()),
            "the key whose drift was applied moves to what the scan saw",
        );
        assert_eq!(
            stats.get("Скрыт.bsl"),
            Some(&2),
            "a key the scan never listed keeps the only record that it exists",
        );
        assert_eq!(
            stats.get("Отложен.xml"),
            Some(&3),
            "and a key whose drift was refused keeps its old value, or the refusal \
             would read as a cancellation: the next scan would see no drift at all",
        );

        // Control on the shape: apply every drifted key and the merge must agree with
        // the wholesale rebase over them — an implementation that preserves everything
        // fails here.
        let mut stats = baseline();
        rebase_over_incomplete_scan(
            &mut stats,
            ["Правленый.bsl", "Отложен.xml"].into_iter(),
            &new_fp,
        );
        assert_eq!(
            stats,
            HashMap::from([
                ("Правленый.bsl".to_string(), edited.fingerprint()),
                ("Отложен.xml".to_string(), postponed.fingerprint()),
                ("Скрыт.bsl".to_string(), 2),
            ]),
        );
    }

    /// A scan that could not read part of the tree saw LESS than the tree holds, so the
    /// files it did not list are not gone — they are unproven. Reconciling against such a
    /// scan unregisters live files, loses their baseline keys and blames the change hub
    /// for deliveries it never owed.
    #[cfg(unix)]
    mod incomplete_scan {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        /// Hide a directory from the walk. `false` when permissions do not bind this
        /// user (UID 0): the input cannot exist, so the test has nothing to assert.
        fn close_dir(path: &std::path::Path) -> bool {
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::read_dir(path).is_ok() {
                open_dir(path);
                return false;
            }
            true
        }

        fn open_dir(path: &std::path::Path) {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// Whether a full rebuild has been asked for, ever: one asked for marks the reload
        /// running before its thread starts, and the thread counts itself before it lets the
        /// mark go, so at any moment after the ask one of the two shows it.
        fn a_rebuild_was_asked_for(state: &DiagnosticsState) -> bool {
            lock_recover(&state.inner).reload == ReloadState::Running
                || state.rebuilds_started.load(std::sync::atomic::Ordering::SeqCst) > 0
        }

        fn file_id(state: &DiagnosticsState, path: &std::path::Path) -> Option<vfs::FileId> {
            match state.read(|resident, _| resident.file_id_for(path)) {
                ResidentOutcome::Ready(id, _) => id,
                _ => panic!("expected Ready outcome"),
            }
        }

        /// A module whose directory became unreadable is still on disk: it keeps its
        /// registration and its `FileId`. The positive control is the second half — with
        /// the directory readable again, a REAL removal does unregister it.
        #[test]
        fn an_unreadable_directory_does_not_unregister_a_live_module() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");

            let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
            state.drift_interval = Duration::from_millis(0);
            state.ensure_loading();
            wait_ready(&state);

            let hidden = module_path(root, "Скрытый");
            let before = file_id(&state, &hidden).expect("the module resolves before the hiding");

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));

            assert_eq!(
                file_id(&state, &hidden),
                Some(before),
                "a directory the scan cannot enter is not a removal: the file is still there",
            );

            open_dir(&closed);
            fs::remove_file(&hidden).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            assert_eq!(
                file_id(&state, &hidden),
                None,
                "a real removal seen by a complete scan still unregisters the module",
            );
        }

        /// The same class through the metadata path: a `.xml` hidden by an unreadable
        /// directory must not un-discover its object (that route is the substrate
        /// point-refresh, not the `.bsl` removal bucket).
        #[test]
        fn an_unreadable_directory_does_not_undiscover_a_metadata_object() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_catalog(root, "Товары", 9);

            let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
            state.drift_interval = Duration::from_millis(0);
            state.ensure_loading();
            wait_ready(&state);

            let module = module_path(root, "Сервер");
            assert!(catalog_resolves(&state, &module), "the catalog resolves before the hiding");

            let closed = root.join("Catalogs");
            if !close_dir(&closed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));

            assert!(
                catalog_resolves(&state, &module),
                "a descriptor the scan cannot reach is not a deleted descriptor",
            );

            open_dir(&closed);
            fs::remove_file(root.join("Catalogs/Товары.xml")).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            assert!(
                !catalog_resolves(&state, &module),
                "a real removal seen by a complete scan still tombstones the object",
            );
        }

        /// An edit elsewhere makes the baseline move WHILE part of the tree is hidden —
        /// the only way the rebase runs at all, since hiding a directory on its own
        /// produces no drift the scan may assert. The hidden file's key has to survive
        /// that move: it is the sole record that the file was ever there, and without it
        /// the real removal below has nothing to be compared against.
        #[test]
        fn an_edit_beside_a_hidden_subtree_keeps_its_keys() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");

            let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
            state.drift_interval = Duration::from_millis(0);
            state.ensure_loading();
            wait_ready(&state);

            let hidden = module_path(root, "Скрытый");
            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }

            std::thread::sleep(Duration::from_millis(10));
            fs::write(
                module_path(root, "Сервер"),
                "&НаСервере\nФункция Считать() Экспорт Возврат 7; КонецФункции\n",
            )
            .unwrap();
            // Apply that edit: this is the pass whose baseline move must not take the
            // hidden key with it.
            let _ = state.read(|_, _| ());

            open_dir(&closed);
            fs::remove_file(&hidden).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            assert_eq!(
                file_id(&state, &hidden),
                None,
                "the removal is only visible if the baseline still remembered the file",
            );
        }

        /// A config edit is the one drift that cannot be applied in place, so it goes
        /// through a full rebuild — and a rebuild is a wholesale assertion about which
        /// files exist. Built over a tree it could not fully read, it would replace the
        /// serving resident with a shorter one, dropping live files by the back door the
        /// narrowing closed at the front.
        ///
        /// The control is the second half: once the tree is readable the config change
        /// must actually land, or the refusal would be a silent freeze.
        #[test]
        fn a_config_edit_does_not_rebuild_over_a_tree_it_cannot_read() {
            use ide::DiagnosticCode;

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

            let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
            state.drift_interval = Duration::from_millis(0);
            state.ensure_loading();
            wait_ready(&state);

            let hidden = module_path(root, "Скрытый");
            let before = file_id(&state, &hidden).expect("the module resolves before the hiding");

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            edit_moving_mtime(&root.join("bsl-analyzer.toml"), || {
                write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n")
            });

            // Every read scans and decides on the spot; only the rebuild it might ask for runs
            // elsewhere, and asking for one is itself visible at once.
            for _ in 0..50 {
                let _ = state.read(|_, _| ());
            }
            assert_eq!(
                file_id(&state, &hidden),
                Some(before),
                "a rebuild over a half-read tree must not replace the resident",
            );
            // And it was not merely refused at the swap: fifty windows of a config edit
            // that cannot be applied must not have asked for fifty resident builds. Only the
            // operation's own record can say so — a declined build leaves nothing behind.
            assert!(
                !a_rebuild_was_asked_for(&state),
                "a rebuild that would be declined is not started at all",
            );

            open_dir(&closed);
            wait_until("the postponed config change lands once the tree can be read", || {
                let disabled = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
                if matches!(disabled, ResidentOutcome::Ready(false, _)) {
                    Ok(())
                } else {
                    Err("the resident does not serve the edited config yet")
                }
            });
            assert!(
                file_id(&state, &hidden).is_some(),
                "and the module the rebuild once dropped is back in the universe",
            );
        }

        /// The substrate refresh is not the point operation its arguments suggest: it
        /// re-discovers every affected config root by walking it, then publishes the
        /// listing it found. So one permitted `.xml` edit hands a half-read tree the
        /// authority to restate a whole root, and the objects behind the closed door
        /// vanish from the listing — the loss the narrowing refused, arriving through the
        /// applier instead of the diff.
        #[test]
        fn a_visible_xml_edit_does_not_restate_a_root_through_a_closed_door() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_catalog(root, "Товары", 9);
            write(root, "Configuration.xml", "<Configuration><Name>Конфа</Name></Configuration>");

            let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
            state.drift_interval = Duration::from_millis(0);
            state.ensure_loading();
            wait_ready(&state);

            let module = module_path(root, "Сервер");
            assert!(catalog_resolves(&state, &module), "the catalog resolves before the hiding");

            let closed = root.join("Catalogs");
            if !close_dir(&closed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
            write(root, "Configuration.xml", "<Configuration><Name>Другая</Name></Configuration>");
            let _ = state.read(|_, _| ());

            assert!(
                catalog_resolves(&state, &module),
                "an edit the scan may assert does not license restating what it could not read",
            );

            open_dir(&closed);
            std::thread::sleep(Duration::from_millis(10));
            let _ = state.read(|_, _| ());
            assert!(
                catalog_resolves(&state, &module),
                "and the postponed edit lands without costing the object",
            );
        }

        /// The same rebuild reached through the event path, which holds no scan and so
        /// cannot decline to ask for one. Here the refusal has to live at the swap: the
        /// build knows what its own walk saw, and a build that saw part of the tree may
        /// not retire a resident that saw all of it.
        ///
        /// Deliberately paired with the scan-path test above rather than folded into it:
        /// the two are stopped by different code, and one test passing for either reason
        /// would leave the other half unguarded.
        #[test]
        fn a_delivered_config_edit_does_not_rebuild_over_a_tree_it_cannot_read() {
            use ide::DiagnosticCode;

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

            let (state, hub) = state_with_hub(root);
            state.ensure_loading();
            wait_ready(&state);

            let hidden = module_path(root, "Скрытый");
            let before = file_id(&state, &hidden).expect("the module resolves before the hiding");

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            let mut observer = hub.subscribe();
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n");
            // Without this the test could pass having never triggered a rebuild at all.
            wait_for_delivery(
                &hub,
                &mut observer,
                "bsl-analyzer.toml",
                "the hub delivered the config edit, so the drain will ask for a rebuild",
            );

            // The rebuild the drain asks for runs on its own thread; the verdict below is about
            // its swap, so it waits for that rebuild to have run and let its mark go.
            wait_until("the rebuild the delivered edit asked for has run its course", || {
                let _ = state.read(|_, _| ());
                let started = state.rebuilds_started.load(std::sync::atomic::Ordering::SeqCst);
                let reload = lock_recover(&state.inner).reload.clone();
                if started > 0 && reload == super::super::super::types::ReloadState::Idle {
                    Ok(())
                } else {
                    Err((started, reload))
                }
            });
            assert_eq!(
                file_id(&state, &hidden),
                Some(before),
                "a rebuild asked for by an event is still a rebuild over a half-read tree",
            );

            open_dir(&closed);
            // The event that asked for this rebuild was consumed when it was refused, and a
            // healthy hub answers reads from the drain, which never re-compares the config.
            // The watchdog's scan is what asks again — a bounded wait, and the deliberate
            // alternative to re-walking the tree on every drift window while it stays
            // unreadable.
            state.reconcile_tick();
            wait_until("the postponed config change lands once the tree can be read", || {
                let disabled = state.read(|r, _| r.config().is_disabled(DiagnosticCode::Typo));
                if matches!(disabled, ResidentOutcome::Ready(false, _)) {
                    Ok(())
                } else {
                    Err("the resident does not serve the edited config yet")
                }
            });
        }

        /// A declined rebuild has already consumed the events that asked for it: its
        /// cursor was resubscribed at the start, on the assumption that the build's own
        /// scan covered them. Declining makes that assumption false, so an edit delivered
        /// in that window is recorded nowhere — and with a healthy hub every later read
        /// answers from the drain, which has nothing left to hand over. Only a scan can
        /// find it again, and the refusal is what has to ask for one.
        #[test]
        fn a_declined_rebuild_does_not_swallow_the_events_it_consumed() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

            let (state, hub) = state_with_hub(root);
            state.ensure_loading();
            wait_ready(&state);

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            let mut observer = hub.subscribe();
            // One window carrying both: the config edit asks for the rebuild that will be
            // declined, and the body edit is the bystander whose delivery it consumes.
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n");
            write(
                root,
                "CommonModules/Сервер/Ext/Module.bsl",
                "&НаСервере\nФункция Считать() Экспорт Возврат 42; КонецФункции\n",
            );
            // Both needles are counted across one draining loop rather than by two calls
            // to `wait_for_delivery`: that helper advances the cursor past everything it
            // drained, so the first call would consume the delivery the second is waiting
            // for and the setup would fail for a reason that is not the subject.
            let (mut config_seen, mut body_seen) = (false, false);
            wait_until("the hub delivered the config edit and the body edit", || {
                let batch = hub.drain(observer);
                observer = batch.cursor;
                for entry in &batch.entries {
                    let path = entry.raw.to_string_lossy();
                    config_seen |= path.contains("bsl-analyzer.toml");
                    body_seen |= path.contains("Module.bsl");
                }
                if config_seen && body_seen {
                    Ok(())
                } else {
                    Err((config_seen, body_seen))
                }
            });

            // Observed through the generation, not through the file text: a resident whose
            // recorded revision no longer matches disk refuses to hand its text over at all
            // (`file_text revision mismatch`), so the very state under test would panic the
            // probe instead of failing it. Applying a body edit bumps the generation, and
            // nothing else in this window can: the config rebuild is declined, and the
            // hidden directory is unchanged.
            let generation_before = raw_generation(&state);
            wait_until("the body edit the declined rebuild consumed is found again", || {
                let _ = state.read(|_, _| ());
                let generation = raw_generation(&state);
                if generation > generation_before {
                    Ok(())
                } else {
                    Err(generation)
                }
            });
            open_dir(&closed);
        }

        /// A scan that applied nothing did not move the baseline, and must not say it
        /// did: the epoch is what tells a cached snapshot it is comparing against a world
        /// that has since changed. Bumping it over a no-op makes the scan's own cache
        /// incomparable, so every second read throws the cache away and walks the whole
        /// tree again — at the very moment the postponement was meant to stop paying for
        /// walks.
        #[test]
        fn a_postponed_config_edit_does_not_cost_a_walk_per_read() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

            let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
            // Long enough that every walk after the first is the defect, not the throttle.
            state.drift_interval = Duration::from_secs(3600);
            state.ensure_loading();
            wait_ready(&state);

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n");

            let before = state.scan_count();
            for _ in 0..3 {
                let _ = state.read(|_, _| ());
            }
            assert_eq!(
                state.scan_count(),
                before + 1,
                "one walk answers the postponed config edit; the rest are served from its cache",
            );
            open_dir(&closed);
        }

        /// While the configuration the resident was built from is known to be out of
        /// date, the resident must not ADMIT files. A path can be new precisely because
        /// the postponed edit added the root it lies in, and serving a file from a
        /// topology this resident never adopted answers it against the wrong
        /// configuration. Refreshing the files it already has is a different act and
        /// keeps working.
        #[test]
        fn a_pending_config_edit_admits_no_new_files() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

            let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
            state.drift_interval = Duration::from_millis(0);
            state.ensure_loading();
            wait_ready(&state);

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            // An object module: ordinary source, so the structure rule lets it through —
            // the only thing that may hold it back is the pending configuration.
            write_catalog(root, "Товары", 9);
            write(
                root,
                "Catalogs/Товары/Ext/ObjectModule.bsl",
                "Процедура ПередЗаписью(Отказ) КонецПроцедуры",
            );
            edit_moving_mtime(&root.join("bsl-analyzer.toml"), || {
                write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n")
            });

            // Every read scans and admits on the spot; nothing here waits on another thread.
            let object_module = root.join("Catalogs/Товары/Ext/ObjectModule.bsl");
            for _ in 0..50 {
                let _ = state.read(|_, _| ());
            }
            assert_eq!(
                file_id(&state, &object_module),
                None,
                "a file is not admitted to a resident whose configuration is out of date",
            );

            open_dir(&closed);
            wait_until("and it is admitted by the rebuild the healed tree allows", || {
                let _ = state.read(|_, _| ());
                file_id(&state, &object_module).ok_or("not admitted yet")
            });
        }

        /// The same refusal reached through the event path. A postponed configuration is
        /// not a fact either applier can hold privately: the drain never learns that a
        /// rebuild was declined, so a rule computed inside the scan would leave the
        /// healthy-hub path admitting files the scan path had just refused.
        #[test]
        fn a_pending_config_edit_admits_no_new_files_through_the_drain() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

            let (state, hub) = state_with_hub(root);
            state.ensure_loading();
            wait_ready(&state);

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            let mut observer = hub.subscribe();
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n");
            wait_for_delivery(
                &hub,
                &mut observer,
                "bsl-analyzer.toml",
                "the hub delivered the config edit",
            );
            // Let the rebuild be asked for, declined, and its forced scan run.
            let scans = state.scan_count();
            wait_until("the postponed config edit's forced scan runs", || {
                let _ = state.read(|_, _| ());
                if state.scan_count() > scans {
                    Ok(())
                } else {
                    Err("no walk yet")
                }
            });

            // Now a brand-new file arrives by event, with the configuration still pending.
            let object_module = root.join("Catalogs/Товары/Ext/ObjectModule.bsl");
            write(
                root,
                "Catalogs/Товары/Ext/ObjectModule.bsl",
                "Процедура ПередЗаписью(Отказ) КонецПроцедуры",
            );
            wait_for_delivery(
                &hub,
                &mut observer,
                "ObjectModule.bsl",
                "the hub delivered the new file",
            );
            // The delivery is in the hub; every read drains and admits on the spot.
            for _ in 0..20 {
                let _ = state.read(|_, _| ());
            }
            assert_eq!(
                file_id(&state, &object_module),
                None,
                "the drain admits no file while the resident's configuration is out of date",
            );

            open_dir(&closed);
            wait_until("and admits it once the configuration is adopted", || {
                let _ = state.read(|_, _| ());
                state.reconcile_tick();
                file_id(&state, &object_module).ok_or("not admitted yet")
            });
        }

        /// A descriptor is admitted the same way a body is — by appearing — so the same
        /// refusal covers it. The drain sees `.xml` drift as one bucket regardless of
        /// direction, which is why the new ones have to be told apart by the baseline:
        /// a key it never held is an admission, a key it holds is an edit or a removal.
        #[test]
        fn a_pending_config_edit_admits_no_new_descriptors_through_the_drain() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = false\n");

            let (state, hub) = state_with_hub(root);
            state.ensure_loading();
            wait_ready(&state);

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            let mut observer = hub.subscribe();
            write(root, "bsl-analyzer.toml", "[diagnostics.parameters]\nTypo = true\n");
            wait_for_delivery(
                &hub,
                &mut observer,
                "bsl-analyzer.toml",
                "the hub delivered the config edit",
            );
            let scans = state.scan_count();
            wait_until("the postponed config edit's forced scan runs", || {
                let _ = state.read(|_, _| ());
                if state.scan_count() > scans {
                    Ok(())
                } else {
                    Err("no walk yet")
                }
            });

            write_catalog(root, "Товары", 9);
            wait_for_delivery(
                &hub,
                &mut observer,
                "Товары.xml",
                "the hub delivered the new descriptor",
            );
            // The delivery is in the hub; every read drains and admits on the spot.
            for _ in 0..20 {
                let _ = state.read(|_, _| ());
            }

            let module = module_path(root, "Сервер");
            assert!(
                !catalog_resolves(&state, &module),
                "a descriptor is admitted no more freely than a body is",
            );

            open_dir(&closed);
            wait_until("and resolves once the configuration is adopted", || {
                let _ = state.read(|_, _| ());
                state.reconcile_tick();
                if catalog_resolves(&state, &module) {
                    Ok(())
                } else {
                    Err("not resolved yet")
                }
            });
        }

        /// The reconciler charges every hub consumer for a miss, so it must only charge
        /// for drift the scan may speak for. An unreadable subtree is not a miss: the hub
        /// owed no event, because nothing changed.
        #[test]
        fn an_unproven_removal_does_not_degrade_the_hub() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            sample_workspace(root);
            write_common_module(root, "Скрытый", true, "Процедура С()\nКонецПроцедуры");

            let (state, hub) = state_with_hub(root);
            state.ensure_loading();
            wait_ready(&state);

            let closed = root.join("CommonModules/Скрытый");
            if !close_dir(&closed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
            // Whatever the permission change itself delivered is discarded: with those
            // entries in hand the reconciler could call the phantom removals delivered,
            // and the gate would pass without proving anything.
            state.drain_and_discard_cursor();
            assert_eq!(hub.health(), Health::Healthy, "healthy before the reconcile");

            state.reconcile_tick();

            assert_eq!(
                hub.health().label(),
                "healthy",
                "an unreadable subtree is not an undelivered change",
            );
            open_dir(&closed);
        }
    }

    fn resident_has_file(state: &DiagnosticsState, path: &std::path::Path) -> bool {
        match state.read(|r, _| r.file_id_for(path).is_some()) {
            ResidentOutcome::Ready(v, _) => v,
            _ => panic!("the resident must be ready on this stand"),
        }
    }

    /// Poll until the resident carries `path`, or the poll budget runs out. A fixed sleep
    /// cannot replace this: the work being waited for is a full rebuild, whose duration
    /// is the machine's to decide.
    fn resident_gains_file(state: &DiagnosticsState, path: &std::path::Path) -> bool {
        let mut deadline = crate::change_hub::test_support::Patience::new(
            crate::change_hub::test_support::STATE_CEILING,
            std::time::Duration::from_millis(20),
        );
        while deadline.allows() {
            if resident_has_file(state, path) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        resident_has_file(state, path)
    }

    /// A NAME lookup rather than a path one: resolve a metadata object by its name from
    /// the module that lives beside it. `None` while that module is not resident.
    fn catalog_resolves_from(
        state: &DiagnosticsState,
        module: &std::path::Path,
        name: &str,
    ) -> Option<bool> {
        match state.read(|r, _| {
            let fid = r.file_id_for(module)?;
            Some(
                r.db()
                    .resolve_metadata_object_for_file(fid, bsl_metadata::MdoType::Catalog, name)
                    .is_some(),
            )
        }) {
            ResidentOutcome::Ready(v, _) => v,
            _ => panic!("the resident must be ready on this stand"),
        }
    }

    /// The project declares a scoped extension, but this test deliberately arms a healthy
    /// hub without that extension root. A file added beneath the omitted root is not delivered
    /// by events; the forced rescan must find it. The control verifies that files under the
    /// watched configuration root still arrive through the event path.
    #[test]
    fn a_forced_rescan_finds_files_in_a_scoped_root_the_hub_does_not_watch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // The main scan root is a grandchild; the scoped extension is intentionally left
        // out of this test hub's watch set.
        let cf = root.join("src/cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(&cf, "Сервер", true, "&НаСервере\nФункция Ч() Экспорт КонецФункции");
        let ext = root.join("src/cfe/Расш");
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
        fs::write(
            root.join("bsl-analyzer.toml"),
            "[source]\nroot = \"src/cf\"\nextensions = [{ name = \"Расш\", path = \"src/cfe/Расш\", scope = \"InfoBase\" }]\n",
        )
        .unwrap();

        let project = project_model::Project::new(root).expect("valid test project");
        assert!(
            project.source_roots().contains(&ext),
            "the scoped extension belongs to the Project even though this hub omits it",
        );
        let targets = crate::change_hub::watch_targets_for(root, std::slice::from_ref(&cf));
        // The long interval keeps the test hub healthy without changing its explicit targets.
        let hub = crate::change_hub::WorkspaceChangeHub::start_targets_with_period(
            targets,
            Duration::from_secs(3600),
        );
        assert!(hub.wait_until_watching(), "the hub must arm");

        let mut state =
            DiagnosticsState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        wait_ready(&state);
        assert!(
            hub.rearm(
                crate::change_hub::watch_targets_for(root, std::slice::from_ref(&cf)),
                Duration::from_secs(5),
            ),
            "the fixture hub stays healthy while omitting the scoped extension root",
        );
        // Rearming changes coverage and marks this cursor for reconciliation. Consume that
        // setup-only mark before measuring the event and forced-rescan paths below.
        state.drain_and_discard_cursor();
        let scans_before_changes = state.scan_count();

        // Positive control, same stand: a new module INSIDE the recursive scan root must
        // arrive without any force, and through the event path (no scan).
        write_common_module(&cf, "Ковбой", true, "&НаСервере\nФункция Ф() Экспорт КонецФункции");
        let watched = cf.join("CommonModules/Ковбой/Ext/Module.bsl");
        assert!(
            resident_gains_file(&state, &watched),
            "control: a change inside the watch must arrive with no force at all"
        );
        assert_eq!(
            state.scan_count(),
            scans_before_changes,
            "control: it arrived by the event path without an additional scan"
        );

        // New files in the scoped extension root are outside this hub's watch coverage.
        write_common_module(
            &ext,
            "РасшМодуль",
            true,
            "&НаСервере\nФункция Р() Экспорт КонецФункции",
        );
        write_catalog(&ext, "Товары", 9);
        let ext_module = ext.join("CommonModules/РасшМодуль/Ext/Module.bsl");
        let never_written = ext.join("CommonModules/Призрак/Ext/Module.bsl");

        // A settle window: a negative observation needs time in which the event could
        // have arrived and did not. Only the timing profile waits it out; the readings
        // themselves hold at any moment.
        if cfg!(timing_contract) {
            std::thread::sleep(Duration::from_millis(500));
        }

        let cursor = *lock_recover(&state.hub_cursor);
        assert_eq!(hub.health_for(cursor), Health::Healthy, "the hub is healthy and right");
        assert!(!resident_has_file(&state, &ext_module), "the ordinary read must not see it");
        assert_eq!(
            state.scan_count(),
            scans_before_changes,
            "the healthy path still walks nothing before the explicit force"
        );

        state.force_rescan();
        // The forced scan surfaces files from the declared extension root.
        assert!(
            resident_gains_file(&state, &ext_module),
            "the forced re-scan must surface files under the root the hub omitted"
        );
        assert!(
            state.scan_count() > scans_before_changes,
            "only the explicit force may add a scan for the omitted root"
        );
        assert_eq!(
            catalog_resolves_from(&state, &ext_module, "Товары"),
            Some(true),
            "and the NAME resolves once it has"
        );
        assert_eq!(
            catalog_resolves_from(&state, &ext_module, "Призрак"),
            Some(false),
            "control: a name absent from disk resolves neither before nor after"
        );
        assert!(
            !resident_has_file(&state, &never_written),
            "control: a path absent from disk is not found after the force either"
        );
    }

    /// Inside the drift window the scan is served from cache, so a module written there
    /// is invisible however many times the caller reads. The forced re-scan drops that
    /// cache, which is the whole of its second half — and the half the healthy-hub stand
    /// above cannot exercise, because a healthy hub never fills the cache at all.
    #[test]
    fn a_forced_rescan_finds_a_module_written_inside_the_drift_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sample_workspace(root);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_secs(60);
        state.ensure_loading();
        wait_ready(&state);

        // Warm the throttle cache: a successful build leaves it empty, and an empty cache
        // is walked by the very next read — which would make the miss below unobservable.
        let _ = state.read(|_, _| ());
        assert!(lock_recover(&state.scan).is_some(), "the window is armed");

        // Older than the storm-guard floor, so the force is allowed to drop the cache.
        std::thread::sleep(FORCE_RESCAN_FLOOR + Duration::from_millis(50));
        write_common_module(
            root,
            "Опоздавший",
            true,
            "&НаСервере\nФункция О() Экспорт КонецФункции",
        );
        let late = root.join("CommonModules/Опоздавший/Ext/Module.bsl");
        let never_written = root.join("CommonModules/Призрак/Ext/Module.bsl");

        assert!(!resident_has_file(&state, &late), "inside the window the cache answers");

        state.force_rescan();
        assert!(resident_has_file(&state, &late), "the forced re-scan drops the cache and walks");
        assert!(
            !resident_has_file(&state, &never_written),
            "control: the walk finds what is on disk, not whatever was asked for"
        );
    }
}
