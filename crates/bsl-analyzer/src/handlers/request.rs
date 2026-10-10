use anyhow::Result;
use base_db::SourceDatabase as _;
use ide::{
    DocumentHighlightKind as IdeDocumentHighlightKind, FoldingRangeKind as IdeFoldingRangeKind,
    InlayHintKind as IdeInlayHintKind, Location as IdeLocation, RenameError,
};
use line_index::LineIndex;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse, CompletionItem,
    CompletionItemKind, CompletionParams, CompletionResponse, DocumentChanges,
    DocumentHighlight as LspDocumentHighlight, DocumentHighlightKind as LspDocumentHighlightKind,
    DocumentHighlightParams, DocumentSymbolParams, DocumentSymbolResponse,
    FoldingRange as LspFoldingRange, FoldingRangeKind as LspFoldingRangeKind, FoldingRangeParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    InlayHint as LspInlayHint, InlayHintKind as LspInlayHintKind, InlayHintLabel, InlayHintParams,
    Location, MarkupContent, MarkupKind, OneOf, OptionalVersionedTextDocumentIdentifier,
    PrepareRenameResponse, Range, ReferenceParams, RenameParams, SelectionRange,
    SelectionRangeParams, SemanticTokens, SemanticTokensParams, SemanticTokensResult,
    SignatureHelpParams, SymbolKind, TextDocumentEdit, TextDocumentPositionParams, TextEdit,
    WorkspaceEdit, WorkspaceSymbol as LspWorkspaceSymbol, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use rustc_hash::FxHashMap;
use salsa::Database as _;
use vfs::FileId;

use crate::call_hierarchy_index_state::{
    CallHierarchyIndexCompletion, CallHierarchyIndexPrepareAction,
};
use crate::frozen_context::LatencyRequestContext;
use crate::global_state::{GlobalStateSnapshot, Task};

const CALL_HIERARCHY_CANCELLATION_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(10);

pub fn handle_goto_definition(
    ctx: &LatencyRequestContext,
    params: GotoDefinitionParams,
) -> Result<Option<GotoDefinitionResponse>> {
    let _p = tracing::info_span!(
        "handle_goto_definition",
        uri = %params.text_document_position_params.text_document.uri
    )
    .entered();

    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let source_doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = source_doc.text();
    let line_index = source_doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let target = ctx.analysis.goto_definition(file_id, offset.into());

    match target {
        Some(nav_target) => {
            tracing::debug!(
                target_file_id = nav_target.file_id.0,
                ?nav_target.range,
                "goto_definition: found target"
            );
            let target_url = ctx.url_for_file_id(nav_target.file_id)?;

            let target_text: String = if nav_target.file_id == file_id {
                text.to_string()
            } else if let Some(doc) = ctx.mem_docs.get(&target_url) {
                doc.text().to_string()
            } else {
                ctx.analysis.file_text(nav_target.file_id)
            };

            let target_line_index = LineIndex::new(&target_text);

            let target_range = crate::lsp::range_with_encoding(
                &target_line_index,
                &target_text,
                nav_target.range,
                ctx.position_encoding,
            )
            .ok_or_else(|| anyhow::anyhow!("Failed to convert range"))?;

            let location = Location { uri: target_url, range: target_range };

            Ok(Some(GotoDefinitionResponse::Scalar(location)))
        }
        None => {
            tracing::debug!(
                file_id = file_id.0,
                offset = u32::from(offset),
                "goto_definition: no target found"
            );
            Ok(None)
        }
    }
}

pub fn handle_type_definition(
    ctx: &LatencyRequestContext,
    params: GotoDefinitionParams,
) -> Result<Option<GotoDefinitionResponse>> {
    let _p = tracing::info_span!(
        "handle_type_definition",
        uri = %params.text_document_position_params.text_document.uri
    )
    .entered();

    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let source_doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = source_doc.text();
    let line_index = source_doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let Some(nav_target) = ctx.analysis.type_definition(file_id, offset.into()) else {
        return Ok(None);
    };

    let target_url = ctx.url_for_file_id(nav_target.file_id)?;
    let target_text: String = if nav_target.file_id == file_id {
        text.to_string()
    } else if let Some(doc) = ctx.mem_docs.get(&target_url) {
        doc.text().to_string()
    } else {
        ctx.analysis.file_text(nav_target.file_id)
    };
    let target_line_index = LineIndex::new(&target_text);
    let target_range = crate::lsp::range_with_encoding(
        &target_line_index,
        &target_text,
        nav_target.range,
        ctx.position_encoding,
    )
    .ok_or_else(|| anyhow::anyhow!("Failed to convert range"))?;

    Ok(Some(GotoDefinitionResponse::Scalar(Location { uri: target_url, range: target_range })))
}

pub fn handle_find_references(
    ctx: &LatencyRequestContext,
    params: ReferenceParams,
) -> Result<Option<Vec<Location>>> {
    let _p = tracing::info_span!(
        "handle_find_references",
        uri = %params.text_document_position.text_document.uri
    )
    .entered();

    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let locations = ctx.analysis.find_references(file_id, offset.into());

    if locations.is_empty() {
        return Ok(None);
    }

    let mut converter = ReferenceLocationConverter::new(ctx, file_id, text);
    let lsp_locations: Vec<Location> =
        locations.into_iter().map(|loc| converter.convert(loc)).collect::<Result<Vec<_>>>()?;

    if lsp_locations.is_empty() {
        Ok(None)
    } else {
        Ok(Some(lsp_locations))
    }
}

pub fn handle_prepare_rename(
    ctx: &LatencyRequestContext,
    params: TextDocumentPositionParams,
) -> Result<Option<PrepareRenameResponse>> {
    let _p =
        tracing::info_span!("handle_prepare_rename", uri = %params.text_document.uri).entered();

    let uri = params.text_document.uri;
    let position = params.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let Some(target) = ctx.analysis.prepare_rename(file_id, offset.into()) else {
        return Ok(None);
    };

    let range =
        crate::lsp::range_with_encoding(line_index, text, target.range, ctx.position_encoding)
            .ok_or_else(|| anyhow::anyhow!("Failed to convert rename range"))?;

    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
        range,
        placeholder: target.current_name,
    }))
}

pub fn handle_rename(
    ctx: &LatencyRequestContext,
    params: RenameParams,
) -> Result<Option<WorkspaceEdit>> {
    let _p = tracing::info_span!(
        "handle_rename",
        uri = %params.text_document_position.text_document.uri
    )
    .entered();

    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;
    let new_name = params.new_name;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let locations = match ctx.analysis.rename(file_id, offset.into(), &new_name) {
        Ok(locations) => locations,
        Err(RenameError::NotRenameable) => {
            anyhow::bail!("This symbol cannot be renamed")
        }
        Err(RenameError::InvalidIdentifier(name)) => {
            anyhow::bail!("'{name}' is not a valid identifier")
        }
    };

    let mut converter = ReferenceLocationConverter::new(ctx, file_id, text);
    let mut grouped: FxHashMap<lsp_types::Url, Vec<TextEdit>> = FxHashMap::default();
    for location in locations {
        let lsp_location = converter.convert(location)?;
        grouped
            .entry(lsp_location.uri)
            .or_default()
            .push(TextEdit { range: lsp_location.range, new_text: new_name.clone() });
    }

    let edit = if ctx.supports_workspace_edit_document_changes {
        // Versioned edits let the client discard a rename computed against a buffer it
        // has since changed. An unopened file carries `None` — its on-disk content is
        // authoritative, so there is no editor version to match.
        let document_changes = grouped
            .into_iter()
            .map(|(uri, edits)| {
                let version = ctx.mem_docs.get(&uri).map(|doc| doc.version());
                TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                }
            })
            .collect();
        WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(document_changes)),
            change_annotations: None,
        }
    } else {
        WorkspaceEdit {
            changes: Some(grouped.into_iter().collect()),
            document_changes: None,
            change_annotations: None,
        }
    };

    Ok(Some(edit))
}

pub fn handle_prepare_call_hierarchy(
    ctx: &LatencyRequestContext,
    params: CallHierarchyPrepareParams,
) -> Result<Option<Vec<CallHierarchyItem>>> {
    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let Some(ide_item) = ctx.analysis.prepare_call_hierarchy(file_id, offset.into()) else {
        return Ok(None);
    };

    let db = ctx.analysis.database();
    let source_root = db.file_source_root_input(file_id).source_root_id(db);
    let authorization = ctx.call_hierarchy_index.prepare_authorization(source_root);
    let _span = tracing::info_span!(
        "handle_prepare_call_hierarchy",
        uri = %uri,
        ?source_root,
        generation = authorization.map(|(generation, _)| generation),
        implementation = "compact_reverse_index",
        workspace_call_graph = false,
    )
    .entered();
    match authorization {
        Some((generation, CallHierarchyIndexPrepareAction::StartBuild)) => {
            tracing::debug!(
                ?source_root,
                generation,
                phase = "prepare",
                authorization = "accepted",
                "call hierarchy prepare authorized compact index generation"
            );
            if let Err(error) = ctx
                .task_sender
                .send(Task::CallHierarchyIndexBuildRequested { source_root, generation })
            {
                tracing::warn!(
                    ?source_root,
                    generation,
                    ?error,
                    "call hierarchy prepare could not enqueue index build"
                );
            }
        }
        Some((generation, CallHierarchyIndexPrepareAction::UseReady)) => tracing::debug!(
            ?source_root,
            generation,
            phase = "prepare",
            authorization = "ready",
            "call hierarchy prepare reused compact index generation"
        ),
        Some((generation, CallHierarchyIndexPrepareAction::UseExisting)) => tracing::debug!(
            ?source_root,
            generation,
            phase = "prepare",
            authorization = "already_authorized",
            "call hierarchy prepare reused compact index generation"
        ),
        None => tracing::warn!(
            ?source_root,
            phase = "prepare",
            failure_reason = "generation_overflow",
            "call hierarchy index generation overflow"
        ),
    }

    let mut converter = ReferenceLocationConverter::new(ctx, file_id, text);
    let item = to_lsp_call_hierarchy_item(&mut converter, ide_item)?;
    Ok(Some(vec![item]))
}

pub fn handle_call_hierarchy_incoming(
    ctx: &LatencyRequestContext,
    params: CallHierarchyIncomingCallsParams,
) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
    let item = params.item;
    let file_id = ctx.file_id_for_url(&item.uri)?;
    let text = call_hierarchy_anchor_text(ctx, &item.uri, file_id);
    let line_index = LineIndex::new(&text);

    let offset = crate::lsp::offset_with_encoding(
        &line_index,
        &text,
        item.selection_range.start,
        ctx.position_encoding,
    )?;

    let db = ctx.analysis.database();
    let source_root = db.file_source_root_input(file_id).source_root_id(db);
    let generation = ctx.call_hierarchy_index.generation(source_root);
    let _span = tracing::info_span!(
        "handle_call_hierarchy_incoming",
        uri = %item.uri,
        ?source_root,
        ?generation,
        wait_timeout_ms = ctx.call_hierarchy_wait_policy.timeout.as_millis() as u64,
        implementation = "compact_reverse_index",
        workspace_call_graph = false,
    )
    .entered();
    let Some(generation) = generation else {
        tracing::debug!(
            ?source_root,
            phase = "incoming",
            wait_result = "unprepared",
            null_reason = "unprepared",
            "call hierarchy incoming returned null"
        );
        return Ok(None);
    };
    if !ctx.call_hierarchy_index.is_prepared(source_root, generation) {
        tracing::debug!(
            ?source_root,
            generation,
            phase = "incoming",
            wait_result = "stale_prepare",
            null_reason = "stale_prepare",
            "call hierarchy incoming returned null"
        );
        return Ok(None);
    }
    let index = match ctx.call_hierarchy_index.wait_or_ready(source_root, generation) {
        Some(crate::call_hierarchy_index_state::CallHierarchyIndexWaitOrReady::Ready(index)) => {
            tracing::debug!(
                ?source_root,
                generation,
                phase = "incoming",
                wait_result = "ready",
                "call hierarchy incoming read compact index"
            );
            index
        }
        Some(crate::call_hierarchy_index_state::CallHierarchyIndexWaitOrReady::Waiting(waiter)) => {
            let started = std::time::Instant::now();
            let deadline = started + ctx.call_hierarchy_wait_policy.timeout;
            tracing::debug!(
                ?source_root,
                generation,
                timeout_ms = ctx.call_hierarchy_wait_policy.timeout.as_millis() as u64,
                phase = "incoming",
                "call hierarchy incoming waiting for compact index"
            );
            loop {
                ctx.analysis.database().unwind_if_revision_cancelled();
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    tracing::debug!(
                        ?source_root,
                        generation,
                        waited_ms = started.elapsed().as_millis() as u64,
                        phase = "incoming",
                        wait_result = "timeout",
                        null_reason = "timeout",
                        "call hierarchy incoming returned null"
                    );
                    return Ok(None);
                }

                match waiter.recv_timeout(remaining.min(CALL_HIERARCHY_CANCELLATION_POLL_INTERVAL))
                {
                    Ok(CallHierarchyIndexCompletion::Ready(index)) => {
                        tracing::debug!(
                            ?source_root,
                            generation,
                            waited_ms = started.elapsed().as_millis() as u64,
                            phase = "incoming",
                            wait_result = "ready",
                            "call hierarchy incoming compact index became ready"
                        );
                        break index;
                    }
                    Ok(CallHierarchyIndexCompletion::Failed(reason)) => {
                        tracing::debug!(
                            ?source_root,
                            generation,
                            %reason,
                            phase = "incoming",
                            wait_result = "failed",
                            null_reason = "failed",
                            "call hierarchy incoming returned null"
                        );
                        return Ok(None);
                    }
                    Ok(CallHierarchyIndexCompletion::Superseded) => {
                        tracing::debug!(
                            ?source_root,
                            generation,
                            phase = "incoming",
                            wait_result = "superseded",
                            null_reason = "superseded",
                            "call hierarchy incoming returned null"
                        );
                        return Ok(None);
                    }
                    Ok(CallHierarchyIndexCompletion::Shutdown) => {
                        tracing::debug!(
                            ?source_root,
                            generation,
                            phase = "incoming",
                            wait_result = "shutdown",
                            null_reason = "shutdown",
                            "call hierarchy incoming returned null"
                        );
                        return Ok(None);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        tracing::debug!(
                            ?source_root,
                            generation,
                            phase = "incoming",
                            wait_result = "disconnected",
                            null_reason = "disconnected",
                            "call hierarchy incoming returned null"
                        );
                        return Ok(None);
                    }
                }
            }
        }
        None => {
            tracing::debug!(
                ?source_root,
                generation,
                phase = "incoming",
                wait_result = "waiter_unavailable",
                null_reason = "waiter_unavailable",
                "call hierarchy incoming returned null"
            );
            return Ok(None);
        }
    };
    let Some(calls) =
        ctx.analysis.call_hierarchy_incoming_from_index(file_id, offset.into(), index)
    else {
        tracing::debug!(
            ?source_root,
            generation,
            phase = "incoming",
            wait_result = "ready",
            null_reason = "unresolved_or_no_indexed_callers",
            "call hierarchy incoming returned null"
        );
        return Ok(None);
    };

    let _served_span = tracing::info_span!(
        "call_hierarchy_incoming_served_from_index",
        ?source_root,
        generation,
        caller_count = calls.len(),
        implementation = "compact_reverse_index",
        workspace_call_graph = false,
    )
    .entered();
    tracing::debug!(
        ?source_root,
        generation,
        phase = "incoming",
        wait_result = "ready",
        caller_count = calls.len(),
        "call hierarchy incoming served from compact index"
    );
    let mut converter = ReferenceLocationConverter::new(ctx, file_id, &text);
    let mut result = Vec::with_capacity(calls.len());
    for call in calls {
        // Incoming call sites live in the caller's own file.
        let caller_file = call.item.file_id;
        let from = to_lsp_call_hierarchy_item(&mut converter, call.item)?;
        let from_ranges = convert_call_ranges(&mut converter, caller_file, call.ranges)?;
        result.push(CallHierarchyIncomingCall { from, from_ranges });
    }
    Ok(Some(result))
}

pub fn handle_call_hierarchy_outgoing(
    ctx: &LatencyRequestContext,
    params: CallHierarchyOutgoingCallsParams,
) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
    let _p =
        tracing::info_span!("handle_call_hierarchy_outgoing", uri = %params.item.uri).entered();

    let item = params.item;
    let file_id = ctx.file_id_for_url(&item.uri)?;
    let text = call_hierarchy_anchor_text(ctx, &item.uri, file_id);
    let line_index = LineIndex::new(&text);

    let offset = crate::lsp::offset_with_encoding(
        &line_index,
        &text,
        item.selection_range.start,
        ctx.position_encoding,
    )?;

    let calls = ctx.analysis.call_hierarchy_outgoing(file_id, offset.into());
    if calls.is_empty() {
        return Ok(None);
    }

    let mut converter = ReferenceLocationConverter::new(ctx, file_id, &text);
    let mut result = Vec::with_capacity(calls.len());
    for call in calls {
        let to = to_lsp_call_hierarchy_item(&mut converter, call.item)?;
        // Outgoing call sites live in the anchor method's file, not the callee's.
        let from_ranges = convert_call_ranges(&mut converter, file_id, call.ranges)?;
        result.push(CallHierarchyOutgoingCall { to, from_ranges });
    }
    Ok(Some(result))
}

