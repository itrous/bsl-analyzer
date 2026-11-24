use hir::metadata_symbol::{
    MetadataIdentityError, MetadataIdentityResolver, MetadataPathResolution,
};
use ide_db::base_db::{RootQueryDb, SourceDatabase, BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT};
use ide_db::RootDatabaseImpl;
use syntax::{TextRange, TextSize};
use vfs::FileId;

use crate::metadata_rename::{
    MetadataPreviewError, MetadataPreviewIssue, MetadataPreviewIssueKind,
    MetadataReferenceLanguage, MetadataRenamePreview,
};

use super::{
    BodySource, ReferenceHit, ReferenceKind, ReferencesOutcome, ReferencesRequest, ReferencesResult,
};

pub(super) fn at(
    db: &RootDatabaseImpl,
    file_id: FileId,
    offset: TextSize,
    request: &ReferencesRequest,
) -> Option<ReferencesResult> {
    match crate::metadata_rename::metadata_reference_search_at(
        db,
        file_id,
        offset,
        request.max_files,
    ) {
        Ok(preview) => Some(answer(db, preview, request)),
        Err(MetadataPreviewError::NotMetadata) => (db
            .file_source_root_input(file_id)
            .source_root_id(db)
            == METADATA_SOURCE_ROOT)
            .then(|| {
                ReferencesResult::outcome_only(ReferencesOutcome::NotFound, BodySource::Resident)
            }),
        Err(MetadataPreviewError::Identity(reason)) => Some(unresolved(file_id, offset, reason)),
        Err(MetadataPreviewError::InvalidIdentifier) => None,
    }
}

pub(super) fn by_name(
    db: &RootDatabaseImpl,
    name: &str,
    request: &ReferencesRequest,
) -> Option<ReferencesResult> {
    let mut files = Vec::new();
    for root in [BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT] {
        if let Some(input) = db.try_source_root_input(root) {
            files.extend(input.root(db).iter());
        }
    }
    files.sort_by_key(|file| file.0);
    files.dedup();
    if let Some(anchors) = &request.anchor_files {
        files.retain(|file| anchors.contains(file));
    }
    let mut targets = Vec::new();
    let mut unresolved_metadata = None;
    let capped = files.len() > request.max_files;
    for file_id in files.into_iter().take(request.max_files.max(1)) {
        salsa::Database::unwind_if_revision_cancelled(db);
        match hir::metadata_symbol::metadata_path_for_address(db, file_id, name) {
            MetadataPathResolution::Resolved(path) => match db.metadata_identity(file_id, &path) {
                Ok(identity) => {
                    if !targets.iter().any(|(existing, _, _)| existing == &identity) {
                        targets.push((identity, file_id, path));
                    }
                }
                Err(reason) => unresolved_metadata = Some((file_id, reason)),
            },
            MetadataPathResolution::Unresolved => {
                unresolved_metadata = Some((file_id, MetadataIdentityError::MissingDeclaration))
            }
            MetadataPathResolution::Ambiguous => {
                return Some(ReferencesResult::outcome_only(
                    ReferencesOutcome::Ambiguous,
                    BodySource::Resident,
                ));
            }
            MetadataPathResolution::NonMetadata => {}
        }
    }
    match targets.as_slice() {
        [(_, file_id, path)] => {
            match crate::metadata_rename::metadata_reference_search(
                db,
                *file_id,
                path,
                request.max_files,
            ) {
                Ok(mut preview) => {
                    if capped {
                        preview.issues.push(MetadataPreviewIssue {
                            file_id: None,
                            range: None,
                            kind: MetadataPreviewIssueKind::FileLimit,
                        });
                    }
                    if let Some((file_id, reason)) = unresolved_metadata {
                        preview.issues.push(MetadataPreviewIssue {
                            file_id: Some(file_id),
                            range: None,
                            kind: MetadataPreviewIssueKind::UnresolvedReference(reason),
                        });
                    }
                    Some(answer(db, preview, request))
                }
                Err(MetadataPreviewError::Identity(reason)) => {
                    Some(unresolved(*file_id, TextSize::from(0), reason))
                }
                _ => None,
            }
        }
        [] => unresolved_metadata.map(|(file, reason)| unresolved(file, TextSize::from(0), reason)),
        _ => {
            Some(ReferencesResult::outcome_only(ReferencesOutcome::Ambiguous, BodySource::Resident))
        }
    }
}

