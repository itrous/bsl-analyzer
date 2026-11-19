mod xml_paths;

use bsl_metadata::declaration_identity::DeclarationSegment;
use bsl_metadata::rename_xml::{
    metadata_source_fingerprint, MetadataXmlSource, XmlMetadataReferences, XmlNameToken,
    XmlReferenceAnchor, XmlReferenceProblem,
};
use hir::metadata_symbol::{
    MetadataIdentityError, MetadataIdentityResolver, MetadataPathResolution,
};
use hir::Name;
use ide_db::base_db::{SourceDatabase, BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT};
use ide_db::RootDatabaseImpl;

use super::*;
use crate::metadata_rename::{
    metadata_rename_preview, MetadataPreviewError, MetadataPreviewIssueKind,
};

pub fn prepare_metadata_rename_plan(
    db: &RootDatabaseImpl,
    anchor_file: FileId,
    target_path: &MetadataSymbolPath,
    new_name: &str,
    snapshot: &PreparedMetadataRenameSnapshot,
    max_files: usize,
) -> Result<MetadataRenamePlan, MetadataPreviewError> {
    let preview = metadata_rename_preview(db, anchor_file, target_path, new_name, max_files)?;
    let mut plan = MetadataRenamePlan {
        target: preview.target,
        path: preview.path,
        new_name: new_name.into(),
        roots: snapshot.roots.clone(),
        context_sha256: snapshot.context_sha256,
        read_set: snapshot
            .files
            .iter()
            .map(|file| MetadataFileRead {
                file_id: file.file_id,
                root_id: file.root_id.clone(),
                relative_path: file.relative_path.clone(),
                before_sha256: metadata_source_fingerprint(&file.bytes),
                open_document: file.open_document.clone(),
            })
            .collect(),
        directories: snapshot.directories.clone(),
        files: Vec::new(),
        resources: Vec::new(),
        problems: validate_metadata_rename_roster(snapshot),
    };
    check_destination_name(db, anchor_file, &mut plan);
    let mut checked_name_contexts =
        vec![db.metadata_symbol_roots_for_file(anchor_file, plan.path.owner_kind)];
    plan.problems.extend(
        preview
            .issues
            .into_iter()
            .filter(|issue| {
                !matches!(
                    issue.kind,
                    MetadataPreviewIssueKind::XmlCoverageRequired
                        | MetadataPreviewIssueKind::SnapshotBindingRequired
                )
            })
            .map(MetadataPlanProblem::Reference),
    );
    let mut prepared_ids = Vec::new();
    for source_root in [BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT] {
        if let Some(input) = db.try_source_root_input(source_root) {
            prepared_ids.extend(input.root(db).iter());
        }
    }
    if prepared_ids
        .iter()
        .any(|file_id| !snapshot.files.iter().any(|file| file.file_id == Some(*file_id)))
    {
        plan.problems.push(MetadataPlanProblem::UnindexedSource);
    }
    let owner_path = MetadataSymbolPath {
        owner_kind: plan.path.owner_kind,
        owner_name: plan.path.owner_name.clone(),
        members: Vec::new(),
    };
    let owner_names = hir::metadata_symbol::metadata_reference_names(db, anchor_file, &owner_path);
    let container = MetadataSymbolPath {
        owner_kind: plan.path.owner_kind,
        owner_name: plan.path.owner_name.clone(),
        members: plan.path.members[..plan.path.members.len().saturating_sub(1)].to_vec(),
    };
    let container_identity =
        db.metadata_identity(anchor_file, &container).map_err(MetadataPreviewError::Identity)?;
    if snapshot.files.len() > max_files {
        plan.problems.push(MetadataPlanProblem::Reference(MetadataPreviewIssue {
            file_id: None,
            range: None,
            kind: MetadataPreviewIssueKind::FileLimit,
        }));
    }
    for file in snapshot.files.iter().take(max_files) {
        salsa::Database::unwind_if_revision_cancelled(db);
        if matches!(
            file.kind,
            MetadataPlanFileKind::OpaqueResource | MetadataPlanFileKind::ProjectInput
        ) {
            continue;
        }
        let Some(file_id) = file.file_id.filter(|id| prepared_ids.contains(id)) else {
            plan.problems.push(MetadataPlanProblem::UnindexedSource);
            continue;
        };
        let Some(root) = snapshot.roots.iter().find(|root| root.id == file.root_id) else {
            continue;
        };
        let context = db.metadata_symbol_roots_for_file(file_id, plan.path.owner_kind);
        if !checked_name_contexts.contains(&context) {
            checked_name_contexts.push(context);
            if db
                .metadata_identity(file_id, &plan.path)
                .is_ok_and(|identity| identity == plan.target)
            {
                check_destination_name(db, file_id, &mut plan);
            }
        }
        let source_root = db.file_source_root_input(file_id);
        let resident_root = db.source_root_input(source_root.source_root_id(db)).root(db);
        let actual_path =
            resident_root.file_set().path_for_file(&file_id).map(|path| path.as_path());
        let path_matches = actual_path.is_some_and(|actual| {
            [&root.source_path, &root.canonical_path].into_iter().any(|root_path| {
                let expected_path = root_path.join(&file.relative_path);
                if root.case_sensitive {
                    actual == expected_path
                } else {
                    actual.to_str().zip(expected_path.to_str()).is_some_and(|(actual, expected)| {
                        stdx::case::eq_ignore_case(actual, expected)
                    })
                }
            })
        });
        if !path_matches {
            plan.problems.push(MetadataPlanProblem::ResidentTextMismatch);
            continue;
        }
        let source = match MetadataXmlSource::decode(&file.bytes) {
            Ok(source) => source,
            Err(reason) => {
                plan.problems.push(MetadataPlanProblem::Xml { file_id, reason });
                continue;
            }
        };
        let resident = db.file_text(file_id);
        let offset = usize::from(resident.starts_with('\u{feff}')) * 3;
        if db.file_is_unread(file_id) || resident.get(offset..) != Some(source.text()) {
            plan.problems.push(MetadataPlanProblem::ResidentTextMismatch);
            continue;
        }
        if file.open_document.as_ref().is_some_and(|document| {
            document.document_text_sha256 != metadata_document_fingerprint(&resident)
        }) {
            plan.problems.push(MetadataPlanProblem::DocumentTextMismatch);
        }
        let mut edits = Vec::new();
        match file.kind {
            MetadataPlanFileKind::Bsl => {
                for edit in preview.edits.iter().filter(|edit| edit.file_id == file_id) {
                    let range = usize::from(edit.range.start())
                        .checked_sub(offset)
                        .zip(usize::from(edit.range.end()).checked_sub(offset));
                    if let Some(range) = range.and_then(|(start, end)| source.raw_range(start..end))
                    {
                        add_edit(&file.bytes, &source, range, &edit.new_text, &mut edits);
                    } else {
                        plan.problems.push(MetadataPlanProblem::ResidentTextMismatch);
                    }
                }
            }
            MetadataPlanFileKind::Xml => {
                let mut facts =
                    source.inspect(file.effective_form_attributes.as_deref().map(Vec::as_slice));
                if facts.form_bindings_required && file.effective_form_attributes.is_none() {
                    match db.metadata_rename_form_attributes(file_id) {
                        Ok(attributes) => facts = source.inspect(Some(&attributes)),
                        Err(_) => facts.problems.push(XmlReferenceProblem::UnknownFormBinding),
                    }
                }
                plan.problems.extend(
                    facts
                        .problems
                        .iter()
                        .filter(|reason| **reason != XmlReferenceProblem::DataSetOutputContract)
                        .cloned()
                        .map(|reason| MetadataPlanProblem::Xml { file_id, reason }),
                );
                for declaration in &facts.declarations {
                    let Some(owner) = declaration.declaration.path.first() else {
                        continue;
                    };
                    let Ok(kind) = owner.kind.parse() else {
                        continue;
                    };
                    if !potential_owner(kind, &owner.name, &plan, &owner_names) {
                        continue;
                    }
                    let path = MetadataSymbolPath {
                        owner_kind: kind,
                        owner_name: Name::new(&owner.name),
                        members: declaration.declaration.path[1..].to_vec(),
                    };
                    if matched(db, file_id, &path, &mut plan) {
                        add_token(&file.bytes, &source, &declaration.name, new_name, &mut edits);
                        if plan.path.members.is_empty() && declaration.name.text != new_name {
                            match metadata_object_resource_renames(
                                snapshot,
                                &file.root_id,
                                &file.relative_path,
                                &declaration.name.text,
                                new_name,
                            ) {
                                Ok(operations) => plan.resources.extend(operations),
                                Err(problem) => plan.problems.push(problem),
                            }
                        }
                    }
                }
                xml_paths::dump_info(
                    db,
                    file_id,
                    file,
                    &source,
                    &facts,
                    snapshot,
                    &owner_names,
                    new_name,
                    &mut edits,
                    &mut plan,
                );
                let property_references =
                    crate::metadata_rename::xml_target::property_references(&facts);
                for reference in facts.references.iter().chain(&property_references) {
                    match &reference.anchor {
                        XmlReferenceAnchor::Metadata { kind, name } => {
                            if !potential_owner(*kind, &name.text, &plan, &owner_names) {
                                continue;
                            }
                            let mut path = MetadataSymbolPath {
                                owner_kind: *kind,
                                owner_name: Name::new(&name.text),
                                members: Vec::new(),
                            };
                            if matched(db, file_id, &path, &mut plan) {
                                add_token(&file.bytes, &source, name, new_name, &mut edits);
                            }
                            for (kind, token) in
                                reference.declared_member_kinds.iter().zip(&reference.members)
                            {
                                path.members.push(DeclarationSegment {
                                    kind: kind.clone(),
                                    name: token.text.clone(),
                                });
                                if matched(db, file_id, &path, &mut plan) {
                                    add_token(&file.bytes, &source, token, new_name, &mut edits);
                                }
                            }
                        }
                        XmlReferenceAnchor::FormValue(attribute) => {
                            let names = reference
                                .members
                                .iter()
                                .map(|member| Name::new(&member.text))
                                .collect::<Vec<_>>();
                            let paths = hir::metadata_symbol::metadata_paths_for_form_binding(
                                db, file_id, attribute, &names,
                            );
                            apply_paths(
                                db,
                                file_id,
                                &file.bytes,
                                &source,
                                &reference.members,
                                paths,
                                new_name,
                                &mut edits,
                                &mut plan,
                            );
                        }
                    }
                }
                xml_queries(
                    db,
                    file_id,
                    file,
                    &source,
                    &facts,
                    &container_identity,
                    new_name,
                    &mut edits,
                    &mut plan,
                );
            }
            MetadataPlanFileKind::Unsupported => plan.problems.push(MetadataPlanProblem::Xml {
                file_id,
                reason: XmlReferenceProblem::UnsupportedSurface,
            }),
            MetadataPlanFileKind::OpaqueResource | MetadataPlanFileKind::ProjectInput => {}
        }
        if edits.is_empty() {
            continue;
        }
        if !root.writable {
            plan.problems.push(MetadataPlanProblem::ReadonlyRoot);
        }
        match render_metadata_edits(&file.bytes, &mut edits) {
            Ok(after_bytes) => {
                match MetadataXmlSource::decode(&after_bytes) {
                    Ok(after)
                        if file.kind == MetadataPlanFileKind::Xml && !after.is_well_formed() =>
                    {
                        plan.problems.push(MetadataPlanProblem::Xml {
                            file_id,
                            reason: XmlReferenceProblem::MalformedXml,
                        })
                    }
                    Ok(after)
                        if file.kind == MetadataPlanFileKind::Bsl
                            && parser::parse(after.text()).has_errors() =>
                    {
                        plan.problems.push(MetadataPlanProblem::Reference(MetadataPreviewIssue {
                            file_id: Some(file_id),
                            range: None,
                            kind: MetadataPreviewIssueKind::ParseErrors,
                        }))
                    }
                    Err(reason) => plan.problems.push(MetadataPlanProblem::Xml { file_id, reason }),
                    _ => {}
                }
                plan.files.push(MetadataPlannedFile {
                    root_id: file.root_id.clone(),
                    relative_path: file.relative_path.clone(),
                    before_sha256: source.sha256,
                    edits,
                    after_bytes,
                });
            }
            Err(problem) => plan.problems.push(problem),
        }
    }
    plan.resources.sort_by(|left, right| {
        (&left.root_id, &left.from_relative).cmp(&(&right.root_id, &right.from_relative))
    });
    plan.resources.dedup();
    Ok(plan)
}

