//! `РеквизитФормыВЗначение(Имя[, Тип])` in a managed form module: the platform
//! documents the result as `Произвольный`, but the call fixes it — through the
//! declared type of the named attribute, or through a literal `Тип` argument.
//! The object that comes back must be the same value `Обработки.X.Создать()`
//! gives, so navigation and member checks through it behave identically.

use hir::{
    Builders, HirDatabase, InferenceDiagnostic, MetadataKind, TypeId, TypeKernelDb, TypeKind,
};
use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

const DATA_PROCESSOR_FORM: &str =
    "DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl";
const DATA_PROCESSOR_OBJECT: &str = "DataProcessors/ТестоваяОбработка/Ext/ObjectModule.bsl";
const REPORT_FORM: &str = "Reports/ТестовыйОтчёт/Forms/Форма/Ext/Form/Module.bsl";
const CATALOG_FORM: &str = "Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
const DOCUMENT_FORM: &str = "Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl";
const RECORD_FORM: &str =
    "InformationRegisters/РегистрСведений1/Forms/ФормаЗаписи/Ext/Form/Module.bsl";
const COMMON_MODULE: &str = "CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl";

const OBJECT_MODULE_TEXT: &str = "Процедура ЗаполнитьНастройки() Экспорт\nКонецПроцедуры\n\n\
                                  Процедура Внутренняя()\nКонецПроцедуры\n";

