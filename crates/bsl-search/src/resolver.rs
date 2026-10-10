use crate::domain::{BaselineRef, DocumentPath, IndexedDocument, OverlayChange, SearchOverlay};
use crate::error::SearchError;
use crate::ports::ResolvedViewService;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedView {
    baseline: BaselineRef,
    documents: Vec<IndexedDocument>,
}

impl ResolvedView {
    pub fn new(baseline: BaselineRef, documents: Vec<IndexedDocument>) -> Self {
        Self { baseline, documents }
    }

    pub fn baseline(&self) -> &BaselineRef {
        &self.baseline
    }

    pub fn documents(&self) -> &[IndexedDocument] {
        &self.documents
    }

    pub fn documents_in_collection<'a>(
        &'a self,
        collection: &'a str,
    ) -> impl Iterator<Item = &'a IndexedDocument> + 'a {
        self.documents.iter().filter(move |doc| doc.collection == collection)
    }
}

/// What [`ResolvedView`] holds, counted: how many documents the view would carry and
/// how many distinct file paths they span, with no document loaded. A status line
/// reports these two numbers and nothing else of the view, and the view itself is
/// the whole indexed corpus in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedViewCounts {
    baseline: BaselineRef,
    files: usize,
    chunks: usize,
}

impl ResolvedViewCounts {
    pub fn baseline(&self) -> &BaselineRef {
        &self.baseline
    }

    /// Distinct `path` spellings among the view's documents, as a reader counting
    /// [`ResolvedView::documents`] by path would see them.
    pub fn files(&self) -> usize {
        self.files
    }

