//! The one way a tool reads the resident analysis database.
//!
//! Every resident-backed answer is computed on a blocking thread, under the resident
//! mutex, and may take seconds on a large configuration. Three properties have to hold
//! for all of them, and holding them per tool is how they drift apart:
//!
//! - the answer runs on a PER-REQUEST database handle, so its Salsa cancellation token
//!   can be cancelled without touching the resident's master handle or any concurrent
//!   call (a handle clone carries a token of its own);
//! - that handle dies before the read returns: a live clone blocks every write to the
//!   database, and the incremental drift apply is a write;
//! - a cancelled call answers as a cancelled call, a call cut short by a writer answers
//!   as a retry, and a panic answers as a panic — three different things that all arrive
//!   as an unwind.
//!
//! [`resident_call`] owns all three. Tools describe what to compute and how to render
//! each outcome; they do not decide when to observe cancellation, because a decision
//! written in a tool can only be verified in that tool.
//!
//! # How soon a cancelled call lets go
//!
//! The guarantee is stated in artefacts, not in loop steps: **a cancelled call frees the
//! resident within ONE artefact** — a file, a module, a metadata object, a candidate.
//! A walk that grows with the workspace observes the token at every artefact it visits,
//! and so does the selection that feeds it: filtering a whole configuration costs a full
//! pass and, for a category with no members, never reaches the loop that would have
//! noticed. A step that cannot be divided — a sort, a fold that must finish — takes its
//! checkpoint immediately before it, so a cancel that has already arrived does not pay
//! for tens of thousands of comparisons.
//!
//! Work bounded by ONE artefact is deliberately left alone: the attributes of an object,
//! the methods of a module, the findings of a file, the matches within a line. The walk
//! that reached that artefact already checkpointed on it, and its size is bounded by the
//! artefact rather than by the workspace.
//!
//! Finer than that is not merely expensive, it is wrong in places: the drift apply
//! (`apply_resident_changes`, `retry_resident_holes`) writes salsa inputs, and unwinding
//! through it would tear a mutation in half. The WRITE path is not cancellable at all.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use super::lifecycle::DiagnosticsState;
use super::resident::DiagnosticsResident;
use super::types::ResidentOutcome;
use crate::cancel::{join_unless_cancelled, RequestCancel};

/// How a resident call ended, before any tool-specific rendering.
pub(crate) enum CallOutcome<T> {
    /// The call ran to completion; `T` is whatever the tool computed.
    Ready(T),
    /// The client sent `notifications/cancelled` (or the transport went away).
    Cancelled,
    /// A writer moved the database out from under the call. Nobody cancelled
    /// anything: the caller is owed a retry, not a report of its own cancellation.
    Superseded,
    /// The work panicked. Not cancellation, and never reported as one.
    Panicked,
}

/// A resident-reading call in progress. Handed to the tool's body so it can read the
/// resident as many times as its answer needs — all within one blocking task, one
/// cancellation registry, and one unwind boundary.
pub(crate) struct ResidentSession {
    diag: DiagnosticsState,
    cancel: Arc<RequestCancel>,
}

impl ResidentSession {
    /// One read of the resident, on a database handle owned by this request.
    ///
    /// The handle is cloned and its token registered inside the resident lock, and it
    /// is dropped before the lock is released — including when the body unwinds. The
    /// `unwind_if_revision_cancelled` checkpoints the analysis layers already carry
    /// observe this request's cancellation through that token.
    pub(crate) fn read<T>(
        &self,
        f: impl FnOnce(&DiagnosticsResident, &ide::Analysis, u64) -> T,
    ) -> ResidentOutcome<T> {
        // Before `diag.read`, because `read` polls for drift FIRST — and with a forced
        // scan or a degraded change hub that poll stats the whole tree. Registering the
        // salsa token inside the closure protects the queries and nothing before them,
        // so a session cancelled by now would still pay for that walk.
        if self.cancel.is_cancelled() {
            std::panic::resume_unwind(Box::new(salsa::Cancelled::Local));
        }
        self.diag.read(|resident, generation| {
            // The clone is a local: it cannot outlive this closure, and an unwind
            // through it drops it just the same. A clone that escaped would park the
            // next `set_file_text_source` on salsa's `while *clones != 1`.
            let analysis = ide::Analysis::from_database(resident.db().clone());
            self.cancel.register(salsa::Database::cancellation_token(analysis.database()));
            // Attached for the WHOLE body: that is what keeps a cancel landing inside a
            // query alive for the next file-boundary checkpoint (`Analysis::attached`).
            analysis.attached(|analysis| f(resident, analysis, generation))
        })
    }

    /// One read of the resident for work that fans out to database handles of its own.
    ///
    /// Same door as [`read`](Self::read) — same lock, same cancellation registry — but
    /// WITHOUT attaching a handle to the calling thread. Rayon runs part of a fan-out on
    /// the thread that started it, and a worker there queries its own clone: with an
    /// attach in place that clone is a second database on one thread, which salsa
    /// rejects («Cannot change database mid-query»). The fan-out registers each worker
    /// handle itself, so it needs the registry, not the attach.
    pub(crate) fn read_fanout<T>(
        &self,
        f: impl FnOnce(&mut DiagnosticsResident, u64) -> T,
    ) -> ResidentOutcome<T> {
        if self.cancel.is_cancelled() {
            std::panic::resume_unwind(Box::new(salsa::Cancelled::Local));
        }
        self.diag.read_mut(|resident, generation| f(resident, generation))
    }

    /// A read on an EMPTY database this request owns, for the answer a tool still
    /// serves when the resident is not there to answer it — the platform surface is in
    /// every handle, resident or not.
    ///
    /// It goes through the same door for the same reason: a handle built outside it
    /// carries a token nobody cancelled, so the `unwind_if_revision_cancelled`
    /// checkpoints on the way through would read a clear token and the abandoned work
    /// would run to the end of the platform catalogue for a response nobody reads.
    ///
    /// Not to be called from inside [`read`](Self::read): salsa allows one database per
    /// thread inside an attach scope and panics on a second.
    pub(crate) fn read_detached<T>(&self, f: impl FnOnce(&ide::Analysis) -> T) -> T {
        if self.cancel.is_cancelled() {
            std::panic::resume_unwind(Box::new(salsa::Cancelled::Local));
        }
        let analysis = ide::Analysis::new();
        self.cancel.register(salsa::Database::cancellation_token(analysis.database()));
        analysis.attached(f)
    }

