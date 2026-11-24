use hir::{Builders, DefDatabase, HirDatabase, InferenceResult, TypeId, TypeKernelDb, TypeKind};
use ide::{Analysis, Diagnostic, DiagnosticCode, DiagnosticsConfig};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn setup(source: &str) -> (RootDatabaseImpl, FileId) {
    setup_at(source, "/test.bsl")
}

fn setup_at(source: &str, path: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(&format!("//- {path}\n{source}"));
    let mut db = RootDatabaseImpl::new();
    let mut files = FileSet::default();
    for (id, file) in &fixture.files {
        files.insert(*id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    for (id, file) in &fixture.files {
        db.set_file_source_root(*id, SourceRootId(0));
        db.set_file_text(*id, &file.content);
    }
    db.set_all_config_paths(
        vec![(
            None,
            std::path::PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../bsl-metadata/fixtures/designer"
            )),
        )],
        None,
    );
    (db, fixture.first_file().unwrap())
}

fn reports(db: &RootDatabaseImpl, file: FileId, code: DiagnosticCode) -> Vec<Diagnostic> {
    Analysis::from_database(db.clone())
        .diagnostics(file, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == code)
        .collect()
}

fn local(inferred: &InferenceResult, name: &str) -> TypeId {
    *inferred
        .var_types
        .get(name)
        .unwrap_or_else(|| panic!("missing {name}: {:?}", inferred.var_types))
}

#[test]
fn configuration_and_reference_metadata_are_accepted_by_access_right() {
    let source = r#"&НаСервере
Процедура Проверить()
    Конфигурация = Метаданные;
    Описание = Справочники.Справочник1.ПустаяСсылка().Метаданные();
    Администрирование = ПравоДоступа("Администрирование", Конфигурация);
    Вывод = ПравоДоступа("Вывод", Метаданные);
    МобильныйКлиент = ПравоДоступа("МобильныйКлиент", Метаданные);
    Чтение = ПравоДоступа("Чтение", Описание);
    English = AccessRight("Output", Metadata);
КонецПроцедуры"#;
    let (db, file) = setup(source);
    let inferred = db.infer(file);
    assert_ne!(local(&inferred, "конфигурация"), db.unknown());
    assert_ne!(local(&inferred, "описание"), db.unknown());
    for name in ["администрирование", "вывод", "мобильныйклиент", "чтение", "english"]
    {
        assert!(matches!(db.lookup_type(local(&inferred, name)), TypeKind::Boolean));
    }
    assert!(reports(&db, file, DiagnosticCode::TypeMismatch).is_empty());
}

#[test]
fn access_right_keeps_non_metadata_arguments_incompatible() {
    let source = "Процедура Проверить()\nПравоДоступа(\"Вывод\", 42);\nAccessRight(\"Output\", \"Metadata\");\nКонецПроцедуры";
    let (db, file) = setup(source);
    let diagnostics = reports(&db, file, DiagnosticCode::TypeMismatch);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    let values: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| {
            &source[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
        })
        .collect();
    assert_eq!(values, ["42", "\"Metadata\""]);
}

#[test]
fn predefined_item_name_accepts_the_reported_chart_manager_and_its_reference() {
    let source = r#"&НаСервере
Процедура Проверить()
    СсылкаВида = ПланыВидовХарактеристик.ПланВидовХарактеристик1.ПустаяСсылка();
    ИмяВида = ПланыВидовХарактеристик.ПланВидовХарактеристик1.ПолучитьИмяПредопределенного(СсылкаВида);
    English = ChartsOfCharacteristicTypes.ПланВидовХарактеристик1.GetPredefinedItemName(СсылкаВида);
    ИмяКаталога = Справочники.Справочник1.ПолучитьИмяПредопределенного(Справочники.Справочник1.ПустаяСсылка());
    ИмяСчета = ПланыСчетов.ПланСчетов1.ПолучитьИмяПредопределенного(ПланыСчетов.ПланСчетов1.ПустаяСсылка());
    ИмяРасчета = ПланыВидовРасчета.ПланВидовРасчета1.ПолучитьИмяПредопределенного(ПланыВидовРасчета.ПланВидовРасчета1.ПустаяСсылка());
КонецПроцедуры"#;
    let (db, file) = setup(source);
    let inferred = db.infer(file);
    assert_ne!(local(&inferred, "ссылкавида"), db.unknown());
    for name in ["имявида", "english", "имякаталога", "имясчета", "имярасчета"]
    {
        assert!(matches!(db.lookup_type(local(&inferred, name)), TypeKind::String(_)));
    }
    for code in [
        DiagnosticCode::UnresolvedMethodCall,
        DiagnosticCode::TypeMismatch,
        DiagnosticCode::MismatchedArgCount,
        DiagnosticCode::UnavailableInEnvironment,
    ] {
        assert!(reports(&db, file, code).is_empty(), "{code:?}");
    }
}