/// Ids follow `files`, starting at 1.
fn build_db(files: &[(&str, &str)]) -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (index, (relative, _)) in files.iter().enumerate() {
        let path = designer_path().join(relative).to_string_lossy().to_string();
        file_set.insert(FileId::from_raw(index as u32 + 1), VfsPath::new(path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (index, (_, text)) in files.iter().enumerate() {
        let id = FileId::from_raw(index as u32 + 1);
        db.set_file_source_root(id, SourceRootId(0));
        db.set_file_text(id, text);
    }
    db.set_all_config_paths(vec![(None, designer_path())]);
    db
}

fn form_body(statements: &str) -> String {
    format!("&НаСервере\nПроцедура Тест(Имя, ТипЗначения)\n{statements}КонецПроцедуры\n")
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> TypeId {
    *db.infer(file_id)
        .var_types
        .get(var_lower)
        .unwrap_or_else(|| panic!("{var_lower} must be inferred"))
}

/// An untyped local is not recorded at all, so a miss reads as `Unknown`.
fn infer_in(module: &str, statements: &str, var_lower: &str) -> (RootDatabaseImpl, TypeId) {
    let db = build_db(&[(module, &form_body(statements))]);
    let recorded = db.infer(FileId::from_raw(1)).var_types.get(var_lower).copied();
    let ty = recorded.unwrap_or_else(|| db.unknown());
    (db, ty)
}

fn assert_metadata_ref(db: &RootDatabaseImpl, ty: TypeId, kind: MetadataKind, name: &str) {
    match db.lookup_type(ty) {
        TypeKind::MetadataRef(facet) => {
            assert_eq!(facet.kind, kind);
            assert_eq!(facet.name.as_str(), name);
        }
        other => panic!("expected MetadataRef({kind:?}, {name}), got {other:?}"),
    }
}

fn assert_unknown(db: &RootDatabaseImpl, ty: TypeId) {
    assert!(
        matches!(db.lookup_type(ty), TypeKind::Unknown),
        "expected Unknown, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn the_main_attribute_of_a_data_processor_form_is_its_object() {
    let (db, ty) = infer_in(
        DATA_PROCESSOR_FORM,
        "    Обработка = РеквизитФормыВЗначение(\"Объект\");\n",
        "обработка",
    );
    assert_metadata_ref(&db, ty, MetadataKind::DataProcessorObject, "ТестоваяОбработка");
}

#[test]
fn the_value_is_the_one_the_manager_creates() {
    let statements = "    Обработка = РеквизитФормыВЗначение(\"Объект\");\n    \
                      Созданная = Обработки.ТестоваяОбработка.Создать();\n";
    let db = build_db(&[(DATA_PROCESSOR_FORM, &form_body(statements))]);
    let file_id = FileId::from_raw(1);
    assert_eq!(var_ty(&db, file_id, "обработка"), var_ty(&db, file_id, "созданная"));
}

#[test]
fn the_english_spelling_and_the_self_receiver_agree() {
    let statements = "    Первая = FormAttributeToValue(\"Объект\");\n    \
                      Вторая = ЭтаФорма.РеквизитФормыВЗначение(\"Объект\");\n";
    let db = build_db(&[(DATA_PROCESSOR_FORM, &form_body(statements))]);
    let file_id = FileId::from_raw(1);
    let first = var_ty(&db, file_id, "первая");
    assert_metadata_ref(&db, first, MetadataKind::DataProcessorObject, "ТестоваяОбработка");
    assert_eq!(first, var_ty(&db, file_id, "вторая"));
}

#[test]
fn report_catalog_and_document_forms_follow_their_main_attribute() {
    let call = "    Значение = РеквизитФормыВЗначение(\"Объект\");\n";
    let (db, ty) = infer_in(REPORT_FORM, call, "значение");
    assert_metadata_ref(&db, ty, MetadataKind::ReportObject, "ТестовыйОтчёт");
    let (db, ty) = infer_in(CATALOG_FORM, call, "значение");
    assert_metadata_ref(&db, ty, MetadataKind::CatalogObject, "Справочник1");
    let (db, ty) = infer_in(DOCUMENT_FORM, call, "значение");
    assert_metadata_ref(&db, ty, MetadataKind::DocumentObject, "Документ1");
}

/// A record-manager attribute is typed as form data over its register, but the value
/// `РеквизитФормыВЗначение` turns it into is not modelled yet, so it stays unknown.
#[test]
fn a_record_manager_attribute_stays_unknown() {
    let (db, ty) =
        infer_in(RECORD_FORM, "    Значение = РеквизитФормыВЗначение(\"Запись\");\n", "значение");
    assert_unknown(&db, ty);
}

#[test]
fn a_value_tree_attribute_converts_to_a_value_tree() {
    let statements = "    Дерево = РеквизитФормыВЗначение(\"ДеревоРазделов\");\n    \
                      Образец = Новый ДеревоЗначений;\n";
    let db = build_db(&[(DATA_PROCESSOR_FORM, &form_body(statements))]);
    let file_id = FileId::from_raw(1);
    assert_eq!(var_ty(&db, file_id, "дерево"), var_ty(&db, file_id, "образец"));
}

#[test]
fn a_literal_type_argument_wins_over_the_attribute() {
    let (db, ty) = infer_in(
        DATA_PROCESSOR_FORM,
        "    Значение = РеквизитФормыВЗначение(\"Объект\", Тип(\"СправочникОбъект.Справочник1\"));\n",
        "значение",
    );
    assert_metadata_ref(&db, ty, MetadataKind::CatalogObject, "Справочник1");
}

#[test]
fn what_the_call_does_not_pin_down_stays_unknown() {
    for statements in [
        "    Значение = РеквизитФормыВЗначение(Имя);\n",
        "    Значение = РеквизитФормыВЗначение(\"НетТакогоРеквизита\");\n",
        "    Значение = РеквизитФормыВЗначение(\"СписокЗначенийРеквизит\");\n",
        "    Значение = РеквизитФормыВЗначение(\"Объект\", ТипЗначения);\n",
    ] {
        let (db, ty) = infer_in(DATA_PROCESSOR_FORM, statements, "значение");
        assert_unknown(&db, ty);
    }
}

#[test]
fn outside_a_form_module_nothing_changes() {
    let (db, ty) =
        infer_in(COMMON_MODULE, "    Значение = РеквизитФормыВЗначение(\"Объект\");\n", "значение");
    assert_unknown(&db, ty);
}

#[test]
fn a_module_method_of_the_same_name_owns_the_call() {
    let text = "&НаСервере\nФункция РеквизитФормыВЗначение(Имя)\n    Возврат 1;\nКонецФункции\n\n\
                &НаСервере\nПроцедура Тест()\n    Значение = РеквизитФормыВЗначение(\"Объект\");\n\
                КонецПроцедуры\n";
    let db = build_db(&[(DATA_PROCESSOR_FORM, text)]);
    let ty = var_ty(&db, FileId::from_raw(1), "значение");
    assert!(
        !matches!(db.lookup_type(ty), TypeKind::MetadataRef(_)),
        "the module's own function decides the result, got {:?}",
        db.lookup_type(ty)
    );
}

const CALLER: &str = "&НаСервере\nПроцедура ЗаполнитьНаСервере()\n    \
                      Обработка = РеквизитФормыВЗначение(\"Объект\");\n    \
                      Обработка.ЗаполнитьНастройки();\n    \
                      Обработка.Внутренняя();\n    \
                      Обработка.НетТакогоМетода();\n    \
                      Созданная = Обработки.ТестоваяОбработка.Создать();\n    \
                      Созданная.Внутренняя();\n    \
                      Созданная.НетТакогоМетода();\n\
                      КонецПроцедуры\n";

#[test]
fn goto_definition_reaches_the_object_module_export() {
    let analysis = Analysis::from_database(build_db(&[
        (DATA_PROCESSOR_FORM, CALLER),
        (DATA_PROCESSOR_OBJECT, OBJECT_MODULE_TEXT),
    ]));
    let offset = CALLER.find("Обработка.ЗаполнитьНастройки").unwrap() + "Обработка.".len() + 2;
    let target = analysis
        .goto_definition(FileId::from_raw(1), offset as u32)
        .expect("the export method must resolve through the converted object");
    assert_eq!(target.file_id, FileId::from_raw(2));
    assert_eq!(target.name, "ЗаполнитьНастройки");
}

/// A non-export and a missing method reached through the converted object are
/// reported exactly as through the object the manager creates — once for each
/// route, nothing else.
#[test]
fn member_checks_match_the_manager_created_object() {
    let db =
        build_db(&[(DATA_PROCESSOR_FORM, CALLER), (DATA_PROCESSOR_OBJECT, OBJECT_MODULE_TEXT)]);
    let unresolved: Vec<String> = db
        .infer(FileId::from_raw(1))
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedMethodCall { method_name, kind, .. } => {
                Some(format!("{}:{kind:?}", method_name.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        unresolved,
        vec![
            "Внутренняя:MethodNotExport".to_string(),
            "НетТакогоМетода:MethodNotFound".to_string(),
            "Внутренняя:MethodNotExport".to_string(),
            "НетТакогоМетода:MethodNotFound".to_string(),
        ]
    );
}
