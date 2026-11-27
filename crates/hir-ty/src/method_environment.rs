use hir_def::call_graph::NotifyReg;
use hir_def::effective_module::EffectiveModuleId;
use hir_def::execution_env::{self, EnvFlags, EnvOptions};
use hir_def::weaving::WeavingModuleId;
use hir_def::{MethodId, ModuleId, Name};
use rustc_hash::FxHashSet;

use crate::db::HirDatabase;

/// Runtime models considered by notify projection, independent of which build a project editor
/// currently selects. A registration is checked against source-supported environments; narrower
/// constructor and preprocessor facts still restrict each site.
pub(crate) fn notify_env_options() -> EnvOptions {
    EnvOptions {
        client_environments: EnvFlags::ALL_CLIENTS | EnvFlags::MOBILE_CLIENT,
        ordinary_app_support: true,
        checked_environments: EnvFlags::ALL,
        ordinary_forms_in_managed_app: None,
        managed_run_mode: true,
    }
}

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

/// Local declarations that may satisfy a callback on the current module object,
/// including paired base declarations for effective and weaving routes.
pub fn notify_handler_methods(
    db: &dyn HirDatabase,
    file_id: vfs::FileId,
    name: &Name,
    effective: Option<EffectiveModuleId<'_>>,
    weaving: Option<WeavingModuleId<'_>>,
) -> Vec<MethodId> {
    let mut methods: Vec<MethodId> = match effective {
        Some(eid) => hir_def::effective_module::symbol_tree_effective(db, eid)
            .find_methods(name)
            .into_iter()
            .map(|method| method.id)
            .collect(),
        None => db
            .symbol_tree(ModuleId::new(file_id))
            .find_methods(name)
            .into_iter()
            .map(|method| method.id)
            .collect(),
    };

    if effective.is_some() {
        // The splice omits extension additions outside change-and-validate methods. Those
        // declarations remain visible on the combined object, except interceptors: their name
        // identifies the wrapper rather than a callable callback handler.
        methods.extend(
            db.symbol_tree(ModuleId::new(file_id))
                .find_methods(name)
                .into_iter()
                .filter(|method| {
                    db.interface_method(method.id).is_none_or(|declaration| {
                        !declaration.directives.iter().any(|directive| directive.is_interception())
                    })
                })
                .map(|method| method.id),
        );
    }
    if let Some(wid) = weaving {
        methods.extend(
            db.symbol_tree(ModuleId::new(wid.base_file(db)))
                .find_methods(name)
                .into_iter()
                .map(|method| method.id),
        );
    }

    let mut seen = FxHashSet::default();
    methods.retain(|method| seen.insert(*method));
    methods
}

/// Outcome of checking declarations provide an exported handler in every proven
/// environment of a particular registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyHandlerAvailability {
    Exported,
    NotExported,
    NotFound,
    Unknown,
}

/// Checks same-module callback declarations against one registration, accounting for
/// form surfaces that are not represented by the supplied declarations.
pub fn notify_handler_availability_in_registration(
    db: &dyn HirDatabase,
    module: hir_def::ModuleId,
    registration: &NotifyReg,
    methods: &[MethodId],
) -> NotifyHandlerAvailability {
    let metadata = db.module_metadata(module);
    // A base form can supply a handler outside the local declaration surface.
    if !notify_form_is_eligible(&metadata)
        || metadata.form.as_deref().is_some_and(|form| form.has_base_form())
    {
        return NotifyHandlerAvailability::Unknown;
    }
    notify_handler_availability_for_bodies(db, registration, &[methods.to_vec()], false)
}

fn notify_form_is_eligible(metadata: &hir_def::ModuleMetadata) -> bool {
    // Ordinary and unreadable forms cannot be judged by the managed-form contract.
    metadata.module_type != bsl_metadata::ModuleType::FormModule
        || metadata.form.as_deref().is_some_and(|form| form.is_managed())
}

/// Whether this exported declaration participates in a registration.
///
/// This is deliberately separate from checking whether the registration's complete local
/// surface is exported: another conditional declaration may provide a non-exported alternative.
pub fn notify_method_is_exported_in_registration(
    db: &dyn HirDatabase,
    registration: &NotifyReg,
    method: MethodId,
    methods: &[MethodId],
) -> bool {
    methods.contains(&method)
        && db.interface_method(method).is_some_and(|declaration| declaration.is_export)
        && possible_method_env(db, method, &notify_env_options())
            .is_some_and(|environment| environment.intersects(registration.environments.possible))
}

