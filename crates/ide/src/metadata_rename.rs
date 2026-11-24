//! Read-only metadata rename preview. XML coverage and snapshot-bound apply are separate.

pub(crate) mod query_output;
pub(crate) mod xml_target;

use hir::metadata_symbol::{
    MetadataIdentityError, MetadataIdentityResolver, MetadataSymbolIdentity, MetadataSymbolPath,
};
use hir::{DefDatabase, Name};
use ide_db::base_db::{RootQueryDb, SourceDatabase, BSL_SOURCE_ROOT};
use ide_db::{RootDatabase, RootDatabaseImpl};
use rustc_hash::FxHashSet;
use salsa::Database;
use syntax::{SyntaxKind, TextRange, TextSize};
use vfs::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataReferenceLanguage {
    Bsl,
    Sdbl,
}

/// The exact existing bytes are part of the preview, not an instruction to apply them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRenameEdit {
    pub file_id: FileId,
    pub range: TextRange,
    pub old_text: String,
    pub new_text: String,
    pub language: MetadataReferenceLanguage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataPreviewIssueKind {
    XmlCoverageRequired,
    SnapshotBindingRequired,
    FileLimit,
    UnreadFile,
    ParseErrors,
    DynamicReference,
    IncompleteQuery,
    UnresolvedReference(MetadataIdentityError),
    UnattestedQuerySource,
    MissingQueryIndex,
    InvalidQuery,
    QueryOutputContract,
    QueryReference(sdbl_hir::metadata_reference::MetadataReferenceIssueKind),
    UnmappableQueryRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPreviewIssue {
    pub file_id: Option<FileId>,
    pub range: Option<TextRange>,
    pub kind: MetadataPreviewIssueKind,
}

/// Known edits stay available even when an uncovered consumer blocks application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRenamePreview {
    pub target: MetadataSymbolIdentity,
    pub path: MetadataSymbolPath,
    pub edits: Vec<MetadataRenameEdit>,
    pub files_read: Vec<FileId>,
    pub issues: Vec<MetadataPreviewIssue>,
}