fn call_hierarchy_anchor_text(
    ctx: &LatencyRequestContext,
    uri: &lsp_types::Url,
    file_id: FileId,
) -> String {
    match ctx.mem_docs.get(uri) {
        Some(doc) => doc.text().to_string(),
        None => ctx.analysis.file_text(file_id),
    }
}

fn to_lsp_call_hierarchy_item(
    converter: &mut ReferenceLocationConverter,
    item: ide::CallHierarchyItem,
) -> Result<CallHierarchyItem> {
    let range = converter.convert(IdeLocation { file_id: item.file_id, range: item.range })?;
    let selection =
        converter.convert(IdeLocation { file_id: item.file_id, range: item.selection_range })?;
    Ok(CallHierarchyItem {
        name: item.name,
        kind: if item.is_function { SymbolKind::FUNCTION } else { SymbolKind::METHOD },
        tags: None,
        detail: item.detail,
        uri: range.uri,
        range: range.range,
        selection_range: selection.range,
        data: None,
    })
}

fn convert_call_ranges(
    converter: &mut ReferenceLocationConverter,
    file_id: FileId,
    ranges: Vec<ide::TextRange>,
) -> Result<Vec<Range>> {
    ranges
        .into_iter()
        .map(|range| Ok(converter.convert(IdeLocation { file_id, range })?.range))
        .collect()
}

pub fn handle_inlay_hint(
    ctx: &LatencyRequestContext,
    params: InlayHintParams,
) -> Result<Option<Vec<LspInlayHint>>> {
    let _p = tracing::info_span!("handle_inlay_hint", uri = %params.text_document.uri).entered();

    let uri = params.text_document.uri;
    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    // A visible range from a since-shrunk buffer can fall out of bounds; treat it
    // as "nothing to hint" rather than a request failure, matching signature help.
    let (Ok(start), Ok(end)) = (
        crate::lsp::offset_with_encoding(
            line_index,
            text,
            params.range.start,
            ctx.position_encoding,
        ),
        crate::lsp::offset_with_encoding(line_index, text, params.range.end, ctx.position_encoding),
    ) else {
        return Ok(None);
    };
    let range = ide::TextRange::new(start, end);

    let hints = ctx.analysis.inlay_hints(file_id, range);
    if hints.is_empty() {
        return Ok(None);
    }

    let mut result = Vec::with_capacity(hints.len());
    for hint in hints {
        let converted = crate::lsp::range_with_encoding(
            line_index,
            text,
            ide::TextRange::empty(hint.position),
            ctx.position_encoding,
        )
        .ok_or_else(|| anyhow::anyhow!("Failed to convert inlay hint position"))?;
        result.push(LspInlayHint {
            position: converted.start,
            label: InlayHintLabel::String(hint.label),
            kind: Some(match hint.kind {
                IdeInlayHintKind::Parameter => LspInlayHintKind::PARAMETER,
                IdeInlayHintKind::Type => LspInlayHintKind::TYPE,
            }),
            text_edits: None,
            tooltip: None,
            padding_left: Some(hint.padding_left),
            padding_right: Some(hint.padding_right),
            data: None,
        });
    }
    Ok(Some(result))
}

pub fn handle_document_highlight(
    ctx: &LatencyRequestContext,
    params: DocumentHighlightParams,
) -> Result<Option<Vec<LspDocumentHighlight>>> {
    let _p = tracing::info_span!(
        "handle_document_highlight",
        uri = %params.text_document_position_params.text_document.uri
    )
    .entered();

    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let highlights = ctx.analysis.document_highlights(file_id, offset.into());
    if highlights.is_empty() {
        return Ok(None);
    }

    let lsp_highlights: Vec<LspDocumentHighlight> = highlights
        .into_iter()
        .filter_map(|highlight| {
            let range = crate::lsp::range_with_encoding(
                line_index,
                text,
                highlight.range,
                ctx.position_encoding,
            )?;
            Some(LspDocumentHighlight {
                range,
                kind: Some(convert_document_highlight_kind(highlight.kind)),
            })
        })
        .collect();

    if lsp_highlights.is_empty() {
        Ok(None)
    } else {
        Ok(Some(lsp_highlights))
    }
}

pub fn handle_folding_range(
    ctx: &LatencyRequestContext,
    params: FoldingRangeParams,
) -> Result<Option<Vec<LspFoldingRange>>> {
    let _p = tracing::info_span!("handle_folding_range", uri = %params.text_document.uri).entered();

    let uri = params.text_document.uri;
    let file_id = ctx.file_id_for_url(&uri)?;

    // The document must be open, but only as a precondition now: the folded
    // lines come from `ide` already resolved over the analysis text, so the
    // handler no longer projects offsets through a second line index built
    // over a second text (github#55).
    if ctx.mem_docs.get(&uri).is_none() {
        return Err(anyhow::anyhow!("Document not in MemDocs: {}", uri));
    }

    let ranges = ctx.analysis.folding_ranges(file_id);
    if ranges.is_empty() {
        return Ok(None);
    }

    let mut lsp_ranges: Vec<LspFoldingRange> = ranges
        .into_iter()
        .map(|folding_range| LspFoldingRange {
            start_line: folding_range.start_line,
            start_character: None,
            end_line: folding_range.end_line,
            end_character: None,
            kind: folding_range.kind.map(convert_folding_range_kind),
            collapsed_text: None,
        })
        .collect();

    // A client that named a `rangeLimit` gets no more than that many ranges
    // (github#56); which folds leave first is documented below.
    if let Some(limit) = ctx.folding_range_limit {
        truncate_folding_ranges(&mut lsp_ranges, limit as usize);
    }

    if lsp_ranges.is_empty() {
        Ok(None)
    } else {
        Ok(Some(lsp_ranges))
    }
}

/// Trims the response to the client's `rangeLimit`. The folds that leave first
/// are the deepest ones: collapsing an outer fold hides the inner ones anyway,
/// so the outer skeleton — regions, procedures — survives the cut. Among equals
/// the shorter fold leaves first (it hides fewer lines), then the later one.
fn truncate_folding_ranges(ranges: &mut Vec<LspFoldingRange>, limit: usize) {
    if ranges.len() <= limit {
        return;
    }

    let depths = folding_depths(ranges);
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by_key(|&index| {
        let range = &ranges[index];
        (depths[index], std::cmp::Reverse(range.end_line - range.start_line), range.start_line)
    });

    let mut keep: Vec<usize> = order[..limit].to_vec();
    keep.sort_unstable();
    let kept: Vec<LspFoldingRange> = keep.into_iter().map(|index| ranges[index].clone()).collect();

    tracing::info!(
        limit,
        kept = kept.len(),
        dropped = ranges.len() - kept.len(),
        "folding ranges truncated to the client's rangeLimit"
    );
    *ranges = kept;
}

/// How deeply each fold is nested. A stack of open ends: a fold stays an
/// ancestor while its end is not before the current fold's end. For properly
/// nested ranges this is exactly the number of ancestors; when ranges cross, an
/// evicted ancestor is not counted, so the depth reads as the number of *open*
/// enveloping folds — a lower bound, which is all the cut order needs.
fn folding_depths(ranges: &[LspFoldingRange]) -> Vec<u32> {
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by_key(|&index| {
        (ranges[index].start_line, std::cmp::Reverse(ranges[index].end_line))
    });

    let mut depths = vec![0u32; ranges.len()];
    let mut open_ends: Vec<u32> = Vec::new();
    for index in order {
        let end_line = ranges[index].end_line;
        while open_ends.last().is_some_and(|&open_end| open_end < end_line) {
            open_ends.pop();
        }
        depths[index] = open_ends.len() as u32;
        open_ends.push(end_line);
    }
    depths
}

pub fn handle_hover(ctx: &LatencyRequestContext, params: HoverParams) -> Result<Option<Hover>> {
    let _p = tracing::info_span!(
        "handle_hover",
        uri = %params.text_document_position_params.text_document.uri
    )
    .entered();

    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let hover_result = ctx.analysis.hover(file_id, offset.into(), ctx.diagnostics_config.locale);

    match hover_result {
        Some(result) => {
            let range = result.range.and_then(|r| {
                crate::lsp::range_with_encoding(line_index, text, r, ctx.position_encoding)
            });

            let contents = HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: result.markup,
            });

            Ok(Some(Hover { contents, range }))
        }
        None => Ok(None),
    }
}

pub fn handle_completion(
    ctx: &LatencyRequestContext,
    params: CompletionParams,
) -> Result<Option<CompletionResponse>> {
    let _p = tracing::info_span!(
        "handle_completion",
        uri = %params.text_document_position.text_document.uri
    )
    .entered();

    tracing::debug!(
        "completion request at line={} char={}",
        params.text_document_position.position.line,
        params.text_document_position.position.character
    );

    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let line_num = position.line as usize;
    let lines: Vec<&str> = text.lines().collect();
    if line_num < lines.len() {
        let line_text = lines[line_num];
        tracing::debug!(
            "Line {} content (first 100 chars): {:?}",
            line_num,
            &line_text.chars().take(100).collect::<String>()
        );
        tracing::debug!(
            "Position.character={} (UTF-16 code units from line start)",
            position.character
        );
    }

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };
    tracing::debug!("Converted position to offset: {:?}", offset);

    let items = ctx.analysis.completions(
        file_id,
        offset.into(),
        ctx.workspace_root.clone(),
        ctx.diagnostics_config.locale,
    );
    tracing::debug!("IDE API returned {} completion items", items.len());

    if items.is_empty() {
        tracing::debug!("No completion items, returning None");
        return Ok(None);
    }

    // Multi-line snippet bodies carry only relative (tab) nesting; ask clients
    // that honor `InsertTextMode` to indent the continuation lines to the cursor
    // column. Clients without the capability re-indent by their own default.
    let adjust_indentation = ctx.supports_insert_text_mode_adjust_indentation;
    let lsp_items: Vec<CompletionItem> =
        items.into_iter().map(|item| convert_completion_item(item, adjust_indentation)).collect();
    tracing::debug!("Converted to {} LSP items, returning CompletionResponse", lsp_items.len());

    Ok(Some(CompletionResponse::Array(lsp_items)))
}

pub fn handle_semantic_tokens_full(
    ctx: &LatencyRequestContext,
    params: SemanticTokensParams,
) -> Result<Option<SemanticTokensResult>> {
    let start = std::time::Instant::now();
    let _p = tracing::info_span!(
        "handle_semantic_tokens_full",
        uri = %params.text_document.uri
    )
    .entered();

    let uri = params.text_document.uri;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let highlight_start = std::time::Instant::now();
    let highlight_result = ctx.analysis.highlight(file_id);
    let highlight_elapsed = highlight_start.elapsed();
    tracing::debug!(
        file_id = file_id.0,
        highlight_count = highlight_result.highlights.len(),
        resolved_external_files = highlight_result.resolved_external_files.len(),
        elapsed_ms = highlight_elapsed.as_millis() as u64,
        "semantic_tokens: analysis.highlight() completed"
    );

    let tokens = crate::lsp::semantic_tokens_with_encoding(
        line_index,
        text,
        &highlight_result.highlights,
        ctx.position_encoding,
    );
    let total_elapsed = start.elapsed();
    tracing::debug!(
        file_id = file_id.0,
        token_count = tokens.len(),
        total_ms = total_elapsed.as_millis() as u64,
        %uri,
        "semantic_tokens: completed"
    );

    if !highlight_result.resolved_external_files.is_empty() {
        use crate::global_state::Task;
        let _ = ctx
            .task_sender
            .send(Task::PreloadExternalFiles { files: highlight_result.resolved_external_files });
    }

    Ok(Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data: tokens })))
}

pub fn handle_document_symbol(
    ctx: &LatencyRequestContext,
    params: DocumentSymbolParams,
) -> Result<Option<DocumentSymbolResponse>> {
    let _p = tracing::info_span!(
        "handle_document_symbol",
        uri = %params.text_document.uri
    )
    .entered();

    let uri = params.text_document.uri;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let symbols = ctx.analysis.document_symbols(file_id);

    document_symbol_response(line_index, text, symbols, ctx.position_encoding, &uri)
}

pub fn handle_selection_range(
    ctx: &LatencyRequestContext,
    params: SelectionRangeParams,
) -> Result<Option<Vec<SelectionRange>>> {
    let _p =
        tracing::info_span!("handle_selection_range", uri = %params.text_document.uri).entered();

    let uri = params.text_document.uri;
    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let mut offsets = Vec::with_capacity(params.positions.len());
    for position in &params.positions {
        // The result must be one range per position; a stale position (buffer
        // shrank under the request) can't be answered, so bail cleanly.
        let Ok(offset) =
            crate::lsp::offset_with_encoding(line_index, text, *position, ctx.position_encoding)
        else {
            return Ok(None);
        };
        offsets.push(offset);
    }

    let chains = ctx.analysis.selection_ranges(file_id, &offsets);

    let mut result = Vec::with_capacity(chains.len());
    for chain in chains {
        // Nest outermost → innermost so each `parent` points at the wider span.
        let mut current: Option<Box<SelectionRange>> = None;
        for range in chain.iter().rev() {
            let lsp_range =
                crate::lsp::range_with_encoding(line_index, text, *range, ctx.position_encoding)
                    .ok_or_else(|| anyhow::anyhow!("Failed to convert selection range"))?;
            current = Some(Box::new(SelectionRange { range: lsp_range, parent: current }));
        }
        let Some(innermost) = current else {
            return Ok(None);
        };
        result.push(*innermost);
    }
    Ok(Some(result))
}

pub fn handle_workspace_symbol(
    ctx: &LatencyRequestContext,
    params: WorkspaceSymbolParams,
) -> Result<Option<WorkspaceSymbolResponse>> {
    let _p = tracing::info_span!("handle_workspace_symbol", query = %params.query).entered();

    let found = ctx.analysis.workspace_symbols(&params.query);
    if found.truncated {
        // `workspace/symbol` has no field for an incomplete answer, so the cut
        // cannot reach the client. It can reach whoever is reading the log.
        tracing::info!(
            total = found.total,
            total_exact = found.total_exact,
            returned = found.candidates.len(),
            "workspace/symbol result was capped; the protocol cannot say so to the client",
        );
    }
    let symbols = found.candidates;
    if symbols.is_empty() {
        return Ok(None);
    }

    // No document of its own: this request names no file, and every candidate's
    // file is read the same overlay-or-disk way.
    let mut converter = ReferenceLocationConverter::detached(ctx);

    let mut result = Vec::with_capacity(symbols.len());
    for symbol in symbols {
        let Some(place) = symbol.place else { continue };
        // A module or a metadata object has no declaration node of its own; the
        // file start is where its card is anchored too.
        let range = place.range.unwrap_or_else(|| ide::TextRange::empty(0.into()));
        // Reading a closed file's text panics when it changed under analysis or
        // became unreadable — a race this answer walks straight into, since a
        // name search returns files nobody asked about and metadata XML is
        // rewritten by the configurator and by git alike. One racing file must
        // not fail the whole request; a `salsa::Cancelled` is a real abort and
        // keeps unwinding.
        let converted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            converter.convert(IdeLocation { file_id: place.file_id, range })
        }));
        let location = match converted {
            Ok(location) => location?,
            Err(payload) if payload.is::<salsa::Cancelled>() => std::panic::resume_unwind(payload),
            Err(_) => {
                tracing::warn!(
                    file_id = place.file_id.0,
                    "workspace/symbol: skipping a candidate after a text-read panic"
                );
                continue;
            }
        };
        result.push(LspWorkspaceSymbol {
            name: symbol.display,
            kind: workspace_symbol_kind(symbol.category),
            tags: None,
            container_name: None,
            location: OneOf::Left(location),
            data: None,
        });
    }
    Ok(Some(WorkspaceSymbolResponse::Nested(result)))
}

