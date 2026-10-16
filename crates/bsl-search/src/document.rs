use crate::domain::{IndexedDocument, SourceSpan};
use crate::error::{EmbeddingFailureCode, SearchError};
use crate::ports::GraphContextProvider;
use crate::token_policy::TokenPolicy;
use crate::workspace_roots::FileKey;
use code_chunk::{Chunk, ChunkKind, Chunker};

#[derive(Debug, Clone)]
pub struct Document {
    pub title: String,
    pub body: String,
    pub kind: String,
}

/// The single embedding-text builder for an indexed code chunk — the one place a
/// chunk's text is composed for both lexical context and vector embedding, across
/// every path (local index, workspace overlay, central publish). It also keys
/// re-embedding (see [`semantic_key_for_indexed_document`]).
///
/// Layout: the BSL-native module path (richer than the raw file path for recall),
/// the kind and symbol, then the optional graph context (dispatch / signature /
/// calls / metadata reads — what the method *does*), then the source. Graph context
/// sits between the header and the body so the vector sees behaviour before syntax;
/// `render()` upstream already newline-terminates each line.
pub fn semantic_text_for_indexed_document(document: &IndexedDocument) -> String {
    semantic_text_from_parts(
        &document.path,
        &document.kind,
        &document.symbol_name,
        document.graph_context.as_deref().unwrap_or(""),
        &document.text,
    )
}

pub fn semantic_key_for_indexed_document(document: &IndexedDocument) -> String {
    blake3::hash(semantic_text_for_indexed_document(document).as_bytes()).to_hex().to_string()
}

/// The single source of truth for the embedding-text layout, expressed over a
/// chunk's raw parts. Both the indexing path (via
/// [`semantic_text_for_indexed_document`]) and the central serving-side key
/// recomputation read through this so the blake3 key matches on both ends — the
/// `rel_path` is folded into the BSL-native module path exactly once, here.
pub fn semantic_text_from_parts(
    rel_path: &str,
    kind: &str,
    symbol_name: &str,
    graph_context: &str,
    text: &str,
) -> String {
    let module_path = crate::context::file_path_to_module_path(rel_path);
    format!("Module: {module_path}\nKind: {kind}\nSymbol: {symbol_name}\n{graph_context}{text}")
}

pub fn semantic_key_from_parts(
    rel_path: &str,
    kind: &str,
    symbol_name: &str,
    graph_context: &str,
    text: &str,
) -> String {
    blake3::hash(
        semantic_text_from_parts(rel_path, kind, symbol_name, graph_context, text).as_bytes(),
    )
    .to_hex()
    .to_string()
}

/// Build the `IndexedDocument` for one code chunk — the single construction point
/// shared by the local index and the workspace overlay, so both carry identical
/// fields (and the same graph context). Graph context is requested from `provider`
/// only for method chunks (procedure / function); module headers and absent
/// providers yield `None`, leaving the embedding text unenriched.
///
/// The flag says the provider failed to render this chunk's context. The document then
/// carries none, and the caller owes the render: a failure stored as "no context" would
/// never be asked for again.
pub(crate) fn indexed_document_for_chunk(
    key: &FileKey,
    chunk: &Chunk,
    source: &str,
    provider: Option<&dyn GraphContextProvider>,
) -> (IndexedDocument, bool) {
    let kind = chunk.kind.label();
    let (graph_context, context_failed) = match (chunk.kind, provider) {
        // The graph reads a module's identity out of the metadata-shaped path,
        // which is the path relative to its own root — an extension repeats that
        // shape, so the root-relative spelling is the one to hand over.
        (ChunkKind::Procedure | ChunkKind::Function, Some(provider)) => {
            match provider.try_graph_context_with_source(key, &chunk.name, kind, source) {
                Ok(context) => (context, false),
                Err(error) => {
                    tracing::debug!(
                        root = %key.root_id,
                        path = %key.path,
                        method = %chunk.name,
                        "graph context render failed; the file owes a re-render: {error}"
                    );
                    (None, true)
                }
            }
        }
        _ => (None, false),
    };
    let document = IndexedDocument {
        collection: "code".to_owned(),
        root_id: key.root_id.clone(),
        path: key.path.clone(),
        symbol_name: chunk.name.clone(),
        kind: kind.to_owned(),
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        content_hash: blake3::hash(chunk.text.as_bytes()).to_hex().to_string(),
        text: chunk.text.clone(),
        graph_context,
        source_span: None,
    };
    (document, context_failed)
}

