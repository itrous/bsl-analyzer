use bsl_metadata::declaration_identity::DeclarationSegment;
use bsl_metadata::rename_xml::{
    MetadataXmlSource, XmlMetadataReference, XmlMetadataReferences, XmlNameToken,
    XmlReferenceAnchor,
};
use hir::metadata_symbol::{
    MetadataIdentityError, MetadataIdentityResolver, MetadataPathResolution, MetadataSymbolPath,
    MetadataUse,
};
use ide_db::base_db::SourceDatabase;
use ide_db::RootDatabaseImpl;
use syntax::{TextRange, TextSize};
use vfs::FileId;

use super::MetadataPreviewError;

pub(super) fn metadata_target_in_xml(
    db: &RootDatabaseImpl,
    file_id: FileId,
    offset: TextSize,
) -> Result<MetadataUse, MetadataPreviewError> {
    let (candidates, incomplete) = metadata_uses_in_xml(db, file_id, Some(offset))?;
    let mut targets = candidates.into_iter();
    let Some(target) = targets.next() else {
        return if incomplete {
            Err(MetadataPreviewError::Identity(MetadataIdentityError::UnreadSource))
        } else {
            Err(MetadataPreviewError::NotMetadata)
        };
    };
    if targets.any(|candidate| candidate.identity != target.identity) {
        return Err(MetadataPreviewError::Identity(MetadataIdentityError::AmbiguousDeclaration));
    }
    Ok(target)
}

pub(crate) fn metadata_uses_in_xml(
    db: &RootDatabaseImpl,
    file_id: FileId,
    offset: Option<TextSize>,
) -> Result<(Vec<MetadataUse>, bool), MetadataPreviewError> {
    if db.file_is_unread(file_id) {
        return Err(MetadataPreviewError::Identity(MetadataIdentityError::UnreadSource));
    }
    let text = db.file_text(file_id);
    let source = MetadataXmlSource::decode(text.as_bytes())
        .map_err(|_| MetadataPreviewError::Identity(MetadataIdentityError::UnreadSource))?;
    let mut facts = source.inspect(None);
    if facts.form_bindings_required {
        let attributes =
            db.metadata_rename_form_attributes(file_id).map_err(MetadataPreviewError::Identity)?;
        facts = source.inspect(Some(&attributes));
    }
    let mut candidates = Vec::new();
    for declaration in &facts.declarations {
        if !contains(&declaration.name, offset) {
            continue;
        }
        let Some(owner) = declaration.declaration.path.first() else {
            continue;
        };
        let kind = owner.kind.parse().map_err(|_| {
            MetadataPreviewError::Identity(MetadataIdentityError::MissingDeclaration)
        })?;
        push_path(
            db,
            file_id,
            &declaration.name,
            MetadataSymbolPath {
                owner_kind: kind,
                owner_name: hir::Name::new(&owner.name),
                members: declaration.declaration.path[1..].to_vec(),
            },
            &mut candidates,
        )?;
    }
    let properties = property_references(&facts);
    for reference in facts.references.iter().chain(&properties) {
        match &reference.anchor {
            XmlReferenceAnchor::Metadata { kind, name } => {
                let mut path = MetadataSymbolPath {
                    owner_kind: *kind,
                    owner_name: hir::Name::new(&name.text),
                    members: Vec::new(),
                };
                if contains(name, offset) {
                    push_path(db, file_id, name, path.clone(), &mut candidates)?;
                }
                for (kind, member) in reference.declared_member_kinds.iter().zip(&reference.members)
                {
                    path.members
                        .push(DeclarationSegment { kind: kind.clone(), name: member.text.clone() });
                    if contains(member, offset) {
                        push_path(db, file_id, member, path.clone(), &mut candidates)?;
                    }
                }
            }
            XmlReferenceAnchor::FormValue(attribute) => {
                if !reference.members.iter().any(|token| contains(token, offset)) {
                    continue;
                }
                let names = reference
                    .members
                    .iter()
                    .map(|member| hir::Name::new(&member.text))
                    .collect::<Vec<_>>();
                let paths = hir::metadata_symbol::metadata_paths_for_form_binding(
                    db, file_id, attribute, &names,
                );
                push_paths_at(db, file_id, &reference.members, paths, offset, &mut candidates)?;
            }
        }
    }
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
            continue;
        }
        for reference in package.source_map.metadata_references() {
            let Some(range) = query.original_range(
                usize::from(reference.range.start())..usize::from(reference.range.end()),
            ) else {
                continue;
            };
            let token = XmlNameToken { text: String::new(), range };
            if contains(&token, offset) {
                push_path(
                    db,
                    file_id,
                    &token,
                    MetadataSymbolPath {
                        owner_kind: reference.owner_kind,
                        owner_name: hir::Name::new(&reference.owner_name),
                        members: reference.members.clone(),
                    },
                    &mut candidates,
                )?;
            }
        }
        for field in facts.data_set_fields.iter().filter(|field| field.query == index) {
            let Some(tokens) = field.field.tokens() else {
                continue;
            };
            if !tokens.iter().skip(1).any(|token| contains(token, offset)) {
                continue;
            }
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
                return Err(MetadataPreviewError::Identity(
                    MetadataIdentityError::AmbiguousDeclaration,
                ));
            };
            let names =
                tokens[1..].iter().map(|token| hir::Name::new(&token.text)).collect::<Vec<_>>();
            let paths = hir::metadata_symbol::metadata_paths_for_sdbl_value(
                db, file_id, &output.ty, &names,
            );
            push_paths_at(db, file_id, &tokens[1..], paths, offset, &mut candidates)?;
        }
        for expression in composition_expression_paths(db, file_id, &facts, index, &package) {
            push_paths_at(
                db,
                file_id,
                &expression.tokens,
                expression.paths,
                offset,
                &mut candidates,
            )?;
        }
    }
    let mut incomplete = !facts.problems.is_empty();
    let mut uses = Vec::new();
    for candidate in candidates {
        match candidate {
            Ok(reference) => uses.push(reference),
            Err(error) if offset.is_some() => return Err(error),
            Err(_) => incomplete = true,
        }
    }
    Ok((uses, incomplete))
}

