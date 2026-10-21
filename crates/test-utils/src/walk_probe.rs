//! How far a reference walk got, read from the span `ide` already emits.
//!
//! `ide::references` opens a `find_references_in_file` span per file, so a cancellation
//! gate can learn that a walk has started — and how many files it entered — without a
//! `sleep` and without a counter in production code. The price is that the subscriber
//! and the counter are process-global: every test in a binary that walks references
//! adds to the same number, and a held first span is whichever walk gets there first.
//! A test whose walk runs under a span that names something only it uses — the URI of
//! its own anchor, say — claims the count and the hold with [`attribute_to`], and walks
//! of other tests then pass through untouched. A test that cannot name its walk must
//! serialise against every other test in its binary that can move the counter — a gate
//! mutex owned by that test crate — else [`await_walk_start`] returns on somebody
//! else's file and the gate measures an interleaving instead of a cancellation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const WALK_SPAN: &str = "find_references_in_file";

/// Files the walk has entered since the last [`reset`].
static WALKED: AtomicUsize = AtomicUsize::new(0);

/// The marker of the active [`attribute_to`], if any.
static ATTRIBUTED_TO: Mutex<Option<&'static str>> = Mutex::new(None);

/// Extension on a span whose fields mention the active marker; walk spans beneath it are
/// the attributed walk's.
struct Attributed {
    marker: &'static str,
}

/// Whether any field of a span mentions `marker`.
struct Mentions {
    marker: &'static str,
    found: bool,
}

impl tracing::field::Visit for Mentions {
    fn record_str(&mut self, _: &tracing::field::Field, value: &str) {
        self.found |= value.contains(self.marker);
    }

    fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.found {
            self.found = format!("{value:?}").contains(self.marker);
        }
    }
}

struct FirstSpanGate {
    state: Mutex<FirstSpanState>,
    changed: Condvar,
}

#[derive(Default)]
struct FirstSpanState {
    claimed: bool,
    entered: bool,
    released: bool,
}

static FIRST_SPAN_GATE: Mutex<Option<Arc<FirstSpanGate>>> = Mutex::new(None);

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct WalkCounter;

impl<S> tracing_subscriber::Layer<S> for WalkCounter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let active = lock_recover(&ATTRIBUTED_TO);
        let marker = *active;
        if attrs.metadata().name() != WALK_SPAN {
            drop(active);
            if let Some(marker) = marker {
                let mut mentions = Mentions { marker, found: false };
                attrs.record(&mut mentions);
                if let Some(span) = mentions.found.then(|| ctx.span(id)).flatten() {
                    span.extensions_mut().insert(Attributed { marker });
                }
            }
            return;
        }
        let ours = marker.is_none()
            || ctx.span_scope(id).is_some_and(|mut scope| {
                scope.any(|span| {
                    span.extensions()
                        .get::<Attributed>()
                        .is_some_and(|claimed| Some(claimed.marker) == marker)
                })
            });
        if ours {
            // A callback that saw no attribution must finish claiming before a new
            // attribution can reset the count and arm its own first-span gate.
            WALKED.fetch_add(1, Ordering::SeqCst);
            let gate = lock_recover(&FIRST_SPAN_GATE).clone();
            drop(active);
            if let Some(gate) = gate {
                let mut state = lock_recover(&gate.state);
                if !state.claimed {
                    state.claimed = true;
                    state.entered = true;
                    gate.changed.notify_all();
                    let (state, _) = gate
                        .changed
                        .wait_timeout_while(
                            state,
                            crate::wait::hang_guard(Duration::from_secs(60)),
                            |state| !state.released,
                        )
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    assert!(state.released, "hang guard: the held walk was never released");
                }
            }
        }
    }
}

/// Install the counting subscriber once for the whole test binary.
///
/// A global subscriber installed earlier by something else wins silently, and the
/// counter then never moves: a positive control that expects the full walk count is
/// what exposes that, so every gate on this counter must keep one.
pub fn install() {
    use tracing_subscriber::prelude::*;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::registry()
            .with(WalkCounter)
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .try_init();
    });
}

pub fn reset() {
    WALKED.store(0, Ordering::SeqCst);
}

/// Count and hold only the walks that run under a span whose fields mention `marker`, until
/// the returned guard drops. Arm it before the walk starts: the span that carries the marker
/// is recognised when it opens.
pub fn attribute_to(marker: &'static str) -> Attribution {
    let mut active = lock_recover(&ATTRIBUTED_TO);
    assert!(active.is_none(), "a walk attribution is already active");
    *active = Some(marker);
    Attribution { _private: () }
}

/// RAII handle of [`attribute_to`]; on drop every walk counts again.
pub struct Attribution {
    _private: (),
}

impl Drop for Attribution {
    fn drop(&mut self) {
        *lock_recover(&ATTRIBUTED_TO) = None;
    }
}

pub fn entered() -> usize {
    WALKED.load(Ordering::SeqCst)
}