/// The dictionary's category as an LSP kind.
///
/// The mapping lives here, at the protocol boundary, because it is a fact about
/// LSP and not about BSL. It is also lossy in a way the old one already was: a
/// procedure and a function were distinct in the analyzer and both `FUNCTION`
/// here, so collapsing them into one category costs the client nothing.
fn workspace_symbol_kind(category: ide::NameCategory) -> SymbolKind {
    match category {
        ide::NameCategory::CommonModule | ide::NameCategory::Module => SymbolKind::MODULE,
        ide::NameCategory::ModuleMethod => SymbolKind::FUNCTION,
        ide::NameCategory::ModuleVariable => SymbolKind::VARIABLE,
        ide::NameCategory::MetadataObject => SymbolKind::CLASS,
        ide::NameCategory::MetadataMember => SymbolKind::FIELD,
        ide::NameCategory::Form => SymbolKind::OBJECT,
        // Unreachable in practice: a platform member has no file, and the
        // request asks only for candidates that have one. The arm exists
        // because the vocabulary is closed, not because the case can arrive.
        ide::NameCategory::PlatformMember => SymbolKind::FUNCTION,
    }
}

pub fn handle_signature_help(
    ctx: &LatencyRequestContext,
    params: SignatureHelpParams,
) -> Result<Option<lsp_types::SignatureHelp>> {
    let _p = tracing::info_span!(
        "handle_signature_help",
        uri = %params.text_document_position_params.text_document.uri
    )
    .entered();

    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = ctx.file_id_for_url(&uri)?;

    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();

    let offset =
        match crate::lsp::offset_with_encoding(line_index, text, position, ctx.position_encoding) {
            Ok(o) => o,
            Err(_) => {
                tracing::debug!(
                    "Position out of bounds, likely race with didChange - returning empty"
                );
                return Ok(None);
            }
        };

    let sig_help = ctx.analysis.signature_help(file_id, offset.into());

    Ok(sig_help.map(to_lsp_signature_help))
}

fn to_lsp_signature_help(sh: ide::SignatureHelp) -> lsp_types::SignatureHelp {
    let signatures: Vec<_> = sh
        .signatures
        .iter()
        .map(|sig| {
            let parameters: Vec<_> = sig
                .parameters
                .iter()
                .map(|p| lsp_types::ParameterInformation {
                    label: lsp_types::ParameterLabel::Simple(p.label.clone()),
                    documentation: p.documentation.as_ref().map(|d| {
                        lsp_types::Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: d.clone(),
                        })
                    }),
                })
                .collect();

            lsp_types::SignatureInformation {
                label: sig.signature.clone(),
                documentation: sig.doc.as_ref().map(|d| {
                    lsp_types::Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: d.clone(),
                    })
                }),
                parameters: Some(parameters),
                active_parameter: None,
            }
        })
        .collect();

    lsp_types::SignatureHelp {
        signatures,
        active_signature: sh.active_signature.map(|i| i as u32),
        active_parameter: sh.active_parameter.map(|i| i as u32),
    }
}

pub fn handle_code_action(
    ctx: &LatencyRequestContext,
    params: CodeActionParams,
) -> Result<Option<CodeActionResponse>> {
    let _p = tracing::info_span!(
        "handle_code_action",
        uri = %params.text_document.uri
    )
    .entered();

    let uri = params.text_document.uri;
    let file_id = ctx.file_id_for_url(&uri)?;
    let doc = ctx
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    let text = doc.text();
    let line_index = doc.line_index();
    let range = crate::lsp::text_range_with_encoding(
        line_index,
        text,
        params.range,
        ctx.position_encoding,
    )?;

    let diagnostics = ctx.analysis.file_diagnostics_cached(file_id, ctx.diagnostics_config.clone());
    let only = &params.context.only;

    let mut actions = Vec::new();

    // Individual quick fixes for the diagnostics touching the requested range, plus a
    // "fix all occurrences of code X" batch (once per code) for codes with ≥2 safe fixes.
    if kind_requested(CodeActionKind::QUICKFIX.as_str(), only) {
        for diag in diagnostics.iter() {
            if diag.fixes.is_empty() || diag.range.intersect(range).is_none() {
                continue;
            }
            for fix in &diag.fixes {
                if let Some(action) = crate::lsp::to_proto::code_action_with_encoding(
                    line_index,
                    text,
                    &uri,
                    diag,
                    fix,
                    ctx.position_encoding,
                ) {
                    actions.push(CodeActionOrCommand::CodeAction(action));
                }
            }
        }

        // "Fix all occurrences of code X" — offered only for a code visible in the
        // requested range (like a normal quick fix), but merging every occurrence
        // of it in the file.
        let mut seen_codes = rustc_hash::FxHashSet::default();
        for diag in diagnostics.iter() {
            if diag.range.intersect(range).is_none() || !seen_codes.insert(diag.code) {
                continue;
            }
            if let Some(action) = fix_all_occurrences_action(
                line_index,
                text,
                &uri,
                &diagnostics,
                diag.code,
                ctx.position_encoding,
            ) {
                actions.push(CodeActionOrCommand::CodeAction(action));
            }
        }
    }

    // Whole-file `source.fixAll`: every safe fix in the file, merged without overlaps.
    // Scoped to the whole file (not the request range) as the action title promises.
    if kind_requested(crate::lsp::to_proto::FIX_ALL_BSL, only) {
        let merged = ide::batch_fixes::merge_fixes(
            diagnostics
                .iter()
                .flat_map(|diag| diag.fixes.iter())
                .filter(|fix| fix.safe_for_fix_all),
        );
        if let Some(action) = crate::lsp::to_proto::aggregate_code_action(
            line_index,
            text,
            &uri,
            "Исправить все безопасные замечания в файле".to_string(),
            CodeActionKind::new(crate::lsp::to_proto::FIX_ALL_BSL),
            &merged,
            ctx.position_encoding,
        ) {
            actions.push(CodeActionOrCommand::CodeAction(action));
        }
    }

    if actions.is_empty() {
        Ok(None)
    } else {
        Ok(Some(actions))
    }
}

/// An action kind is requested when the client sends no `only` filter, or lists a kind
/// equal to or a super-kind (dotted prefix) of it — so `source.fixAll` matches the
/// server's `source.fixAll.bsl-analyzer`.
fn kind_requested(action_kind: &str, only: &Option<Vec<CodeActionKind>>) -> bool {
    match only {
        None => true,
        Some(list) => list.iter().any(|requested| {
            let requested = requested.as_str();
            action_kind == requested || action_kind.starts_with(&format!("{requested}."))
        }),
    }
}

/// A batched quick fix that applies every safe fix of one diagnostic code in the file.
/// Emitted only when at least two such fixes exist.
fn fix_all_occurrences_action(
    line_index: &LineIndex,
    text: &str,
    uri: &lsp_types::Url,
    diagnostics: &[ide::Diagnostic],
    code: ide::DiagnosticCode,
    encoding: crate::lsp::PositionEncoding,
) -> Option<lsp_types::CodeAction> {
    let safe_fixes = || {
        diagnostics
            .iter()
            .filter(|diag| diag.code == code)
            .flat_map(|diag| diag.fixes.iter())
            .filter(|fix| fix.safe_for_fix_all && !fix.edits.is_empty())
    };
    if safe_fixes().count() < 2 {
        return None;
    }

    let merged = ide::batch_fixes::merge_fixes(safe_fixes());
    crate::lsp::to_proto::aggregate_code_action(
        line_index,
        text,
        uri,
        format!("Исправить все «{}» в файле", code.as_str()),
        CodeActionKind::QUICKFIX,
        &merged,
        encoding,
    )
}

/// Pull-model single-document diagnostics (`textDocument/diagnostic`).
///
/// Uses the same `file_diagnostics_query` the push path uses, so a pulled report is
/// identical to what the editor already shows — and unlike push it also serves closed
/// files, reading their text from the disk-backed database. The `previous_result_id`
/// fast-path returns a tiny `Unchanged` report when the diagnostics have not changed.
///
/// This is intentionally scope-independent: `WorkspaceDiagnosticsScope::Extensions`
/// governs which files the bulk `workspace/diagnostic` sweep reports, not an explicit
/// single-file request. A user who opens a base-configuration file expects its
/// diagnostics, and computing them for that one file on demand is cheap (it does not
/// trigger whole-base analysis).
pub fn handle_document_diagnostic(
    ctx: &LatencyRequestContext,
    params: lsp_types::DocumentDiagnosticParams,
) -> Result<lsp_types::DocumentDiagnosticReportResult> {
    use lsp_types::{
        DocumentDiagnosticReport, DocumentDiagnosticReportResult, FullDocumentDiagnosticReport,
        RelatedFullDocumentDiagnosticReport, RelatedUnchangedDocumentDiagnosticReport,
        UnchangedDocumentDiagnosticReport,
    };

    let uri = params.text_document.uri;
    let _p = tracing::info_span!("handle_document_diagnostic", uri = %uri).entered();

    let file_id = ctx.file_id_for_url(&uri)?;

    // An edited-but-unsaved buffer no longer matches the disk state the
    // vendor-diff scope was computed against: analyze it whole-file.
    let mut config = ctx.diagnostics_config.clone();
    if config.scope.is_some() && ctx.scope_dirty_docs.contains(&uri) {
        config.scope = None;
    }
    let baseline_applies = crate::diagnostics_baseline::applies_under_scope(config.scope.is_some());
    let ide_diagnostics = ctx.analysis.file_diagnostics_cached(file_id, config);
    let text = match ctx.mem_docs.get(&uri) {
        Some(doc) => std::borrow::Cow::Borrowed(doc.text()),
        None => std::borrow::Cow::Owned(ctx.analysis.file_text(file_id).to_string()),
    };
    let ide_diagnostics = match (
        ctx.workspace_root.as_deref().filter(|_| baseline_applies),
        uri.to_file_path().ok().as_deref(),
    ) {
        (Some(root), Some(path)) => crate::diagnostics_baseline::active_for_file(
            &ctx.diagnostics_baseline,
            root,
            path,
            &text,
            ide_diagnostics.iter().cloned().collect(),
        ),
        _ => ide_diagnostics.iter().cloned().collect(),
    };
    let result_id = crate::lsp::diagnostics_result_id(&ide_diagnostics);

    if params.previous_result_id.as_deref() == Some(result_id.as_str()) {
        return Ok(DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(
            RelatedUnchangedDocumentDiagnosticReport {
                related_documents: None,
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id,
                },
            },
        )));
    }

    // An open buffer's overlay text (and its cached line index) reflects unsaved edits;
    // a closed file has neither, so fall back to the database's disk-backed text and
    // to an index built here. Rebuilding the index for an open buffer would redo, on
    // every keystroke-driven pull, work the document already holds.
    let owned_line_index = ctx.mem_docs.get(&uri).is_none().then(|| LineIndex::new(&text));
    let line_index = match (&owned_line_index, ctx.mem_docs.get(&uri)) {
        (Some(index), _) => index,
        (None, Some(doc)) => doc.line_index(),
        (None, None) => unreachable!("an index is built exactly when the document is closed"),
    };
    let items = crate::lsp::to_proto::diagnostics_with_encoding(
        line_index,
        &text,
        &ide_diagnostics,
        ctx.position_encoding,
        crate::lsp::to_proto::CodeDescriptions::from_client_support(ctx.supports_code_description),
        ctx.supports_diagnostic_tags,
    );

    Ok(DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
        RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: Some(result_id),
                items,
            },
        },
    )))
}

/// Emits a `WorkDoneProgress` `End` for a started progress token on drop, so the client's progress
/// indicator is always closed — including when the sweep unwinds on a Salsa cancellation.
struct WorkDoneProgressGuard<'a> {
    ctx: &'a LatencyRequestContext,
    token: lsp_types::ProgressToken,
}

impl Drop for WorkDoneProgressGuard<'_> {
    fn drop(&mut self) {
        self.ctx.send_progress(
            &self.token,
            serde_json::to_value(lsp_types::WorkDoneProgress::End(
                lsp_types::WorkDoneProgressEnd { message: None },
            ))
            .unwrap_or_default(),
        );
    }
}

/// Pull-model whole-workspace diagnostics (`workspace/diagnostic`).
///
/// Reports diagnostics for every in-scope BSL file in one sweep. Scope comes from
/// `[features] workspaceDiagnostics`: `Extensions` reports only files under a
/// configuration-extension root (the base configuration stays loaded so cross-references
/// resolve, but base files are not reported); `All` reports the whole configuration.
///
/// Runs on the task pool via `on_latency`, which also provides the boot gate (declined
/// with `ContentModified` until the workspace has loaded) and cancellation: a concurrent
/// edit bumps the Salsa revision and the sweep unwinds, so the client simply re-pulls.
/// Each file's `previousResultId` drives a tiny `Unchanged` report when nothing changed.
///
/// When the client supplies a `partialResultToken`, reports are streamed in chunks via
/// `$/progress` as they are computed (so the Problems panel fills progressively instead of
/// waiting for a minutes-long `all` sweep), and the final response is an empty report. A
/// `workDoneToken` drives a work-done progress indicator over the same sweep.
pub fn handle_workspace_diagnostic(
    ctx: &LatencyRequestContext,
    params: lsp_types::WorkspaceDiagnosticParams,
) -> Result<lsp_types::WorkspaceDiagnosticReportResult> {
    use lsp_types::{
        WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressReport, WorkspaceDiagnosticReport,
        WorkspaceDiagnosticReportResult,
    };

    let _p = tracing::info_span!("handle_workspace_diagnostic").entered();

    let scope =
        ctx.project.as_ref().map(|p| p.config.features.workspace_diagnostics).unwrap_or_default();
    if !scope.is_enabled() {
        return Ok(WorkspaceDiagnosticReportResult::Report(WorkspaceDiagnosticReport::default()));
    }

    // Extension roots for scope filtering. For `Extensions`, only files under one of these
    // are reported; `All` reports every BSL file.
    let ext_roots: Vec<&std::path::Path> = ctx
        .project
        .as_ref()
        .map(|p| p.extension_paths().iter().map(|(_, path)| path.as_path()).collect())
        .unwrap_or_default();

    // Vendor-diff file-gate: unchanged-vs-base files are excluded up front so the
    // sweep does not walk thousands of files whose report is guaranteed empty.
    // Dirty open documents no longer match the disk-derived scope: they stay in
    // the sweep regardless of the gate and are analyzed whole-file below.
    let analysis_scope = ctx.diagnostics_config.scope.clone();
    let dirty_ids: std::collections::HashSet<vfs::FileId> = if analysis_scope.is_some() {
        ctx.scope_dirty_docs.iter().filter_map(|uri| ctx.file_id_for_url(uri).ok()).collect()
    } else {
        Default::default()
    };
    let mut file_ids: Vec<vfs::FileId> = ctx
        .file_paths
        .iter()
        .filter(|(file_id, path)| {
            path_in_workspace_scope(path, scope, &ext_roots)
                && (dirty_ids.contains(file_id)
                    || analysis_scope.as_ref().is_none_or(|s| s.is_file_in_scope(path)))
        })
        .map(|(file_id, _)| file_id)
        .collect();
    // Stable report order across pulls (protocol-agnostic, but keeps diffs/logs sane).
    file_ids.sort_by_key(|f| f.0);

    let previous: FxHashMap<lsp_types::Url, String> =
        params.previous_result_ids.into_iter().map(|p| (p.uri, p.value)).collect();

    let total = file_ids.len();
    tracing::info!(scope = ?scope, files = total, "workspace diagnostics sweep");

    let partial_token = params.partial_result_params.partial_result_token;
    let work_done_token = params.work_done_progress_params.work_done_token;

    // The client created the work-done token, so the server drives it directly (no create
    // round-trip). The guard sends `End` on drop — including a Salsa-cancellation unwind — so the
    // client's progress indicator never stays stuck if the sweep is aborted mid-flight.
    let _work_done_guard = work_done_token.as_ref().map(|token| {
        ctx.send_progress(
            token,
            serde_json::to_value(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "Workspace diagnostics".to_string(),
                cancellable: Some(false),
                message: Some(format!("0/{total}")),
                percentage: Some(0),
            }))
            .unwrap_or_default(),
        );
        WorkDoneProgressGuard { ctx, token: token.clone() }
    });

    // Compute in chunks so streaming clients see progress and the heavy per-file memos fall out
    // of the Salsa LRU between chunks instead of all piling up at once.
    const CHUNK: usize = 64;
    let mut collected = Vec::new();
    let mut done = 0usize;
    for chunk in file_ids.chunks(CHUNK) {
        let computed = if dirty_ids.is_empty() {
            ctx.analysis.workspace_diagnostics(chunk, ctx.diagnostics_config.clone())
        } else {
            // Dirty buffers analyze without the scope (their disk hunks are
            // stale); everything else keeps the filtered config.
            let (dirty, clean): (Vec<vfs::FileId>, Vec<vfs::FileId>) =
                chunk.iter().copied().partition(|file_id| dirty_ids.contains(file_id));
            let mut computed =
                ctx.analysis.workspace_diagnostics(&clean, ctx.diagnostics_config.clone());
            let mut unscoped = ctx.diagnostics_config.clone();
            unscoped.scope = None;
            computed.extend(ctx.analysis.workspace_diagnostics(&dirty, unscoped));
            computed
        };
        let chunk_items: Vec<_> = computed
            .into_iter()
            .filter_map(|(file_id, diagnostics)| {
                workspace_report_item(ctx, file_id, diagnostics.to_vec(), &previous)
            })
            .collect();
        done += chunk.len();

        if let Some(token) = &partial_token {
            ctx.send_progress(token, serde_json::json!({ "items": chunk_items }));
        } else {
            collected.extend(chunk_items);
        }
        if let Some(token) = &work_done_token {
            let percentage = ((done as f64 / total.max(1) as f64) * 100.0) as u32;
            ctx.send_progress(
                token,
                serde_json::to_value(WorkDoneProgress::Report(WorkDoneProgressReport {
                    cancellable: Some(false),
                    message: Some(format!("{done}/{total}")),
                    percentage: Some(percentage),
                }))
                .unwrap_or_default(),
            );
        }
    }

    // `End` is emitted by `_work_done_guard` on drop (here and on a cancellation unwind alike).

    // Streamed reports were already delivered via `$/progress`; the response is then an empty
    // report. Without a partial-result token everything is returned in the response instead.
    let items = if partial_token.is_some() { Vec::new() } else { collected };
    Ok(WorkspaceDiagnosticReportResult::Report(WorkspaceDiagnosticReport { items }))
}