fn check_destination_name(db: &RootDatabaseImpl, file: FileId, plan: &mut MetadataRenamePlan) {
    match destination_name_conflicts(db, file, plan) {
        Ok(true) => plan.problems.push(MetadataPlanProblem::NameCollision),
        Ok(false) => {}
        Err(reason) => plan.problems.push(MetadataPlanProblem::Reference(MetadataPreviewIssue {
            file_id: Some(file),
            range: None,
            kind: MetadataPreviewIssueKind::UnresolvedReference(reason),
        })),
    }
}

fn destination_name_conflicts(
    db: &RootDatabaseImpl,
    anchor_file: FileId,
    plan: &MetadataRenamePlan,
) -> Result<bool, MetadataIdentityError> {
    let mut destination = plan.path.clone();
    if let Some(member) = destination.members.last_mut() {
        member.name.clone_from(&plan.new_name);
    } else {
        destination.owner_name = Name::new(&plan.new_name);
    }
    match db.metadata_identity(anchor_file, &destination) {
        Ok(identity) => return Ok(identity != plan.target),
        Err(MetadataIdentityError::MissingDeclaration) => {}
        Err(reason) => return Err(reason),
    }
    if !destination.members.last().is_some_and(|member| {
        matches!(member.kind.as_str(), "Attribute" | "Dimension" | "Resource" | "TabularSection")
    }) {
        return Ok(false);
    }
    let mut address =
        format!("{}.{}", destination.owner_kind.english_name(), destination.owner_name);
    for member in &destination.members {
        address.push('.');
        address.push_str(&member.name);
    }
    match hir::metadata_symbol::metadata_path_for_address(db, anchor_file, &address) {
        MetadataPathResolution::Resolved(path) => {
            db.metadata_identity(anchor_file, &path).map(|identity| identity != plan.target)
        }
        MetadataPathResolution::Unresolved => Ok(false),
        MetadataPathResolution::NonMetadata => Ok(true),
        MetadataPathResolution::Ambiguous => Err(MetadataIdentityError::AmbiguousDeclaration),
    }
}

