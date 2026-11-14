//! The resident analysis host's lifecycle, rendered for every tool that reads it.
//!
//! `metadata`, `symbol_info` and `diagnostics` all answer off ONE resident database, so
//! "not ready yet" and "how far along is the build" must read identically whichever of
//! them the consumer asked. A per-tool copy of these shapes drifts — and a consumer that
//! learned to recognize the envelope from one tool then fails to recognize it from the
//! next, which is worse than no envelope at all: an unrecognized "retry" reads as a
//! content answer, so an object that exists looks absent.

use rmcp::model::CallToolResult;
use serde_json::{json, Value};

use crate::diagnostics_state::StatusReport;
use crate::tools::response::structured;

/// The `status` action's body: the resident lifecycle snapshot. Always available (it
/// needs no built resident), so an agent can poll readiness and progress instead of
/// firing a data action just to read its `loading` envelope.
pub(crate) fn status(
    report: &StatusReport,
    owns_caches: Option<bool>,
    standalone_extension: Option<&str>,
) -> CallToolResult {
    let mut body = json!({
        "state": report.state,
        "generation": report.generation,
        "reload": report.reload,
    });
    // A superseded backend keeps answering from what it already holds but produces no new
    // derived state. Without saying so, its answers slowly drift from the sources with nothing
    // in the protocol to explain why. Present once the verdict exists — before that nobody has
    // established it, and a `false` there would be a guess wearing the shape of an answer.
    if let Some(owns) = owns_caches {
        body["owns_caches"] = json!(owns);
    }
    // Which commit this binary came from, so an observation can be dated to the code that
    // produced it: the version number is the same across a day of builds. Absent when the
    // build could not determine it.
    if let Some(build) = build_body(build_info::commit(), build_info::committed()) {
        body["build"] = build;
    }
    // The project is analyzed without its main configuration — the workspace is
    // an extension, or it declares external objects and no base. Calls into that
    // configuration cannot resolve, so the findings this backend is about to
    // report are wrong in a way only the project shape explains.
    if let Some(notice) = standalone_extension {
        body["standalone_extension"] = json!(notice);
    }
    if let Some(files) = report.files {
        body["files"] = json!(files);
    }
    // Assembled field by field, so a new one has to be added here explicitly or the
    // internal report is right while nothing shows outside.
    if let Some(unread) = report.unread_files {
        body["unread_files"] = json!(unread);
    }
    if let Some(ms) = report.elapsed_ms {
        body["elapsed_ms"] = json!(ms);
    }
    if let Some(err) = &report.error {
        body["error"] = json!(err);
    }
    // The retry facts, present only when a failure is being retried: `retry_after_ms` says
    // this failure is retryable and when the next attempt comes (`0` = now), `attempts`
    // says how many have failed in a row, `failed_age_ms` how old the cause is. A caller
    // polling `status` needs all three to pace itself.
    if let Some(ms) = report.retry_after_ms {
        body["retry_after_ms"] = json!(ms);
    }
    if report.attempts > 0 {
        body["attempts"] = json!(report.attempts);
    }
    if let Some(ms) = report.failed_age_ms {
        body["failed_age_ms"] = json!(ms);
    }
    if let Some(watch) = &report.watch {
        body["watch"] = json!({
            "mode": watch.mode,
            "health": watch.health,
            "events_seen": watch.events_seen,
        });
    }
    structured(body)
}

/// The `build` object for a status body: which commit this binary came from. Pure, so
/// the shape is testable without a second build — `status` passes what the binary knows.
fn build_body(commit: Option<&str>, committed: Option<&str>) -> Option<Value> {
    let commit = commit?;
    let mut build = json!({ "commit": commit });
    if let Some(committed) = committed {
        build["committed"] = json!(committed);
    }
    Some(build)
}

/// What a not-ready envelope's `status` field says.
///
/// TOTAL on purpose, and closed on two values: `loading` and `failed` are the only verdicts
/// this envelope publishes, so `ready` can never appear here whatever the lifecycle does
/// underneath — a "not ready" body that says `ready` is a body no consumer of either tool can
/// act on, and the schema of the two tools that publish this envelope would reject it. `idle`
/// reads as `loading` because the lazy build starts with the very next call: "idle" invites
/// the reading that nothing will happen without external help.
pub(crate) fn readiness_status(state: &str) -> &'static str {
    match state {
        "failed" => "failed",
        _ => "loading",
    }
}