impl MetadataRenamePreview {
    pub fn can_apply(&self) -> bool {
        self.issues.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataPreviewError {
    InvalidIdentifier,
    NotMetadata,
    Identity(MetadataIdentityError),
}

pub fn metadata_rename_preview_at(
    db: &RootDatabaseImpl,
    file_id: FileId,
    offset: TextSize,
    new_name: &str,
    max_files: usize,
) -> Result<MetadataRenamePreview, MetadataPreviewError> {
    let target = metadata_target_at(db, file_id, offset)?;
    metadata_rename_preview(db, file_id, &target.path, new_name, max_files)
}

pub(crate) fn metadata_reference_search_at(
    db: &RootDatabaseImpl,
    file_id: FileId,
    offset: TextSize,
    max_files: usize,
) -> Result<MetadataRenamePreview, MetadataPreviewError> {
    let target = metadata_target_at(db, file_id, offset)?;
    metadata_reference_search(db, file_id, &target.path, max_files)
}

pub(crate) fn metadata_reference_search(
    db: &RootDatabaseImpl,
    file_id: FileId,
    path: &MetadataSymbolPath,
    max_files: usize,
) -> Result<MetadataRenamePreview, MetadataPreviewError> {
    let spelling =
        path.members.last().map(|member| member.name.as_str()).unwrap_or(path.owner_name.as_str());
    collect_metadata_uses(db, file_id, path, spelling, max_files)
}

/// Resolve one BSL, query, or supported XML occurrence without walking its references.
pub fn metadata_target_at(
    db: &RootDatabaseImpl,
    file_id: FileId,
    offset: TextSize,
) -> Result<hir::metadata_symbol::MetadataUse, MetadataPreviewError> {
    if db.file_source_root_input(file_id).source_root_id(db)
        == ide_db::base_db::METADATA_SOURCE_ROOT
    {
        return xml_target::metadata_target_in_xml(db, file_id, offset);
    }
    if let Some(target) = hir::metadata_symbol::metadata_use_at(db, file_id, offset)
        .map_err(MetadataPreviewError::Identity)?
    {
        return Ok(target);
    }
    let text = db.file_text(file_id);
    let queries = db.all_sdbl_in_file(file_id);
    let packages = db.sdbl_hir_in_file(file_id);
    let sources = query_source_ranges(db, file_id, &db.parse(file_id).syntax_node());
    let mut target = None;
    for (id, query) in queries.iter().filter(|(_, query)| {
        query.bsl_literal_range.contains(offset)
            && !query.incomplete
            && query.is_valid()
            && sources.iter().any(|source| source.range.contains_range(query.bsl_literal_range))
    }) {
        let Some((_, package)) = packages.iter().find(|(package_id, _)| package_id == id) else {
            continue;
        };
        if package
            .queries()
            .iter()
            .any(|query| query.hir.diagnostics.iter().any(sdbl_hir::SdblDiagnostic::is_error))
        {
            continue;
        }
        for reference in package.source_map.metadata_references() {
            let Some(range) = query_reference_range(&text, query, reference.range) else {
                continue;
            };
            if !(range.start() <= offset && offset <= range.end()) {
                continue;
            }
            let path = MetadataSymbolPath {
                owner_kind: reference.owner_kind,
                owner_name: Name::new(&reference.owner_name),
                members: reference.members.clone(),
            };
            let identity =
                db.metadata_identity(file_id, &path).map_err(MetadataPreviewError::Identity)?;
            if target.as_ref().is_some_and(|previous: &hir::metadata_symbol::MetadataUse| {
                previous.identity != identity
            }) {
                return Err(MetadataPreviewError::Identity(
                    MetadataIdentityError::AmbiguousDeclaration,
                ));
            }
            target = Some(hir::metadata_symbol::MetadataUse { identity, path, range });
        }
    }
    target.ok_or(MetadataPreviewError::NotMetadata)
}

pub fn metadata_rename_preview(
    db: &RootDatabaseImpl,
    anchor_file: FileId,
    path: &MetadataSymbolPath,
    new_name: &str,
    max_files: usize,
) -> Result<MetadataRenamePreview, MetadataPreviewError> {
    if !crate::rename::is_valid_identifier(new_name)
        || parser::parse_sdbl(&format!("SELECT T.{new_name} FROM Catalog.{new_name} AS T"))
            .has_errors()
    {
        return Err(MetadataPreviewError::InvalidIdentifier);
    }
    let mut result = collect_metadata_uses(db, anchor_file, path, new_name, max_files)?;
    result.edits.retain(|edit| edit.old_text != edit.new_text);
    Ok(result)
}

fn collect_metadata_uses(
    db: &RootDatabaseImpl,
    anchor_file: FileId,
    path: &MetadataSymbolPath,
    new_name: &str,
    max_files: usize,
) -> Result<MetadataRenamePreview, MetadataPreviewError> {
    let target = db.metadata_identity(anchor_file, path).map_err(MetadataPreviewError::Identity)?;
    let names = hir::metadata_symbol::metadata_reference_names(db, anchor_file, path);
    if names.is_empty() {
        return Err(MetadataPreviewError::NotMetadata);
    }
    let mut result = MetadataRenamePreview {
        target,
        path: path.clone(),
        edits: Vec::new(),
        files_read: Vec::new(),
        issues: vec![
            MetadataPreviewIssue {
                file_id: None,
                range: None,
                kind: MetadataPreviewIssueKind::XmlCoverageRequired,
            },
            MetadataPreviewIssue {
                file_id: None,
                range: None,
                kind: MetadataPreviewIssueKind::SnapshotBindingRequired,
            },
        ],
    };
    let Some(source) = db.try_source_root_input(BSL_SOURCE_ROOT) else {
        return Ok(result);
    };
    let usage = db.name_usage_index(BSL_SOURCE_ROOT);
    let mut bsl_candidates = FxHashSet::default();
    for name in &names {
        bsl_candidates.extend(usage.files_with(&hir::normalize_usage_name(name)).iter().copied());
    }
    let mut files = source.root(db).iter().collect::<Vec<_>>();
    files.sort_by_key(|file| (!bsl_candidates.contains(file), file.0));
    if files.len() > max_files {
        result.issues.push(MetadataPreviewIssue {
            file_id: None,
            range: None,
            kind: MetadataPreviewIssueKind::FileLimit,
        });
        files.truncate(max_files);
    }
    for file_id in files {
        db.unwind_if_revision_cancelled();
        result.files_read.push(file_id);
        if db.file_is_unread(file_id) {
            issue(&mut result, file_id, None, MetadataPreviewIssueKind::UnreadFile);
            continue;
        }
        let text = db.file_text(file_id);
        let parse = db.parse(file_id);
        let root = parse.syntax_node();
        if parse.has_errors() {
            issue(&mut result, file_id, None, MetadataPreviewIssueKind::ParseErrors);
            continue;
        }
        {
            let offsets = db.file_name_offsets_ref(file_id);
            let mut candidates = FxHashSet::default();
            for name in &names {
                candidates
                    .extend(offsets.offsets(&hir::normalize_match_name(name)).iter().copied());
            }
            let mut candidates = candidates.into_iter().collect::<Vec<_>>();
            candidates.sort_unstable();
            for offset in candidates {
                match hir::metadata_symbol::metadata_use_at(db, file_id, offset.into()) {
                    Ok(Some(reference)) if reference.identity == result.target => push_edit(
                        &mut result,
                        file_id,
                        reference.range,
                        &text,
                        new_name,
                        MetadataReferenceLanguage::Bsl,
                    ),
                    Err(error) => issue(
                        &mut result,
                        file_id,
                        Some(TextRange::empty(offset.into())),
                        MetadataPreviewIssueKind::UnresolvedReference(error),
                    ),
                    _ => {}
                }
            }
        }
        index_references(db, file_id, &root, &text, &names, new_name, &mut result);
        query_references(db, file_id, &root, &text, new_name, &mut result);
    }
    result.files_read.push(result.target.declaration_file);
    result.files_read.sort_by_key(|id| id.0);
    result.files_read.dedup();
    result.edits.sort_by_key(|edit| (edit.file_id.0, edit.range.start(), edit.range.end()));
    result.edits.dedup_by(|left, right| left.file_id == right.file_id && left.range == right.range);
    Ok(result)
}

fn index_references(
    db: &RootDatabaseImpl,
    file_id: FileId,
    root: &syntax::SyntaxNode,
    text: &str,
    names: &[Name],
    new_name: &str,
    result: &mut MetadataRenamePreview,
) {
    for index in root.descendants().filter(|node| node.kind() == SyntaxKind::INDEX_EXPR) {
        let mut children = index.children();
        let Some(receiver) = children.next() else {
            continue;
        };
        let Some(key) = children.next() else { continue };
        if !hir::metadata_symbol::metadata_receiver_has_members(db, file_id, &receiver) {
            continue;
        }
        let strings = key
            .descendants_with_tokens()
            .filter_map(|item| item.into_token())
            .filter(|token| !token.kind().is_trivia())
            .collect::<Vec<_>>();
        let [token] = strings.as_slice() else {
            issue(
                result,
                file_id,
                Some(key.text_range()),
                MetadataPreviewIssueKind::DynamicReference,
            );
            continue;
        };
        if token.kind() != SyntaxKind::STRING {
            issue(
                result,
                file_id,
                Some(key.text_range()),
                MetadataPreviewIssueKind::DynamicReference,
            );
            continue;
        }
        let raw = token.text();
        let Some(content) = raw.strip_prefix('"').and_then(|raw| raw.strip_suffix('"')) else {
            continue;
        };
        if content.contains('"')
            || !names.iter().any(|name| stdx::case::eq_ignore_case(content, name.as_str()))
        {
            continue;
        }
        let range = TextRange::new(
            token.text_range().start() + TextSize::from(1),
            token.text_range().end() - TextSize::from(1),
        );
        match hir::metadata_symbol::metadata_member_use(
            db,
            file_id,
            &receiver,
            &Name::new(content),
            range,
        ) {
            Ok(Some(reference)) if reference.identity == result.target => {
                push_edit(result, file_id, range, text, new_name, MetadataReferenceLanguage::Bsl)
            }
            Err(error) => issue(
                result,
                file_id,
                Some(range),
                MetadataPreviewIssueKind::UnresolvedReference(error),
            ),
            _ => {}
        }
    }
}

fn query_references(
    db: &RootDatabaseImpl,
    file_id: FileId,
    root: &syntax::SyntaxNode,
    text: &str,
    new_name: &str,
    result: &mut MetadataRenamePreview,
) {
    let queries = db.all_sdbl_in_file(file_id);
    let packages = db.sdbl_hir_in_file(file_id);
    let sources = query_source_ranges(db, file_id, root);
    for source in sources.iter().filter(|source| source.requires_text) {
        if !queries.iter().any(|(_, query)| {
            source.range.contains_range(query.bsl_literal_range)
                && !query.incomplete
                && query.is_valid()
        }) {
            issue(result, file_id, Some(source.range), MetadataPreviewIssueKind::DynamicReference);
        }
    }
    for (id, query) in queries.iter() {
        if !sources.iter().any(|source| source.range.contains_range(query.bsl_literal_range)) {
            issue(
                result,
                file_id,
                Some(query.bsl_literal_range),
                MetadataPreviewIssueKind::UnattestedQuerySource,
            );
            continue;
        }
        if query.incomplete || !query.is_valid() {
            issue(
                result,
                file_id,
                Some(query.bsl_literal_range),
                MetadataPreviewIssueKind::IncompleteQuery,
            );
            continue;
        }
        let Some((_, package)) = packages.iter().find(|(package_id, _)| package_id == id) else {
            issue(
                result,
                file_id,
                Some(query.bsl_literal_range),
                MetadataPreviewIssueKind::MissingQueryIndex,
            );
            continue;
        };
        if package
            .queries()
            .iter()
            .any(|query| query.hir.diagnostics.iter().any(sdbl_hir::SdblDiagnostic::is_error))
        {
            issue(
                result,
                file_id,
                Some(query.bsl_literal_range),
                MetadataPreviewIssueKind::InvalidQuery,
            );
            continue;
        }
        for problem in package.source_map.metadata_reference_issues() {
            let range = query_reference_range(text, query, problem.range)
                .unwrap_or(query.bsl_literal_range);
            issue(
                result,
                file_id,
                Some(range),
                MetadataPreviewIssueKind::QueryReference(problem.reason),
            );
        }
        for reference in package.source_map.metadata_references() {
            let path = MetadataSymbolPath {
                owner_kind: reference.owner_kind,
                owner_name: Name::new(&reference.owner_name),
                members: reference.members.clone(),
            };
            match db.metadata_identity(file_id, &path) {
                Ok(identity) if identity == result.target => {
                    let Some(range) = query_reference_range(text, query, reference.range) else {
                        issue(
                            result,
                            file_id,
                            Some(query.bsl_literal_range),
                            MetadataPreviewIssueKind::UnmappableQueryRange,
                        );
                        continue;
                    };
                    push_edit(
                        result,
                        file_id,
                        range,
                        text,
                        new_name,
                        MetadataReferenceLanguage::Sdbl,
                    );
                }
                Err(error) => issue(
                    result,
                    file_id,
                    Some(query.bsl_literal_range),
                    MetadataPreviewIssueKind::UnresolvedReference(error),
                ),
                _ => {}
            }
        }
        let container_path = MetadataSymbolPath {
            owner_kind: result.path.owner_kind,
            owner_name: result.path.owner_name.clone(),
            members: result.path.members[..result.path.members.len().saturating_sub(1)].to_vec(),
        };
        let container_identity =
            db.metadata_identity(result.target.declaration_file, &container_path);
        let mut references = Vec::new();
        let mut containers = Vec::new();
        for reference in package.source_map.metadata_references() {
            let path = MetadataSymbolPath {
                owner_kind: reference.owner_kind,
                owner_name: Name::new(&reference.owner_name),
                members: reference.members.clone(),
            };
            if let Ok(identity) = db.metadata_identity(file_id, &path) {
                if identity == result.target {
                    references.push(reference.range);
                }
                if container_identity.as_ref().is_ok_and(|container| container == &identity) {
                    containers.push(reference.range);
                }
            }
        }
        if let Err(reason) = container_identity {
            issue(
                result,
                file_id,
                Some(query.bsl_literal_range),
                MetadataPreviewIssueKind::UnresolvedReference(reason),
            );
        }
        let policy = query_output::metadata_query_output_policy(
            package,
            &references,
            &containers,
            result.path.members.is_empty(),
        );
        for range in policy.incomplete {
            issue(
                result,
                file_id,
                query_reference_range(text, query, range),
                MetadataPreviewIssueKind::QueryOutputContract,
            );
        }
        for (range, alias) in policy.preserve_aliases {
            let Some(source_range) = query_reference_range(text, query, range) else {
                continue;
            };
            if let Some(edit) = result.edits.iter_mut().find(|edit| {
                edit.file_id == file_id
                    && edit.range == source_range
                    && edit.language == MetadataReferenceLanguage::Sdbl
                    && edit.new_text != edit.old_text
            }) {
                edit.new_text = format!("{} AS {}", edit.new_text, alias);
            }
        }
    }
}

fn issue(
    result: &mut MetadataRenamePreview,
    file_id: FileId,
    range: Option<TextRange>,
    kind: MetadataPreviewIssueKind,
) {
    result.issues.push(MetadataPreviewIssue { file_id: Some(file_id), range, kind });
}

fn push_edit(
    result: &mut MetadataRenamePreview,
    file_id: FileId,
    range: TextRange,
    text: &str,
    new_name: &str,
    language: MetadataReferenceLanguage,
) {
    if let Some(old) = ranged_text(text, range) {
        result.edits.push(MetadataRenameEdit {
            file_id,
            range,
            old_text: old.to_string(),
            new_text: new_name.into(),
            language,
        });
    }
}

fn ranged_text(text: &str, range: TextRange) -> Option<&str> {
    text.get(usize::from(range.start())..usize::from(range.end()))
}

fn query_reference_range(
    text: &str,
    query: &syntax::SdblQueryInfo,
    range: TextRange,
) -> Option<TextRange> {
    let literal = ranged_text(text, query.bsl_literal_range)?;
    let relative = match &query.literal_map {
        Some(map) => map.map_range_to_literal(range),
        None => syntax::sdbl_query::map_range_query_to_literal(literal, range),
    };
    let source = relative + query.bsl_literal_range.start();
    let expected = ranged_text(&query.query_text, range)?;
    (ranged_text(text, source)? == expected).then_some(source)
}

struct QuerySource {
    range: TextRange,
    requires_text: bool,
}

fn query_source_ranges(
    db: &RootDatabaseImpl,
    file_id: FileId,
    root: &syntax::SyntaxNode,
) -> Vec<QuerySource> {
    let mut result = Vec::new();
    for node in root.descendants() {
        match node.kind() {
            SyntaxKind::NEW_EXPR
                if hir::metadata_symbol::is_query_receiver_at(db, file_id, &node) =>
            {
                let requires_text = node
                    .children()
                    .find(|child| child.kind() == SyntaxKind::ARG_LIST)
                    .is_some_and(|args| args.children().next().is_some());
                result.push(QuerySource { range: node.text_range(), requires_text });
            }
            SyntaxKind::ASSIGN_STMT => {
                let Some(left) = node.children().next() else {
                    continue;
                };
                let Some(field) =
                    left.descendants().find(|node| node.kind() == SyntaxKind::FIELD_EXPR)
                else {
                    continue;
                };
                let Some(token) = syntax::ast_utils::field_tail_name_token(&field) else {
                    continue;
                };
                if !matches!(token.text().to_lowercase().as_str(), "текст" | "text") {
                    continue;
                }
                let hir::NameClass::FieldName { receiver, .. } = hir::classify_token(&token) else {
                    continue;
                };
                if hir::metadata_symbol::is_query_receiver_at(db, file_id, &receiver) {
                    result.push(QuerySource { range: node.text_range(), requires_text: true });
                }
            }
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_metadata::declaration_identity::DeclarationSegment;
    use bsl_metadata::MdoType;
    use ide_db::base_db::{SourceRoot, METADATA_SOURCE_ROOT};
    use ide_db::metadata::{MdoEntry, MetadataListingData};
    use std::path::Path;
    use vfs::{file_set::FileSet, VfsPath};

    const BASE: FileId = FileId(1);
    const EXTENSION: FileId = FileId(2);
    const DECLARATION: FileId = FileId(100);
    const OTHER_DECLARATION: FileId = FileId(101);
    const EXTENSION_DECLARATION: FileId = FileId(102);

    fn fixture_path(path: impl AsRef<std::path::Path>) -> std::path::PathBuf {
        std::path::absolute(path).unwrap()
    }

    fn declaration(
        name: &str,
        owner: u128,
        field: u128,
        ownership: &str,
        field_ownership: &str,
    ) -> String {
        format!("<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses' xmlns:v8='http://v8.1c.ru/8.1/data/core' xmlns:xs='http://www.w3.org/2001/XMLSchema'><Catalog uuid='{}'><Properties><Name>{name}</Name>{ownership}</Properties><ChildObjects><Attribute uuid='{}'><Properties><Name>Caption</Name>{field_ownership}<Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute></ChildObjects></Catalog></MetaDataObject>",bsl_metadata::Uuid::from_u128(owner),bsl_metadata::Uuid::from_u128(field))
    }

    fn database(
        base: &str,
        extension: &str,
        extension_ownership: &str,
        field_ownership: &str,
    ) -> RootDatabaseImpl {
        let mut db = RootDatabaseImpl::new();
        db.set_all_config_paths(
            vec![
                (None, fixture_path("/rename/base")),
                (Some("Extension".into()), fixture_path("/rename/extension")),
            ],
            None,
        );
        let mut sources = FileSet::new();
        for (id, path, text) in [
            (BASE, "/rename/base/CommonModules/Test/Ext/Module.bsl", base),
            (EXTENSION, "/rename/extension/CommonModules/Test/Ext/Module.bsl", extension),
        ] {
            sources.insert(id, VfsPath::new(fixture_path(path)));
            db.set_file_source_root(id, BSL_SOURCE_ROOT);
            db.set_file_text(id, text);
        }
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(sources));
        let mut metadata = FileSet::new();
        let adopted = extension_ownership.contains(">Adopted<");
        let declarations = [
            (
                DECLARATION,
                "/rename/base/Catalogs/Partners.xml",
                declaration("Partners", 1, 2, "", ""),
            ),
            (
                OTHER_DECLARATION,
                "/rename/base/Catalogs/Customers.xml",
                declaration("Customers", 3, 4, "", ""),
            ),
            (
                EXTENSION_DECLARATION,
                "/rename/extension/Catalogs/Partners.xml",
                declaration(
                    "Partners",
                    if adopted { 11 } else { 1 },
                    if adopted { 12 } else { 2 },
                    extension_ownership,
                    field_ownership,
                ),
            ),
        ];
        for (id, path, text) in declarations {
            metadata.insert(id, VfsPath::new(fixture_path(path)));
            db.set_file_source_root(id, METADATA_SOURCE_ROOT);
            db.set_file_text(id, &text);
        }
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(metadata));
        db.set_metadata_listing(
            fixture_path("/rename/base").to_str().unwrap(),
            MetadataListingData {
                entries: vec![
                    MdoEntry {
                        kind: MdoType::Catalog,
                        name: "Partners".into(),
                        main: DECLARATION,
                        predefined: None,
                    },
                    MdoEntry {
                        kind: MdoType::Catalog,
                        name: "Customers".into(),
                        main: OTHER_DECLARATION,
                        predefined: None,
                    },
                ],
                ..MetadataListingData::default()
            },
        );
        db.set_metadata_listing(
            fixture_path("/rename/extension").to_str().unwrap(),
            MetadataListingData {
                entries: vec![MdoEntry {
                    kind: MdoType::Catalog,
                    name: "Partners".into(),
                    main: EXTENSION_DECLARATION,
                    predefined: None,
                }],
                ..MetadataListingData::default()
            },
        );
        db.set_workspace_load_complete(true);
        db.use_prepared_metadata_inputs();
        db
    }

    fn target() -> MetadataSymbolPath {
        MetadataSymbolPath {
            owner_kind: MdoType::Catalog,
            owner_name: Name::new("Partners"),
            members: vec![DeclarationSegment { kind: "Attribute".into(), name: "Caption".into() }],
        }
    }

    const BSL:&str="// Параметры:\n// P - СправочникСсылка.Partners\n// C - СправочникСсылка.Customers\nПроцедура Тест(Unknown)\n P = Справочники.Partners.ПустаяСсылка();\n C = Справочники.Customers.ПустаяСсылка();\n A = P.Caption;\n B = C.Caption;\n D = P[\"Caption\"];\n S = Новый Структура(\"Caption\", 1);\n E = S.Caption;\n Текст = \"Caption\";\n F = Unknown.Caption;\nКонецПроцедуры\n";

    #[test]
    fn metadata_preview_separates_same_named_fields_and_source_sets() {
        let db = database(
            BSL,
            BSL,
            "<ObjectBelonging>Own</ObjectBelonging>",
            "<ObjectBelonging>Own</ObjectBelonging>",
        );
        let preview = metadata_rename_preview(&db, BASE, &target(), "Heading", 32).unwrap();
        assert_eq!(preview.target.declaration_file, DECLARATION);
        assert_eq!(preview.edits.len(), 2, "{preview:#?}");
        assert!(preview
            .edits
            .iter()
            .all(|edit| edit.file_id == BASE && edit.old_text == "Caption"));
        assert!(preview
            .issues
            .iter()
            .any(|issue| matches!(issue.kind, MetadataPreviewIssueKind::UnresolvedReference(_))));
        assert!(!preview.can_apply());
        let other = metadata_rename_preview(&db, EXTENSION, &target(), "Heading", 32).unwrap();
        assert_eq!(other.target.declaration_file, EXTENSION_DECLARATION);
        assert!(other.edits.iter().all(|edit| edit.file_id == EXTENSION));
        let at = TextSize::from(BSL.find("P.Caption").unwrap() as u32 + 2);
        assert!(crate::rename::prepare_rename(&db, BASE, at).is_none());
    }

    #[test]
    fn metadata_preview_adopted_uuid_uses_the_real_declaration() {
        let owner=format!("<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>{}</ExtendedConfigurationObject>",bsl_metadata::Uuid::from_u128(1));
        let member=format!("<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>{}</ExtendedConfigurationObject>",bsl_metadata::Uuid::from_u128(2));
        let db = database(BSL, BSL, &owner, &member);
        let preview = metadata_rename_preview(&db, EXTENSION, &target(), "Heading", 32).unwrap();
        assert_eq!(preview.target.declaration_file, DECLARATION);
        assert_eq!(preview.edits.len(), 4, "{preview:#?}");
        assert!(preview.edits.iter().any(|edit| edit.file_id == BASE));
        assert!(preview.edits.iter().any(|edit| edit.file_id == EXTENSION));
    }

    #[test]
    fn metadata_preview_keeps_bare_owner_fields_separate_from_declared_locals() {
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let source = "Процедура Тест()\n Caption = Caption + \"x\";\nКонецПроцедуры\n";
        let object = FileId(7);
        let mut files = db.source_root_input(BSL_SOURCE_ROOT).root(&db).file_set().clone();
        files.insert(
            object,
            VfsPath::new(fixture_path("/rename/base/Catalogs/Partners/Ext/ObjectModule.bsl")),
        );
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(files));
        db.set_file_source_root(object, BSL_SOURCE_ROOT);
        db.set_file_text(object, source);
        assert!(
            db.module_metadata(hir::ModuleId::new(object)).mdo.is_some(),
            "missing module owner"
        );
        let preview = metadata_rename_preview(&db, object, &target(), "Heading", 32).unwrap();
        assert_eq!(preview.edits.len(), 2, "{preview:#?}");
        let shadow = "Процедура Тест()\n Перем Caption;\n Caption = 1;\nКонецПроцедуры\n";
        db.set_file_text(object, shadow);
        let preview = metadata_rename_preview(&db, object, &target(), "Heading", 32).unwrap();
        assert!(preview.edits.is_empty(), "{preview:#?}");
        let local = crate::rename::rename(
            &db,
            object,
            TextSize::from(shadow.find("Caption").unwrap() as u32),
            "Local",
        )
        .unwrap();
        assert_eq!(local.len(), 2);
        db.set_file_text(object, "Процедура Тест(Caption)\n A = Caption;\nКонецПроцедуры\n");
        let parameter = metadata_rename_preview(&db, object, &target(), "Heading", 32).unwrap();
        assert!(parameter.edits.is_empty(), "{parameter:#?}");
    }

    #[test]
    fn metadata_preview_maps_sdbl_aliases_escaped_quotes_unicode_and_crlf() {
        let source="Процедура Тест()\r\n Q = Новый Запрос;\r\n Q.Текст = \"SELECT \"\"Привет Caption\"\", P.Caption, C.Caption\r\n |FROM Catalog.Partners AS P, Catalog.Customers AS C\";\r\n Сообщение = \"SELECT P.Caption FROM Catalog.Partners AS P\";\r\nКонецПроцедуры\r\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let preview = metadata_rename_preview(&db, BASE, &target(), "Заголовок", 32).unwrap();
        assert_eq!(preview.edits.len(), 1, "{preview:#?}");
        let edit = &preview.edits[0];
        assert_eq!(edit.language, MetadataReferenceLanguage::Sdbl);
        assert_eq!(edit.old_text, "Caption");
        assert_eq!(
            edit.range.start(),
            TextSize::from(source.find("P.Caption").unwrap() as u32 + 2)
        );
        assert_eq!(&source[edit.range], "Caption");
        let anchored =
            metadata_rename_preview_at(&db, BASE, edit.range.start(), "Заголовок", 32).unwrap();
        assert_eq!(anchored.target, preview.target);
        assert_eq!(anchored.edits, preview.edits);
    }

    #[test]
    fn metadata_preview_reports_dynamic_queries_and_incomplete_index_without_edits() {
        let source="Процедура Тест(Дополнение)\n Q = Новый Запрос;\n Q.Текст = \"SELECT P.Caption FROM Catalog.Partners AS P\" + Дополнение;\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let preview = metadata_rename_preview(&db, BASE, &target(), "Heading", 32).unwrap();
        assert!(preview.edits.is_empty(), "{preview:#?}");
        assert!(preview.issues.iter().any(|issue| matches!(
            issue.kind,
            MetadataPreviewIssueKind::DynamicReference | MetadataPreviewIssueKind::IncompleteQuery
        )));
        let capped = metadata_rename_preview(&db, BASE, &target(), "Heading", 0).unwrap();
        assert!(capped
            .issues
            .iter()
            .any(|issue| issue.kind == MetadataPreviewIssueKind::FileLimit));
        assert!(!capped.can_apply());
    }

    #[test]
    fn metadata_identity_requires_extension_owner_provenance() {
        let db = database("", "", "<ObjectBelonging/>", "");
        assert_eq!(
            db.metadata_identity(EXTENSION, &target()),
            Err(MetadataIdentityError::UnprovedAdoption)
        );
    }

    #[test]
    fn metadata_query_output_consumers_are_preserved_or_explicitly_partial() {
        let source = "Процедура Тест()\n Q = Новый Запрос(\"ВЫБРАТЬ P.Caption ИЗ Справочник.Partners КАК P\");\n R = Q.Выполнить().Выбрать();\n Пока R.Следующий() Цикл\n X = R.Caption;\n КонецЦикла;\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let preview = metadata_rename_preview(&db, BASE, &target(), "Heading", 32).unwrap();
        assert!(
            preview.edits.iter().any(|edit| edit.new_text.contains("AS Caption"))
                || preview
                    .issues
                    .iter()
                    .any(|issue| issue.kind == MetadataPreviewIssueKind::QueryOutputContract),
            "{preview:#?}"
        );
    }

    #[test]
    fn metadata_query_unload_consumers_keep_the_output_name() {
        let source = "Процедура Тест()\n Q = Новый Запрос(\"SELECT P.Caption FROM Catalog.Partners AS P\");\n T = Q.Execute().Unload();\n X = T[0].Caption;\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let preview = metadata_rename_preview(&db, BASE, &target(), "Heading", 32).unwrap();
        assert!(
            preview.edits.iter().any(|edit| edit.new_text.contains("AS Caption"))
                || preview
                    .issues
                    .iter()
                    .any(|issue| issue.kind == MetadataPreviewIssueKind::QueryOutputContract),
            "{preview:#?}"
        );
    }

    #[test]
    fn metadata_query_target_wildcard_is_explicitly_partial() {
        let source = "Процедура Тест()\n Q = Новый Запрос(\"SELECT P.* FROM Catalog.Partners AS P\");\n X = Q.Execute().Unload()[0].Caption;\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let preview = metadata_rename_preview(&db, BASE, &target(), "Heading", 32).unwrap();
        assert!(
            preview
                .issues
                .iter()
                .any(|issue| issue.kind == MetadataPreviewIssueKind::QueryOutputContract),
            "{preview:#?}"
        );
    }

    #[test]
    fn metadata_query_unrelated_wildcard_is_not_a_target_output_dependency() {
        let source = "Процедура Тест()\n Q = Новый Запрос(\"SELECT C.* FROM Catalog.Customers AS C\");\n S = Новый Структура(\"Caption\", 1);\n X = S.Caption;\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let preview = metadata_rename_preview(&db, BASE, &target(), "Heading", 32).unwrap();
        assert!(
            !preview
                .issues
                .iter()
                .any(|issue| issue.kind == MetadataPreviewIssueKind::QueryOutputContract),
            "{preview:#?}"
        );
        assert!(preview.edits.is_empty());
    }

    #[test]
    fn metadata_query_object_rename_preserves_implicit_table_qualifier() {
        let source = "Процедура Тест()\n Q = Новый Запрос(\"SELECT Partners.Caption FROM Catalog.Partners WHERE Partners.Caption = &Value\");\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let mut path = target();
        path.members.clear();
        let preview = metadata_rename_preview(&db, BASE, &path, "Buyers", 32).unwrap();
        assert_eq!(preview.edits.len(), 1, "{preview:#?}");
        assert_eq!(preview.edits[0].new_text, "Buyers AS Partners");
        assert!(!preview
            .issues
            .iter()
            .any(|issue| issue.kind == MetadataPreviewIssueKind::QueryOutputContract));
    }

    #[test]
    fn metadata_query_object_rename_keeps_explicit_alias_and_other_owner() {
        let source = "Процедура Тест()\n Q = Новый Запрос(\"SELECT P.Caption, C.Caption FROM Catalog.Partners AS P, Catalog.Customers AS C\");\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let mut path = target();
        path.members.clear();
        let preview = metadata_rename_preview(&db, BASE, &path, "Buyers", 32).unwrap();
        assert_eq!(preview.edits.len(), 1, "{preview:#?}");
        assert_eq!(preview.edits[0].old_text, "Partners");
        assert_eq!(preview.edits[0].new_text, "Buyers");
    }

    #[test]
    fn metadata_public_references_share_cursor_identity_and_report_partial() {
        let source = "Процедура Тест()\n P = Справочники.Partners.ПустаяСсылка();\n X = P.Caption;\n Q = Новый Запрос(\"SELECT P.Caption AS Stable FROM Catalog.Partners AS P\");\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let offset = TextSize::from(source.find("P.Caption").unwrap() as u32 + 2);
        let target = metadata_target_at(&db, BASE, offset).unwrap();
        let result = crate::references::find_references_at_rich(&db, BASE, offset, true, 32);
        assert_eq!(target.identity.declaration_file, DECLARATION);
        assert_eq!(result.hits.len(), 3, "{result:#?}");
        let crate::references::ReferencesOutcome::Partial { reasons } = result.outcome else {
            panic!("metadata coverage must be explicit")
        };
        assert!(reasons
            .iter()
            .any(|issue| issue.kind == MetadataPreviewIssueKind::XmlCoverageRequired));
        assert!(!reasons
            .iter()
            .any(|issue| issue.kind == MetadataPreviewIssueKind::SnapshotBindingRequired));
        let query_offset = TextSize::from(source.rfind("P.Caption").unwrap() as u32 + 2);
        assert_eq!(metadata_target_at(&db, BASE, query_offset).unwrap().identity, target.identity);
        let xml = db.file_text(DECLARATION);
        let xml_offset = TextSize::from(xml.find("Caption").unwrap() as u32);
        assert_eq!(
            metadata_target_at(&db, DECLARATION, xml_offset).unwrap().identity,
            target.identity
        );
        let xml_result =
            crate::references::find_references_at_rich(&db, DECLARATION, xml_offset, true, 32);
        assert_eq!(xml_result.hits, result.hits);
        let without_declaration =
            crate::references::find_references_at_rich(&db, BASE, offset, false, 32);
        assert_eq!(without_declaration.hits.len(), 2);
        let mut request = crate::ReferencesRequest {
            anchor: crate::ReferenceAnchor::Name("Catalog.Partners.Caption".into()),
            anchor_files: None,
            area: crate::ReferenceArea::default(),
            kinds: None,
            include_declaration: true,
            max_files: 32,
        };
        let ambiguous = crate::find_references_by_name(&db, &request, &[]);
        assert_eq!(ambiguous.outcome, crate::ReferencesOutcome::Ambiguous);
        request.anchor_files = Some([BASE].into_iter().collect());
        let anchored = crate::find_references_by_name(&db, &request, &[]);
        assert!(
            matches!(anchored.outcome, crate::ReferencesOutcome::Partial { .. }),
            "{anchored:#?}"
        );
        assert_eq!(anchored.hits, result.hits);
    }

    fn add_xml(db: &mut RootDatabaseImpl, id: FileId, path: &str, text: &str) {
        let mut files = db.source_root_input(METADATA_SOURCE_ROOT).root(db).file_set().clone();
        files.insert(id, VfsPath::new(fixture_path(path)));
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(files));
        db.set_file_source_root(id, METADATA_SOURCE_ROOT);
        db.set_file_text(id, text);
    }

