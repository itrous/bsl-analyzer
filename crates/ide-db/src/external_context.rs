//! Per-file context from the owner's published roots and source-backed configuration header.

use crate::{metadata, vfs_helpers, RootDatabase, RootDatabaseImpl};
use hir::{CompilationContextOrigin, FileCompilationContext};
use project_model::ExternalConfigurationOwner;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use vfs::FileId;

impl RootDatabaseImpl {
    /// The compilation context of an external export or a prepared additional owner.
    /// Ordinary project files keep the existing project-wide settings.
    pub fn file_compilation_context(&self, file_id: FileId) -> Option<Arc<FileCompilationContext>> {
        let path = vfs_helpers::get_file_path(self, file_id)?;
        let snapshot = self.workspace_configs_snapshot();
        let prepared = snapshot.prepared_roots();
        let matched = prepared
            .iter()
            .enumerate()
            .filter_map(|(index, (_, configured, canonical))| {
                [configured, canonical]
                    .into_iter()
                    .filter(|root| path.starts_with(root))
                    .map(|root| root.as_os_str().len())
                    .max()
                    .map(|len| (len, index))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, index)| index)?;
        let (origin, root) = if matched >= snapshot.paths.len() {
            (CompilationContextOrigin::SourceConfiguration, Some(prepared[matched].1.clone()))
        } else {
            match snapshot.external_owner(matched)? {
                ExternalConfigurationOwner::Unknown => (CompilationContextOrigin::Unknown, None),
                ExternalConfigurationOwner::LegacyProjectDefault(root) => {
                    let path = prepared
                        .iter()
                        .find(|(_, _, canonical)| canonical == &root.canonical_path)
                        .map_or(root.path, |(_, path, _)| path.clone());
                    (CompilationContextOrigin::LegacyExternalDefault, Some(path))
                }
                ExternalConfigurationOwner::Selected(root) => {
                    let path = prepared
                        .iter()
                        .find(|(_, _, canonical)| canonical == &root.canonical_path)
                        .map_or(root.path, |(_, path, _)| path.clone());
                    (CompilationContextOrigin::ExplicitExternalOwner, Some(path))
                }
            }
        };
        let mut context = FileCompilationContext {
            origin,
            configuration_root: root.clone(),
            compatibility_mode: None,
            environment: self.env_options(),
            default_run_mode: None,
        };
        let is_main_owner = root.as_ref().is_some_and(|root| {
            let canonical = prepared
                .iter()
                .find(|(_, path, _)| path == root)
                .map(|(_, _, canonical)| canonical);
            snapshot.paths.iter().enumerate().any(|(index, (label, _))| {
                label.is_none() && canonical == snapshot.canonical_paths.get(index)
            })
        });
        if origin == CompilationContextOrigin::LegacyExternalDefault {
            context.compatibility_mode = self.compatibility_mode();
        }
        let header = root.as_ref().and_then(|root| self.compilation_configuration_header(root));
        if header.as_ref().is_some_and(|header| header.is_extension())
            || (origin != CompilationContextOrigin::LegacyExternalDefault && header.is_none())
        {
            context.origin = CompilationContextOrigin::Unknown;
            context.compatibility_mode = None;
            context.environment.ordinary_forms_in_managed_app = None;
            context.environment.ordinary_app_support = false;
            context.environment.managed_run_mode = false;
            return Some(Arc::new(context));
        }
        if let Some(header) = header {
            context.default_run_mode = header.default_run_mode().map(Arc::from);
            if origin != CompilationContextOrigin::LegacyExternalDefault {
                context.compatibility_mode = header.compatibility_mode().map(Arc::from);
                context.environment.ordinary_forms_in_managed_app =
                    header.ordinary_forms_in_managed_application_declared();
                context.environment.ordinary_app_support =
                    matches!(header.default_run_mode(), Some("OrdinaryApplication"));
                if is_main_owner {
                    if let Some(mode) = &snapshot.compilation_overrides.compatibility_mode {
                        context.compatibility_mode = Some(Arc::from(mode.as_str()));
                    }
                    if let Some(ordinary) = snapshot.compilation_overrides.ordinary_app_support {
                        context.environment.ordinary_app_support = ordinary;
                    }
                }
                context.environment.ordinary_app_support |= context
                    .environment
                    .checked_environments
                    .contains(hir::execution_env::EnvFlags::THICK_CLIENT_ORDINARY);
                context.environment.managed_run_mode =
                    matches!(header.default_run_mode(), Some("ManagedApplication" | "Auto"));
            }
        }
        Some(Arc::new(context))
    }

    fn compilation_configuration_header(
        &self,
        root: &Path,
    ) -> Option<Arc<bsl_metadata::Configuration>> {
        let key = metadata::intern_configuration_path(
            self,
            &root.to_string_lossy(),
            self.config_root_revision_for_path(root),
        );
        compilation_header(self, key)
    }

