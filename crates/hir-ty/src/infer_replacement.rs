use super::{routed_inference_context, HirDatabase, InferenceContext};
use bsl_platform::{PlatformData, PlatformVersion};
use hir_def::execution_env::{self, EnvFlags};
use hir_def::resolver::{QualifiedMethodError, Resolution};
use hir_def::{DefWithBodyId, Name};
use intern::NormName;
use vfs::FileId;

/// Whether the replacement resolves to its intended callable and compiles here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementApplicability {
    Applicable,
    Unavailable,
    Unknown,
}

/// Judge a hypothetical replacement through the active body's ordinary name cascade.
/// Missing ownership or environment facts withhold the recommendation; providers
/// without a file context retain the existing project-default platform checks.
pub fn replacement_applicability<'db>(
    db: &'db dyn HirDatabase,
    file: FileId,
    owner: DefWithBodyId,
    target: &str,
    effective: Option<hir_def::effective_module::EffectiveModuleId<'db>>,
    weaving: Option<hir_def::weaving::WeavingModuleId<'db>>,
) -> ReplacementApplicability {
    let Some(mut context) = routed_inference_context(db, file, owner, effective, weaving) else {
        return ReplacementApplicability::Unknown;
    };
    context.replacement_applicability(target)
}

impl InferenceContext<'_> {
    fn replacement_applicability(&mut self, target: &str) -> ReplacementApplicability {
        let file_context = self.db.file_compilation_context(self.context_file_id);
        if file_context.as_ref().is_some_and(|context| {
            context.origin == hir_def::compilation_context::CompilationContextOrigin::Unknown
        }) {
            return ReplacementApplicability::Unknown;
        }
        let raw_mode = self.db.compatibility_mode_for_file(self.context_file_id);
        let parsed_mode = raw_mode.as_deref().and_then(PlatformVersion::parse_compatibility_mode);
        let mode = parsed_mode.flatten();
        let unknown_mode = file_context.is_some() && parsed_mode.is_none();
        let (receiver, member) = match target.split_once('.') {
            Some((receiver, member)) if !receiver.is_empty() && !member.contains('.') => {
                (Some(Name::new(receiver)), Name::new(member))
            }
            None if !target.is_empty() => (None, Name::new(target)),
            _ => return ReplacementApplicability::Unknown,
        };
        if let Some(receiver) = receiver {
            return self.qualified_replacement(&receiver, &member, mode, unknown_mode);
        }
        if self.is_call_name_shadowed(&member)
            || self.bare_receiver_global_method(&member, 0).is_some()
        {
            return ReplacementApplicability::Unavailable;
        }
        if file_context.is_some()
            && (!self.db.workspace_load_complete()
                || self.global_surface_incomplete
                || matches!(self.implicit_self_surface(), super::ImplicitSelfSurface::Unreadable))
        {
            return ReplacementApplicability::Unknown;
        }
        let threshold = crate::compat_mode::hidden_global(member.as_str());
        if threshold.is_some() && unknown_mode {
            return ReplacementApplicability::Unknown;
        }
        if newer(threshold, mode)
            || newer(crate::min_platform::global_function(member.as_str()), self.min_platform)
        {
            return ReplacementApplicability::Unavailable;
        }
        let Some(signatures) = crate::builtin::builtin_functions().get(member.as_str()) else {
            return ReplacementApplicability::Unknown;
        };
        let environment = signatures
            .iter()
            .fold(EnvFlags::EMPTY, |env, signature| env | unrestricted(signature.env()));
        self.replacement_environment(environment)
    }

    fn qualified_replacement(
        &mut self,
        receiver: &Name,
        member: &Name,
        mode: Option<PlatformVersion>,
        unknown_mode: bool,
    ) -> ReplacementApplicability {
        let resolver = self.get_resolver();
        if matches!(
            resolver.resolve_name(self.db, receiver),
            Some(Resolution::Local(_) | Resolution::Variable(_) | Resolution::Method(_))
        ) || self.body_declares_binding(receiver)
            || (self.assignment_target_names.contains(&NormName::intern(receiver.as_str()))
                && self.assignment_can_declare_implicit_local(receiver))
            || crate::form_self::resolve_form_self_property(self.db, &resolver, receiver).is_some()
            || crate::form_attr::resolve_form_attribute(self.db, &resolver, receiver).is_some()
            || crate::this_object_attr::resolve_this_object_member(self.db, &resolver, receiver)
                .is_some()
            || crate::this_object_attr::resolve_this_record_set_member(self.db, &resolver, receiver)
                .is_some()
        {
            return ReplacementApplicability::Unavailable;
        }
        if resolver.user_common_module_exists(self.db, receiver) {
            if PlatformData::instance()
                .resolve_global_member(receiver.as_str(), member.as_str())
                .is_some()
            {
                return ReplacementApplicability::Unavailable;
            }
            let target = match resolver.resolve_qualified_method(self.db, receiver, member) {
                Ok(target) if target.is_export => target,
                Ok(_)
                | Err(QualifiedMethodError::NotFound | QualifiedMethodError::NotVisibleInConfigs) =>
                {
                    return ReplacementApplicability::Unavailable;
                }
                Err(QualifiedMethodError::BodyUnread) => return ReplacementApplicability::Unknown,
            };
            let Some(module) =
                self.db.resolve_common_module(self.context_file_id, receiver.as_str())
            else {
                return ReplacementApplicability::Unknown;
            };
            let mut metadata = (*self.db.module_metadata(target.method_id.module)).clone();
            metadata.common_module = Some(module.clone());
            let options = self.db.env_options_for_file(self.context_file_id);
            let Some(declaration) = self.db.interface_method(target.method_id) else {
                return ReplacementApplicability::Unknown;
            };
            let Some(possible_environment) =
                crate::method_environment::possible_method_env_with_metadata(
                    self.db,
                    target.method_id,
                    &metadata,
                    &options,
                )
            else {
                return ReplacementApplicability::Unknown;
            };
            let call_environment = |environment: EnvFlags| {
                if module.is_server_call() && environment.contains(EnvFlags::SERVER) {
                    environment | EnvFlags::SERVER_CALL_CALLERS
                } else {
                    environment
                }
            };
            let proven_environment = possible_environment & declaration.preproc_env;
            let verdict = self.replacement_environment(call_environment(proven_environment));
            return if verdict == ReplacementApplicability::Unavailable
                && self.replacement_environment(call_environment(possible_environment))
                    != ReplacementApplicability::Unavailable
            {
                ReplacementApplicability::Unknown
            } else {
                verdict
            };
        }
        if self.global_read_export(receiver).is_some() {
            return ReplacementApplicability::Unavailable;
        }
        if self.db.file_compilation_context(self.context_file_id).is_some()
            && (!self.db.workspace_load_complete() || self.global_surface_incomplete)
        {
            return ReplacementApplicability::Unknown;
        }
        let platform = PlatformData::instance();
        let Some(property) = platform.get_global_property(receiver.as_str()) else {
            return ReplacementApplicability::Unknown;
        };
        let Some(method) = platform.resolve_global_member(receiver.as_str(), member.as_str())
        else {
            return ReplacementApplicability::Unknown;
        };
        let receiver_threshold = crate::compat_mode::hidden_global(receiver.as_str());
        let member_threshold =
            crate::compat_mode::hidden_type_member(method.type_name.as_str(), member.as_str());
        if unknown_mode && (receiver_threshold.is_some() || member_threshold.is_some()) {
            return ReplacementApplicability::Unknown;
        }
        if newer(receiver_threshold, mode)
            || newer(member_threshold, mode)
            || newer(
                crate::min_platform::global_member(receiver.as_str(), member.as_str()),
                self.min_platform,
            )
        {
            return ReplacementApplicability::Unavailable;
        }
        self.replacement_environment(
            unrestricted(EnvFlags::from_platform_context(property.context.as_ref()))
                & unrestricted(EnvFlags::from_platform_context(method.context.as_ref())),
        )
    }

    fn replacement_environment(&self, available: EnvFlags) -> ReplacementApplicability {
        if self.local_symbols.is_some() || self.weaving_base.is_some() {
            return ReplacementApplicability::Unknown;
        }
        let options = self.db.env_options_for_file(self.context_file_id);
        let context = self.db.file_compilation_context(self.context_file_id);
        let environment = match self.owner {
            DefWithBodyId::Method(local_id) => crate::method_environment::possible_method_env(
                self.db,
                hir_def::MethodId {
                    module: hir_def::ModuleId::new(self.context_file_id),
                    local_id,
                },
                &options,
            ),
            DefWithBodyId::ModuleCode => {
                let metadata =
                    self.db.module_metadata(hir_def::ModuleId::new(self.context_file_id));
                Some(context.as_ref().map_or_else(
                    || execution_env::module_code_env(&metadata, &options),
                    |context| context.module_environment(&metadata),
                ))
            }
        };
        let Some(environment) = environment.filter(|env| !env.is_empty()) else {
            return if context.is_none() {
                ReplacementApplicability::Applicable
            } else {
                ReplacementApplicability::Unknown
            };
        };
        if environment.without(available).is_empty() {
            return ReplacementApplicability::Applicable;
        }
        let checked = environment & options.checked_environments;
        if checked.is_empty() {
            ReplacementApplicability::Unknown
        } else if checked.without(available).is_empty() {
            ReplacementApplicability::Applicable
        } else {
            ReplacementApplicability::Unavailable
        }
    }
}

fn newer(since: Option<PlatformVersion>, target: Option<PlatformVersion>) -> bool {
    matches!((since, target), (Some(since), Some(target)) if since.release_newer_than(target))
}

fn unrestricted(environment: EnvFlags) -> EnvFlags {
    if environment.is_empty() {
        EnvFlags::ALL
    } else {
        environment
    }
}
