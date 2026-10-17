//! What more than one end-to-end binary needs and none of them owns.

use std::future::Future;
use std::time::Duration;

use mcp_server::McpServer;
use serde_json::Value;

/// How many times an answer that depends on a write reaching the resident is asked for under
/// `timing_contract`. The bound counts asks, not seconds: a starved watcher thread takes longer
/// to deliver, and a loaded machine stretches every ask, but neither can turn a delivered answer
/// into a failure. Only an answer that never comes exhausts it, which is a broken mechanism, not
/// a slow one. An ordinary run asks until the answer comes.
const DELIVERY_ATTEMPTS: u32 = 600;

/// The ceiling every hand-written state wait is counted against: one minute, read as polls,
/// under `timing_contract`; none otherwise (see [`test_utils::wait`]).
///
/// A poll budget, not a clock: [`Patience::allows`] spends one poll per call, so a wait that
/// loops on it gives up after the same number of polls however fast the machine is.
#[allow(dead_code)]
pub const STATE_CEILING: Duration = test_utils::wait::ceiling(Duration::from_secs(60));

#[allow(dead_code)]
pub struct Patience {
    left: u64,
}

#[allow(dead_code)]
impl Patience {
    pub fn new(ceiling: Duration, step: Duration) -> Self {
        let polls = ceiling.as_nanos() / step.as_nanos().max(1);
        Self { left: u64::try_from(polls).unwrap_or(u64::MAX) }
    }

    /// Spends one poll; `false` once the allowance is gone.
    pub fn allows(&mut self) -> bool {
        if self.left == 0 {
            return false;
        }
        self.left -= 1;
        true
    }
}

/// How often the question is asked again. Cheap next to the walk an answer may cost, and
/// far above the storm floor a forced re-scan is bounded by.
const ASK_AGAIN_EVERY: Duration = Duration::from_millis(200);

/// A server whose background owners are stopped, and have left, before the stand it reads is
/// removed. Declared after the stand in a test, it is dropped first, so the owners are gone
/// by the time the stand is: a stand deleted under a backlog still draining is read as missing
/// files.
///
/// Not every binary that declares this module uses it, hence the allowance.
#[allow(dead_code)]
pub struct Retired(pub McpServer);

impl std::ops::Deref for Retired {
    type Target = McpServer;

    fn deref(&self) -> &McpServer {
        &self.0
    }
}

impl Drop for Retired {
    fn drop(&mut self) {
        // A test that is already failing reports only once it ends, so an owner that never
        // stops must not keep it from ending; the bound decides nothing the test asserts.
        let budget = if std::thread::panicking() { UNWIND_RETIRE_BUDGET } else { RETIRE_BUDGET };
        self.0.shutdown_within(budget);
    }
}

/// Bound on an owner leaving while a failed test unwinds, in every profile. Not every binary
/// that declares this module uses it, hence the allowance.
#[allow(dead_code)]
const UNWIND_RETIRE_BUDGET: Duration = Duration::from_secs(180);

/// Bound on an owner leaving under `timing_contract`; none otherwise. An owner always leaves at
/// its next wake, after at most the batch it has in hand, so this is reached only by an owner
/// that never stops.
#[allow(dead_code)]
const RETIRE_BUDGET: Duration = test_utils::wait::ceiling(Duration::from_secs(180));

/// Ask until the answer is the one the test asserts; under `timing_contract`, fail saying it
/// never came once [`DELIVERY_ATTEMPTS`] asks are spent. An ordinary run asks until it comes, so
/// an answer that never does is a hang left to the runner.
///
/// The shape every gate over a post-startup disk write needs. Such a write reaches the
/// resident by two mechanisms, and BOTH can be late: the change hub's drain runs on a
/// watcher thread the scheduler may starve, and a forced re-scan waits out a storm floor
/// before it walks. A single call is therefore a race — and a race in a smoke test is a
/// test that fails for something it does not measure.
///
/// This does not soften anything. `accept` is the test's own claim, so an answer that
/// never satisfies it is never taken for one, and in the timing profile it fails with the last
/// answer quoted; a claim about what must
/// STAY absent is not asked this way at all — one call already settles it, and asking
/// again could only be answered by the thing the control says is not there.
pub async fn settled<Ask, Fut>(
    expected: &str,
    accept: impl Fn(&Value) -> bool,
    mut ask: Ask,
) -> Value
where
    Ask: FnMut() -> Fut,
    Fut: Future<Output = Value>,
{
    let mut last = Value::Null;
    let asks = test_utils::wait::polls(ASK_AGAIN_EVERY * DELIVERY_ATTEMPTS, ASK_AGAIN_EVERY);
    for _ in 0..asks {
        let answer = ask().await;
        if accept(&answer) {
            return answer;
        }
        last = answer;
        tokio::time::sleep(ASK_AGAIN_EVERY).await;
    }
    panic!("{expected}, and {DELIVERY_ATTEMPTS} asks never said so: {last}");
}