    fn plan_snapshot(
        db: &RootDatabaseImpl,
    ) -> crate::metadata_rename_plan::PreparedMetadataRenameSnapshot {
        use crate::metadata_rename_plan::*;
        use std::collections::BTreeMap;
        use std::path::PathBuf;
        let roots = [("base", MetadataRootRole::Main), ("extension", MetadataRootRole::Extension)]
            .map(|(id, role)| MetadataPlanRoot {
                id: id.into(),
                source_path: fixture_path(format!("/rename/{id}")),
                canonical_path: fixture_path(format!("/rename/{id}")),
                role,
                writable: true,
                case_sensitive: true,
            })
            .to_vec();
        let mut files = Vec::new();
        let mut directories: BTreeMap<(String, PathBuf), Vec<MetadataDirectoryEntry>> =
            roots.iter().map(|root| ((root.id.clone(), PathBuf::new()), Vec::new())).collect();
        for source in [BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT] {
            let input = db.source_root_input(source);
            let source_root = input.root(db);
            for id in source_root.iter() {
                let path = source_root.file_set().path_for_file(&id).unwrap().as_path();
                let root = roots.iter().find(|root| path.starts_with(&root.source_path)).unwrap();
                let relative = path.strip_prefix(&root.source_path).unwrap().to_path_buf();
                let mut child = relative.clone();
                let mut kind = MetadataDirectoryEntryKind::File;
                while let Some(parent) = child.parent() {
                    let entry = MetadataDirectoryEntry {
                        name: child.file_name().unwrap().to_str().unwrap().into(),
                        kind,
                    };
                    let entries =
                        directories.entry((root.id.clone(), parent.to_path_buf())).or_default();
                    if !entries.contains(&entry) {
                        entries.push(entry);
                    }
                    child = parent.to_path_buf();
                    kind = MetadataDirectoryEntryKind::Directory;
                }
                files.push(PreparedMetadataRenameFile {
                    file_id: Some(id),
                    root_id: root.id.clone(),
                    relative_path: relative,
                    kind: if source == BSL_SOURCE_ROOT {
                        MetadataPlanFileKind::Bsl
                    } else {
                        MetadataPlanFileKind::Xml
                    },
                    bytes: std::sync::Arc::from(db.file_text(id).as_bytes()),
                    open_document: None,
                    effective_form_attributes: None,
                });
            }
        }
        PreparedMetadataRenameSnapshot {
            roots,
            files,
            directories: directories
                .into_iter()
                .map(|((root_id, relative_path), entries)| MetadataDirectorySnapshot {
                    root_id,
                    relative_path,
                    entries,
                })
                .collect(),
            context_sha256: [1; 32],
            unindexed_paths: Vec::new(),
        }
    }