/// Prepare token-bounded parts from original AST source spans before legacy chunk splitting.
/// `check_singleton` must use the embedder's real serializer and byte ceiling.
pub fn prepare_file_documents(
    key: &FileKey,
    source: &str,
    provider: Option<&dyn GraphContextProvider>,
    policy: &TokenPolicy,
    document_prefix: &str,
    check_singleton: impl FnMut(&str) -> Result<(), SearchError>,
) -> Result<Vec<IndexedDocument>, SearchError> {
    prepare_file_documents_owing(key, source, provider, policy, document_prefix, check_singleton)
        .map(|(documents, _)| documents)
}

/// Like [`prepare_file_documents`], and also says whether the provider failed to render some
/// chunk's context: the caller then owes that file a re-render, as with
/// [`indexed_document_for_chunk`].
pub(crate) fn prepare_file_documents_owing(
    key: &FileKey,
    source: &str,
    provider: Option<&dyn GraphContextProvider>,
    policy: &TokenPolicy,
    document_prefix: &str,
    mut check_singleton: impl FnMut(&str) -> Result<(), SearchError>,
) -> Result<(Vec<IndexedDocument>, bool), SearchError> {
    let mut context_failed = false;
    let documents = prepare_file_documents_with_context(
        key,
        source,
        |chunk| {
            Ok(match (chunk.kind, provider) {
                (ChunkKind::Procedure | ChunkKind::Function, Some(provider)) => {
                    match provider.try_graph_context_with_source(
                        key,
                        &chunk.name,
                        chunk.kind.label(),
                        source,
                    ) {
                        Ok(context) => context,
                        Err(error) => {
                            tracing::debug!(
                                root = %key.root_id,
                                path = %key.path,
                                method = %chunk.name,
                                "graph context render failed; the file owes a re-render: {error}"
                            );
                            context_failed = true;
                            None
                        }
                    }
                }
                _ => None,
            })
        },
        policy,
        document_prefix,
        &mut check_singleton,
    )?;
    Ok((documents, context_failed))
}

