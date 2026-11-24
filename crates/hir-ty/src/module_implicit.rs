use bsl_metadata::{MdoType, ModuleType};
use bsl_types::builders::Builders;
use bsl_types::kind::MetadataKind;
use bsl_types::testing::RootConfigCtx;
use hir_def::{ModuleId, Name};
use vfs::FileId;

use crate::db::HirDatabase;
use crate::field_enum::{enumerate_fields_inner, FieldInfo, FieldOrigin};
use crate::form_attr::lower_form_attribute_for_form;

pub fn module_implicit_fields(db: &dyn HirDatabase, file_id: FileId) -> Vec<FieldInfo> {
    let module_id = ModuleId::new(file_id);
    let metadata = db.module_metadata(module_id);
    let obj_resolver = crate::object_resolver::DbObjectResolver::new(db, file_id);

    match metadata.module_type {
        ModuleType::ObjectModule => {
            let Some(mdo) = metadata.mdo.as_ref() else { return Vec::new() };
            let Some(kind) = MetadataKind::object_kind_for(mdo.mdo_type) else {
                return Vec::new();
            };
            let receiver = db.metadata_ref(kind, mdo.name.clone(), &RootConfigCtx);
            enumerate_fields_inner(db, &obj_resolver, receiver)
                .into_iter()
                .filter(|field| crate::this_object_attr::lexical_object_field(mdo, field))
                .collect()
        }
        ModuleType::RecordSetModule => {
            let Some((mdo_type, name)) = module_owner_mdo(&metadata) else {
                return Vec::new();
            };
            let Some(kind) = MetadataKind::record_set_kind_for(mdo_type) else {
                return Vec::new();
            };
            let receiver = db.metadata_ref(kind, name.as_str().to_string(), &RootConfigCtx);
            enumerate_fields_inner(db, &obj_resolver, receiver)
        }
        ModuleType::ValueManagerModule => {
            let Some(mdo) = metadata.mdo.as_ref().filter(|mdo| mdo.mdo_type == MdoType::Constant)
            else {
                return Vec::new();
            };
            let receiver = db.metadata_ref(
                MetadataKind::ConstantValueManager,
                mdo.name.clone(),
                &RootConfigCtx,
            );
            enumerate_fields_inner(db, &obj_resolver, receiver)
        }
        ModuleType::ManagerModule => Vec::new(),
        ModuleType::FormModule => {
            let Some(form) = metadata.form.as_ref() else {
                return Vec::new();
            };
            form.attributes()
                .iter()
                .map(|attr| FieldInfo {
                    name: Name::new(&attr.name),
                    name_en: None,
                    ty: lower_form_attribute_for_form(db, form, attr, &obj_resolver),
                    value_ty: None,
                    is_readonly: false,
                    origin: if attr.is_main {
                        FieldOrigin::MainFormAttribute
                    } else {
                        FieldOrigin::FormAttribute
                    },
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn module_owner_mdo(metadata: &hir_def::ModuleMetadata) -> Option<(MdoType, Name)> {
    match (metadata.mdo.as_ref(), metadata.register.as_ref()) {
        (Some(mdo), _) => Some((mdo.mdo_type, Name::new(&mdo.name))),
        (None, Some(register)) => Some((register.mdo_type(), Name::new(register.name()))),
        (None, None) => None,
    }
}