fn potential_owner(
    kind: bsl_metadata::MdoType,
    name: &str,
    plan: &MetadataRenamePlan,
    spellings: &[Name],
) -> bool {
    kind == plan.path.owner_kind
        && spellings.iter().any(|spelling| stdx::case::eq_ignore_case(spelling.as_str(), name))
}

fn matched(
    db: &RootDatabaseImpl,
    file_id: FileId,
    path: &MetadataSymbolPath,
    plan: &mut MetadataRenamePlan,
) -> bool {
    match db.metadata_identity(file_id, path) {
        Ok(identity) => identity == plan.target,
        Err(reason) => {
            plan.problems.push(MetadataPlanProblem::Reference(MetadataPreviewIssue {
                file_id: Some(file_id),
                range: None,
                kind: MetadataPreviewIssueKind::UnresolvedReference(reason),
            }));
            false
        }
    }
}

fn add_edit(
    bytes: &[u8],
    source: &MetadataXmlSource,
    range: std::ops::Range<usize>,
    text: &str,
    edits: &mut Vec<MetadataRawEdit>,
) {
    if let Some(expected) = bytes.get(range.clone()) {
        let replacement = source.encode_name(text);
        if expected != replacement {
            edits.push(MetadataRawEdit {
                range,
                expected_bytes: expected.to_vec(),
                replacement_bytes: replacement,
            });
        }
    }
}