    fn add_rename_collision_field(db: &mut RootDatabaseImpl, file: FileId, name: &str) {
        let old = db.file_text(file);
        let field = format!("<Attribute uuid='{}'><Properties><Name>{name}</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>", bsl_metadata::Uuid::from_u128(0x34));
        db.set_file_text(
            file,
            &old.replacen("</ChildObjects>", &format!("{field}</ChildObjects>"), 1),
        );
    }

    #[test]
    fn metadata_rename_member_collision_refuses_other_uuid_in_the_same_owner() {
        for (existing, requested) in
            [("Heading", "Heading"), ("Heading", "hEADING"), ("Заголовок", "зАГОЛОВОК")]
        {
            let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
            add_rename_collision_field(&mut db, DECLARATION, existing);
            let snapshot = plan_snapshot(&db);
            let plan = crate::metadata_rename_plan::prepare_metadata_rename_plan(
                &db,
                BASE,
                &target(),
                requested,
                &snapshot,
                64,
            )
            .unwrap();
            assert!(
                !plan.can_apply(),
                "a different sibling UUID must reserve {requested}: {plan:#?}"
            );
            assert_eq!(
                db.file_text(DECLARATION).matches(&format!("<Name>{existing}</Name>")).count(),
                1
            );
        }
    }

    #[test]
    fn metadata_rename_member_name_in_another_owner_remains_available() {
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        add_rename_collision_field(&mut db, OTHER_DECLARATION, "Heading");
        let plan = crate::metadata_rename_plan::prepare_metadata_rename_plan(
            &db,
            BASE,
            &target(),
            "Heading",
            &plan_snapshot(&db),
            64,
        )
        .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        assert!(plan.files.iter().all(|file| !file.relative_path.ends_with("Customers.xml")));
    }

    #[test]
    fn metadata_rename_same_member_identity_allows_unchanged_or_case_only_spelling() {
        let db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        for name in ["Caption", "cAPTION"] {
            let plan = crate::metadata_rename_plan::prepare_metadata_rename_plan(
                &db,
                BASE,
                &target(),
                name,
                &plan_snapshot(&db),
                64,
            )
            .unwrap();
            assert!(plan.can_apply(), "{name}: {:#?}", plan.problems);
            assert!(plan.resources.is_empty());
        }
    }

    #[test]
    fn metadata_rename_member_collision_covers_field_namespace_and_standard_aliases() {
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let original = db.file_text(DECLARATION);
        let section = format!("<TabularSection uuid='{}'><Properties><Name>Rows</Name></Properties><ChildObjects/></TabularSection>", bsl_metadata::Uuid::from_u128(0x35));
        db.set_file_text(
            DECLARATION,
            &original.replacen("</ChildObjects>", &format!("{section}</ChildObjects>"), 1),
        );
        for name in ["Rows", "rOWS", "Ссылка", "Ref", "rEF"] {
            let plan = crate::metadata_rename_plan::prepare_metadata_rename_plan(
                &db,
                BASE,
                &target(),
                name,
                &plan_snapshot(&db),
                64,
            )
            .unwrap();
            assert!(
                plan.problems
                    .contains(&crate::metadata_rename_plan::MetadataPlanProblem::NameCollision),
                "{name}: {:#?}",
                plan.problems
            );
            assert!(!plan.can_apply());
        }
    }

    #[test]
    fn metadata_rename_nested_member_collision_uses_its_row_scope() {
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        add_rename_collision_field(&mut db, DECLARATION, "RootOnly");
        let original = db.file_text(DECLARATION);
        let section = "<TabularSection uuid='00000000-0000-0000-0000-000000000035'><Properties><Name>Rows</Name></Properties><ChildObjects><Attribute uuid='00000000-0000-0000-0000-000000000036'><Properties><Name>Caption</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute><Attribute uuid='00000000-0000-0000-0000-000000000037'><Properties><Name>Heading</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute></ChildObjects></TabularSection>";
        db.set_file_text(
            DECLARATION,
            &original.replacen("</ChildObjects>", &format!("{section}</ChildObjects>"), 1),
        );
        let mut path = target();
        path.members
            .insert(0, DeclarationSegment { kind: "TabularSection".into(), name: "Rows".into() });
        for name in ["Heading", "LineNumber", "НомерСтроки"] {
            let plan = crate::metadata_rename_plan::prepare_metadata_rename_plan(
                &db,
                BASE,
                &path,
                name,
                &plan_snapshot(&db),
                64,
            )
            .unwrap();
            assert!(!plan.can_apply(), "{name}: {:#?}", plan.problems);
        }
        let plan = crate::metadata_rename_plan::prepare_metadata_rename_plan(
            &db,
            BASE,
            &path,
            "RootOnly",
            &plan_snapshot(&db),
            64,
        )
        .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
    }

