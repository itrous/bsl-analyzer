use std::sync::Arc;

use bsl_metadata::{Form, FormElement};
use bsl_types::builders::Builders;
use bsl_types::facet::{
    FormBindingFacet, FormBindingTargetFacet, FormDataFacet, FormElementFacet, MdoRefFacet,
};
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{TypeId, TypeKind};
use bsl_types::testing::RootConfigCtx;
use hir_def::resolver::Resolver;
use hir_def::ty::MetadataKind;
use hir_def::Name;
use vfs::FileId;

use crate::db::HirDatabase;
use crate::field_enum::{FieldInfo, FieldOrigin};
use crate::field_lookup;
use crate::form_attr::lower_form_attribute_to_typeid;
use crate::object_resolver::MetadataResolution;

pub const FORM_ITEMS_TYPE_RU: &str = "ВсеЭлементыФормы";
pub const FORM_ITEMS_TYPE_EN: &str = "FormAllItems";

pub fn is_form_items_collection_ty(db: &dyn HirDatabase, ty: TypeId) -> bool {
    let TypeKind::PlatformObject(facet) = db.lookup_type(ty) else { return false };
    let name = Name::new(facet.name.as_str());
    name.eq_ignore_case(&Name::new(FORM_ITEMS_TYPE_RU))
        || name.eq_ignore_case(&Name::new(FORM_ITEMS_TYPE_EN))
}

pub(crate) fn lookup_form_item_field(
    db: &dyn HirDatabase,
    resolver: &Resolver,
    receiver: TypeId,
    field: &Name,
) -> Option<FieldInfo> {
    if !is_form_items_collection_ty(db, receiver) {
        return None;
    }
    if !crate::this_object::is_managed_form_module(db, resolver) {
        return None;
    }
    let module_id = resolver.module_id()?;
    let metadata = db.module_metadata(module_id);
    let form = metadata.form.as_ref()?;
    let element = form.find_element(field.as_str())?;
    let obj_resolver = crate::object_resolver::DbObjectResolver::new(db, module_id.file_id);
    Some(FieldInfo {
        name: Name::new(&element.name),
        name_en: None,
        ty: lower_form_element(db, form, element, &obj_resolver),
        value_ty: None,
        is_readonly: true,
        origin: FieldOrigin::PlatformProperty { env: hir_def::execution_env::EnvFlags::ALL },
    })
}

pub(crate) fn lower_form_element(
    db: &dyn TypeKernelDb,
    form: &Form,
    element: &FormElement,
    resolver: &dyn MetadataResolution,
) -> TypeId {
    let binding = element
        .data_path
        .as_deref()
        .filter(|dp| !dp.starts_with('~'))
        .and_then(|dp| resolve_data_path(db, dp, form, resolver));
    db.mk_form_control(element.kind, binding)
}

pub fn lower_form_element_for_file(
    db: &dyn HirDatabase,
    file_id: FileId,
    form: &Form,
    element: &FormElement,
) -> TypeId {
    let obj_resolver = crate::object_resolver::DbObjectResolver::new(db, file_id);
    lower_form_element(db, form, element, &obj_resolver)
}

fn row_typeid_of_tabular_section_target(
    db: &dyn TypeKernelDb,
    target: &FormBindingTargetFacet,
) -> Option<TypeId> {
    match target {
        FormBindingTargetFacet::TabularSection { mdo_ref, section } => {
            let qualified = format!("{}.{}", mdo_ref.name.as_str(), section.as_str());
            Some(db.metadata_ref(
                MetadataKind::TabularSectionRow { parent: mdo_ref.mdo_type },
                qualified,
                &RootConfigCtx,
            ))
        }
        FormBindingTargetFacet::Attribute { .. } => None,
        _ => None,
    }
}

