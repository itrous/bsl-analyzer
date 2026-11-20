use hir_def::execution_env::{self, EnvFlags, EnvOptions};
use hir_def::MethodId;

use crate::db::HirDatabase;

/// Environments a method body runs in: its module's, narrowed by its own
/// directive and by the module-level `#Если` regions around it. Read off the
/// position-free interface so a per-method query stays valid across edits.
pub(crate) fn effective_method_env(
    db: &dyn HirDatabase,
    method: MethodId,
    options: &EnvOptions,
) -> EnvFlags {
    let metadata = db.module_metadata(method.module);
    let Some(decl) = db.interface_method(method) else {
        return db.file_compilation_context(method.module.file_id).map_or_else(
            || execution_env::body_env(&metadata, &[], options),
            |context| context.body_environment(&metadata, &[]),
        );
    };
    let environment = db.file_compilation_context(method.module.file_id).map_or_else(
        || execution_env::body_env(&metadata, &decl.directives, options),
        |context| context.body_environment(&metadata, &decl.directives),
    );
    if environment.is_empty() {
        return environment;
    }
    environment & decl.preproc_env
}

/// Environments that may compile a method body: its module's, narrowed by its
/// directive and by the module-level `#Если` regions around it, keeping those
/// the regions could not decide. `None` — the environment is unknown; an empty
/// set — the regions rule out every environment and the platform drops the
/// whole method.
pub(crate) fn possible_method_env(
    db: &dyn HirDatabase,
    method: MethodId,
    options: &EnvOptions,
) -> Option<EnvFlags> {
    let metadata = db.module_metadata(method.module);
    possible_method_env_with_metadata(db, method, &metadata, options)
}

pub(crate) fn possible_method_env_with_metadata(
    db: &dyn HirDatabase,
    method: MethodId,
    metadata: &hir_def::ModuleMetadata,
    options: &EnvOptions,
) -> Option<EnvFlags> {
    let decl = db.interface_method(method)?;
    let environment = db.file_compilation_context(method.module.file_id).map_or_else(
        || execution_env::body_env(metadata, &decl.directives, options),
        |context| context.body_environment(metadata, &decl.directives),
    );
    if environment.is_empty() {
        return None;
    }
    Some(environment & (decl.preproc_env | decl.preproc_undecided))
}

/// [`effective_method_env`] or the module-code environment, by body owner —
/// the one answer position-based IDE queries share with inference.
pub fn owner_body_env(
    db: &dyn HirDatabase,
    file_id: vfs::FileId,
    owner: hir_def::DefWithBodyId,
) -> EnvFlags {
    let options = db.env_options();
    match owner {
        hir_def::DefWithBodyId::Method(local_id) => effective_method_env(
            db,
            MethodId { module: hir_def::ModuleId { file_id }, local_id },
            &options,
        ),
        hir_def::DefWithBodyId::ModuleCode => {
            let metadata = db.module_metadata(hir_def::ModuleId { file_id });
            execution_env::module_code_env(&metadata, &options)
        }
    }
}