#[test]
fn predefined_item_name_keeps_wrong_arguments_and_unsupported_receivers() {
    let source = r#"Процедура Проверить()
    ПланыВидовХарактеристик.ПланВидовХарактеристик1.ПолучитьИмяПредопределенного(42);
    ПланыВидовХарактеристик.ПланВидовХарактеристик1.ПолучитьИмяПредопределенного(Справочники.Справочник1.ПустаяСсылка());
    ПланыВидовХарактеристик.ПланВидовХарактеристик1.GetPredefinedItemName();
    Документы.Документ1.ПолучитьИмяПредопределенного(Документы.Документ1.ПустаяСсылка());
КонецПроцедуры"#;
    let (db, file) = setup(source);
    assert_eq!(reports(&db, file, DiagnosticCode::TypeMismatch).len(), 2);
    assert_eq!(reports(&db, file, DiagnosticCode::MismatchedArgCount).len(), 1);
    let unresolved = reports(&db, file, DiagnosticCode::UnresolvedMethodCall);
    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    assert!(unresolved[0].message.contains("Документ1"));
}

#[test]
fn predefined_item_name_keeps_its_manager_execution_environments() {
    let source = "&НаКлиенте\nПроцедура Проверить(Ссылка)\nПланыВидовХарактеристик.ПланВидовХарактеристик1.ПолучитьИмяПредопределенного(Ссылка);\nКонецПроцедуры";
    let module = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/designer/CommonModules/КлиентскийОбщийМодуль/Ext/Module.bsl"
    ));
    let (db, file) = setup_at(source, module.to_str().unwrap());
    assert!(db.module_metadata(hir::ModuleId::new(file)).common_module.is_some());
    let unavailable = reports(&db, file, DiagnosticCode::UnavailableInEnvironment);
    assert!(
        unavailable
            .iter()
            .any(|diagnostic| diagnostic.message.contains("ПолучитьИмяПредопределенного")),
        "{unavailable:?}"
    );
}

#[test]
fn dom_node_type_missing_value_has_a_precise_range_and_other_owners_stay_separate() {
    let source = r#"Процедура Проверить()
    Неверный = ТипУзлаDOM.УзелЭлемента;
    Верный = ТипУзлаDOM.Элемент;
    English = DOMNodeType.Element;
    Другой = Новый Структура("УзелЭлемента", 1);
    ЗначениеДругого = Другой.УзелЭлемента;
КонецПроцедуры
Процедура ДругойКонтекст(ТипУзлаDOM)
    Неизвестное = ТипУзлаDOM.УзелЭлемента;
КонецПроцедуры"#;
    let (db, file) = setup(source);
    let diagnostics = reports(&db, file, DiagnosticCode::UnresolvedField);
    let platform = bsl_platform::PlatformData::instance();
    assert_eq!(platform.get_type_properties("DOMNodeType").len(), 13);
    assert!(platform.get_property("DOMNodeType", "Element").is_some());
    assert!(platform.get_property("ТипУзлаDOM", "Элемент").is_some());
    #[cfg(corpus_contract)]
    {
        assert!(platform.help_origin().unwrap().platform_version.is_none());
        assert!(!platform.platform_type_member_surface_verified(
            "DOMNodeType",
            bsl_platform::MemberSurface::Fields
        ));
        assert!(
            diagnostics.is_empty(),
            "an unknown corpus version cannot prove a miss: {diagnostics:?}"
        );
    }
    #[cfg(not(corpus_contract))]
    {
        assert!(platform.platform_type_member_surface_verified(
            "DOMNodeType",
            bsl_platform::MemberSurface::Fields
        ));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            &source[usize::from(diagnostics[0].range.start())
                ..usize::from(diagnostics[0].range.end())],
            "ТипУзлаDOM.УзелЭлемента"
        );
        assert!(usize::from(diagnostics[0].range.start()) < source.find("КонецПроцедуры").unwrap());
    }
}