pub(crate) fn refine_form_control_property(
    db: &dyn TypeKernelDb,
    receiver: TypeId,
    field: &Name,
) -> Option<FieldInfo> {
    let target = match db.lookup_type(receiver) {
        TypeKind::FormControl { kind: FormElementFacet::Table, binding: Some(binding) } => {
            binding.target.clone()
        }
        _ => return None,
    };
    let row = row_typeid_of_tabular_section_target(db, &target)?;

    let selected_rows_ru = Name::new("ВыделенныеСтроки");
    let selected_rows_en = Name::new("SelectedRows");
    let current_row_ru = Name::new("ТекущаяСтрока");
    let current_row_en = Name::new("CurrentRow");
    let current_data_ru = Name::new("ТекущиеДанные");
    let current_data_en = Name::new("CurrentData");

    let (canonical_ru, canonical_en, ty, is_readonly) =
        if field.eq_ignore_case(&selected_rows_ru) || field.eq_ignore_case(&selected_rows_en) {
            (selected_rows_ru, selected_rows_en, db.array(Some(row)), true)
        } else if field.eq_ignore_case(&current_row_ru) || field.eq_ignore_case(&current_row_en) {
            (current_row_ru, current_row_en, row, false)
        } else if field.eq_ignore_case(&current_data_ru) || field.eq_ignore_case(&current_data_en) {
            (current_data_ru, current_data_en, row, true)
        } else {
            return None;
        };

    Some(FieldInfo {
        name: canonical_ru,
        name_en: Some(canonical_en),
        ty,
        value_ty: None,
        is_readonly,
        origin: FieldOrigin::PlatformProperty { env: hir_def::execution_env::EnvFlags::ALL },
    })
}