/// Builds one `workspace/diagnostic` report entry for a computed file: an `Unchanged` stub when
/// the client's `previousResultId` still matches, otherwise a `Full` report. Returns `None` only
/// when the file id has no resolvable URL (it was removed from the frozen snapshot).
fn workspace_report_item(
    ctx: &LatencyRequestContext,
    file_id: vfs::FileId,
    diagnostics: Vec<ide::Diagnostic>,
    previous: &FxHashMap<lsp_types::Url, String>,
) -> Option<lsp_types::WorkspaceDocumentDiagnosticReport> {
    use lsp_types::{
        FullDocumentDiagnosticReport, UnchangedDocumentDiagnosticReport,
        WorkspaceDocumentDiagnosticReport, WorkspaceFullDocumentDiagnosticReport,
        WorkspaceUnchangedDocumentDiagnosticReport,
    };

    let url = ctx.url_for_file_id(file_id).ok()?;
    // The LSP spec wants the buffer version for an open document (and `None` only for a closed
    // one), so a client can associate or discard the report against its buffer.
    let open_doc = ctx.mem_docs.get(&url);
    let version = open_doc.map(|doc| doc.version() as i64);

    // The baseline decides which diagnostics are active, so the result id — and with it
    // the "unchanged" verdict — can only be computed after it. Conversion to LSP items
    // is the expensive half and stays behind that verdict: an unchanged document must
    // not pay for a report the client already has.
    //
    // An open buffer's overlay text (and cached line index) reflects unsaved edits; a closed
    // file has neither, so fall back to the database's disk-backed text.
    //
    // A closed file's disk-backed text read (and conversion) can panic if the file was
    // deleted/rewritten mid-sweep. Catch it so one racing file does not fail the whole
    // request (and, when streaming, discard already-sent chunks); a `salsa::Cancelled`
    // is a real abort and must keep unwinding.
    // When the baseline cannot change the set, the result id is known without the file
    // text — and an unchanged document then costs neither a read nor an index. The
    // whole-config sweep goes through here once per file, so that read is not free.
    // The baseline is not applied under an analysis scope — see `applies_under_scope`.
    let baseline_applies = ctx.diagnostics_baseline.affects_diagnostics()
        && crate::diagnostics_baseline::applies_under_scope(ctx.diagnostics_config.scope.is_some());
    if !baseline_applies {
        let result_id = crate::lsp::diagnostics_result_id(&diagnostics);
        if previous.get(&url).map(String::as_str) == Some(result_id.as_str()) {
            return Some(WorkspaceDocumentDiagnosticReport::Unchanged(
                WorkspaceUnchangedDocumentDiagnosticReport {
                    uri: url,
                    version,
                    unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                        result_id,
                    },
                },
            ));
        }
    }

    let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let text = match open_doc {
            Some(doc) => std::borrow::Cow::Borrowed(doc.text()),
            None => std::borrow::Cow::Owned(ctx.analysis.file_text(file_id).to_string()),
        };
        let diagnostics = match (
            ctx.workspace_root.as_deref().filter(|_| baseline_applies),
            ctx.file_paths.path_for_file_id(file_id),
        ) {
            (Some(root), Some(path)) => crate::diagnostics_baseline::active_for_file(
                &ctx.diagnostics_baseline,
                root,
                path,
                &text,
                diagnostics,
            ),
            _ => diagnostics,
        };
        let result_id = crate::lsp::diagnostics_result_id(&diagnostics);
        if previous.get(&url).map(String::as_str) == Some(result_id.as_str()) {
            return (result_id, None);
        }
        let owned_line_index = open_doc.is_none().then(|| LineIndex::new(&text));
        let line_index = match (&owned_line_index, open_doc) {
            (Some(index), _) => index,
            (None, Some(doc)) => doc.line_index(),
            (None, None) => unreachable!("an index is built exactly when the document is closed"),
        };
        let lsp = crate::lsp::to_proto::diagnostics_with_encoding(
            line_index,
            &text,
            &diagnostics,
            ctx.position_encoding,
            crate::lsp::to_proto::CodeDescriptions::from_client_support(
                ctx.supports_code_description,
            ),
            ctx.supports_diagnostic_tags,
        );
        (result_id, Some(lsp))
    }));
    let (result_id, lsp_items) = match report {
        Ok(report) => report,
        Err(payload) if payload.is::<salsa::Cancelled>() => std::panic::resume_unwind(payload),
        Err(_) => {
            tracing::warn!(
                file_id = file_id.0,
                "workspace diagnostics: skipping file after a text-read panic"
            );
            return None;
        }
    };
    let Some(lsp_items) = lsp_items else {
        return Some(WorkspaceDocumentDiagnosticReport::Unchanged(
            WorkspaceUnchangedDocumentDiagnosticReport {
                uri: url,
                version,
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id,
                },
            },
        ));
    };

    Some(WorkspaceDocumentDiagnosticReport::Full(WorkspaceFullDocumentDiagnosticReport {
        uri: url,
        version,
        full_document_diagnostic_report: FullDocumentDiagnosticReport {
            result_id: Some(result_id),
            items: lsp_items,
        },
    }))
}

/// Whether a file belongs to the `workspace/diagnostic` sweep for the given scope.
/// Only BSL source files are ever in scope; `Extensions` additionally requires the file
/// to live under one of the configuration-extension roots.
pub(crate) fn path_in_workspace_scope(
    path: &std::path::Path,
    scope: project_model::WorkspaceDiagnosticsScope,
    ext_roots: &[&std::path::Path],
) -> bool {
    use project_model::WorkspaceDiagnosticsScope as S;
    if !project_model::is_bsl_source_path(path) {
        return false;
    }
    match scope {
        S::All => true,
        S::Extensions => ext_roots.iter().any(|root| path.starts_with(root)),
        S::Off => false,
    }
}

fn convert_document_symbol(
    line_index: &LineIndex,
    text: &str,
    sym: ide::DocumentSymbol,
    encoding: crate::lsp::PositionEncoding,
) -> Option<lsp_types::DocumentSymbol> {
    let range = crate::lsp::range_with_encoding(line_index, text, sym.range, encoding)?;
    let selection_range =
        crate::lsp::range_with_encoding(line_index, text, sym.selection_range, encoding)?;

    let kind = match sym.kind() {
        ide::SymbolKind::Procedure | ide::SymbolKind::Function => lsp_types::SymbolKind::FUNCTION,
        ide::SymbolKind::Variable => lsp_types::SymbolKind::VARIABLE,
        ide::SymbolKind::Region => lsp_types::SymbolKind::NAMESPACE,
    };

    let children = if sym.children.is_empty() {
        None
    } else {
        Some(convert_document_symbols(line_index, text, sym.children, encoding)?)
    };

    #[allow(deprecated)]
    Some(lsp_types::DocumentSymbol {
        name: sym.name,
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children,
    })
}

/// The whole map or nothing.
///
/// A node whose range does not project onto the buffer means the analysis snapshot and the
/// open document have drifted apart. Dropping just that node — and, since children hang off
/// it, its whole subtree — leaves a map that reads as the truth about the file: an empty
/// region looks like an empty region, a missing method looks like a method that is not
/// there. `documentSymbol` has no field to mark an answer partial, so the only honest
/// answers are the whole map or a refusal, and a refusal is self-healing: the client asks
/// again after the next edit.
fn convert_document_symbols(
    line_index: &LineIndex,
    text: &str,
    symbols: Vec<ide::DocumentSymbol>,
    encoding: crate::lsp::PositionEncoding,
) -> Option<Vec<lsp_types::DocumentSymbol>> {
    symbols
        .into_iter()
        .map(|symbol| convert_document_symbol(line_index, text, symbol, encoding))
        .collect()
}

/// The `documentSymbol` answer: no symbols, the whole map, or a refusal naming the file.
///
/// Split from the handler so the choice between those three is testable without an LSP
/// context — it is the part with a decision in it, and the handler around it is wiring.
fn document_symbol_response(
    line_index: &LineIndex,
    text: &str,
    symbols: Vec<ide::DocumentSymbol>,
    encoding: crate::lsp::PositionEncoding,
    uri: &lsp_types::Url,
) -> Result<Option<DocumentSymbolResponse>> {
    if symbols.is_empty() {
        return Ok(None);
    }

    match convert_document_symbols(line_index, text, symbols, encoding) {
        Some(converted) => Ok(Some(DocumentSymbolResponse::Nested(converted))),
        None => {
            tracing::warn!(
                uri = %uri,
                "document symbol range does not project onto the open buffer; \
                 the analysis snapshot and the document have drifted apart",
            );
            Err(anyhow::anyhow!(
                "document symbols for {uri} could not be projected onto the current buffer"
            ))
        }
    }
}

struct ReferenceLocationConverter<'ctx> {
    ctx: &'ctx LatencyRequestContext,
    /// The requested document's text, already in hand and not worth re-reading.
    /// Absent for a request that has no document of its own — a workspace-wide
    /// search answers about files the caller never named.
    source: Option<(FileId, String)>,
    target_files: FxHashMap<FileId, ReferenceTargetFile>,
}

struct ReferenceTargetFile {
    uri: lsp_types::Url,
    text: String,
    line_index: LineIndex,
}

impl<'ctx> ReferenceLocationConverter<'ctx> {
    fn new(ctx: &'ctx LatencyRequestContext, source_file_id: FileId, source_text: &str) -> Self {
        Self {
            ctx,
            source: Some((source_file_id, source_text.to_string())),
            target_files: FxHashMap::default(),
        }
    }

    /// A converter for a request with no document of its own: every file it
    /// touches is read through the overlay-or-disk path like any other target.
    fn detached(ctx: &'ctx LatencyRequestContext) -> Self {
        Self { ctx, source: None, target_files: FxHashMap::default() }
    }

    fn convert(&mut self, ide_loc: IdeLocation) -> Result<Location> {
        let encoding = self.ctx.position_encoding;
        let target = self.target_file(ide_loc.file_id)?;
        let range = crate::lsp::range_with_encoding(
            &target.line_index,
            &target.text,
            ide_loc.range,
            encoding,
        )
        .ok_or_else(|| anyhow::anyhow!("Failed to convert reference range"))?;
        Ok(Location { uri: target.uri.clone(), range })
    }

    fn target_file(&mut self, file_id: FileId) -> Result<&ReferenceTargetFile> {
        if !self.target_files.contains_key(&file_id) {
            let uri = self.ctx.url_for_file_id(file_id)?;
            let text = if let Some((_, text)) =
                self.source.as_ref().filter(|(source_id, _)| *source_id == file_id)
            {
                text.clone()
            } else if let Some(doc) = self.ctx.mem_docs.get(&uri) {
                doc.text().to_string()
            } else {
                self.ctx.analysis.file_text(file_id)
            };
            let line_index = LineIndex::new(&text);
            self.target_files.insert(file_id, ReferenceTargetFile { uri, text, line_index });
        }

        Ok(self
            .target_files
            .get(&file_id)
            .expect("reference target file must be cached after insertion"))
    }
}

fn convert_document_highlight_kind(kind: IdeDocumentHighlightKind) -> LspDocumentHighlightKind {
    match kind {
        IdeDocumentHighlightKind::Text => LspDocumentHighlightKind::TEXT,
        IdeDocumentHighlightKind::Read => LspDocumentHighlightKind::READ,
        IdeDocumentHighlightKind::Write => LspDocumentHighlightKind::WRITE,
    }
}

fn convert_folding_range_kind(kind: IdeFoldingRangeKind) -> LspFoldingRangeKind {
    match kind {
        IdeFoldingRangeKind::Region => LspFoldingRangeKind::Region,
        IdeFoldingRangeKind::Comment => LspFoldingRangeKind::Comment,
    }
}

fn convert_completion_item(item: ide::CompletionItem, adjust_indentation: bool) -> CompletionItem {
    let has_snippet = item.insert_text.contains('$');
    // Multi-line snippet bodies carry only relative (tab) nesting. Ask capable
    // clients to adjust the continuation lines' leading whitespace to the cursor
    // column so the block isn't flushed to column 0. Single-line snippets and
    // plain text need no adjustment.
    let insert_text_mode = if adjust_indentation && has_snippet && item.insert_text.contains('\n') {
        Some(lsp_types::InsertTextMode::ADJUST_INDENTATION)
    } else {
        None
    };
    CompletionItem {
        label: item.label,
        detail: item.detail,
        kind: Some(convert_completion_kind(item.kind)),
        insert_text: Some(item.insert_text),
        insert_text_format: if has_snippet {
            Some(lsp_types::InsertTextFormat::SNIPPET)
        } else {
            None
        },
        insert_text_mode,
        documentation: item.documentation.map(lsp_types::Documentation::String),
        sort_text: item.sort_text,
        filter_text: item.filter_text,
        label_details: item
            .source
            .map(|s| lsp_types::CompletionItemLabelDetails { detail: None, description: Some(s) }),
        ..Default::default()
    }
}

fn convert_completion_kind(kind: ide::CompletionItemKind) -> CompletionItemKind {
    match kind {
        ide::CompletionItemKind::MdoType => CompletionItemKind::CLASS,
        ide::CompletionItemKind::MdoObject => CompletionItemKind::MODULE,
        ide::CompletionItemKind::Field => CompletionItemKind::FIELD,
        ide::CompletionItemKind::Property => CompletionItemKind::PROPERTY,
        ide::CompletionItemKind::Function => CompletionItemKind::FUNCTION,
        ide::CompletionItemKind::Method => CompletionItemKind::METHOD,
        ide::CompletionItemKind::Keyword => CompletionItemKind::KEYWORD,
        ide::CompletionItemKind::Constant => CompletionItemKind::CONSTANT,
        ide::CompletionItemKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
        ide::CompletionItemKind::Constructor => CompletionItemKind::CONSTRUCTOR,
        ide::CompletionItemKind::Snippet => CompletionItemKind::SNIPPET,
    }
}

pub fn handle_formatting(
    snap: GlobalStateSnapshot,
    params: lsp_types::DocumentFormattingParams,
) -> Result<Option<Vec<lsp_types::TextEdit>>> {
    let _p = tracing::info_span!(
        "handle_formatting",
        uri = %params.text_document.uri
    )
    .entered();

    let uri = params.text_document.uri;

    let file_id = crate::lsp::file_id_snapshot(&snap, &uri)?;

    let text = snap
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;

    let line_index = LineIndex::new(&text);

    let config = formatting_config_from_options(&params.options);

    let result = snap.analysis.format_file(file_id, &config);
    tracing::debug!("format_file: {} edits", result.edits.len());

    if result.edits.is_empty() {
        return Ok(None);
    }

    let lsp_edits: Vec<lsp_types::TextEdit> = result
        .edits
        .into_iter()
        .filter_map(|edit| {
            let range = crate::lsp::range_with_encoding(
                &line_index,
                &text,
                edit.range,
                snap.position_encoding,
            )?;
            tracing::trace!("edit {:?} → {:?}", edit.range, truncate_edit_preview(&edit.new_text));
            Some(lsp_types::TextEdit { range, new_text: edit.new_text })
        })
        .collect();

    if lsp_edits.is_empty() {
        Ok(None)
    } else {
        Ok(Some(lsp_edits))
    }
}

fn truncate_edit_preview(s: &str) -> String {
    let escaped: String = s
        .chars()
        .map(|c| match c {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            c => c.to_string(),
        })
        .collect();
    if escaped.chars().count() > 60 {
        let head: String = escaped.chars().take(57).collect();
        format!("{}...", head)
    } else {
        escaped
    }
}