/// The methods that provide exported handlers in any supplied registration environment.
///
/// The HIR identifiers and declaration environments are evaluated once per method; registrations
/// sharing a callback name are combined through their possible environments.
pub fn notify_methods_exported_in_registrations(
    db: &dyn HirDatabase,
    methods: &[MethodId],
    registrations: &[&NotifyReg],
) -> FxHashSet<MethodId> {
    let mut environments = EnvFlags::EMPTY;
    for (index, registration) in registrations.iter().enumerate() {
        if index & 0xff == 0 {
            db.unwind_if_revision_cancelled();
        }
        environments = environments.union(registration.environments.possible);
    }
    let options = notify_env_options();
    let mut exported = FxHashSet::default();
    for (index, &method) in methods.iter().enumerate() {
        if index & 0xff == 0 {
            db.unwind_if_revision_cancelled();
        }
        if db.interface_method(method).is_some_and(|declaration| declaration.is_export)
            && possible_method_env(db, method, &options)
                .is_some_and(|environment| environment.intersects(environments))
        {
            exported.insert(method);
        }
    }
    exported
}

/// Checks conditional declarations in common-module bodies against the registration's
/// environments. Each inner slice is one body, in resolver priority order. Within a body,
/// declarations are combined because conditional declarations may provide complementary
/// runtime surfaces. A later body is considered only in environments where earlier bodies
/// have no possible declaration. `incomplete_tail` keeps unread or unmapped expected bodies
/// from being mistaken for a definitive absence. Form eligibility is determined from the
/// module containing the registration; a base form does not affect a foreign module's surface.
pub fn notify_handler_availability_for_method_bodies_in_registration(
    db: &dyn HirDatabase,
    module: hir_def::ModuleId,
    registration: &NotifyReg,
    method_bodies: &[Vec<MethodId>],
    incomplete_tail: bool,
) -> NotifyHandlerAvailability {
    if !notify_form_is_eligible(&db.module_metadata(module)) {
        return NotifyHandlerAvailability::Unknown;
    }
    notify_handler_availability_for_bodies(db, registration, method_bodies, incomplete_tail)
}

fn notify_handler_availability_for_bodies(
    db: &dyn HirDatabase,
    registration: &NotifyReg,
    method_bodies: &[Vec<MethodId>],
    incomplete_tail: bool,
) -> NotifyHandlerAvailability {
    use NotifyHandlerAvailability as Availability;

    let registration_definite = registration.environments.definite;
    let registration_possible = registration.environments.possible;
    if registration_definite.is_empty() {
        return Availability::Unknown;
    }
    if method_bodies.iter().all(Vec::is_empty) && !incomplete_tail {
        return Availability::NotFound;
    }

    let options = notify_env_options();

    let mut body_environments = Vec::with_capacity(method_bodies.len());
    for methods in method_bodies {
        let mut exported_definite = EnvFlags::EMPTY;
        let mut exported_possible = EnvFlags::EMPTY;
        let mut other_definite = EnvFlags::EMPTY;
        let mut other_possible = EnvFlags::EMPTY;
        for &method in methods {
            let Some(declaration) = db.interface_method(method) else {
                return Availability::Unknown;
            };
            let definite = effective_method_env(db, method, &options);
            let Some(possible) = possible_method_env(db, method, &options) else {
                return Availability::Unknown;
            };
            let (definite_env, possible_env) = if declaration.is_export {
                (&mut exported_definite, &mut exported_possible)
            } else {
                (&mut other_definite, &mut other_possible)
            };
            *definite_env = *definite_env | definite;
            *possible_env = *possible_env | possible;
        }
        body_environments.push((
            exported_definite,
            exported_possible,
            other_definite,
            other_possible,
        ));
    }

    enum EnvironmentVerdict {
        Exported,
        NotExported,
        Unknown,
    }

    let mut has_unexported_registration_env = false;
    let mut has_missing_registration_env = false;
    let mut has_undecided_registration_env = false;

    for environment in registration_definite.iter() {
        let mut environment_verdict = None;
        for &(exported_definite, exported_possible, other_definite, other_possible) in
            &body_environments
        {
            environment_verdict = if exported_definite.contains(environment) {
                Some(EnvironmentVerdict::Exported)
            } else if exported_possible.contains(environment) {
                Some(EnvironmentVerdict::Unknown)
            } else if other_definite.contains(environment) {
                Some(EnvironmentVerdict::NotExported)
            } else if other_possible.contains(environment) {
                Some(EnvironmentVerdict::Unknown)
            } else {
                None
            };
            if environment_verdict.is_some() {
                break;
            }
        }

        match environment_verdict {
            Some(EnvironmentVerdict::Exported) => {}
            Some(EnvironmentVerdict::NotExported) => has_unexported_registration_env = true,
            Some(EnvironmentVerdict::Unknown) => has_undecided_registration_env = true,
            None if incomplete_tail => has_undecided_registration_env = true,
            None => has_missing_registration_env = true,
        }
    }

    if has_unexported_registration_env {
        Availability::NotExported
    } else if has_missing_registration_env {
        Availability::NotFound
    } else if has_undecided_registration_env
        || !registration_possible.without(registration_definite).is_empty()
    {
        Availability::Unknown
    } else {
        Availability::Exported
    }
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
