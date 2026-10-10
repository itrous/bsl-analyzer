use hir::{Builders, HirDatabase, MetadataKind, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn register_recordset_module_path() -> PathBuf {
    designer_fixture_path().join("InformationRegisters/РегистрСведений1/Ext/RecordSetModule.bsl")
}

fn temp_register_recordset_module_path(config_path: &std::path::Path) -> PathBuf {
    config_path.join("InformationRegisters/РегистрСведенийE2E/Ext/RecordSetModule.bsl")
}

fn temp_accumulation_recordset_module_path(config_path: &std::path::Path) -> PathBuf {
    config_path.join("AccumulationRegisters/РегистрНакопленияE2E/Ext/RecordSetModule.bsl")
}

fn temp_calculation_recordset_module_path(config_path: &std::path::Path) -> PathBuf {
    config_path.join("CalculationRegisters/РегистрРасчетаE2E/Ext/RecordSetModule.bsl")
}

fn catalog_object_module_path() -> PathBuf {
    designer_fixture_path().join("Catalogs/Справочник1/Ext/ObjectModule.bsl")
}

fn common_module_path() -> PathBuf {
    designer_fixture_path().join("CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl")
}

fn setup_at(path: PathBuf, text: &str) -> (RootDatabaseImpl, FileId) {
    setup_at_with_config(path, designer_fixture_path(), text)
}

fn setup_at_with_config(
    path: PathBuf,
    config_path: PathBuf,
    text: &str,
) -> (RootDatabaseImpl, FileId) {
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    file_set.insert(file_id, VfsPath::new(path.to_string_lossy().to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    db.set_all_config_paths(vec![(None, config_path)]);
    (db, file_id)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn is_metadata_ref(db: &RootDatabaseImpl, ty: TypeId, kind: MetadataKind, name: &str) -> bool {
    matches!(
        db.lookup_type(ty),
        TypeKind::MetadataRef(facet) if facet.kind == kind && facet.name.as_str() == name
    )
}

fn assert_metadata_ref(
    db: &RootDatabaseImpl,
    actual: Option<TypeId>,
    kind: MetadataKind,
    name: &str,
) {
    let actual = actual.expect("expected metadata ref type");
    assert!(
        is_metadata_ref(db, actual, kind, name),
        "expected MetadataRef({kind:?}, {name}), got {:?}",
        db.lookup_type(actual)
    );
}

fn temp_designer_config_with_register_recorders() -> PathBuf {
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir()
        .join(format!("bsl-analyzer-register-recorders-{}-{suffix}", std::process::id()));
    fs::create_dir_all(root.join("InformationRegisters")).unwrap();
    fs::create_dir_all(root.join("AccumulationRegisters")).unwrap();
    fs::create_dir_all(root.join("CalculationRegisters")).unwrap();
    fs::create_dir_all(root.join("Documents")).unwrap();
    fs::create_dir_all(root.join("CommonModules")).unwrap();

    fs::write(
        root.join("InformationRegisters/РегистрСведенийE2E.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <InformationRegister uuid="11111111-1111-1111-1111-111111111111">
        <Properties>
            <Name>РегистрСведенийE2E</Name>
            <InformationRegisterPeriodicity>Second</InformationRegisterPeriodicity>
        </Properties>
    </InformationRegister>
</MetaDataObject>"#,
    )
    .unwrap();
    fs::write(
        root.join("AccumulationRegisters/РегистрНакопленияE2E.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <AccumulationRegister uuid="22222222-2222-2222-2222-222222222222">
        <Properties>
            <Name>РегистрНакопленияE2E</Name>
        </Properties>
    </AccumulationRegister>
</MetaDataObject>"#,
    )
    .unwrap();
    fs::write(
        root.join("CalculationRegisters/РегистрРасчетаE2E.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CalculationRegister uuid="44444444-4444-4444-4444-444444444444">
        <Properties>
            <Name>РегистрРасчетаE2E</Name>
            <Periodicity>Month</Periodicity>
            <ActionPeriod>false</ActionPeriod>
        </Properties>
    </CalculationRegister>
</MetaDataObject>"#,
    )
    .unwrap();
    for (file, doc_name) in
        [("ДокументE2E1.xml", "ДокументE2E1"), ("ДокументE2E2.xml", "ДокументE2E2")]
    {
        fs::write(
            root.join("Documents").join(file),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" version="2.10">
    <Document uuid="33333333-3333-3333-3333-333333333333">
        <Properties>
            <Name>{doc_name}</Name>
            <RegisterRecords>
                <xr:Item xsi:type="xr:MDObjectRef">InformationRegister.РегистрСведенийE2E</xr:Item>
                <xr:Item xsi:type="xr:MDObjectRef">AccumulationRegister.РегистрНакопленияE2E</xr:Item>
                <xr:Item xsi:type="xr:MDObjectRef">CalculationRegister.РегистрРасчетаE2E</xr:Item>
            </RegisterRecords>
        </Properties>
    </Document>
</MetaDataObject>"#
            ),
        )
        .unwrap();
    }

    root
}

#[test]
fn infer_this_record_set_resolves_in_information_register_module() {
    let text = r#"
Функция Тест()
    Набор = ЭтотОбъект;
    Возврат Набор;
КонецФункции
"#;
    let (db, file_id) = setup_at(register_recordset_module_path(), text);
    assert_metadata_ref(
        &db,
        var_ty(&db, file_id, "набор"),
        MetadataKind::InformationRegisterRecordSet,
        "РегистрСведений1",
    );
}

#[test]
fn infer_this_record_set_english_spelling() {
    let text = "Функция Тест()\n    T = ThisObject;\n    Return T;\nКонецФункции";
    let (db, file_id) = setup_at(register_recordset_module_path(), text);
    assert_metadata_ref(
        &db,
        var_ty(&db, file_id, "t"),
        MetadataKind::InformationRegisterRecordSet,
        "РегистрСведений1",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn for_each_yields_information_register_record_kind() {
    let text = r#"
Процедура Тест()
    Для Каждого З Из ЭтотОбъект Цикл
        Х = З;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup_at(register_recordset_module_path(), text);
    assert_metadata_ref(
        &db,
        var_ty(&db, file_id, "з"),
        MetadataKind::InformationRegisterRecord,
        "РегистрСведений1",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn record_dimension_resolves() {
    let text = r#"
Процедура Тест()
    Для Каждого З Из ЭтотОбъект Цикл
        Р = З.Справочник1;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup_at(register_recordset_module_path(), text);
    assert_metadata_ref(&db, var_ty(&db, file_id, "р"), MetadataKind::CatalogRef, "Справочник1");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn record_standard_period_resolves() {
    let config_path = temp_designer_config_with_register_recorders();
    let text = r#"
Процедура Тест()
    Для Каждого З Из ЭтотОбъект Цикл
        П = З.Период;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) =
        setup_at_with_config(temp_register_recordset_module_path(&config_path), config_path, text);
    let actual = var_ty(&db, file_id, "п").expect("П must be inferred");
    assert!(
        matches!(db.lookup_type(actual), TypeKind::Date(_)),
        "Период must resolve to Date, got {:?}",
        db.lookup_type(actual)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn record_standard_active_resolves() {
    let text = r#"
Процедура Тест()
    Для Каждого З Из ЭтотОбъект Цикл
        А = З.Активность;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup_at(register_recordset_module_path(), text);
    assert_eq!(var_ty(&db, file_id, "а"), Some(db.boolean()));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn record_recorder_resolves_to_union_of_recorders() {
    let config_path = temp_designer_config_with_register_recorders();
    let text = r#"
Процедура Тест()
    Для Каждого Запись Из ЭтотОбъект Цикл
        Р = Запись.Регистратор;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) =
        setup_at_with_config(temp_register_recordset_module_path(&config_path), config_path, text);
    let actual = var_ty(&db, file_id, "р").expect("Регистратор must be inferred");
    assert!(
        matches!(db.lookup_type(actual), TypeKind::Union(members)
            if members.iter().any(|member| {
                is_metadata_ref(&db, *member, MetadataKind::DocumentRef, "ДокументE2E1")
            }) && members.iter().any(|member| {
                is_metadata_ref(&db, *member, MetadataKind::DocumentRef, "ДокументE2E2")
            })
        ),
        "Регистратор must resolve to union of recorder refs, got {:?}",
        db.lookup_type(actual),
    );
}

#[test]
fn filter_recorder_resolves_to_filter_item() {
    let config_path = temp_designer_config_with_register_recorders();
    let text = r#"
Процедура Тест()
    Э = ЭтотОбъект.Отбор.Регистратор;
КонецПроцедуры
"#;
    let (db, file_id) = setup_at_with_config(
        temp_accumulation_recordset_module_path(&config_path),
        config_path,
        text,
    );
    assert_eq!(var_ty(&db, file_id, "э"), Some(db.platform_object("ЭлементОтбора".to_string())),);
}

#[test]
fn filter_calculation_register_recorder_resolves() {
    let config_path = temp_designer_config_with_register_recorders();
    let text = r#"
Процедура Тест()
    Э = ЭтотОбъект.Отбор.Регистратор;
КонецПроцедуры
"#;
    let (db, file_id) = setup_at_with_config(
        temp_calculation_recordset_module_path(&config_path),
        config_path,
        text,
    );
    assert_eq!(var_ty(&db, file_id, "э"), Some(db.platform_object("ЭлементОтбора".to_string())),);
}

#[test]
fn filter_period_resolves_for_periodic_inforeg() {
    let config_path = temp_designer_config_with_register_recorders();
    let text = r#"
Процедура Тест()
    Э = ЭтотОбъект.Отбор.Период;
КонецПроцедуры
"#;
    let (db, file_id) =
        setup_at_with_config(temp_register_recordset_module_path(&config_path), config_path, text);
    assert_eq!(var_ty(&db, file_id, "э"), Some(db.platform_object("ЭлементОтбора".to_string())),);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn additional_properties_implicit_bare_resolves() {
    let text = r#"
Функция Тест()
    С = ДополнительныеСвойства;
    Возврат С;
КонецФункции
"#;
    let (db, file_id) = setup_at(register_recordset_module_path(), text);
    assert_eq!(var_ty(&db, file_id, "с"), Some(db.structure(None)));
}

#[test]
fn object_module_does_not_produce_record_set_metadata_ref() {
    let text = r#"
Функция Тест()
    Х = ЭтотОбъект;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup_at(catalog_object_module_path(), text);
    assert!(
        !matches!(
            var_ty(&db, file_id, "х"),
            Some(ty) if matches!(
                db.lookup_type(ty),
                TypeKind::MetadataRef(facet)
                    if matches!(
                        facet.kind,
                        MetadataKind::InformationRegisterRecordSet
                    | MetadataKind::AccumulationRegisterRecordSet
                    | MetadataKind::AccountingRegisterRecordSet
                    | MetadataKind::CalculationRegisterRecordSet
                    )
            )
        ),
        "Catalog ObjectModule must not infer any *RecordSet kind",
    );
}

#[test]
fn common_module_does_not_produce_record_set_metadata_ref() {
    let text = r#"
Функция Тест() Экспорт
    Набор = ЭтотОбъект;
    Возврат Набор;
КонецФункции
"#;
    let (db, file_id) = setup_at(common_module_path(), text);

    let infer = db.infer(file_id);
    let has_record_set = infer.var_types.values().any(|tid| {
        matches!(
            db.lookup_type(*tid),
            TypeKind::MetadataRef(facet)
                if matches!(
                    facet.kind,
                    MetadataKind::InformationRegisterRecordSet
                    | MetadataKind::AccumulationRegisterRecordSet
                    | MetadataKind::AccountingRegisterRecordSet
                    | MetadataKind::CalculationRegisterRecordSet
                )
        )
    });
    assert!(!has_record_set);
}