    /// Cheap check for loops between salsa queries, where there is nothing to unwind
    /// from and no query boundary to observe the token at.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The request's cancellation registry, for work that fans out to its own database
    /// handles — the rayon sweep clones one per worker, and each registers here so a
    /// single cancel reaches them all.
    pub(crate) fn cancel(&self) -> &Arc<RequestCancel> {
        &self.cancel
    }

    /// Ask for a drift re-scan before the next read (storm-guarded by the state), and
    /// report what that guard owes — see [`DiagnosticsState::force_rescan`].
    pub(crate) fn force_rescan(&self) -> Option<Duration> {
        self.diag.force_rescan()
    }

    /// Read once, and read again behind a forced re-scan when the first answer is a miss
    /// the caller would act on as final.
    ///
    /// A miss is not always the truth: the resident can simply be behind the disk, by two
    /// routes that share this one answer. The throttled drift scan serves a stat universe
    /// up to its window old, so an object written inside that window is not there yet. And
    /// a healthy change hub reports only what the watch reaches, which is not the whole
    /// tree: a scan root deeper than a direct child of the workspace root leaves the
    /// directories above it watched by nobody, and a root appearing there raises no event
    /// at all. Walking the disk once more answers both.
    ///
    /// Exactly one retry, and its outcome is the answer: a second miss is the truth, and a
    /// loop of genuinely absent lookups must not walk the tree per attempt (the storm guard
    /// bounds the rate, this bounds the count). Cancellation is observed BEFORE the forced
    /// scan rather than only inside the retry, because that scan walks the tree — the most
    /// expensive thing to do for a caller that has already gone.
    ///
    /// A force the storm guard DECLINES is not a retry that ran. It is the same blind read
    /// a second time, and its miss reaches the caller wearing the finality of one that
    /// walked. So the floor is waited out — at most the floor itself — and the force asked
    /// for again. The guard keeps its rate (still one walk per floor); what it loses is the
    /// power to answer for the walk it prevented.
    ///
    /// WHICH answers qualify belongs to the tool: only it knows what its own miss looks
    /// like.
    pub(crate) fn read_retrying_a_stale_miss<T>(
        &self,
        read: impl Fn() -> ResidentOutcome<T>,
        is_stale_miss: impl Fn(&T) -> bool,
    ) -> ResidentOutcome<T> {
        let outcome = read();
        let ResidentOutcome::Ready(answer, _) = &outcome else {
            return outcome;
        };
        if !is_stale_miss(answer) || self.is_cancelled() {
            return outcome;
        }
        #[cfg(test)]
        {
            self.diag.note_stale_miss_consultation();
            self.diag.fire_pre_force_probe();
        }
        if let Some(owed) = self.force_rescan() {
            // Blocking is the right shape here: this whole body already runs on a blocking
            // thread (`resident_call`), the wait is bounded by the floor, and nothing of
            // this state is held across it.
            std::thread::sleep(owed);
            if self.is_cancelled() {
                return outcome;
            }
            // If the floor declines this one too, another walk finished during the wait and
            // its verdict is taken. Waiting again would bound nothing: the count of retries
            // is what stops a loop of genuinely absent lookups from walking the tree once
            // per attempt, and one wait already turns "no walk at all" into "a walk no
            // older than the floor".
            self.force_rescan();
        }
        read()
    }

    /// The lifecycle report, for rendering a `loading` envelope.
    pub(crate) fn status_report(&self) -> super::types::StatusReport {
        self.diag.status_report()
    }
}