fn add_token(
    bytes: &[u8],
    source: &MetadataXmlSource,
    token: &XmlNameToken,
    new_name: &str,
    edits: &mut Vec<MetadataRawEdit>,
) {
    if token.text == new_name {
        return;
    }
    add_edit(bytes, source, token.range.clone(), new_name, edits);
}

#[allow(clippy::too_many_arguments)] // Both raw ranges and semantic identities are required for one source file.
fn apply_paths(
    db: &RootDatabaseImpl,
    file_id: FileId,
    bytes: &[u8],
    source: &MetadataXmlSource,
    tokens: &[XmlNameToken],
    paths: Vec<MetadataPathResolution>,
    new_name: &str,
    edits: &mut Vec<MetadataRawEdit>,
    plan: &mut MetadataRenamePlan,
) {
    for (token, path) in tokens.iter().zip(paths) {
        match path {
            MetadataPathResolution::Resolved(path) => {
                if matched(db, file_id, &path, plan) {
                    add_token(bytes, source, token, new_name, edits);
                }
            }
            MetadataPathResolution::NonMetadata => {}
            MetadataPathResolution::Unresolved | MetadataPathResolution::Ambiguous => {
                plan.problems.push(MetadataPlanProblem::Xml {
                    file_id,
                    reason: XmlReferenceProblem::UnknownFormBinding,
                })
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // XML byte mapping and query metadata resolution share the same immutable input.
fn xml_queries(
    db: &RootDatabaseImpl,
    file_id: FileId,
    file: &PreparedMetadataRenameFile,
    source: &MetadataXmlSource,
    facts: &XmlMetadataReferences,
    container_identity: &MetadataSymbolIdentity,
    new_name: &str,
    edits: &mut Vec<MetadataRawEdit>,
    plan: &mut MetadataRenamePlan,
) {
    for (index, query) in facts.queries.iter().enumerate() {
        let parsed = parser::parse_sdbl(&query.text);
        let resolver = hir::DbSdblResolver::new(db, file_id);
        let package = sdbl_hir::lower_sdbl_to_hir_with_resolver(&parsed, Some(&resolver));
        if parsed.has_errors()
            || package
                .queries()
                .iter()
                .any(|query| query.hir.diagnostics.iter().any(sdbl_hir::SdblDiagnostic::is_error))
        {
            plan.problems.push(MetadataPlanProblem::Xml {
                file_id,
                reason: XmlReferenceProblem::UnknownReferenceContract,
            });
            continue;
        }
        if !package.source_map.metadata_reference_issues().is_empty() {
            plan.problems.push(MetadataPlanProblem::Xml {
                file_id,
                reason: XmlReferenceProblem::UnknownReferenceContract,
            });
        }
        let mut target_ranges = Vec::new();
        let mut container_ranges = Vec::new();
        for reference in package.source_map.metadata_references() {
            let path = MetadataSymbolPath {
                owner_kind: reference.owner_kind,
                owner_name: Name::new(&reference.owner_name),
                members: reference.members.clone(),
            };
            let container_members = &plan.path.members[..plan.path.members.len().saturating_sub(1)];
            let same_members = |members: &[DeclarationSegment]| {
                members.len() == path.members.len()
                    && members.iter().zip(&path.members).all(|(expected, actual)| {
                        expected.kind == actual.kind
                            && stdx::case::eq_ignore_case(&expected.name, &actual.name)
                    })
            };
            // Standard fields have no source UUID. They cannot denote a different named declaration.
            if !same_members(&plan.path.members) && !same_members(container_members) {
                continue;
            }
            if let Ok(identity) = db.metadata_identity(file_id, &path) {
                if &identity == container_identity {
                    container_ranges.push(reference.range);
                }
                if identity == plan.target {
                    target_ranges.push(reference.range);
                }
            } else {
                plan.problems.push(MetadataPlanProblem::Xml {
                    file_id,
                    reason: XmlReferenceProblem::UnknownReferenceContract,
                });
            }
        }
        let policy = crate::metadata_rename::query_output::metadata_query_output_policy(
            &package,
            &target_ranges,
            &container_ranges,
            plan.path.members.is_empty(),
        );
        if !policy.incomplete.is_empty() {
            plan.problems.push(MetadataPlanProblem::Reference(MetadataPreviewIssue {
                file_id: Some(file_id),
                range: None,
                kind: MetadataPreviewIssueKind::QueryOutputContract,
            }));
        }
        for range in target_ranges {
            if query.text.get(usize::from(range.start())..usize::from(range.end()))
                == Some(new_name)
            {
                continue;
            }
            let replacement = policy
                .preserve_aliases
                .iter()
                .find(|(reference, _)| *reference == range)
                .map(|(_, alias)| format!("{new_name} AS {alias}"))
                .unwrap_or_else(|| new_name.into());
            if let Some(raw) =
                query.original_range(usize::from(range.start())..usize::from(range.end()))
            {
                add_edit(&file.bytes, source, raw, &replacement, edits);
            } else {
                plan.problems.push(MetadataPlanProblem::Xml {
                    file_id,
                    reason: XmlReferenceProblem::UnmappableValue,
                });
            }
        }
        for field in facts.data_set_fields.iter().filter(|field| field.query == index) {
            let Some(tokens) = field.field.tokens() else {
                plan.problems.push(MetadataPlanProblem::Xml {
                    file_id,
                    reason: XmlReferenceProblem::DataSetOutputContract,
                });
                continue;
            };
            let Some(head) = tokens.first() else { continue };
            let outputs = package
                .queries()
                .iter()
                .flat_map(|query| &query.hir.select.fields)
                .filter(|output| {
                    output
                        .alias_or_name()
                        .is_some_and(|name| stdx::case::eq_ignore_case(name.as_str(), &head.text))
                })
                .collect::<Vec<_>>();
            let [output] = outputs.as_slice() else {
                plan.problems.push(MetadataPlanProblem::Xml {
                    file_id,
                    reason: XmlReferenceProblem::DataSetOutputContract,
                });
                continue;
            };
            let names = tokens[1..].iter().map(|token| Name::new(&token.text)).collect::<Vec<_>>();
            let paths = hir::metadata_symbol::metadata_paths_for_sdbl_value(
                db, file_id, &output.ty, &names,
            );
            apply_paths(
                db,
                file_id,
                &file.bytes,
                source,
                &tokens[1..],
                paths,
                new_name,
                edits,
                plan,
            );
        }
        for expression in crate::metadata_rename::xml_target::composition_expression_paths(
            db, file_id, facts, index, &package,
        ) {
            apply_paths(
                db,
                file_id,
                &file.bytes,
                source,
                &expression.tokens,
                expression.paths,
                new_name,
                edits,
                plan,
            );
        }
    }
}