fn contains(token: &XmlNameToken, offset: Option<TextSize>) -> bool {
    offset.is_none_or(|offset| {
        let offset = usize::from(offset);
        token.range.start <= offset && offset <= token.range.end
    })
}

fn push_path(
    db: &RootDatabaseImpl,
    file_id: FileId,
    token: &XmlNameToken,
    path: MetadataSymbolPath,
    targets: &mut Vec<Result<MetadataUse, MetadataPreviewError>>,
) -> Result<(), MetadataPreviewError> {
    let start = u32::try_from(token.range.start)
        .map_err(|_| MetadataPreviewError::Identity(MetadataIdentityError::UnreadSource))?;
    let end = u32::try_from(token.range.end)
        .map_err(|_| MetadataPreviewError::Identity(MetadataIdentityError::UnreadSource))?;
    let target = db
        .metadata_identity(file_id, &path)
        .map(|identity| MetadataUse {
            identity,
            path,
            range: TextRange::new(start.into(), end.into()),
        })
        .map_err(MetadataPreviewError::Identity);
    targets.push(target);
    Ok(())
}

fn push_paths_at(
    db: &RootDatabaseImpl,
    file_id: FileId,
    tokens: &[XmlNameToken],
    paths: Vec<MetadataPathResolution>,
    offset: Option<TextSize>,
    targets: &mut Vec<Result<MetadataUse, MetadataPreviewError>>,
) -> Result<(), MetadataPreviewError> {
    for (token, path) in tokens.iter().zip(paths) {
        if !contains(token, offset) {
            continue;
        }
        match path {
            MetadataPathResolution::Resolved(path) => push_path(db, file_id, token, path, targets)?,
            MetadataPathResolution::NonMetadata => {}
            MetadataPathResolution::Ambiguous => targets.push(Err(MetadataPreviewError::Identity(
                MetadataIdentityError::AmbiguousDeclaration,
            ))),
            MetadataPathResolution::Unresolved => targets.push(Err(
                MetadataPreviewError::Identity(MetadataIdentityError::MissingDeclaration),
            )),
        }
    }
    Ok(())
}

