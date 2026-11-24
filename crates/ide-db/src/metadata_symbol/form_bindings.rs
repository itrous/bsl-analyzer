use std::sync::Arc;

use bsl_conventions::{ConventionalName, XML_EXTENSION};

use bsl_metadata::FormAttribute;
use hir::metadata_symbol::{MetadataIdentityError, MetadataIdentityResolver, MetadataSymbolPath};
use vfs::FileId;

use crate::{metadata::RootKind, RootDatabaseImpl};

use super::form_composition::{
    compose_form, FormDescriptorKind, FormParent, FormPurpose, FormRequest,
};

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
        let kind = if external_kind.is_some() || owner.is_some() {
            FormDescriptorKind::Form
        } else {
            FormDescriptorKind::CommonForm
        };
        let parent = owner
            .map(|(kind, name)| {
                let name = hir::Name::new(name);
                let path = MetadataSymbolPath {
                    owner_kind: *kind,
                    owner_name: name.clone(),
                    members: Vec::new(),
                };
                let identity = self.metadata_identity(form_xml_file, &path)?;
                Ok(FormParent { kind: *kind, name, identity })
            })
            .transpose()?;
        let relative_path = |path: &std::path::Path| {
            path.to_str()
                .ok_or(MetadataIdentityError::MissingDeclaration)
                .map(|path| path.replace(std::path::MAIN_SEPARATOR, "/"))
        };
        let result = compose_form(
            self,
            FormRequest {
                selected: selected.clone(),
                roots: roots
                    .into_iter()
                    .map(|root| {
                        let label = snapshot
                            .paths
                            .iter()
                            .find(|(_, path)| path == &root)
                            .and_then(|(label, _)| label.clone())
                            .unwrap_or_default();
                        (label, root)
                    })
                    .collect(),
                directory: relative_path(form_directory)?,
                descriptor: relative_path(&descriptor)?,
                managed: relative_path(relative)?,
                kind,
                purpose: FormPurpose::Rename(parent),
            },
        );
        let form = result.rename_form()?;
        Ok(Arc::new(form.attributes().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{MdoEntry, MetadataListingData};
    use base_db::{SourceDatabase, SourceRoot, METADATA_SOURCE_ROOT};
    use bsl_metadata::MdoType;
    use vfs::{file_set::FileSet, VfsPath};

    #[test]
    fn rename_form_checks_the_parent_declaration_in_every_visible_root() {
        let main = std::path::absolute("/form-bindings/base").unwrap();
        let extension = std::path::absolute("/form-bindings/extension").unwrap();
        let mut db = RootDatabaseImpl::new();
        db.set_all_config_paths(
            vec![(None, main.clone()), (Some("Extension".into()), extension.clone())],
            None,
        );
        db.use_prepared_metadata_inputs();
        let descriptor = |kind, name, id, property, target: Option<&str>| {
            let extends = target
                .map(|uuid| {
                    format!("<ExtendedConfigurationObject>{uuid}</ExtendedConfigurationObject>")
                })
                .unwrap_or_default();
            format!(
                "<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'>
                    <{kind} uuid='{id}'><Properties><Name>{name}</Name>
                    {property}{extends}</Properties></{kind}>
                </MetaDataObject>"
            )
        };
        let own = "<ObjectBelonging>Own</ObjectBelonging>";
        let adopted = "<ObjectBelonging>Adopted</ObjectBelonging>";
        let parent_id = "10000000-0000-0000-0000-000000000001";
        let other_parent_id = "10000000-0000-0000-0000-000000000002";
        let base_form_id = "10000000-0000-0000-0000-000000000003";
        let extension_form_id = "10000000-0000-0000-0000-000000000004";
        let base_form = "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'
            xmlns:v='http://v8.1c.ru/8.1/data/core' xmlns:xs='http://www.w3.org/2001/XMLSchema'>
            <Attributes><Attribute name='Base' id='1'><Type><v:Type>xs:string</v:Type></Type>
            </Attribute></Attributes></Form>";
        let extension_form = base_form.replace("'Base' id='1'", "'Extension' id='2'");
        let mut files = FileSet::new();
        for (index, (root, parent, form, content)) in [
            (
                &main,
                descriptor("Catalog", "Orders", parent_id, own, None),
                descriptor("Form", "Dialog", base_form_id, own, None),
                base_form,
            ),
            (
                &extension,
                descriptor("Catalog", "Orders", other_parent_id, adopted, Some(parent_id)),
                descriptor("Form", "Dialog", extension_form_id, adopted, Some(base_form_id)),
                extension_form.as_str(),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let parent_id = FileId::from_raw(100 + index as u32 * 3);
            db.set_metadata_listing(
                &root.to_string_lossy(),
                MetadataListingData {
                    entries: vec![MdoEntry {
                        kind: MdoType::Catalog,
                        name: "Orders".into(),
                        main: parent_id,
                        predefined: None,
                    }],
                    ..MetadataListingData::default()
                },
            );
            for (offset, (path, text)) in [
                ("Catalogs/Orders.xml", parent.as_str()),
                ("Catalogs/Orders/Forms/Dialog.xml", form.as_str()),
                ("Catalogs/Orders/Forms/Dialog/Ext/Form.xml", content),
            ]
            .into_iter()
            .enumerate()
            {
                let id = FileId::from_raw(parent_id.index() + offset as u32);
                files.insert(id, VfsPath::new(root.join(path)));
                db.set_file_source_root(id, METADATA_SOURCE_ROOT);
                db.set_file_text(id, text);
            }
        }
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(files.clone()));
        let form = FileId::from_raw(105);
        let attributes = db.metadata_rename_form_attributes(form).unwrap();
        assert_eq!(
            attributes.iter().map(|attribute| attribute.name.as_str()).collect::<Vec<_>>(),
            ["Base", "Extension"]
        );

        db.set_file_text(
            FileId::from_raw(103),
            &descriptor("Catalog", "Orders", other_parent_id, own, None),
        );
        assert_eq!(
            db.metadata_rename_form_attributes(form),
            Err(MetadataIdentityError::UnprovedAdoption)
        );
        let selected_descriptor = FileId::from_raw(104);
        for ownership in [
            "<ObjectBelonging>Unknown</ObjectBelonging>",
            "<ExtendedConfigurationObject>invalid</ExtendedConfigurationObject>",
        ] {
            db.set_file_text(
                selected_descriptor,
                &descriptor("Form", "Dialog", extension_form_id, ownership, None),
            );
            assert_eq!(
                db.metadata_rename_form_attributes(form),
                Err(MetadataIdentityError::UnprovedAdoption)
            );
        }
        db.set_file_text(
            selected_descriptor,
            &descriptor("Form", "Dialog", extension_form_id, "", None),
        );
        let attributes = db.metadata_rename_form_attributes(form).unwrap();
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].name, "Extension");

        db.set_file_text(form, &extension_form.replace("</Form>", "<BaseForm/></Form>"));
        assert_eq!(
            db.metadata_rename_form_attributes(form),
            Err(MetadataIdentityError::UnprovedAdoption)
        );
        db.set_file_text(form, &extension_form);
        db.set_file_text(
            selected_descriptor,
            &descriptor("Form", "Dialog", extension_form_id, own, None),
        );
        let attributes = db.metadata_rename_form_attributes(form).unwrap();
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].name, "Extension");

        db.set_file_unreadable(form);
        assert_eq!(
            db.metadata_rename_form_attributes(form),
            Err(MetadataIdentityError::UnreadSource)
        );
        db.set_file_text(form, &extension_form);
        db.set_file_text(
            selected_descriptor,
            &descriptor("Form", "Dialog", "00000000-0000-0000-0000-000000000000", own, None),
        );
        assert_eq!(
            db.metadata_rename_form_attributes(form),
            Err(MetadataIdentityError::UnreadSource)
        );

        files.remove(selected_descriptor);
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(files));
        assert_eq!(
            db.metadata_rename_form_attributes(form),
            Err(MetadataIdentityError::MissingDeclaration)
        );
    }
}