pub fn handle_range_formatting(
    snap: GlobalStateSnapshot,
    params: lsp_types::DocumentRangeFormattingParams,
) -> Result<Option<Vec<lsp_types::TextEdit>>> {
    let _p = tracing::info_span!(
        "handle_range_formatting",
        uri = %params.text_document.uri
    )
    .entered();

    let total_start = std::time::Instant::now();

    let uri = params.text_document.uri;

    let start = std::time::Instant::now();
    let file_id = crate::lsp::file_id_snapshot(&snap, &uri)?;
    tracing::debug!("file_id_snapshot: {:?}", start.elapsed());

    let start = std::time::Instant::now();
    let text = snap
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;
    tracing::debug!("mem_docs.get: {:?}, text len: {}", start.elapsed(), text.len());

    let start = std::time::Instant::now();
    let line_index = LineIndex::new(&text);
    tracing::debug!("LineIndex::new: {:?}", start.elapsed());

    let start = std::time::Instant::now();
    let range = crate::lsp::text_range_with_encoding(
        &line_index,
        &text,
        params.range,
        snap.position_encoding,
    )?;
    tracing::debug!("text_range conversion: {:?}, range: {:?}", start.elapsed(), range);

    let config = formatting_config_from_options(&params.options);

    let start = std::time::Instant::now();
    let result = snap.analysis.format_range(file_id, range, &config);
    tracing::debug!("format_range: {:?}, edits: {}", start.elapsed(), result.edits.len());

    if result.edits.is_empty() {
        tracing::debug!("total time (no edits): {:?}", total_start.elapsed());
        return Ok(None);
    }

    let start = std::time::Instant::now();
    let lsp_edits: Vec<lsp_types::TextEdit> = result
        .edits
        .into_iter()
        .filter_map(|edit| {
            let range = crate::lsp::range_with_encoding(
                &line_index,
                &text,
                edit.range,
                snap.position_encoding,
            )?;
            tracing::trace!("edit {:?} → {:?}", edit.range, truncate_edit_preview(&edit.new_text));
            Some(lsp_types::TextEdit { range, new_text: edit.new_text })
        })
        .collect();
    tracing::debug!("convert edits: {:?}", start.elapsed());

    tracing::debug!("range_formatting total: {:?}", total_start.elapsed());

    if lsp_edits.is_empty() {
        Ok(None)
    } else {
        Ok(Some(lsp_edits))
    }
}

pub fn handle_on_type_formatting(
    snap: GlobalStateSnapshot,
    params: lsp_types::DocumentOnTypeFormattingParams,
) -> Result<Option<Vec<lsp_types::TextEdit>>> {
    let _p = tracing::info_span!(
        "handle_on_type_formatting",
        uri = %params.text_document_position.text_document.uri
    )
    .entered();

    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;

    let file_id = crate::lsp::file_id_snapshot(&snap, &uri)?;

    let text = snap
        .mem_docs
        .get(&uri)
        .ok_or_else(|| anyhow::anyhow!("Document not in MemDocs: {}", uri))?;

    let line_index = LineIndex::new(&text);

    let offset = match crate::lsp::offset_with_encoding(
        &line_index,
        &text,
        position,
        snap.position_encoding,
    ) {
        Ok(o) => o,
        Err(_) => {
            tracing::debug!("Position out of bounds, likely race with didChange - returning empty");
            return Ok(None);
        }
    };

    let char_typed = params.ch.chars().next().unwrap_or('\0');

    let config = formatting_config_from_options(&params.options);

    let edits = snap.analysis.on_type_formatting(file_id, offset.into(), char_typed, &config);

    match edits {
        Some(ide_edits) => {
            let lsp_edits: Vec<lsp_types::TextEdit> = ide_edits
                .into_iter()
                .filter_map(|edit| {
                    let range = crate::lsp::range_with_encoding(
                        &line_index,
                        &text,
                        edit.range,
                        snap.position_encoding,
                    )?;
                    Some(lsp_types::TextEdit { range, new_text: edit.new_text })
                })
                .collect();

            if lsp_edits.is_empty() {
                Ok(None)
            } else {
                Ok(Some(lsp_edits))
            }
        }
        None => Ok(None),
    }
}