pub(crate) fn property_references(facts: &XmlMetadataReferences) -> Vec<XmlMetadataReference> {
    facts
        .property_references
        .iter()
        .filter_map(|reference| {
            let kind = reference.names.first()?.text.parse().ok()?;
            let name = reference.names.get(1)?.clone();
            let mut members = Vec::new();
            let mut declared_member_kinds = Vec::new();
            for pair in reference.names[2..].as_chunks::<2>().0 {
                // Standard fields and resource tails have no user declaration UUID.
                if !matches!(
                    pair[0].text.as_str(),
                    "Attribute" | "TabularSection" | "Dimension" | "Resource" | "EnumValue"
                ) {
                    break;
                }
                declared_member_kinds.push(pair[0].text.clone());
                members.push(pair[1].clone());
            }
            Some(XmlMetadataReference {
                anchor: XmlReferenceAnchor::Metadata { kind, name },
                members,
                declared_member_kinds,
            })
        })
        .collect()
}

pub(crate) struct CompositionExpressionPaths {
    pub tokens: Vec<XmlNameToken>,
    pub paths: Vec<MetadataPathResolution>,
}

pub(crate) fn composition_expression_paths(
    db: &RootDatabaseImpl,
    file_id: FileId,
    facts: &XmlMetadataReferences,
    query: usize,
    package: &sdbl_hir::SdblPackage,
) -> Vec<CompositionExpressionPaths> {
    facts
        .data_set_expressions
        .iter()
        .filter(|expression| expression.query == query)
        .map(|expression| {
            let tokens = expression.path.tokens().unwrap_or_default();
            let members = tokens.get(1..).unwrap_or_default().to_vec();
            let paths = resolve_composition_expression(
                db,
                file_id,
                facts,
                query,
                package,
                &expression.path.text,
                &tokens,
            )
            .unwrap_or_else(|| vec![MetadataPathResolution::Unresolved; members.len()]);
            CompositionExpressionPaths { tokens: members, paths }
        })
        .collect()
}

fn resolve_composition_expression(
    db: &RootDatabaseImpl,
    file_id: FileId,
    facts: &XmlMetadataReferences,
    query: usize,
    package: &sdbl_hir::SdblPackage,
    expression: &str,
    tokens: &[XmlNameToken],
) -> Option<Vec<MetadataPathResolution>> {
    // Only the common direct identifier path is admitted; this is not a DCS expression parser.
    if parser::parse_sdbl(&format!("SELECT {expression}")).has_errors()
        || package.queries().len() != 1
    {
        return None;
    }
    let head = tokens.first()?;
    let fields = facts
        .data_set_fields
        .iter()
        .filter(|field| {
            field.query == query && stdx::case::eq_ignore_case(&field.data_path.text, &head.text)
        })
        .collect::<Vec<_>>();
    let [field] = fields.as_slice() else { return None };
    let backend = field.field.tokens()?;
    let [backend_head] = backend.as_slice() else { return None };
    let outputs = package.queries()[0]
        .hir
        .select
        .fields
        .iter()
        .filter(|output| {
            output
                .alias_or_name()
                .is_some_and(|name| stdx::case::eq_ignore_case(name.as_str(), &backend_head.text))
        })
        .collect::<Vec<_>>();
    let [output] = outputs.as_slice() else { return None };
    if !matches!(&output.ty, sdbl_hir::SdblType::Ref(_)) {
        return None;
    }
    let names = tokens[1..].iter().map(|token| hir::Name::new(&token.text)).collect::<Vec<_>>();
    let paths =
        hir::metadata_symbol::metadata_paths_for_sdbl_value(db, file_id, &output.ty, &names);
    if paths.len() != names.len()
        || paths.iter().any(|path| {
            matches!(path, MetadataPathResolution::Unresolved | MetadataPathResolution::Ambiguous)
        })
        || !matches!(paths.last(), Some(MetadataPathResolution::Resolved(_)))
    {
        return None;
    }
    Some(paths)
}