    #[test]
    fn metadata_rename_member_collision_is_checked_in_each_adopted_owner_context() {
        for redeclared in [true, false] {
            let owner = format!("<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>{}</ExtendedConfigurationObject>", bsl_metadata::Uuid::from_u128(1));
            let field = format!("<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>{}</ExtendedConfigurationObject>", bsl_metadata::Uuid::from_u128(2));
            let mut db = database("", "", &owner, &field);
            let mut original = db.file_text(EXTENSION_DECLARATION).to_string();
            if !redeclared {
                let start = original.find("<Attribute ").unwrap();
                let end = original.find("</Attribute>").unwrap() + "</Attribute>".len();
                original.replace_range(start..end, "");
            }
            let sibling = "<Attribute uuid='00000000-0000-0000-0000-000000000038'><Properties><Name>Heading</Name><ObjectBelonging>Own</ObjectBelonging><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>";
            db.set_file_text(
                EXTENSION_DECLARATION,
                &original.replacen("</ChildObjects>", &format!("{sibling}</ChildObjects>"), 1),
            );
            let plan = crate::metadata_rename_plan::prepare_metadata_rename_plan(
                &db,
                BASE,
                &target(),
                "Heading",
                &plan_snapshot(&db),
                64,
            )
            .unwrap();
            assert!(
                !plan.can_apply(),
                "an adopted owner cannot collide with its own field: {plan:#?}"
            );
            assert!(plan
                .problems
                .contains(&crate::metadata_rename_plan::MetadataPlanProblem::NameCollision));
        }
    }

