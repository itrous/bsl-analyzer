use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use ide::metadata_rename_plan::{
    prepare_metadata_rename_plan, validate_metadata_plan_snapshot, MetadataPlanRoot,
    MetadataRenamePlan, PreparedMetadataRenameSnapshot,
};
use ide_host_core::{WorkspaceDiskGate, WorkspaceDiskObservation, WorkspaceDiskWrite};
use lsp_server::{ErrorCode, Notification, Request, RequestId, Response};
use lsp_types::Url;
use serde_json::{json, Value};

use super::context::capture_project_context;
use super::documents::{
    document_path, freeze_context_manifest, freeze_document_manifest, matches_context_input,
};
use super::journal::{Bounds, JournalStore, Receipt, Status};
use super::protocol::{self, AbortParams, CommitParams, PreviewParams, PreviewResult};
use super::snapshot::capture_metadata_snapshot;
use crate::global_state::{GlobalState, GlobalStateSnapshot};
use crate::mem_docs::FrozenMemDocs;

const SESSION_TTL: Duration = Duration::from_secs(300);
const MAX_SESSIONS: usize = 4;
const MAX_FILES: usize = 10_000;
const MAX_PREVIEW_BYTES: usize = 8 * 1024 * 1024;
const JOURNAL_BOUNDS: Bounds = Bounds {
    record_bytes: 8 * 1024 * 1024,
    blob_bytes: 64 * 1024 * 1024,
    total_blob_bytes: 128 * 1024 * 1024,
    entries: MAX_FILES,
};

#[derive(Debug)]
pub(crate) struct Session {
    plan: SessionPlan,
    created: Instant,
}

#[derive(Debug)]
enum SessionPlan {
    Rename {
        plan: Box<MetadataRenamePlan>,
        context_documents:
            BTreeMap<std::path::PathBuf, ide::metadata_rename_plan::MetadataOpenDocument>,
    },
    Recover {
        roots: Vec<MetadataPlanRoot>,
        context_sha256: [u8; 32],
        documents: BTreeMap<std::path::PathBuf, ide::metadata_rename_plan::MetadataOpenDocument>,
    },
}

#[derive(Debug)]
struct Active {
    id: RequestId,
    session_id: String,
    committing: bool,
    cancelled: Arc<AtomicBool>,
    violated: Arc<AtomicBool>,
    roots: Vec<MetadataPlanRoot>,
    read_cancel: Option<salsa::CancellationToken>,
    context_inputs: Vec<super::context::MetadataContextInput>,
}

impl Active {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(token) = &self.read_cancel {
            token.cancel();
        }
    }

    fn violate(&self) {
        self.violated.store(true, Ordering::Release);
        if let Some(token) = &self.read_cancel {
            token.cancel();
        }
    }
}

pub(crate) struct State {
    pub supported: bool,
    sessions: BTreeMap<String, Box<Session>>,
    sequence: u64,
    active: Option<Active>,
    shutdown_pending: bool,
    completion_sender: crossbeam_channel::Sender<Completion>,
    pub completion_receiver: crossbeam_channel::Receiver<Completion>,
    pub needs_rescan: bool,
    pub rescan_version: Option<u32>,
    pub gate: WorkspaceDiskGate,
    pub writer: Option<WorkspaceDiskWrite>,
    pub pending_response: Option<Response>,
    pub buffers_deferred: bool,
    publication: Option<Box<Publication>>,
    #[cfg(test)]
    pub cache_authority: Option<std::path::PathBuf>,
    #[cfg(test)]
    probe: Option<Arc<TransactionProbe>>,
}

impl Default for State {
    fn default() -> Self {
        let (completion_sender, completion_receiver) = crossbeam_channel::bounded(1);
        Self {
            supported: false,
            sessions: BTreeMap::new(),
            sequence: 0,
            active: None,
            shutdown_pending: false,
            completion_sender,
            completion_receiver,
            needs_rescan: false,
            rescan_version: None,
            gate: WorkspaceDiskGate::default(),
            writer: None,
            pending_response: None,
            buffers_deferred: false,
            publication: None,
            #[cfg(test)]
            cache_authority: None,
            #[cfg(test)]
            probe: None,
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancel();
        }
    }
}

impl State {
    pub(crate) fn committing(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.committing)
    }

    fn expire(&mut self) {
        self.sessions.retain(|_, session| session.created.elapsed() < SESSION_TTL);
    }
}

#[derive(Debug)]
pub struct Completion {
    id: RequestId,
    session_id: String,
    result: std::result::Result<Finished, String>,
}

#[derive(Debug)]
enum Finished {
    Preview {
        session: Option<Box<Session>>,
        reply: PreviewResult,
    },
    Commit {
        receipt: Receipt,
        roots: Vec<MetadataPlanRoot>,
        writer: WorkspaceDiskWrite,
        publication: Box<Publication>,
    },
    Recovered {
        receipts: Vec<Receipt>,
        roots: Vec<MetadataPlanRoot>,
        writer: WorkspaceDiskWrite,
        publication: Box<Publication>,
        publication_only: std::collections::BTreeSet<String>,
    },
}

pub(crate) fn is_request(method: &str) -> bool {
    matches!(method, protocol::PREVIEW | protocol::COMMIT | protocol::ABORT)
}

pub(crate) fn client_supported(capabilities: &Value) -> bool {
    protocol::supported(capabilities)
}

pub(crate) fn refresh_loader(mut message: vfs::loader::Message) -> vfs::loader::Message {
    use vfs::loader::Message;
    match &mut message {
        Message::Loaded { files } | Message::Changed { files } => {
            // Watcher payloads collected during commit may contain an intermediate version.
            for (path, bytes) in files {
                *bytes = std::fs::read(path.as_path()).ok();
            }
        }
        Message::RemovedRecursive { paths } => paths.retain(|path| {
            let path: &Path = path.as_ref();
            !path.exists()
        }),
        _ => {}
    }
    message
}

pub(crate) fn request_shutdown(state: &mut GlobalState) -> bool {
    if let Some(active) = &state.metadata_rename.active {
        active.cancel();
        state.metadata_rename.shutdown_pending = true;
        return true;
    }
    false
}

pub(crate) fn handle_request(state: &mut GlobalState, request: Request) -> Result<()> {
    let id = request.id.clone();
    if !state.metadata_rename.supported {
        state.respond(Response::new_err(
            id,
            ErrorCode::InvalidRequest as i32,
            "metadata rename requires protocolVersion=1 and buffersOwnReloads=true".into(),
        ));
        return Ok(());
    }
    let result = start_request(state, request);
    if let Err(error) = result {
        state.respond(Response::new_err(id, ErrorCode::RequestFailed as i32, error.to_string()));
    }
    Ok(())
}

fn start_request(state: &mut GlobalState, request: Request) -> Result<()> {
    state.metadata_rename.expire();
    if request.method == protocol::ABORT {
        let params: AbortParams = serde_json::from_value(request.params)?;
        let global_violation = params.violation
            && !(params.dirty == Some(false)
                && params.event_kind == Some(protocol::DocumentEventKind::Change));
        let reported =
            if global_violation {
                params.document_uri.as_ref().and_then(|uri| uri.to_file_path().ok()).is_some_and(
                    |path| state.workspace_disk_gate().report_document_change(&path, None),
                )
            } else {
                false
            };
        let removed = state.metadata_rename.sessions.remove(&params.session_id).is_some();
        let running = state
            .metadata_rename
            .active
            .as_ref()
            .filter(|active| active.session_id == params.session_id);
        if let Some(active) = running {
            if params.violation {
                active.violate();
                state.workspace_disk_gate().participant_violation().store(true, Ordering::Release);
            } else {
                active.cancel();
            }
        }
        let aborted = removed || running.is_some();
        state.respond(Response::new_ok(
            request.id,
            json!({"aborted": aborted,"violationReported":reported}),
        ));
        return Ok(());
    }
    ensure!(state.metadata_rename.active.is_none(), "a metadata rename request is already running");
    ensure!(state.vfs_done && !state.project_rejected, "metadata workspace is not ready");
    let cancelled = Arc::new(AtomicBool::new(false));
    let violated = Arc::new(AtomicBool::new(false));
    let gate = state.workspace_disk_gate();
    let mut snapshot = if gate.recovery_required() {
        InputAdmission::Recovery(gate.begin_recovery(Duration::from_secs(30), &|| false)?)
    } else {
        let read = state.prepare_disk_read()?;
        let snapshot = state.snapshot()?;
        drop(read);
        InputAdmission::Readable(Box::new(snapshot))
    };
    let project = state.project.as_ref().context("metadata project is not loaded")?;
    let shared_project = state.analysis_host.owner().is_some();
    let context = capture_selected_context(project, shared_project)?;
    #[cfg(test)]
    let store = match &state.metadata_rename.cache_authority {
        Some(path) => JournalStore::open(path, JOURNAL_BOUNDS)?,
        None => store(&context.roots)?,
    };
    #[cfg(not(test))]
    let store = store(&context.roots)?;
    let documents = state.mem_docs.freeze();
    let roots = context.roots.clone();
    let (session_id, committing, work) = if request.method == protocol::PREVIEW {
        ensure!(
            state.metadata_rename.sessions.len() < MAX_SESSIONS,
            "too many metadata previews; abort an earlier preview"
        );
        let params: PreviewParams = serde_json::from_value(request.params)?;
        ensure!(params.documents.len() <= MAX_FILES, "too many open metadata documents");
        state.metadata_rename.sequence += 1;
        let session_id = format!("{}-{}", std::process::id(), state.metadata_rename.sequence);
        (session_id, false, Work::Preview(params))
    } else {
        let params: CommitParams = serde_json::from_value(request.params)?;
        ensure!(
            params.exclusive_session,
            "metadata commit requires an explicitly confirmed exclusive session"
        );
        ensure!(params.documents.len() <= MAX_FILES, "too many open metadata documents");
        let session = state
            .metadata_rename
            .sessions
            .remove(&params.session_id)
            .context("metadata preview expired, was aborted, or was already consumed")?;
        (params.session_id.clone(), true, Work::Commit(params, session.plan))
    };
    let observation = if committing {
        let filter = mutation_filter(&context, Vec::new());
        match &mut snapshot {
            InputAdmission::Readable(_) => Some(gate.observe_document_changes(filter)?),
            InputAdmission::Recovery(writer) => {
                writer.observe_document_changes(filter);
                None
            }
        }
    } else {
        None
    };
    let active = Active {
        id: request.id.clone(),
        session_id: session_id.clone(),
        committing,
        cancelled: Arc::clone(&cancelled),
        violated: Arc::clone(&violated),
        roots,
        context_inputs: context.inputs.clone(),
        read_cancel: match &snapshot {
            InputAdmission::Readable(snapshot) => {
                use salsa::Database as _;
                Some(snapshot.analysis.database().cancellation_token())
            }
            InputAdmission::Recovery(_) => None,
        },
    };
    let sender = state.metadata_rename.completion_sender.clone();
    let filesystem = TransactionEnvironment {
        gate,
        observation,
        store,
        shared_project,
        #[cfg(test)]
        probe: state.metadata_rename.probe.clone(),
    };
    let completion_id = request.id;
    // The applying worker never enters the latency dispatcher: writes have one attempt.
    let spawn = std::thread::Builder::new().name("metadata-rename".into()).spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(snapshot, documents, context, work, &cancelled, &violated, filesystem)
        }))
        .map_err(|_| {
            "metadata rename interrupted; rebuild the preview to recover its journal".into()
        })
        .and_then(|result| result.map_err(|error| error.to_string()));
        let _ = sender.send(Completion { id: completion_id, session_id, result });
    });
    spawn.context("cannot start metadata rename worker")?;
    state.metadata_rename.active = Some(active);
    Ok(())
}

enum Work {
    Preview(PreviewParams),
    Commit(CommitParams, SessionPlan),
}

enum InputAdmission {
    Readable(Box<GlobalStateSnapshot>),
    Recovery(WorkspaceDiskWrite),
}

#[derive(Debug)]
struct Publication {
    store: JournalStore,
    roots: Vec<MetadataPlanRoot>,
    context: [u8; 32],
    transactions: Vec<String>,
}

impl Publication {
    fn complete(&self) -> Result<()> {
        for transaction in &self.transactions {
            self.store.complete_publication(transaction, &self.roots, self.context)?;
        }
        Ok(())
    }
}

struct TransactionEnvironment {
    gate: WorkspaceDiskGate,
    observation: Option<WorkspaceDiskObservation>,
    store: JournalStore,
    shared_project: bool,
    #[cfg(test)]
    probe: Option<Arc<TransactionProbe>>,
}

#[cfg(test)]
pub(crate) struct TransactionProbe {
    before_writer: bool,
    paused: crossbeam_channel::Sender<usize>,
    resume: crossbeam_channel::Receiver<()>,
    #[cfg(unix)]
    host: Option<ExtensionHostProbe>,
}

