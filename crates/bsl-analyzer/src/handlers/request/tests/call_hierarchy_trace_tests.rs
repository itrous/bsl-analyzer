use std::sync::Arc;

use parking_lot::Mutex;
use tracing::{
    field::{Field, Visit},
    span::{Attributes, Id},
    Event, Subscriber,
};
use tracing_subscriber::{layer::Context, prelude::*, registry::LookupSpan, Layer};

use super::*;

const TRACE_TEST_CHILD_ENV: &str = "BSL_CALL_HIERARCHY_TRACE_TEST_CHILD";
const TRACE_TEST_NAME: &str =
    "handlers::request::tests::call_hierarchy_trace_tests::call_hierarchy_prepare_then_incoming_traces_index_only_serving";

#[derive(Clone, Default)]
struct SpanCapture {
    spans: Arc<Mutex<Vec<TraceRecord>>>,
    events: Arc<Mutex<Vec<TraceRecord>>>,
}

#[derive(Clone)]
struct TraceRecord {
    name: &'static str,
    fields: String,
}

struct FieldCapture<'a> {
    fields: &'a mut String,
}

impl Visit for FieldCapture<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, &value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, &value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, &value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, &value);
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, &value);
    }
}

impl FieldCapture<'_> {
    fn record(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.push_str(field.name());
        self.fields.push_str(" = ");
        self.fields.push_str(&format!("{value:?}"));
        self.fields.push(' ');
    }
}

impl SpanCapture {
    fn contains(&self, name: &str) -> bool {
        self.spans.lock().iter().any(|captured| captured.name == name)
    }

    fn has_span_fields(&self, name: &str, fields: &[&str]) -> bool {
        self.spans.lock().iter().any(|captured| {
            captured.name == name && fields.iter().all(|field| captured.fields.contains(field))
        })
    }

    fn has_event_fields(&self, fields: &[&str]) -> bool {
        self.events
            .lock()
            .iter()
            .any(|captured| fields.iter().all(|field| captured.fields.contains(field)))
    }
}

impl<S> Layer<S> for SpanCapture
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
        let mut fields = String::new();
        attributes.record(&mut FieldCapture { fields: &mut fields });
        self.spans.lock().push(TraceRecord { name: attributes.metadata().name(), fields });
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = String::new();
        event.record(&mut FieldCapture { fields: &mut fields });
        self.events.lock().push(TraceRecord { name: event.metadata().name(), fields });
    }
}

#[test]
fn call_hierarchy_prepare_then_incoming_traces_index_only_serving() {
    if std::env::var_os(TRACE_TEST_CHILD_ENV).is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TRACE_TEST_NAME])
            .env(TRACE_TEST_CHILD_ENV, "1")
            .status()
            .expect("spawn isolated trace test");
        assert!(status.success(), "isolated trace test must pass");
        return;
    }

    // Given: an LSP call-hierarchy target with one indexed caller.
    let capture = SpanCapture::default();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with(capture.clone());

    tracing::subscriber::with_default(subscriber, || {
        let mut state = create_test_state();
        state.init_empty_source_root();
        let uri = crate::test_uri::file_uri("ch-trace.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let receiver = state.task_pool.receiver.clone();

        // When: prepare authorizes the build and incoming resolves the published index.
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let Task::CallHierarchyIndexBuildRequested { source_root, generation } =
            receiver.try_recv().expect("prepare must enqueue a build")
        else {
            panic!("prepare must enqueue a call-hierarchy build");
        };
        let (built_root, index) = build_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        assert_eq!(built_root, source_root);
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            generation,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(generation),
        ));
        assert!(state.call_hierarchy_index.publish(source_root, generation, index));
        let calls = handle_call_hierarchy_incoming(&latency_ctx(&state), incoming_params(item))
            .expect("incoming request")
            .expect("published index serves callers");

        // Then: the production handlers return the caller through their index-only path.
        assert_eq!(calls.len(), 1);
    });

    assert!(capture.has_span_fields(
        "handle_prepare_call_hierarchy",
        &["source_root =", "generation =", "workspace_call_graph = false"],
    ));
    assert!(capture.has_span_fields(
        "handle_call_hierarchy_incoming",
        &["source_root =", "generation =", "wait_timeout_ms =", "workspace_call_graph = false"],
    ));
    assert!(capture.has_span_fields(
        "call_hierarchy_incoming_served_from_index",
        &["source_root =", "generation =", "caller_count = 1", "workspace_call_graph = false"],
    ));
    assert!(capture.has_event_fields(&[
        "authorization = \"accepted\"",
        "phase = \"prepare\"",
        "message = call hierarchy prepare authorized compact index generation",
    ]));
    assert!(capture.has_event_fields(&[
        "phase = \"incoming\"",
        "wait_result = \"ready\"",
        "message = call hierarchy incoming served from compact index",
    ]));
    assert!(!capture.contains("workspace_call_graph"));
}