pub(crate) fn prepare_file_documents_with_context(
    key: &FileKey,
    source: &str,
    mut render_context: impl FnMut(&Chunk) -> Result<Option<String>, SearchError>,
    policy: &TokenPolicy,
    document_prefix: &str,
    check_singleton: &mut impl FnMut(&str) -> Result<(), SearchError>,
) -> Result<Vec<IndexedDocument>, SearchError> {
    let mut documents = Vec::new();
    for parent in Chunker::source_chunks(source) {
        let chunk = parent.chunk;
        let kind = chunk.kind.label();
        let context = render_context(&chunk)?;
        let full_input = final_document_input(
            key,
            &chunk,
            context.as_deref().unwrap_or(""),
            &chunk.text,
            document_prefix,
        );
        let ranges = if chunk.text.len() > crate::token_policy::MAX_SOURCE_WINDOW_BYTES {
            Some(policy.split_source(&chunk.text, |part_index, slice| {
                check_source_part(
                    key,
                    &chunk,
                    kind,
                    context.as_deref().unwrap_or(""),
                    document_prefix,
                    part_index,
                    slice,
                    policy,
                    check_singleton,
                )
            })?)
        } else {
            match check_prepared_input(policy, &full_input, check_singleton) {
                Ok(()) => None,
                Err(error) if is_input_too_large(&error) => {
                    Some(policy.split_source(&chunk.text, |part_index, slice| {
                        check_source_part(
                            key,
                            &chunk,
                            kind,
                            context.as_deref().unwrap_or(""),
                            document_prefix,
                            part_index,
                            slice,
                            policy,
                            check_singleton,
                        )
                    })?)
                }
                Err(error) => return Err(error),
            }
        };
        match ranges {
            None => documents.push(prepared_document(
                key,
                &chunk,
                context.clone(),
                SourceSpan {
                    parent_symbol: chunk.name.clone(),
                    parent_byte_start: parent.source_byte_start,
                    parent_byte_end: parent.source_byte_end,
                    byte_start: parent.source_byte_start,
                    byte_end: parent.source_byte_end,
                    part_index: 1,
                    part_count: 1,
                },
                chunk.text.clone(),
            )),
            Some(ranges) => {
                let count = ranges.len() as u32;
                for (index, range) in ranges.into_iter().enumerate() {
                    let chunk_text = chunk.text[range.clone()].to_owned();
                    let absolute_start = parent.source_byte_start + range.start as u32;
                    let absolute_end = parent.source_byte_start + range.end as u32;
                    let mut part = chunk.clone();
                    part.name = if chunk.name.is_empty() {
                        format!("Header (часть {})", index + 1)
                    } else {
                        format!("{} (часть {})", chunk.name, index + 1)
                    };
                    part.line_start +=
                        chunk.text[..range.start].bytes().filter(|byte| *byte == b'\n').count()
                            as u32;
                    part.line_end = part.line_start
                        + chunk_text.bytes().filter(|byte| *byte == b'\n').count() as u32
                        + u32::from(!chunk_text.ends_with('\n'));
                    documents.push(prepared_document(
                        key,
                        &part,
                        context.clone(),
                        SourceSpan {
                            parent_symbol: chunk.name.clone(),
                            parent_byte_start: parent.source_byte_start,
                            parent_byte_end: parent.source_byte_end,
                            byte_start: absolute_start,
                            byte_end: absolute_end,
                            part_index: index as u32 + 1,
                            part_count: count,
                        },
                        chunk_text,
                    ));
                }
            }
        }
    }
    Ok(documents)
}

#[allow(clippy::too_many_arguments)]
fn check_source_part(
    key: &FileKey,
    parent: &Chunk,
    kind: &str,
    graph_context: &str,
    document_prefix: &str,
    part_index: u32,
    slice: &str,
    policy: &TokenPolicy,
    check_singleton: &mut impl FnMut(&str) -> Result<(), SearchError>,
) -> Result<(), SearchError> {
    let name = if parent.name.is_empty() {
        format!("Header (часть {part_index})")
    } else {
        format!("{} (часть {part_index})", parent.name)
    };
    let input = semantic_text_from_parts(&key.path, kind, &name, graph_context, slice);
    check_prepared_input(policy, &format!("{document_prefix}{input}"), check_singleton)
}

/// The documents of a file, split by the embedder's token policy when it has one, and whether
/// the provider failed to render some chunk's context (the file then owes a re-render).
pub(crate) fn prepare_indexed_documents(
    key: &FileKey,
    source: &str,
    provider: Option<&dyn GraphContextProvider>,
    embedder: Option<&crate::Embedder>,
) -> Result<(Vec<IndexedDocument>, bool), SearchError> {
    if let Some(embedder) = embedder {
        if let Some(policy) = embedder.token_policy() {
            return prepare_file_documents_owing(
                key,
                source,
                provider,
                policy,
                embedder.document_prefix(),
                |input| embedder.check_singleton_input(input),
            );
        }
    }
    let mut context_failed = false;
    let documents = Chunker::chunk(source)
        .iter()
        .map(|chunk| {
            let (document, failed) = indexed_document_for_chunk(key, chunk, source, provider);
            context_failed |= failed;
            document
        })
        .collect();
    Ok((documents, context_failed))
}