/// Hold the first subsequent reference-walk span until the returned guard is released.
///
/// The guard releases the span and removes the process-global gate on drop, including when
/// a test assertion unwinds.
pub fn hold_first_span() -> (FirstSpanGuard, FirstSpanWaiter) {
    let gate = Arc::new(FirstSpanGate {
        state: Mutex::new(FirstSpanState::default()),
        changed: Condvar::new(),
    });
    let mut active = lock_recover(&FIRST_SPAN_GATE);
    assert!(active.is_none(), "a first-span gate is already active");
    *active = Some(Arc::clone(&gate));
    (FirstSpanGuard { gate: Arc::clone(&gate) }, FirstSpanWaiter { gate })
}

/// RAII handle that releases a held reference-walk span on drop.
pub struct FirstSpanGuard {
    gate: Arc<FirstSpanGate>,
}

impl FirstSpanGuard {
    pub fn release(&self) {
        let mut state = lock_recover(&self.gate.state);
        state.released = true;
        self.gate.changed.notify_all();
    }
}

impl Drop for FirstSpanGuard {
    fn drop(&mut self) {
        self.release();
        let mut active = lock_recover(&FIRST_SPAN_GATE);
        if active.as_ref().is_some_and(|gate| Arc::ptr_eq(gate, &self.gate)) {
            *active = None;
        }
    }
}

/// Bounded observer for the first span held by [`hold_first_span`].
#[derive(Clone)]
pub struct FirstSpanWaiter {
    gate: Arc<FirstSpanGate>,
}

impl FirstSpanWaiter {
    /// Wait for the tracing callback to reach the held span, bounded by `timeout`.
    pub fn wait_entered(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock_recover(&self.gate.state);
        while !state.entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, result) = self
                .gate
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if result.timed_out() && !state.entered {
                return false;
            }
        }
        true
    }
}

/// A walk that will be held at its first file until a cancel has landed. Seeing the walk
/// start is not enough for that: a walk left running can reach its end before the cancel
/// does, and the gate then judges a finished walk.
pub struct HeldWalk {
    hold: FirstSpanGuard,
    entered: FirstSpanWaiter,
}

/// Arm the hold BEFORE starting the walk, so its first file is the one held.
pub fn hold_walk_start() -> HeldWalk {
    let (hold, entered) = hold_first_span();
    HeldWalk { hold, entered }
}

impl HeldWalk {
    /// Wait for the walk to reach its held first file, run `cancel`, then let the walk go on,
    /// so the walk can only meet the cancel with files still ahead of it.
    pub fn cancel_once_entered(self, cancel: impl FnOnce()) {
        assert!(
            self.entered.wait_entered(crate::wait::hang_guard(Duration::from_secs(60))),
            "the walk never started"
        );
        cancel();
        drop(self.hold);
    }
}

/// Block until the walk has entered its first file. It is not held there: a cancel that must
/// land mid-flight goes through [`hold_walk_start`]. A fixed sleep would race the build of
/// whatever the walk reads first. Only the hang guard limits an ordinary wait.
pub fn await_walk_start() {
    let started =
        crate::wait::poll_until(Duration::from_secs(60), Duration::from_millis(1), || {
            entered() > 0
        });
    assert!(started, "the walk never started");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk_under(uri: &str) {
        let _request = tracing::info_span!("handle_find_references", uri = %uri).entered();
        let _file = tracing::debug_span!("find_references_in_file").entered();
    }

    /// The only test of this binary that walks, so the process-global probe is its own.
    /// A foreign walk runs on this very thread while the hold is armed: claimed by mistake,
    /// it would never return.
    #[test]
    fn an_attributed_hold_lets_a_foreign_walk_pass_and_holds_its_own() {
        install();
        reset();
        let selecting_gate = lock_recover(&FIRST_SPAN_GATE);
        let entering = std::thread::spawn(|| walk_under("file:///unattributed/Module.bsl"));
        assert!(
            crate::wait::poll_until(Duration::from_secs(60), Duration::from_millis(1), || {
                entered() > 0
            }),
            "hang guard: the walk never started"
        );
        let attribution_is_locked =
            matches!(ATTRIBUTED_TO.try_lock(), Err(std::sync::TryLockError::WouldBlock));
        drop(selecting_gate);
        entering.join().expect("the counted walk finishes selecting its gate");
        assert!(
            attribution_is_locked,
            "a counted walk released attribution before selecting its gate"
        );

        let _attributed = attribute_to("ours/Module.bsl");
        reset();
        let held = hold_walk_start();

        walk_under("file:///theirs/Module.bsl");
        assert_eq!(entered(), 0, "a foreign walk was counted");

        let ours = std::thread::spawn(|| walk_under("file:///ours/Module.bsl"));
        let mut cancelled_while_held = false;
        held.cancel_once_entered(|| cancelled_while_held = !ours.is_finished());
        ours.join().expect("the attributed walk goes on once released");
        assert!(cancelled_while_held, "the attributed walk was not held at its first file");
        assert_eq!(entered(), 1, "the attributed walk was not counted");

        let old_parent = tracing::info_span!("request", uri = "file:///ours/Module.bsl");
        drop(_attributed);
        let _next = attribute_to("next/Module.bsl");
        reset();
        {
            let _old = old_parent.enter();
            walk_under("file:///theirs/Module.bsl");
        }
        assert_eq!(entered(), 0, "an old anchor leaked into a different attribution");
        walk_under("file:///next/Module.bsl");
        assert_eq!(entered(), 1, "the new anchor was not counted");
    }
}