fn formatting_config_from_options(options: &lsp_types::FormattingOptions) -> ide::FormattingConfig {
    ide::FormattingConfig {
        use_tabs: !options.insert_spaces,
        indent_size: if options.insert_spaces { options.tab_size } else { 1 },
        continuation_indent: 1,
        space_after_comma: true,
        space_around_assignment: true,
        space_around_binary_ops: true,
        trim_trailing_whitespace: options.trim_trailing_whitespace.unwrap_or(true),
        insert_final_newline: options.insert_final_newline.unwrap_or(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use hir::{CallHierarchyReverseIndex, Definition, MethodCallPair, Semantics};
    use lsp_types::request::{CallHierarchyIncomingCalls, Request as _};
    use lsp_types::{Position, TextDocumentIdentifier, TextDocumentPositionParams};
    use std::sync::Arc;
    use vfs::VfsPath;

    use crate::frozen_context::{FrozenFilePaths, LatencyRequestContext};
    use crate::global_state::GlobalState;
    use crate::handlers::RequestDispatcher;
    use crate::task_pool::TaskPool;

    mod call_hierarchy_trace_tests;
    mod inlay_hint_tests;
    mod position_race_tests;

    /// How long a stand waits for a thread to reach a parked or published state before
    /// calling the test hung. Scheduling latency on a loaded machine is the only thing
    /// it absorbs, so it is generous and nothing is measured against it.
    const PARK_HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(30);

    /// A symbol whose range does not project onto the open buffer is the one case where a
    /// map can lie without looking wrong: the answer stays well-formed while a method — or
    /// a whole region's contents — silently vanishes from it. These four inputs pin the
    /// three answers the request may give, and the second one is the case the old
    /// `filter_map` swallowed.
    mod document_symbol_response_tests {
        use super::super::document_symbol_response;
        use crate::lsp::PositionEncoding;
        use ide::{DocumentSymbol, MethodDetail, SymbolDetail, TextRange};
        use line_index::LineIndex;

        const SOURCE: &str = "Процедура П()\nКонецПроцедуры";

        fn method(
            name: &str,
            start: u32,
            end: u32,
            children: Vec<DocumentSymbol>,
        ) -> DocumentSymbol {
            DocumentSymbol {
                name: name.to_string(),
                range: TextRange::new(start.into(), end.into()),
                selection_range: TextRange::new(start.into(), (start + 2).into()),
                detail: SymbolDetail::Procedure(MethodDetail {
                    is_export: false,
                    directives: Vec::new(),
                    params: Vec::new(),
                }),
                children,
            }
        }

        fn answer(
            symbols: Vec<DocumentSymbol>,
        ) -> anyhow::Result<Option<lsp_types::DocumentSymbolResponse>> {
            let line_index = LineIndex::new(SOURCE);
            let uri = crate::test_uri::file_uri("ws/Module.bsl");
            document_symbol_response(&line_index, SOURCE, symbols, PositionEncoding::Utf16, &uri)
        }

        /// Past the end of the buffer: the offsets belong to a different text than the one
        /// open, so nothing about this file can be answered.
        const PAST_END: u32 = 10_000;

        #[test]
        fn a_root_symbol_that_does_not_project_refuses_instead_of_vanishing() {
            assert!(answer(vec![method("П", PAST_END, PAST_END + 20, Vec::new())]).is_err());
        }

        #[test]
        fn a_child_that_does_not_project_refuses_instead_of_vanishing() {
            let parent =
                method("П", 0, 27, vec![method("Внутри", PAST_END, PAST_END + 20, Vec::new())]);

            assert!(
                answer(vec![parent]).is_err(),
                "a map missing the child reads as a file without it",
            );
        }

        #[test]
        fn a_map_that_projects_whole_is_served_whole() {
            let parent = method("П", 0, 27, vec![method("Вложенный", 0, 13, Vec::new())]);

            let served = answer(vec![parent]).expect("projects").expect("not empty");
            let lsp_types::DocumentSymbolResponse::Nested(roots) = served else {
                panic!("nested response expected");
            };
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].children.as_ref().map(Vec::len), Some(1));
        }

        #[test]
        fn an_empty_map_is_an_empty_answer_and_not_a_refusal() {
            assert!(answer(Vec::new()).expect("no symbols is not a failure").is_none());
        }
    }

    #[test]
    fn workspace_scope_filters_by_extension_root() {
        use project_model::WorkspaceDiagnosticsScope::{All, Extensions, Off};
        use std::path::Path;

        let ext_root = Path::new("/proj/src/cfe/ExtA");
        let ext_roots = [ext_root];
        let ext_file = Path::new("/proj/src/cfe/ExtA/CommonModules/M/Ext/Module.bsl");
        let base_file = Path::new("/proj/src/cf/CommonModules/M/Ext/Module.bsl");
        let xml_file = Path::new("/proj/src/cfe/ExtA/Configuration.xml");

        // Extensions: only BSL files under an extension root.
        assert!(path_in_workspace_scope(ext_file, Extensions, &ext_roots));
        assert!(!path_in_workspace_scope(base_file, Extensions, &ext_roots));
        assert!(!path_in_workspace_scope(xml_file, Extensions, &ext_roots), "non-BSL excluded");

        // All: every BSL file, extension or base.
        assert!(path_in_workspace_scope(ext_file, All, &ext_roots));
        assert!(path_in_workspace_scope(base_file, All, &ext_roots));
        assert!(!path_in_workspace_scope(xml_file, All, &ext_roots), "non-BSL still excluded");

        // Off: nothing (defensive — the handler returns early before this).
        assert!(!path_in_workspace_scope(ext_file, Off, &ext_roots));
    }

    fn snippet_item(insert_text: &str) -> ide::CompletionItem {
        ide::CompletionItem {
            label: "Если".into(),
            detail: None,
            kind: ide::CompletionItemKind::Snippet,
            insert_text: insert_text.into(),
            documentation: None,
            sort_text: None,
            filter_text: None,
            source: None,
        }
    }

    #[test]
    fn multiline_snippet_requests_adjust_indentation_when_supported() {
        let item =
            convert_completion_item(snippet_item("Если ${1:Усл} Тогда\n\t$0\nКонецЕсли;"), true);
        assert_eq!(item.insert_text_format, Some(lsp_types::InsertTextFormat::SNIPPET));
        assert_eq!(item.insert_text_mode, Some(lsp_types::InsertTextMode::ADJUST_INDENTATION));
    }

    #[test]
    fn multiline_snippet_omits_mode_when_unsupported() {
        let item =
            convert_completion_item(snippet_item("Если ${1:Усл} Тогда\n\t$0\nКонецЕсли;"), false);
        assert_eq!(item.insert_text_mode, None);
    }

    #[test]
    fn single_line_snippet_never_sets_mode() {
        let item = convert_completion_item(snippet_item("ВызватьИсключение;$0"), true);
        assert_eq!(item.insert_text_mode, None);
    }

    fn create_test_state() -> GlobalState {
        let (sender, _receiver) = unbounded();
        GlobalState::new(sender)
    }

    fn latency_ctx(state: &GlobalState) -> LatencyRequestContext {
        LatencyRequestContext {
            analysis: state.analysis_host.analysis(),
            workspace_root: state.workspace_root.clone(),
            project: state.project.clone(),
            supports_code_description: state.supports_code_description,
            supports_diagnostic_tags: state.supports_diagnostic_tags,
            diagnostics_baseline: std::sync::Arc::clone(&state.diagnostics_baseline),
            diagnostics_config: state.diagnostics_config.clone(),
            position_encoding: state.position_encoding,
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
            file_paths: FrozenFilePaths::freeze(&state.vfs.read()),
            scope_dirty_docs: state.scope_dirty_docs.clone(),
        }
    }

    fn latency_ctx_with_token(
        state: &GlobalState,
    ) -> (LatencyRequestContext, salsa::CancellationToken) {
        use salsa::Database as _;

        let db = state.analysis_host.raw_database().clone();
        let token = db.cancellation_token();
        let ctx = LatencyRequestContext {
            analysis: ide::Analysis::from_database(db),
            workspace_root: state.workspace_root.clone(),
            project: state.project.clone(),
            supports_code_description: state.supports_code_description,
            supports_diagnostic_tags: state.supports_diagnostic_tags,
            diagnostics_baseline: std::sync::Arc::clone(&state.diagnostics_baseline),
            diagnostics_config: state.diagnostics_config.clone(),
            position_encoding: state.position_encoding,
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
            file_paths: FrozenFilePaths::freeze(&state.vfs.read()),
            scope_dirty_docs: state.scope_dirty_docs.clone(),
        };
        (ctx, token)
    }

    #[test]
    fn hover_after_an_excluded_did_open_returns_an_error_without_creating_a_file() {
        let mut state = create_test_state();
        state.init_empty_source_root();
        let dir = tempfile::tempdir().unwrap();
        let hidden_dir = dir.path().join("generated");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        let hidden = hidden_dir.join("Hidden.bsl");
        let uri = lsp_types::Url::from_file_path(&hidden).unwrap();
        state.source_exclusions = project_model::ExcludedPaths::new([hidden_dir]);

        crate::handlers::handle_did_open(
            &mut state,
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "bsl".to_owned(),
                    version: 1,
                    text: "Процедура Скрытая()\nКонецПроцедуры".to_owned(),
                },
            },
        )
        .unwrap();
        let ctx = latency_ctx(&state);
        let result = handle_hover(
            &ctx,
            lsp_types::HoverParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    position: lsp_types::Position::new(0, 0),
                },
                work_done_progress_params: Default::default(),
            },
        );

        assert!(result.is_err(), "an absent excluded document produced a hover result");
        assert!(state.vfs.read().file_id(&vfs::VfsPath::new(hidden)).is_none());
        assert!(!state.request_tokens.values().any(|token| !token.is_cancelled()));
    }

    #[test]
    fn test_goto_definition_not_found() {
        let mut state = create_test_state();

        let uri = crate::test_uri::file_uri("test.bsl");

        state.mem_docs.insert(uri.clone(), "Процедура Тест() КонецПроцедуры".to_string(), 1);

        let ctx = latency_ctx(&state);

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line: 0, character: 0 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = handle_goto_definition(&ctx, params);
        assert!(result.is_err() || result.unwrap().is_none());
    }

    #[test]
    fn goto_definition_context_frozen_against_main_thread_mutation() {
        let mut state = create_test_state();

        let uri = crate::test_uri::file_uri("frozen.bsl");
        state.mem_docs.insert(uri.clone(), "original".to_string(), 1);

        let ctx = latency_ctx(&state);

        state.mem_docs.update(
            &uri,
            vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "rewritten".to_string(),
            }],
        );

        let doc = ctx.mem_docs.get(&uri).expect("document must be in frozen view");
        assert_eq!(doc.text(), "original");
        assert_eq!(doc.version(), 1);

        assert_eq!(state.mem_docs.get(&uri).as_deref(), Some("rewritten"));
    }

    #[test]
    fn test_find_references_empty() {
        let mut state = create_test_state();

        let uri = crate::test_uri::file_uri("test.bsl");

        state.mem_docs.insert(uri.clone(), "Процедура Тест() КонецПроцедуры".to_string(), 1);

        let ctx = latency_ctx(&state);

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line: 0, character: 0 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_types::ReferenceContext { include_declaration: true },
        };

        let result = handle_find_references(&ctx, params);
        assert!(result.is_err() || result.unwrap().is_none());
    }

    fn open_source(state: &mut GlobalState, uri: &lsp_types::Url, source: &str) {
        state.mem_docs.insert(uri.clone(), source.to_string(), 1);
        let open_file_id = state.vfs_file_for_url(uri).unwrap();
        state.open_files.insert(open_file_id);
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                VfsPath::new(uri.to_file_path().unwrap()),
                Some(Arc::from(source)),
            );
        }
        state.process_changes(false);
    }

    struct CallHierarchyFixture<'a> {
        uri: &'a lsp_types::Url,
        source: &'a str,
    }

    fn method_id_at(
        state: &mut GlobalState,
        fixture: &CallHierarchyFixture<'_>,
        name: &str,
    ) -> hir::MethodId {
        let file_id = state.vfs_file_for_url(fixture.uri).expect("source file is registered");
        let offset = line_index::TextSize::from(
            fixture.source.find(name).expect("method name is present") as u32,
        );
        let db = state.analysis_host.raw_database();
        let definition = Semantics::new(db)
            .symbol_at(file_id, offset)
            .and_then(|symbol| symbol.definition)
            .expect("method name resolves");
        match definition {
            Definition::Method(id) => id,
            _ => panic!("method name must resolve to a method"),
        }
    }

    fn build_call_hierarchy_index(
        state: &mut GlobalState,
        fixture: &CallHierarchyFixture<'_>,
        caller_names: &[&str],
    ) -> (base_db::SourceRootId, Arc<CallHierarchyReverseIndex>) {
        let target = method_id_at(state, fixture, "Помощник");
        let mut index = CallHierarchyReverseIndex::new();
        index.replace_module(
            target.module,
            caller_names
                .iter()
                .map(|name| MethodCallPair::new(method_id_at(state, fixture, name), target)),
            0,
        );

        let file_id = state.vfs_file_for_url(fixture.uri).expect("source file is registered");
        let db = state.analysis_host.raw_database();
        let source_root = db.file_source_root_input(file_id).source_root_id(db);
        (source_root, Arc::new(index))
    }

    fn publish_call_hierarchy_index(
        state: &mut GlobalState,
        fixture: &CallHierarchyFixture<'_>,
        caller_names: &[&str],
    ) -> base_db::SourceRootId {
        let (source_root, index) = build_call_hierarchy_index(state, fixture, caller_names);
        let generation = state
            .call_hierarchy_index
            .next_generation(source_root)
            .expect("generation counter does not overflow");
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            generation,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(generation),
        ));
        assert!(state.call_hierarchy_index.publish(source_root, generation, index));
        source_root
    }

    #[test]
    fn prepare_rename_reports_placeholder_for_local_variable() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("rename.bsl");
        let source =
            "Процедура Тест()\n    Перем МояПеременная;\n    МояПеременная = 10;\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        let ctx = latency_ctx(&state);
        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: Position { line: 1, character: 10 },
        };

        let result = handle_prepare_rename(&ctx, params).unwrap().unwrap();
        match result {
            PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => {
                assert_eq!(placeholder, "МояПеременная");
            }
            other => panic!("expected RangeWithPlaceholder, got {other:?}"),
        }
    }

    #[test]
    fn rename_local_variable_edits_every_occurrence() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("rename.bsl");
        let source = "Процедура Тест()\n    Перем МояПеременная;\n    МояПеременная = 10;\n    Результат = МояПеременная;\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        let ctx = latency_ctx(&state);
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character: 10 },
            },
            new_name: "Итог".to_string(),
            work_done_progress_params: Default::default(),
        };

        let edit = handle_rename(&ctx, params).unwrap().unwrap();
        let changes = edit.changes.expect("workspace edit must carry changes");
        let edits = changes.get(&uri).expect("edits for the renamed file");
        assert_eq!(edits.len(), 3, "declaration + 2 usages");
        assert!(edits.iter().all(|e| e.new_text == "Итог"));
    }

    #[test]
    fn rename_uses_versioned_document_changes_when_supported() {
        let mut state = create_test_state();
        state.init_empty_source_root();
        state.supports_workspace_edit_document_changes = true;

        let uri = crate::test_uri::file_uri("rename.bsl");
        let source =
            "Процедура Тест()\n    Перем МояПеременная;\n    МояПеременная = 10;\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        let ctx = latency_ctx(&state);
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character: 10 },
            },
            new_name: "Итог".to_string(),
            work_done_progress_params: Default::default(),
        };

        let edit = handle_rename(&ctx, params).unwrap().unwrap();
        assert!(edit.changes.is_none(), "versioned path must not populate changes");
        match edit.document_changes.expect("document_changes must be present") {
            DocumentChanges::Edits(edits) => {
                assert_eq!(edits.len(), 1);
                assert_eq!(edits[0].text_document.uri, uri);
                assert_eq!(edits[0].text_document.version, Some(1));
                assert_eq!(edits[0].edits.len(), 2, "declaration + 1 usage");
            }
            other => panic!("expected edits, got {other:?}"),
        }
    }

    #[test]
    fn rename_rejects_invalid_identifier() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("rename.bsl");
        let source =
            "Процедура Тест()\n    Перем МояПеременная;\n    МояПеременная = 10;\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        let ctx = latency_ctx(&state);
        let params = RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line: 1, character: 10 },
            },
            new_name: "Если".to_string(),
            work_done_progress_params: Default::default(),
        };

        assert!(handle_rename(&ctx, params).is_err());
    }

    fn call_hierarchy_item_at(
        state: &GlobalState,
        uri: &lsp_types::Url,
        position: Position,
    ) -> CallHierarchyItem {
        let ctx = latency_ctx(state);
        let params = CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: Default::default(),
        };
        let mut items = handle_prepare_call_hierarchy(&ctx, params).unwrap().unwrap();
        assert_eq!(items.len(), 1);
        items.pop().unwrap()
    }

    fn incoming_params(item: CallHierarchyItem) -> CallHierarchyIncomingCallsParams {
        CallHierarchyIncomingCallsParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    #[test]
    fn prepare_call_hierarchy_reports_method_item() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("ch.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        assert_eq!(item.name, "Помощник");
        assert_eq!(item.kind, SymbolKind::METHOD);
        assert_eq!(item.selection_range.start, Position { line: 0, character: 10 });
    }

    #[test]
    fn prepare_call_hierarchy_enqueues_one_build_start_per_generation() {
        // Given: a call-hierarchy anchor in a loaded source root.
        let mut state = create_test_state();
        state.init_empty_source_root();
        let uri = crate::test_uri::file_uri("ch-prepare.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let receiver = state.task_pool.receiver.clone();

        // When: its initial prepare starts a generation and a later prepare sees it building.
        let _ = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let task = receiver.try_recv().expect("prepare must enqueue a build start");
        let crate::global_state::Task::CallHierarchyIndexBuildRequested { source_root, generation } =
            task
        else {
            panic!("prepare must enqueue a call-hierarchy build start");
        };
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            generation,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(generation),
        ));
        assert!(state.call_hierarchy_index.has_active_build(source_root));
        assert!(!state.call_hierarchy_index.record_prepare(source_root, generation));
        let _ = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });

        // Then: one idempotent start signal is queued for the source root.
        match receiver.try_recv() {
            Err(_) => {}
            Ok(task) => panic!("duplicate prepare unexpectedly queued {task:?}"),
        }
    }

    #[test]
    fn call_hierarchy_outgoing_reports_callee_and_ranges() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("ch.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        // Anchor on Первый (line 3), which calls Помощник once.
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 3, character: 10 });

        let ctx = latency_ctx(&state);
        let params = CallHierarchyOutgoingCallsParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let calls = handle_call_hierarchy_outgoing(&ctx, params).unwrap().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to.name, "Помощник");
        assert_eq!(calls[0].from_ranges.len(), 1);
        // The call site sits on line 4 inside Первый's body.
        assert_eq!(calls[0].from_ranges[0].start.line, 4);
    }

    #[test]
    fn call_hierarchy_incoming_reports_callers() {
        // Given: a published compact index naming two callers of one method.
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("ch.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n\nПроцедура Второй()\n    Помощник();\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        publish_call_hierarchy_index(&mut state, &fixture, &["Первый", "Второй"]);

        // When: the LSP request resolves incoming calls from that index.
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });

        let ctx = latency_ctx(&state);
        let calls = handle_call_hierarchy_incoming(&ctx, incoming_params(item)).unwrap().unwrap();
        // Then: every caller and its live call-site ranges are returned.
        assert_eq!(calls.len(), 2);
        let first = calls.iter().find(|call| call.from.name == "Первый").unwrap();
        assert_eq!(first.from_ranges.len(), 1);
        let second = calls.iter().find(|call| call.from.name == "Второй").unwrap();
        assert_eq!(second.from_ranges.len(), 2);
    }

    #[test]
    fn call_hierarchy_index_warm_prepare_reuses_ready() {
        // Given: a resident compact index for one target and its caller.
        let mut state = create_test_state();
        state.init_empty_source_root();
        let uri = crate::test_uri::file_uri("ch-warm.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let source_root = publish_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        let receiver = state.task_pool.receiver.clone();

        // When: the client prepares and expands the same Ready index twice.
        let first = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let first_calls =
            handle_call_hierarchy_incoming(&latency_ctx(&state), incoming_params(first))
                .unwrap()
                .unwrap();
        let second = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let second_calls =
            handle_call_hierarchy_incoming(&latency_ctx(&state), incoming_params(second))
                .unwrap()
                .unwrap();

        // Then: both expansions use generation one without requesting a rebuild.
        assert_eq!(first_calls.len(), 1);
        assert_eq!(second_calls.len(), 1);
        assert_eq!(state.call_hierarchy_index.generation(source_root), Some(1));
        assert!(
            receiver.try_recv().is_err(),
            "prepare against Ready must not schedule a replacement build"
        );
    }

    #[test]
    fn call_hierarchy_index_single_worker_no_deadlock() {
        // Given: a prepared build and exactly one shared latency-pool worker.
        let mut state = create_test_state();
        state.task_pool = TaskPool::new_with_workers(1);
        state.init_empty_source_root();
        // The dispatcher rejects requests until VFS loading has completed in production.
        state.vfs_done = true;
        let uri = crate::test_uri::file_uri("ch-single-worker.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let Task::CallHierarchyIndexBuildRequested { source_root, generation } =
            state.task_pool.receiver.try_recv().expect("prepare must request a build")
        else {
            panic!("prepare must request a call-hierarchy build");
        };
        let (_, index) = build_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            generation,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(generation),
        ));
        let lifecycle = state.call_hierarchy_index.ensure();
        let task_receiver = state.task_pool.receiver.clone();

        // When: incoming waits while the build completion is queued on the shared pool.
        let request = lsp_server::Request::new(
            lsp_server::RequestId::from(81),
            CallHierarchyIncomingCalls::METHOD.to_owned(),
            serde_json::to_value(incoming_params(item)).expect("incoming params serialize"),
        );
        RequestDispatcher { req: Some(request), global_state: &mut state }
            .on_waiting_latency::<CallHierarchyIncomingCalls>(handle_call_hierarchy_incoming)
            .finish();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !lifecycle.has_waiter(source_root, generation) {
            assert!(std::time::Instant::now() < deadline, "incoming request did not park");
            std::thread::yield_now();
        }
        state
            .task_pool
            .pool
            .try_spawn(move || {
                assert!(lifecycle.publish(source_root, generation, index));
                Task::AnalysisProgressTick { epoch: 0 }
            })
            .expect("build completion fits in the shared pool queue");

        // Then: the queued completion runs and the incoming request returns callers.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let response = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let task = task_receiver
                .recv_timeout(remaining)
                .expect("single-worker incoming request must complete");
            if let Task::RequestResult { response } = task {
                break response;
            }
        };
        assert!(response.error.is_none(), "incoming response must succeed");
        let calls: Option<Vec<CallHierarchyIncomingCall>> =
            serde_json::from_value(response.result.expect("incoming result")).expect("call list");
        assert_eq!(calls.expect("indexed callers").len(), 1);
    }

    #[test]
    fn call_hierarchy_index_lsp_waits_for_a_prepared_build() {
        // Given: a prepared target whose compact index is still building.
        let mut state = create_test_state();
        state.init_empty_source_root();
        let uri = crate::test_uri::file_uri("ch-wait.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let (source_root, index) = build_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            1,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(1),
        ));
        let lifecycle = state.call_hierarchy_index.clone();
        let publisher = std::thread::spawn(move || {
            // A hang guard, not a measurement: the request parks as soon as it is
            // scheduled, which a loaded machine may put off for a long time.
            let deadline = std::time::Instant::now() + PARK_HANG_GUARD;
            while !lifecycle.has_waiter(source_root, 1) {
                assert!(std::time::Instant::now() < deadline, "incoming request did not park");
                std::thread::yield_now();
            }
            assert!(lifecycle.publish(source_root, 1, index));
        });
        let mut ctx = latency_ctx(&state);
        // The wait only has to outlast the publisher's scheduling; a waiter that gives
        // up first answers `None` and fails below, so the bound stays a bound.
        ctx.call_hierarchy_wait_policy.timeout = PARK_HANG_GUARD;

        // When: incoming calls arrive before the build publishes.
        let result = handle_call_hierarchy_incoming(
            &ctx,
            CallHierarchyIncomingCallsParams {
                item,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        );
        publisher.join().expect("publisher must complete");
        let calls = result.unwrap().unwrap();

        // Then: the bounded waiter serves the newly published caller list.
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].from.name, "Первый");
    }

    #[test]
    fn call_hierarchy_index_lsp_incoming_without_prepare_returns_none_without_starting() {
        // Given: an otherwise valid call-hierarchy item without retained prepare state.
        let mut state = create_test_state();
        state.init_empty_source_root();
        let uri = crate::test_uri::file_uri("ch-unprepared.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        state.call_hierarchy_index = Default::default();

        // When: incoming calls arrive without a successful prepare for this lifecycle.
        let result = handle_call_hierarchy_incoming(&latency_ctx(&state), incoming_params(item));

        // Then: the request returns null and does not create a lifecycle generation.
        assert!(result.unwrap().is_none());
        let file_id = state.vfs_file_for_url(&uri).expect("fixture file is registered");
        let db = state.analysis_host.raw_database();
        let source_root = db.file_source_root_input(file_id).source_root_id(db);
        assert_eq!(state.call_hierarchy_index.generation(source_root), None);
    }

    #[test]
    fn call_hierarchy_index_lsp_timeout_returns_none_then_serves_published_index() {
        // Given: a prepared build that will outlive a short incoming-call deadline.
        let mut state = create_test_state();
        state.init_empty_source_root();
        let uri = crate::test_uri::file_uri("ch-timeout.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let (source_root, index) = build_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            1,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(1),
        ));
        let mut ctx = latency_ctx(&state);
        ctx.call_hierarchy_wait_policy.timeout = std::time::Duration::from_millis(20);

        // When: the single waiter reaches its deadline before publication.
        let started = std::time::Instant::now();
        let timed_out = handle_call_hierarchy_incoming(&ctx, incoming_params(item.clone()));

        // Then: it returns promptly, releases the permit, and a later Ready request succeeds.
        assert!(timed_out.unwrap().is_none());
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        assert!(!state.call_hierarchy_index.has_waiter(source_root, 1));
        assert!(state.call_hierarchy_index.publish(source_root, 1, index));
        let calls = handle_call_hierarchy_incoming(&latency_ctx(&state), incoming_params(item))
            .unwrap()
            .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].from.name, "Первый");
    }

    #[test]
    fn call_hierarchy_index_lsp_cancelled_wait_releases_permit_without_cancelling_build() {
        // Given: an incoming request parked on a prepared build.
        let mut state = create_test_state();
        state.init_empty_source_root();
        let uri = crate::test_uri::file_uri("ch-cancel.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let (source_root, index) = build_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            1,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(1),
        ));
        let (ctx, token) = latency_ctx_with_token(&state);
        let waiter = std::thread::spawn(move || {
            salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                handle_call_hierarchy_incoming(&ctx, incoming_params(item))
            }))
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while !state.call_hierarchy_index.has_waiter(source_root, 1) {
            assert!(std::time::Instant::now() < deadline, "incoming request did not park");
            std::thread::yield_now();
        }

        // When: the client cancels the waiting request.
        token.cancel();
        let cancelled = waiter.join().expect("waiting request must finish");

        // Then: cancellation detaches the request but preserves the shared build for later callers.
        assert!(cancelled.is_err());
        assert!(!state.call_hierarchy_index.has_waiter(source_root, 1));
        assert!(state.call_hierarchy_index.is_building(source_root, 1));
        assert!(state.call_hierarchy_index.publish(source_root, 1, index));
        let calls = handle_call_hierarchy_incoming(
            &latency_ctx(&state),
            incoming_params(call_hierarchy_item_at(
                &state,
                &uri,
                Position { line: 0, character: 10 },
            )),
        )
        .unwrap()
        .unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn call_hierarchy_index_lsp_allows_one_waiter_while_followers_and_hover_complete() {
        // Given: a prepared index build that has not yet published.
        let mut state = create_test_state();
        state.init_empty_source_root();
        let uri = crate::test_uri::file_uri("ch-followers.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });
        let (source_root, index) = build_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        assert!(state.call_hierarchy_index.start_build(
            source_root,
            1,
            crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId(1),
        ));
        // One bound for every request: a follower that parked would answer no sooner
        // than this, so "answered sooner" below is what tells a follower that did not
        // park from one that did — whatever the machine's speed.
        let wait_bound = PARK_HANG_GUARD;
        let mut waiting_ctx = latency_ctx(&state);
        waiting_ctx.call_hierarchy_wait_policy.timeout = wait_bound;
        let follower_contexts: Vec<_> = (0..4)
            .map(|_| {
                let mut ctx = latency_ctx(&state);
                ctx.call_hierarchy_wait_policy.timeout = wait_bound;
                ctx
            })
            .collect();
        let hover_ctx = latency_ctx(&state);

        std::thread::scope(|scope| {
            let waiting_item = item.clone();
            let waiting_request = scope.spawn(move || {
                handle_call_hierarchy_incoming(&waiting_ctx, incoming_params(waiting_item))
            });
            let deadline = std::time::Instant::now() + PARK_HANG_GUARD;
            while !state.call_hierarchy_index.has_waiter(source_root, 1) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "first incoming request did not park"
                );
                std::thread::yield_now();
            }

            // When: concurrent incoming requests and an unrelated hover arrive.
            let followers_started = std::time::Instant::now();
            let followers: Vec<_> = follower_contexts
                .into_iter()
                .map(|ctx| {
                    let item = item.clone();
                    scope.spawn(move || handle_call_hierarchy_incoming(&ctx, incoming_params(item)))
                })
                .collect();
            let hover_uri = uri.clone();
            let hover = scope.spawn(move || {
                handle_hover(
                    &hover_ctx,
                    HoverParams {
                        text_document_position_params: TextDocumentPositionParams {
                            text_document: TextDocumentIdentifier { uri: hover_uri },
                            position: Position { line: 0, character: 10 },
                        },
                        work_done_progress_params: Default::default(),
                    },
                )
            });
            for follower in followers {
                assert!(
                    follower.join().expect("follower request must finish").unwrap().is_none(),
                    "only the first Building request may wait"
                );
            }
            let followers_took = followers_started.elapsed();
            assert!(
                followers_took < wait_bound,
                "a follower waited out the bound instead of answering at once: {followers_took:?}",
            );
            assert!(hover.join().expect("hover request must finish").unwrap().is_some());
            assert!(state.call_hierarchy_index.publish(source_root, 1, index));

            // Then: the sole waiter receives callers after publication.
            let calls =
                waiting_request.join().expect("waiting request must finish").unwrap().unwrap();
            assert_eq!(calls.len(), 1);
        });
    }

    #[test]
    fn call_hierarchy_incoming_returns_none_after_index_supersession() {
        // Given: a request target with a ready compact index.
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("ch-superseded.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let source_root = publish_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });

        // When: a layout change supersedes the resident index.
        assert!(state.call_hierarchy_index.supersede(source_root));
        let ctx = latency_ctx(&state);
        let params = CallHierarchyIncomingCallsParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        // Then: the stale index cannot serve the request.
        assert!(handle_call_hierarchy_incoming(&ctx, params).unwrap().is_none());
    }

    #[test]
    fn call_hierarchy_index_ready_invalidation() {
        // Given: a prepared target whose compact index is ready.
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("ch-ready-edit.bsl");
        let source = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    Помощник();\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);
        let fixture = CallHierarchyFixture { uri: &uri, source };
        let source_root = publish_call_hierarchy_index(&mut state, &fixture, &["Первый"]);
        let item = call_hierarchy_item_at(&state, &uri, Position { line: 0, character: 10 });

        // When: a body-only edit is processed before the event loop can rebuild the index.
        let edited = "Процедура Помощник()\nКонецПроцедуры\n\nПроцедура Первый()\n    // body edit\n    Помощник();\nКонецПроцедуры\n";
        state.mem_docs.insert(uri.clone(), edited.to_owned(), 2);
        state
            .vfs
            .write()
            .set_file_contents(VfsPath::new(uri.to_file_path().unwrap()), Some(Arc::from(edited)));
        state.process_changes(false);

        // Then: the ready index is no longer eligible to serve its stale callers.
        assert!(!state.call_hierarchy_index.is_ready_generation(source_root, 1));
        assert!(handle_call_hierarchy_incoming(&latency_ctx(&state), incoming_params(item))
            .unwrap()
            .is_none());
    }

    #[test]
    fn workspace_symbol_finds_exported_method() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("ws.bsl");
        let source =
            "Функция ОбщийРасчёт() Экспорт\nКонецФункции\n\nФункция Приватный()\nКонецФункции\n";
        open_source(&mut state, &uri, source);

        let ctx = latency_ctx(&state);
        let params = WorkspaceSymbolParams {
            partial_result_params: Default::default(),
            work_done_progress_params: Default::default(),
            query: "Общий".to_string(),
        };

        let response = handle_workspace_symbol(&ctx, params).unwrap().unwrap();
        let symbols = match response {
            WorkspaceSymbolResponse::Nested(s) => s,
            other => panic!("expected nested workspace symbols, got {other:?}"),
        };
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "ОбщийРасчёт");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        match &symbols[0].location {
            OneOf::Left(loc) => assert_eq!(loc.uri, uri),
            OneOf::Right(_) => panic!("expected a full location with range"),
        }
    }

    #[test]
    fn type_definition_returns_none_for_platform_typed_value() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("td.bsl");
        let source = "Процедура Тест()\n    Счётчик = 1;\n    Сообщить(Счётчик);\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        let ctx = latency_ctx(&state);
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line: 1, character: 4 }, // on the number-typed "Счётчик"
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        // A platform primitive (Число) has no navigable type definition; the
        // handler must decline cleanly rather than error or panic.
        let result = handle_type_definition(&ctx, params);
        assert!(result.is_err() || result.unwrap().is_none());
    }

    #[test]
    fn selection_range_nests_from_cursor_outward() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("sel.bsl");
        let source = "Процедура Тест()\n    Итог = Первое + Второе;\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        let ctx = latency_ctx(&state);
        let params = SelectionRangeParams {
            text_document: TextDocumentIdentifier { uri },
            positions: vec![Position { line: 1, character: 11 }], // inside "Первое"
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let ranges = handle_selection_range(&ctx, params).unwrap().unwrap();
        assert_eq!(ranges.len(), 1);

        // Walk the parent chain; each parent must strictly contain its child.
        let mut node = &ranges[0];
        let mut depth = 1;
        while let Some(parent) = &node.parent {
            let child = &node.range;
            let wider = &parent.range;
            let contains = (wider.start.line, wider.start.character)
                <= (child.start.line, child.start.character)
                && (child.end.line, child.end.character) <= (wider.end.line, wider.end.character);
            assert!(contains, "parent {wider:?} must contain child {child:?}");
            node = parent;
            depth += 1;
        }
        assert!(depth >= 2, "expected a nested chain, got depth {depth}");
    }

    #[test]
    fn inlay_hint_labels_call_arguments() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("hints.bsl");
        let source = "Функция Сложить(Первое, Второе)\n    Возврат Первое;\nКонецФункции\n\nПроцедура Тест()\n    Сложить(10, 20);\nКонецПроцедуры\n";
        open_source(&mut state, &uri, source);

        let ctx = latency_ctx(&state);
        let params = InlayHintParams {
            work_done_progress_params: Default::default(),
            text_document: TextDocumentIdentifier { uri },
            range: lsp_types::Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 7, character: 0 },
            },
        };

        let hints = handle_inlay_hint(&ctx, params).unwrap().unwrap();
        assert!(hints.iter().all(|h| h.kind == Some(LspInlayHintKind::PARAMETER)));
        let labels: Vec<String> = hints
            .iter()
            .map(|h| match &h.label {
                InlayHintLabel::String(s) => s.clone(),
                _ => panic!("expected string label"),
            })
            .collect();
        assert!(labels.contains(&"Первое:".to_string()), "{labels:?}");
        assert!(labels.contains(&"Второе:".to_string()), "{labels:?}");
    }

    #[test]
    fn document_highlight_returns_ranges_and_kinds() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("highlight.bsl");
        let source = r#"
