use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

#[cfg(test)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, OnceLock,
};

use anyhow::Result;
use lsp_server::{ErrorCode, Notification, Request, Response};
use salsa::Database as _;
use serde::{de::DeserializeOwned, Serialize};

use crate::frozen_context::{FrozenFilePaths, LatencyRequestContext};
use crate::global_state::{
    GlobalState, LatencyAttemptOutcome, RequestAttemptKey, RequestLifecycle, RequestLifecyclePhase,
    RequestOriginGuard, Task,
};

/// Why a latency request is declined once the initial project was rejected: unlike a
/// load in progress, nothing will open the gate before the server restarts.
pub(crate) const PROJECT_REJECTED_MESSAGE: &str =
    "project failed to load; fix the configuration and restart the server";

pub struct RequestDispatcher<'a> {
    pub req: Option<Request>,
    pub global_state: &'a mut GlobalState,
}

#[derive(Clone, Copy)]
enum LatencyExecutor {
    TaskPool,
    DedicatedThread,
}

/// What a latency request gets while the initial workspace load is still streaming in.
#[derive(Clone, Copy)]
enum WhileLoading {
    Decline,
    /// Answer on the event-loop thread. Only for handlers that read nothing but the
    /// requested file's own text and parse: those are exact against a half-loaded
    /// workspace, and their cost is bounded by one file. Running inline, rather than on
    /// the pool, means the db clone is gone before the loop applies the next streamed
    /// VFS batch — so the batch's revision bump neither waits on the request nor
    /// cancels it into a `RequestCanceled` the client would not retry.
    AnswerInline,
}

#[cfg(test)]
struct G1PendingWriteGate {
    through_attempt: u8,
    entered: crossbeam_channel::Sender<u8>,
    release: Arc<AtomicBool>,
}

#[cfg(test)]
static G1_PENDING_WRITE_GATES: OnceLock<
    Mutex<std::collections::HashMap<String, Arc<G1PendingWriteGate>>>,
> = OnceLock::new();

#[cfg(test)]
fn g1_pending_write_gates(
) -> &'static Mutex<std::collections::HashMap<String, Arc<G1PendingWriteGate>>> {
    G1_PENDING_WRITE_GATES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
pub(crate) fn g1_arm_pending_write_gate(
    id: &lsp_server::RequestId,
    through_attempt: u8,
) -> (crossbeam_channel::Receiver<u8>, Arc<AtomicBool>) {
    let (entered, receiver) = crossbeam_channel::unbounded();
    let release = Arc::new(AtomicBool::new(false));
    let gate =
        Arc::new(G1PendingWriteGate { through_attempt, entered, release: Arc::clone(&release) });
    g1_pending_write_gates()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(format!("{id:?}"), gate);
    (receiver, release)
}

#[cfg(test)]
pub(crate) fn g1_disarm_pending_write_gate(id: &lsp_server::RequestId) {
    g1_pending_write_gates()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&format!("{id:?}"));
}

#[cfg(test)]
fn g1_wait_for_test_writer(key: &RequestAttemptKey, ctx: &LatencyRequestContext) {
    let gate = g1_pending_write_gates()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&format!("{:?}", key.id))
        .cloned();
    let Some(gate) = gate.filter(|gate| key.attempt <= gate.through_attempt) else {
        return;
    };

    let baseline = ctx
        .analysis
        .database()
        .salsa_event_global()
        .expect("controlled overlap uses an event-enabled Salsa database")
        .set_cancellation;
    if gate.entered.send(key.attempt).is_err() {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !gate.release.load(Ordering::Acquire)
        && ctx
            .analysis
            .database()
            .salsa_event_global()
            .is_some_and(|events| events.set_cancellation <= baseline)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "controlled PendingWrite gate did not observe a real writer"
        );
        std::thread::yield_now();
    }
}