    /// Ordered declaration roots whose metadata identity is established for this source file.
    /// A legacy analysis default does not prove an external object's runtime metadata owner.
    pub fn metadata_symbol_roots_for_file(
        &self,
        file_id: FileId,
        target_kind: bsl_metadata::MdoType,
    ) -> Option<Vec<PathBuf>> {
        let path = vfs_helpers::get_file_path(self, file_id)?;
        let snapshot = self.workspace_configs_snapshot();
        if let Some(index) =
            snapshot.owner_of_path(&path).filter(|index| snapshot.is_external(*index))
        {
            let own_kind = match snapshot.kinds[index] {
                metadata::RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor) => {
                    bsl_metadata::MdoType::ExternalDataProcessor
                }
                metadata::RootKind::External(bsl_metadata::ExternalObjectKind::Report) => {
                    bsl_metadata::MdoType::ExternalReport
                }
                _ => return None,
            };
            if target_kind == own_kind {
                return Some(vec![snapshot.paths[index].1.clone()]);
            }
            if matches!(
                target_kind,
                bsl_metadata::MdoType::ExternalDataProcessor
                    | bsl_metadata::MdoType::ExternalReport
            ) {
                return Some(Vec::new());
            }
            if !self
                .file_compilation_context(file_id)
                .is_some_and(|context| context.has_proven_external_owner())
            {
                return None;
            }
        }
        if self
            .file_compilation_context(file_id)
            .is_some_and(|context| context.origin == CompilationContextOrigin::Unknown)
        {
            return None;
        }
        let visible = self.visible_roots_for_file(file_id)?;
        Some(
            visible
                .main
                .into_iter()
                .chain(visible.chain.into_iter().filter_map(|(_, root)| {
                    let index = snapshot.paths.iter().position(|(_, path)| path == &root);
                    (!index.is_some_and(|index| snapshot.is_external(index))).then_some(root)
                }))
                .collect(),
        )
    }
}

#[salsa::tracked(returns(clone))]
fn compilation_header<'db>(
    db: &'db dyn RootDatabase,
    root: metadata::ConfigurationPathInput<'db>,
) -> Option<Arc<bsl_metadata::Configuration>> {
    let _ = root.root_revision(db);
    let source = db.try_source_root_input(base_db::METADATA_SOURCE_ROOT)?;
    let fallback = PathBuf::from(root.path(db));
    let prepared = metadata::WorkspaceConfigsInput::try_get(db)
        .map(|input| input.snapshot(db).prepared_roots())
        .unwrap_or_default();
    // Interned keys normalize Windows spelling; VFS candidates retain the published spelling.
    let paths = prepared
        .iter()
        .find(|(_, configured, canonical)| {
            [configured, canonical].into_iter().any(|path| {
                metadata::canonicalize_configuration_path(&path.to_string_lossy()) == *root.path(db)
            })
        })
        .map_or([&fallback, &fallback], |(_, configured, canonical)| [configured, canonical]);
    let header = paths.into_iter().find_map(|path| {
        let candidate = path
            .join(bsl_conventions::ConventionalName::ConfigurationXml.canonical())
            .to_string_lossy()
            .into_owned();
        base_db::resolve_vfs_path_ci_query(
            db,
            source,
            candidate,
            &[bsl_conventions::SegmentMatch::Ci],
        )
    })?;
    if db.file_is_unread(header) {
        return None;
    }
    bsl_metadata::Configuration::from_xml_str(db.file_text_ref(header)).ok().map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_db::{SourceDatabase, SourceRoot, BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT};
    use project_model::OwnerConfigurationRoot;
    use vfs::{file_set::FileSet, VfsPath};

    #[test]
    fn prepared_owner_header_uses_declared_or_canonical_spelling() {
        let base = std::path::absolute("/ExternalHeaderFixture").unwrap();
        let external = base.join("External");
        let configured = base.join("DeclaredOwner");
        let canonical = base.join("CanonicalOwner");
        for header_root in [&configured, &canonical] {
            let mut db = RootDatabaseImpl::new();
            let mut snapshot = metadata::WorkspaceConfigsSnapshot::from_paths(vec![(
                Some("External".into()),
                external.clone(),
            )]);
            snapshot.kinds[0] =
                metadata::RootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor);
            snapshot.external_owners.push((
                0,
                ExternalConfigurationOwner::Selected(OwnerConfigurationRoot {
                    path: configured.clone(),
                    canonical_path: canonical.clone(),
                }),
            ));
            db.set_workspace_configs_snapshot(snapshot);
            let module = FileId(1);
            let mut sources = FileSet::new();
            sources.insert(module, VfsPath::new(external.join("Processor/Ext/ObjectModule.bsl")));
            db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(sources));
            db.set_file_source_root(module, BSL_SOURCE_ROOT);
            db.set_file_text(module, "");
            let header = FileId(2);
            let mut metadata = FileSet::new();
            metadata.insert(
                header,
                VfsPath::new(
                    header_root
                        .join(bsl_conventions::ConventionalName::ConfigurationXml.canonical()),
                ),
            );
            db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(metadata));
            db.set_file_source_root(header, METADATA_SOURCE_ROOT);
            db.set_file_text(header, "<MetaDataObject><Configuration uuid='00000000-0000-0000-0000-000000000001'><Properties><Name>Owner</Name><CompatibilityMode>Version8_2_16</CompatibilityMode><DefaultRunMode>OrdinaryApplication</DefaultRunMode></Properties></Configuration></MetaDataObject>");
            let context = db.file_compilation_context(module).unwrap();
            assert_eq!(
                context.origin,
                CompilationContextOrigin::ExplicitExternalOwner,
                "{header_root:?}"
            );
            assert_eq!(context.configuration_root.as_ref(), Some(&configured));
            assert_eq!(context.compatibility_mode.as_deref(), Some("Version8_2_16"));
            assert_eq!(context.default_run_mode.as_deref(), Some("OrdinaryApplication"));
        }
    }
}