Процедура Тест()
    Перем МояПеременная;

    МояПеременная = 10;
    Сообщить(МояПеременная);
КонецПроцедуры
"#;

        state.mem_docs.insert(uri.clone(), source.to_string(), 1);
        let open_file_id = state.vfs_file_for_url(&uri).unwrap();
        state.open_files.insert(open_file_id);
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                VfsPath::new(uri.to_file_path().unwrap()),
                Some(Arc::from(source)),
            );
        }
        state.process_changes(false);

        let ctx = latency_ctx(&state);
        let params = DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line: 2, character: 10 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = handle_document_highlight(&ctx, params).unwrap().unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].kind, Some(LspDocumentHighlightKind::TEXT));
        assert_eq!(result[0].range.start, Position { line: 2, character: 10 });
        assert_eq!(result[1].kind, Some(LspDocumentHighlightKind::WRITE));
        assert_eq!(result[1].range.start, Position { line: 4, character: 4 });
        assert_eq!(result[2].kind, Some(LspDocumentHighlightKind::READ));
        assert_eq!(result[2].range.start, Position { line: 5, character: 13 });
    }

    #[test]
    fn folding_range_returns_lines_and_region_kind() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("folding.bsl");
        let source = "#Область Public\nПроцедура Тест()\n    Если Истина Тогда\n        Сообщить(1);\n    КонецЕсли;\nКонецПроцедуры\n#КонецОбласти";

        state.mem_docs.insert(uri.clone(), source.to_string(), 1);
        let open_file_id = state.vfs_file_for_url(&uri).unwrap();
        state.open_files.insert(open_file_id);
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                VfsPath::new(uri.to_file_path().unwrap()),
                Some(Arc::from(source)),
            );
        }
        state.process_changes(false);

        let ctx = latency_ctx(&state);
        let params = FoldingRangeParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = handle_folding_range(&ctx, params).unwrap().unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].start_line, 0);
        assert_eq!(result[0].end_line, 6);
        assert_eq!(result[0].kind, Some(LspFoldingRangeKind::Region));
        assert_eq!(result[1].start_line, 1);
        assert_eq!(result[1].end_line, 5);
        assert_eq!(result[1].kind, None);
        assert_eq!(result[2].start_line, 2);
        assert_eq!(result[2].end_line, 4);
        assert_eq!(result[2].kind, None);
    }

    fn folding_lines_for(
        file_name: &str,
        source: &str,
    ) -> Vec<(u32, u32, Option<LspFoldingRangeKind>)> {
        folding_answer(file_name, source, None).unwrap_or_default()
    }

    /// Сырой ответ хендлера: `None` — это пустой ответ (`null`), и его нельзя
    /// путать с присутствующим пустым списком.
    fn folding_answer(
        file_name: &str,
        source: &str,
        limit: Option<u32>,
    ) -> Option<Vec<(u32, u32, Option<LspFoldingRangeKind>)>> {
        let mut state = create_test_state();
        state.init_empty_source_root();
        state.folding_range_limit = limit;

        let uri = crate::test_uri::file_uri(file_name);
        state.mem_docs.insert(uri.clone(), source.to_string(), 1);
        let open_file_id = state.vfs_file_for_url(&uri).unwrap();
        state.open_files.insert(open_file_id);
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                VfsPath::new(uri.to_file_path().unwrap()),
                Some(Arc::from(source)),
            );
        }
        state.process_changes(false);

        let ctx = latency_ctx(&state);
        let params = FoldingRangeParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        handle_folding_range(&ctx, params).unwrap().map(|ranges| {
            ranges
                .iter()
                .map(|range| (range.start_line, range.end_line, range.kind.clone()))
                .collect()
        })
    }

    const FOLDING_COMMENT_SOURCE: &str = r#"#Область ПрограммныйИнтерфейс

// Возвращает описание физического лица для передачи в ФЭС.
//
// Параметры:
//  ФизическоеЛицо - СправочникСсылка.ФизическиеЛица - лицо, чьи данные собираются.
//  Дата           - Дата - на какой момент брать историю наименований.
//
// Возвращаемое значение:
//  Структура - состав полей описан в документации подсистемы.
//
Функция ОписаниеФизическогоЛица(ФизическоеЛицо, Дата) Экспорт

    // Пустая ссылка сюда доходить не должна: вызывающий код проверяет её сам,
    // но контракт дешевле продублировать, чем потом искать пустую структуру
    // в выгрузке.
    Если НЕ ЗначениеЗаполнено(ФизическоеЛицо) Тогда
        ВызватьИсключение "Не задано физическое лицо";
    КонецЕсли;

    Возврат Новый Структура;

КонецФункции