    #[test]
    fn metadata_xml_plan_binding_accepts_the_snapshot_canonical_root_spelling() {
        use crate::metadata_rename_plan::*;
        let source = "Процедура Тест()\r\n P = Справочники.Partners.ПустаяСсылка();\r\n X = P.Caption;\r\nКонецПроцедуры\r\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        for case_sensitive in [true, false] {
            let mut snapshot = plan_snapshot(&db);
            for root in &mut snapshot.roots {
                root.source_path = fixture_path(format!("/declared/{}", root.id));
                root.case_sensitive = case_sensitive;
                assert_ne!(root.source_path, root.canonical_path);
            }
            let plan = prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &snapshot, 64)
                .unwrap();
            assert!(plan.can_apply(), "{:#?}", plan.problems);
            assert!(plan.files.iter().any(|file| {
                file.root_id == "base"
                    && file.relative_path == Path::new("CommonModules/Test/Ext/Module.bsl")
                    && std::str::from_utf8(&file.after_bytes).unwrap().contains("P.Heading")
            }));
        }
    }

    #[test]
    fn metadata_xml_plan_binding_rejects_an_unrelated_resident_root_with_identical_bytes() {
        use crate::metadata_rename_plan::*;
        let source = "Процедура Тест()\n P = Справочники.Partners.ПустаяСсылка();\n X = P.Caption;\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let mut snapshot = plan_snapshot(&db);
        for root in &mut snapshot.roots {
            root.source_path = fixture_path(format!("/declared/{}", root.id));
            root.canonical_path = fixture_path(format!("/unrelated/{}", root.id));
            root.case_sensitive = false;
        }
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &snapshot, 64).unwrap();
        assert!(!plan.can_apply());
        assert!(plan.problems.contains(&MetadataPlanProblem::ResidentTextMismatch));
        assert!(plan.files.is_empty());
    }

    #[test]
    fn metadata_xml_plan_preserves_uuid_bytes_and_query_output_contract() {
        use crate::metadata_rename_plan::*;
        let source = "Процедура Тест()\r\n P = Справочники.Partners.ПустаяСсылка();\r\n X = P.Caption;\r\nКонецПроцедуры\r\n";
        let mut db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let declaration = format!("\u{feff}{}\r\n", db.file_text(DECLARATION));
        db.set_file_text(DECLARATION, &declaration);
        let composition = include_str!("../../bsl-metadata/fixtures/rename/composition.xml")
            .replace(" WHERE P.Code = &amp;Code", "");
        add_xml(
            &mut db,
            FileId(200),
            "/rename/base/Reports/Report/Templates/Layout/Ext/Template.xml",
            &composition,
        );
        let snapshot = plan_snapshot(&db);
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &snapshot, 64).unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        assert_eq!(plan.files.len(), 3, "{plan:#?}");
        let xml = plan
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("Catalogs/Partners.xml"))
            .unwrap();
        assert_eq!(
            xml.after_bytes,
            declaration.replacen("<Name>Caption</Name>", "<Name>Heading</Name>", 1).as_bytes()
        );
        let layout = plan
            .files
            .iter()
            .find(|file| file.relative_path.to_string_lossy().ends_with("Template.xml"))
            .unwrap();
        let after = std::str::from_utf8(&layout.after_bytes).unwrap();
        assert!(after.contains("P.Heading AS Stable"));
        assert!(after
            .contains("<dataPath>Stable</dataPath><field>Stable</field><title>Caption</title>"));
        assert!(plan.resources.is_empty());
        assert_eq!(plan.read_set.len(), snapshot.files.len());
    }

    #[test]
    fn metadata_xml_plan_renames_object_resources_and_confirmed_command_paths() {
        use crate::metadata_rename_plan::*;
        let source = "Процедура Тест()\n Q = Новый Запрос(\"SELECT Partners.Caption FROM Catalog.Partners\");\nКонецПроцедуры\n";
        let mut db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        add_xml(
            &mut db,
            FileId(200),
            "/rename/base/Ext/CommandInterface.xml",
            include_str!("../../bsl-metadata/fixtures/rename/commands.xml"),
        );
        let snapshot = plan_snapshot(&db);
        let mut path = target();
        path.members.clear();
        let plan = prepare_metadata_rename_plan(&db, BASE, &path, "Buyers", &snapshot, 64).unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        assert_eq!(plan.resources.len(), 1, "{plan:#?}");
        assert_eq!(plan.resources[0].to_relative, Path::new("Catalogs/Buyers.xml"));
        let command = plan
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("Ext/CommandInterface.xml"))
            .unwrap();
        let after = std::str::from_utf8(&command.after_bytes).unwrap();
        assert!(after.contains("Catalog.Buyers.StandardCommand.OpenList"), "{after}");
        assert!(after.contains("Catalog.Customers.StandardCommand.OpenList"));
        let bsl = plan
            .files
            .iter()
            .find(|file| file.relative_path.to_string_lossy().ends_with("Module.bsl"))
            .unwrap();
        assert!(std::str::from_utf8(&bsl.after_bytes)
            .unwrap()
            .contains("Catalog.Buyers AS Partners"));
    }

    #[test]
    fn metadata_xml_plan_covers_every_prepared_corpus_surface_without_global_text_replacement() {
        use crate::metadata_rename_plan::*;
        let mut db = database("", "", "<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>00000000-0000-0000-0000-000000000001</ExtendedConfigurationObject>", "");
        let corpus = [
            ("base-additions", "Catalogs/BA252Other.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/Catalogs/BA252Other.xml")),
            ("base-additions", "Catalogs/BA252Partners/Templates/BA252Layout/Ext/Template.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/Catalogs/BA252Partners/Templates/BA252Layout/Ext/Template.xml")),
            ("base-additions", "Catalogs/BA252Partners/Templates/BA252Layout.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/Catalogs/BA252Partners/Templates/BA252Layout.xml")),
            ("base-additions", "Catalogs/BA252Partners.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/Catalogs/BA252Partners.xml")),
            ("base-additions", "CommonForms/BA252Card/Ext/Form.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/CommonForms/BA252Card/Ext/Form.xml")),
            ("base-additions", "CommonForms/BA252Card.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/CommonForms/BA252Card.xml")),
            ("base-additions", "CommonModules/BA252References/Ext/Module.bsl", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/CommonModules/BA252References/Ext/Module.bsl")),
            ("base-additions", "CommonModules/BA252References.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/CommonModules/BA252References.xml")),
            ("base-additions", "ExchangePlans/BA252Exchange/Ext/Content.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/ExchangePlans/BA252Exchange/Ext/Content.xml")),
            ("base-additions", "ExchangePlans/BA252Exchange.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/ExchangePlans/BA252Exchange.xml")),
            ("base-additions", "Ext/CommandInterface.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/base-additions/Ext/CommandInterface.xml")),
            ("extension", "Catalogs/BA252Partners.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/extension/Catalogs/BA252Partners.xml")),
            ("extension", "CommonForms/BA252OverlayCard/Ext/Form.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/extension/CommonForms/BA252OverlayCard/Ext/Form.xml")),
            ("extension", "CommonForms/BA252OverlayCard.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/extension/CommonForms/BA252OverlayCard.xml")),
            ("extension", "CommonModules/BA252OverlayReferences/Ext/Module.bsl", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/extension/CommonModules/BA252OverlayReferences/Ext/Module.bsl")),
            ("extension", "CommonModules/BA252OverlayReferences.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/extension/CommonModules/BA252OverlayReferences.xml")),
            ("extension", "Configuration.xml", include_str!("../../bsl-metadata/fixtures/rename/full-corpus/extension/Configuration.xml")),
        ];
        let mut bsl_files = FileSet::new();
        for (index, (root, relative, text)) in corpus.iter().enumerate() {
            let root = if *root == "base-additions" { "base" } else { "extension" };
            let relative =
                relative.replace("BA252Partners", "Partners").replace("BA252Other", "Customers");
            let text = text.replace("BA252Partners", "Partners").replace("BA252Other", "Customers");
            let path = fixture_path(format!("/rename/{root}/{relative}"));
            if relative.ends_with(".bsl") {
                let id = if root == "base" { BASE } else { EXTENSION };
                bsl_files.insert(id, VfsPath::new(path));
                db.set_file_text(id, &text);
            } else {
                let id = match (root, relative.as_str()) {
                    ("base", "Catalogs/Partners.xml") => DECLARATION,
                    ("base", "Catalogs/Customers.xml") => OTHER_DECLARATION,
                    ("extension", "Catalogs/Partners.xml") => EXTENSION_DECLARATION,
                    _ => FileId(200 + index as u32),
                };
                add_xml(&mut db, id, path.to_str().unwrap(), &text);
            }
        }
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(bsl_files));
        let snapshot = plan_snapshot(&db);
        assert_eq!(snapshot.files.len(), 17);
        let mut object = target();
        object.members.clear();
        for (target, new_name) in [(target(), "Heading"), (object, "Buyers")] {
            let plan =
                prepare_metadata_rename_plan(&db, BASE, &target, new_name, &snapshot, 128).unwrap();
            assert!(plan.can_apply(), "{:?}", plan.problems);
            assert_eq!(plan.read_set.len(), snapshot.files.len());
            for file in &plan.files {
                assert_ne!(file.relative_path, Path::new("Catalogs/Customers.xml"));
                if file.relative_path.extension().is_some_and(|extension| extension == "bsl") {
                    let after = std::str::from_utf8(&file.after_bytes).unwrap();
                    assert!(
                        after.contains("OtherDot = O.Caption;")
                            && after.contains("OtherKey = O[\"Caption\"]")
                    );
                    assert!(
                        after.contains("StructureKey = S.Caption;")
                            && after.contains("CaptionText = \"Caption\";")
                    );
                    assert!(after.contains("ObjectText = \"Partners\";"));
                }
            }
            let changed = |suffix: &str| -> Vec<String> {
                plan.files
                    .iter()
                    .filter(|file| file.relative_path.ends_with(suffix))
                    .map(|file| String::from_utf8(file.after_bytes.clone()).unwrap())
                    .collect()
            };
            let forms = changed("Ext/Form.xml");
            assert_eq!(forms.len(), 2);
            for after in forms {
                assert!(after.contains("<DataPath>Other.Caption</DataPath>"));
                assert!(after.contains(if new_name == "Heading" {
                    "<DataPath>Partner.Heading</DataPath>"
                } else {
                    "cfg:CatalogRef.Buyers"
                }));
            }
            let dcs = changed("BA252Layout/Ext/Template.xml");
            assert_eq!(dcs.len(), 1);
            assert!(dcs[0].contains("Other.Caption") && dcs[0].contains("StableCaption"));
            assert!(dcs[0].contains(if new_name == "Heading" {
                "Partner.Heading"
            } else {
                "Catalog.Buyers"
            }));
            if new_name == "Buyers" {
                assert!(changed("Ext/CommandInterface.xml")[0]
                    .contains("Catalog.Buyers.StandardCommand.OpenList"));
                assert!(changed("Ext/Content.xml")[0].contains("Catalog.Buyers"));
                assert!(plan
                    .resources
                    .iter()
                    .any(|resource| resource.from_relative == Path::new("Catalogs/Partners")
                        && resource.to_relative == Path::new("Catalogs/Buyers")));
            }
        }
    }

    #[test]
    fn metadata_xml_plan_keeps_unrelated_full_native_generated_types_in_the_read_set() {
        use crate::metadata_rename_plan::*;
        let source = "Процедура Тест()\n P = Справочники.Partners.ПустаяСсылка();\n X = P.Caption;\nКонецПроцедуры\n";
        let mut db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let fixtures = [
            (
                "Documents/Partners.xml",
                include_str!("../../bsl-metadata/fixtures/rename/generated-types/document.xml"),
            ),
            (
                "Documents/NativeReceipt.xml",
                include_str!(
                    "../../bsl-metadata/fixtures/rename/generated-types/document-tables.xml"
                ),
            ),
            (
                "Catalogs/NativeCatalog.xml",
                include_str!(
                    "../../bsl-metadata/fixtures/rename/generated-types/catalog-tables.xml"
                ),
            ),
            (
                "ExchangePlans/NativeExchange.xml",
                include_str!(
                    "../../bsl-metadata/fixtures/rename/generated-types/exchange-plan.xml"
                ),
            ),
            (
                "Reports/NativeReport.xml",
                include_str!("../../bsl-metadata/fixtures/rename/generated-types/report.xml"),
            ),
        ];
        for (index, (relative, xml)) in fixtures.iter().enumerate() {
            add_xml(&mut db, FileId(200 + index as u32), &format!("/rename/base/{relative}"), xml);
        }
        let snapshot = plan_snapshot(&db);
        let mut object = target();
        object.members.clear();
        for (path, new_name) in [(target(), "Heading"), (object, "Buyers")] {
            let plan =
                prepare_metadata_rename_plan(&db, BASE, &path, new_name, &snapshot, 64).unwrap();
            assert!(plan.can_apply(), "{:?}", plan.problems);
            assert_eq!(plan.read_set.len(), snapshot.files.len());
            assert_eq!(plan.files.len(), 2);
            for (relative, xml) in fixtures {
                assert!(!plan.files.iter().any(|file| file.relative_path == Path::new(relative)));
                let unchanged = snapshot
                    .files
                    .iter()
                    .find(|file| file.relative_path == Path::new(relative))
                    .unwrap();
                assert_eq!(unchanged.bytes.as_ref(), xml.as_bytes());
            }
        }
    }

    #[test]
    fn metadata_xml_plan_rejects_unsupported_and_unindexed_sources() {
        use crate::metadata_rename_plan::*;
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        add_xml(&mut db, FileId(200), "/rename/base/ConfigDumpInfo.xml", "<ConfigDumpInfo/>");
        let snapshot = plan_snapshot(&db);
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &snapshot, 64).unwrap();
        assert!(!plan.can_apply());
        assert!(plan.problems.iter().any(|problem| matches!(
            problem,
            MetadataPlanProblem::Xml {
                reason: bsl_metadata::rename_xml::XmlReferenceProblem::UnsupportedSurface,
                ..
            }
        )));
        let mut missing = snapshot;
        missing.files.pop();
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &missing, 64).unwrap();
        assert!(plan.problems.contains(&MetadataPlanProblem::UnindexedSource));
        let original = db.file_text(DECLARATION);
        db.set_file_text(
            DECLARATION,
            &original.replace(
                "</Properties>",
                "<DefaultObjectForm>Catalog.Partners.Form.Card</DefaultObjectForm></Properties>",
            ),
        );
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &plan_snapshot(&db), 64)
                .unwrap();
        assert!(plan.problems.iter().any(|problem| matches!(
            problem,
            MetadataPlanProblem::Xml {
                file_id: DECLARATION,
                reason: bsl_metadata::rename_xml::XmlReferenceProblem::UnknownReferenceContract
            }
        )));
    }

    #[test]
    fn metadata_xml_form_uses_prepared_uuid_attested_attributes_and_local_column_types() {
        use crate::metadata_rename_plan::*;
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let descriptor = "<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><CommonForm uuid='00000000-0000-0000-0000-000000000050'><Properties><Name>Card</Name><FormType>Managed</FormType></Properties></CommonForm></MetaDataObject>";
        let form = "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform' xmlns:d='http://v8.1c.ru/8.1/data/enterprise/current-config' xmlns:v='http://v8.1c.ru/8.1/data/core'><ChildItems><InputField name='Caption' id='1'><DataPath>Partner.Caption</DataPath><Title>Caption</Title></InputField><InputField name='Other' id='2'><DataPath>Customer.Caption</DataPath></InputField></ChildItems><Attributes><Attribute name='Partner' id='1'><Type><v:Type>d:CatalogRef.Partners</v:Type></Type></Attribute><Attribute name='Customer' id='2'><Type><v:Type>d:CatalogRef.Customers</v:Type></Type></Attribute></Attributes></Form>";
        add_xml(&mut db, FileId(200), "/rename/base/CommonForms/Card.xml", descriptor);
        add_xml(&mut db, FileId(201), "/rename/base/CommonForms/Card/Ext/Form.xml", form);
        let attrs = db.metadata_rename_form_attributes(FileId(201)).unwrap();
        assert_eq!(attrs.len(), 2);
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &plan_snapshot(&db), 64)
                .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        let changed = plan
            .files
            .iter()
            .find(|file| file.relative_path.to_string_lossy().ends_with("Form.xml"))
            .unwrap();
        let text = std::str::from_utf8(&changed.after_bytes).unwrap();
        assert!(text.contains("Partner.Heading"));
        assert!(text.contains("Customer.Caption"));
        assert!(text.contains("<Title>Caption</Title>"));
        let at = TextSize::from(form.find("Partner.Caption").unwrap() as u32 + 8);
        assert_eq!(
            metadata_target_at(&db, FileId(201), at).unwrap().identity,
            db.metadata_identity(BASE, &target()).unwrap()
        );
        let mut local = bsl_metadata::FormAttribute::new(
            "Rows",
            bsl_metadata::AttributeType::Platform(bsl_metadata::PlatformValueType::ValueTable),
        );
        local.columns.push(bsl_metadata::FormAttributeColumn {
            name: "Partner".into(),
            attr_type: bsl_metadata::AttributeType::Ref {
                mdo_type: MdoType::Catalog,
                name: "Partners".into(),
            },
        });
        let names = [Name::new("Partner"), Name::new("Caption")];
        let paths =
            hir::metadata_symbol::metadata_paths_for_form_binding(&db, BASE, &local, &names);
        assert_eq!(paths[0], hir::metadata_symbol::MetadataPathResolution::NonMetadata);
        assert_eq!(paths[1], hir::metadata_symbol::MetadataPathResolution::Resolved(target()));
        local.columns[0].attr_type = bsl_metadata::AttributeType::Unknown;
        let paths =
            hir::metadata_symbol::metadata_paths_for_form_binding(&db, BASE, &local, &names);
        assert!(matches!(
            paths[1],
            hir::metadata_symbol::MetadataPathResolution::Unresolved
                | hir::metadata_symbol::MetadataPathResolution::NonMetadata
        ));
        db.set_file_text(FileId(200), &descriptor.replace("Managed", "Ordinary"));
        assert!(db.metadata_rename_form_attributes(FileId(201)).is_err());
    }

    #[test]
    fn metadata_xml_form_extension_requires_matching_adoption_and_retains_base_attributes() {
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let descriptor = "<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><CommonForm uuid='00000000-0000-0000-0000-000000000050'><Properties><Name>Card</Name><FormType>Managed</FormType></Properties></CommonForm></MetaDataObject>";
        let base = "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform' xmlns:d='http://v8.1c.ru/8.1/data/enterprise/current-config' xmlns:v='http://v8.1c.ru/8.1/data/core'><Attributes><Attribute name='Partner' id='1'><Type><v:Type>d:CatalogRef.Partners</v:Type></Type></Attribute></Attributes></Form>";
        add_xml(&mut db, FileId(200), "/rename/base/CommonForms/Card.xml", descriptor);
        add_xml(&mut db, FileId(201), "/rename/base/CommonForms/Card/Ext/Form.xml", base);
        let adopted = descriptor.replace("000000000050", "000000000051").replace("</Properties>", "<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>00000000-0000-0000-0000-000000000050</ExtendedConfigurationObject></Properties>");
        let extension = "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'><BaseForm/><ChildItems><InputField name='Field' id='2'><DataPath>Partner.Caption</DataPath></InputField></ChildItems></Form>";
        add_xml(&mut db, FileId(202), "/rename/extension/CommonForms/Card.xml", &adopted);
        add_xml(&mut db, FileId(203), "/rename/extension/CommonForms/Card/Ext/Form.xml", extension);
        let attributes = db.metadata_rename_form_attributes(FileId(203)).unwrap();
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].name, "Partner");
        let at = TextSize::from(extension.find("Partner.Caption").unwrap() as u32 + 8);
        let reference = metadata_target_at(&db, FileId(203), at).unwrap();
        assert_eq!(reference.identity.declaration_file, EXTENSION_DECLARATION);
        db.set_file_text(
            FileId(202),
            &adopted.replace("000000000050</Extended", "000000000099</Extended"),
        );
        assert_eq!(
            db.metadata_rename_form_attributes(FileId(203)),
            Err(MetadataIdentityError::UnprovedAdoption)
        );
    }

    #[test]
    fn metadata_xml_composition_nested_field_follows_selected_output_type() {
        use crate::metadata_rename_plan::*;
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let composition = include_str!("../../bsl-metadata/fixtures/rename/composition.xml")
            .replace("SELECT P.Cap&#116;ion AS Stable, P.Ref", "SELECT P.Ref AS Partner")
            .replace(" WHERE P.Code = &amp;Code", "")
            .replace("<field>Stable</field>", "<field>Partner.Caption</field>");
        add_xml(
            &mut db,
            FileId(200),
            "/rename/base/CommonTemplates/Layout/Ext/Template.xml",
            &composition,
        );
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &plan_snapshot(&db), 64)
                .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        let layout = plan
            .files
            .iter()
            .find(|file| file.relative_path.to_string_lossy().ends_with("Template.xml"))
            .unwrap();
        let after = std::str::from_utf8(&layout.after_bytes).unwrap();
        assert!(after.contains("<field>Partner.Heading</field>"));
        assert!(after.contains("<dataPath>Stable</dataPath>"));
        assert!(after.contains("SELECT P.Ref AS Partner"));
        let at = TextSize::from(composition.find("Partner.Caption").unwrap() as u32 + 8);
        assert_eq!(metadata_target_at(&db, FileId(200), at).unwrap().identity, plan.target);
        let references = crate::references::find_references_at_rich(&db, FileId(200), at, true, 64);
        assert!(
            references.hits.iter().any(|hit| hit.file_id == FileId(200) && hit.range.contains(at)),
            "{references:#?}"
        );
    }

    #[test]
    fn metadata_xml_tabular_data_path_keeps_parent_and_column_identity() {
        use crate::metadata_rename_plan::*;
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let original = db.file_text(DECLARATION);
        let section = "<TabularSection uuid='00000000-0000-0000-0000-000000000020'><Properties><Name>Rows</Name></Properties><ChildObjects><Attribute uuid='00000000-0000-0000-0000-000000000021'><Properties><Name>Caption</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute></ChildObjects></TabularSection>";
        db.set_file_text(
            DECLARATION,
            &original.replacen("</ChildObjects>", &format!("{section}</ChildObjects>"), 1),
        );
        let descriptor = "<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><CommonForm uuid='00000000-0000-0000-0000-000000000050'><Properties><Name>Card</Name><FormType>Managed</FormType></Properties></CommonForm></MetaDataObject>";
        add_xml(&mut db, FileId(200), "/rename/base/CommonForms/Card.xml", descriptor);
        add_xml(
            &mut db,
            FileId(201),
            "/rename/base/CommonForms/Card/Ext/Form.xml",
            include_str!("../../bsl-metadata/fixtures/rename/managed.xml"),
        );
        let mut path = target();
        path.members
            .insert(0, DeclarationSegment { kind: "TabularSection".into(), name: "Rows".into() });
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &path, "Heading", &plan_snapshot(&db), 64)
                .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        let form = plan
            .files
            .iter()
            .find(|file| file.relative_path.to_string_lossy().ends_with("Form.xml"))
            .unwrap();
        let after = std::str::from_utf8(&form.after_bytes).unwrap();
        assert!(after.contains("Object.Rows.Heading"));
        assert!(after.contains("Other.Caption"));
        assert!(after.contains("<Title>Caption</Title>"));
        let changed = plan
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("Catalogs/Partners.xml"))
            .unwrap();
        let declarations = bsl_metadata::declaration_identity::metadata_declarations(
            std::str::from_utf8(&changed.after_bytes).unwrap(),
        )
        .unwrap();
        assert_eq!(
            declarations
                .iter()
                .find(|declaration| declaration.uuid == bsl_metadata::Uuid::from_u128(2))
                .unwrap()
                .path
                .last()
                .unwrap()
                .name,
            "Caption"
        );
        assert_eq!(
            declarations
                .iter()
                .find(|declaration| declaration.uuid == bsl_metadata::Uuid::from_u128(0x21))
                .unwrap()
                .path
                .last()
                .unwrap()
                .name,
            "Heading"
        );
    }

    #[test]
    fn metadata_preview_does_not_bind_an_unknown_external_owner_to_main_identity() {
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let mut snapshot = db.workspace_configs_snapshot().as_ref().clone();
        snapshot.kinds[1] =
            ide_db::metadata::RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor);
        db.set_workspace_configs_snapshot(snapshot);
        assert_eq!(
            db.metadata_identity(EXTENSION, &target()),
            Err(MetadataIdentityError::UnprovedExternalOwner)
        );
    }

    #[test]
    fn metadata_preview_attests_the_query_constructor_but_not_sql_display_text() {
        let source="Процедура Тест()\n Q = Новый Запрос(\"SELECT P.Caption FROM Catalog.Partners AS P\");\n Сообщение = \"SELECT P.Caption FROM Catalog.Partners AS P\";\nКонецПроцедуры\n";
        let db = database(source, "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let packages = db.sdbl_hir_in_file(BASE);
        assert!(
            packages.iter().all(|(_, package)| package.queries().iter().all(|query| !query
                .hir
                .diagnostics
                .iter()
                .any(sdbl_hir::SdblDiagnostic::is_error))),
            "{packages:#?}"
        );
        let preview = metadata_rename_preview(&db, BASE, &target(), "Heading", 32).unwrap();
        assert_eq!(preview.edits.len(), 1, "{preview:#?}");
        assert_eq!(
            preview.edits[0].range.start(),
            TextSize::from(source.find("P.Caption").unwrap() as u32 + 2)
        );
        let display = TextSize::from(source.rfind("P.Caption").unwrap() as u32 + 2);
        assert_eq!(
            metadata_rename_preview_at(&db, BASE, display, "Heading", 32),
            Err(MetadataPreviewError::NotMetadata)
        );
    }

    fn native_xml_database() -> RootDatabaseImpl {
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let files = [
            ("Catalogs/BaseCatalog.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/Catalogs/BaseCatalog.xml")),
            ("CommonAttributes/AreaNoAuto.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/CommonAttributes/AreaNoAuto.xml")),
            ("CommonAttributes/AreaUse.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/CommonAttributes/AreaUse.xml")),
            ("CommonAttributes/OrdinaryAuto.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/CommonAttributes/OrdinaryAuto.xml")),
            ("CommonAttributes/OtherNoAuto.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/CommonAttributes/OtherNoAuto.xml")),
            ("CommonAttributes/OtherUse.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/CommonAttributes/OtherUse.xml")),
            ("ConfigDumpInfo.xml", include_str!("../../bsl-metadata/fixtures/rename/native-config-dump-info.xml")),
            ("Configuration.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/Configuration.xml")),
            ("InformationRegisters/BaseRegister.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/InformationRegisters/BaseRegister.xml")),
            ("Languages/English.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/Languages/English.xml")),
            ("SessionParameters/AreaNoAutoValue.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/SessionParameters/AreaNoAutoValue.xml")),
            ("SessionParameters/AreaUseValue.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/SessionParameters/AreaUseValue.xml")),
            ("SessionParameters/OtherNoAutoValue.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/SessionParameters/OtherNoAutoValue.xml")),
            ("SessionParameters/OtherUseValue.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/SessionParameters/OtherUseValue.xml")),
            ("SessionParameters/UseSeparation.xml", include_str!("../../bsl-metadata/fixtures/extension_common_attributes/base/SessionParameters/UseSeparation.xml")),
        ];
        let mut entries = vec![
            MdoEntry {
                kind: MdoType::Catalog,
                name: "Partners".into(),
                main: DECLARATION,
                predefined: None,
            },
            MdoEntry {
                kind: MdoType::Catalog,
                name: "Customers".into(),
                main: OTHER_DECLARATION,
                predefined: None,
            },
        ];
        for (index, (path, text)) in files.into_iter().enumerate() {
            let id = FileId(300 + index as u32);
            add_xml(&mut db, id, &format!("/rename/base/{path}"), text);
            if path == "Catalogs/BaseCatalog.xml" {
                entries.push(MdoEntry {
                    kind: MdoType::Catalog,
                    name: "BaseCatalog".into(),
                    main: id,
                    predefined: None,
                });
            }
            if path == "InformationRegisters/BaseRegister.xml" {
                entries.push(MdoEntry {
                    kind: MdoType::InformationRegister,
                    name: "BaseRegister".into(),
                    main: id,
                    predefined: None,
                });
            }
        }
        db.set_metadata_listing(
            fixture_path("/rename/base").to_str().unwrap(),
            MetadataListingData { entries, ..MetadataListingData::default() },
        );
        db
    }

    #[test]
    fn metadata_xml_native_properties_dump_info_and_standard_paths_share_the_existing_identity() {
        use crate::metadata_rename_plan::*;
        let db = native_xml_database();
        let path = MetadataSymbolPath {
            owner_kind: MdoType::Catalog,
            owner_name: Name::new("BaseCatalog"),
            members: Vec::new(),
        };
        let snapshot = plan_snapshot(&db);
        let plan = prepare_metadata_rename_plan(&db, BASE, &path, "RenamedCatalog", &snapshot, 64)
            .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        assert_eq!(plan.files.len(), 3, "{plan:#?}");
        let original =
            include_str!("../../bsl-metadata/fixtures/rename/native-config-dump-info.xml");
        let info = plan
            .files
            .iter()
            .find(|file| file.relative_path.to_string_lossy() == "ConfigDumpInfo.xml")
            .unwrap();
        assert_eq!(
            info.after_bytes,
            original
                .replacen("name=\"Catalog.BaseCatalog\"", "name=\"Catalog.RenamedCatalog\"", 1)
                .as_bytes()
        );
        let catalog = plan
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("Catalogs/BaseCatalog.xml"))
            .unwrap();
        let after = std::str::from_utf8(&catalog.after_bytes).unwrap();
        assert!(after.contains("Catalog.RenamedCatalog.StandardAttribute.Description"));
        assert!(after.contains("Catalog.RenamedCatalog.StandardAttribute.Code"));
        assert_eq!(catalog.edits.len(), 8);
        let mut expected = include_str!(
            "../../bsl-metadata/fixtures/extension_common_attributes/base/Catalogs/BaseCatalog.xml"
        )
        .replacen("<Name>BaseCatalog</Name>", "<Name>RenamedCatalog</Name>", 1);
        for class in
            ["CatalogObject", "CatalogRef", "CatalogSelection", "CatalogList", "CatalogManager"]
        {
            expected = expected.replacen(
                &format!("name=\"{class}.BaseCatalog\""),
                &format!("name=\"{class}.RenamedCatalog\""),
                1,
            );
        }
        for field in ["Description", "Code"] {
            expected = expected.replacen(
                &format!("Catalog.BaseCatalog.StandardAttribute.{field}"),
                &format!("Catalog.RenamedCatalog.StandardAttribute.{field}"),
                1,
            );
        }
        assert_eq!(catalog.after_bytes, expected.as_bytes());
        assert_eq!(plan.resources.len(), 1);
        let id = snapshot
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("Catalogs/BaseCatalog.xml"))
            .unwrap()
            .file_id
            .unwrap();
        let source = db.file_text(id);
        let position = TextSize::from(
            (source.find("Catalog.BaseCatalog.StandardAttribute.Description").unwrap()
                + "Catalog.B".len()) as u32,
        );
        let target = metadata_target_at(&db, id, position).unwrap();
        assert_eq!(target.identity, plan.target);
    }

    #[test]
    fn metadata_xml_dump_info_refuses_same_name_wrong_uuid_and_duplicate_names() {
        use crate::metadata_rename_plan::*;
        let mut db = native_xml_database();
        let snapshot = plan_snapshot(&db);
        let file = snapshot
            .files
            .iter()
            .find(|file| file.relative_path.to_string_lossy() == "ConfigDumpInfo.xml")
            .unwrap();
        let id = file.file_id.unwrap();
        let original = db.file_text(id).to_string();
        let wrong = original.replacen(
            "018e4d6b-f86e-5cbd-b1eb-c6310ea5d8fa",
            "00000000-0000-0000-0000-000000000003",
            1,
        );
        db.set_file_text(id, &wrong);
        let path = MetadataSymbolPath {
            owner_kind: MdoType::Catalog,
            owner_name: Name::new("BaseCatalog"),
            members: Vec::new(),
        };
        let plan = prepare_metadata_rename_plan(
            &db,
            BASE,
            &path,
            "RenamedCatalog",
            &plan_snapshot(&db),
            64,
        )
        .unwrap();
        assert!(!plan.can_apply());
        assert!(plan.problems.iter().any(|problem| matches!(problem, MetadataPlanProblem::Xml { file_id, reason: bsl_metadata::rename_xml::XmlReferenceProblem::UnknownReferenceContract } if *file_id == id)));
        assert!(!plan
            .files
            .iter()
            .any(|file| file.relative_path.to_string_lossy() == "ConfigDumpInfo.xml"));
        let duplicate = original.replacen("</ConfigVersions>", "<Metadata name='Catalog.BaseCatalog' id='018e4d6b-f86e-5cbd-b1eb-c6310ea5d8fa'/></ConfigVersions>", 1);
        db.set_file_text(id, &duplicate);
        let plan = prepare_metadata_rename_plan(
            &db,
            BASE,
            &path,
            "RenamedCatalog",
            &plan_snapshot(&db),
            64,
        )
        .unwrap();
        assert!(!plan.can_apply());
    }

    fn dump_info_database() -> RootDatabaseImpl {
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        add_xml(&mut db, FileId(210), "/rename/base/Catalogs/Partners/Forms/Card.xml", "<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><Form uuid='00000000-0000-0000-0000-000000000050'><Properties><Name>Card</Name><FormType>Managed</FormType></Properties></Form></MetaDataObject>");
        add_xml(
            &mut db,
            FileId(211),
            "/rename/base/ConfigDumpInfo.xml",
            include_str!("../../bsl-metadata/fixtures/rename/config-dump-info.xml"),
        );
        db
    }

    #[test]
    fn metadata_xml_dump_info_nested_member_entities_and_external_property_ids_are_attested() {
        use crate::metadata_rename_plan::*;
        let db = dump_info_database();
        for (path, new_name, expected) in [
            (target(), "Heading", "Catalog.Partners.Attribute.Heading"),
            (
                MetadataSymbolPath { members: Vec::new(), ..target() },
                "Buyers",
                "Catalog.Buyers.Form.Card.Form",
            ),
        ] {
            let plan =
                prepare_metadata_rename_plan(&db, BASE, &path, new_name, &plan_snapshot(&db), 64)
                    .unwrap();
            assert!(plan.can_apply(), "{:#?}", plan.problems);
            let info = plan
                .files
                .iter()
                .find(|file| file.relative_path.to_string_lossy() == "ConfigDumpInfo.xml")
                .unwrap();
            let after = std::str::from_utf8(&info.after_bytes).unwrap();
            assert!(after.contains(expected), "{after}");
            assert!(after.contains(
                "id='00000000-0000-0000-0000-000000000050.0' configVersion='opaque-form-version'"
            ));
            assert!(after.contains("Catalog.Customers"));
            assert!(info
                .edits
                .iter()
                .all(|edit| edit.expected_bytes == b"Partners"
                    || edit.expected_bytes == b"Cap&#116;ion"));
        }
    }

    #[test]
    fn metadata_xml_dump_info_attestation_does_not_cross_snapshot_roots() {
        use crate::metadata_rename_plan::*;
        let db = dump_info_database();
        let mut snapshot = plan_snapshot(&db);
        let form =
            snapshot.files.iter_mut().find(|file| file.file_id == Some(FileId(210))).unwrap();
        form.root_id = "extension".into();
        form.relative_path = "Catalogs/Partners/Forms/Card.xml".into();
        let plan = prepare_metadata_rename_plan(
            &db,
            BASE,
            &MetadataSymbolPath { members: Vec::new(), ..target() },
            "Buyers",
            &snapshot,
            64,
        )
        .unwrap();
        assert!(plan.problems.iter().any(|problem| matches!(problem, MetadataPlanProblem::Xml { file_id, reason: bsl_metadata::rename_xml::XmlReferenceProblem::UnknownReferenceContract } if *file_id == FileId(211))));
    }

    #[test]
    fn metadata_xml_generated_type_noop_and_unrelated_owner_keep_every_byte() {
        use crate::metadata_rename_plan::*;
        let db = native_xml_database();
        let path = MetadataSymbolPath {
            owner_kind: MdoType::Catalog,
            owner_name: Name::new("BaseCatalog"),
            members: Vec::new(),
        };
        let snapshot = plan_snapshot(&db);
        let noop =
            prepare_metadata_rename_plan(&db, BASE, &path, "BaseCatalog", &snapshot, 64).unwrap();
        assert!(noop.can_apply(), "{:#?}", noop.problems);
        assert!(noop.files.is_empty());
        assert!(noop.resources.is_empty());
        let plan = prepare_metadata_rename_plan(&db, BASE, &path, "RenamedCatalog", &snapshot, 64)
            .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        assert!(!plan
            .files
            .iter()
            .any(|file| file.relative_path.to_string_lossy().contains("BaseRegister")));
        for before in snapshot
            .files
            .iter()
            .filter(|file| file.relative_path.to_string_lossy().contains("BaseRegister"))
        {
            assert_eq!(before.bytes.as_ref(), include_bytes!("../../bsl-metadata/fixtures/extension_common_attributes/base/InformationRegisters/BaseRegister.xml"));
        }
    }

    #[test]
    fn metadata_xml_generated_owner_uuid_must_match_the_snapshot_and_semantic_source() {
        use crate::metadata_rename_plan::*;
        let mut db = native_xml_database();
        let snapshot = plan_snapshot(&db);
        let catalog = snapshot
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("Catalogs/BaseCatalog.xml"))
            .unwrap();
        let id = catalog.file_id.unwrap();
        let current = db.file_text(id).replacen(
            "018e4d6b-f86e-5cbd-b1eb-c6310ea5d8fa",
            "00000000-0000-0000-0000-000000000099",
            1,
        );
        db.set_file_text(id, &current);
        let path = MetadataSymbolPath {
            owner_kind: MdoType::Catalog,
            owner_name: Name::new("BaseCatalog"),
            members: Vec::new(),
        };
        let plan = prepare_metadata_rename_plan(&db, BASE, &path, "RenamedCatalog", &snapshot, 64)
            .unwrap();
        assert!(!plan.can_apply());
        assert!(plan.problems.contains(&MetadataPlanProblem::ResidentTextMismatch));
        assert!(!plan
            .files
            .iter()
            .any(|file| file.relative_path == Path::new("Catalogs/BaseCatalog.xml")));
    }
    #[test]
    fn metadata_xml_calculated_expression_tracks_dataset_alias_type_and_keeps_local_keys() {
        use crate::metadata_rename_plan::*;
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let xml = format!(
            "\u{feff}{}",
            include_str!("../../bsl-metadata/fixtures/rename/composition-calculated-path.xml")
                .replace('\n', "\r\n")
        );
        let id = FileId(220);
        add_xml(&mut db, id, "/rename/base/Reports/Report/Templates/Layout/Ext/Template.xml", &xml);
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Heading", &plan_snapshot(&db), 64)
                .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        let file = plan
            .files
            .iter()
            .find(|file| file.relative_path.to_string_lossy().ends_with("Template.xml"))
            .unwrap();
        assert_eq!(
            file.after_bytes,
            xml.replacen("Partner.Cap&#116;ion", "Partner.Heading", 1).as_bytes()
        );
        assert_eq!(file.edits.len(), 1);
        let selected = metadata_target_at(
            &db,
            id,
            TextSize::from((xml.find("Cap&#116;ion").unwrap() + 2) as u32),
        )
        .unwrap();
        assert_eq!(selected.identity, plan.target);
        let customer = metadata_target_at(
            &db,
            id,
            TextSize::from((xml.find("Customer.Caption").unwrap() + "Customer.".len()) as u32),
        )
        .unwrap();
        assert_ne!(customer.identity, plan.target);
        let noop =
            prepare_metadata_rename_plan(&db, BASE, &target(), "Caption", &plan_snapshot(&db), 64)
                .unwrap();
        assert!(noop.can_apply(), "{:#?}", noop.problems);
        assert!(noop.files.is_empty());
        assert!(metadata_target_at(
            &db,
            id,
            TextSize::from((xml.find("<title>Caption</title>").unwrap() + "<title>".len()) as u32)
        )
        .is_err());
    }

    #[test]
    fn metadata_xml_calculated_expression_refuses_unknown_duplicate_or_scalar_outputs() {
        use crate::metadata_rename_plan::*;
        let original =
            include_str!("../../bsl-metadata/fixtures/rename/composition-calculated-path.xml");
        for xml in [
            original.replacen("<field>SelectedPartner</field>", "<field>Missing</field>", 1),
            original
                .replacen("<field>SelectedCustomer</field>", "<field>SelectedPartner</field>", 1)
                .replacen("<dataPath>Customer</dataPath>", "<dataPath>Partner</dataPath>", 1),
            original.replacen("P.Ref AS SelectedPartner", "P.Caption AS SelectedPartner", 1),
            original.replacen("Partner.Cap&#116;ion", "Partner.Missing", 1),
            original.replacen("Partner.Cap&#116;ion", "Unknown.Caption", 1),
            original.replacen("Partner.Cap&#116;ion", "Sum(Partner.Caption)", 1),
        ] {
            let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
            add_xml(
                &mut db,
                FileId(220),
                "/rename/base/Reports/Report/Templates/Layout/Ext/Template.xml",
                &xml,
            );
            let plan = prepare_metadata_rename_plan(
                &db,
                BASE,
                &target(),
                "Heading",
                &plan_snapshot(&db),
                64,
            )
            .unwrap();
            assert!(!plan.can_apply(), "{xml}");
        }
    }

    #[test]
    fn metadata_xml_exchange_plan_content_changes_only_bound_owner_and_preserves_controls() {
        use crate::metadata_rename_plan::*;
        let mut db = database("", "", "<ObjectBelonging>Own</ObjectBelonging>", "");
        let xml = "\u{feff}<ExchangePlanContent xmlns='http://v8.1c.ru/8.3/xcf/extrnprops'>\r\n<!-- Catalog.Partners is a user comment -->\r\n<Item><Metadata>Catalog.Partners</Metadata><AutoRecord>Allow</AutoRecord></Item>\r\n<Item><Metadata>Catalog.Customers</Metadata><AutoRecord>Deny</AutoRecord></Item>\r\n</ExchangePlanContent>";
        add_xml(&mut db, FileId(221), "/rename/base/ExchangePlans/Sync/Ext/Content.xml", xml);
        let path = MetadataSymbolPath { members: Vec::new(), ..target() };
        let plan =
            prepare_metadata_rename_plan(&db, BASE, &path, "Buyers", &plan_snapshot(&db), 64)
                .unwrap();
        assert!(plan.can_apply(), "{:#?}", plan.problems);
        let content = plan
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("ExchangePlans/Sync/Ext/Content.xml"))
            .unwrap();
        assert_eq!(content.edits.len(), 1);
        assert_eq!(
            content.after_bytes,
            xml.replacen(
                "<Metadata>Catalog.Partners</Metadata>",
                "<Metadata>Catalog.Buyers</Metadata>",
                1
            )
            .as_bytes()
        );
    }
}