/// The two answers a resident read that came back `Failed` can become, decided from ONE
/// lifecycle snapshot — see [`failure_answer`].
pub(crate) enum FailureAnswer {
    /// The resident IS failed: the cause and the structured `data` for the error, both taken
    /// from that snapshot.
    Failed { reason: String, data: Value },
    /// The lifecycle moved on between the read and the snapshot (a retry started, a resident
    /// was published): the snapshot itself is what the tool renders as its not-ready envelope.
    Moved(super::super::diagnostics_state::StatusReport),
}

/// The answer to a resident read that came back `Failed`, from ONE lifecycle snapshot.
///
/// One snapshot, because the failure and its retry contract are facts about the SAME state: a
/// second sample can catch a retry that started in between and publish `state: "failed"` with
/// no `retry_after_ms` — a failure with no way back, which is exactly the shape a client
/// cannot act on. And when the state has moved on, the failure message is about a state that
/// is no longer current at all: what the caller gets is the lifecycle as it IS (the retry
/// running, or the resident that arrived), not a stale verdict carrying the new attempt's
/// numbers.
pub(crate) fn failure_answer(report: &StatusReport, read_reason: &str) -> FailureAnswer {
    if report.state != "failed" {
        return FailureAnswer::Moved(report.clone());
    }
    // The snapshot names its own cause; the read's is the fallback for a failed state that
    // carries none, so a failure republished in between is not answered with the older one.
    let reason = report.error.as_deref().unwrap_or(read_reason).to_owned();
    let mut data = json!({
        "state": "failed",
        "reason": reason,
        "generation": report.generation,
        "attempts": report.attempts,
    });
    if let Some(ms) = report.retry_after_ms {
        data["retry_after_ms"] = json!(ms);
    }
    if let Some(ms) = report.failed_age_ms {
        data["failed_age_ms"] = json!(ms);
    }
    FailureAnswer::Failed { reason, data }
}

/// The body of a "still building the resident database" envelope: the machine-readable
/// `status`/`detail` pair plus the lifecycle snapshot, so a poller can tell a build that
/// is progressing from one that is stuck or failed. Split out from [`loading`] because
/// `metadata` carries the same body under a human sentence rather than the JSON mirror.
pub(crate) fn loading_body(report: &StatusReport, detail: &str) -> Value {
    let mut body = json!({
        // Not a constant: a report whose state is `failed` is rendered as such, so a
        // consumer that branches on this field never reads "still coming" about a build
        // that has stopped.
        "status": readiness_status(report.state),
        "detail": detail,
        "state": report.state,
        "generation": report.generation,
    });
    if let Some(ms) = report.elapsed_ms {
        body["elapsed_ms"] = json!(ms);
    }
    if let Some(err) = &report.error {
        body["error"] = json!(err);
    }
    if let Some(ms) = report.retry_after_ms {
        body["retry_after_ms"] = json!(ms);
    }
    if report.attempts > 0 {
        body["attempts"] = json!(report.attempts);
    }
    if let Some(ms) = report.failed_age_ms {
        body["failed_age_ms"] = json!(ms);
    }
    body
}