/// Run one resident-reading call under the rmcp per-request cancellation token.
///
/// The body runs on a blocking thread. When the token fires, this returns
/// [`CallOutcome::Cancelled`] immediately WITHOUT waiting for that thread: it may still
/// be queued behind another call on the resident mutex, and once it runs it unwinds at
/// its first salsa checkpoint and releases the mutex on its own.
pub(crate) async fn resident_call<T, F>(
    diag: DiagnosticsState,
    ct: tokio_util::sync::CancellationToken,
    body: F,
) -> CallOutcome<T>
where
    F: FnOnce(&ResidentSession) -> T + Send + 'static,
    T: Send + 'static,
{
    let cancel = Arc::new(RequestCancel::default());
    let session = ResidentSession { diag, cancel: Arc::clone(&cancel) };

    let join = tokio::task::spawn_blocking(move || {
        match salsa::Cancelled::catch(AssertUnwindSafe(|| body(&session))) {
            Ok(value) => CallOutcome::Ready(value),
            // This request's own token. Everything else that arrives as an unwind is
            // somebody else's event and must not be dressed up as the client's cancel.
            Err(salsa::Cancelled::Local) => CallOutcome::Cancelled,
            Err(salsa::Cancelled::PendingWrite) => CallOutcome::Superseded,
            Err(other) => {
                tracing::error!(?other, "resident call unwound on a salsa variant it cannot name");
                CallOutcome::Panicked
            }
        }
    });

    match join_unless_cancelled(ct, || cancel.cancel_all(), join).await {
        // Per the MCP cancellation spec the client ignores any response after its
        // `notifications/cancelled`, so there is nothing to wait for and nothing to
        // publish; the detached body unwinds and logs on its own.
        None => CallOutcome::Cancelled,
        Some(Ok(outcome)) => outcome,
        Some(Err(error)) => {
            // A real panic, not cancellation. The caller answers with a fixed sentence
            // (the payload is not the client's business), so the payload has to be
            // recorded HERE or it is lost: `JoinError`'s own text carries the panic
            // message and the thread it died on, and without this line debugging a
            // genuine bug in a resident tool is harder than in one that never moved
            // to this path.
            tracing::error!(%error, "resident call panicked");
            CallOutcome::Panicked
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics_state::drift::FORCE_RESCAN_FLOOR;
    use crate::diagnostics_state::lock_recover;
    use crate::diagnostics_state::test_support::{
        wait_ready, wait_until, write, write_common_module,
    };
    use crate::walk_probe::{await_walk_start, entered, install, reset, WALK_GATE};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::{Duration, Instant};

    /// Two answers a fabricated read can give the retry policy. Any two distinct values
    /// would do: the policy judges by the predicate it is handed, not by the type.
    const MISS: u8 = 0;
    const HIT: u8 = 1;

    /// Callers of one popular name. Measured, not assigned: on this stand an
    /// uncancelled walk takes ~3.5 s in a debug build (200 callers took 0.35 s — far too
    /// short for a cancel to land mid-walk, and any gate built on it is green whatever
    /// the code does), while the resident itself builds in ~80 ms.
    const CALLERS: usize = 1000;

    const DECLARED: &str = "Объявление.ПриИзмененииПоля";

    fn stand(root: &std::path::Path) {
        write_common_module(
            root,
            "Объявление",
            true,
            "&НаСервере\nПроцедура ПриИзмененииПоля() Экспорт\nКонецПроцедуры\n",
        );
        for i in 0..CALLERS {
            write_common_module(
                root,
                &format!("Вызов{i:04}"),
                true,
                "&НаСервере\nПроцедура Тело() Экспорт\n    \
                 Объявление.ПриИзмененииПоля();\nКонецПроцедуры\n",
            );
        }
        write(root, "bsl-analyzer.toml", "[source]\nroot = \".\"\n");
    }

    fn ready_state(root: &std::path::Path) -> DiagnosticsState {
        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_millis(0);
        state.ensure_loading();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match state.status() {
                super::super::types::DiagnosticsStatus::Ready { .. } => return state,
                super::super::types::DiagnosticsStatus::Failed(msg) => panic!("resident: {msg}"),
                other => {
                    assert!(Instant::now() < deadline, "resident never became ready: {other:?}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// The whole `references` answer for the popular name — anchor, walk and render.
    /// Reduced to whether it answered at all: these gates time the walk, they do not read it.
    fn walk(session: &ResidentSession) -> ResidentOutcome<bool> {
        match references_read(session, DECLARED)() {
            ResidentOutcome::Ready(answer, freshness) => {
                ResidentOutcome::Ready(answer.is_ok(), freshness)
            }
            ResidentOutcome::Loading => ResidentOutcome::Loading,
            ResidentOutcome::Disabled => ResidentOutcome::Disabled,
            ResidentOutcome::Failed(msg) => ResidentOutcome::Failed(msg),
        }
    }

    /// How long a second resident call waits behind a first one, with and without a
    /// cancel. The cancelled call must stop holding the resident; the uncancelled one is
    /// the positive control that proves the stand is big enough for the difference to
    /// exist at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_call_frees_the_resident_slot() {
        let _serialised = WALK_GATE.lock().await;
        install();
        let dir = tempfile::tempdir().unwrap();
        stand(dir.path());

        let neighbour = |state: DiagnosticsState| async move {
            let started = Instant::now();
            let out = resident_call(state, tokio_util::sync::CancellationToken::new(), |s| {
                s.read(|resident, _, _| resident.file_count())
            })
            .await;
            assert!(matches!(out, CallOutcome::Ready(ResidentOutcome::Ready(_, _))));
            started.elapsed()
        };

        // Each phase gets its OWN resident. Reusing one would let the control warm every
        // salsa memo the subject then reads back in milliseconds — a gate that measures
        // memo warmth passes whether or not the cancel ever reaches the walk.
        let control = ready_state(dir.path());
        reset();
        let ct = tokio_util::sync::CancellationToken::new();
        let first = tokio::spawn(resident_call(control.clone(), ct, walk));
        await_walk_start();
        let waited_behind_a_live_call = neighbour(control.clone()).await;
        first.await.expect("the uncancelled call finishes");

        // Subject: the same sequence on an equally cold resident, cancelled once the
        // walk is under way.
        let subject = ready_state(dir.path());
        reset();
        let ct = tokio_util::sync::CancellationToken::new();
        let cancelled = tokio::spawn(resident_call(subject.clone(), ct.clone(), walk));
        await_walk_start();
        ct.cancel();
        let waited_behind_a_cancelled_call = neighbour(subject.clone()).await;
        assert!(
            matches!(cancelled.await.expect("joined"), CallOutcome::Cancelled),
            "the cancelled call must answer as cancelled"
        );

        assert!(
            waited_behind_a_live_call > Duration::from_secs(1),
            "positive control is inert: the neighbour waited only {waited_behind_a_live_call:?} \
             behind a LIVE call, so this stand cannot show a cancel freeing the slot"
        );
        assert!(
            waited_behind_a_cancelled_call * 3 < waited_behind_a_live_call,
            "a cancelled call still held the resident: {waited_behind_a_cancelled_call:?} \
             behind a cancelled call vs {waited_behind_a_live_call:?} behind a live one"
        );
    }

    /// The walk itself stops. Answering quickly proves nothing on its own — the join is
    /// released without waiting for the blocking task — so this counts the files the walk
    /// actually entered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_walk_stops_before_the_last_file() {
        let _serialised = WALK_GATE.lock().await;
        install();
        let dir = tempfile::tempdir().unwrap();
        stand(dir.path());

        // Control: an uncancelled walk enters every calling file, so the counter is
        // shown to be able to reach the total it is later asserted to fall short of.
        // On its own resident: a warmed one would make the subject's walk finish before
        // the cancel could land.
        let control = ready_state(dir.path());
        reset();
        let out = resident_call(control, tokio_util::sync::CancellationToken::new(), walk).await;
        assert!(matches!(out, CallOutcome::Ready(ResidentOutcome::Ready(true, _))));
        let full = entered();
        assert!(full >= CALLERS, "an uncancelled walk must enter every caller, entered {full}");

        let state = ready_state(dir.path());
        reset();
        let ct = tokio_util::sync::CancellationToken::new();
        let cancelled = tokio::spawn(resident_call(state.clone(), ct.clone(), walk));
        await_walk_start();
        ct.cancel();
        assert!(matches!(cancelled.await.expect("joined"), CallOutcome::Cancelled));

        // The blocking body is detached: take the resident lock to know it has unwound.
        let out = resident_call(state.clone(), tokio_util::sync::CancellationToken::new(), |s| {
            s.read(|resident, _, _| resident.file_count())
        })
        .await;
        assert!(matches!(out, CallOutcome::Ready(ResidentOutcome::Ready(_, _))));

        let seen = entered();
        assert!(
            seen < full,
            "the walk ran to completion despite the cancel: entered {seen} of {full}"
        );
    }

    /// A cancelled call must not leave its database handle alive: salsa blocks every
    /// write while a clone exists, and the incremental drift apply is a write. Nothing
    /// else on this path is: a full rebuild swaps in a NEW database and never writes to
    /// the old one, so it would pass with a leaked clone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_call_leaves_no_handle_blocking_the_incremental_apply() {
        let _serialised = WALK_GATE.lock().await;
        install();
        let dir = tempfile::tempdir().unwrap();
        stand(dir.path());
        let state = ready_state(dir.path());

        reset();
        let ct = tokio_util::sync::CancellationToken::new();
        let cancelled = tokio::spawn(resident_call(state.clone(), ct.clone(), walk));
        await_walk_start();
        ct.cancel();
        assert!(matches!(cancelled.await.expect("joined"), CallOutcome::Cancelled));

        // Edit a body: the next read polls drift and applies it in place — the write
        // that a leaked clone would park forever on `while *clones != 1`.
        std::fs::write(
            dir.path().join("CommonModules/Вызов0000/Ext/Module.bsl"),
            "&НаСервере\nПроцедура Тело() Экспорт\n    Объявление.ПриИзмененииПоля();\n    \
             Объявление.ПриИзмененииПоля();\nКонецПроцедуры\n",
        )
        .unwrap();

        // On a thread with a deadline: a blocked apply must fail this gate, not hang it.
        let (tx, rx) = std::sync::mpsc::channel();
        let applying = state.clone();
        std::thread::spawn(move || {
            let out = applying.read(|resident, _| resident.file_count());
            let _ = tx.send(matches!(out, ResidentOutcome::Ready(_, _)));
        });
        let applied = rx.recv_timeout(Duration::from_secs(30)).expect(
            "the incremental apply must not block: a handle from the cancelled call \
                     is still alive and salsa is waiting for it to drop",
        );
        assert!(applied, "the resident must still serve after the apply");
    }

    /// Cancellation reaches this request and nothing else: the resident and its master
    /// handle are untouched, so the next call answers in full.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancel_does_not_reach_the_next_call() {
        let _serialised = WALK_GATE.lock().await;
        install();
        let dir = tempfile::tempdir().unwrap();
        stand(dir.path());
        let state = ready_state(dir.path());

        reset();
        let ct = tokio_util::sync::CancellationToken::new();
        let cancelled = tokio::spawn(resident_call(state.clone(), ct.clone(), walk));
        await_walk_start();
        ct.cancel();
        assert!(matches!(cancelled.await.expect("joined"), CallOutcome::Cancelled));

        let after =
            resident_call(state.clone(), tokio_util::sync::CancellationToken::new(), walk).await;
        assert!(
            matches!(after, CallOutcome::Ready(ResidentOutcome::Ready(true, _))),
            "a call after a cancelled one must answer in full"
        );
    }

    /// A resident that is not ready yet must not outrun the token. The `loading` envelope
    /// is a body like any other, and a tool that decided to publish it BEFORE entering the
    /// door would answer a cancelled call with content — which is why no tool branches on
    /// the lifecycle before `resident_call` any more.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_loading_resident_does_not_outrun_the_token() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bsl-analyzer.toml", "[source]\nroot = \".\"\n");
        // Deliberately never brought to Ready: this is the first-call state.
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());

        let ct = tokio_util::sync::CancellationToken::new();
        ct.cancel();
        let outcome = resident_call(state, ct, |session| {
            session.read(|resident, _, _| resident.file_count())
        })
        .await;

        // The body is spawned either way and unwinds on its own; what the door owes is
        // that nothing it computed is PUBLISHED. Asserting the body never started would
        // be asserting a race, not a property.
        assert!(
            matches!(outcome, CallOutcome::Cancelled),
            "a cancelled call answered from the lifecycle instead of answering as cancelled"
        );
    }

    /// `DiagnosticsState::read` polls for drift BEFORE it takes the lock, and a forced
    /// scan stats the whole tree. The request's salsa token is registered inside that
    /// lock, so it protects the queries and nothing before them: a session cancelled by
    /// now must refuse at the door, not pay for a walk nobody will read.
    #[test]
    fn a_cancelled_session_does_not_pay_for_a_drift_scan() {
        use std::panic::AssertUnwindSafe;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        write_common_module(
            dir.path(),
            "Модуль",
            true,
            "&НаСервере\nПроцедура П() Экспорт\nКонецПроцедуры\n",
        );
        write(dir.path(), "bsl-analyzer.toml", "[source]\nroot = \".\"\n");
        let state = ready_state(dir.path());

        // The scan is armed exactly as a forced rescan arms it.
        *crate::diagnostics_state::lock_recover(&state.scan) = None;
        state.force_scan.store(true, Ordering::SeqCst);
        let before = state.scan_count();

        let cancel = Arc::new(RequestCancel::default());
        cancel.cancel_all();
        let session = ResidentSession { diag: state.clone(), cancel };
        let caught = salsa::Cancelled::catch(AssertUnwindSafe(|| {
            session.read(|resident, _, _| resident.file_count())
        }));

        assert!(matches!(caught, Err(salsa::Cancelled::Local)), "the read must refuse outright");
        assert_eq!(
            state.scan_count(),
            before,
            "a cancelled session walked the tree: the drift poll ran before the refusal"
        );
    }

    /// A read served without the resident answers to this request's cancel too. The
    /// handle is empty, but the platform surface it walks is not, and a handle built
    /// outside the door carries a token nobody ever cancels.
    ///
    /// The inner `attach` stands in for the salsa queries such a walk makes: each is its
    /// own outermost scope unless the door holds one, and leaving that scope clears the
    /// handle's token — so this gate colours both the registration and the attach.
    #[test]
    fn a_detached_read_answers_to_this_requests_cancel() {
        use std::panic::AssertUnwindSafe;

        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(RequestCancel::default());
        let session = ResidentSession {
            diag: DiagnosticsState::for_workspace(dir.path().to_path_buf()),
            cancel: Arc::clone(&cancel),
        };

        let caught = salsa::Cancelled::catch(AssertUnwindSafe(|| {
            session.read_detached(|analysis| {
                let db = analysis.database();
                // The notification lands while the walk is under way.
                cancel.cancel_all();
                salsa::Database::attach(db, |_| ());
                salsa::Database::unwind_if_revision_cancelled(db);
                "walked the whole platform catalogue"
            })
        }));

        assert!(
            matches!(caught, Err(salsa::Cancelled::Local)),
            "the detached read ran to the end for a cancelled request"
        );
    }

    /// Three unwinds arrive the same way and mean three different things. A live
    /// `PendingWrite` cannot be produced on this path — every write needs the resident
    /// mutex the reader holds — so the gate is put on the classification itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_kind_of_unwind_keeps_its_own_meaning() {
        let raise = |cancelled: salsa::Cancelled| {
            move |_: &ResidentSession| -> bool {
                std::panic::resume_unwind(Box::new(cancelled));
            }
        };
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bsl-analyzer.toml", "[source]\nroot = \".\"\n");
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());
        let ct = || tokio_util::sync::CancellationToken::new();

        assert!(matches!(
            resident_call(state.clone(), ct(), raise(salsa::Cancelled::Local)).await,
            CallOutcome::Cancelled
        ));
        assert!(
            matches!(
                resident_call(state.clone(), ct(), raise(salsa::Cancelled::PendingWrite)).await,
                CallOutcome::Superseded
            ),
            "a writer cutting the call short is not the client's cancellation"
        );
        assert!(
            matches!(
                resident_call(state.clone(), ct(), raise(salsa::Cancelled::PropagatedPanic)).await,
                CallOutcome::Panicked
            ),
            "a panic must never be dressed up as a cancellation"
        );
    }

    /// A freshness verdict for a fabricated outcome. Its values are never read by the
    /// retry policy — it decides on the ANSWER — so any consistent set will do.
    fn fresh() -> super::super::types::Freshness {
        super::super::types::Freshness { revision: 1, stale: false, reload: "idle", topology: 7 }
    }

    fn bare_session(root: &std::path::Path) -> (DiagnosticsState, ResidentSession) {
        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        let session =
            ResidentSession { diag: state.clone(), cancel: Arc::new(RequestCancel::default()) };
        (state, session)
    }

    /// A state whose throttle cache reads as walked JUST NOW, so every force meets the
    /// storm guard's decline. No resident behind it: these gates count reads and forces,
    /// and building a database would only add time to a wait they measure.
    fn a_state_the_storm_guard_is_protecting(root: &std::path::Path) -> DiagnosticsState {
        let state = DiagnosticsState::for_workspace(root.to_path_buf());
        *lock_recover(&state.scan) = Some(crate::diagnostics_state::drift::ScanCache {
            at: Instant::now(),
            stats: Vec::new(),
            config_fp: 0,
            baseline_epoch: 0,
            verdict: crate::graph::universe::ScanVerdict::for_test(0, 0),
        });
        state
    }

    /// A declined force is waited out and asked again — one wait, one extra consultation,
    /// and the retry that was owed. Counted here rather than answered: the end-to-end gate
    /// below reads what the walk found, this one pins the policy's shape.
    #[test]
    fn a_force_the_floor_declines_costs_one_wait_and_one_more_ask() {
        let dir = tempfile::tempdir().unwrap();
        let state = a_state_the_storm_guard_is_protecting(dir.path());
        let session =
            ResidentSession { diag: state.clone(), cancel: Arc::new(RequestCancel::default()) };
        let reads = AtomicUsize::new(0);

        let started = Instant::now();
        let outcome = session.read_retrying_a_stale_miss(
            || {
                reads.fetch_add(1, AtomicOrdering::SeqCst);
                ResidentOutcome::Ready(MISS, fresh())
            },
            |answer| *answer == MISS,
        );

        assert!(matches!(outcome, ResidentOutcome::Ready(..)), "a Ready outcome stays Ready");
        assert_eq!(
            reads.load(AtomicOrdering::SeqCst),
            2,
            "the retry the caller was owed still ran"
        );
        assert_eq!(
            state.forced_rescans(),
            2,
            "one force the floor declined and one asked after the wait — a policy that took \
             the decline for an answer would consult once",
        );
        assert!(
            started.elapsed() >= FORCE_RESCAN_FLOOR,
            "and it waited the floor out rather than asking again straight away, which the \
             guard would decline exactly as it declined the first",
        );
    }

    /// A cancel arriving DURING that wait stops the retry, for the same reason a cancel
    /// before it does: the force it would ask for walks the tree, and the caller has gone.
    /// The positive control is the same stand with no cancel, which does retry — without it
    /// a policy that never retried behind a declined force would pass this gate too.
    #[test]
    fn a_cancel_arriving_during_the_wait_stops_the_retry() {
        let dir = tempfile::tempdir().unwrap();
        let state = a_state_the_storm_guard_is_protecting(dir.path());

        let cancel = Arc::new(RequestCancel::default());
        let session = ResidentSession { diag: state.clone(), cancel: Arc::clone(&cancel) };
        // Fired from outside, because a cancel raised by the read itself lands BEFORE the
        // force and never reaches the window this gate is about — and fired on the EVENT,
        // not on a clock. The hatch counter rises inside the declined force, which is the
        // statement immediately before the sleep, so waiting on it puts the cancel after
        // the force this gate requires to have happened and inside the wait it is about.
        // A timer would have to land between the two, and a stand that hopes to hit a
        // window is measuring the scheduler.
        let armed = state.clone();
        let ticker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while armed.forced_rescans() == 0 && Instant::now() < deadline {
                std::thread::yield_now();
            }
            cancel.cancel_all();
        });
        let reads = AtomicUsize::new(0);
        let _ = session.read_retrying_a_stale_miss(
            || {
                reads.fetch_add(1, AtomicOrdering::SeqCst);
                ResidentOutcome::Ready(MISS, fresh())
            },
            |answer| *answer == MISS,
        );
        ticker.join().expect("the canceller");
        assert_eq!(reads.load(AtomicOrdering::SeqCst), 1, "a caller that left is not read for");
        assert_eq!(
            state.forced_rescans(),
            1,
            "and the walk the wait was for is never asked: only the declined force was",
        );

        let live = ResidentSession {
            diag: a_state_the_storm_guard_is_protecting(dir.path()),
            cancel: Arc::new(RequestCancel::default()),
        };
        let reads = AtomicUsize::new(0);
        let _ = live.read_retrying_a_stale_miss(
            || {
                reads.fetch_add(1, AtomicOrdering::SeqCst);
                ResidentOutcome::Ready(MISS, fresh())
            },
            |answer| *answer == MISS,
        );
        assert_eq!(
            reads.load(AtomicOrdering::SeqCst),
            2,
            "control: an uncancelled call waits the floor out and reads again",
        );
    }

    /// A miss the tool calls stale earns exactly one forced re-scan and exactly one more
    /// read. Two retries would let a loop of genuinely absent lookups walk the tree once
    /// per attempt; none would leave the caller a final-looking miss the disk contradicts.
    #[test]
    fn a_warranted_miss_forces_one_rescan_and_reads_again() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session) = bare_session(dir.path());
        let reads = AtomicUsize::new(0);

        let outcome = session.read_retrying_a_stale_miss(
            || {
                reads.fetch_add(1, AtomicOrdering::SeqCst);
                ResidentOutcome::Ready(MISS, fresh())
            },
            |answer| *answer == MISS,
        );

        assert!(matches!(outcome, ResidentOutcome::Ready(..)), "a Ready outcome stays Ready");
        assert_eq!(reads.load(AtomicOrdering::SeqCst), 2, "one first read and one retry, no more");
        assert_eq!(state.forced_rescans(), 1, "the hatch is consulted exactly once");
    }

    /// The answer is the SECOND read. Both reads here are misses by the predicate but
    /// carry different values, because a node that threw the retry away and returned the
    /// first outcome would satisfy every count the previous test makes.
    #[test]
    fn an_answer_after_a_forced_retry_is_the_second_read() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, session) = bare_session(dir.path());
        let reads = AtomicUsize::new(0);

        let outcome = session.read_retrying_a_stale_miss(
            || {
                let nth = reads.fetch_add(1, AtomicOrdering::SeqCst);
                ResidentOutcome::Ready(nth, fresh())
            },
            |_| true,
        );

        match outcome {
            ResidentOutcome::Ready(nth, _) => {
                assert_eq!(nth, 1, "the retry's outcome is the answer, not the first read's")
            }
            _ => panic!("a Ready outcome stays Ready"),
        }
    }

    /// An answer the tool does not call a miss is returned as it is: the hatch is not
    /// consulted, and the tree is not walked for an answer nobody doubted.
    #[test]
    fn an_answer_the_predicate_declines_is_returned_without_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session) = bare_session(dir.path());
        let reads = AtomicUsize::new(0);

        let outcome = session.read_retrying_a_stale_miss(
            || {
                reads.fetch_add(1, AtomicOrdering::SeqCst);
                ResidentOutcome::Ready(HIT, fresh())
            },
            |answer| *answer == MISS,
        );

        assert!(matches!(outcome, ResidentOutcome::Ready(..)));
        assert_eq!(reads.load(AtomicOrdering::SeqCst), 1, "no retry for an answer that resolved");
        assert_eq!(state.forced_rescans(), 0, "and no consultation of the hatch");
    }

    /// An outcome that is not `Ready` carries no answer to judge: a resident that is
    /// loading, disabled or failed is not a resident that fell behind the disk, and a
    /// re-scan cannot turn any of the three into a hit.
    #[test]
    fn an_outcome_that_is_not_ready_is_returned_without_a_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let (state, session) = bare_session(dir.path());

        for (name, make) in [
            ("loading", (|| ResidentOutcome::<u8>::Loading) as fn() -> ResidentOutcome<u8>),
            ("disabled", || ResidentOutcome::<u8>::Disabled),
            ("failed", || ResidentOutcome::<u8>::Failed("build refused".to_owned())),
        ] {
            let before = state.forced_rescans();
            let reads = AtomicUsize::new(0);

            let outcome = session.read_retrying_a_stale_miss(
                || {
                    reads.fetch_add(1, AtomicOrdering::SeqCst);
                    make()
                },
                |_| true,
            );

            assert!(!matches!(outcome, ResidentOutcome::Ready(..)), "{name} stays what it is");
            assert_eq!(reads.load(AtomicOrdering::SeqCst), 1, "{name} is not read twice");
            assert_eq!(state.forced_rescans(), before, "{name} consults no hatch");
        }
    }

    /// A cancel that arrives during the first read stops the forced re-scan too: that scan
    /// walks the tree, and walking it for a caller who has gone is the most expensive thing
    /// this path can do for nobody. The positive control is the same stand without the
    /// cancel, so a policy that never retried at all could not pass both halves.
    #[test]
    fn a_cancel_arriving_before_the_retry_cancels_the_rescan_too() {
        let dir = tempfile::tempdir().unwrap();
        let state = DiagnosticsState::for_workspace(dir.path().to_path_buf());

        let cancel = Arc::new(RequestCancel::default());
        let cancelled = ResidentSession { diag: state.clone(), cancel: Arc::clone(&cancel) };
        let reads = AtomicUsize::new(0);
        let _ = cancelled.read_retrying_a_stale_miss(
            || {
                reads.fetch_add(1, AtomicOrdering::SeqCst);
                cancel.cancel_all();
                ResidentOutcome::Ready(MISS, fresh())
            },
            |answer| *answer == MISS,
        );
        assert_eq!(reads.load(AtomicOrdering::SeqCst), 1, "a cancelled call is not read twice");
        assert_eq!(state.forced_rescans(), 0, "and asks for no walk");

        let live =
            ResidentSession { diag: state.clone(), cancel: Arc::new(RequestCancel::default()) };
        let reads = AtomicUsize::new(0);
        let _ = live.read_retrying_a_stale_miss(
            || {
                reads.fetch_add(1, AtomicOrdering::SeqCst);
                ResidentOutcome::Ready(MISS, fresh())
            },
            |answer| *answer == MISS,
        );
        assert_eq!(reads.load(AtomicOrdering::SeqCst), 2, "control: an uncancelled call retries");
        assert_eq!(state.forced_rescans(), 1, "control: and consults the hatch once");
    }

    // --- the whole cycle, on a real resident and through a real tool ------------------
    //
    // The gates above take the policy apart: that a miss consults the hatch, that the
    // retry's outcome is the answer, that each half of the hatch moves the resident. Every
    // one of them watches a link and reads a counter or the resident's own tables; none
    // watches an ANSWER. So the thing a caller is actually owed — a name that is on disk
    // stops coming back as a final-looking miss — was held together by argument across
    // four gates rather than by an input.
    //
    // These two hold it by input, once per tool with a miss shape of its own: a single
    // answer that is `resolved` where a plain read of the same name at the same moment is
    // `not_found`, with the tool's predicate, the policy, the forced walk and the drift
    // apply all inside the measurement.

    /// The body both modules of the stand carry. One method name for both is deliberate:
    /// the subject asks for `Опоздавший.Считать` while `Сервер.Считать` is already
    /// resident, so a resolver that stopped honouring the module part of a qualified name
    /// would fail the control instead of quietly passing it.
    const MODULE: &str = "&НаСервере\nПроцедура Считать() Экспорт\nКонецПроцедуры\n";

    /// The method that arrives after the resident was built.
    const LATE: &str = "Опоздавший.Считать";

    /// A name nothing ever declared: whatever the retry walks, it cannot make this one
    /// resolve.
    const NEVER_DECLARED: &str = "Призрак.Считать";

    type ReferencesAnswer = Result<crate::tools::references::Answer, rmcp::ErrorData>;
    type SymbolInfoAnswer = Result<Option<ide::SymbolInfoCard>, rmcp::ErrorData>;

    /// A resident that is behind the disk with no way to catch up but a forced re-scan.
    ///
    /// No change hub at all, so there is no event path to deliver the module and no watcher
    /// timing in the measurement; and a drift window long enough to outlive the test, so
    /// the throttled scan keeps answering from the universe it walked before the module
    /// existed. Handed back with that cache warm and already older than the storm floor —
    /// a force younger than the floor is the one the guard declines, and a stand built on
    /// a declined force would measure the guard instead of the retry.
    ///
    /// The late artefact is a whole new module rather than a method appended to a resident
    /// one, because file text is read from disk lazily and checked against the revision the
    /// resident recorded: an unapplied edit to a file it already holds is not a stale answer
    /// but a hard refusal. "Behind the disk" is only representable as a file it has never
    /// seen.
    fn a_resident_behind_the_disk() -> (tempfile::TempDir, DiagnosticsState) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_common_module(root, "Сервер", true, MODULE);

        let mut state = DiagnosticsState::for_workspace(root.to_path_buf());
        state.drift_interval = Duration::from_secs(600);
        state.ensure_loading();
        wait_ready(&state);

        // A successful build leaves the throttle cache empty, and an empty cache is walked
        // by the very next read — the read this stand needs to be blind. Armed until it
        // STAYS armed: the build publishes `Ready` under the resident lock and empties the
        // cache just after it, so a window armed the instant `Ready` appears can be wiped
        // from under the stand by the thread that published it.
        wait_until("the drift window stays armed", || {
            let _ = state.read(|_, _| ());
            std::thread::sleep(Duration::from_millis(20));
            match lock_recover(&state.scan).is_some() {
                true => Ok(()),
                false => Err("the build emptied the cache after it was armed"),
            }
        });

        std::thread::sleep(FORCE_RESCAN_FLOOR + Duration::from_millis(50));
        write_common_module(root, "Опоздавший", true, MODULE);
        (dir, state)
    }

    /// Stamp the throttle cache as walked JUST NOW — the single state in which the storm
    /// guard declines a force, and the one every gate above steps over on purpose.
    ///
    /// Set rather than waited for: a stand that hoped to land inside the floor would be
    /// measuring the scheduler, and the input it needs would be the first thing a loaded
    /// machine takes away.
    fn a_scan_the_storm_guard_still_protects(state: &DiagnosticsState) {
        lock_recover(&state.scan)
            .as_mut()
            .expect("the stand hands back a warm throttle cache")
            .at = Instant::now();
    }

    fn session_over(state: &DiagnosticsState) -> ResidentSession {
        ResidentSession { diag: state.clone(), cancel: Arc::new(RequestCancel::default()) }
    }

    /// The whole `references` answer for one name, as the handler computes it.
    fn references_read<'a>(
        session: &'a ResidentSession,
        symbol: &'a str,
    ) -> impl Fn() -> ResidentOutcome<ReferencesAnswer> + 'a {
        move || {
            session.read(|resident, analysis, _| {
                let params = crate::tools::references::Params {
                    symbol: Some(symbol),
                    anchor_root_id: None,
                    root_id: None,
                    path: None,
                    line: None,
                    column: None,
                    line_content: None,
                    area_root_id: None,
                    area_path_prefix: None,
                    kinds: &[],
                    include_declaration: Some(true),
                    limit: None,
                    max_files: None,
                    include_preview: None,
                };
                // No external sources: a graph source answers from a store of its own — it
                // would hide what the resident does or does not know, and add work of its
                // own to an interval the cancellation gates time.
                crate::tools::references::answer(resident, analysis.database(), &params, 6000, &[])
            })
        }
    }

    /// The tool's own verdict on its answer, exactly as the handler asks it.
    fn references_missed(answer: &ReferencesAnswer) -> bool {
        matches!(answer, Ok(answer) if crate::tools::references::warrants_rescan(answer))
    }

    /// The wire `outcome` of a `references` answer — the field a caller reads as final.
    fn references_outcome(served: ResidentOutcome<ReferencesAnswer>) -> String {
        let ResidentOutcome::Ready(answer, freshness) = served else {
            panic!("the resident must be ready on this stand");
        };
        let body = crate::tools::references::finish(
            answer.expect("references answered"),
            freshness.revision,
            freshness.topology,
            freshness.stale,
        )
        .structured_content
        .expect("the answer is structured content");
        body["outcome"].as_str().expect("an answer names its outcome").to_owned()
    }

    /// One `references` call answers `resolved` for a declaration the resident cannot see,
    /// and only because its first answer was a miss that earned a forced re-scan.
    ///
    /// The controls are what make that readable. A plain read of the same name at the same
    /// moment answers `not_found`, so the subject's answer is the retry's and not the
    /// stand's. And a name nothing ever declared stays `not_found` through the same policy,
    /// so the retry is shown to walk the disk rather than to soften a miss.
    ///
    /// What this colours is the cache-dropping half of the force. The other half — routing
    /// a HEALTHY hub's read onto the scan — cannot be coloured here, because a stand with
    /// no hub is on the scan path already; it has a gate of its own beside `force_rescan`.
    #[test]
    fn references_resolves_a_late_declaration_only_behind_the_forced_retry() {
        // Walking references moves the process-global span counter the cancellation
        // gates measure with; taking their gate keeps this test out of their numbers.
        let _serialised = WALK_GATE.blocking_lock();
        let (_dir, state) = a_resident_behind_the_disk();
        let session = session_over(&state);

        assert_eq!(
            references_outcome(references_read(&session, LATE)()),
            "not_found",
            "control: a read with no retry behind it must not see the new declaration, or \
             this stand cannot tell the retry from the stand"
        );

        assert_eq!(
            references_outcome(
                session
                    .read_retrying_a_stale_miss(references_read(&session, LATE), references_missed)
            ),
            "resolved",
            "a declaration that exists on disk came back as a final-looking miss: the \
             forced re-scan, or the read behind it, did not happen"
        );

        // The subject's scan re-stamped the cache, so this control's own force would be the
        // one the storm guard declines — and a control that never walked would say nothing
        // about what a walk finds.
        std::thread::sleep(FORCE_RESCAN_FLOOR + Duration::from_millis(50));
        let walks = state.scan_count();
        assert_eq!(
            references_outcome(session.read_retrying_a_stale_miss(
                references_read(&session, NEVER_DECLARED),
                references_missed,
            )),
            "not_found",
            "control: the retry walks the disk, and the disk does not declare this name"
        );
        assert_eq!(
            state.scan_count(),
            walks + 1,
            "control: and it really walked — a miss the storm guard silently declined to \
             re-scan says nothing about what a walk finds"
        );
    }

    /// The card for one name, as the handler resolves it.
    fn symbol_info_read<'a>(
        session: &'a ResidentSession,
        symbol: &'a str,
    ) -> impl Fn() -> ResidentOutcome<SymbolInfoAnswer> + 'a {
        move || {
            session.read(|resident, analysis, _| {
                crate::tools::symbol_info::resolve_card(
                    resident,
                    analysis.database(),
                    Some(symbol),
                    None,
                    None,
                    None,
                    None,
                    crate::tools::symbol_info::sections_from(&[]),
                    crate::tools::symbol_info::locale_from(None).expect("the default locale"),
                )
            })
        }
    }

    /// The tool's own verdict on its answer, exactly as the handler asks it.
    fn symbol_info_missed(symbol: &str) -> impl Fn(&SymbolInfoAnswer) -> bool + '_ {
        move |answer| {
            crate::tools::symbol_info::warrants_rescan(
                Some(symbol),
                answer.as_ref().map(Option::as_ref),
            )
        }
    }

    /// The card a `symbol_info` answer carries, or `None` where the resident resolved
    /// nothing — the miss a caller reads as final.
    fn symbol_info_card(served: ResidentOutcome<SymbolInfoAnswer>) -> Option<ide::SymbolInfoCard> {
        let ResidentOutcome::Ready(answer, _) = served else {
            panic!("the resident must be ready on this stand");
        };
        answer.expect("symbol_info answered")
    }

    /// The same cycle for `symbol_info`, whose miss is an absent card rather than an
    /// outcome word, with the same two controls.
    #[test]
    fn symbol_info_resolves_a_late_declaration_only_behind_the_forced_retry() {
        let (_dir, state) = a_resident_behind_the_disk();
        let session = session_over(&state);

        assert!(
            symbol_info_card(symbol_info_read(&session, LATE)()).is_none(),
            "control: a read with no retry behind it must not see the new declaration, or \
             this stand cannot tell the retry from the stand"
        );

        assert!(
            symbol_info_card(session.read_retrying_a_stale_miss(
                symbol_info_read(&session, LATE),
                symbol_info_missed(LATE),
            ))
            .is_some(),
            "a declaration that exists on disk came back as a final-looking miss: the \
             forced re-scan, or the read behind it, did not happen"
        );

        std::thread::sleep(FORCE_RESCAN_FLOOR + Duration::from_millis(50));
        let walks = state.scan_count();
        assert!(
            symbol_info_card(session.read_retrying_a_stale_miss(
                symbol_info_read(&session, NEVER_DECLARED),
                symbol_info_missed(NEVER_DECLARED),
            ))
            .is_none(),
            "control: the retry walks the disk, and the disk does not declare this name"
        );
        assert_eq!(
            state.scan_count(),
            walks + 1,
            "control: and it really walked — a miss the storm guard silently declined to \
             re-scan says nothing about what a walk finds"
        );
    }

    /// A force the storm guard DECLINES must not become the answer.
    ///
    /// The guard bounds how often the tree is walked, and a force arriving while the last
    /// scan is younger than [`FORCE_RESCAN_FLOOR`] arms nothing at all. That is right for
    /// the walk and wrong for the caller: the retry then re-reads the very resident that
    /// just missed, and the miss it repeats reads as final for a declaration lying on disk.
    /// Nothing else in the call can contradict it — a healthy hub's event stream never
    /// re-observes a file the caller just wrote, and the throttled scan keeps serving the
    /// universe it walked last — so the policy has to wait the floor out and ask again.
    ///
    /// The input is the state the sibling gates sleep past on purpose: a scan stamped
    /// NEWER than the floor. In production it is not exotic — the idle sweeper's
    /// `reconcile_tick` re-stamps that cache on its own schedule, so any call landing
    /// within a floor of a tick meets exactly this.
    ///
    /// The controls: the hatch is consulted TWICE, so the answer came from a second force
    /// and not from a first one that quietly worked; and a name nothing declares stays a
    /// miss through the same wait, so waiting is shown to walk the disk rather than to
    /// soften a verdict.
    #[test]
    fn a_force_the_storm_guard_declines_is_waited_out_and_asked_again() {
        // Walking references moves the process-global span counter the cancellation
        // gates measure with; taking their gate keeps this test out of their numbers.
        let _serialised = WALK_GATE.blocking_lock();
        let (_dir, state) = a_resident_behind_the_disk();
        let session = session_over(&state);

        // Stated where it is READ, not before the call. The guard answers on the age of the
        // scan cache at the instant it is consulted, and between any earlier setup and that
        // instant stands a full references read over the resident: measured at 1.6 s on a
        // loaded machine against a 250 ms floor, so a stamp taken earlier is six times
        // expired by the time it decides anything, and the stand quietly exercises the
        // branch it was built to avoid. The seam fires immediately before the consultation,
        // which is the only place the input cannot decay out from under it.
        {
            let state = state.clone();
            session.diag.set_pre_force_probe(move || a_scan_the_storm_guard_still_protects(&state));
        }
        let walks = state.scan_count();
        let forces = state.forced_rescans();

        assert_eq!(
            references_outcome(
                session
                    .read_retrying_a_stale_miss(references_read(&session, LATE), references_missed)
            ),
            "resolved",
            "a declaration that exists on disk came back as a final-looking miss because the \
             storm guard swallowed the only force the call had"
        );
        assert_eq!(
            state.scan_count(),
            walks + 1,
            "the wait has to end in a walk — a policy that merely slept would answer the \
             same blind resident it started with"
        );
        assert_eq!(
            state.forced_rescans(),
            forces + 2,
            "one force the guard declined and one it took: an answer behind a single \
             consultation did not come from this policy"
        );

        // The subject's own walk re-stamped the cache, so this control meets the declined
        // force too — and it must still come back a miss.
        a_scan_the_storm_guard_still_protects(&state);
        let walks = state.scan_count();
        assert_eq!(
            references_outcome(session.read_retrying_a_stale_miss(
                references_read(&session, NEVER_DECLARED),
                references_missed,
            )),
            "not_found",
            "control: waiting the floor out walks the disk, and the disk does not declare \
             this name"
        );
        assert_eq!(
            state.scan_count(),
            walks + 1,
            "control: and it really walked — a miss nothing re-scanned says nothing about \
             what a walk finds"
        );
    }
}