impl RequestDispatcher<'_> {
    pub fn on_sync_mut<R>(
        &mut self,
        f: fn(&mut GlobalState, R::Params) -> Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug,
        R::Result: Serialize,
    {
        let (req, params) = match self.parse_request::<R>() {
            Some(it) => it,
            None => return self,
        };

        tracing::debug!("Handling {} (id: {})", R::METHOD, req.id);

        let result = f(self.global_state, params);
        let response = result_to_response::<R>(req.id, result);

        self.global_state.respond(response);
        self
    }

    pub fn on_sync<R>(
        &mut self,
        f: fn(crate::global_state::GlobalStateSnapshot, R::Params) -> Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug,
        R::Result: Serialize,
    {
        let (req, params) = match self.parse_request::<R>() {
            Some(it) => it,
            None => return self,
        };

        tracing::debug!("Handling {} (id: {})", R::METHOD, req.id);

        let snapshot = self.global_state.snapshot();
        let result = f(snapshot, params);
        let response = result_to_response::<R>(req.id, result);

        self.global_state.respond(response);
        self
    }

    pub fn on_latency<R>(
        &mut self,
        f: fn(&LatencyRequestContext, R::Params) -> Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug + Send + 'static,
        R::Result: Serialize + Send + 'static,
    {
        self.on_latency_with::<R>(f, LatencyExecutor::TaskPool, WhileLoading::Decline, false)
    }

    /// Like [`Self::on_latency`], but for a request answered from the requested file's own
    /// syntax tree (document outline, folding, selection ranges): such a request is
    /// answered even while the workspace is loading instead of being declined, because
    /// nothing it reads depends on the rest of the workspace. An editor asks for these
    /// the moment a file is opened, and a decline there leaves the outline empty for the
    /// whole load.
    pub fn on_syntax_latency<R>(
        &mut self,
        f: fn(&LatencyRequestContext, R::Params) -> Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug + Send + 'static,
        R::Result: Serialize + Send + 'static,
    {
        self.on_latency_with::<R>(f, LatencyExecutor::TaskPool, WhileLoading::AnswerInline, false)
    }

    pub fn on_retryable_latency<R>(
        &mut self,
        f: fn(&LatencyRequestContext, R::Params) -> Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug + Send + 'static,
        R::Result: Serialize + Send + 'static,
    {
        self.on_latency_with::<R>(f, LatencyExecutor::TaskPool, WhileLoading::Decline, true)
    }

    pub fn on_waiting_latency<R>(
        &mut self,
        f: fn(&LatencyRequestContext, R::Params) -> Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug + Send + 'static,
        R::Result: Serialize + Send + 'static,
    {
        self.on_latency_with::<R>(f, LatencyExecutor::DedicatedThread, WhileLoading::Decline, false)
    }

    fn on_latency_with<R>(
        &mut self,
        f: fn(&LatencyRequestContext, R::Params) -> Result<R::Result>,
        executor: LatencyExecutor,
        while_loading: WhileLoading,
        retry_eligible: bool,
    ) -> &mut Self
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug + Send + 'static,
        R::Result: Serialize + Send + 'static,
    {
        let (req, params) = match self.parse_request::<R>() {
            Some(it) => it,
            None => return self,
        };

        let executor_name = match executor {
            LatencyExecutor::TaskPool => "task pool",
            LatencyExecutor::DedicatedThread => "dedicated thread",
        };
        tracing::debug!("Handling {} on {} (id: {})", R::METHOD, executor_name, req.id);

        // While the workspace is still loading, analysis would run over a
        // half-loaded source root (wrong results), and its db clone is exactly
        // what every streamed VFS batch's revision bump must wait on — the wait
        // that freezes the load. Decline instead: ContentModified is the
        // benign "state changed, retry if still needed" code clients swallow
        // silently, and the vfs_done finalize replays diagnostics and requests
        // a semantic-tokens refresh, so nothing is permanently lost. A shared session
        // without an editor view is loading too while its owner has no analysis published,
        // a state that moves without an event resetting `vfs_done`.
        let serving = if self.global_state.vfs_done {
            self.global_state.analysis_host.serving_analysis()
        } else {
            None
        };
        let Some(analysis) = serving else {
            // A shared session without an editor view has no store holding the file a
            // syntax request names, only its empty fallback.
            let host = &self.global_state.analysis_host;
            let while_loading = if host.is_shared() && host.view().is_none() {
                WhileLoading::Decline
            } else {
                while_loading
            };
            match while_loading {
                WhileLoading::Decline => {
                    tracing::debug!(
                        method = R::METHOD,
                        id = ?req.id,
                        "declining request: workspace loading"
                    );
                    let message = if self.global_state.project_rejected {
                        PROJECT_REJECTED_MESSAGE
                    } else {
                        "workspace is still loading"
                    };
                    self.global_state.respond(Response::new_err(
                        req.id,
                        ErrorCode::ContentModified as i32,
                        message.to_string(),
                    ));
                }
                WhileLoading::AnswerInline => {
                    tracing::debug!(
                        method = R::METHOD,
                        id = ?req.id,
                        "answering syntax request inline: workspace loading"
                    );
                    // No cancellation token is registered: a `$/cancelRequest` is read by
                    // this same loop, so it cannot arrive while the handler runs.
                    let (ctx, _token) =
                        self.latency_context(self.global_state.analysis_host.analysis());
                    let response = run_latency_handler::<R>(req.id, ctx, params, f);
                    self.global_state.respond(response);
                }
            }
            return self;
        };

        if self.global_state.analysis_host.is_shared()
            && self.global_state.request_lifecycles.len()
                >= self.global_state.task_pool.pool.capacity().unwrap_or(1)
        {
            self.global_state.respond(Response::new_err(
                req.id,
                ErrorCode::ContentModified as i32,
                "server is busy, please retry".to_string(),
            ));
            return self;
        }

        match executor {
            LatencyExecutor::TaskPool if !self.global_state.task_pool.pool.has_capacity() => {
                tracing::warn!(method = R::METHOD, id = ?req.id, "declining request: task pool saturated");
                self.global_state.respond(Response::new_err(
                    req.id,
                    ErrorCode::ContentModified as i32,
                    "server is busy, please retry".to_string(),
                ));
                return self;
            }
            LatencyExecutor::TaskPool | LatencyExecutor::DedicatedThread => {}
        }

        let key = if let Some(key) = self.global_state.retry_dispatch.take() {
            assert_eq!(key.id, req.id, "retry dispatch must match its request id");
            let lifecycle = self
                .global_state
                .request_lifecycles
                .get_mut(&key.id)
                .expect("queued retry keeps its logical request");
            assert_eq!(lifecycle.generation, key.generation);
            assert_eq!(lifecycle.attempt, key.attempt);
            assert_eq!(lifecycle.phase, RequestLifecyclePhase::RetryQueued);
            lifecycle.phase = RequestLifecyclePhase::Running;
            key
        } else {
            self.global_state.next_request_generation =
                self.global_state.next_request_generation.wrapping_add(1);
            let generation = self.global_state.next_request_generation;
            let origin = retry_eligible
                .then(|| retry_origin(&req.method, &req.params))
                .flatten()
                .and_then(|uri| {
                    self.global_state.mem_docs.identity(&uri).map(|identity| RequestOriginGuard {
                        uri,
                        identity,
                        workspace_root: self.global_state.workspace_root.clone(),
                        source_roots: self
                            .global_state
                            .project
                            .as_ref()
                            .map(|project| project.source_roots())
                            .unwrap_or_default(),
                    })
                });
            if let Some(token) = self.global_state.request_tokens.remove(&req.id) {
                token.cancel();
            }
            self.global_state.request_token_owners.remove(&req.id);
            self.global_state.request_lifecycles.insert(
                req.id.clone(),
                RequestLifecycle {
                    generation,
                    method: req.method.clone(),
                    request: retry_eligible.then(|| req.clone()),
                    attempt: 1,
                    phase: RequestLifecyclePhase::Running,
                    retry_origin_revision: None,
                    retry_eligible,
                    origin,
                    client_cancelled: false,
                },
            );
            RequestAttemptKey { id: req.id.clone(), generation, attempt: 1 }
        };

        let (ctx, token) = self.latency_context(analysis);
        if self.global_state.analysis_host.is_shared() {
            self.global_state.shared_request_stamps.insert(
                req.id.clone(),
                (
                    self.global_state.shared_request_generation,
                    salsa::plumbing::current_revision(ctx.analysis.database()),
                ),
            );
        }
        self.global_state.request_token_owners.insert(req.id.clone(), key.clone());
        self.global_state.request_tokens.insert(req.id.clone(), token);

        let job_key = key.clone();
        match executor {
            LatencyExecutor::TaskPool => {
                let spawned = self.global_state.task_pool.pool.try_spawn(move || {
                    let outcome = run_latency_attempt::<R>(job_key.clone(), ctx, params, f);
                    Task::LatencyRequestFinished { key: job_key, outcome }
                });
                if let Err(err) = spawned {
                    self.reject_latency_launch::<R>(&key, err);
                }
            }
            LatencyExecutor::DedicatedThread => {
                let task_sender = self.global_state.task_pool.pool.sender.clone();
                let spawned = std::thread::Builder::new()
                    .name("bsl-call-hierarchy-waiter".to_owned())
                    .spawn(move || {
                        let outcome = run_latency_attempt::<R>(job_key.clone(), ctx, params, f);
                        let _ = task_sender
                            .send(Task::LatencyRequestFinished { key: job_key, outcome });
                    });
                if let Err(err) = spawned {
                    self.reject_latency_launch::<R>(&key, err);
                }
            }
        }
        self
    }

    /// A frozen view of the server over `analysis` for one latency handler, with the token
    /// that cancels the queries it runs.
    fn latency_context(
        &self,
        analysis: ide::Analysis,
    ) -> (LatencyRequestContext, salsa::CancellationToken) {
        let state = &*self.global_state;
        let token = analysis.database().cancellation_token();
        let file_paths = if state.analysis_host.is_shared() {
            let bindings = state.open_file_uri_ids.iter().filter_map(|(uri, &file_id)| {
                if !state.open_files.contains(&file_id) || !state.mem_docs.contains(uri) {
                    return None;
                }
                let path = uri.to_file_path().ok()?;
                if !project_model::is_bsl_source_path(&path)
                    || state
                        .project
                        .as_ref()
                        .is_some_and(|project| project.is_source_excluded(&path))
                {
                    return None;
                }
                Some((uri.clone(), file_id))
            });
            FrozenFilePaths::freeze_database(analysis.database()).with_open_uri_ids(bindings)
        } else {
            FrozenFilePaths::freeze(&state.vfs.read())
        };
        let ctx = LatencyRequestContext {
            analysis,
            workspace_root: state.workspace_root.clone(),
            project: state.project.clone(),
            diagnostics_baseline: Arc::clone(&state.diagnostics_baseline),
            diagnostics_config: state.diagnostics_config().clone(),
            position_encoding: state.position_encoding,
            supports_code_description: state.supports_code_description,
            supports_diagnostic_tags: state.supports_diagnostic_tags,
            supports_insert_text_mode_adjust_indentation: state
                .supports_insert_text_mode_adjust_indentation,
            supports_workspace_edit_document_changes: state
                .supports_workspace_edit_document_changes,
            folding_range_limit: state.folding_range_limit,
            task_sender: state.task_pool.pool.sender.clone(),
            call_hierarchy_index: state.call_hierarchy_index.ensure(),
            call_hierarchy_wait_policy: state.call_hierarchy_wait_policy,
            client_sender: state.sender.clone(),
            mem_docs: state.mem_docs.freeze(),
            file_paths,
            scope_dirty_docs: state.scope_dirty_docs.clone(),
        };
        (ctx, token)
    }

    fn reject_latency_launch<R>(&mut self, key: &RequestAttemptKey, error: impl fmt::Debug)
    where
        R: lsp_types::request::Request,
    {
        tracing::warn!(method = R::METHOD, request_id = ?key.id, attempt = key.attempt, ?error, "could not launch latency request");
        if self.global_state.request_token_owners.get(&key.id) == Some(key) {
            self.global_state.request_token_owners.remove(&key.id);
            self.global_state.request_tokens.remove(&key.id);
        }
        if self.global_state.request_lifecycles.get(&key.id).is_some_and(|lifecycle| {
            lifecycle.generation == key.generation
                && lifecycle.attempt == key.attempt
                && lifecycle.phase == RequestLifecyclePhase::Running
        }) {
            self.global_state.request_lifecycles.remove(&key.id);
            self.global_state.shared_request_stamps.remove(&key.id);
        }
        self.global_state.respond(Response::new_err(
            key.id.clone(),
            ErrorCode::ContentModified as i32,
            "server is busy, please retry".to_string(),
        ));
    }

    pub fn finish(&mut self) {
        if let Some(req) = self.req.take() {
            tracing::error!("Unhandled request: {}", req.method);
            let response = Response::new_err(
                req.id,
                ErrorCode::MethodNotFound as i32,
                format!("Method not found: {}", req.method),
            );
            self.global_state.respond(response);
        }
    }

    fn parse_request<R>(&mut self) -> Option<(Request, R::Params)>
    where
        R: lsp_types::request::Request,
        R::Params: DeserializeOwned + fmt::Debug,
    {
        let req = self.req.as_ref()?;

        if req.method != R::METHOD {
            return None;
        }

        let req = self.req.take().unwrap();

        let params = match serde_json::from_value::<R::Params>(req.params.clone()) {
            Ok(params) => params,
            Err(err) => {
                tracing::error!("Failed to parse params for {}: {}", R::METHOD, err);
                let response = Response::new_err(
                    req.id.clone(),
                    ErrorCode::InvalidParams as i32,
                    format!("Invalid params: {}", err),
                );
                self.global_state.respond(response);
                return None;
            }
        };

        Some((req, params))
    }
}

fn retry_origin(method: &str, params: &serde_json::Value) -> Option<lsp_types::Url> {
    use lsp_types::request::{
        Completion, GotoDefinition, GotoTypeDefinition, HoverRequest, Request as _,
    };

    match method {
        HoverRequest::METHOD => serde_json::from_value::<lsp_types::HoverParams>(params.clone())
            .ok()
            .map(|p| p.text_document_position_params.text_document.uri),
        GotoDefinition::METHOD | GotoTypeDefinition::METHOD => {
            serde_json::from_value::<lsp_types::GotoDefinitionParams>(params.clone())
                .ok()
                .map(|p| p.text_document_position_params.text_document.uri)
        }
        Completion::METHOD => serde_json::from_value::<lsp_types::CompletionParams>(params.clone())
            .ok()
            .map(|p| p.text_document_position.text_document.uri),
        _ => None,
    }
}

pub struct NotificationDispatcher<'a> {
    pub not: Option<Notification>,
    pub global_state: &'a mut GlobalState,
}

impl NotificationDispatcher<'_> {
    pub fn on_sync_mut<N>(
        &mut self,
        f: fn(&mut GlobalState, N::Params) -> Result<()>,
    ) -> Result<&mut Self>
    where
        N: lsp_types::notification::Notification,
        N::Params: DeserializeOwned + fmt::Debug,
    {
        let not = match self.not.as_ref() {
            Some(it) => it,
            None => return Ok(self),
        };

        if not.method != N::METHOD {
            return Ok(self);
        }

        let not = self.not.take().unwrap();

        tracing::debug!("Handling notification: {}", N::METHOD);

        let params = match serde_json::from_value::<N::Params>(not.params.clone()) {
            Ok(params) => params,
            Err(err) => {
                tracing::error!("Failed to parse notification params for {}: {}", N::METHOD, err);
                return Ok(self);
            }
        };

        f(self.global_state, params)?;

        Ok(self)
    }

    pub fn finish(&mut self) {
        if let Some(not) = &self.not {
            if !not.method.starts_with("$/") {
                tracing::warn!("Unhandled notification: {}", not.method);
            }
        }
    }
}

fn result_to_response<R>(id: lsp_server::RequestId, result: Result<R::Result>) -> Response
where
    R: lsp_types::request::Request,
    R::Result: Serialize,
{
    match result {
        Ok(result) => {
            let result = serde_json::to_value(result).unwrap_or_else(|err| {
                tracing::error!("Failed to serialize result for {}: {}", R::METHOD, err);
                serde_json::Value::Null
            });
            Response::new_ok(id, result)
        }
        Err(err) => {
            tracing::error!("Request {} failed: {:?}", R::METHOD, err);
            Response::new_err(id, ErrorCode::InternalError as i32, format!("{:#}", err))
        }
    }
}

fn run_latency_handler<R>(
    id: lsp_server::RequestId,
    ctx: LatencyRequestContext,
    params: R::Params,
    f: fn(&LatencyRequestContext, R::Params) -> Result<R::Result>,
) -> Response
where
    R: lsp_types::request::Request,
    R::Result: Serialize,
{
    let key = RequestAttemptKey { id, generation: 0, attempt: 1 };
    match run_latency_attempt::<R>(key.clone(), ctx, params, f) {
        LatencyAttemptOutcome::Response(response) => response,
        LatencyAttemptOutcome::Panicked { .. } => Response::new_err(
            key.id,
            ErrorCode::InternalError as i32,
            "internal handler panic".to_string(),
        ),
        LatencyAttemptOutcome::Cancelled { kind, .. } => match kind {
            salsa::Cancelled::PropagatedPanic => Response::new_err(
                key.id,
                ErrorCode::ContentModified as i32,
                "content modified".to_string(),
            ),
            _ => Response::new_err(
                key.id,
                ErrorCode::RequestCanceled as i32,
                "request cancelled".to_string(),
            ),
        },
    }
}

fn run_latency_attempt<R>(
    key: RequestAttemptKey,
    ctx: LatencyRequestContext,
    params: R::Params,
    f: fn(&LatencyRequestContext, R::Params) -> Result<R::Result>,
) -> LatencyAttemptOutcome
where
    R: lsp_types::request::Request,
    R::Result: Serialize,
{
    let revision = salsa::plumbing::current_revision(ctx.analysis.database());
    let started = std::time::Instant::now();
    let run_key = key.clone();
    let run = move || {
        #[cfg(test)]
        g1_wait_for_test_writer(&run_key, &ctx);
        let attached = || ctx.analysis.attached(|_| f(&ctx, params));
        match salsa::Cancelled::catch(AssertUnwindSafe(attached)) {
            Ok(result) => {
                LatencyAttemptOutcome::Response(result_to_response::<R>(run_key.id.clone(), result))
            }
            Err(kind) => {
                let cancel_kind = match kind {
                    salsa::Cancelled::Local => "local",
                    salsa::Cancelled::PendingWrite => "pending-write",
                    salsa::Cancelled::PropagatedPanic => "propagated",
                    _ => "other",
                };
                tracing::info!(
                    request_id = ?run_key.id,
                    method = R::METHOD,
                    attempt = run_key.attempt,
                    cancel_kind,
                    revision = ?revision,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "async LSP request unwound"
                );
                if matches!(kind, salsa::Cancelled::PropagatedPanic)
                    && crate::panic_watch::panicked_in(ctx.analysis.database(), revision)
                {
                    LatencyAttemptOutcome::Panicked { revision }
                } else {
                    LatencyAttemptOutcome::Cancelled { kind, revision }
                }
            }
        }
    };

    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(outcome) => outcome,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&'static str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic>".to_string());
            tracing::error!(
                request_id = ?key.id,
                method = R::METHOD,
                attempt = key.attempt,
                revision = ?revision,
                panic = %msg,
                backtrace = %std::backtrace::Backtrace::force_capture(),
                "async LSP handler panicked"
            );
            LatencyAttemptOutcome::Response(Response::new_err(
                key.id,
                ErrorCode::InternalError as i32,
                "internal handler panic".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use lsp_types::request::{HoverRequest, Request as _, Shutdown};
    use std::time::Duration;

    /// Bound of a wait for a task that a regression would never deliver; a hang watchdog,
    /// not a latency measure.
    const HANG_WATCHDOG: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

    fn response_task(task: Task) -> Response {
        match task {
            Task::LatencyRequestFinished {
                outcome: LatencyAttemptOutcome::Response(response),
                ..
            } => response,
            task => panic!("expected a completed latency response, got {task:?}"),
        }
    }

    fn hover_params() -> lsp_types::HoverParams {
        lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: crate::test_uri::file_uri("test.bsl"),
                },
                position: lsp_types::Position { line: 0, character: 0 },
            },
            work_done_progress_params: Default::default(),
        }
    }

    #[test]
    fn test_request_dispatcher_shutdown() {
        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);

        let req = Request::new(
            lsp_server::RequestId::from(1),
            Shutdown::METHOD.to_string(),
            serde_json::Value::Null,
        );

        RequestDispatcher { req: Some(req), global_state: &mut state }
            .on_sync_mut::<Shutdown>(|state, ()| {
                state.shutdown_requested = true;
                Ok(())
            })
            .finish();

        assert!(state.shutdown_requested);
    }

    #[test]
    fn on_latency_declines_with_content_modified_while_loading() {
        let (sender, receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        assert!(!state.vfs_done, "default GlobalState models a loading workspace");

        let req = Request::new(
            lsp_server::RequestId::from(5),
            HoverRequest::METHOD.to_string(),
            serde_json::to_value(hover_params()).unwrap(),
        );

        RequestDispatcher { req: Some(req), global_state: &mut state }
            .on_latency::<HoverRequest>(|_ctx, _p| Ok(None))
            .finish();

        // Declined synchronously: no job spawned, no token registered.
        assert!(state.request_tokens.is_empty(), "no cancellation token for a declined request");
        assert!(
            state.task_pool.receiver.try_recv().is_err(),
            "no task pool job for a declined request"
        );
        let msg = receiver.try_recv().expect("immediate response");
        let lsp_server::Message::Response(response) = msg else {
            panic!("expected a response, got {msg:?}");
        };
        assert_eq!(response.id, lsp_server::RequestId::from(5));
        let err = response.error.expect("loading decline is an error response");
        assert_eq!(err.code, ErrorCode::ContentModified as i32);
    }

    #[test]
    fn on_latency_happy_path() {
        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;

        let req = Request::new(
            lsp_server::RequestId::from(42),
            HoverRequest::METHOD.to_string(),
            serde_json::to_value(hover_params()).unwrap(),
        );

        let receiver = state.task_pool.receiver.clone();
        RequestDispatcher { req: Some(req), global_state: &mut state }
            .on_latency::<HoverRequest>(|_ctx, _p| Ok(None))
            .finish();

        let task =
            receiver.recv_timeout(HANG_WATCHDOG).expect("task pool did not produce RequestResult");
        let response = response_task(task);
        assert_eq!(response.id, lsp_server::RequestId::from(42));
        assert!(response.error.is_none(), "happy path must not set error");
        assert_eq!(response.result, Some(serde_json::Value::Null));
    }

    #[test]
    fn shared_admission_declines_before_launching_an_extra_waiter() {
        let backend = mcp_server::SharedState::shared();
        let (sender, receiver) = unbounded();
        let mut state = GlobalState::new_shared(sender, backend.workspace_handle());
        state.vfs_done = true;
        let capacity = state.task_pool.pool.capacity().unwrap();
        for index in 0..capacity {
            state.request_lifecycles.insert(
                lsp_server::RequestId::from(index as i32),
                RequestLifecycle {
                    generation: 1,
                    method: HoverRequest::METHOD.to_owned(),
                    request: None,
                    attempt: 1,
                    phase: RequestLifecyclePhase::Running,
                    retry_origin_revision: None,
                    retry_eligible: false,
                    origin: None,
                    client_cancelled: false,
                },
            );
        }
        let id = lsp_server::RequestId::from(capacity as i32);
        let request = Request::new(
            id.clone(),
            HoverRequest::METHOD.to_owned(),
            serde_json::to_value(hover_params()).unwrap(),
        );
        RequestDispatcher { req: Some(request), global_state: &mut state }
            .on_waiting_latency::<HoverRequest>(|_, _| {
                panic!("a saturated session must not launch")
            })
            .finish();
        let lsp_server::Message::Response(response) =
            receiver.try_recv().expect("busy admission returns a response synchronously")
        else {
            panic!("busy admission returns a response");
        };
        assert_eq!(response.id, id);
        assert_eq!(response.error.unwrap().code, ErrorCode::ContentModified as i32);
        assert!(!state.request_tokens.contains_key(&id));
        assert_eq!(state.request_lifecycles.len(), capacity);
        assert!(state.task_pool.receiver.try_recv().is_err());
    }

    #[test]
    fn on_latency_panic_becomes_internal_error() {
        let _guard = crate::panic_watch::panic_test_guard();
        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;

        let req = Request::new(
            lsp_server::RequestId::from(7),
            HoverRequest::METHOD.to_string(),
            serde_json::to_value(hover_params()).unwrap(),
        );

        let receiver = state.task_pool.receiver.clone();
        RequestDispatcher { req: Some(req), global_state: &mut state }
            .on_latency::<HoverRequest>(|_ctx, _p| panic!("handler boom"))
            .finish();

        let task =
            receiver.recv_timeout(HANG_WATCHDOG).expect("task pool did not produce RequestResult");
        let response = response_task(task);
        assert_eq!(response.id, lsp_server::RequestId::from(7));
        let err = response.error.expect("panic must produce error");
        assert_eq!(err.code, ErrorCode::InternalError as i32);
        assert!(state.request_tokens.contains_key(&lsp_server::RequestId::from(7)));
    }

    #[test]
    fn on_latency_duplicate_id_cancels_previous() {
        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;

        let id = lsp_server::RequestId::from(99);

        let req1 = Request::new(
            id.clone(),
            HoverRequest::METHOD.to_string(),
            serde_json::to_value(hover_params()).unwrap(),
        );
        RequestDispatcher { req: Some(req1), global_state: &mut state }
            .on_latency::<HoverRequest>(|_ctx, _p| Ok(None))
            .finish();

        let first_token =
            state.request_tokens.get(&id).expect("first dispatch must register token").clone();
        // Let the first job finish before its token is cancelled: the handler runs
        // under `Analysis::attached`, and the exit of that scope resets the token, so a
        // cancel that raced the job's completion could be read back as clear.
        let task = state
            .task_pool
            .receiver
            .recv_timeout(HANG_WATCHDOG)
            .expect("the first job must answer");
        assert!(
            matches!(
                task,
                Task::LatencyRequestFinished { outcome: LatencyAttemptOutcome::Response(_), .. }
            ),
            "{task:?}"
        );

        let req2 = Request::new(
            id.clone(),
            HoverRequest::METHOD.to_string(),
            serde_json::to_value(hover_params()).unwrap(),
        );
        RequestDispatcher { req: Some(req2), global_state: &mut state }
            .on_latency::<HoverRequest>(|_ctx, _p| Ok(None))
            .finish();

        assert!(
            first_token.is_cancelled(),
            "dispatching a duplicate id must cancel the previous token"
        );
        let current =
            state.request_tokens.get(&id).expect("second dispatch must register a new token");
        assert!(!current.is_cancelled(), "new token must still be live");
    }

    /// Builds a `LatencyRequestContext` whose db handle shares its cancellation
    /// token with the returned token — the same wiring `on_latency` sets up, so
    /// cancelling the token unwinds queries run through `ctx.analysis`.
    fn latency_ctx_with_token(
        state: &GlobalState,
    ) -> (crate::frozen_context::LatencyRequestContext, salsa::CancellationToken) {
        use salsa::Database as _;

        let db = state.analysis_host.raw_database().clone();
        let token = db.cancellation_token();
        let analysis = ide::Analysis::from_database(db);
        let ctx = crate::frozen_context::LatencyRequestContext {
            analysis,
            workspace_root: state.workspace_root.clone(),
            project: state.project.clone(),
            diagnostics_baseline: Arc::clone(&state.diagnostics_baseline),
            diagnostics_config: state.diagnostics_config().clone(),
            position_encoding: state.position_encoding,
            supports_code_description: state.supports_code_description,
            supports_diagnostic_tags: state.supports_diagnostic_tags,
            supports_insert_text_mode_adjust_indentation: state
                .supports_insert_text_mode_adjust_indentation,
            supports_workspace_edit_document_changes: state
                .supports_workspace_edit_document_changes,
            folding_range_limit: state.folding_range_limit,
            task_sender: state.task_pool.pool.sender.clone(),
            call_hierarchy_index: state.call_hierarchy_index.ensure(),
            call_hierarchy_wait_policy: state.call_hierarchy_wait_policy,
            client_sender: state.sender.clone(),
            mem_docs: state.mem_docs.freeze(),
            file_paths: crate::frozen_context::FrozenFilePaths::freeze(&state.vfs.read()),
            scope_dirty_docs: state.scope_dirty_docs.clone(),
        };
        (ctx, token)
    }

    #[test]
    fn run_latency_handler_unwinds_cancelled_to_request_canceled() {
        use salsa::Database as _;

        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;

        let (ctx, token) = latency_ctx_with_token(&state);
        // Client cancelled while the request is in flight: the token fires
        // before the handler reaches its cancellation checkpoint.
        token.cancel();

        let id = lsp_server::RequestId::from(11);
        let response =
            run_latency_handler::<HoverRequest>(id.clone(), ctx, hover_params(), |ctx, _p| {
                // Stand-in for the salsa auto-check at a query boundary (or a manual
                // `unwind_if_revision_cancelled()` inside a multi-file loop): a live
                // handler that observes a cancelled token must unwind, not finish.
                ctx.analysis.database().unwind_if_revision_cancelled();
                Ok(None)
            });

        assert_eq!(response.id, id);
        let err = response.error.expect("a cancelled handler must produce an error response");
        assert_eq!(
            err.code,
            ErrorCode::RequestCanceled as i32,
            "an unwound cancellation must map to RequestCanceled, not InternalError"
        );
    }

    /// Salsa reports "the thread owning your query unwound" as
    /// `PropagatedPanic` whether it unwound on a panic or on a pending write —
    /// the claim guard keeps `WaitResult::Cancelled` for a thread-LOCAL
    /// cancellation only. A write-cancelled wait is not an error the user should
    /// see: the client must re-request, which is what `ContentModified` says.
    #[test]
    fn propagated_unwind_without_a_panic_is_content_modified() {
        let _guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::install();

        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;

        let response = propagated_unwind_response(&state, lsp_server::RequestId::from(13));

        let err = response.error.expect("an unwound request must produce an error response");
        assert_eq!(
            err.code,
            ErrorCode::ContentModified as i32,
            "a propagated cancellation must not be reported as an internal panic"
        );
    }

    #[test]
    fn a_panic_in_another_storage_does_not_change_the_request_verdict() {
        let _guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::install();
        let (sender, _) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;
        let (other_sender, _) = unbounded();
        let other = GlobalState::new(other_sender);
        assert_eq!(
            salsa::plumbing::current_revision(state.analysis_host.raw_database()),
            salsa::plumbing::current_revision(other.analysis_host.raw_database()),
        );
        panic_inside_a_query(&other, "another editor query panicked");
        let response = propagated_unwind_response(&state, lsp_server::RequestId::from(18));
        assert_eq!(response.error.unwrap().code, ErrorCode::ContentModified as i32);
    }

    /// The other half of the same verdict: when a panic really did happen while
    /// the request was waiting, the propagated unwind keeps reporting one.
    /// Without this, mapping every propagated unwind to `ContentModified` would
    /// pass the test above while hiding real panics.
    #[test]
    fn propagated_unwind_after_a_real_panic_stays_internal_error() {
        let _guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::install();

        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;

        let (ctx, _token) = latency_ctx_with_token(&state);
        let id = lsp_server::RequestId::from(14);
        let response = run_latency_handler::<HoverRequest>(id, ctx, hover_params(), |ctx, _p| {
            // Stands in for a sibling thread blowing up inside the query this
            // request was blocked on. The message it prints to stderr is
            // expected output of this test.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ctx.analysis.database().attach(|_| panic!("a real panic elsewhere"))
            }));
            std::panic::resume_unwind(Box::new(salsa::Cancelled::PropagatedPanic))
        });

        let err = response.error.expect("an unwound request must produce an error response");
        assert_eq!(
            err.code,
            ErrorCode::InternalError as i32,
            "a real panic must keep reaching the client as one"
        );
    }

    /// Salsa propagates a panic a SECOND time, to a reader that never blocked
    /// on anything: a cycle head that panicked earlier in the same revision
    /// leaves a valueless provisional memo, and reading it throws
    /// `PropagatedPanic` (`function/execute.rs`, `previous_iteration`). That
    /// panic predates the request, so a per-request window cannot see it.
    #[test]
    fn propagated_unwind_replaying_an_earlier_panic_stays_internal_error() {
        let _guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::install();

        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;
        panic_inside_a_query(&state, "cycle head panicked earlier");

        let response = propagated_unwind_response(&state, lsp_server::RequestId::from(15));

        let err = response.error.expect("an unwound request must produce an error response");
        assert_eq!(
            err.code,
            ErrorCode::InternalError as i32,
            "a panic replayed from a poisoned memo must still reach the client"
        );
    }

    /// An LRU eviction takes `&mut` on the database but starts no revision
    /// (`zalsa::evict_lru` does not call `new_revision`, unlike a synthetic
    /// write), so the poisoned memo it leaves behind is still live. Any proxy
    /// for the revision boundary drifts exactly here.
    #[test]
    fn an_lru_trim_between_the_panic_and_the_reader_changes_nothing() {
        let _guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::install();

        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;
        panic_inside_a_query(&state, "cycle head panicked before the idle trim");

        // What the idle trim and the mid-sweep batch do between requests.
        state.analysis_host.raw_database_mut().enforce_lru();

        let response = propagated_unwind_response(&state, lsp_server::RequestId::from(16));

        let err = response.error.expect("an unwound request must produce an error response");
        assert_eq!(
            err.code,
            ErrorCode::InternalError as i32,
            "an LRU eviction is not a revision boundary and cannot clear a panic"
        );
    }

    /// The counterpart that keeps the verdict from being "always InternalError":
    /// a real input write does start a revision, and a poisoned memo cannot
    /// outlive it.
    #[test]
    fn a_write_between_the_panic_and_the_reader_clears_the_verdict() {
        let _guard = crate::panic_watch::panic_test_guard();
        crate::panic_watch::install();

        let (sender, _receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;
        panic_inside_a_query(&state, "a panic of the previous revision");

        // A salsa input write — the real revision boundary.
        state.analysis_host.raw_database_mut().set_workspace_load_complete(true);

        let response = propagated_unwind_response(&state, lsp_server::RequestId::from(17));

        let err = response.error.expect("an unwound request must produce an error response");
        assert_eq!(
            err.code,
            ErrorCode::ContentModified as i32,
            "a panic of a finished revision cannot reach a reader of the next one"
        );
    }

    /// A panic on a thread with the database attached — the only kind that can
    /// strand a waiter or poison a memo, and the only kind that is stamped.
    fn panic_inside_a_query(state: &GlobalState, message: &'static str) {
        use salsa::Database as _;

        let db = state.analysis_host.raw_database();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.attach(|_| panic!("{message}"))
        }));
    }

    /// One request whose handler unwinds the way salsa delivers "the thread
    /// owning your query is gone": `resume_unwind`, which skips the panic hook.
    fn propagated_unwind_response(state: &GlobalState, id: lsp_server::RequestId) -> Response {
        let (ctx, _token) = latency_ctx_with_token(state);
        run_latency_handler::<HoverRequest>(id, ctx, hover_params(), |_ctx, _p| {
            std::panic::resume_unwind(Box::new(salsa::Cancelled::PropagatedPanic))
        })
    }

    #[test]
    fn late_cancel_after_completion_is_noop() {
        let (sender, client_receiver) = unbounded();
        let mut state = GlobalState::new(sender);
        state.vfs_done = true;

        let id = lsp_server::RequestId::from(12);
        let req = Request::new(
            id.clone(),
            HoverRequest::METHOD.to_string(),
            serde_json::to_value(hover_params()).unwrap(),
        );

        let receiver = state.task_pool.receiver.clone();
        RequestDispatcher { req: Some(req), global_state: &mut state }
            .on_latency::<HoverRequest>(|_ctx, _p| Ok(None))
            .finish();

        // The handler completes once, producing exactly one Ok response.
        let task =
            receiver.recv_timeout(HANG_WATCHDOG).expect("task pool did not produce RequestResult");
        let token = state.request_tokens.get(&id).expect("attempt token is registered").clone();
        crate::server::handle_task(&mut state, task)
            .expect("event loop applies request completion");
        let lsp_server::Message::Response(response) =
            client_receiver.recv_timeout(HANG_WATCHDOG).expect("event loop sends one response")
        else {
            panic!("expected request response")
        };
        assert_eq!(response.id, id);
        assert!(response.error.is_none(), "a completed handler must return Ok, not cancelled");

        // A `$/cancelRequest` that races in *after* completion now finds no
        // token: it must take the no-op branch, not panic, and produce no work.
        crate::handlers::notification::handle_cancel(
            &mut state,
            lsp_types::CancelParams { id: lsp_types::NumberOrString::Number(12) },
        )
        .unwrap();

        // No second response was produced by the late cancel.
        assert!(receiver.try_recv().is_err(), "a late cancel must not conjure a second task");
        assert!(
            client_receiver.try_recv().is_err(),
            "a late cancel must not send a second response"
        );

        // The already-consumed token is one-shot: a late `cancel()` (and a
        // duplicate) is harmless and idempotent.
        assert!(!token.is_cancelled());
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }
}
