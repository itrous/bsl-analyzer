//! Public projection of proven metadata uses and their source declaration identity.

use bsl_metadata::Uuid;
use syntax::{TextRange, TextSize};
use vfs::FileId;

pub use hir_ty::metadata_symbol::{MetadataPathResolution, MetadataSymbolPath};

/// Identity within the current workspace session; it is not a restart-stable journal key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MetadataSymbolIdentity {
    pub declaration_file: FileId,
    pub owner_uuid: Uuid,
    pub declaration_uuid: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataIdentityError {
    NoVisibleSource,
    MissingDeclaration,
    UnreadSource,
    AmbiguousDeclaration,
    UnprovedAdoption,
    UnprovedExternalOwner,
}

/// Source adapters bind semantic paths to UUID declarations in the visible root chain.
pub trait MetadataIdentityResolver {
    fn metadata_identity(
        &self,
        file_id: FileId,
        path: &MetadataSymbolPath,
    ) -> Result<MetadataSymbolIdentity, MetadataIdentityError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataUse {
    pub identity: MetadataSymbolIdentity,
    pub path: MetadataSymbolPath,
    pub range: TextRange,
}

pub fn is_query_receiver_at<DB: hir_ty::db::HirDatabase + base_db::RootQueryDb>(
    db: &DB,
    file_id: FileId,
    receiver: &syntax::SyntaxNode,
) -> bool {
    hir_ty::metadata_symbol::is_query_receiver_type(
        db,
        crate::Semantics::new(db).type_of_expr(file_id, receiver),
    )
}

pub fn metadata_receiver_has_members<DB: hir_ty::db::HirDatabase + base_db::RootQueryDb>(
    db: &DB,
    file_id: FileId,
    receiver: &syntax::SyntaxNode,
) -> bool {
    hir_ty::metadata_symbol::metadata_receiver_has_members(
        db,
        crate::Semantics::new(db).type_of_expr(file_id, receiver),
    )
}

pub fn metadata_reference_names<DB: hir_ty::db::HirDatabase>(
    db: &DB,
    file_id: FileId,
    path: &MetadataSymbolPath,
) -> Vec<crate::Name> {
    hir_ty::metadata_symbol::metadata_reference_names(
        &hir_ty::DbObjectResolver::new(db, file_id),
        path,
    )
}

/// Resolve only member names: ordinary symbols retain their existing references/rename path.
pub fn metadata_use_at<DB>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
) -> Result<Option<MetadataUse>, MetadataIdentityError>
where
    DB: hir_ty::db::HirDatabase + base_db::RootQueryDb + MetadataIdentityResolver,
{
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let Some(token) =
        root.token_at_offset(offset).right_biased().filter(|token| token.kind().is_name_token())
    else {
        return Ok(None);
    };
    match crate::classify_token(&token) {
        crate::NameClass::FieldName { receiver, token, is_call: false } => metadata_member_use(
            db,
            file_id,
            &receiver,
            &crate::Name::new(token.text()),
            token.text_range(),
        ),
        crate::NameClass::FreeName { token, is_call: false } => {
            if crate::Semantics::new(db).symbol_for_token(file_id, &token).is_some_and(|symbol| {
                !matches!(symbol.key, crate::SemanticSymbolKey::ImplicitLocal { .. })
            }) {
                return Ok(None);
            }
            project_resolution(
                db,
                file_id,
                hir_ty::metadata_symbol::metadata_path_for_implicit_field(
                    db,
                    file_id,
                    &crate::Name::new(token.text()),
                ),
                token.text_range(),
            )
        }
        _ => Ok(None),
    }
}

pub fn metadata_member_use<DB>(
    db: &DB,
    file_id: FileId,
    receiver: &syntax::SyntaxNode,
    name: &crate::Name,
    range: TextRange,
) -> Result<Option<MetadataUse>, MetadataIdentityError>
where
    DB: hir_ty::db::HirDatabase + base_db::RootQueryDb + MetadataIdentityResolver,
{
    let sema = crate::Semantics::new(db);
    let receiver_type = sema.type_of_expr(file_id, receiver);
    let resolver = hir_ty::DbObjectResolver::new(db, file_id);
    project_resolution(
        db,
        file_id,
        hir_ty::metadata_symbol::metadata_path_for_receiver(db, &resolver, receiver_type, name),
        range,
    )
}

fn project_resolution<DB: MetadataIdentityResolver>(
    db: &DB,
    file_id: FileId,
    resolution: MetadataPathResolution,
    range: TextRange,
) -> Result<Option<MetadataUse>, MetadataIdentityError> {
    match resolution {
        MetadataPathResolution::Resolved(path) => {
            let identity = db.metadata_identity(file_id, &path)?;
            Ok(Some(MetadataUse { identity, path, range }))
        }
        MetadataPathResolution::NonMetadata => Ok(None),
        MetadataPathResolution::Ambiguous => Err(MetadataIdentityError::AmbiguousDeclaration),
        MetadataPathResolution::Unresolved => Err(MetadataIdentityError::MissingDeclaration),
    }
}

/// Resolve a qualified metadata address through the normal type and field facts.
pub fn metadata_path_for_address<DB: hir_ty::db::HirDatabase>(
    db: &DB,
    file_id: FileId,
    address: &str,
) -> MetadataPathResolution {
    hir_ty::metadata_symbol::metadata_path_for_address(
        db,
        &hir_ty::DbObjectResolver::new(db, file_id),
        address,
    )
}

pub fn metadata_paths_for_form_binding<DB: hir_ty::db::HirDatabase>(
    db: &DB,
    file_id: FileId,
    attribute: &bsl_metadata::FormAttribute,
    members: &[crate::Name],
) -> Vec<MetadataPathResolution> {
    hir_ty::metadata_symbol::metadata_paths_for_form_binding(
        db,
        &hir_ty::DbObjectResolver::new(db, file_id),
        attribute,
        members,
    )
}

pub fn metadata_paths_for_sdbl_value<DB: hir_ty::db::HirDatabase>(
    db: &DB,
    file_id: FileId,
    value: &sdbl_hir::SdblType,
    members: &[crate::Name],
) -> Vec<MetadataPathResolution> {
    hir_ty::metadata_symbol::metadata_paths_for_sdbl_value(
        db,
        &hir_ty::DbObjectResolver::new(db, file_id),
        value,
        members,
    )
}