fn unresolved(
    file_id: FileId,
    offset: TextSize,
    reason: MetadataIdentityError,
) -> ReferencesResult {
    ReferencesResult::outcome_only(
        ReferencesOutcome::Partial {
            reasons: vec![MetadataPreviewIssue {
                file_id: Some(file_id),
                range: Some(TextRange::empty(offset)),
                kind: MetadataPreviewIssueKind::UnresolvedReference(reason),
            }],
        },
        BodySource::Resident,
    )
}

fn answer(
    db: &RootDatabaseImpl,
    preview: MetadataRenamePreview,
    request: &ReferencesRequest,
) -> ReferencesResult {
    let mut reasons = preview
        .issues
        .into_iter()
        .filter(|issue| issue.kind != MetadataPreviewIssueKind::SnapshotBindingRequired)
        .collect::<Vec<_>>();
    let mut hits = preview
        .edits
        .into_iter()
        .filter_map(|reference| {
            if request.area.files.as_ref().is_some_and(|files| !files.contains(&reference.file_id))
            {
                return None;
            }
            let parse = db.parse(reference.file_id);
            let root = parse.syntax_node();
            let token = root.token_at_offset(reference.range.start()).right_biased()?;
            let kind = match reference.language {
                MetadataReferenceLanguage::Bsl => super::classify_reference_token(&token),
                MetadataReferenceLanguage::Sdbl => ReferenceKind::Read,
            };
            if request.kinds.as_ref().is_some_and(|kinds| !kinds.contains(&kind)) {
                return None;
            }
            Some(ReferenceHit {
                file_id: reference.file_id,
                range: reference.range,
                enclosing_range: super::enclosing_method_range(&token),
                kind,
            })
        })
        .collect::<Vec<_>>();
    let mut files_scanned = preview.files_read.len();
    if let Some(input) = db.try_source_root_input(METADATA_SOURCE_ROOT) {
        let mut files = input.root(db).iter().collect::<Vec<_>>();
        files.sort_by_key(|file| file.0);
        for file_id in files {
            if files_scanned >= request.max_files {
                reasons.push(MetadataPreviewIssue {
                    file_id: Some(file_id),
                    range: None,
                    kind: MetadataPreviewIssueKind::FileLimit,
                });
                break;
            }
            files_scanned += 1;
            match crate::metadata_rename::xml_target::metadata_uses_in_xml(db, file_id, None) {
                Ok((uses, _)) => {
                    let text = db.file_text(file_id);
                    let declaration_ranges =
                        bsl_metadata::rename_xml::MetadataXmlSource::decode(text.as_bytes())
                            .map(|source| {
                                source
                                    .inspect(None)
                                    .declarations
                                    .into_iter()
                                    .map(|declaration| declaration.name.range)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                    for reference in
                        uses.into_iter().filter(|reference| reference.identity == preview.target)
                    {
                        if request
                            .area
                            .files
                            .as_ref()
                            .is_some_and(|files| !files.contains(&file_id))
                        {
                            continue;
                        }
                        let is_declaration = declaration_ranges.contains(
                            &(usize::from(reference.range.start())
                                ..usize::from(reference.range.end())),
                        );
                        if is_declaration && !request.include_declaration {
                            continue;
                        }
                        let kind = if is_declaration {
                            ReferenceKind::Declaration
                        } else {
                            ReferenceKind::Read
                        };
                        if request.kinds.as_ref().is_some_and(|kinds| !kinds.contains(&kind)) {
                            continue;
                        }
                        hits.push(ReferenceHit {
                            file_id,
                            range: reference.range,
                            enclosing_range: None,
                            kind,
                        });
                    }
                }
                Err(MetadataPreviewError::Identity(reason)) => reasons.push(MetadataPreviewIssue {
                    file_id: Some(file_id),
                    range: None,
                    kind: MetadataPreviewIssueKind::UnresolvedReference(reason),
                }),
                _ => {}
            }
        }
    }
    let files_capped =
        reasons.iter().any(|issue| issue.kind == MetadataPreviewIssueKind::FileLimit);
    let per_file = super::order_and_count_by_file(db, &mut hits);
    let outcome = if reasons.is_empty() {
        ReferencesOutcome::Resolved
    } else {
        ReferencesOutcome::Partial { reasons }
    };
    ReferencesResult {
        outcome,
        body_source: BodySource::Resident,
        hits,
        per_file,
        declarations: Vec::new(),
        candidates: None,
        files_scanned,
        files_capped,
        anchor_candidates_capped: false,
        anchor_line: None,
        anchor_sites: Vec::new(),
    }
}