#КонецОбласти"#;

    fn issue_folding_lines() -> Vec<(u32, u32, Option<LspFoldingRangeKind>)> {
        vec![
            (0, 24, Some(LspFoldingRangeKind::Region)),
            (2, 10, Some(LspFoldingRangeKind::Comment)),
            (11, 22, None),
            (13, 15, Some(LspFoldingRangeKind::Comment)),
            (16, 18, None),
        ]
    }

    #[test]
    fn folding_range_reports_comment_kind() {
        assert_eq!(
            folding_lines_for("folding_comment.bsl", FOLDING_COMMENT_SOURCE),
            issue_folding_lines()
        );
    }

    #[test]
    fn folding_range_reports_comment_kind_on_crlf() {
        // Модуль, пришедший из 1С под Windows, разделён CRLF: `\r` уезжает внутрь
        // токена комментария, а пустая строка приходит парой NEWLINE с пробельным
        // токеном между ними. Номера строк в ответе от этого меняться не должны.
        let crlf = FOLDING_COMMENT_SOURCE.replace('\n', "\r\n");
        assert_eq!(folding_lines_for("folding_comment_crlf.bsl", &crlf), issue_folding_lines());

        // В фикстуре из issue пустых строк внутри серий нет, поэтому сама по себе
        // она CRLF-слепую сборку серий не различает. Этот вход различает: пустая
        // строка обязана разорвать серию и здесь.
        assert_eq!(
            folding_lines_for("folding_crlf_split.bsl", "// а\r\n// б\r\n\r\n// в\r\n// г\r\n"),
            vec![
                (0, 1, Some(LspFoldingRangeKind::Comment)),
                (3, 4, Some(LspFoldingRangeKind::Comment)),
            ]
        );
    }

    /// `rangeLimit` — не подсказка «сколько получится»: ответ урезается, и на
    /// контрольном входе без лимита видно, что режется именно он.
    #[test]
    fn folding_range_is_truncated_to_the_client_range_limit() {
        let source = "Процедура П1()\n\
                      Сообщить(1);\n\
                      КонецПроцедуры\n\
                      Процедура П2()\n\
                      Сообщить(2);\n\
                      КонецПроцедуры\n\
                      Процедура П3()\n\
                      Сообщить(3);\n\
                      КонецПроцедуры";

        assert_eq!(
            folding_lines_for("folding_limit_all.bsl", source),
            vec![(0, 2, None), (3, 5, None), (6, 8, None)]
        );
        assert_eq!(
            folding_answer("folding_limit_two.bsl", source, Some(2)),
            Some(vec![(0, 2, None), (3, 5, None)])
        );
        // Лимит выше ответа не режет ничего, а нулевой лимит — это именно
        // пустой ответ (`null`), а не присутствующий пустой список.
        assert_eq!(
            folding_answer("folding_limit_ten.bsl", source, Some(10)),
            Some(vec![(0, 2, None), (3, 5, None), (6, 8, None)])
        );
        assert_eq!(folding_answer("folding_limit_zero.bsl", source, Some(0)), None);
    }

    /// Политика урезания: первыми уходят самые глубоко вложенные складки —
    /// внешний каркас переживает лимит.
    #[test]
    fn range_limit_drops_the_deepest_fold_first() {
        let mut ranges = vec![fold(0, 19), fold(1, 10), fold(2, 9), fold(11, 12)];

        truncate_folding_ranges(&mut ranges, 3);

        assert_eq!(fold_pairs(&ranges), vec![(0, 19), (1, 10), (11, 12)]);
    }

    /// При равной глубине уходит самая короткая складка: она скрывает меньше
    /// строк.
    #[test]
    fn range_limit_drops_the_shortest_fold_at_equal_depth() {
        let mut ranges = vec![fold(0, 5), fold(10, 12), fold(20, 30)];

        truncate_folding_ranges(&mut ranges, 2);

        assert_eq!(fold_pairs(&ranges), vec![(0, 5), (20, 30)]);
    }

    fn fold(start_line: u32, end_line: u32) -> LspFoldingRange {
        LspFoldingRange {
            start_line,
            start_character: None,
            end_line,
            end_character: None,
            kind: None,
            collapsed_text: None,
        }
    }

    fn fold_pairs(ranges: &[LspFoldingRange]) -> Vec<(u32, u32)> {
        ranges.iter().map(|range| (range.start_line, range.end_line)).collect()
    }

    fn setup_code_action_doc(source: &str) -> (GlobalState, lsp_types::Url) {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("fixall.bsl");
        state.mem_docs.insert(uri.clone(), source.to_string(), 1);
        let open_file_id = state.vfs_file_for_url(&uri).unwrap();
        state.open_files.insert(open_file_id);
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                VfsPath::new(uri.to_file_path().unwrap()),
                Some(Arc::from(source)),
            );
        }
        state.process_changes(false);
        (state, uri)
    }

    fn code_action_params(
        uri: lsp_types::Url,
        only: Option<Vec<CodeActionKind>>,
    ) -> CodeActionParams {
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range: lsp_types::Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 4, character: 0 },
            },
            context: lsp_types::CodeActionContext { diagnostics: vec![], only, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    fn actions_of_kind<'a>(
        actions: &'a [CodeActionOrCommand],
        kind: &str,
    ) -> Vec<&'a lsp_types::CodeAction> {
        actions
            .iter()
            .filter_map(|action| match action {
                CodeActionOrCommand::CodeAction(ca)
                    if ca.kind.as_ref().is_some_and(|k| k.as_str() == kind) =>
                {
                    Some(ca)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn code_action_offers_source_fix_all_over_the_whole_file() {
        let source =
            "Процедура Тест()\n    ЭтаФорма.Закрыть();\n    ЭтаФорма.Открыть();\nКонецПроцедуры\n";
        let (state, uri) = setup_code_action_doc(source);
        let ctx = latency_ctx(&state);

        let result = handle_code_action(&ctx, code_action_params(uri, None)).unwrap().unwrap();

        // Two individual quick fixes (one per `ЭтаФорма`).
        let quickfixes = actions_of_kind(&result, "quickfix");
        assert!(
            quickfixes.iter().filter(|ca| ca.diagnostics.is_some()).count() >= 2,
            "expected a quick fix per occurrence, got {result:#?}"
        );

        // One aggregate `source.fixAll` action covering both occurrences.
        let fix_all = actions_of_kind(&result, crate::lsp::to_proto::FIX_ALL_BSL);
        assert_eq!(fix_all.len(), 1, "expected exactly one source.fixAll action");
        let edits = fix_all[0]
            .edit
            .as_ref()
            .and_then(|we| we.changes.as_ref())
            .and_then(|c| c.values().next())
            .expect("fixAll must carry edits");
        assert_eq!(edits.len(), 2, "fixAll must merge both occurrences");
        assert!(edits.iter().all(|e| e.new_text == "ЭтотОбъект"));

        // One "fix all occurrences of this code" batched quick fix (kind quickfix, no diag).
        let batched = quickfixes.iter().filter(|ca| ca.diagnostics.is_none()).count();
        assert_eq!(batched, 1, "expected one 'fix all occurrences' batch");
    }

    #[test]
    fn code_action_only_source_fix_all_suppresses_quick_fixes() {
        let source =
            "Процедура Тест()\n    ЭтаФорма.Закрыть();\n    ЭтаФорма.Открыть();\nКонецПроцедуры\n";
        let (state, uri) = setup_code_action_doc(source);
        let ctx = latency_ctx(&state);

        let only = Some(vec![CodeActionKind::SOURCE_FIX_ALL]);
        let result = handle_code_action(&ctx, code_action_params(uri, only)).unwrap().unwrap();

        assert!(
            actions_of_kind(&result, "quickfix").is_empty(),
            "quick fixes must be filtered out"
        );
        assert_eq!(
            actions_of_kind(&result, crate::lsp::to_proto::FIX_ALL_BSL).len(),
            1,
            "requesting source.fixAll must still match the bsl-analyzer subkind"
        );
    }

    #[test]
    fn code_action_only_quickfix_suppresses_source_fix_all() {
        let source =
            "Процедура Тест()\n    ЭтаФорма.Закрыть();\n    ЭтаФорма.Открыть();\nКонецПроцедуры\n";
        let (state, uri) = setup_code_action_doc(source);
        let ctx = latency_ctx(&state);

        let only = Some(vec![CodeActionKind::QUICKFIX]);
        let result = handle_code_action(&ctx, code_action_params(uri, only)).unwrap().unwrap();

        assert!(
            actions_of_kind(&result, crate::lsp::to_proto::FIX_ALL_BSL).is_empty(),
            "source.fixAll must be filtered out when only quickfix is requested"
        );
        assert!(!actions_of_kind(&result, "quickfix").is_empty(), "quick fixes must remain");
    }

    /// A name search answers about files nobody named, and one of them can be
    /// gone by the time its location is built: metadata XML is rewritten by the
    /// configurator and by git alike, and reading a closed file that changed
    /// under analysis panics by design.
    ///
    /// One racing file must cost its own candidate, not the whole request. The
    /// readable neighbour in the stand is what keeps this from passing on an
    /// answer that is simply empty.
    #[test]
    fn workspace_symbol_skips_a_candidate_whose_file_is_gone() {
        // The panic below is real and runs under an attached database, so it stamps the
        // process-wide panic watch like any other: a test asserting the watch is clean
        // must not run beside it.
        let _guard = crate::panic_watch::panic_test_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let module_path = tmp.path().join("Ровный.bsl");
        let module_text = "Процедура ДрейфТестМетод() Экспорт\nКонецПроцедуры\n";
        std::fs::write(&module_path, module_text).expect("write module");
        let xml_path = tmp.path().join("ДрейфТест.xml");
        std::fs::write(&xml_path, "<MetaDataObject/>").expect("write xml");

        let mut state = create_test_state();
        state.init_empty_source_root();

        let module_uri = lsp_types::Url::parse(&format!("file://{}", module_path.display()))
            .expect("module url");
        let module_id = state.vfs_file_for_url(&module_uri).unwrap();
        let xml_id = {
            let mut vfs = state.vfs.write();
            vfs.alloc_file_id(VfsPath::new(xml_path.clone()))
        };
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(VfsPath::new(module_path.clone()), Some(Arc::from(module_text)));
            vfs.set_file_contents(
                VfsPath::new(xml_path.clone()),
                Some(Arc::from("<MetaDataObject/>")),
            );
        }
        state.process_changes(false);

        {
            let db = state.analysis_host.raw_database_mut();
            db.set_all_config_paths(vec![(None, tmp.path().to_path_buf())]);
            db.set_metadata_listing(
                &tmp.path().to_string_lossy(),
                ide_db::metadata::MetadataListingData {
                    entries: vec![ide_db::metadata::MdoEntry {
                        kind: bsl_metadata::MdoType::Catalog,
                        name: "ДрейфТест".to_string(),
                        main: xml_id,
                        predefined: None,
                    }],
                    ..Default::default()
                },
            );
        }

        // Both candidates are in the answer before anything goes wrong: without
        // this the assertion below would hold on a stand that never found the
        // XML in the first place.
        {
            let analysis = state.analysis_host.analysis();
            let found = analysis.workspace_symbols("ДрейфТест");
            let files: std::collections::BTreeSet<_> =
                found.candidates.iter().filter_map(|c| c.place.map(|p| p.file_id)).collect();
            assert!(files.contains(&xml_id), "the XML is a candidate: {files:?}");
            assert!(files.contains(&module_id), "the module is a candidate: {files:?}");
        }

        // The XML goes away between the lookup and the location: exactly the
        // race, and `file_text` answers it with a panic.
        std::fs::remove_file(&xml_path).expect("remove xml");

        let ctx = latency_ctx(&state);
        let response = handle_workspace_symbol(
            &ctx,
            WorkspaceSymbolParams { query: "ДрейфТест".to_string(), ..Default::default() },
        )
        .expect("one unreadable file must not fail the request");

        let Some(WorkspaceSymbolResponse::Nested(symbols)) = response else {
            panic!("the readable neighbour is still an answer: {response:?}");
        };
        assert_eq!(symbols.len(), 1, "{symbols:?}");
        assert_eq!(symbols[0].name, "ДрейфТестМетод");
    }

    #[test]
    fn reference_location_converter_uses_target_file_uri_and_text() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let source_uri = crate::test_uri::file_uri("source.bsl");
        let target_uri = crate::test_uri::file_uri("target.bsl");
        let source_text = "Процедура Источник()\nКонецПроцедуры";
        let target_text = "ПерваяСтрока\n    Цель();\n";

        let source_file_id = state.vfs_file_for_url(&source_uri).unwrap();
        let target_file_id = state.vfs_file_for_url(&target_uri).unwrap();
        // Both open: text lives in the resident overlay (these synthetic files
        // have no disk path for the disk-backed path to read).
        state.mem_docs.insert(source_uri.clone(), source_text.to_string(), 1);
        state.open_files.insert(source_file_id);
        state.open_files.insert(target_file_id);

        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                VfsPath::new(source_uri.to_file_path().unwrap()),
                Some(Arc::from(source_text)),
            );
            vfs.set_file_contents(
                VfsPath::new(target_uri.to_file_path().unwrap()),
                Some(Arc::from(target_text)),
            );
        }

        state.process_changes(false);
        let ctx = latency_ctx(&state);

        let start = target_text.find("Цель").unwrap() as u32;
        let end = start + "Цель".len() as u32;
        let ide_loc = IdeLocation {
            file_id: target_file_id,
            range: ide::TextRange::new(start.into(), end.into()),
        };

        let mut converter = ReferenceLocationConverter::new(&ctx, source_file_id, source_text);
        let lsp_loc = converter.convert(ide_loc).unwrap();

        assert_eq!(lsp_loc.uri, target_uri);
        assert_eq!(lsp_loc.range.start, Position { line: 1, character: 4 });
        assert_eq!(lsp_loc.range.end, Position { line: 1, character: 8 });
    }

    #[test]
    fn completion_returns_none_on_out_of_bounds_position() {
        let mut state = create_test_state();
        state.init_empty_source_root();

        let uri = crate::test_uri::file_uri("short.bsl");
        let source = "short";

        state.mem_docs.insert(uri.clone(), source.to_string(), 1);
        let open_file_id = state.vfs_file_for_url(&uri).unwrap();
        state.open_files.insert(open_file_id);
        {
            let mut vfs = state.vfs.write();
            vfs.set_file_contents(
                VfsPath::new(uri.to_file_path().unwrap()),
                Some(Arc::from(source)),
            );
        }
        state.process_changes(false);

        let ctx = latency_ctx(&state);
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line: 999, character: 999 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };

        let result = handle_completion(&ctx, params);
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn signature_help_maps_all_overloads_to_lsp() {
        let help = ide::SignatureHelp {
            signatures: vec![
                ide::SignatureInformation {
                    signature: "Добавить(Значение)".to_string(),
                    doc: None,
                    parameters: vec![ide::ParameterInfo {
                        label: "Значение".to_string(),
                        documentation: None,
                    }],
                },
                ide::SignatureInformation {
                    signature: "Добавить(Значение, ТипОбхода)".to_string(),
                    doc: None,
                    parameters: vec![
                        ide::ParameterInfo {
                            label: "Значение".to_string(), documentation: None
                        },
                        ide::ParameterInfo {
                            label: "ТипОбхода".to_string(), documentation: None
                        },
                    ],
                },
            ],
            active_signature: Some(1),
            active_parameter: Some(0),
        };

        let lsp = to_lsp_signature_help(help);
        assert_eq!(lsp.signatures.len(), 2, "both overloads must be mapped");
        assert_eq!(lsp.signatures[0].label, "Добавить(Значение)", "first signature label mismatch");
        assert_eq!(
            lsp.signatures[1].label, "Добавить(Значение, ТипОбхода)",
            "second signature label mismatch"
        );
        assert_eq!(lsp.active_signature, Some(1), "active signature must be preserved");
        assert_eq!(lsp.active_parameter, Some(0), "active parameter must be preserved");
    }
    /// A `$/cancelRequest` against a reference walk over many files, through the real
    /// door: dispatcher, task pool, `handle_cancel`, response code. The walk is counted
    /// from the span `ide` emits per file, so the cancel is sent on observing the first
    /// file rather than after a sleep.
    mod cancel_walk_tests {
        use super::*;
        use crate::global_state::Task;
        use lsp_types::request::References;
        use std::time::Duration;
        use test_utils::walk_probe;

        /// Serialises the tests of this binary that read the process-global walk
        /// counter; see `test_utils::walk_probe`.
        static WALK_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

        /// A test that failed while holding the gate poisons it; the gate guards no
        /// data, so the next test simply takes it.
        fn serialised() -> std::sync::MutexGuard<'static, ()> {
            WALK_GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// Callers of one exported procedure. Enough that an uncancelled walk takes
        /// long enough for a cancel sent on its first file to land mid-walk; the
        /// positive control below reports the count the walk really visits.
        const CALLERS: usize = 1000;
        const ROOT: &str = "cancel-walk/CommonModules";
        const ANCHOR_SOURCE: &str = "Процедура ПриИзмененииПоля() Экспорт\nКонецПроцедуры\n";
        const CALLER_SOURCE: &str =
            "Процедура Тело() Экспорт\n    Объявление.ПриИзмененииПоля();\nКонецПроцедуры\n";

        fn anchor_uri() -> lsp_types::Url {
            crate::test_uri::file_uri(&format!("{ROOT}/Объявление/Ext/Module.bsl"))
        }

        /// The callers are registered as open buffers: the walk takes its candidates
        /// from the name-usage index over the `.bsl` paths of the anchor's source root,
        /// and an open buffer stays resident in salsa instead of being re-read from a
        /// disk this stand does not have. No metadata is needed for every file to be
        /// entered.
        fn stand(workers: Option<usize>) -> GlobalState {
            let mut state = create_test_state();
            if let Some(workers) = workers {
                state.task_pool = TaskPool::new_with_workers(workers);
            }
            state.init_empty_source_root();
            state.vfs_done = true;

            {
                let mut vfs = state.vfs.write();
                for i in 0..CALLERS {
                    let path = VfsPath::new(crate::test_uri::file_path(&format!(
                        "{ROOT}/Вызов{i:04}/Ext/Module.bsl"
                    )));
                    vfs.set_file_contents(path.clone(), Some(Arc::from(CALLER_SOURCE)));
                    let file_id = vfs.file_id(&path).expect("just registered");
                    state.open_files.insert(file_id);
                }
            }
            // Registers the anchor the same way and folds every pending VFS change,
            // callers included, into salsa.
            open_source(&mut state, &anchor_uri(), ANCHOR_SOURCE);
            state
        }

        fn references_request(id: i32) -> lsp_server::Request {
            let params = lsp_types::ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: anchor_uri() },
                    position: Position { line: 0, character: 10 },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: lsp_types::ReferenceContext { include_declaration: true },
            };
            lsp_server::Request::new(
                lsp_server::RequestId::from(id),
                References::METHOD.to_owned(),
                serde_json::to_value(params).unwrap(),
            )
        }

        fn dispatch_references(state: &mut GlobalState, id: i32) {
            RequestDispatcher { req: Some(references_request(id)), global_state: state }
                .on_latency::<References>(handle_find_references)
                .finish();
        }

        fn cancel(state: &mut GlobalState, id: i32) {
            crate::handlers::notification::handle_cancel(
                state,
                lsp_types::CancelParams { id: lsp_types::NumberOrString::Number(id) },
            )
            .unwrap();
        }

        fn response(state: &GlobalState, id: i32) -> lsp_server::Response {
            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let task = state
                    .task_pool
                    .receiver
                    .recv_timeout(remaining)
                    .expect("the references request never answered");
                if let Task::RequestResult { response } = task {
                    if response.id == lsp_server::RequestId::from(id) {
                        return response;
                    }
                }
            }
        }

        /// The uncancelled walk enters every caller: what the counter can reach, so a
        /// smaller number after a cancel means files skipped and not a counter that
        /// never moved.
        #[test]
        fn an_uncancelled_walk_enters_every_caller() {
            let _serialised = serialised();
            walk_probe::install();
            walk_probe::reset();
            let mut state = stand(None);

            dispatch_references(&mut state, 40);
            let response = response(&state, 40);

            assert!(response.error.is_none(), "{:?}", response.error);
            assert!(
                walk_probe::entered() >= CALLERS,
                "the walk entered {} files of {CALLERS} callers",
                walk_probe::entered()
            );
        }

        #[test]
        fn a_client_cancel_stops_the_walk_before_its_end() {
            let _serialised = serialised();
            walk_probe::install();
            walk_probe::reset();
            let mut state = stand(None);

            dispatch_references(&mut state, 41);
            walk_probe::await_walk_start();
            cancel(&mut state, 41);
            let response = response(&state, 41);

            let err = response.error.expect("a cancelled walk must not answer with locations");
            assert_eq!(err.code, lsp_server::ErrorCode::RequestCanceled as i32, "{err:?}");
            assert!(
                walk_probe::entered() < CALLERS,
                "the walk ran to the end after the cancel: entered {} of {CALLERS}",
                walk_probe::entered()
            );
        }

        /// One worker, so the second request can only start once the first has let go
        /// of the slot: both walks together entering fewer files than two full walks
        /// proves the cancelled one released it before reaching its end.
        #[test]
        fn a_cancelled_walk_hands_its_only_worker_to_the_next_request() {
            let _serialised = serialised();
            walk_probe::install();
            walk_probe::reset();
            let mut state = stand(Some(1));

            dispatch_references(&mut state, 42);
            walk_probe::await_walk_start();
            cancel(&mut state, 42);
            dispatch_references(&mut state, 43);
            let first = response(&state, 42);
            let second = response(&state, 43);

            assert_eq!(
                first.error.map(|err| err.code),
                Some(lsp_server::ErrorCode::RequestCanceled as i32)
            );
            assert!(second.error.is_none(), "{:?}", second.error);
            assert!(
                walk_probe::entered() < 2 * CALLERS,
                "the cancelled walk kept the worker to its end: entered {} of 2×{CALLERS}",
                walk_probe::entered()
            );
        }
    }
}