    /// The view's document count.
    pub fn chunks(&self) -> usize {
        self.chunks
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct InMemoryResolvedViewResolver;

impl InMemoryResolvedViewResolver {
    pub fn resolve(
        &self,
        baseline: BaselineRef,
        baseline_documents: Vec<IndexedDocument>,
        overlay: SearchOverlay,
    ) -> Result<ResolvedView, SearchError> {
        if baseline != overlay.baseline {
            return Err(SearchError::Index(
                "overlay baseline does not match requested baseline".to_owned(),
            ));
        }

        let mut grouped: HashMap<DocumentPath, Vec<IndexedDocument>> = HashMap::new();
        for document in baseline_documents {
            grouped.entry(document.document_path()).or_default().push(document);
        }

        for change in overlay.changes {
            match change {
                OverlayChange::ReplaceFile(file) => {
                    grouped.insert(file.target, file.items);
                }
                OverlayChange::DeleteFile(target) => {
                    grouped.remove(&target);
                }
            }
        }

        let mut documents: Vec<IndexedDocument> = grouped.into_values().flatten().collect();
        documents.sort_by(|lhs, rhs| {
            (
                lhs.collection.as_str(),
                lhs.path.as_str(),
                lhs.line_start,
                lhs.line_end,
                lhs.symbol_name.as_str(),
                lhs.kind.as_str(),
                lhs.content_hash.as_str(),
            )
                .cmp(&(
                    rhs.collection.as_str(),
                    rhs.path.as_str(),
                    rhs.line_start,
                    rhs.line_end,
                    rhs.symbol_name.as_str(),
                    rhs.kind.as_str(),
                    rhs.content_hash.as_str(),
                ))
        });

        Ok(ResolvedView::new(baseline, documents))
    }
}

impl InMemoryResolvedViewResolver {
    /// The counts [`Self::resolve`] would report for the same baseline and overlay,
    /// computed from the baseline's documents-per-file instead of the documents: the
    /// overlay's replace/delete of a file applies to a per-file count exactly as it
    /// applies to a per-file group of documents.
    pub fn resolve_counts(
        &self,
        baseline: BaselineRef,
        baseline_documents_per_file: Vec<(DocumentPath, usize)>,
        overlay: SearchOverlay,
    ) -> Result<ResolvedViewCounts, SearchError> {
        if baseline != overlay.baseline {
            return Err(SearchError::Index(
                "overlay baseline does not match requested baseline".to_owned(),
            ));
        }

        let mut grouped: HashMap<DocumentPath, usize> = HashMap::new();
        for (path, count) in baseline_documents_per_file {
            *grouped.entry(path).or_default() += count;
        }

        for change in overlay.changes {
            match change {
                OverlayChange::ReplaceFile(file) => {
                    grouped.insert(file.target, file.items.len());
                }
                OverlayChange::DeleteFile(target) => {
                    grouped.remove(&target);
                }
            }
        }

        let chunks = grouped.values().sum();
        let files = grouped
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(path, _)| path.path.as_str())
            .collect::<HashSet<_>>()
            .len();
        Ok(ResolvedViewCounts { baseline, files, chunks })
    }
}

impl ResolvedViewService for InMemoryResolvedViewResolver {
    fn resolve_view(
        &self,
        baseline: BaselineRef,
        baseline_documents: Vec<IndexedDocument>,
        overlay: SearchOverlay,
    ) -> Result<ResolvedView, SearchError> {
        self.resolve(baseline, baseline_documents, overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::{InMemoryResolvedViewResolver, ResolvedViewService};
    use crate::domain::{BaselineRef, CorpusId, DocumentPath, IndexedDocument, SearchOverlay};

    fn baseline() -> BaselineRef {
        BaselineRef::for_snapshot(CorpusId::WorkspaceCode, "main@abc123")
    }

    struct DocFixture<'a> {
        collection: &'a str,
        path: &'a str,
        symbol_name: &'a str,
        kind: &'a str,
        line_start: u32,
        line_end: u32,
        text: &'a str,
        content_hash: &'a str,
    }

    fn doc(fixture: DocFixture<'_>) -> IndexedDocument {
        IndexedDocument {
            collection: fixture.collection.to_owned(),
            root_id: crate::CONFIGURATION_ROOT_ID.to_owned(),
            path: fixture.path.to_owned(),
            symbol_name: fixture.symbol_name.to_owned(),
            kind: fixture.kind.to_owned(),
            line_start: fixture.line_start,
            line_end: fixture.line_end,
            text: fixture.text.to_owned(),
            content_hash: fixture.content_hash.to_owned(),
            graph_context: None,
            source_span: None,
        }
    }

    #[test]
    fn keeps_baseline_documents_when_overlay_is_empty() {
        let baseline_ref = baseline();
        let documents = vec![
            doc(DocFixture {
                collection: "code",
                path: "CommonModules/A.bsl",
                symbol_name: "Header",
                kind: "header",
                line_start: 0,
                line_end: 0,
                text: "module",
                content_hash: "h1",
            }),
            doc(DocFixture {
                collection: "code",
                path: "CommonModules/A.bsl",
                symbol_name: "DoWork",
                kind: "function",
                line_start: 10,
                line_end: 20,
                text: "function body",
                content_hash: "h2",
            }),
        ];
        let overlay = SearchOverlay::new(baseline_ref.clone());

        let view = InMemoryResolvedViewResolver
            .resolve_view(baseline_ref.clone(), documents.clone(), overlay)
            .unwrap();

        assert_eq!(view.baseline(), &baseline_ref);
        assert_eq!(view.documents(), documents.as_slice());
    }

    #[test]
    fn replaces_one_file_without_touching_other_baseline_files() {
        let baseline_ref = baseline();
        let baseline_documents = vec![
            doc(DocFixture {
                collection: "code",
                path: "CommonModules/A.bsl",
                symbol_name: "OldProcedure",
                kind: "procedure",
                line_start: 1,
                line_end: 5,
                text: "old body",
                content_hash: "old-a",
            }),
            doc(DocFixture {
                collection: "code",
                path: "CommonModules/B.bsl",
                symbol_name: "StableProcedure",
                kind: "procedure",
                line_start: 1,
                line_end: 5,
                text: "stable body",
                content_hash: "stable-b",
            }),
        ];
        let mut overlay = SearchOverlay::new(baseline_ref.clone());
        overlay.replace_file(
            DocumentPath::configuration("code", "CommonModules/A.bsl"),
            vec![doc(DocFixture {
                collection: "code",
                path: "CommonModules/A.bsl",
                symbol_name: "NewProcedure",
                kind: "procedure",
                line_start: 1,
                line_end: 7,
                text: "new body",
                content_hash: "new-a",
            })],
        );

        let view = InMemoryResolvedViewResolver
            .resolve_view(baseline_ref, baseline_documents, overlay)
            .unwrap();

        let symbols: Vec<&str> =
            view.documents().iter().map(|document| document.symbol_name.as_str()).collect();
        assert_eq!(symbols, vec!["NewProcedure", "StableProcedure"]);
    }

    #[test]
    fn deletes_file_from_resolved_view() {
        let baseline_ref = baseline();
        let baseline_documents = vec![
            doc(DocFixture {
                collection: "code",
                path: "CommonModules/A.bsl",
                symbol_name: "ProcedureA",
                kind: "procedure",
                line_start: 1,
                line_end: 5,
                text: "body a",
                content_hash: "hash-a",
            }),
            doc(DocFixture {
                collection: "code",
                path: "CommonModules/B.bsl",
                symbol_name: "ProcedureB",
                kind: "procedure",
                line_start: 1,
                line_end: 5,
                text: "body b",
                content_hash: "hash-b",
            }),
        ];
        let mut overlay = SearchOverlay::new(baseline_ref.clone());
        overlay.delete_file(DocumentPath::configuration("code", "CommonModules/A.bsl"));

        let view = InMemoryResolvedViewResolver
            .resolve_view(baseline_ref, baseline_documents, overlay)
            .unwrap();

        assert_eq!(view.documents().len(), 1);
        assert_eq!(view.documents()[0].path, "CommonModules/B.bsl");
    }

    #[test]
    fn collection_views_see_baseline_and_overlay_documents_together() {
        let baseline_ref = baseline();
        let baseline_documents = vec![
            doc(DocFixture {
                collection: "code",
                path: "CommonModules/A.bsl",
                symbol_name: "ProcedureA",
                kind: "procedure",
                line_start: 1,
                line_end: 5,
                text: "body a",
                content_hash: "a",
            }),
            doc(DocFixture {
                collection: "platform",
                path: "String/Trim",
                symbol_name: "Trim",
                kind: "method",
                line_start: 0,
                line_end: 0,
                text: "doc body",
                content_hash: "p",
            }),
        ];
        let mut overlay = SearchOverlay::new(baseline_ref.clone());
        overlay.replace_file(
            DocumentPath::configuration("code", "CommonModules/B.bsl"),
            vec![doc(DocFixture {
                collection: "code",
                path: "CommonModules/B.bsl",
                symbol_name: "ProcedureB",
                kind: "procedure",
                line_start: 1,
                line_end: 5,
                text: "body b",
                content_hash: "b",
            })],
        );

        let view = InMemoryResolvedViewResolver
            .resolve_view(baseline_ref, baseline_documents, overlay)
            .unwrap();

        let code_symbols: Vec<&str> = view
            .documents_in_collection("code")
            .map(|document| document.symbol_name.as_str())
            .collect();
        let platform_symbols: Vec<&str> = view
            .documents_in_collection("platform")
            .map(|document| document.symbol_name.as_str())
            .collect();

        assert_eq!(code_symbols, vec!["ProcedureA", "ProcedureB"]);
        assert_eq!(platform_symbols, vec!["Trim"]);
    }

    /// The counted view agrees with the resolved one under an overlay that replaces
    /// one file, empties another and deletes a third — and differs from the bare
    /// baseline's counts, so the overlay is what the test exercises.
    #[test]
    fn resolve_counts_matches_resolve_under_an_overlay() {
        let docs = vec![
            doc(DocFixture {
                collection: "code",
                path: "A.bsl",
                symbol_name: "a1",
                kind: "procedure",
                line_start: 1,
                line_end: 2,
                text: "a1",
                content_hash: "h1",
            }),
            doc(DocFixture {
                collection: "code",
                path: "A.bsl",
                symbol_name: "a2",
                kind: "procedure",
                line_start: 3,
                line_end: 4,
                text: "a2",
                content_hash: "h2",
            }),
            doc(DocFixture {
                collection: "code",
                path: "B.bsl",
                symbol_name: "b1",
                kind: "procedure",
                line_start: 1,
                line_end: 2,
                text: "b1",
                content_hash: "h3",
            }),
            doc(DocFixture {
                collection: "code",
                path: "C.bsl",
                symbol_name: "c1",
                kind: "procedure",
                line_start: 1,
                line_end: 2,
                text: "c1",
                content_hash: "h4",
            }),
            doc(DocFixture {
                collection: "code",
                path: "D.bsl",
                symbol_name: "d1",
                kind: "procedure",
                line_start: 1,
                line_end: 2,
                text: "d1",
                content_hash: "h5",
            }),
        ];
        let per_file: Vec<(DocumentPath, usize)> = {
            let mut grouped: std::collections::HashMap<DocumentPath, usize> = Default::default();
            for document in &docs {
                *grouped.entry(document.document_path()).or_default() += 1;
            }
            grouped.into_iter().collect()
        };
        let mut overlay = SearchOverlay::new(baseline());
        overlay.replace_file(
            DocumentPath::new("code", crate::CONFIGURATION_ROOT_ID, "A.bsl"),
            vec![
                doc(DocFixture {
                    collection: "code",
                    path: "A.bsl",
                    symbol_name: "a1",
                    kind: "procedure",
                    line_start: 1,
                    line_end: 2,
                    text: "a1'",
                    content_hash: "h6",
                }),
                doc(DocFixture {
                    collection: "code",
                    path: "A.bsl",
                    symbol_name: "a2",
                    kind: "procedure",
                    line_start: 3,
                    line_end: 4,
                    text: "a2'",
                    content_hash: "h7",
                }),
                doc(DocFixture {
                    collection: "code",
                    path: "A.bsl",
                    symbol_name: "a3",
                    kind: "procedure",
                    line_start: 5,
                    line_end: 6,
                    text: "a3",
                    content_hash: "h8",
                }),
            ],
        );
        overlay.replace_file(
            DocumentPath::new("code", crate::CONFIGURATION_ROOT_ID, "B.bsl"),
            Vec::new(),
        );
        overlay.delete_file(DocumentPath::new("code", crate::CONFIGURATION_ROOT_ID, "C.bsl"));

        let resolver = InMemoryResolvedViewResolver;
        let view = resolver.resolve(baseline(), docs.clone(), overlay.clone()).unwrap();
        let view_files: std::collections::HashSet<&str> =
            view.documents().iter().map(|document| document.path.as_str()).collect();
        let counts = resolver.resolve_counts(baseline(), per_file.clone(), overlay).unwrap();

        assert_eq!(counts.chunks(), view.documents().len());
        assert_eq!(counts.files(), view_files.len());
        assert_eq!(
            (counts.files(), counts.chunks()),
            (2, 4),
            "A replaced by 3, B emptied, C deleted, D kept"
        );
        let bare =
            resolver.resolve_counts(baseline(), per_file, SearchOverlay::new(baseline())).unwrap();
        assert_eq!(
            (bare.files(), bare.chunks()),
            (4, 5),
            "the control: the overlay moved both counts"
        );
    }
}