fn resolve_data_path(
    db: &dyn TypeKernelDb,
    data_path: &str,
    form: &Form,
    resolver: &dyn MetadataResolution,
) -> Option<FormBindingFacet> {
    let segments: Vec<Name> =
        data_path.split('.').filter(|s| !s.is_empty()).map(Name::new).collect();
    let (head, rest) = segments.split_first()?;

    let attr = form.find_attribute(head.as_str())?;
    let mut current_id = lower_form_attribute_to_typeid(db, attr, resolver);

    for seg in rest {
        let info = field_lookup::lookup_field(db, resolver, current_id, seg)?;
        current_id = info.ty;
    }

    let target = match db.lookup_type(current_id) {
        TypeKind::MetadataRef(facet) => match &facet.kind {
            MetadataKind::TabularSection { parent } => {
                let (owner, section) = facet.name.as_str().rsplit_once('.')?;
                FormBindingTargetFacet::TabularSection {
                    mdo_ref: MdoRefFacet::new(*parent, owner.to_string()),
                    section: section.to_string(),
                }
            }
            _ => FormBindingTargetFacet::Attribute { ty: current_id },
        },
        TypeKind::FormData { kind: FormDataFacet::Collection, underlying: Some(mdo_ref) } => {
            let (owner, section) = mdo_ref.name.as_str().rsplit_once('.')?;
            FormBindingTargetFacet::TabularSection {
                mdo_ref: MdoRefFacet::new(mdo_ref.mdo_type, owner.to_string()),
                section: section.to_string(),
            }
        }
        _ => FormBindingTargetFacet::Attribute { ty: current_id },
    };

    let path: Arc<[String]> = segments.iter().map(|n| n.as_str().to_string()).collect();
    Some(FormBindingFacet::new(path, target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_resolver::ConfigsObjectResolver;
    use bsl_config::VisibleConfig;
    use bsl_types::facet::TableSource;
    use bsl_types::testing::InMemoryDb;

    fn lower_form_element(
        db: &InMemoryDb,
        form: &Form,
        element: &FormElement,
        configs: &[VisibleConfig],
    ) -> TypeId {
        super::lower_form_element(db, form, element, &ConfigsObjectResolver(configs))
    }

    fn refine_form_control_property(
        db: &InMemoryDb,
        receiver: TypeId,
        field: &Name,
    ) -> Option<FieldInfo> {
        super::refine_form_control_property(db, receiver, field)
    }

    use bsl_metadata::tabular_section::{TabularSection, TabularSectionAttribute};
    use bsl_metadata::{
        AttributeType, Configuration, Form, FormAttribute, FormElement, FormElementKind, FormType,
        MdoType, MetadataObject,
    };
    use uuid::Uuid;

    fn empty_form(name: &str) -> Form {
        Form::new(name.to_string(), FormType::Managed, Uuid::nil())
    }

    fn wrap_config(config: Configuration) -> Vec<VisibleConfig> {
        vec![VisibleConfig { name: None, configuration: Arc::new(config) }]
    }

    fn document_with_section(
        doc_name: &str,
        attrs: Vec<(&str, AttributeType)>,
        section_name: &str,
        section_attrs: Vec<(&str, AttributeType)>,
    ) -> MetadataObject {
        let mut doc = MetadataObject::new(MdoType::Document, doc_name);
        for (name, ty) in attrs {
            doc.add_attribute(bsl_metadata::Attribute {
                name: name.to_string(),
                name_en: None,
                attr_type: ty,
            });
        }
        let mut ts = TabularSection::new(Uuid::new_v4(), section_name);
        let cols: Vec<TabularSectionAttribute> = section_attrs
            .into_iter()
            .map(|(n, t)| TabularSectionAttribute::new(Uuid::new_v4(), n.to_string(), t))
            .collect();
        ts.set_attributes(cols);
        doc.add_tabular_section(ts);
        doc
    }

    #[test]
    fn lower_form_element_button_with_no_data_path_has_no_binding() {
        let form = empty_form("Ф");
        let element = FormElement::with_kind("Кнопка1", 1, None, FormElementKind::Button, None);
        let db = InMemoryDb::new();
        let ty = lower_form_element(&db, &form, &element, &[]);
        match db.lookup_type(ty) {
            TypeKind::FormControl { kind, binding } => {
                assert_eq!(*kind, FormElementKind::Button);
                assert!(binding.is_none(), "no DataPath ⇒ binding=None");
            }
            other => panic!("expected FormControl, got {other:?}"),
        }
    }

    #[test]
    fn lower_form_element_with_wrong_data_path_has_no_binding() {
        let form = empty_form("Ф");
        let element = FormElement::with_kind(
            "СломаннаяТаблица",
            1,
            Some("~Объект.Удалена".to_string()),
            FormElementKind::Table,
            None,
        );
        let db = InMemoryDb::new();
        let ty = lower_form_element(&db, &form, &element, &[]);
        match db.lookup_type(ty) {
            TypeKind::FormControl { kind: FormElementKind::Table, binding } => {
                assert!(binding.is_none(), "~-prefixed DataPath ⇒ binding=None");
            }
            other => panic!("expected FormControl{{Table,None}}, got {other:?}"),
        }
    }

    #[test]
    fn lower_form_element_unknown_first_segment_yields_no_binding() {
        let form = empty_form("Ф");
        let element = FormElement::with_kind(
            "ЗабытоеПоле",
            1,
            Some("ОтсутствующийРеквизит.X".to_string()),
            FormElementKind::Field,
            None,
        );
        let db = InMemoryDb::new();
        let ty = lower_form_element(&db, &form, &element, &[]);
        match db.lookup_type(ty) {
            TypeKind::FormControl { binding, .. } => assert!(binding.is_none()),
            other => panic!("expected FormControl, got {other:?}"),
        }
    }

    #[test]
    fn lower_form_element_with_scalar_attribute_yields_attribute_target() {
        let mut form = empty_form("Ф");
        form.attributes
            .push(FormAttribute::new("Замечание", AttributeType::String { length: Some(100) }));
        let element = FormElement::with_kind(
            "ПолеЗамечание",
            1,
            Some("Замечание".to_string()),
            FormElementKind::Field,
            None,
        );
        let db = InMemoryDb::new();
        let id = super::lower_form_element(&db, &form, &element, &ConfigsObjectResolver(&[]));
        match db.lookup_type(id) {
            TypeKind::FormControl { kind: FormElementFacet::Field, binding: Some(b) } => {
                assert_eq!(b.path.len(), 1);
                assert_eq!(b.path[0].as_str(), "Замечание");
                match &b.target {
                    FormBindingTargetFacet::Attribute { ty } => {
                        assert_eq!(*ty, db.string(None, false))
                    }
                    other => panic!("expected Attribute{{String}}, got {other:?}"),
                }
            }
            other => panic!("expected FormControl{{Field,Some(Attribute)}}, got {other:?}"),
        }
    }

    #[test]
    fn lower_form_element_resolves_object_attribute_data_path_type() {
        let mut form = empty_form("ФормаКонтрагента");
        form.attributes.push(FormAttribute {
            name: "Объект".to_string(),
            attr_type: AttributeType::Ref {
                mdo_type: MdoType::Catalog,
                name: "Контрагенты".to_string(),
            },
            is_main: true,
            columns: vec![],
            additional_columns: vec![],
        });
        let element = FormElement::with_kind(
            "ИНН",
            1,
            Some("Объект.ИНН".to_string()),
            FormElementKind::Field,
            None,
        );
        let mut config = Configuration::new("Test");
        let mut catalog = MetadataObject::new(MdoType::Catalog, "Контрагенты");
        catalog.add_attribute(bsl_metadata::Attribute {
            name: "ИНН".to_string(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(12) },
        });
        config.add_metadata_object(catalog);
        let configs = wrap_config(config);
        let db = InMemoryDb::new();

        let id = lower_form_element(&db, &form, &element, &configs);

        match db.lookup_type(id) {
            TypeKind::FormControl { binding: Some(binding), .. } => {
                assert_eq!(binding.path.as_ref(), ["Объект", "ИНН"]);
                assert_eq!(
                    binding.target,
                    FormBindingTargetFacet::Attribute { ty: db.string(None, false) }
                );
            }
            other => panic!("expected typed form-control binding, got {other:?}"),
        }
    }

    #[test]
    fn lower_form_element_with_tabular_section_path_yields_tabular_section_target() {
        let mut form = empty_form("ФормаПКО");
        form.attributes.push(FormAttribute {
            name: "Объект".to_string(),
            attr_type: AttributeType::Ref {
                mdo_type: MdoType::Document, name: "ПКО".to_string()
            },
            is_main: true,
            columns: vec![],
            additional_columns: vec![],
        });

        let mut config = Configuration::new("Test");
        config.add_metadata_object(document_with_section(
            "ПКО",
            vec![],
            "Переприемка",
            vec![("ШтрихКод", AttributeType::String { length: Some(13) })],
        ));
        let configs = wrap_config(config);

        let element = FormElement::with_kind(
            "Переприемка",
            255,
            Some("Объект.Переприемка".to_string()),
            FormElementKind::Table,
            None,
        );
        let db = InMemoryDb::new();
        let id = super::lower_form_element(&db, &form, &element, &ConfigsObjectResolver(&configs));
        match db.lookup_type(id) {
            TypeKind::FormControl { kind: FormElementFacet::Table, binding: Some(b) } => {
                assert_eq!(b.path.len(), 2);
                assert_eq!(b.path[0].as_str(), "Объект");
                assert_eq!(b.path[1].as_str(), "Переприемка");
                match &b.target {
                    FormBindingTargetFacet::TabularSection { mdo_ref, section } => {
                        assert_eq!(mdo_ref.mdo_type, MdoType::Document);
                        assert_eq!(mdo_ref.name.as_str(), "ПКО");
                        assert_eq!(section.as_str(), "Переприемка");
                    }
                    other => panic!("expected TabularSection target, got {other:?}"),
                }
            }
            other => panic!("expected FormControl{{Table,Some(TabularSection)}}, got {other:?}"),
        }
    }

    #[test]
    fn lower_form_element_carries_kind_for_other_buckets() {
        let form = empty_form("Ф");
        for k in [
            FormElementKind::Group,
            FormElementKind::UsualGroup,
            FormElementKind::Pages,
            FormElementKind::Page,
            FormElementKind::CommandBar,
            FormElementKind::ButtonGroup,
            FormElementKind::Decoration,
            FormElementKind::Addition,
            FormElementKind::Other,
        ] {
            let element = FormElement::with_kind("X", 1, None, k, None);
            let db = InMemoryDb::new();
            match db.lookup_type(lower_form_element(&db, &form, &element, &[])) {
                TypeKind::FormControl { kind, binding: None } => assert_eq!(*kind, k),
                other => panic!("expected FormControl{{kind={k:?},None}}, got {other:?}"),
            }
        }
    }

    #[test]
    fn cyrillic_case_insensitive_receiver_match() {
        let canonical = Name::new(FORM_ITEMS_TYPE_RU);
        let mixed = Name::new("вСеЭлементыФормы");
        let lower = Name::new("всеэлементыформы");
        let upper = Name::new("ВСЕЭЛЕМЕНТЫФОРМЫ");
        assert!(mixed.eq_ignore_case(&canonical));
        assert!(lower.eq_ignore_case(&canonical));
        assert!(upper.eq_ignore_case(&canonical));
        assert!(!"вСеЭлементыФормы".eq_ignore_ascii_case(FORM_ITEMS_TYPE_RU));

        let canonical_en = Name::new(FORM_ITEMS_TYPE_EN);
        let mixed_en = Name::new("formALLitems");
        assert!(mixed_en.eq_ignore_case(&canonical_en));
    }

    fn binding_to(mdo: MdoType, owner: &str, section: &str) -> FormBindingFacet {
        FormBindingFacet::new(
            Arc::from([owner.to_string(), section.to_string()]),
            FormBindingTargetFacet::TabularSection {
                mdo_ref: MdoRefFacet::new(mdo, owner.to_string()),
                section: section.to_string(),
            },
        )
    }

    #[test]
    fn refine_selected_rows_returns_typed_array_of_row() {
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(
            FormElementKind::Table,
            Some(binding_to(MdoType::Document, "ПКО", "Переприемка")),
        );
        let info = refine_form_control_property(&db, receiver, &Name::new("ВыделенныеСтроки"))
            .expect("refined property");
        match db.lookup_type(info.ty) {
            TypeKind::Array(facet) => match facet.element {
                Some(elem) => match db.lookup_type(elem) {
                    TypeKind::MetadataRef(facet) => {
                        assert_eq!(
                            facet.kind,
                            MetadataKind::TabularSectionRow { parent: MdoType::Document }
                        );
                        assert_eq!(facet.name.as_str(), "ПКО.Переприемка");
                    }
                    other => panic!("expected row MetadataRef inside TypedArray, got {other:?}"),
                },
                None => panic!("expected typed Array(row), got untyped Array"),
            },
            other => panic!("expected TypedArray(row), got {other:?}"),
        }
        assert!(info.is_readonly);
        assert_eq!(info.name.as_str(), "ВыделенныеСтроки");
        assert_eq!(info.name_en.as_ref().unwrap().as_str(), "SelectedRows");
    }

    #[test]
    fn refine_current_row_returns_row_ty() {
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(
            FormElementKind::Table,
            Some(binding_to(MdoType::Catalog, "Номенклатура", "ЕдиницыИзмерения")),
        );
        let info = refine_form_control_property(&db, receiver, &Name::new("ТекущаяСтрока"))
            .expect("refined ТекущаяСтрока");
        match db.lookup_type(info.ty) {
            TypeKind::MetadataRef(facet) => {
                assert_eq!(
                    facet.kind,
                    MetadataKind::TabularSectionRow { parent: MdoType::Catalog }
                );
                assert_eq!(facet.name.as_str(), "Номенклатура.ЕдиницыИзмерения");
            }
            other => panic!("expected row Ty, got {other:?}"),
        }
        assert_eq!(info.name_en.as_ref().unwrap().as_str(), "CurrentRow");
    }

    #[test]
    fn refine_current_data_returns_row_ty() {
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(
            FormElementKind::Table,
            Some(binding_to(MdoType::Document, "ПКО", "Переприемка")),
        );
        let info = refine_form_control_property(&db, receiver, &Name::new("ТекущиеДанные"))
            .expect("refined ТекущиеДанные");
        assert!(matches!(
            db.lookup_type(info.ty),
            TypeKind::MetadataRef(facet)
                if matches!(facet.kind, MetadataKind::TabularSectionRow { .. })
        ));
        assert_eq!(info.name_en.as_ref().unwrap().as_str(), "CurrentData");
    }

    #[test]
    fn refine_recognises_english_aliases() {
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(
            FormElementKind::Table,
            Some(binding_to(MdoType::Document, "ПКО", "Переприемка")),
        );
        for english in ["SelectedRows", "CurrentRow", "CurrentData"] {
            assert!(
                refine_form_control_property(&db, receiver, &Name::new(english)).is_some(),
                "{english} must resolve via English alias"
            );
        }
    }

    #[test]
    fn refine_is_case_insensitive_cyrillic() {
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(
            FormElementKind::Table,
            Some(binding_to(MdoType::Document, "ПКО", "Переприемка")),
        );
        for spelling in ["ВЫДЕЛЕННЫЕСТРОКИ", "выделенныестроки", "вЫдЕлЕнНыЕсТрОкИ"]
        {
            assert!(
                refine_form_control_property(&db, receiver, &Name::new(spelling)).is_some(),
                "spelling {spelling:?} must resolve"
            );
        }
    }

    #[test]
    fn refine_returns_none_for_non_refined_field() {
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(
            FormElementKind::Table,
            Some(binding_to(MdoType::Document, "ПКО", "Переприемка")),
        );
        assert!(refine_form_control_property(&db, receiver, &Name::new("Видимость")).is_none());
        assert!(refine_form_control_property(&db, receiver, &Name::new("Заголовок")).is_none());
        assert!(refine_form_control_property(&db, receiver, &Name::new("ШтрихКод")).is_none());
    }

    #[test]
    fn refine_returns_none_when_kind_is_not_table() {
        for kind in [
            FormElementKind::Field,
            FormElementKind::Button,
            FormElementKind::Group,
            FormElementKind::UsualGroup,
            FormElementKind::Pages,
            FormElementKind::Page,
            FormElementKind::CommandBar,
            FormElementKind::ButtonGroup,
            FormElementKind::Decoration,
            FormElementKind::Addition,
            FormElementKind::Other,
        ] {
            let db = InMemoryDb::new();
            let receiver =
                db.mk_form_control(kind, Some(binding_to(MdoType::Document, "ПКО", "Переприемка")));
            assert!(
                refine_form_control_property(&db, receiver, &Name::new("ВыделенныеСтроки"))
                    .is_none(),
                "kind {kind:?} must not refine .ВыделенныеСтроки"
            );
        }
    }

    #[test]
    fn refine_returns_none_when_binding_absent() {
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(FormElementKind::Table, None);
        assert!(
            refine_form_control_property(&db, receiver, &Name::new("ВыделенныеСтроки")).is_none()
        );
    }

    #[test]
    fn refine_is_readonly_matches_platform_per_property() {
        let db = InMemoryDb::new();
        let receiver = db.mk_form_control(
            FormElementKind::Table,
            Some(binding_to(MdoType::Document, "ПКО", "Переприемка")),
        );
        let selected =
            refine_form_control_property(&db, receiver, &Name::new("ВыделенныеСтроки")).unwrap();
        assert!(selected.is_readonly, "ВыделенныеСтроки is platform-readonly");
        let selected_en =
            refine_form_control_property(&db, receiver, &Name::new("SelectedRows")).unwrap();
        assert!(selected_en.is_readonly);

        let current_data =
            refine_form_control_property(&db, receiver, &Name::new("ТекущиеДанные")).unwrap();
        assert!(current_data.is_readonly, "ТекущиеДанные is platform-readonly");
        let current_data_en =
            refine_form_control_property(&db, receiver, &Name::new("CurrentData")).unwrap();
        assert!(current_data_en.is_readonly);

        let current_row =
            refine_form_control_property(&db, receiver, &Name::new("ТекущаяСтрока")).unwrap();
        assert!(!current_row.is_readonly, "ТекущаяСтрока is writable per platform_data");
        let current_row_en =
            refine_form_control_property(&db, receiver, &Name::new("CurrentRow")).unwrap();
        assert!(!current_row_en.is_readonly);
    }

    #[test]
    fn refine_returns_none_when_target_is_attribute() {
        let db = InMemoryDb::new();
        let attr_ty = db.value_table(None, TableSource::Unknown);
        let attr_binding = FormBindingFacet::new(
            Arc::from(["ТабличнаяЧасть".to_string()]),
            FormBindingTargetFacet::Attribute { ty: attr_ty },
        );
        let receiver = db.mk_form_control(FormElementFacet::Table, Some(attr_binding));
        assert!(super::refine_form_control_property(&db, receiver, &Name::new("ВыделенныеСтроки"))
            .is_none());
    }

    #[test]
    fn lower_form_element_main_attribute_object_path_segment_resolves() {
        let mut form = empty_form("Ф");
        form.attributes.push(FormAttribute {
            name: "Объект".to_string(),
            attr_type: AttributeType::Ref {
                mdo_type: MdoType::Document, name: "ПКО".to_string()
            },
            is_main: true,
            columns: vec![],
            additional_columns: vec![],
        });
        let element = FormElement::with_kind(
            "ПолеОбъекта",
            1,
            Some("Объект".to_string()),
            FormElementKind::Field,
            None,
        );
        let db = InMemoryDb::new();
        let id = super::lower_form_element(&db, &form, &element, &ConfigsObjectResolver(&[]));
        match db.lookup_type(id) {
            TypeKind::FormControl { kind: FormElementFacet::Field, binding: Some(b) } => {
                assert_eq!(b.path.len(), 1);
                match &b.target {
                    FormBindingTargetFacet::Attribute { ty: inner } => {
                        match db.lookup_type(*inner) {
                            TypeKind::FormData {
                                kind: FormDataFacet::Structure,
                                underlying: Some(mdo_ref),
                            } => {
                                assert_eq!(mdo_ref.mdo_type, MdoType::Document);
                                assert_eq!(mdo_ref.name.as_str(), "ПКО");
                            }
                            other => panic!("expected FormData{{Structure}}, got {other:?}"),
                        }
                    }
                    other => panic!("expected Attribute target, got {other:?}"),
                }
            }
            other => panic!("expected FormControl{{Field,Some(Attribute)}}, got {other:?}"),
        }
    }
}