/// A transient "still building the resident database" result, emitted while the
/// background build runs. Not an error — the agent should retry shortly.
pub(crate) fn loading(report: &StatusReport, detail: &str) -> CallToolResult {
    structured(loading_body(report, detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics_state::WatchReport;

    fn ready_report() -> StatusReport {
        StatusReport {
            state: "ready",
            generation: 3,
            files: Some(17),
            unread_files: Some(0),
            reload: "none",
            error: None,
            elapsed_ms: None,
            failed_age_ms: None,
            attempts: 0,
            retry_after_ms: None,
            watch: Some(WatchReport { mode: "event-driven", health: "healthy", events_seen: 5 }),
        }
    }

    /// The snapshot every tool reading the resident answers `status` with — `metadata` and
    /// `diagnostics` both render through this one function, so a consumer polling either
    /// reads the same fields.
    #[test]
    fn status_reports_the_lifecycle_snapshot() {
        let body = status(&ready_report(), Some(true), None)
            .structured_content
            .expect("structuredContent");

        assert_eq!(body["state"], "ready");
        assert_eq!(body["generation"], 3);
        assert_eq!(body["files"], 17);
        assert_eq!(body["reload"], "none");
        assert_eq!(body["watch"]["mode"], "event-driven");
    }

    #[test]
    fn status_carries_the_standalone_extension_notice_when_there_is_one() {
        let silent =
            status(&ready_report(), Some(true), None).structured_content.expect("structured");
        let warned =
            status(&ready_report(), Some(true), Some("/ws/ext is a configuration extension"))
                .structured_content
                .expect("structured");

        assert!(silent.get("standalone_extension").is_none(), "no notice, no field");
        assert_eq!(warned["standalone_extension"], "/ws/ext is a configuration extension");
    }

    #[test]
    fn status_states_whether_this_backend_owns_the_derived_caches() {
        // A superseded backend answers from what it holds and produces no new
        // derived state; a client has no other way to learn that.
        let owning = status(&ready_report(), Some(true), None)
            .structured_content
            .expect("structuredContent");
        let superseded = status(&ready_report(), Some(false), None)
            .structured_content
            .expect("structuredContent");
        // And the third state, which is not a verdict at all: nothing has established one yet.
        let unknown =
            status(&ready_report(), None, None).structured_content.expect("structuredContent");

        assert_eq!(owning["owns_caches"], true);
        assert_eq!(superseded["owns_caches"], false);
        assert!(
            unknown.get("owns_caches").is_none(),
            "a verdict nobody has established is published as one: {unknown}",
        );
    }

    #[test]
    fn a_build_without_a_commit_reports_no_build_object() {
        assert!(
            build_body(None, Some("2026-11-14T00:00:00+00:00")).is_none(),
            "a date alone identifies nothing, so it must not be published as a build",
        );
    }

    #[test]
    fn a_stamped_build_reports_commit_and_date() {
        let build = build_body(Some("0123456789ab"), Some("2026-11-14T03:25:06+03:00"))
            .expect("a stamped build has a build object");

        assert_eq!(build["commit"], "0123456789ab");
        assert_eq!(build["committed"], "2026-11-14T03:25:06+03:00");
    }

    #[test]
    fn the_status_body_carries_the_build_when_the_binary_knows_it() {
        let body =
            status(&ready_report(), None, None).structured_content.expect("structuredContent");

        // Conditional on purpose: the stamp is a compile-time property of the running test
        // binary, so one binary cannot exercise both branches.
        match build_info::commit() {
            Some(commit) => assert_eq!(body["build"]["commit"], commit),
            None => assert!(body["build"].is_null(), "no stamp, no build object: {body}"),
        }
    }

    /// A failed build is not a slow one: the envelope's `status` says `failed` (the flat
    /// "loading" was a lie an agent polling readiness could not act on), the cause and the
    /// retry contract travel beside it, and the structured error carries the same verdict.
    #[test]
    fn a_failed_build_surfaces_its_error_in_both_shapes() {
        let report = StatusReport {
            state: "failed",
            generation: 0,
            files: None,
            unread_files: None,
            reload: "none",
            error: Some("builder panicked".to_owned()),
            elapsed_ms: None,
            failed_age_ms: Some(1_500),
            attempts: 2,
            retry_after_ms: Some(4_500),
            watch: None,
        };

        let status_body =
            status(&report, Some(true), None).structured_content.expect("structuredContent");
        assert_eq!(status_body["state"], "failed");
        assert_eq!(status_body["error"], "builder panicked");
        assert_eq!(status_body["retry_after_ms"], 4_500);
        assert_eq!(status_body["attempts"], 2);
        assert_eq!(status_body["failed_age_ms"], 1_500);

        let loading_body = loading_body(&report, "building; retry shortly");
        assert_eq!(
            loading_body["status"], "failed",
            "a failed resident is not building: the envelope must not read as `loading`",
        );
        assert_eq!(loading_body["state"], "failed");
        assert_eq!(loading_body["error"], "builder panicked");
        assert_eq!(loading_body["retry_after_ms"], 4_500);
        assert_eq!(loading_body["attempts"], 2);

        let answer = failure_answer(&report, "builder panicked");
        let FailureAnswer::Failed { data, .. } = answer else {
            panic!("a failed report must answer as a failure");
        };
        assert_eq!(data["state"], "failed");
        assert_eq!(data["reason"], "builder panicked");
        assert_eq!(data["retry_after_ms"], 4_500);
        assert_eq!(data["attempts"], 2);
    }

    /// The reason is a fact of the same snapshot as the retry contract: a failure republished
    /// between the read and the snapshot is answered with ITS cause, not the cause the read saw
    /// beside the newer attempt's numbers.
    #[test]
    fn a_failure_is_answered_with_the_reason_of_its_own_snapshot() {
        let report = StatusReport {
            state: "failed",
            generation: 1,
            files: None,
            unread_files: None,
            reload: "none",
            error: Some("second attempt failed".to_owned()),
            elapsed_ms: None,
            failed_age_ms: Some(10),
            attempts: 2,
            retry_after_ms: Some(9_000),
            watch: None,
        };

        let FailureAnswer::Failed { reason, data } =
            failure_answer(&report, "first attempt failed")
        else {
            panic!("a failed report must answer as a failure");
        };
        assert_eq!(reason, "second attempt failed", "the error text names the snapshot's cause");
        assert_eq!(data["reason"], "second attempt failed");
        assert_eq!(data["attempts"], 2);
    }

    /// The status field is TOTAL, and closed on two values: whatever the lifecycle does, this
    /// envelope never publishes `ready`/`disabled` — a "not ready" body claiming readiness is
    /// one no consumer can act on, and the two tools that publish this shape into a closed
    /// schema would reject it.
    #[test]
    fn the_not_ready_envelope_publishes_only_loading_or_failed() {
        for state in ["disabled", "idle", "loading", "ready", "failed"] {
            let published = readiness_status(state);
            assert!(
                matches!(published, "loading" | "failed"),
                "state {state} published as {published}",
            );
            assert_eq!(published == "failed", state == "failed", "state {state}");

            let report = StatusReport {
                state,
                generation: 7,
                files: None,
                unread_files: None,
                reload: "none",
                error: (state == "failed").then(|| "cause".to_owned()),
                elapsed_ms: None,
                failed_age_ms: None,
                attempts: 0,
                retry_after_ms: None,
                watch: None,
            };
            let body = loading_body(&report, "not ready");
            assert!(
                matches!(body["status"].as_str(), Some("loading" | "failed")),
                "the envelope published {body} for state {state}",
            );
        }
    }

    /// The failure answer is taken from ONE snapshot: a report that is not failed is answered
    /// with the LIFECYCLE, never with a failure wearing no retry contract.
    #[test]
    fn a_moved_on_lifecycle_is_not_served_as_a_failure() {
        let mut report = StatusReport {
            state: "loading",
            generation: 4,
            files: None,
            unread_files: None,
            reload: "none",
            error: Some("the cause the retry answers".to_owned()),
            elapsed_ms: Some(120),
            failed_age_ms: Some(9_000),
            attempts: 1,
            retry_after_ms: None,
            watch: None,
        };

        match failure_answer(&report, "boom") {
            FailureAnswer::Moved(moved) => {
                assert_eq!(moved.state, "loading");
                assert_eq!(moved.error.as_deref(), Some("the cause the retry answers"));
                assert_eq!(moved.elapsed_ms, Some(120));
            }
            FailureAnswer::Failed { data, .. } => {
                panic!("a running retry was served as a failure: {data}")
            }
        }

        // Ready too: a resident that arrived while the failed read was in flight is not
        // described as a failure with no way back.
        report.state = "ready";
        report.files = Some(3);
        report.retry_after_ms = None;
        assert!(matches!(failure_answer(&report, "boom"), FailureAnswer::Moved(_)));
    }

    /// The control on the test above: a state that is not failed keeps the `loading`
    /// verdict, so the envelope's status is derived from the lifecycle rather than pinned.
    #[test]
    fn a_live_build_still_reads_as_loading() {
        let report = StatusReport {
            state: "loading",
            generation: 1,
            files: None,
            unread_files: None,
            reload: "none",
            error: Some("previous attempt failed".to_owned()),
            elapsed_ms: Some(30),
            failed_age_ms: Some(1_000),
            attempts: 1,
            retry_after_ms: None,
            watch: None,
        };

        let body = loading_body(&report, "building; retry shortly");
        assert_eq!(body["status"], "loading");
        assert_eq!(body["state"], "loading");
        assert_eq!(body["elapsed_ms"], 30);
        // The retry of a failure keeps the cause it answers: a caller sees WHY the second
        // attempt is running, not a state indistinguishable from a first one.
        assert_eq!(body["error"], "previous attempt failed");
        assert!(body.get("retry_after_ms").is_none(), "a build in flight owes no wait: {body}",);
    }
}