#[cfg(test)]
#[cfg(unix)]
struct ExtensionHostProbe {
    source: std::path::PathBuf,
    destination: std::path::PathBuf,
    first_operation: bool,
    fault: bool,
    phase: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
#[cfg(unix)]
impl ExtensionHostProbe {
    fn reached(&self, applied: usize) -> bool {
        if self.first_operation {
            applied == 1
        } else {
            !self.source.exists()
                && self.destination.is_dir()
                && !self.source.with_extension("xml").exists()
                && self.destination.with_extension("xml").is_file()
        }
    }
}

#[cfg(test)]
pub(crate) fn install_transaction_probe(
    state: &mut GlobalState,
    probe: Option<Arc<TransactionProbe>>,
) {
    state.metadata_rename.probe = probe;
}

fn store(roots: &[MetadataPlanRoot]) -> Result<JournalStore> {
    let cache_parent = dirs::cache_dir()
        .context("no common cache authority for metadata journals")?
        .canonicalize()
        .context("metadata journal cache authority does not exist")?;
    let cache = cache_parent.join("bsl-analyzer");
    ensure!(
        !roots.iter().any(|root| cache.starts_with(&root.canonical_path)),
        "metadata journal cache authority is inside a source root"
    );
    std::fs::create_dir_all(&cache)?;
    JournalStore::open(&cache, JOURNAL_BOUNDS)
}

fn execute(
    snapshot: InputAdmission,
    documents: FrozenMemDocs,
    context: super::context::MetadataProjectContext,
    work: Work,
    cancelled: &AtomicBool,
    violated: &AtomicBool,
    filesystem: TransactionEnvironment,
) -> Result<Finished> {
    let TransactionEnvironment {
        gate,
        observation,
        store,
        shared_project,
        #[cfg(test)]
        probe,
    } = filesystem;
    let pending = store.pending(&context.roots, context.context_sha256)?;
    ensure!(!cancelled.load(Ordering::Acquire), "metadata rename cancelled");
    match work {
        Work::Preview(params) => {
            let manifest = freeze_document_manifest(&context.roots, &documents, &params.documents)?;
            let context_documents = freeze_context_manifest(&context.inputs, &params.documents)?;
            if !pending.is_empty() || matches!(snapshot, InputAdmission::Recovery(_)) {
                let approved = pending.iter().all(|journal| journal.recovery_approved);
                let mut files = Vec::new();
                let mut preview_bytes = 0usize;
                if approved {
                    let source = capture_metadata_snapshot(
                        &ide_db::RootDatabaseImpl::new(),
                        context.roots.clone(),
                        context.context_sha256,
                        &context.inputs,
                        &manifest,
                        cancelled,
                    )?;
                    for journal in &pending {
                        if journal.publication_only {
                            continue;
                        }
                        let receipt = store.preview_recovery(
                            &journal.transaction,
                            &context.roots,
                            context.context_sha256,
                        )?;
                        for known in receipt.known_files {
                            if !is_text_source(&known.old_path) {
                                continue;
                            }
                            let root = context
                                .roots
                                .iter()
                                .find(|root| root.id == known.root_id)
                                .context("unknown recovery preview root")?;
                            if context.inputs.iter().any(|input| {
                                matches_context_input(
                                    input,
                                    &root.source_path.join(&known.old_path),
                                )
                            }) {
                                continue;
                            }
                            let Some(after_text) = base_db::decode_disk_bytes(&known.bytes) else {
                                continue;
                            };
                            let before_text = source
                                .files
                                .iter()
                                .find(|file| {
                                    file.root_id == known.root_id
                                        && file.relative_path == known.old_path
                                })
                                .and_then(|file| base_db::decode_disk_bytes(&file.bytes))
                                .unwrap_or_default();
                            if before_text != after_text {
                                preview_bytes = preview_bytes
                                    .saturating_add(before_text.len())
                                    .saturating_add(after_text.len());
                                ensure!(
                                    preview_bytes <= MAX_PREVIEW_BYTES,
                                    "metadata recovery preview exceeds response size limit"
                                );
                                files.push(protocol::PreviewFile {
                                    uri: file_uri(&root.source_path.join(known.old_path))?,
                                    before_text,
                                    after_text,
                                });
                            }
                        }
                    }
                }
                let protocol_roots = preview_roots(&context.roots)?;
                let context_inputs = preview_context_inputs(&context.inputs)?;
                let mut all_documents = manifest;
                all_documents.extend(context_documents);
                let clean = !all_documents.values().any(|document| document.dirty);
                let session = (approved && clean).then(|| {
                    Box::new(Session {
                        created: Instant::now(),
                        plan: SessionPlan::Recover {
                            roots: context.roots,
                            context_sha256: context.context_sha256,
                            documents: all_documents,
                        },
                    })
                });
                let reply = PreviewResult {
                    operation: "recover",
                    roots: protocol_roots,
                    context_inputs,
                    session_id: None,
                    expires_in_seconds: SESSION_TTL.as_secs(),
                    can_apply: approved && clean,
                    problems: if !clean {
                        vec!["save participating documents before recovery".into()]
                    } else if approved {
                        Vec::new()
                    } else {
                        vec!["recovery_required: pending journal has a different approved source context".into()]
                    },
                    files,
                    resources: Vec::new(),
                    documents: params.documents,
                };
                ensure!(
                    serde_json::to_vec(&reply)?.len() <= MAX_PREVIEW_BYTES,
                    "metadata recovery preview exceeds response size limit"
                );
                return Ok(Finished::Preview { session, reply });
            }
            let InputAdmission::Readable(snapshot) = snapshot else {
                anyhow::bail!("recovery_required: no approved pending journal is available");
            };
            let source = capture_metadata_snapshot(
                snapshot.analysis.database(),
                context.roots.clone(),
                context.context_sha256,
                &context.inputs,
                &manifest,
                cancelled,
            )?;
            let path = params
                .text_document
                .uri
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("metadata target must be a file URI"))?;
            let canonical = path.canonicalize()?;
            let file = source
                .files
                .iter()
                .find(|file| {
                    source.roots.iter().any(|root| {
                        root.id == file.root_id
                            && root.canonical_path.join(&file.relative_path) == canonical
                    })
                })
                .context("metadata target is outside the captured source roots")?;
            let file_id = file.file_id.context("metadata target is not indexed")?;
            let disk_text = base_db::decode_disk_bytes(&file.bytes)
                .context("unsupported metadata source encoding")?;
            let editor_document = documents.get(&params.text_document.uri);
            let text = editor_document
                .map(|document| document.text())
                .unwrap_or_else(|| disk_text.strip_prefix('\u{feff}').unwrap_or(&disk_text));
            let offset = crate::lsp::offset_with_encoding(
                &line_index::LineIndex::new(text),
                text,
                params.position,
                snapshot.position_encoding,
            )?;
            let resident = snapshot.analysis.file_text(file_id);
            let offset = u32::from(offset)
                + if editor_document.is_none() && resident.starts_with('\u{feff}') { 3 } else { 0 };
            let target = snapshot
                .analysis
                .metadata_target_at(file_id, offset)
                .map_err(|error| anyhow::anyhow!("metadata target is not proven: {error:?}"))?;
            let plan = prepare_metadata_rename_plan(
                snapshot.analysis.database(),
                file_id,
                &target.path,
                &params.new_name,
                &source,
                MAX_FILES,
            )
            .map_err(|error| anyhow::anyhow!("metadata rename plan rejected: {error:?}"))?;
            ensure!(!cancelled.load(Ordering::Acquire), "metadata rename cancelled");
            let mut reply = preview_reply(&plan, &source, params.documents)?;
            reply.context_inputs = preview_context_inputs(&context.inputs)?;
            if context_documents.values().any(|document| document.dirty) {
                reply.can_apply = false;
                reply.problems.push("save metadata context documents before commit".into());
            }
            if let Err(problem) = validate_resource_selection(&context.project, &plan) {
                reply.can_apply = false;
                reply.problems.push(problem.to_string());
            }
            ensure!(
                serde_json::to_vec(&reply)?.len() <= MAX_PREVIEW_BYTES,
                "metadata preview exceeds response size limit"
            );
            let session = reply.can_apply.then(|| {
                Box::new(Session {
                    plan: SessionPlan::Rename { plan: Box::new(plan), context_documents },
                    created: Instant::now(),
                })
            });
            Ok(Finished::Preview { session, reply })
        }
        Work::Commit(
            params,
            SessionPlan::Recover { roots, context_sha256, documents: expected_documents },
        ) => {
            ensure!(
                roots == context.roots && context_sha256 == context.context_sha256,
                "recovery_required: source context changed after recovery preview"
            );
            let manifest = freeze_document_manifest(&context.roots, &documents, &params.documents)?;
            let context_documents = freeze_context_manifest(&context.inputs, &params.documents)?;
            let mut all_documents = manifest.clone();
            all_documents.extend(context_documents);
            ensure!(
                all_documents == expected_documents,
                "recovery preview has stale document versions"
            );
            let db = ide_db::RootDatabaseImpl::new();
            let source = capture_metadata_snapshot(
                &db,
                context.roots.clone(),
                context.context_sha256,
                &context.inputs,
                &manifest,
                cancelled,
            )?;
            ensure!(
                !all_documents.values().any(|document| document.dirty)
                    && !source.files.iter().any(|file| file
                        .open_document
                        .as_ref()
                        .is_some_and(|document| document.dirty)),
                "save participating documents before recovery"
            );
            let mut expected = Vec::new();
            for file in &source.files {
                if is_text_source(&file.relative_path) {
                    if let Some(text) = base_db::decode_disk_bytes(&file.bytes) {
                        expected.push((
                            file.root_id.clone(),
                            file.relative_path.clone(),
                            ide::metadata_rename_plan::metadata_document_fingerprint(&text),
                        ));
                    }
                }
            }
            for journal in &pending {
                for file in store
                    .preview_recovery(&journal.transaction, &roots, context_sha256)?
                    .known_files
                {
                    if is_text_source(&file.old_path) {
                        if let Some(text) = base_db::decode_disk_bytes(&file.bytes) {
                            expected.retain(|(root, path, _)| {
                                *root != file.root_id || *path != file.old_path
                            });
                            expected.push((
                                file.root_id,
                                file.old_path,
                                ide::metadata_rename_plan::metadata_document_fingerprint(&text),
                            ));
                        }
                    }
                }
            }
            let filter = mutation_filter(&context, expected);
            let mut writer = match snapshot {
                InputAdmission::Recovery(mut writer) => {
                    writer.observe_document_changes(filter);
                    writer
                }
                InputAdmission::Readable(snapshot) => {
                    drop(snapshot);
                    let mut writer = observation
                        .context("recovery commit has no owned mutation observation")?
                        .begin_recovery(Duration::from_secs(30), &|| {
                            cancelled.load(Ordering::Acquire)
                        })?;
                    writer.observe_document_changes(filter);
                    writer
                }
            };
            ensure!(
                !cancelled.load(Ordering::Acquire)
                    && !violated.load(Ordering::Acquire)
                    && !gate.participant_violation().load(Ordering::Acquire),
                "recovery_required: exclusive recovery session changed before writes"
            );
            let mut receipts = Vec::new();
            validate_live_context(&context.project, &roots, context_sha256, shared_project)?;
            let publication_only = pending
                .iter()
                .filter(|journal| journal.publication_only)
                .map(|journal| journal.transaction.clone())
                .collect();
            for journal in pending {
                ensure!(
                    journal.recovery_approved,
                    "recovery_required: pending journal has a different approved source context"
                );
                receipts.push(store.recover_guarded(
                    &journal.transaction,
                    &roots,
                    context_sha256,
                    &mut || {
                        ensure!(
                            !violated.load(Ordering::Acquire)
                                && !gate.participant_violation().load(Ordering::Acquire),
                            "recovery_required: exclusive recovery session changed"
                        );
                        validate_live_context(
                            &context.project,
                            &roots,
                            context_sha256,
                            shared_project,
                        )?;
                        writer.mark_writing();
                        Ok(())
                    },
                )?);
            }
            let publication = Box::new(Publication {
                store,
                roots: roots.clone(),
                context: context_sha256,
                transactions: receipts.iter().map(|receipt| receipt.transaction.clone()).collect(),
            });
            Ok(Finished::Recovered { receipts, roots, writer, publication, publication_only })
        }
        Work::Commit(params, SessionPlan::Rename { plan, context_documents }) => {
            ensure!(
                pending.is_empty(),
                "recovery_required: rebuild the preview to recover its pending journal"
            );
            let manifest = freeze_document_manifest(&context.roots, &documents, &params.documents)?;
            let current_context_documents =
                freeze_context_manifest(&context.inputs, &params.documents)?;
            ensure!(
                current_context_documents == context_documents
                    && !current_context_documents.values().any(|document| document.dirty),
                "metadata preview has dirty or changed context document versions"
            );
            let InputAdmission::Readable(snapshot) = snapshot else {
                anyhow::bail!("recovery_required: rebuild the recovery preview");
            };
            let mut locks = store.lock_roots(&context.roots)?;
            locks.defer_publication();
            let source = capture_metadata_snapshot(
                snapshot.analysis.database(),
                context.roots.clone(),
                context.context_sha256,
                &context.inputs,
                &manifest,
                cancelled,
            )?;
            let problems = validate_metadata_plan_snapshot(&plan, &source);
            ensure!(problems.is_empty(), "metadata preview is stale: {problems:?}");
            // No Salsa snapshot survives into filesystem writes or blocks editor change handling.
            drop(snapshot);
            let mut expected = Vec::new();
            for file in &source.files {
                if !matches!(
                    file.kind,
                    ide::metadata_rename_plan::MetadataPlanFileKind::Bsl
                        | ide::metadata_rename_plan::MetadataPlanFileKind::Xml
                ) {
                    continue;
                }
                let bytes = plan
                    .files
                    .iter()
                    .find(|planned| {
                        planned.root_id == file.root_id
                            && planned.relative_path == file.relative_path
                    })
                    .map_or(file.bytes.as_ref(), |planned| planned.after_bytes.as_slice());
                let text =
                    base_db::decode_disk_bytes(bytes).context("unsupported planned editor text")?;
                let hash = ide::metadata_rename_plan::metadata_document_fingerprint(&text);
                let original = base_db::decode_disk_bytes(&file.bytes)
                    .context("unsupported original editor text")?;
                let original_hash =
                    ide::metadata_rename_plan::metadata_document_fingerprint(&original);
                let mut after = file.relative_path.clone();
                for operation in &plan.resources {
                    if operation.root_id == file.root_id {
                        if let Ok(suffix) = after.strip_prefix(&operation.from_relative) {
                            after = operation.to_relative.join(suffix);
                        }
                    }
                }
                for known in
                    std::iter::once(hash).chain((original_hash != hash).then_some(original_hash))
                {
                    expected.push((file.root_id.clone(), file.relative_path.clone(), known));
                    expected.push((file.root_id.clone(), after.clone(), known));
                }
            }
            #[cfg(test)]
            if let Some(probe) = probe.as_ref().filter(|probe| probe.before_writer) {
                probe.paused.send(0)?;
                probe.resume.recv_timeout(Duration::from_secs(30))?;
            }
            let mut reload_filter = Some(mutation_filter(&context, expected));
            let mut writer = observation
                .context("metadata commit has no owned mutation observation")?
                .begin_write(Duration::from_secs(30), &|| cancelled.load(Ordering::Acquire))?;
            let shared_violation = gate.participant_violation();
            ensure!(
                !violated.load(Ordering::Acquire) && !shared_violation.load(Ordering::Acquire),
                "recovery_required: exclusive session changed before writes"
            );
            validate_live_context(
                &context.project,
                &plan.roots,
                plan.context_sha256,
                shared_project,
            )?;
            validate_resource_selection(&context.project, &plan)?;
            let mut before_write = || {
                ensure!(
                    !violated.load(Ordering::Acquire) && !shared_violation.load(Ordering::Acquire),
                    "recovery_required: exclusive session changed before writes"
                );
                validate_live_context(
                    &context.project,
                    &plan.roots,
                    plan.context_sha256,
                    shared_project,
                )?;
                validate_resource_selection(&context.project, &plan)?;
                if let Some(filter) = reload_filter.take() {
                    writer.observe_document_changes(filter);
                }
                writer.mark_writing();
                Ok(())
            };
            #[cfg(test)]
            #[cfg(unix)]
            let terminal_probe = probe.as_ref().filter(|probe| probe.host.is_some()).cloned();
            #[cfg(test)]
            let receipt = if let Some(probe) = probe {
                #[cfg(unix)]
                let mut host_paused = false;
                locks.apply_with_observers(
                    &plan,
                    &source,
                    cancelled,
                    &shared_violation,
                    &mut before_write,
                    &mut |applied| {
                        #[cfg(unix)]
                        if let Some(host) = &probe.host {
                            if host.reached(applied) && !host_paused {
                                host_paused = true;
                                host.phase.store(1, Ordering::Release);
                                probe.paused.send(applied)?;
                                probe.resume.recv_timeout(Duration::from_secs(240))?;
                                ensure!(
                                    !host.fault,
                                    "controlled ExtensionHost source-operation fault"
                                );
                            }
                            return Ok(());
                        }
                        if applied == 1 && !probe.before_writer {
                            probe.paused.send(applied)?;
                            probe.resume.recv_timeout(Duration::from_secs(30))?;
                        }
                        Ok(())
                    },
                )?
            } else {
                locks.apply_guarded(
                    &plan,
                    &source,
                    cancelled,
                    &shared_violation,
                    &mut before_write,
                )?
            };
            #[cfg(not(test))]
            let receipt = locks.apply_guarded(
                &plan,
                &source,
                cancelled,
                &shared_violation,
                &mut before_write,
            )?;
            #[cfg(test)]
            #[cfg(unix)]
            if let Some(probe) = terminal_probe {
                probe.host.as_ref().unwrap().phase.store(2, Ordering::Release);
                probe.paused.send(usize::MAX)?;
                probe.resume.recv_timeout(Duration::from_secs(240))?;
            }
            let publication = Box::new(Publication {
                store,
                roots: context.roots.clone(),
                context: context.context_sha256,
                transactions: vec![receipt.transaction.clone()],
            });
            Ok(Finished::Commit { receipt, roots: context.roots, writer, publication })
        }
    }
}

