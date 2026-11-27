//! The selected configuration context of a source file, without project-loader dependencies.

use std::path::PathBuf;
use std::sync::Arc;

use crate::execution_env::EnvOptions;

/// Why the configuration context may be used for this source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilationContextOrigin {
    SourceConfiguration,
    /// A preserved analysis default for an external object; not a proven runtime owner.
    LegacyExternalDefault,
    ExplicitExternalOwner,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCompilationContext {
    pub origin: CompilationContextOrigin,
    pub configuration_root: Option<PathBuf>,
    pub compatibility_mode: Option<Arc<str>>,
    pub environment: EnvOptions,
    /// The exact header value; absence does not establish an application mode.
    pub default_run_mode: Option<Arc<str>>,
}

impl FileCompilationContext {
    /// Environments of a module in this selected configuration.
    pub fn module_environment(
        &self,
        metadata: &crate::ModuleMetadata,
    ) -> crate::execution_env::EnvFlags {
        use crate::execution_env::{self, EnvFlags};
        if self.origin == CompilationContextOrigin::Unknown {
            return EnvFlags::EMPTY;
        }
        let is_external_object = metadata.module_type == bsl_metadata::ModuleType::ObjectModule
            && metadata.mdo.as_ref().is_some_and(|object| {
                matches!(
                    object.mdo_type,
                    bsl_metadata::MdoType::ExternalDataProcessor
                        | bsl_metadata::MdoType::ExternalReport
                )
            });
        if is_external_object
            && self.origin == CompilationContextOrigin::ExplicitExternalOwner
            && !matches!(
                self.default_run_mode.as_deref(),
                Some("OrdinaryApplication" | "ManagedApplication" | "Auto")
            )
        {
            return EnvFlags::EMPTY;
        }
        if is_external_object && self.default_run_mode.as_deref() == Some("OrdinaryApplication") {
            return EnvFlags::THICK_CLIENT_ORDINARY;
        }
        execution_env::module_base_env(metadata, &self.environment)
    }

    /// Apply the existing directive semantics to this configuration's module environment.
    pub fn body_environment(
        &self,
        metadata: &crate::ModuleMetadata,
        annotations: &[crate::item_tree::AnnotationKind],
    ) -> crate::execution_env::EnvFlags {
        crate::execution_env::body_env_with_base(
            metadata,
            annotations,
            &self.environment,
            self.module_environment(metadata),
        )
    }

    /// An actual external host identity is available only through an explicit owner.
    pub fn has_proven_external_owner(&self) -> bool {
        self.origin == CompilationContextOrigin::ExplicitExternalOwner
            && self.configuration_root.is_some()
    }
}
