use std::sync::Arc;

use base_db::FileIdInput;
use bsl_metadata::{
    AttributeType, MdoType, MetadataObject, MetadataResolver, QueryMetadataResolver, Register,
};
use vfs::FileId;

use crate::configs::ConfigsDatabase;
use crate::{DefDatabase, DefWithBodyId, MethodId, MethodIdInput, ModuleId, SdblExprId};
use cfg_types::ExprId;

/// Db-backed metadata resolution for SDBL lowering and completion: routes each lookup through
/// the file-scoped per-MDO [`ConfigsDatabase`] accessors so lowering a query
/// depends on just the metadata objects it references, not the whole merged
/// `Configuration`. Mirrors hir-ty's `DbObjectResolver` but lives here because
/// `sdbl_hir_for_file_query` only holds a `&dyn ConfigsDatabase`.
pub struct DbSdblResolver<'a> {
    db: &'a dyn ConfigsDatabase,
    file_id: FileId,
}

impl<'a> DbSdblResolver<'a> {
    /// Resolve metadata in the source roots visible to this file.
    pub fn new(db: &'a dyn ConfigsDatabase, file_id: FileId) -> Self {
        Self { db, file_id }
    }
}

impl std::fmt::Debug for DbSdblResolver<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbSdblResolver").field("file_id", &self.file_id).finish()
    }
}

impl MetadataResolver for DbSdblResolver<'_> {
    fn resolve_defined_type(&self, name: &str) -> Option<AttributeType> {
        self.db.resolve_defined_type(self.file_id, name)
    }
}

impl QueryMetadataResolver for DbSdblResolver<'_> {
    fn resolve_metadata_object(
        &self,
        mdo_type: MdoType,
        name: &str,
    ) -> Option<Arc<MetadataObject>> {
        self.db.resolve_metadata_object(self.file_id, mdo_type, name)
    }

    fn resolve_register(&self, mdo_type: MdoType, name: &str) -> Option<Arc<Register>> {
        self.db.resolve_register(self.file_id, mdo_type, name)
    }
}

pub type SdblInFile = Vec<(SdblExprId, syntax::SdblQueryInfo)>;

pub type SdblHirEntries = Arc<Vec<(SdblExprId, Arc<sdbl_hir::SdblPackage>)>>;

/// Approximate live heap bytes for Salsa's `memory_usage` report: the entries
/// vector backbone. The per-entry `SdblQueryInfo` (which pins a slice of the green
/// SDBL AST) is not deeply traversed, so this under-counts the AST payload; the
/// lowered HIR is accounted instead by [`sdbl_hir_for_file_heap`].
pub(crate) fn all_sdbl_in_file_heap(v: &Arc<SdblInFile>) -> usize {
    crate::heap_estimate::vec_bytes::<(SdblExprId, syntax::SdblQueryInfo)>(v.len())
}

/// Approximate live heap bytes for Salsa's `memory_usage` report: the entries
/// vector plus each uniquely-owned [`sdbl_hir::SdblPackage`]'s estimated heap.
pub(crate) fn sdbl_hir_for_file_heap(v: &SdblHirEntries) -> usize {
    let mut bytes =
        crate::heap_estimate::vec_bytes::<(SdblExprId, Arc<sdbl_hir::SdblPackage>)>(v.len());
    for (_, package) in v.iter() {
        bytes += package.estimated_heap();
    }
    bytes
}