fn capture_selected_context(
    project: &project_model::Project,
    shared: bool,
) -> Result<super::context::MetadataProjectContext> {
    let selected = capture_project_context(project)?;
    let live_project = if shared {
        mcp_server::project::at(&project.root)?
    } else {
        project_model::Project::new(&project.root)?
    };
    let live = capture_project_context(&live_project)?;
    ensure!(
        selected.context_sha256 == live.context_sha256,
        "selected metadata project is stale; reload the workspace before preview or commit"
    );
    Ok(live)
}

fn validate_live_context(
    project: &project_model::Project,
    roots: &[MetadataPlanRoot],
    expected: [u8; 32],
    shared_project: bool,
) -> Result<()> {
    let live = capture_selected_context(project, shared_project)?;
    ensure!(
        live.roots == roots && live.context_sha256 == expected,
        "metadata project context changed before source writes"
    );
    Ok(())
}

fn mutation_filter(
    context: &super::context::MetadataProjectContext,
    expected: Vec<(String, std::path::PathBuf, [u8; 32])>,
) -> impl Fn(&Path, Option<[u8; 32]>) -> bool + Send + Sync + 'static {
    let roots = context.roots.clone();
    let inputs = context.inputs.clone();
    move |path, hash| {
        let source = file_uri(path).ok().and_then(|uri| document_path(&roots, &uri).ok().flatten());
        if source.is_none() && !inputs.iter().any(|input| matches_context_input(input, path)) {
            return false;
        }
        if let (Some(path), Some(hash)) = (source, hash) {
            if expected.iter().any(|(id, relative, expected)| {
                *expected == hash
                    && roots.iter().any(|root| {
                        if root.id != *id {
                            return false;
                        }
                        let expected = root.source_path.join(relative);
                        if root.case_sensitive {
                            expected == path
                        } else {
                            stdx::case::eq_ignore_case(
                                &expected.to_string_lossy(),
                                &path.to_string_lossy(),
                            )
                        }
                    })
            }) {
                return false;
            }
        }
        true
    }
}

fn validate_resource_selection(
    project: &project_model::Project,
    plan: &MetadataRenamePlan,
) -> Result<()> {
    for operation in &plan.resources {
        let root = plan
            .roots
            .iter()
            .find(|root| root.id == operation.root_id)
            .context("unknown resource source root")?;
        let destination = root.source_path.join(&operation.to_relative);
        ensure!(
            !project.is_source_excluded(&destination),
            "metadata resource rename would enter an excluded source path"
        );
        for input in plan.read_set.iter().filter(|input| input.root_id == operation.root_id) {
            let Ok(suffix) = input.relative_path.strip_prefix(&operation.from_relative) else {
                continue;
            };
            let path = if suffix.as_os_str().is_empty() {
                destination.clone()
            } else {
                destination.join(suffix)
            };
            ensure!(
                !project.is_source_excluded(&path),
                "metadata resource rename would change the selected source set"
            );
        }
    }
    Ok(())
}

