use std::sync::Arc;

use bsl_conventions::{ConventionalName, XML_EXTENSION};

use bsl_metadata::inspection_sources::InspectionIdentity;
use bsl_metadata::{Form, FormAttribute, ObjectBelonging};
use hir::metadata_symbol::{MetadataIdentityError, MetadataIdentityResolver, MetadataSymbolPath};
use vfs::FileId;

use crate::{metadata::RootKind, RootDatabaseImpl};

impl RootDatabaseImpl {
    /// Compose only UUID-attested form sources from the prepared visible root chain.
    pub fn metadata_rename_form_attributes(
        &self,
        form_xml_file: FileId,
    ) -> Result<Arc<Vec<FormAttribute>>, MetadataIdentityError> {
        let path = crate::vfs_helpers::get_file_path(self, form_xml_file)
            .ok_or(MetadataIdentityError::NoVisibleSource)?;
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(ConventionalName::FormXml.canonical()))
        {
            return Err(MetadataIdentityError::MissingDeclaration);
        }
        let snapshot = self.workspace_configs_snapshot();
        let external_kind = match snapshot.owner_kind_of_path(&path) {
            Some(RootKind::External(kind)) => Some(kind.mdo_type()),
            _ => None,
        };
        let module_path = path
            .parent()
            .ok_or(MetadataIdentityError::MissingDeclaration)?
            .join(ConventionalName::Form.canonical())
            .join(ConventionalName::Module.canonical());
        let key = hir::parse_form_module_path(&module_path.to_string_lossy());
        let owner = key.as_ref().and_then(|key| key.owner.as_ref());
        if external_kind.is_none() && key.is_none() {
            return Err(MetadataIdentityError::MissingDeclaration);
        }
        let roots = if let Some(kind) = external_kind.or_else(|| owner.map(|(kind, _)| *kind)) {
            self.metadata_symbol_roots_for_file(form_xml_file, kind)
                .ok_or(MetadataIdentityError::UnprovedExternalOwner)?
        } else {
            if self
                .file_compilation_context(form_xml_file)
                .is_some_and(|context| context.origin == hir::CompilationContextOrigin::Unknown)
            {
                return Err(MetadataIdentityError::NoVisibleSource);
            }
            let visible = self
                .visible_roots_for_file(form_xml_file)
                .ok_or(MetadataIdentityError::NoVisibleSource)?;
            visible
                .main
                .into_iter()
                .chain(visible.chain.into_iter().map(|(_, root)| root))
                .collect()
        };
        let selected = roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .ok_or(MetadataIdentityError::NoVisibleSource)?;
        let relative =
            path.strip_prefix(selected).map_err(|_| MetadataIdentityError::NoVisibleSource)?;
        let form_directory = relative
            .parent()
            .and_then(|ext| ext.parent())
            .ok_or(MetadataIdentityError::MissingDeclaration)?;
        let descriptor = form_directory.with_extension(XML_EXTENSION);
        let descriptor_kind =
            if external_kind.is_some() || owner.is_some() { "Form" } else { "CommonForm" };
        let parent_path = owner.map(|(kind, name)| MetadataSymbolPath {
            owner_kind: *kind,
            owner_name: hir::Name::new(name),
            members: Vec::new(),
        });
        let parent_identity = parent_path
            .as_ref()
            .map(|owner| self.metadata_identity(form_xml_file, owner))
            .transpose()?;
        let relative_descriptor = descriptor
            .to_str()
            .ok_or(MetadataIdentityError::MissingDeclaration)?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let relative_content = relative
            .to_str()
            .ok_or(MetadataIdentityError::MissingDeclaration)?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let mut effective: Option<(InspectionIdentity, Form)> = None;
        for root in &roots {
            let Some(descriptor_file) = self.inspection_file_id(root, &relative_descriptor) else {
                if root == selected {
                    return Err(MetadataIdentityError::MissingDeclaration);
                }
                continue;
            };
            if let (Some(parent), Some(expected)) = (&parent_path, &parent_identity) {
                if self.metadata_identity(descriptor_file, parent)? != *expected {
                    continue;
                }
            }
            let descriptor_text = self
                .inspection_file(root, &relative_descriptor)
                .ok_or(MetadataIdentityError::UnreadSource)?;
            let identity = InspectionIdentity::from_xml(&descriptor_text, descriptor_kind)
                .map_err(|_| MetadataIdentityError::MissingDeclaration)?;
            if identity.uuid.is_nil()
                || !matches!(identity.form_type.as_deref(), None | Some("Managed"))
            {
                return Err(MetadataIdentityError::UnreadSource);
            }
            let is_extension = snapshot
                .paths
                .iter()
                .position(|(_, path)| path == root)
                .is_some_and(|index| snapshot.kinds.get(index) == Some(&RootKind::Extension));
            if is_extension && identity.object_belonging == ObjectBelonging::Unknown {
                return Err(MetadataIdentityError::UnprovedAdoption);
            }
            let content = self
                .inspection_file(root, &relative_content)
                .ok_or(MetadataIdentityError::UnreadSource)?;
            let form = bsl_metadata::xml_parser::parse_form_xml(&content)
                .map_err(|_| MetadataIdentityError::UnreadSource)?;
            if !form.is_managed() {
                return Err(MetadataIdentityError::UnreadSource);
            }
            if identity.object_belonging == ObjectBelonging::Adopted {
                let Some((base_identity, base)) =
                    effective.as_mut().filter(|(base, _)| identity.adopts(base))
                else {
                    return Err(MetadataIdentityError::UnprovedAdoption);
                };
                base.apply_extension_overlay(&form);
                *base_identity = identity;
            } else {
                if form.has_base_form {
                    return Err(MetadataIdentityError::UnprovedAdoption);
                }
                effective = Some((identity, form));
            }
        }
        let (_, form) = effective.ok_or(MetadataIdentityError::MissingDeclaration)?;
        Ok(Arc::new(form.attributes().to_vec()))
    }
}