#[salsa::tracked(lru = 128, heap_size = all_sdbl_in_file_heap, returns(clone))]
pub fn all_sdbl_in_file_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<SdblInFile> {
    let _span = tracing::debug_span!("all_sdbl_in_file", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let module_id = ModuleId::new(file_id);

    let module_bodies = db.module_bodies(module_id);
    let result = queries_in_bodies(&module_bodies);
    tracing::debug!(count = result.len(), "Collected SDBL from HIR");
    Arc::new(result)
}

pub(crate) fn queries_in_bodies(module_bodies: &crate::ModuleBodies) -> SdblInFile {
    let mut result = Vec::new();

    for (local_id, lowered) in module_bodies.iter_lower_results() {
        for (expr_id, literal, query) in lowered.body().sdbl_exprs() {
            let sdbl_expr_id = SdblExprId::from_method(local_id, expr_id);
            let info = syntax::SdblQueryInfo::from_query(literal.lift(lowered.base), query);
            result.push((sdbl_expr_id, info));
        }
    }

    if let Some(module_code) = module_bodies.module_code_result() {
        for (expr_id, literal, query) in module_code.body().sdbl_exprs() {
            let sdbl_expr_id = SdblExprId::from_module_code(expr_id);
            let info = syntax::SdblQueryInfo::from_query(literal.lift(module_code.base), query);
            result.push((sdbl_expr_id, info));
        }
    }

    result.sort_by_key(|(_, query_info)| query_info.bsl_literal_range.start());

    result
}

/// Lowered SDBL of one body's query literals. Resolves metadata per-MDO instead
/// of depending on the whole merged config, so editing an unrelated metadata
/// object does not re-lower these queries; gated on visible config presence
/// to keep "no config => no validation" — a standalone module must not flag
/// every table.
pub(crate) fn lower_body_queries(
    db: &dyn ConfigsDatabase,
    file_id: FileId,
    body: &crate::Body,
) -> Vec<(ExprId, Arc<sdbl_hir::SdblPackage>)> {
    let mut result = Vec::new();
    let mut resolver_ref: Option<Option<&dyn QueryMetadataResolver>> = None;
    let resolver = DbSdblResolver { db, file_id };
    for (expr_id, _literal, query) in body.sdbl_exprs() {
        let Some(sdbl_ast) = &query.query_ast else { continue };
        // The config check is a database read; take it only once a query needs it.
        let resolver_ref = *resolver_ref.get_or_insert_with(|| {
            db.file_has_visible_config(file_id).then_some(&resolver as &dyn QueryMetadataResolver)
        });
        let package = sdbl_hir::lower_sdbl_to_hir_with_resolver(sdbl_ast, resolver_ref);
        result.push((expr_id, Arc::new(package)));
    }
    result
}

pub type MethodSdblHir = Arc<Vec<(ExprId, Arc<sdbl_hir::SdblPackage>)>>;

fn method_sdbl_hir_heap(v: &MethodSdblHir) -> usize {
    let mut bytes =
        crate::heap_estimate::vec_bytes::<(ExprId, Arc<sdbl_hir::SdblPackage>)>(v.len());
    for (_, package) in v.iter() {
        bytes += package.estimated_heap();
    }
    bytes
}

/// SDBL HIR of one method's query literals. Keyed by the method so that the
/// method's inference depends on its own queries alone; the file-wide view
/// below is a fold over this. Held at the cap of `method_body`, which it reads.
#[salsa::tracked(lru = 8192, heap_size = method_sdbl_hir_heap, returns(clone))]
pub fn method_sdbl_hir_query<'db>(
    db: &'db dyn ConfigsDatabase,
    method: MethodIdInput<'db>,
) -> MethodSdblHir {
    let mid = method.method_id(db);
    let _span = tracing::debug_span!("method_sdbl_hir", ?mid).entered();
    let body = db.method_body_ref(method);
    Arc::new(lower_body_queries(db, mid.module.file_id, body))
}

/// The lowered package of one query literal, wherever its body lives.
pub fn sdbl_package_for(
    db: &dyn ConfigsDatabase,
    file_id: FileId,
    owner: DefWithBodyId,
    expr_id: ExprId,
) -> Option<Arc<sdbl_hir::SdblPackage>> {
    match owner {
        DefWithBodyId::Method(local_id) => {
            let method = MethodId { module: ModuleId::new(file_id), local_id };
            method_sdbl_hir_query(db, MethodIdInput::new(db, method))
                .iter()
                .find(|(id, _)| *id == expr_id)
                .map(|(_, package)| Arc::clone(package))
        }
        DefWithBodyId::ModuleCode => {
            let target = SdblExprId { owner, expr_id };
            sdbl_hir_for_file_query(db, FileIdInput::new(db, file_id))
                .iter()
                .find(|(id, _)| *id == target)
                .map(|(_, package)| Arc::clone(package))
        }
    }
}

#[salsa::tracked(lru = 64, heap_size = sdbl_hir_for_file_heap, returns(clone))]
pub fn sdbl_hir_for_file_query<'db>(
    db: &'db dyn ConfigsDatabase,
    file_id_input: FileIdInput<'db>,
) -> SdblHirEntries {
    let _span = tracing::debug_span!("sdbl_hir_for_file", ?file_id_input).entered();
    let file_id = file_id_input.file_id(db);
    let module_id = ModuleId::new(file_id);
    let module_bodies = db.module_bodies_ref(module_id);

    let mut result = Vec::new();
    for (local_id, _) in module_bodies.iter_bodies() {
        let method = MethodIdInput::new(db, MethodId { module: module_id, local_id });
        for (expr_id, package) in method_sdbl_hir_query(db, method).iter() {
            result.push((SdblExprId::from_method(local_id, *expr_id), Arc::clone(package)));
        }
    }
    if let Some(module_code) = module_bodies.module_code() {
        for (expr_id, package) in lower_body_queries(db, file_id, module_code) {
            result.push((SdblExprId::from_module_code(expr_id), package));
        }
    }

    tracing::debug!(count = result.len(), "Lowered SDBL HIR for file");
    Arc::new(result)
}

/// Retention cap of `method_sdbl_hir_query`; see `set_lowering_lru_sweep_mode`.
pub(crate) fn set_method_sdbl_hir_lru_capacity(db: &mut dyn ConfigsDatabase, cap: usize) {
    method_sdbl_hir_query::set_lru_capacity(db, cap);
}