fn preview_reply(
    plan: &MetadataRenamePlan,
    source: &PreparedMetadataRenameSnapshot,
    documents: Vec<super::documents::MetadataDocumentState>,
) -> Result<PreviewResult> {
    let mut files = Vec::new();
    let mut bytes = 0usize;
    for planned in &plan.files {
        let root = plan
            .roots
            .iter()
            .find(|root| root.id == planned.root_id)
            .context("missing metadata plan root")?;
        let before = source
            .files
            .iter()
            .find(|file| {
                file.root_id == planned.root_id && file.relative_path == planned.relative_path
            })
            .context("missing metadata preview input")?;
        let before_text = base_db::decode_disk_bytes(&before.bytes)
            .context("unsupported metadata preview encoding")?;
        let after_text = base_db::decode_disk_bytes(&planned.after_bytes)
            .context("unsupported metadata preview encoding")?;
        bytes = bytes.saturating_add(before_text.len()).saturating_add(after_text.len());
        ensure!(bytes <= MAX_PREVIEW_BYTES, "metadata preview exceeds response size limit");
        files.push(protocol::PreviewFile {
            uri: file_uri(&root.source_path.join(&planned.relative_path))?,
            before_text,
            after_text,
        });
    }
    let resources = plan
        .resources
        .iter()
        .map(|resource| {
            let root = plan
                .roots
                .iter()
                .find(|root| root.id == resource.root_id)
                .context("missing metadata resource root")?;
            Ok(protocol::PreviewResource {
                old_uri: file_uri(&root.source_path.join(&resource.from_relative))?,
                new_uri: file_uri(&root.source_path.join(&resource.to_relative))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let reply = PreviewResult {
        operation: "rename",
        roots: preview_roots(&plan.roots)?,
        context_inputs: Vec::new(),
        session_id: None,
        expires_in_seconds: SESSION_TTL.as_secs(),
        can_apply: plan.can_apply(),
        problems: plan.problems.iter().map(|p| format!("{p:?}")).collect(),
        files,
        resources,
        documents,
    };
    Ok(reply)
}

fn file_uri(path: &Path) -> Result<Url> {
    Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("metadata path cannot be represented as a file URI"))
}

fn preview_roots(roots: &[MetadataPlanRoot]) -> Result<Vec<protocol::PreviewRoot>> {
    roots
        .iter()
        .map(|root| {
            Ok(protocol::PreviewRoot {
                source_uri: file_uri(&root.source_path)?,
                canonical_uri: file_uri(&root.canonical_path)?,
                case_sensitive: root.case_sensitive,
            })
        })
        .collect()
}

fn preview_context_inputs(
    inputs: &[super::context::MetadataContextInput],
) -> Result<Vec<protocol::ContextInput>> {
    inputs
        .iter()
        .map(|input| {
            Ok(protocol::ContextInput {
                uri: file_uri(&input.path)?,
                canonical_uri: input.canonical_path.as_deref().map(file_uri).transpose()?,
                case_sensitive: input.case_sensitive,
            })
        })
        .collect()
}

fn is_text_source(path: &Path) -> bool {
    project_model::is_bsl_source_path(path)
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case(bsl_conventions::XML_EXTENSION))
}

pub(crate) fn observe_notification(state: &GlobalState, notification: &Notification) {
    let Some(active) = &state.metadata_rename.active else {
        return;
    };
    if notification.method == "$/cancelRequest" {
        if notification.params.get("id")
            == Some(&serde_json::to_value(&active.id).unwrap_or(Value::Null))
        {
            active.cancel();
        }
        return;
    }
    if notification.method == "exit" {
        active.cancel();
    }
}

pub(crate) fn observe_document_change(state: &GlobalState, method: &str, uri: &Url) {
    let Ok(path) = uri.to_file_path() else { return };
    let gate = state.workspace_disk_gate();
    let hash = if state.metadata_rename.supported && method == "textDocument/didChange" {
        state
            .mem_docs
            .get(uri)
            .map(|text| bsl_metadata::rename_xml::metadata_source_fingerprint(text.as_bytes()))
    } else {
        None
    };
    let reported = gate.report_document_change(&path, hash);
    let Some(active) = &state.metadata_rename.active else {
        return;
    };
    if !active.committing {
        return;
    }
    let relevant = active.context_inputs.iter().any(|input| matches_context_input(input, &path))
        || document_path(&active.roots, uri).ok().flatten().is_some();
    if relevant && (reported || !gate.is_frozen()) {
        active.violate();
        gate.participant_violation().store(true, Ordering::Release);
    }
}

pub(crate) fn finish(state: &mut GlobalState, completion: Completion) -> Result<()> {
    let Some(active) = state.metadata_rename.active.take() else {
        return Ok(());
    };
    ensure!(active.id == completion.id, "metadata rename completion identity mismatch");
    let response = match completion.result {
        Ok(Finished::Preview { session, mut reply })
            if !active.cancelled.load(Ordering::Acquire) =>
        {
            if let Some(session) = session {
                reply.session_id = Some(completion.session_id.clone());
                state.metadata_rename.sessions.insert(completion.session_id, session);
            }
            Response::new_ok(completion.id, reply)
        }
        Ok(Finished::Preview { .. }) => Response::new_err(
            completion.id,
            ErrorCode::RequestCanceled as i32,
            "metadata preview cancelled".into(),
        ),
        Ok(Finished::Commit { receipt, roots, writer, publication }) => {
            state.metadata_rename.sessions.clear();
            let paths = publish_receipts(state, &roots, std::slice::from_ref(&receipt))?;
            if receipt.status == Status::RecoveryRequired {
                writer.require_recovery();
            } else {
                state.metadata_rename.publication = Some(publication);
                if let Err(error) = publish_terminal(state, writer) {
                    state.respond(Response::new_err(
                        completion.id,
                        ErrorCode::RequestFailed as i32,
                        format!("recovery_required: {error}"),
                    ));
                    return Ok(());
                }
            }
            let mut reply = serde_json::to_value(&receipt)?;
            reply["changedUris"] = serde_json::to_value(paths)?;
            Response::new_ok(completion.id, reply)
        }
        Ok(Finished::Recovered { receipts, roots, writer, publication, publication_only }) => {
            state.metadata_rename.sessions.clear();
            let recovered = receipts.iter().all(|receipt| {
                receipt.status == Status::RolledBack
                    || (receipt.status == Status::Committed
                        && publication_only.contains(&receipt.transaction))
            });
            let paths = publish_receipts(state, &roots, &receipts)?;
            if recovered {
                state.metadata_rename.publication = Some(publication);
                if let Err(error) = publish_terminal(state, writer) {
                    state.respond(Response::new_err(
                        completion.id,
                        ErrorCode::RequestFailed as i32,
                        format!("recovery_required: {error}"),
                    ));
                    return Ok(());
                }
            } else {
                writer.require_recovery();
            }
            Response::new_ok(
                completion.id,
                json!({"status": if recovered { "recovered" } else {
                "recovery_required" }, "rebuildPreview": recovered, "receipts": receipts,
                "changedUris": paths }),
            )
        }
        Err(error) => Response::new_err(
            completion.id,
            if active.cancelled.load(Ordering::Acquire) {
                ErrorCode::RequestCanceled as i32
            } else {
                ErrorCode::RequestFailed as i32
            },
            error,
        ),
    };
    if state.metadata_rename.buffers_deferred && !state.workspace_disk_gate().is_frozen() {
        let _read = state.prepare_disk_read()?;
        crate::handlers::notification::replay_open_documents_after_disk_transaction(state)?;
    }
    if state.metadata_rename.writer.is_some() {
        state.metadata_rename.pending_response = Some(response);
    } else {
        state.respond(response);
    }
    if state.metadata_rename.shutdown_pending {
        state.shutdown_requested = true;
    }
    Ok(())
}

pub(crate) fn finish_publication(
    state: &mut GlobalState,
    writer: WorkspaceDiskWrite,
) -> Result<()> {
    let publication =
        state.metadata_rename.publication.take().context("metadata publication binding missing")?;
    writer
        .finish_with_publication(|| publication.complete().map_err(|error| error.to_string()))
        .map_err(anyhow::Error::msg)
}

fn publish_terminal(state: &mut GlobalState, writer: WorkspaceDiskWrite) -> Result<()> {
    ensure!(
        !state.workspace_disk_gate().participant_violation().load(Ordering::Acquire),
        "exclusive session changed before terminal publication"
    );
    if let Some(owner) = state.analysis_host.owner().cloned() {
        state.metadata_rename.needs_rescan = false;
        let snapshot =
            owner.publish_disk_transaction(&writer).map_err(|error| anyhow::anyhow!(error))?;
        if let Some(view) = state.analysis_host.view_mut() {
            view.catch_up(snapshot, &[]);
        } else {
            state.analysis_host.ensure_view(snapshot);
        }
        state.shared_view_seeded = true;
        state.mark_shared_view_changed();
        crate::handlers::notification::replay_open_documents_after_disk_transaction(state)?;
        ensure!(
            !state.workspace_disk_gate().participant_violation().load(Ordering::Acquire),
            "exclusive session changed during terminal publication"
        );
        finish_publication(state, writer)?;
        owner.notify_disk_transaction_finished();
        crate::handlers::notification::refresh_after_shared_catch_up(state);
    } else {
        state.metadata_rename.needs_rescan = true;
        state.metadata_rename.writer = Some(writer);
    }
    Ok(())
}

fn publish_receipts(
    state: &mut GlobalState,
    roots: &[MetadataPlanRoot],
    receipts: &[Receipt],
) -> Result<Vec<Url>> {
    let mut paths = Vec::new();
    let mut vfs = state.vfs.write();
    let shared = state.analysis_host.is_shared();
    for receipt in receipts.iter().filter(|receipt| receipt.status != Status::RecoveryRequired) {
        for known in &receipt.known_files {
            let root = roots
                .iter()
                .find(|root| root.id == known.root_id)
                .context("journal receipt has an unknown metadata source root")?;
            let before = root.source_path.join(&known.old_path);
            let after = root.source_path.join(&known.current_path);
            if before != after {
                if !shared {
                    vfs.set_file_contents(vfs::VfsPath::new(&before), None);
                }
                paths.push(before);
            }
            // Editor overlays retain their LSP version until the client flushes its reloads.
            let source = is_text_source(&after);
            if !shared && source && !state.mem_docs.contains(&file_uri(&after)?) {
                vfs.set_file_contents(
                    vfs::VfsPath::new(&after),
                    base_db::decode_disk_bytes(&known.bytes).map(Arc::from),
                );
            }
            paths.push(after);
        }
    }
    drop(vfs);
    if !shared && !paths.is_empty() {
        state.process_changes(false);
        state.refresh_metadata_substrate(&paths);
    }
    paths.iter().map(|path| file_uri(path)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        state: GlobalState,
        receiver: crossbeam_channel::Receiver<lsp_server::Message>,
        source: tempfile::TempDir,
        _cache: tempfile::TempDir,
        descriptor: std::path::PathBuf,
        module: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_root("cf")
        }

        fn with_root(root: &str) -> Self {
            let source = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let configuration_root = source.path().join(root);
            let catalog = configuration_root.join("Catalogs/Partners");
            std::fs::create_dir_all(catalog.join("Ext")).unwrap();
            std::fs::write(
                source.path().join("bsl-analyzer.toml"),
                format!("[source]\nroot = '{root}'\nextensions = []\nexternals = []\n"),
            )
            .unwrap();
            std::fs::write(configuration_root.join("Configuration.xml"),
                "<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><Configuration uuid='00000000-0000-0000-0000-000000000010'><Properties><Name>Main</Name></Properties><ChildObjects><Catalog>Partners</Catalog></ChildObjects></Configuration></MetaDataObject>").unwrap();
            let descriptor = configuration_root.join("Catalogs/Partners.xml");
            std::fs::write(&descriptor, "\u{feff}<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses' xmlns:v8='http://v8.1c.ru/8.1/data/core' xmlns:xs='http://www.w3.org/2001/XMLSchema'><Catalog uuid='00000000-0000-0000-0000-000000000001'><Properties><Name>Partners</Name></Properties>\r\n<ChildObjects><Attribute uuid='00000000-0000-0000-0000-000000000002'>\r\n<Properties><Name>Caption</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute></ChildObjects></Catalog></MetaDataObject>").unwrap();
            let module = catalog.join("Ext/ObjectModule.bsl");
            std::fs::write(&module, "\u{feff}Процедура Тест()\r\n P = Справочники.Partners.ПустаяСсылка();\r\n X = P.Caption;\r\nКонецПроцедуры\r\n").unwrap();
            let (sender, receiver) = crossbeam_channel::unbounded();
            let mut state = GlobalState::new(sender);
            state.init_empty_source_root();
            state.metadata_rename.supported = true;
            state.metadata_rename.cache_authority = Some(cache.path().to_path_buf());
            state.set_workspace_root(source.path().to_path_buf()).unwrap();
            while !state.vfs_done {
                let message = state.loader_receiver.recv_timeout(Duration::from_secs(30)).unwrap();
                crate::server::handle_loader_msg(&mut state, message).unwrap();
            }
            Self { state, receiver, source, _cache: cache, descriptor, module }
        }

        fn request(&mut self, id: i32, method: &str, params: Value) -> Response {
            crate::server::handle_request(
                &mut self.state,
                Request::new(id.into(), method.into(), params),
            )
            .unwrap();
            if self.state.metadata_rename.active.is_some() {
                let completion = self
                    .state
                    .metadata_rename
                    .completion_receiver
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
                crate::server::finish_metadata_rename(&mut self.state, completion).unwrap();
            }
            while self.state.metadata_rename.writer.is_some() {
                let message =
                    self.state.loader_receiver.recv_timeout(Duration::from_secs(30)).unwrap();
                crate::server::handle_loader_msg(&mut self.state, message).unwrap();
            }
            self.receiver
                .try_iter()
                .find_map(|message| match message {
                    lsp_server::Message::Response(response)
                        if response.id == RequestId::from(id) =>
                    {
                        Some(response)
                    }
                    _ => None,
                })
                .expect("metadata request must respond once")
        }

        fn preview(&mut self, documents: Value) -> Value {
            self.preview_name("Caption", "Heading", documents)
        }

        fn preview_name(&mut self, name: &str, new_name: &str, documents: Value) -> Value {
            let text = std::fs::read_to_string(&self.descriptor).unwrap();
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            let prefix = &text[..text.find(&format!(">{name}<")).unwrap() + 1];
            let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
            let character = prefix.rsplit('\n').next().unwrap().encode_utf16().count();
            let response = self.request(1, protocol::PREVIEW, json!({
                "textDocument":{"uri":file_uri(&self.descriptor).unwrap()},
                    "position":{"line":line,"character":character}, "newName":new_name, "documents":documents,
            }));
            assert!(response.error.is_none(), "{:?}", response.error);
            response.result.unwrap()
        }

        fn xml_document(&self, version: i32, dirty: bool) -> Value {
            let text = std::fs::read_to_string(&self.descriptor).unwrap();
            let hash: String = ide::metadata_rename_plan::metadata_document_fingerprint(&text)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            json!({"uri":file_uri(&self.descriptor).unwrap(),"version":version,"textSha256":hash,"dirty":dirty})
        }
    }

    #[test]
    fn editor_protocol_previews_then_applies_bound_xml_and_bsl_once() {
        let mut fixture = Fixture::new();
        let preview = fixture.preview(json!([]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        assert_eq!(preview["operation"], "rename");
        assert_eq!(preview["roots"].as_array().unwrap().len(), 1);
        assert_eq!(preview["files"].as_array().unwrap().len(), 2);
        let params =
            json!({"sessionId":preview["sessionId"],"exclusiveSession":true,"documents":[]});
        let response = fixture.request(2, protocol::COMMIT, params.clone());
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.result.unwrap()["status"], "committed");
        assert!(std::fs::read_to_string(&fixture.descriptor)
            .unwrap()
            .contains("<Name>Heading</Name>"));
        assert!(std::fs::read_to_string(&fixture.module).unwrap().contains("P.Heading"));
        assert!(!fixture.state.workspace_disk_gate().is_frozen());
        let repeated = fixture.request(3, protocol::COMMIT, params);
        assert!(repeated.error.is_some());
    }

    #[test]
    fn clean_open_bom_bsl_and_later_line_xml_use_editor_hashes_and_keep_raw_bytes() {
        let mut fixture = Fixture::new();
        let uri = file_uri(&fixture.module).unwrap();
        let raw = std::fs::read_to_string(&fixture.module).unwrap();
        let editor_text = raw.strip_prefix('\u{feff}').unwrap().to_owned();
        crate::handlers::notification::handle_did_open(
            &mut fixture.state,
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".into(),
                    version: 7,
                    text: editor_text.clone(),
                },
            },
        )
        .unwrap();
        let hash: String =
            bsl_metadata::rename_xml::metadata_source_fingerprint(editor_text.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
        let bsl = json!({"uri":uri,"version":7,"textSha256":hash,"dirty":false});
        let xml = fixture.xml_document(7, false);
        let documents = json!([bsl, xml]);
        let preview = fixture.preview(documents.clone());
        assert_eq!(preview["canApply"], true, "{preview:#}");
        let response = fixture.request(
            2,
            protocol::COMMIT,
            json!({"sessionId":preview["sessionId"],
            "exclusiveSession":true,"documents":documents}),
        );
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.result.unwrap()["status"], "committed");
        for path in [&fixture.module, &fixture.descriptor] {
            let bytes = std::fs::read(path).unwrap();
            assert!(bytes.starts_with(b"\xef\xbb\xbf"));
            assert!(std::str::from_utf8(&bytes).unwrap().contains("Heading"));
            assert!(bytes.windows(2).any(|window| window == b"\r\n"));
        }
    }

    #[test]
    fn first_line_bom_xml_object_rename_applies_descriptor_container_and_all_references() {
        let mut fixture = Fixture::new();
        let document = fixture.xml_document(7, false);
        let preview = fixture.preview_name("Partners", "Buyers", json!([document.clone()]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        assert_eq!(preview["resources"].as_array().unwrap().len(), 2);
        let response = fixture.request(
            2,
            protocol::COMMIT,
            json!({"sessionId":preview["sessionId"],
            "exclusiveSession":true,"documents":[document]}),
        );
        assert!(response.error.is_none(), "{:?}", response.error);
        let result = response.result.unwrap();
        assert_eq!(
            result["status"], "committed",
            "status={}, reason={}",
            result["status"], result["reason"]
        );
        assert!(!fixture.descriptor.exists());
        assert!(!fixture.module.exists());
        let renamed_descriptor = fixture.descriptor.with_file_name("Buyers.xml");
        let renamed_module = fixture.source.path().join("cf/Catalogs/Buyers/Ext/ObjectModule.bsl");
        for path in [&renamed_descriptor, &renamed_module] {
            let bytes = std::fs::read(path).unwrap();
            assert!(bytes.starts_with(b"\xef\xbb\xbf"));
            assert!(!std::str::from_utf8(&bytes).unwrap().contains("Partners"));
        }
        assert!(std::fs::read_to_string(fixture.source.path().join("cf/Configuration.xml"))
            .unwrap()
            .contains("<Catalog>Buyers</Catalog>"));
    }

    #[test]
    fn project_root_configuration_and_environment_are_readonly_inputs_of_a_complete_plan() {
        let mut fixture = Fixture::with_root(".");
        let environment = fixture.source.path().join(project_model::PROJECT_ENV_FILE_NAME);
        let environment_text = "CONTEXT_TEST_VALUE=synthetic-root-context\n";
        std::fs::write(&environment, environment_text).unwrap();
        let configuration = fixture.source.path().join("bsl-analyzer.toml");
        let before = std::fs::read(&configuration).unwrap();
        let preview = fixture.preview(json!([]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        assert!(!serde_json::to_string(&preview).unwrap().contains("synthetic-root-context"));
        let response = fixture.request(
            2,
            protocol::COMMIT,
            json!({"sessionId":preview["sessionId"],
            "exclusiveSession":true,"documents":[]}),
        );
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.result.unwrap()["status"], "committed");
        assert_eq!(std::fs::read(configuration).unwrap(), before);
        assert_eq!(std::fs::read_to_string(environment).unwrap(), environment_text);
        assert!(std::fs::read_to_string(&fixture.module).unwrap().contains("P.Heading"));
    }

    #[test]
    fn object_rename_cannot_move_sources_into_an_excluded_destination() {
        let mut fixture = Fixture::new();
        std::fs::write(
            fixture.source.path().join("bsl-analyzer.toml"),
            "[source]\nroot='cf'\nextensions=[]\nexternals=[]\nexclude=['cf/Catalogs/Buyers']\n",
        )
        .unwrap();
        let mut config =
            project_model::ProjectConfig::load(fixture.source.path()).unwrap().unwrap();
        config.source_exclude = vec!["cf/Catalogs/Buyers".into()];
        let project = project_model::Project::with_config(fixture.source.path(), config).unwrap();
        assert!(project.is_source_excluded(&fixture.source.path().join("cf/Catalogs/Buyers")));
        fixture.state.project = Some(project);
        let preview = fixture.preview_name("Partners", "Buyers", json!([]));
        assert_eq!(preview["canApply"], false, "{preview:#}");
        assert!(preview["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem.as_str().unwrap().contains("excluded")));
        assert!(preview["sessionId"].is_null());
        assert!(fixture.descriptor.exists());
        assert!(fixture.module.exists());
    }

    #[test]
    fn editor_protocol_refuses_semantic_member_collision_before_session_or_source_writes() {
        let mut fixture = Fixture::new();
        let original = std::fs::read_to_string(&fixture.descriptor).unwrap();
        let sibling = "<Attribute uuid='00000000-0000-0000-0000-000000000034'><Properties><Name>Heading</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>";
        std::fs::write(
            &fixture.descriptor,
            original.replacen("</ChildObjects>", &format!("{sibling}</ChildObjects>"), 1),
        )
        .unwrap();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.init_empty_source_root();
        state.metadata_rename.supported = true;
        state.metadata_rename.cache_authority = Some(fixture._cache.path().to_path_buf());
        state.set_workspace_root(fixture.source.path().to_path_buf()).unwrap();
        while !state.vfs_done {
            let message = state.loader_receiver.recv_timeout(Duration::from_secs(30)).unwrap();
            crate::server::handle_loader_msg(&mut state, message).unwrap();
        }
        fixture.state = state;
        fixture.receiver = receiver;
        let before_xml = std::fs::read(&fixture.descriptor).unwrap();
        let before_bsl = std::fs::read(&fixture.module).unwrap();
        let preview = fixture.preview(json!([]));
        assert_eq!(preview["canApply"], false);
        assert!(preview["sessionId"].is_null());
        assert!(
            preview["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|problem| problem.as_str().unwrap().contains("NameCollision")),
            "{preview:#}"
        );
        let response = fixture.request(
            2,
            protocol::COMMIT,
            json!({"sessionId":"no-issued-preview","exclusiveSession":true,"documents":[]}),
        );
        assert!(response.error.is_some());
        assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), before_xml);
        assert_eq!(std::fs::read(&fixture.module).unwrap(), before_bsl);
    }

    #[test]
    fn editor_protocol_refuses_a_known_external_consumer_missing_from_the_resident_index() {
        let mut fixture = Fixture::new();
        let external = fixture.source.path().join("external");
        let consumer = external.join("Consumer/Ext/ObjectModule.bsl");
        std::fs::create_dir_all(consumer.parent().unwrap()).unwrap();
        let external_descriptor = external.join("Consumer.xml");
        std::fs::write(&external_descriptor, "<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><ExternalDataProcessor uuid='00000000-0000-0000-0000-000000000060'><Properties><Name>Consumer</Name></Properties></ExternalDataProcessor></MetaDataObject>").unwrap();
        std::fs::write(&consumer, "Процедура Потребитель()\n P = Справочники.Partners.ПустаяСсылка();\n X = P.Caption;\nКонецПроцедуры\n").unwrap();
        std::fs::write(fixture.source.path().join("bsl-analyzer.toml"), "[source]\nroot='cf'\nextensions=[]\nexternals=[{name='Consumer',path='external',configuration_root='cf'}]\n").unwrap();
        let project = project_model::Project::new(fixture.source.path()).unwrap();
        assert_eq!(project.external_paths().len(), 1);
        assert_eq!(project.external_configuration_owners().len(), 1);
        assert!(project.external_configuration_owners()[0].1.proven_root().is_some());
        let configurations = ide_db::metadata::WorkspaceConfigsSnapshot::from_project(&project);
        assert!(configurations.is_external(configurations.owner_of_path(&consumer).unwrap()));
        fixture
            .state
            .analysis_host
            .raw_database_mut()
            .set_workspace_configs_snapshot(configurations);
        fixture.state.project = Some(project);
        assert!(fixture
            .state
            .snapshot()
            .unwrap()
            .file_id_for_url(&file_uri(&consumer).unwrap())
            .is_err());
        let before = [&fixture.descriptor, &fixture.module, &external_descriptor, &consumer]
            .into_iter()
            .map(|path| (path.clone(), std::fs::read(path).unwrap()))
            .collect::<Vec<_>>();
        let preview = fixture.preview(json!([]));
        assert_eq!(preview["canApply"], false, "{preview:#}");
        assert!(preview["sessionId"].is_null());
        let problems = preview["problems"].as_array().unwrap();
        assert!(
            problems.iter().any(|reason| reason.as_str().unwrap().contains("UnindexedSource")),
            "{preview:#}"
        );
        assert!(
            problems
                .iter()
                .all(|reason| !reason.as_str().unwrap().contains("UnprovedExternalOwner")),
            "the owner is proven; the missing consumer index is the refusal reason: {preview:#}"
        );
        let response = fixture.request(
            2,
            protocol::COMMIT,
            json!({"sessionId":"no-issued-preview","exclusiveSession":true,"documents":[]}),
        );
        assert!(response.error.is_some());
        for (path, bytes) in before {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }
    #[test]
    fn editor_protocol_refuses_changed_xml_version_and_new_namespace_before_writes() {
        let mut fixture = Fixture::new();
        let original_xml = std::fs::read(&fixture.descriptor).unwrap();
        let original_bsl = std::fs::read(&fixture.module).unwrap();
        let document = fixture.xml_document(7, false);
        let preview = fixture.preview(json!([document]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        let changed_document = fixture.xml_document(8, false);
        let response = fixture.request(2, protocol::COMMIT, json!({
            "sessionId":preview["sessionId"],"exclusiveSession":true,"documents":[changed_document],
        }));
        assert!(response.error.is_some());
        assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), original_xml);
        assert_eq!(std::fs::read(&fixture.module).unwrap(), original_bsl);

        let preview = fixture.preview(json!([]));
        std::fs::write(fixture.source.path().join("cf/New.xml"), "<Unknown/>").unwrap();
        let response = fixture.request(
            3,
            protocol::COMMIT,
            json!({
                "sessionId":preview["sessionId"],"exclusiveSession":true,"documents":[],
            }),
        );
        assert!(response.error.is_some());
        assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), original_xml);
        assert_eq!(std::fs::read(&fixture.module).unwrap(), original_bsl);
    }

    #[test]
    fn editor_protocol_refuses_dirty_xml_and_changed_closed_readonly_input() {
        let mut fixture = Fixture::new();
        let original_xml = std::fs::read(&fixture.descriptor).unwrap();
        let original_bsl = std::fs::read(&fixture.module).unwrap();
        let document = fixture.xml_document(7, false);
        let preview = fixture.preview(json!([document]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        let document = fixture.xml_document(7, true);
        let response = fixture.request(
            2,
            protocol::COMMIT,
            json!({
                "sessionId":preview["sessionId"],"exclusiveSession":true,"documents":[document],
            }),
        );
        assert!(response.error.is_some());
        let preview = fixture.preview(json!([]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        let configuration = fixture.source.path().join("cf/Configuration.xml");
        let foreign = std::fs::read_to_string(&configuration)
            .unwrap()
            .replace("<Name>Main</Name>", "<Name>Foreign</Name>");
        std::fs::write(&configuration, &foreign).unwrap();
        let response = fixture.request(
            3,
            protocol::COMMIT,
            json!({
                "sessionId":preview["sessionId"],"exclusiveSession":true,"documents":[],
            }),
        );
        assert!(response.error.is_some());
        assert_eq!(std::fs::read_to_string(configuration).unwrap(), foreign);
        assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), original_xml);
        assert_eq!(std::fs::read(&fixture.module).unwrap(), original_bsl);
    }

    #[test]
    fn context_document_outside_source_roots_is_bound_without_exposing_environment_text() {
        let mut fixture = Fixture::new();
        let environment = fixture.source.path().join(project_model::PROJECT_ENV_FILE_NAME);
        let text = "CONTEXT_TEST_VALUE=synthetic-context-value\n";
        std::fs::write(&environment, text).unwrap();
        let hash: String = bsl_metadata::rename_xml::metadata_source_fingerprint(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let document = json!({"uri":file_uri(&environment).unwrap(),"version":1,
            "textSha256":hash,"dirty":false});
        let preview = fixture.preview(json!([document.clone()]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        assert!(preview["contextInputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|input| input["uri"] == document["uri"]));
        assert!(!serde_json::to_string(&preview).unwrap().contains("synthetic-context-value"));
        let original_bsl = std::fs::read(&fixture.module).unwrap();
        let mut dirty = document.clone();
        dirty["dirty"] = json!(true);
        let response = fixture.request(
            2,
            protocol::COMMIT,
            json!({"sessionId":preview["sessionId"],
            "exclusiveSession":true,"documents":[dirty]}),
        );
        assert!(response.error.is_some());
        let preview = fixture.preview(json!([document.clone()]));
        let mut changed = document;
        changed["version"] = json!(2);
        let response = fixture.request(
            3,
            protocol::COMMIT,
            json!({"sessionId":preview["sessionId"],
            "exclusiveSession":true,"documents":[changed]}),
        );
        assert!(response.error.is_some());
        assert_eq!(std::fs::read(&fixture.module).unwrap(), original_bsl);
        assert_eq!(std::fs::read_to_string(environment).unwrap(), text);
    }

    fn active(id: i32, committing: bool) -> Active {
        Active {
            id: id.into(),
            session_id: "session".into(),
            committing,
            cancelled: Arc::new(AtomicBool::new(false)),
            violated: Arc::new(AtomicBool::new(false)),
            roots: Vec::new(),
            read_cancel: None,
            context_inputs: Vec::new(),
        }
    }

    fn recovery_session(created: Instant) -> Box<Session> {
        Box::new(Session {
            plan: SessionPlan::Recover {
                roots: Vec::new(),
                context_sha256: [0; 32],
                documents: BTreeMap::new(),
            },
            created,
        })
    }

    #[test]
    fn unsupported_clients_cannot_create_sessions_or_start_a_worker() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        for (id, method) in
            [protocol::PREVIEW, protocol::COMMIT, protocol::ABORT].iter().enumerate()
        {
            handle_request(
                &mut state,
                Request::new((id as i32).into(), method.to_string(), json!({})),
            )
            .unwrap();
            let lsp_server::Message::Response(response) = receiver.recv().unwrap() else {
                panic!("expected protocol refusal")
            };
            assert_eq!(response.error.unwrap().code, ErrorCode::InvalidRequest as i32);
        }
        assert!(state.metadata_rename.active.is_none());
        assert!(state.metadata_rename.sessions.is_empty());
        assert!(state.request_lifecycles.is_empty());
    }

    #[test]
    fn expired_and_aborted_sessions_are_removed_without_apply() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.metadata_rename.supported = true;
        state
            .metadata_rename
            .sessions
            .insert("expired".into(), recovery_session(Instant::now() - SESSION_TTL));
        state.metadata_rename.sessions.insert("live".into(), recovery_session(Instant::now()));
        handle_request(
            &mut state,
            Request::new(1.into(), protocol::ABORT.into(), json!({"sessionId":"live"})),
        )
        .unwrap();
        let lsp_server::Message::Response(response) = receiver.recv().unwrap() else {
            panic!("expected abort receipt")
        };
        assert_eq!(response.result.unwrap()["aborted"], true);
        assert!(state.metadata_rename.sessions.is_empty());
        assert!(state.metadata_rename.active.is_none());
    }

    #[test]
    fn cancel_and_shutdown_reach_the_running_worker_without_retry() {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.metadata_rename.active = Some(active(7, true));
        observe_notification(&state, &Notification::new("$/cancelRequest".into(), json!({"id":8})));
        assert!(!state.metadata_rename.active.as_ref().unwrap().cancelled.load(Ordering::Acquire));
        observe_notification(&state, &Notification::new("$/cancelRequest".into(), json!({"id":7})));
        assert!(state.metadata_rename.active.as_ref().unwrap().cancelled.load(Ordering::Acquire));
        assert!(request_shutdown(&mut state));
        assert!(!state.shutdown_requested);
        assert!(state.metadata_rename.shutdown_pending);
        assert!(state.request_lifecycles.is_empty());
    }

    #[test]
    fn cancelled_preview_never_publishes_a_committable_session() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        let active = active(1, false);
        active.cancelled.store(true, Ordering::Release);
        state.metadata_rename.active = Some(active);
        finish(
            &mut state,
            Completion {
                id: 1.into(),
                session_id: "session".into(),
                result: Ok(Finished::Preview {
                    session: Some(recovery_session(Instant::now())),
                    reply: PreviewResult {
                        operation: "recover",
                        roots: Vec::new(),
                        context_inputs: Vec::new(),
                        session_id: None,
                        expires_in_seconds: 300,
                        can_apply: true,
                        problems: Vec::new(),
                        files: Vec::new(),
                        resources: Vec::new(),
                        documents: Vec::new(),
                    },
                }),
            },
        )
        .unwrap();
        let lsp_server::Message::Response(response) = receiver.recv().unwrap() else {
            panic!("expected cancellation receipt")
        };
        assert_eq!(response.error.unwrap().code, ErrorCode::RequestCanceled as i32);
        assert!(state.metadata_rename.sessions.is_empty());
    }

    #[test]
    fn delayed_watcher_payload_is_reread_after_terminal_state() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("Module.bsl");
        std::fs::write(&path, "final").unwrap();
        let path = paths::AbsPathBuf::try_from(path.to_str().unwrap()).unwrap();
        let vfs::loader::Message::Changed { files } =
            refresh_loader(vfs::loader::Message::Changed {
                files: vec![(path, Some(b"intermediate".to_vec()))],
            })
        else {
            panic!("expected changed file")
        };
        assert_eq!(files[0].1.as_deref(), Some(b"final".as_slice()));
    }

    #[test]
    fn xml_violation_signal_is_distinct_from_ordinary_cancellation() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.metadata_rename.supported = true;
        let mut running = active(1, true);
        let token = salsa::CancellationToken::default();
        running.read_cancel = Some(token.clone());
        state.metadata_rename.active = Some(running);
        handle_request(
            &mut state,
            Request::new(
                2.into(),
                protocol::ABORT.into(),
                json!({"sessionId":"session", "violation":true}),
            ),
        )
        .unwrap();
        let active = state.metadata_rename.active.as_ref().unwrap();
        assert!(active.violated.load(Ordering::Acquire));
        assert!(!active.cancelled.load(Ordering::Acquire));
        assert!(token.is_cancelled());
        assert!(matches!(receiver.recv().unwrap(), lsp_server::Message::Response(_)));
    }

    #[derive(Clone, Copy)]
    enum SharedScenario {
        Commit,
        Cancel,
        CancelCleanBeforeReload,
        CancelDirtyExactBefore,
        OtherBslDirty,
        OtherBslCleanReload,
        OtherBslDirtyExactAfter,
        OtherXmlDirty,
    }

    async fn shared_readers_during_real_journal(scenario: SharedScenario) {
        let rollback_reload = matches!(
            scenario,
            SharedScenario::CancelCleanBeforeReload | SharedScenario::CancelDirtyExactBefore
        );
        let cancel = matches!(scenario, SharedScenario::Cancel) || rollback_reload;
        let conflict = matches!(
            scenario,
            SharedScenario::OtherBslDirty
                | SharedScenario::OtherBslDirtyExactAfter
                | SharedScenario::OtherXmlDirty
                | SharedScenario::CancelDirtyExactBefore
        );
        use mcp_server::WorkspaceRead;
        use rmcp::ServiceExt;

        let mut fixture = Fixture::new();
        let original_xml = std::fs::read(&fixture.descriptor).unwrap();
        let original_bsl = std::fs::read(&fixture.module).unwrap();
        let project = fixture.state.project.as_ref().unwrap().clone();
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(&fixture._cache.path().join("mcp")),
            fixture.source.path(),
            None,
        )
        .unwrap();
        let backend = mcp_server::SharedState::shared_workspace_with_cache_and_prefixes(
            fixture.source.path().to_path_buf(),
            cache,
            None,
        )
        .unwrap();
        let owner = backend.workspace_handle();
        let wakes = owner.subscribe();
        let initial = loop {
            match owner.snapshot() {
                WorkspaceRead::Ready(snapshot) => break snapshot,
                WorkspaceRead::Loading => {
                    wakes.recv_timeout(Duration::from_secs(30)).unwrap();
                }
                other => panic!(
                    "owner did not become ready: {}",
                    match other {
                        WorkspaceRead::Failed(message) | WorkspaceRead::RejectedScope(message) =>
                            message,
                        _ => "unexpected workspace status".into(),
                    }
                ),
            }
        };
        let make_view = |sender| {
            let mut state = GlobalState::new_shared(sender, owner.clone());
            state.init_empty_source_root();
            state.adopt_shared_project(fixture.source.path().to_path_buf(), project.clone());
            state.analysis_host.ensure_view(initial.clone());
            state.shared_view_seeded = true;
            state.vfs_done = true;
            state.metadata_rename.supported = true;
            state.metadata_rename.cache_authority = Some(fixture._cache.path().to_path_buf());
            state
        };
        let (sender, receiver) = crossbeam_channel::unbounded();
        let shared = make_view(sender);
        let (other_sender, other_receiver) = crossbeam_channel::unbounded();
        let mut other_view = make_view(other_sender);
        fixture.state = shared;
        fixture.receiver = receiver;
        let bsl_uri = file_uri(&fixture.module).unwrap();
        let original_editor = String::from_utf8(original_bsl.clone())
            .unwrap()
            .trim_start_matches('\u{feff}')
            .to_owned();
        let bsl_open = matches!(
            scenario,
            SharedScenario::OtherBslDirty
                | SharedScenario::OtherBslCleanReload
                | SharedScenario::OtherBslDirtyExactAfter
                | SharedScenario::CancelCleanBeforeReload
                | SharedScenario::CancelDirtyExactBefore
        );
        if bsl_open {
            crate::server::handle_notification(&mut other_view, Notification::new("textDocument/didOpen".into(), json!({"textDocument":{"uri":bsl_uri,"languageId":"bsl","version":7,"text":original_editor}}))).unwrap();
        }
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = mcp_server::McpServer::new(mcp_server::McpProfile::Workspace, backend.clone());
        let server_task = tokio::spawn(mcp_server::serve_stream(server, server_io));
        let client = tokio::time::timeout(Duration::from_secs(10), ().serve(client_io))
            .await
            .unwrap()
            .unwrap();
        let preview = fixture.preview(json!([]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        let (paused, pause_receiver) = crossbeam_channel::bounded(1);
        let (resume, resume_receiver) = crossbeam_channel::bounded(1);
        fixture.state.metadata_rename.probe = Some(Arc::new(TransactionProbe {
            before_writer: false,
            paused,
            resume: resume_receiver,
            #[cfg(unix)]
            host: None,
        }));
        crate::server::handle_request(
            &mut fixture.state,
            Request::new(
                2.into(),
                protocol::COMMIT.into(),
                json!({"sessionId":preview["sessionId"],"exclusiveSession":true,"documents":[]}),
            ),
        )
        .unwrap();
        assert_eq!(pause_receiver.recv_timeout(Duration::from_secs(30)).unwrap(), 1);
        let changed = usize::from(std::fs::read(&fixture.module).unwrap() != original_bsl)
            + usize::from(std::fs::read(&fixture.descriptor).unwrap() != original_xml);
        assert_eq!(changed, 1, "the pause must follow one installed source replacement");
        assert!(fixture.state.workspace_disk_gate().is_frozen());
        assert!(other_view.snapshot().is_err(), "a frozen owner cannot return a fallback snapshot");
        crate::server::handle_request(
            &mut other_view,
            Request::new(81.into(), "workspace/symbol".into(), json!({"query":"Тест"})),
        )
        .unwrap();
        let rejected = other_receiver
            .try_iter()
            .find_map(|message| match message {
                lsp_server::Message::Response(response) if response.id == RequestId::from(81) => {
                    Some(response)
                }
                _ => None,
            })
            .unwrap();
        assert!(rejected.error.unwrap().message.contains("in progress"));
        assert!(matches!(owner.request_analysis(), WorkspaceRead::Busy));
        let _ = owner.notify_changed_path(&fixture.module);
        let WorkspaceRead::Ready(paused_snapshot) = owner.snapshot() else {
            panic!("revision-only snapshots remain available under the writer")
        };
        assert_eq!(paused_snapshot.generation, initial.generation);
        for (tool, arguments) in [
            ("outline", json!({"path":fixture.module})),
            ("search", json!({"action":"search_code","query":"Heading"})),
            ("diagnostics", json!({"action":"file","path":fixture.module})),
        ] {
            let answer = tokio::time::timeout(
                Duration::from_secs(5),
                client.call_tool(
                    rmcp::model::CallToolRequestParams::new(tool)
                        .with_arguments(arguments.as_object().unwrap().clone()),
                ),
            )
            .await
            .expect("a participating read must refuse rather than wait for the journal");
            let pending = answer
                .as_ref()
                .ok()
                .and_then(|answer| answer.structured_content.as_ref())
                .is_some_and(|content| content.get("status") == Some(&json!("loading")));
            let rendered = match answer {
                Ok(answer) => serde_json::to_string(&answer).unwrap(),
                Err(error) => error.to_string(),
            };
            assert!(
                if tool == "diagnostics" {
                    pending
                } else {
                    rendered.to_ascii_lowercase().contains("transaction")
                },
                "{tool}: {rendered}"
            );
        }
        let expected_after = preview["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["uri"] == json!(bsl_uri))
            .unwrap()["afterText"]
            .as_str()
            .unwrap()
            .trim_start_matches('\u{feff}')
            .to_owned();
        let mut foreign_text = if matches!(scenario, SharedScenario::OtherBslDirty) {
            format!("{original_editor}\n// foreign user edit")
        } else {
            expected_after.clone()
        };
        if bsl_open {
            crate::server::handle_notification(&mut other_view, Notification::new("textDocument/didChange".into(), json!({"textDocument":{"uri":bsl_uri,"version":8},"contentChanges":[{"text":foreign_text}]}))).unwrap();
        }
        let report_uri = if matches!(scenario, SharedScenario::OtherXmlDirty) {
            file_uri(&fixture.descriptor).unwrap()
        } else {
            bsl_uri.clone()
        };
        for (uri, dirty) in [
            (file_uri(&fixture.source.path().join("unrelated.xml")).unwrap(), true),
            (file_uri(&fixture.descriptor).unwrap(), false),
        ] {
            crate::server::handle_request(&mut other_view, Request::new(82.into(), protocol::ABORT.into(), json!({"sessionId":"","violation":true,"documentUri":uri,"dirty":dirty,"eventKind":"change"}))).unwrap();
        }
        if matches!(
            scenario,
            SharedScenario::OtherBslDirtyExactAfter
                | SharedScenario::OtherXmlDirty
                | SharedScenario::OtherBslCleanReload
        ) {
            crate::server::handle_request(&mut other_view, Request::new(83.into(), protocol::ABORT.into(), json!({"sessionId":"","violation":true,"documentUri":report_uri,"dirty":conflict,"eventKind":"change"}))).unwrap();
        }
        assert_eq!(
            owner.disk_gate().participant_violation().load(Ordering::Acquire),
            conflict && !rollback_reload
        );
        assert!(
            !fixture
                .state
                .metadata_rename
                .active
                .as_ref()
                .unwrap()
                .violated
                .load(Ordering::Acquire),
            "the other client's signal must be shared independently of origin state"
        );
        if cancel {
            observe_notification(
                &fixture.state,
                &Notification::new("$/cancelRequest".into(), json!({"id":2})),
            );
        }
        resume.send(()).unwrap();
        let completion = fixture
            .state
            .metadata_rename
            .completion_receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        if rollback_reload {
            assert_eq!(std::fs::read(&fixture.module).unwrap(), original_bsl);
            assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), original_xml);
            foreign_text.clone_from(&original_editor);
            crate::server::handle_notification(&mut other_view, Notification::new("textDocument/didChange".into(), json!({"textDocument":{"uri":bsl_uri,"version":9},"contentChanges":[{"text":foreign_text}]}))).unwrap();
            crate::server::handle_request(&mut other_view, Request::new(84.into(), protocol::ABORT.into(), json!({"sessionId":"","violation":true,"documentUri":bsl_uri,"dirty":conflict,"eventKind":"change"}))).unwrap();
            assert_eq!(
                owner.disk_gate().participant_violation().load(Ordering::Acquire),
                conflict,
                "a proven clean rollback reload is distinct from dirty typing of the same text"
            );
        }
        crate::server::finish_metadata_rename(&mut fixture.state, completion).unwrap();
        let response = fixture
            .receiver
            .try_iter()
            .find_map(|message| match message {
                lsp_server::Message::Response(response) if response.id == RequestId::from(2) => {
                    Some(response)
                }
                _ => None,
            })
            .expect("commit must return after terminal owner publication");
        if rollback_reload && conflict {
            let error = response.error.expect("a late dirty buffer must prevent publication");
            assert!(error.message.contains("recovery_required"), "{error:?}");
            assert!(response.result.is_none());
        } else {
            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(
                response.result.unwrap()["status"],
                if conflict {
                    "recovery_required"
                } else if cancel {
                    "rolled_back"
                } else {
                    "committed"
                }
            );
        }
        if conflict {
            assert!(owner.disk_gate().recovery_required());
            let WorkspaceRead::Ready(current) = owner.snapshot() else {
                panic!("revision-only snapshot")
            };
            assert_eq!(current.generation, initial.generation);
            assert!(
                matches!(owner.request_analysis(), WorkspaceRead::Failed(reason) if reason.contains("recovery"))
            );
            if bsl_open {
                assert_eq!(other_view.mem_docs.get(&bsl_uri), Some(foreign_text.clone()));
                assert_eq!(
                    other_view.mem_docs.get_version(&bsl_uri),
                    Some(if rollback_reload { 9 } else { 8 })
                );
            }
            client.cancel().await.unwrap();
            server_task.abort();
            backend.shutdown();
            return;
        }
        assert!(!owner.disk_gate().is_frozen());
        let WorkspaceRead::Ready(terminal) = owner.snapshot() else { panic!("terminal snapshot") };
        assert_eq!(terminal.generation, initial.generation + 1);
        let final_bsl = std::fs::read_to_string(&fixture.module).unwrap();
        if cancel {
            assert_eq!(final_bsl.as_bytes(), original_bsl);
            assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), original_xml);
        } else {
            assert!(final_bsl.contains("P.Heading"));
        }
        for state in [&mut fixture.state, &mut other_view] {
            let read = state.prepare_disk_read().unwrap();
            let snapshot = state.snapshot().unwrap();
            let id = snapshot.file_id_for_url(&file_uri(&fixture.module).unwrap()).unwrap();
            assert_eq!(
                ide::metadata_rename_plan::metadata_document_fingerprint(
                    &snapshot.analysis.file_text(id)
                ),
                ide::metadata_rename_plan::metadata_document_fingerprint(&final_bsl)
            );
            assert_eq!(state.analysis_host.view().unwrap().generation(), terminal.generation);
            drop(snapshot);
            drop(read);
        }
        let outline = tokio::time::timeout(
            Duration::from_secs(5),
            client.call_tool(
                rmcp::model::CallToolRequestParams::new("outline")
                    .with_arguments(json!({"path":fixture.module}).as_object().unwrap().clone()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!outline.is_error.unwrap_or(false));
        client.cancel().await.unwrap();
        server_task.abort();
        backend.shutdown();
    }

    #[derive(Clone, Copy)]
    enum BeforeWriterEvent {
        Bsl,
        BslBeforeImage,
        BslAfterImage,
        Xml,
        Context,
        Unrelated,
        Cancel,
        CompetingCommit,
    }

    fn other_client_change_before_writer_admission(event: BeforeWriterEvent) {
        use mcp_server::WorkspaceRead;

        let mut fixture = Fixture::new();
        let original_bsl = std::fs::read(&fixture.module).unwrap();
        let original_xml = std::fs::read(&fixture.descriptor).unwrap();
        let project = fixture.state.project.as_ref().unwrap().clone();
        let cache = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(&fixture._cache.path().join("mcp")),
            fixture.source.path(),
            None,
        )
        .unwrap();
        let backend = mcp_server::SharedState::shared_workspace_with_cache_and_prefixes(
            fixture.source.path().to_path_buf(),
            cache,
            None,
        )
        .unwrap();
        let owner = backend.workspace_handle();
        let wakes = owner.subscribe();
        let initial = loop {
            match owner.snapshot() {
                WorkspaceRead::Ready(snapshot) => break snapshot,
                WorkspaceRead::Loading => {
                    wakes.recv_timeout(Duration::from_secs(30)).unwrap();
                }
                _ => panic!("workspace owner did not initialize"),
            }
        };
        let make_view = |sender| {
            let mut state = GlobalState::new_shared(sender, owner.clone());
            state.init_empty_source_root();
            state.adopt_shared_project(fixture.source.path().to_path_buf(), project.clone());
            state.analysis_host.ensure_view(initial.clone());
            state.shared_view_seeded = true;
            state.vfs_done = true;
            state.metadata_rename.supported = true;
            state.metadata_rename.cache_authority = Some(fixture._cache.path().to_path_buf());
            state
        };
        let (sender, receiver) = crossbeam_channel::unbounded();
        let shared = make_view(sender);
        let (other_sender, mut other_receiver) = crossbeam_channel::unbounded();
        let mut other_view = make_view(other_sender);
        fixture.state = shared;
        fixture.receiver = receiver;
        let uri = file_uri(&fixture.module).unwrap();
        let original_text = String::from_utf8(original_bsl.clone()).unwrap();
        let bsl = matches!(
            event,
            BeforeWriterEvent::Bsl
                | BeforeWriterEvent::BslBeforeImage
                | BeforeWriterEvent::BslAfterImage
        );
        if bsl {
            crate::server::handle_notification(&mut other_view, Notification::new(
                "textDocument/didOpen".into(),
                json!({"textDocument":{"uri":uri,"languageId":"bsl","version":7,"text":original_text}}),
            )).unwrap();
        }
        let competing_preview = if matches!(event, BeforeWriterEvent::CompetingCommit) {
            std::mem::swap(&mut fixture.state, &mut other_view);
            std::mem::swap(&mut fixture.receiver, &mut other_receiver);
            let preview = fixture.preview(json!([]));
            assert_eq!(preview["canApply"], true, "{preview:#}");
            std::mem::swap(&mut fixture.state, &mut other_view);
            std::mem::swap(&mut fixture.receiver, &mut other_receiver);
            Some(preview)
        } else {
            None
        };
        let preview = fixture.preview(json!([]));
        assert_eq!(preview["canApply"], true, "{preview:#}");
        let (paused, pause_receiver) = crossbeam_channel::bounded(1);
        let (resume, resume_receiver) = crossbeam_channel::bounded(1);
        fixture.state.metadata_rename.probe = Some(Arc::new(TransactionProbe {
            before_writer: true,
            paused,
            resume: resume_receiver,
            #[cfg(unix)]
            host: None,
        }));
        crate::server::handle_request(
            &mut fixture.state,
            Request::new(
                2.into(),
                protocol::COMMIT.into(),
                json!({"sessionId":preview["sessionId"],"exclusiveSession":true,"documents":[]}),
            ),
        )
        .unwrap();
        assert_eq!(pause_receiver.recv_timeout(Duration::from_secs(30)).unwrap(), 0);
        assert!(fixture.state.metadata_rename.committing());
        assert!(other_view.metadata_rename.active.is_none());
        assert!(!owner.disk_gate().is_frozen(), "pause precedes writer admission");
        assert_eq!(std::fs::read(&fixture.module).unwrap(), original_bsl);
        assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), original_xml);
        let foreign = if matches!(event, BeforeWriterEvent::BslAfterImage) {
            preview["files"]
                .as_array()
                .unwrap()
                .iter()
                .find(|file| file["uri"] == json!(uri))
                .unwrap()["afterText"]
                .as_str()
                .unwrap()
                .to_owned()
        } else if matches!(event, BeforeWriterEvent::BslBeforeImage) {
            original_text.clone()
        } else {
            format!("{original_text}\n// foreign edit before writer admission")
        };
        match event {
            BeforeWriterEvent::Bsl
            | BeforeWriterEvent::BslBeforeImage
            | BeforeWriterEvent::BslAfterImage => {
                crate::server::handle_notification(&mut other_view, Notification::new(
                    "textDocument/didChange".into(),
                    json!({"textDocument":{"uri":uri,"version":8},"contentChanges":[{"text":foreign}]}),
                )).unwrap();
                assert_eq!(other_view.mem_docs.get(&uri), Some(foreign.clone()));
                assert_eq!(other_view.mem_docs.get_version(&uri), Some(8));
            }
            BeforeWriterEvent::Xml | BeforeWriterEvent::Context | BeforeWriterEvent::Unrelated => {
                let changed = match event {
                    BeforeWriterEvent::Xml => fixture.descriptor.clone(),
                    BeforeWriterEvent::Context => fixture.source.path().join("bsl-analyzer.toml"),
                    _ => fixture.source.path().join("unrelated.xml"),
                };
                crate::server::handle_request(&mut other_view, Request::new(
                    83.into(), protocol::ABORT.into(),
                    json!({"sessionId":"","violation":true,"documentUri":file_uri(&changed).unwrap(),"dirty":true,"eventKind":"change"}),
                )).unwrap();
            }
            BeforeWriterEvent::Cancel => {
                crate::server::handle_notification(
                    &mut fixture.state,
                    Notification::new("$/cancelRequest".into(), json!({"id":2})),
                )
                .unwrap();
            }
            BeforeWriterEvent::CompetingCommit => {
                let started = Instant::now();
                crate::server::handle_request(&mut other_view, Request::new(
                    84.into(), protocol::COMMIT.into(),
                    json!({"sessionId":competing_preview.unwrap()["sessionId"],"exclusiveSession":true,"documents":[]}),
                )).unwrap();
                assert!(started.elapsed() < Duration::from_secs(5));
                assert!(other_view.metadata_rename.active.is_none());
                let refusal = other_receiver
                    .try_iter()
                    .find_map(|message| match message {
                        lsp_server::Message::Response(response)
                            if response.id == RequestId::from(84) =>
                        {
                            Some(response)
                        }
                        _ => None,
                    })
                    .expect("the competing commit must refuse before spawning its worker");
                assert!(refusal
                    .error
                    .unwrap()
                    .message
                    .contains("another workspace disk transaction"));
            }
        }
        assert_eq!(std::fs::read(&fixture.module).unwrap(), original_bsl);
        assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), original_xml);
        resume.send(()).unwrap();
        let completion = fixture
            .state
            .metadata_rename
            .completion_receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        crate::server::finish_metadata_rename(&mut fixture.state, completion).unwrap();
        let response = fixture
            .receiver
            .try_iter()
            .find_map(|message| match message {
                lsp_server::Message::Response(response) if response.id == RequestId::from(2) => {
                    Some(response)
                }
                _ => None,
            })
            .expect("commit must return after its terminal publication");
        if bsl {
            assert_eq!(other_view.mem_docs.get(&uri), Some(foreign));
        }
        let readable = owner.disk_gate().try_read().is_ok();
        backend.shutdown();
        assert!(readable, "a pre-write refusal must release its reservation");
        match event {
            BeforeWriterEvent::Unrelated | BeforeWriterEvent::CompetingCommit => {
                assert!(response.error.is_none(), "{response:?}");
                assert_eq!(response.result.unwrap()["status"], "committed");
                assert!(std::fs::read_to_string(&fixture.module).unwrap().contains("P.Heading"));
            }
            _ => {
                let error = response.error.expect("an accepted commit must not report success after an observed edit or cancellation");
                if matches!(event, BeforeWriterEvent::Cancel) {
                    assert_eq!(error.code, ErrorCode::RequestCanceled as i32);
                } else {
                    assert!(error.message.contains("recovery_required"), "{error:?}");
                }
                assert_eq!(std::fs::read(&fixture.module).unwrap(), original_bsl);
                assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), original_xml);
                let context =
                    capture_project_context(fixture.state.project.as_ref().unwrap()).unwrap();
                assert!(JournalStore::open(fixture._cache.path(), JOURNAL_BOUNDS)
                    .unwrap()
                    .pending(&context.roots, context.context_sha256)
                    .unwrap()
                    .is_empty());
            }
        }
    }

    #[test]
    fn shared_other_client_bsl_edit_before_writer_admission_prevents_commit_success() {
        other_client_change_before_writer_admission(BeforeWriterEvent::Bsl);
    }

    #[test]
    fn shared_other_client_xml_edit_before_writer_admission_prevents_commit_success() {
        other_client_change_before_writer_admission(BeforeWriterEvent::Xml);
    }

    #[test]
    fn shared_other_client_context_edit_before_writer_admission_prevents_commit_success() {
        other_client_change_before_writer_admission(BeforeWriterEvent::Context);
    }

    #[test]
    fn shared_other_client_after_image_before_writer_admission_prevents_commit_success() {
        other_client_change_before_writer_admission(BeforeWriterEvent::BslAfterImage);
    }

    #[test]
    fn shared_other_client_before_image_before_writer_admission_prevents_commit_success() {
        other_client_change_before_writer_admission(BeforeWriterEvent::BslBeforeImage);
    }

    #[test]
    fn shared_unrelated_edit_before_writer_admission_preserves_commit() {
        other_client_change_before_writer_admission(BeforeWriterEvent::Unrelated);
    }

    #[test]
    fn shared_cancellation_before_writer_admission_releases_observation() {
        other_client_change_before_writer_admission(BeforeWriterEvent::Cancel);
    }

    #[test]
    fn shared_competing_commit_before_writer_admission_is_bounded_busy() {
        other_client_change_before_writer_admission(BeforeWriterEvent::CompetingCommit);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_commit_hides_real_first_replacement_from_lsp_mcp_search_outline_and_watchers() {
        shared_readers_during_real_journal(SharedScenario::Commit).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_cancel_hides_real_first_replacement_and_publishes_complete_rollback() {
        shared_readers_during_real_journal(SharedScenario::Cancel).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_cancel_clean_before_reload_preserves_complete_rollback() {
        shared_readers_during_real_journal(SharedScenario::CancelCleanBeforeReload).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_cancel_dirty_exact_before_reload_requires_recovery() {
        shared_readers_during_real_journal(SharedScenario::CancelDirtyExactBefore).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_other_client_bsl_typing_after_first_write_requires_recovery() {
        shared_readers_during_real_journal(SharedScenario::OtherBslDirty).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_other_client_clean_bsl_reload_preserves_terminal_view() {
        shared_readers_during_real_journal(SharedScenario::OtherBslCleanReload).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_other_client_dirty_exact_after_text_requires_recovery() {
        shared_readers_during_real_journal(SharedScenario::OtherBslDirtyExactAfter).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_other_client_xml_typing_after_first_write_requires_recovery() {
        shared_readers_during_real_journal(SharedScenario::OtherXmlDirty).await;
    }

    #[test]
    fn preview_refuses_raw_config_changed_before_watcher_updates_selected_project() {
        let mut fixture = Fixture::new();
        std::fs::write(
            fixture.source.path().join("bsl-analyzer.toml"),
            "[source]\nroot='cf'\nextensions=[]\nexternals=[]\nexclude=['cf/Catalogs/Partners']\n",
        )
        .unwrap();
        let text = std::fs::read_to_string(&fixture.descriptor).unwrap();
        let text = text.strip_prefix('\u{feff}').unwrap();
        let prefix = &text[..text.find(">Partners<").unwrap() + 1];
        let response = fixture.request(1, protocol::PREVIEW, json!({"textDocument":{"uri":file_uri(&fixture.descriptor).unwrap()},"position":{"line":0,"character":prefix.encode_utf16().count()},"newName":"Buyers","documents":[]}));
        assert!(
            response.error.is_some()
                || response.result.as_ref().is_some_and(|reply| reply["canApply"] == false),
            "a fresh raw config must not bless cached selection: {response:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&fixture.descriptor).unwrap(),
            format!("\u{feff}{text}")
        );
    }

    #[test]
    fn first_write_recheck_refuses_new_auto_discovered_root_outside_bound_roots() {
        let fixture = Fixture::new();
        std::fs::write(
            fixture.source.path().join("bsl-analyzer.toml"),
            "[source]\nroot='cf'\nexternals=[]\n",
        )
        .unwrap();
        let selected = project_model::Project::new(fixture.source.path()).unwrap();
        let context = capture_project_context(&selected).unwrap();
        validate_live_context(&selected, &context.roots, context.context_sha256, false).unwrap();
        let extension = fixture.source.path().join("cfe/NewExtension");
        std::fs::create_dir_all(&extension).unwrap();
        std::fs::write(extension.join("Configuration.xml"), "<MetaDataObject><Configuration uuid='00000000-0000-0000-0000-000000000099'><Properties><Name>NewExtension</Name><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose></Properties></Configuration></MetaDataObject>").unwrap();
        assert_eq!(
            project_model::Project::new(fixture.source.path()).unwrap().extension_paths().len(),
            1
        );
        assert!(validate_live_context(&selected, &context.roots, context.context_sha256, false)
            .is_err());
    }

    fn commit_completed_before_publication(fixture: &mut Fixture, preview: &Value) -> Completion {
        crate::server::handle_request(
            &mut fixture.state,
            Request::new(
                2.into(),
                protocol::COMMIT.into(),
                json!({"sessionId":preview["sessionId"],"exclusiveSession":true,"documents":[]}),
            ),
        )
        .unwrap();
        let completion = fixture
            .state
            .metadata_rename
            .completion_receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
        assert!(
            matches!(completion.result.as_ref(), Ok(Finished::Commit{receipt,..}) if receipt.status == Status::Committed)
        );
        completion
    }

    #[test]
    fn participant_change_after_durable_commit_keeps_publication_recovery_after_reopen() {
        let mut fixture = Fixture::new();
        let preview = fixture.preview(json!([]));
        let completion = commit_completed_before_publication(&mut fixture, &preview);
        crate::server::handle_request(&mut fixture.state, Request::new(91.into(), protocol::ABORT.into(), json!({"sessionId":"","violation":true,"documentUri":file_uri(&fixture.descriptor).unwrap(),"dirty":true,"eventKind":"change"}))).unwrap();
        crate::server::finish_metadata_rename(&mut fixture.state, completion).unwrap();
        let response = fixture
            .receiver
            .try_iter()
            .find_map(|message| match message {
                lsp_server::Message::Response(response) if response.id == RequestId::from(2) => {
                    Some(response)
                }
                _ => None,
            })
            .unwrap();
        assert!(response.error.unwrap().message.contains("recovery_required"));
        assert!(fixture.state.workspace_disk_gate().recovery_required());
        let context = capture_project_context(fixture.state.project.as_ref().unwrap()).unwrap();
        let reopened = JournalStore::open(fixture._cache.path(), JOURNAL_BOUNDS).unwrap();
        assert!(
            !reopened.pending(&context.roots, context.context_sha256).unwrap().is_empty(),
            "reported recovery must remain durable even after journal source commit"
        );
        let committed_bsl = std::fs::read(&fixture.module).unwrap();
        let committed_xml = std::fs::read(&fixture.descriptor).unwrap();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut restarted = GlobalState::new(sender);
        restarted.init_empty_source_root();
        restarted.metadata_rename.supported = true;
        restarted.metadata_rename.cache_authority = Some(fixture._cache.path().to_path_buf());
        restarted.set_workspace_root(fixture.source.path().to_path_buf()).unwrap();
        while !restarted.vfs_done {
            let message = restarted.loader_receiver.recv_timeout(Duration::from_secs(30)).unwrap();
            crate::server::handle_loader_msg(&mut restarted, message).unwrap();
        }
        fixture.state = restarted;
        fixture.receiver = receiver;
        let recovery = fixture.preview_name("Heading", "Title", json!([]));
        assert_eq!(recovery["operation"], "recover");
        assert_eq!(recovery["canApply"], true);
        assert_eq!(
            recovery["files"],
            json!([]),
            "publication recovery must not propose undoing a durable commit"
        );
        let response = fixture.request(
            3,
            protocol::COMMIT,
            json!({"sessionId":recovery["sessionId"],"exclusiveSession":true,"documents":[]}),
        );
        assert_eq!(response.result.unwrap()["status"], "recovered");
        assert_eq!(std::fs::read(&fixture.module).unwrap(), committed_bsl);
        assert_eq!(std::fs::read(&fixture.descriptor).unwrap(), committed_xml);
        assert!(reopened.pending(&context.roots, context.context_sha256).unwrap().is_empty());
    }

    #[test]
    fn cancellation_after_durable_commit_does_not_force_undo() {
        let mut fixture = Fixture::new();
        let preview = fixture.preview(json!([]));
        let completion = commit_completed_before_publication(&mut fixture, &preview);
        observe_notification(
            &fixture.state,
            &Notification::new("$/cancelRequest".into(), json!({"id":2})),
        );
        crate::server::finish_metadata_rename(&mut fixture.state, completion).unwrap();
        while fixture.state.metadata_rename.writer.is_some() {
            let message =
                fixture.state.loader_receiver.recv_timeout(Duration::from_secs(30)).unwrap();
            crate::server::handle_loader_msg(&mut fixture.state, message).unwrap();
        }
        let response = fixture
            .receiver
            .try_iter()
            .find_map(|message| match message {
                lsp_server::Message::Response(response) if response.id == RequestId::from(2) => {
                    Some(response)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(response.result.unwrap()["status"], "committed");
        assert!(std::fs::read_to_string(&fixture.module).unwrap().contains("P.Heading"));
    }

    #[test]
    fn live_context_recheck_refuses_changed_environment_outside_source_roots() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("cf");
        std::fs::create_dir(&source).unwrap();
        let configuration = source.join("Configuration.xml");
        let original = "<MetaDataObject><Configuration uuid=\"00000000-0000-0000-0000-000000000001\"><Properties><Name>Main</Name></Properties></Configuration></MetaDataObject>";
        std::fs::write(&configuration, original).unwrap();
        std::fs::write(
            temporary.path().join("bsl-analyzer.toml"),
            "[source]\nroot='cf'\nextensions=[]\nexternals=[]\n",
        )
        .unwrap();
        let project = project_model::Project::new(temporary.path()).unwrap();
        let context = capture_project_context(&project).unwrap();
        validate_live_context(&project, &context.roots, context.context_sha256, false).unwrap();
        std::fs::write(
            temporary.path().join(project_model::PROJECT_ENV_FILE_NAME),
            "MODE=changed\n",
        )
        .unwrap();
        assert!(
            validate_live_context(&project, &context.roots, context.context_sha256, false).is_err()
        );
        assert_eq!(std::fs::read_to_string(&configuration).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn extension_host_checkpoint_waits_for_the_directory_and_sibling_xml() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("Old");
        let destination = temp.path().join("New");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.with_extension("xml"), "<descriptor/>\n").unwrap();
        let mut host = ExtensionHostProbe {
            source,
            destination,
            first_operation: false,
            fault: false,
            phase: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        assert!(!host.reached(0));
        host.first_operation = true;
        assert!(host.reached(1));
        host.first_operation = false;
        std::fs::rename(&host.source, &host.destination).unwrap();
        assert!(!host.reached(1));
        std::fs::rename(host.source.with_extension("xml"), host.destination.with_extension("xml"))
            .unwrap();
        assert!(host.reached(2));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires the disposable VS Code ExtensionHost launcher and dedicated fd3"]
    fn extension_host_shared_lsp_server() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::atomic::AtomicUsize;

        let serve = || -> anyhow::Result<()> {
            ensure!(
                std::env::var("BSL_METADATA_TEST_ENABLE").as_deref() == Ok("1"),
                "explicit disposable ExtensionHost fixture required"
            );
            let run = std::path::PathBuf::from(
                std::env::var_os("BSL_METADATA_TEST_RUN").context("test run directory")?,
            )
            .canonicalize()?;
            let root = std::path::PathBuf::from(
                std::env::var_os("BSL_METADATA_TEST_WORKSPACE").context("test workspace")?,
            )
            .canonicalize()?;
            ensure!(
                root == run.join("workspace")
                    && run
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("bsl-metadata-extension-host-"),
                "only the disposable launcher fixture is accepted"
            );
            ensure!(
                std::fs::read_to_string(root.join(".env"))?
                    == "BSL_EXTENSION_TEST_SENTINEL=disposable-fixture\n",
                "unexpected fixture context"
            );
            ensure!(run.join("host-run.json").is_file(), "qualified executable receipt required");
            let scenario = std::env::var("BSL_METADATA_TEST_SCENARIO")?;
            ensure!(
                [
                    "cancel",
                    "fault",
                    "dirty-before-bsl",
                    "dirty-after-bsl",
                    "dirty-before-xml",
                    "dirty-after-xml",
                    "undo",
                    "unproved-namespace",
                    "context-close"
                ]
                .contains(&scenario.as_str()),
                "unsupported controlled fixture scenario"
            );
            let listener = UnixListener::bind(run.join("probe.sock"))?;
            let phase = Arc::new(AtomicUsize::new(0));
            let (paused, paused_rx) = crossbeam_channel::bounded(1);
            let (resume, resume_rx) = crossbeam_channel::bounded(1);
            let control = std::thread::spawn(move || -> anyhow::Result<()> {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(240)))?;
                stream.set_write_timeout(Some(Duration::from_secs(240)))?;
                let mut reader = BufReader::new(stream.try_clone()?);
                for point in paused_rx {
                    let terminal = point == usize::MAX;
                    writeln!(
                        stream,
                        "{}",
                        serde_json::json!({
                            "phase": if terminal { "terminal" } else { "forward" },
                            "applied": if terminal { None } else { Some(point) },
                        })
                    )?;
                    stream.flush()?;
                    let mut answer = String::new();
                    ensure!(
                        reader.read_line(&mut answer)? > 0 && answer.trim() == "resume",
                        "controlled checkpoint was not acknowledged"
                    );
                    resume.send(())?;
                }
                Ok(())
            });
            let probe = Arc::new(TransactionProbe {
                before_writer: false,
                paused,
                resume: resume_rx,
                host: Some(ExtensionHostProbe {
                    source: root.join("cf/Catalogs/Partners"),
                    destination: root.join("cf/Catalogs/RenamedPartners"),
                    first_operation: scenario == "cancel",
                    fault: scenario == "fault",
                    phase: Arc::clone(&phase),
                }),
            });
            let (connection, client) = lsp_server::Connection::memory();
            let sender = client.sender;
            let ingress_path = run.join("client-ingress.jsonl");
            let input = std::thread::spawn(move || -> anyhow::Result<()> {
                let stdin = std::io::stdin();
                let mut reader = BufReader::new(stdin.lock());
                while let Some(message) = lsp_server::Message::read(&mut reader)? {
                    if let lsp_server::Message::Request(request) = &message {
                        if request.method == protocol::ABORT {
                            let mut log = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&ingress_path)?;
                            ensure!(
                                log.metadata()?.len() < 64 * 1024,
                                "fixture ingress log exceeded its bound"
                            );
                            writeln!(
                                log,
                                "{}",
                                serde_json::json!({
                                    "phase": phase.load(Ordering::Acquire),
                                    "violation": request.params.get("violation").and_then(serde_json::Value::as_bool).unwrap_or(false),
                                    "global": request.params.get("sessionId").and_then(serde_json::Value::as_str) == Some(""),
                                    "dirty": request.params.get("dirty"),
                                    "eventKind": request.params.get("eventKind"),
                                })
                            )?;
                        }
                    }
                    let exit = matches!(&message, lsp_server::Message::Notification(notification) if notification.method == "exit");
                    if sender.send(message).is_err() || exit {
                        break;
                    }
                }
                Ok(())
            });
            let mut output = std::fs::OpenOptions::new().write(true).open("/proc/self/fd/3")?;
            let output = std::thread::spawn(move || -> anyhow::Result<()> {
                for message in client.receiver {
                    message.write(&mut output)?;
                }
                Ok(())
            });
            let mut backend = None;
            let result = crate::server::main_loop_shared_with_probe(
                connection,
                || {
                    let project = mcp_server::project::at(&root)?;
                    let cache = mcp_server::WorkspaceCacheLayout::for_project(
                        &project,
                        Some(&run.join("cache")),
                        &root,
                        None,
                    )?;
                    backend =
                        Some(mcp_server::SharedState::shared_workspace_with_cache_and_prefixes(
                            root.clone(),
                            cache,
                            None,
                        )?);
                    Ok(backend.as_ref().unwrap().workspace_handle())
                },
                probe,
            );
            // Unblock an unused control accept if initialization failed before the host connected.
            let _ = UnixStream::connect(run.join("probe.sock"));
            drop(backend);
            control.join().expect("control thread")?;
            output.join().expect("LSP output thread")?;
            if input.is_finished() {
                input.join().expect("LSP input thread")?;
            }
            result
        };
        serve().unwrap();
    }
}
