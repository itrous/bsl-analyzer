use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Sql, MetadataTag::Performance],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const DEFAULT_MIN_PATH_DEPTH: i64 = 3;

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::QueryNestedFieldsByDot { range, parts_count } = diag {
        if let Some(n) = parts_count {
            let min = config
                .get_int(DiagnosticCode::QueryNestedFieldsByDot, "minPathDepth")
                .unwrap_or(DEFAULT_MIN_PATH_DEPTH);
            if (*n as i64) < min {
                return;
            }
        }
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::QueryNestedFieldsByDot,
            "Обнаружено разыменование ссылочного поля",
            *range,
            mapper,
            query_text,
            diagnostics,
        );
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(
        ctx,
        DiagnosticCode::QueryNestedFieldsByDot,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::metadata::{DiagnosticSeverityLevel, MetadataTag};
    use crate::test_utils::{
        check_diagnostics_snapshot_for, check_hir_diagnostic_with_config, format_diags,
    };
    use crate::{DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
    use expect_test::{expect, Expect};
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use vfs::{FileId, FileSet, VfsPath};

    /// `<Type>` of an attribute: two or more entries make the type composite.
    fn type_xml(types: &[&str]) -> String {
        types.iter().map(|ty| format!("<v8:Type>{ty}</v8:Type>")).collect()
    }

    fn attributes_xml(tag: &str, attributes: &[(&str, &[&str])]) -> String {
        attributes
            .iter()
            .enumerate()
            .map(|(idx, (name, types))| {
                format!(
                    "<{tag} uuid=\"20000000-0000-0000-0000-{idx:012}\"><Properties><Name>{name}</Name><Type>{}</Type></Properties></{tag}>",
                    type_xml(types)
                )
            })
            .collect()
    }

    fn object_xml(kind: &str, name: &str, children: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.20">
	<{kind} uuid="10000000-0000-0000-0000-000000000001">
		<Properties><Name>{name}</Name></Properties>
		<ChildObjects>{children}</ChildObjects>
	</{kind}>
</MetaDataObject>"#
        )
    }

    const NOMENCLATURE: &str = "cfg:CatalogRef.Номенклатура";
    const WAREHOUSES: &str = "cfg:CatalogRef.Склады";
    const PARTNERS: &str = "cfg:CatalogRef.Контрагенты";

    /// `ЦеныОбъектов.Объект` and `Номенклатура.Поставщик` are composite references,
    /// `Остатки.Номенклатура` and `Номенклатура.Производитель` are single references.
    struct FixtureConfig {
        root: std::path::PathBuf,
    }

    impl FixtureConfig {
        fn materialize() -> Self {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "bsl_query_nested_fields_fixture_{}_{}",
                std::process::id(),
                id
            ));
            let _ = std::fs::remove_dir_all(&root);
            let write = |path: &str, text: String| {
                let path = root.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).expect("create fixture dirs");
                std::fs::write(path, text).expect("write fixture xml");
            };
            write(
                "Configuration.xml",
                object_xml(
                    "Configuration",
                    "ТестРазыменование",
                    "<Catalog>Номенклатура</Catalog><Catalog>Склады</Catalog>\
                     <Catalog>Контрагенты</Catalog>\
                     <InformationRegister>ЦеныОбъектов</InformationRegister>\
                     <AccumulationRegister>Остатки</AccumulationRegister>",
                ),
            );
            write(
                "Catalogs/Номенклатура.xml",
                object_xml(
                    "Catalog",
                    "Номенклатура",
                    &attributes_xml(
                        "Attribute",
                        &[("Поставщик", &[PARTNERS, WAREHOUSES]), ("Производитель", &[PARTNERS])],
                    ),
                ),
            );
            write("Catalogs/Склады.xml", object_xml("Catalog", "Склады", ""));
            write("Catalogs/Контрагенты.xml", object_xml("Catalog", "Контрагенты", ""));
            write(
                "InformationRegisters/ЦеныОбъектов.xml",
                object_xml(
                    "InformationRegister",
                    "ЦеныОбъектов",
                    &attributes_xml(
                        "Dimension",
                        &[
                            ("Объект", &[NOMENCLATURE, WAREHOUSES]),
                            ("Владелец", &[NOMENCLATURE, "xs:string"]),
                            ("Признак", &["xs:string", "xs:decimal"]),
                        ],
                    ),
                ),
            );
            write(
                "AccumulationRegisters/Остатки.xml",
                object_xml(
                    "AccumulationRegister",
                    "Остатки",
                    &attributes_xml("Dimension", &[("Номенклатура", &[NOMENCLATURE])]),
                ),
            );
            Self { root }
        }
    }

    impl Drop for FixtureConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn check_with_fixture(query_body: &str, config: DiagnosticsConfig, expected: Expect) {
        let fixture = FixtureConfig::materialize();
        let workspace_root = fixture.root.clone();

        let code = format!(
            "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{}\";\nКонецПроцедуры",
            query_body
        );

        let mut db = RootDatabaseImpl::new();

        let mut file_set = FileSet::default();
        let file_id = FileId(0);
        let module_path =
            VfsPath::new(format!("{}/Ext/SessionModule.bsl", workspace_root.display()));
        file_set.insert(file_id, module_path);

        let source_root_id = SourceRootId(0);
        db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, source_root_id);
        db.set_file_text(file_id, &code);

        let configuration_path_input = ide_db::metadata::ConfigurationPathInput::new(
            &db,
            workspace_root.to_string_lossy().to_string(),
            0,
        );

        let provider = ide_db::SalsaProvider::new(&db, Some(configuration_path_input));
        let ctx = DiagnosticsContext::new(&config, file_id, &provider);

        expected.assert_eq(&format_diags(&code, &check(&ctx)));
    }

    fn config_with_min_path_depth(depth: i64) -> DiagnosticsConfig {
        let mut config = DiagnosticsConfig::default();
        config.enabled.push(DiagnosticCode::QueryNestedFieldsByDot);
        config.parameters.insert(
            DiagnosticCode::QueryNestedFieldsByDot,
            serde_json::json!({ "minPathDepth": depth }),
        );
        config
    }

    fn check_without_metadata(code: &str, config: DiagnosticsConfig, expected: Expect) {
        let diagnostics: Vec<_> =
            check_hir_diagnostic_with_config(code, config, crate::diagnostics)
                .into_iter()
                .filter(|d| d.code == DiagnosticCode::QueryNestedFieldsByDot)
                .collect();
        expected.assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn only_a_composite_reference_without_cast_is_reported() {
        check_with_fixture(
            "ВЫБРАТЬ Цены.Объект.Код КАК КодОбъекта, \
             ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Код КАК КодНоменклатуры \
             ИЗ РегистрСведений.ЦеныОбъектов КАК Цены; \
             ВЫБРАТЬ Остатки.Номенклатура.Наименование КАК Наименование \
             ИЗ РегистрНакопления.Остатки КАК Остатки",
            DiagnosticsConfig::all_enabled(),
            expect![[r#"
                QueryNestedFieldsByDot @ 3:29..3:44
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn a_composite_of_a_reference_and_a_primitive_is_reported_one_of_primitives_is_not() {
        check_with_fixture(
            "ВЫБРАТЬ Цены.Владелец.Код КАК Код, Цены.Признак.Длина КАК Длина \
             ИЗ РегистрСведений.ЦеныОбъектов КАК Цены",
            DiagnosticsConfig::all_enabled(),
            expect![[r#"
                QueryNestedFieldsByDot @ 3:29..3:46
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn the_same_composite_path_without_metadata_is_silent() {
        check_without_metadata(
            r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Цены.Объект.Код КАК Код ИЗ РегистрСведений.ЦеныОбъектов КАК Цены";
КонецПроцедуры
"#,
            DiagnosticsConfig::all_enabled(),
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_query_nested_fields_by_dot() {
        let code = r#"Процедура СводкаУспеваемости(Начало, Конец)

	Сводка = Новый Запрос;
	Сводка.Текст =
	"ВЫБРАТЬ
	|	Оценки.Ссылка КАК Журнал,
	|	Оценки.Ученик КАК Ученик,
	|	Оценки.Балл * Оценки.Вес КАК ВзвешенныйБалл,
	|	Оценки.Ссылка.Класс КАК Класс,
	|	Оценки.Ссылка.Предмет КАК Предмет,
	|	Оценки.Ссылка.Учитель КАК Учитель
	|ПОМЕСТИТЬ ВТ_Оценки
	|ИЗ
	|	Документ.ЖурналОценок.Оценки КАК Оценки
	|ГДЕ
	|	Оценки.Ссылка.Дата >= &Начало
	|
	|ИНДЕКСИРОВАТЬ ПО
	|	Класс,
	|	Предмет
	|;
	|
	|ВЫБРАТЬ
	|	Посещения.Ученик КАК Ученик,
	|	Посещения.ПропускиОборот КАК Пропуски
	|ПОМЕСТИТЬ ВТ_Посещения
	|ИЗ
	|	РегистрНакопления.Посещаемость.Обороты(
	|			&Начало,
	|			&Конец,
	|			,
	|			(Ученик.Класс, Ученик.Наставник) В
	|				(ВЫБРАТЬ
	|					ВТ_Оценки.Класс КАК Класс,
	|					ВТ_Оценки.Учитель КАК Учитель
	|				ИЗ
	|					ВТ_Оценки КАК ВТ_Оценки)) КАК Посещения
	|;
	|
	|ВЫБРАТЬ
	|	ВТ_Посещения.Ученик КАК Ученик,
	|	МАКСИМУМ(ВТ_Оценки.Балл) КАК ЛучшийБалл,
	|	ЕСТЬNULL(ВТ_Посещения.Пропуски, 0) КАК Пропуски,
	|	ЗНАЧЕНИЕ(Документ.ЖурналОценок.ПустаяСсылка) КАК ПустойЖурнал
	|ИЗ
	|	ВТ_Оценки КАК ВТ_Оценки
	|		ЛЕВОЕ СОЕДИНЕНИЕ ВТ_Посещения КАК ВТ_Посещения
	|		ПО ВТ_Оценки.Ученик = ВТ_Посещения.Ученик
	|			И ВТ_Оценки.Класс.Параллель = ВТ_Посещения.Ученик.Параллель
	|СГРУППИРОВАТЬ ПО
	|	ВТ_Посещения.Ученик,
	|	ВТ_Посещения.Пропуски
	|;
	|
	|ВЫБРАТЬ
	|	ВЫРАЗИТЬ(ВТ_Оценки.Журнал КАК Документ.ЖурналОценок).Четверть КАК Четверть,
	|	ВЫРАЗИТЬ(ВТ_Оценки.Журнал КАК Документ.ЖурналОценок).Четверть.Начало КАК НачалоЧетверти
	|ИЗ
	|	ВТ_Оценки КАК ВТ_Оценки";

	Сводка.УстановитьПараметр("Начало", Начало);
	Сводка.УстановитьПараметр("Конец", Конец);
	Таблица = Сводка.Выполнить().Выгрузить();

	Нормы = Новый Запрос;
	Нормы.Текст =
		"ВЫБРАТЬ
		|	НормыСрез.МинимальныйБалл КАК МинимальныйБалл
		|ИЗ
		|	РегистрСведений.НормыОценок.СрезПоследних(&Конец, Предмет = &Предмет) КАК НормыСрез";
	Нормы.УстановитьПараметр("Конец", Конец);
	Выборка = Нормы.Выполнить().Выбрать();

КонецПроцедуры
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::QueryNestedFieldsByDot,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_no_false_positives_for_mdo_types() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Справочник.Валюты.Код ИЗ Справочник.Валюты";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::QueryNestedFieldsByDot,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_no_false_positives_for_two_parts() {
        let code = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Ссылка ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::QueryNestedFieldsByDot,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_min_path_depth_above_three_drops_three_part_normal_context() {
        let query = "ВЫБРАТЬ Цены.Объект.Код КАК Код ИЗ РегистрСведений.ЦеныОбъектов КАК Цены";
        check_with_fixture(query, config_with_min_path_depth(4), expect![[r#""#]]);
        check_with_fixture(
            query,
            DiagnosticsConfig::all_enabled(),
            expect![[r#"
                QueryNestedFieldsByDot @ 3:29..3:44
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn the_rule_stays_on_by_default_with_its_severity_tags_and_threshold() {
        check_with_fixture(
            "ВЫБРАТЬ Цены.Объект.Код КАК Код ИЗ РегистрСведений.ЦеныОбъектов КАК Цены",
            DiagnosticsConfig::default(),
            expect![[r#"
                QueryNestedFieldsByDot @ 3:29..3:44
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning"#]],
        );
        assert_eq!(super::METADATA.severity, DiagnosticSeverityLevel::Major);
        assert_eq!(
            super::METADATA.tags,
            &[MetadataTag::Standard, MetadataTag::Sql, MetadataTag::Performance]
        );
    }

    #[test]
    fn a_virtual_table_parameter_path_of_a_composite_field_ignores_the_threshold() {
        check_with_fixture(
            "ВЫБРАТЬ Ц.Номенклатура КАК Н \
             ИЗ РегистрСведений.ЦеныОбъектов.СрезПоследних(, Объект.Код = &К) КАК Ц",
            config_with_min_path_depth(99),
            expect![[r#"
                QueryNestedFieldsByDot @ 3:98..3:108
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning"#]],
        );
        check_with_fixture(
            "ВЫБРАТЬ О.Номенклатура КАК Н \
             ИЗ РегистрНакопления.Остатки.Остатки(, Номенклатура.Наименование = &К) КАК О",
            config_with_min_path_depth(99),
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_min_path_depth_above_three_preserves_cast_member_chain() {
        check_with_fixture(
            "ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Поставщик.Код КАК Код \
             ИЗ РегистрСведений.ЦеныОбъектов КАК Цены",
            config_with_min_path_depth(99),
            expect![[r#"
                QueryNestedFieldsByDot @ 3:29..3:92
                  message: Обнаружено разыменование ссылочного поля
                  severity: Warning"#]],
        );
        check_with_fixture(
            "ВЫБРАТЬ ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Производитель.Код КАК Код \
             ИЗ РегистрСведений.ЦеныОбъектов КАК Цены",
            config_with_min_path_depth(99),
            expect![[r#""#]],
        );
    }

    #[test]
    fn inline_tabular_fields_are_not_a_dereference_hop() {
        // `Т.ТЧ.(А, Б)` — выбор полей табличной части. Разыменования ссылки
        // здесь нет, глубина пути равна двум, и при пороге по умолчанию
        // находки быть не должно.
        check_without_metadata(
            r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Товары.(Номенклатура, Количество) ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#,
            config_with_min_path_depth(3),
            expect![[r#""#]],
        );
    }

    #[test]
    fn a_dereference_of_a_single_reference_before_inline_tabular_fields_is_silent() {
        check_without_metadata(
            r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Ссылка.Товары.(Номенклатура) ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#,
            config_with_min_path_depth(3),
            expect![[r#""#]],
        );
    }

    #[test]
    fn a_two_part_path_is_silent_at_min_path_depth_two() {
        check_without_metadata(
            r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ T.Поле ИЗ Документ.Заказ КАК T";
КонецПроцедуры
"#,
            config_with_min_path_depth(2),
            expect![[r#""#]],
        );
        check_with_fixture(
            "ВЫБРАТЬ Цены.Объект КАК Объект ИЗ РегистрСведений.ЦеныОбъектов КАК Цены",
            config_with_min_path_depth(2),
            expect![[r#""#]],
        );
    }
}