pub(crate) fn chunks_contexts_and_spans(
    documents: &[IndexedDocument],
) -> (Vec<Chunk>, Vec<Option<String>>, Vec<Option<SourceSpan>>) {
    let mut chunks = Vec::with_capacity(documents.len());
    let mut contexts = Vec::with_capacity(documents.len());
    let mut spans = Vec::with_capacity(documents.len());
    for document in documents {
        let kind = match document.kind.as_str() {
            "procedure" => ChunkKind::Procedure,
            "function" => ChunkKind::Function,
            _ => ChunkKind::ModuleHeader,
        };
        chunks.push(Chunk {
            kind,
            name: document.symbol_name.clone(),
            is_export: false,
            annotations: Vec::new(),
            line_start: document.line_start,
            line_end: document.line_end,
            text: document.text.clone(),
        });
        contexts.push(document.graph_context.clone());
        spans.push(document.source_span.clone());
    }
    (chunks, contexts, spans)
}

fn check_prepared_input(
    policy: &TokenPolicy,
    input: &str,
    check_singleton: &mut impl FnMut(&str) -> Result<(), SearchError>,
) -> Result<(), SearchError> {
    policy.check(input)?;
    check_singleton(input)
}

fn final_document_input(
    key: &FileKey,
    chunk: &Chunk,
    graph_context: &str,
    text: &str,
    document_prefix: &str,
) -> String {
    format!(
        "{document_prefix}{}",
        semantic_text_from_parts(&key.path, chunk.kind.label(), &chunk.name, graph_context, text)
    )
}

fn prepared_document(
    key: &FileKey,
    chunk: &Chunk,
    graph_context: Option<String>,
    source_span: SourceSpan,
    text: String,
) -> IndexedDocument {
    IndexedDocument {
        collection: "code".to_owned(),
        root_id: key.root_id.clone(),
        path: key.path.clone(),
        symbol_name: chunk.name.clone(),
        kind: chunk.kind.label().to_owned(),
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        content_hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
        text,
        graph_context,
        source_span: Some(source_span),
    }
}

