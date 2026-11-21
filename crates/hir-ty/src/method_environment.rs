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
        return execution_env::body_env(&metadata, &[], options);
    };
    let environment = execution_env::body_env(&metadata, &decl.directives, options);
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
    let decl = db.interface_method(method)?;
    let metadata = db.module_metadata(method.module);
    let environment = execution_env::body_env(&metadata, &decl.directives, options);
    if environment.is_empty() {
        return None;
    }
    Some(environment & (decl.preproc_env | decl.preproc_undecided))
}

/// Whether a declared method can access the implicit context of its form.
/// An unavailable declaration is treated as potentially having context.
pub fn method_has_form_context(method: Option<&hir_def::MethodDecl>) -> bool {
    !method.is_some_and(|method| {
        method.directives.iter().any(|directive| {
            matches!(
                directive,
                hir_def::item_tree::AnnotationKind::AtServerNoContext
                    | hir_def::item_tree::AnnotationKind::AtClientAtServerNoContext
            )
        })
    })
}

/// Whether a body can access the implicit context of its form.
pub fn owner_has_form_context(
    db: &dyn HirDatabase,
    file_id: vfs::FileId,
    owner: hir_def::DefWithBodyId,
) -> bool {
    let hir_def::DefWithBodyId::Method(local_id) = owner else { return true };
    let module = hir_def::ModuleId::new(file_id);
    method_has_form_context(db.interface_method(MethodId { module, local_id }).as_deref())
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

#[cfg(test)]
mod tests {
    use super::method_has_form_context;
    use hir_def::{
        execution_env::EnvFlags, item_tree::AnnotationKind, MethodDecl, MethodId, MethodKey,
        ModuleId, Name,
    };

    fn method_with_directives(directives: &[AnnotationKind]) -> MethodDecl {
        MethodDecl {
            id: MethodId {
                module: ModuleId::new(vfs::FileId(0)),
                local_id: MethodKey::first("Тест"),
            },
            name: Name::new("Тест"),
            is_function: false,
            is_export: false,
            params: Vec::new(),
            directives: directives.to_vec().into_boxed_slice(),
            preproc_env: EnvFlags::ALL,
            preproc_undecided: EnvFlags::EMPTY,
            docs: None,
            return_type_ref: None,
        }
    }

    #[test]
    fn missing_method_declaration_may_have_form_context() {
        assert!(method_has_form_context(None));
    }

    #[test]
    fn no_context_directives_remove_form_context() {
        for directive in
            [AnnotationKind::AtServerNoContext, AnnotationKind::AtClientAtServerNoContext]
        {
            assert!(!method_has_form_context(Some(&method_with_directives(&[directive]))));
        }
    }

    #[test]
    fn contextual_method_has_form_context() {
        assert!(method_has_form_context(Some(&method_with_directives(&[
            AnnotationKind::AtServer,
        ]))));
    }
}
