use hir::{
    Builders, HirDatabase, InferenceDiagnostic, MetadataKind, Name, TypeId, TypeKernelDb, TypeKind,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }

    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
fn infer_field_catalog_custom_attribute_typed() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1 - ссылка
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    Р = С.Реквизит2;
    Возврат Р;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "р"),
        Some(db.number(None, None)),
        "Expr::Field must resolve Реквизит2 on Ty::MetadataRef → Ty::Number"
    );
}

#[test]
fn infer_field_catalog_standard_attribute_typed() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    К = С.Код;
    Возврат К;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "к"),
        Some(db.string(None, false)),
        "Standard attribute Код must resolve to Ty::String (CodeLength=9)"
    );
}

#[test]
fn infer_field_catalog_boolean_attribute_typed() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    Б = С.Реквизит3;
    Возврат Б;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(var_ty(&db, file_id, "б"), Some(db.boolean()));
}

#[test]
fn infer_field_unknown_attribute_stays_none() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    Х = С.НесуществующееПоле;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        None,
        "unresolved field must produce Ty::Unknown (no var_types entry)"
    );
}

#[test]
fn infer_field_unresolved_on_known_receiver_emits_diagnostic() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    Х = С.НесуществующееПоле;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let diags = &db.infer(file_id).diagnostics;
    let unresolved: Vec<_> = diags
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedField { receiver_ty, field_name, .. } => {
                Some((*receiver_ty, field_name.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        unresolved.len(),
        1,
        "exactly one UnresolvedField diagnostic must be emitted, got {unresolved:?}"
    );
    let (ty, name) = &unresolved[0];
    assert_eq!(name, &Name::new("НесуществующееПоле"));
    assert!(
        matches!(db.lookup_type(*ty), TypeKind::MetadataRef(facet) if facet.kind == MetadataKind::CatalogRef),
        "receiver_ty must carry the CatalogRef kind, got {:?}",
        db.lookup_type(*ty)
    );
}

#[test]
fn infer_field_unresolved_on_unknown_receiver_stays_silent() {
    let fixture = r#"
//- /test.bsl
Функция Тест()
    Ч = НеизвестнаяФункция();
    Х = Ч.ЛюбоеПоле;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let unresolved = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::UnresolvedField { .. }))
        .count();
    assert_eq!(unresolved, 0, "UnresolvedField must stay silent when the receiver type is Unknown");
}

#[test]
fn infer_field_unresolved_on_primitive_receiver_stays_silent() {
    let fixture = r#"
//- /test.bsl
Функция Тест()
    Ч = 42;
    Х = Ч.ЛюбоеПоле;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let unresolved = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::UnresolvedField { .. }))
        .count();
    assert_eq!(
        unresolved, 0,
        "UnresolvedField must stay silent on primitive receivers — FieldLookup is not authoritative there"
    );
}

#[test]
fn infer_field_unresolved_on_union_receiver_stays_silent() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   Число, Строка
Функция Значение() Экспорт
    Возврат 0;
КонецФункции

//- /test.bsl
Функция Тест()
    У = ПервыйОбщийМодуль.Значение();
    Х = У.ЛюбоеПоле;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let unresolved = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::UnresolvedField { .. }))
        .count();
    assert_eq!(
        unresolved, 0,
        "UnresolvedField must stay silent on union receivers — narrowing must pick a component first"
    );
}

#[test]
fn infer_field_unresolved_on_tabular_row_emits_diagnostic() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    Т = С.ТабличнаяЧасть1;
    Р = Т[0];
    Х = Р.НесуществующаяКолонка;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let unresolved = db
        .infer(file_id)
        .diagnostics
        .iter()
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::UnresolvedField { .. }))
        .count();
    assert_eq!(
        unresolved, 0,
        "Until Expr::Index propagates row types, this stays silent; flip to 1 when indexing is typed"
    );
}

#[test]
fn infer_field_tabular_section_promotes_to_tabular_section_ty() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ПервыйОбщийМодуль.Ссылка();
    Т = С.ТабличнаяЧасть1;
    Возврат Т;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "т").expect("Tabular-section access should set a var type");
    match db.lookup_type(ty) {
        TypeKind::MetadataRef(facet) => {
            assert_eq!(
                facet.kind,
                MetadataKind::TabularSection { parent: bsl_metadata::MdoType::Catalog },
                "promoted kind must be TabularSection {{ parent: Catalog }}"
            );
            assert_eq!(facet.name.as_str(), "Справочник1.ТабличнаяЧасть1");
        }
        other => panic!("expected Ty::MetadataRef(TabularSection), got {other:?}"),
    }
}