fn is_input_too_large(error: &SearchError) -> bool {
    error
        .embedding_failure()
        .is_some_and(|failure| failure.code == EmbeddingFailureCode::EmbeddingInputTooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> IndexedDocument {
        IndexedDocument {
            collection: "code".to_owned(),
            root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
            path: "A.bsl".to_owned(),
            symbol_name: "Найти".to_owned(),
            kind: "procedure".to_owned(),
            line_start: 1,
            line_end: 2,
            text: "Возврат 1;".to_owned(),
            content_hash: "h".to_owned(),
            graph_context: None,
            source_span: None,
        }
    }

    #[test]
    fn graph_context_is_folded_between_header_and_body() {
        let plain = doc();
        let enriched = doc().with_graph_context("Dispatch: server | сервер\nCalls: Иная\n");
        assert!(!semantic_text_for_indexed_document(&plain).contains("Dispatch"));
        assert!(semantic_text_for_indexed_document(&enriched)
            .contains("Symbol: Найти\nDispatch: server | сервер\nCalls: Иная\nВозврат 1;"));
        // The embedding cache key tracks the enrichment, so adding/changing context
        // triggers a re-embed.
        assert_ne!(
            semantic_key_for_indexed_document(&plain),
            semantic_key_for_indexed_document(&enriched)
        );
    }

    /// The embedding key must stay deaf to the root, and the cost of breaking that is
    /// paid twice. Every embedding published so far would be orphaned — a full paid
    /// re-embed of the corpus — and the serving side, which recomputes the key from the
    /// columns of a row that has no root to give, would stop matching the publisher's key
    /// for good.
    ///
    /// The property is stated over the key of two documents rather than over "a second
    /// publish calls no embedder", because that phrasing cannot fail: both publishes
    /// compute the key with the same function, so the second hits the first's cache under
    /// ANY recipe, the forbidden one included.
    #[test]
    fn the_embedding_key_ignores_the_root() {
        let configuration = doc();
        let extension = IndexedDocument { root_id: "Расширение".to_owned(), ..doc() };

        assert_eq!(
            semantic_key_for_indexed_document(&configuration),
            semantic_key_for_indexed_document(&extension),
            "the same chunk under a different root embeds to the same vector, so it must \
             keep the same key"
        );
    }

    struct FakeProvider;
    impl GraphContextProvider for FakeProvider {
        fn graph_context(&self, _: &str, symbol_name: &str, _: &str) -> Option<String> {
            Some(format!("Dispatch: server | сервер\nCalls: {symbol_name}Вызов\n"))
        }
    }

    fn chunk(kind: ChunkKind, name: &str, text: &str) -> Chunk {
        Chunk {
            kind,
            name: name.to_owned(),
            is_export: true,
            annotations: Vec::new(),
            line_start: 0,
            line_end: 1,
            text: text.to_owned(),
        }
    }

    #[test]
    fn chunk_document_carries_provider_context_for_methods_only() {
        let path = FileKey::configuration("CommonModules/Сервер/Ext/Module.bsl");
        let provider = FakeProvider;

        // A method chunk gets the provider's context, folded into the embed text.
        let method = chunk(ChunkKind::Procedure, "Делать", "Процедура Делать() КонецПроцедуры");
        let (doc, failed) =
            indexed_document_for_chunk(&path, &method, &method.text, Some(&provider));
        assert!(!failed);
        assert!(doc.graph_context.as_deref().unwrap().contains("Dispatch: server"));
        let embed = semantic_text_for_indexed_document(&doc);
        assert!(embed.contains("Module: ОбщийМодуль.Сервер.Модуль"), "{embed}");
        assert!(embed.contains("Calls: ДелатьВызов"), "{embed}");

        // A module header never gets graph context, even with a provider.
        let (header, _) = indexed_document_for_chunk(
            &path,
            &chunk(ChunkKind::ModuleHeader, "", "Перем А;"),
            "Перем А;",
            Some(&provider),
        );
        assert_eq!(header.graph_context, None);

        // No provider → no context.
        let (plain, failed) = indexed_document_for_chunk(&path, &method, &method.text, None);
        assert_eq!(plain.graph_context, None);
        assert!(!failed, "no provider is no context, not a failed one");
    }

    /// A provider that cannot read its graph leaves the chunk without context AND says so: the
    /// caller must owe the render rather than store the absence as the answer.
    #[test]
    fn a_failed_render_is_reported_not_stored_as_no_context() {
        struct Unreadable;
        impl GraphContextProvider for Unreadable {
            fn graph_context(&self, _: &str, _: &str, _: &str) -> Option<String> {
                None
            }
            fn try_graph_context(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<Option<String>, crate::GraphContextError> {
                Err(crate::GraphContextError("graph unavailable".to_owned()))
            }
        }
        let path = FileKey::configuration("CommonModules/Сервер/Ext/Module.bsl");
        let method = chunk(ChunkKind::Procedure, "Делать", "Процедура Делать() КонецПроцедуры");
        let (doc, failed) =
            indexed_document_for_chunk(&path, &method, &method.text, Some(&Unreadable));
        assert_eq!(doc.graph_context, None);
        assert!(failed);
        let header = chunk(ChunkKind::ModuleHeader, "", "Перем А;");
        assert!(
            !indexed_document_for_chunk(&path, &header, &header.text, Some(&Unreadable)).1,
            "a header is never rendered, so it owes nothing"
        );
    }

    #[test]
    fn blank_graph_context_is_absent_and_does_not_perturb_text() {
        let blank = doc().with_graph_context("   \n");
        assert_eq!(blank.graph_context, None);
        assert_eq!(
            semantic_text_for_indexed_document(&blank),
            semantic_text_for_indexed_document(&doc())
        );
    }

    #[test]
    fn prepared_file_parts_cover_original_source_and_keep_parent_context() {
        let Ok(path) = std::env::var("USER2_TOKENIZER_JSON") else { return };
        let policy = TokenPolicy::load(
            std::path::Path::new(&path),
            "80d0433a2cfc55a4561b0e98b6f822decc48c9d457db498837223f9385ef3aff",
            120,
        )
        .unwrap();
        let source = format!("Процедура Тест()\r\n{}\r\nКонецПроцедуры", "x".repeat(2200));
        let provider = FakeProvider;
        let key = FileKey::configuration("CommonModules/Тест/Ext/Module.bsl");
        let mut accepted_inputs = Vec::new();
        let documents =
            prepare_file_documents(&key, &source, Some(&provider), &policy, "", |input| {
                if input.len() > 1000 {
                    Err(crate::error::EmbeddingFailure::new(
                        EmbeddingFailureCode::EmbeddingInputTooLarge,
                    )
                    .into())
                } else {
                    accepted_inputs.push(input.to_owned());
                    Ok(())
                }
            })
            .unwrap();
        assert!(documents.len() > 1);
        assert!(documents.iter().all(|document| document
            .graph_context
            .as_deref()
            .unwrap()
            .contains("Calls: ТестВызов")));
        let spans = documents
            .iter()
            .map(|document| document.source_span.as_ref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(spans.first().unwrap().part_index, 1);
        assert!(spans.iter().all(|span| span.part_count as usize == documents.len()));
        assert!(spans.windows(2).all(|pair| pair[0].byte_end == pair[1].byte_start));
        assert_eq!(spans.first().unwrap().byte_start, spans.first().unwrap().parent_byte_start);
        assert_eq!(spans.last().unwrap().byte_end, spans.last().unwrap().parent_byte_end);
        let reconstructed =
            documents.iter().map(|document| document.text.as_str()).collect::<String>();
        assert_eq!(reconstructed, source);
        for document in &documents {
            let span = document.source_span.as_ref().unwrap();
            assert_eq!(span.parent_symbol, "Тест");
            assert_eq!(&source[span.byte_start as usize..span.byte_end as usize], document.text);
            assert!(accepted_inputs.contains(&semantic_text_for_indexed_document(document)));
        }

        let header_source = format!("Перем {}", "x".repeat(2200));
        let header_docs =
            prepare_file_documents(&key, &header_source, None, &policy, "", |input| {
                if input.len() > 1000 {
                    Err(crate::error::EmbeddingFailure::new(
                        EmbeddingFailureCode::EmbeddingInputTooLarge,
                    )
                    .into())
                } else {
                    Ok(())
                }
            })
            .unwrap();
        let symbols = header_docs
            .iter()
            .map(|document| document.symbol_name.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(symbols.len(), header_docs.len());
        assert!(header_docs.iter().all(|document| document
            .source_span
            .as_ref()
            .unwrap()
            .parent_symbol
            .is_empty()));

        let large_source = format!("x=1; //{}", "x".repeat(535_405));
        assert_eq!(large_source.len(), 535_412);
        let largest_checked = std::cell::Cell::new(0usize);
        let large_docs = prepare_file_documents(&key, &large_source, None, &policy, "", |input| {
            largest_checked.set(largest_checked.get().max(input.len()));
            Ok(())
        })
        .unwrap();
        assert!(largest_checked.get() <= crate::token_policy::MAX_SOURCE_WINDOW_BYTES + 256);
        assert_eq!(
            large_docs.iter().map(|document| document.text.as_str()).collect::<String>(),
            large_source
        );
        let large_spans = large_docs
            .iter()
            .map(|document| document.source_span.as_ref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(large_spans.first().unwrap().byte_start, 0);
        assert_eq!(large_spans.last().unwrap().byte_end, large_source.len() as u32);
        assert!(large_spans.windows(2).all(|parts| parts[0].byte_end == parts[1].byte_start));
    }
}
