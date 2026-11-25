use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious, MetadataTag::Sql],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub(crate) fn dispatch(
    config: &DiagnosticsConfig,
    diag: &sdbl_hir::SdblDiagnostic,
    mapper: &crate::sdbl_utils::SdblPositionMapper,
    query_text: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let sdbl_hir::SdblDiagnostic::UnknownField { table_name, field_name, range } = diag {
        let code = DiagnosticCode::UnknownFieldInQuery;
        diagnostics.push(Diagnostic {
            code,
            message: format!(
                "Поле \"{}\" не найдено в таблице \"{}\" запроса",
                field_name, table_name
            ),
            severity: config.severity(code),
            range: mapper.map_range(*range, query_text),
            tags: config.tags(code),
            fixes: vec![],
        });
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    crate::sdbl_utils::collect_sdbl_via_dispatch(ctx, DiagnosticCode::UnknownFieldInQuery, dispatch)
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::test_utils::format_diags;
    use crate::{DiagnosticsConfig, DiagnosticsContext};
    use expect_test::{expect, Expect};
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use std::path::PathBuf;
    use vfs::{FileId, FileSet, VfsPath};

    /// Runs the diagnostic against the shared designer config fixture, which
    /// supplies a fully-modeled catalog (`Справочник1` with attributes and a
    /// tabular section) and an information register (`РегистрСведений1`).
    fn check_with_designer(query_body: &str, expected: Expect) {
        let fixtures_dir =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");

        let code = format!(
            "Процедура Тест()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{}\";\nКонецПроцедуры",
            query_body
        );

        let mut db = RootDatabaseImpl::new();
        let workspace_root = PathBuf::from(fixtures_dir);

        let mut file_set = FileSet::default();
        let file_id = FileId(0);
        let module_path = VfsPath::new(format!("{}/Ext/SessionModule.bsl", fixtures_dir));
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
        let config = DiagnosticsConfig::all_enabled();
        let ctx = DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = check(&ctx);
        expected.assert_eq(&format_diags(&code, &diagnostics));
    }

    fn adopted_document_query(query_body: &str) -> Vec<crate::Diagnostic> {
        use crate::test_utils::check_cfe_at_with_unreadable;
        use test_fixture::CfeFixtureBuilder;

        const BASE_UUID: &str = "11111111-1111-1111-1111-111111111111";
        const EXT_UUID: &str = "22222222-2222-2222-2222-222222222222";
        let base_xml = format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
<Document uuid="{BASE_UUID}"><Properties><Name>Документ1</Name></Properties><ChildObjects>
<TabularSection uuid="33333333-3333-3333-3333-333333333333"><Properties><Name>Товары</Name></Properties><ChildObjects>
<Attribute uuid="44444444-4444-4444-4444-444444444444"><Properties><Name>БазовоеПоле</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>
</ChildObjects></TabularSection></ChildObjects></Document></MetaDataObject>"#
        );
        let extension_xml = format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
<Document uuid="{EXT_UUID}"><Properties><Name>Документ1</Name><ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>{BASE_UUID}</ExtendedConfigurationObject></Properties><ChildObjects>
<TabularSection uuid="55555555-5555-5555-5555-555555555555"><Properties><Name>Товары</Name></Properties><ChildObjects>
<Attribute uuid="66666666-6666-6666-6666-666666666666"><Properties><Name>ДобавленноеПоле</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>
</ChildObjects></TabularSection></ChildObjects></Document></MetaDataObject>"#
        );
        let mut builder = CfeFixtureBuilder::new("");
        builder.add_extension("Расширение", "");
        let fixture = builder.build();
        let base_documents = fixture.root().join("Documents");
        let extension_documents = fixture.extensions()[0].root().join("Documents");
        std::fs::create_dir_all(&base_documents).expect("create base Documents");
        std::fs::create_dir_all(&extension_documents).expect("create extension Documents");
        std::fs::write(base_documents.join("Документ1.xml"), base_xml)
            .expect("write base document metadata");
        std::fs::write(extension_documents.join("Документ1.xml"), extension_xml)
            .expect("write extension document metadata");

        let source = format!(
            "Процедура Проверка()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{query_body}\";\nКонецПроцедуры"
        );
        check_cfe_at_with_unreadable(
            "Documents/Документ1/Ext/ManagerModule.bsl",
            &source,
            fixture,
            &[],
            check,
        )
    }

    #[test]
    fn positive_unknown_field_on_register() {
        check_with_designer(
            "ВЫБРАТЬ Т.НетТакогоПоля ИЗ РегистрСведений.РегистрСведений1 КАК Т",
            expect![[r#"
                UnknownFieldInQuery @ 3:31..3:44
                  message: Поле "НетТакогоПоля" не найдено в таблице "РегистрСведений.РегистрСведений1" запроса
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn positive_missing_field_behind_a_reference() {
        check_with_designer(
            "ВЫБРАТЬ Т.Справочник1.НетТакогоПоля ИЗ РегистрСведений.РегистрСведений1 КАК Т",
            expect![[r#"
                UnknownFieldInQuery @ 3:43..3:56
                  message: Поле "НетТакогоПоля" не найдено в таблице "Справочник.Справочник1" запроса
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn negative_declared_standard_and_virtual_fields_behind_a_reference() {
        check_with_designer(
            "ВЫБРАТЬ Т.Справочник1.Реквизит1, Т.Справочник1.Код, Т.Справочник1.Наименование, Т.Справочник1.Родитель, Т.Справочник1.Представление, Т.Справочник1.ТабличнаяЧасть1.Количество ИЗ РегистрСведений.РегистрСведений1 КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn positive_missing_column_of_a_tabular_section_behind_a_reference() {
        check_with_designer(
            "ВЫБРАТЬ Т.Справочник1.ТабличнаяЧасть1.НетКолонки ИЗ РегистрСведений.РегистрСведений1 КАК Т",
            expect![[r#"
                UnknownFieldInQuery @ 3:59..3:69
                  message: Поле "НетКолонки" не найдено в таблице "Справочник.Справочник1.ТабличнаяЧасть1" запроса
                  severity: Blocker"#]],
        );
    }
    #[test]
    fn negative_valid_dimension() {
        check_with_designer(
            "ВЫБРАТЬ Т.Справочник1 ИЗ РегистрСведений.РегистрСведений1 КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn adopted_document_query_keeps_base_and_added_tabular_fields() {
        let diagnostics = adopted_document_query(
            "ВЫБРАТЬ Т.БазовоеПоле, Т.ДобавленноеПоле ИЗ Документ.Документ1.Товары КАК Т",
        );
        assert!(diagnostics.is_empty(), "base and adopted columns should resolve: {diagnostics:?}");
    }

    #[test]
    fn adopted_document_query_still_rejects_a_missing_tabular_field() {
        let diagnostics = adopted_document_query(
            "ВЫБРАТЬ Т.БазовоеПоле, Т.ДобавленноеПоле, Т.НетТакогоПоля ИЗ Документ.Документ1.Товары КАК Т",
        );
        assert_eq!(
            diagnostics.len(),
            1,
            "only the missing column should be diagnosed: {diagnostics:?}"
        );
        assert!(diagnostics[0].message.contains("НетТакогоПоля"));
    }

    #[test]
    fn positive_recorder_fields_on_independent_information_register() {
        // This register explicitly declares Independent; recorder-only fields are absent.
        check_with_designer(
            "ВЫБРАТЬ Т.Период, Т.Регистратор, Т.Активность, Т.НомерСтроки, Т.МоментВремени ИЗ РегистрСведений.РегистрСведений1 КАК Т",
            expect![[r#"
                UnknownFieldInQuery @ 3:41..3:52
                  message: Поле "Регистратор" не найдено в таблице "РегистрСведений.РегистрСведений1" запроса
                  severity: Blocker
                UnknownFieldInQuery @ 3:56..3:66
                  message: Поле "Активность" не найдено в таблице "РегистрСведений.РегистрСведений1" запроса
                  severity: Blocker
                UnknownFieldInQuery @ 3:70..3:81
                  message: Поле "НомерСтроки" не найдено в таблице "РегистрСведений.РегистрСведений1" запроса
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn positive_movement_type_not_on_information_register() {
        // `ВидДвижения` exists only on accumulation registers; on an information
        // register it is a genuine unknown field.
        check_with_designer(
            "ВЫБРАТЬ Т.ВидДвижения ИЗ РегистрСведений.РегистрСведений1 КАК Т",
            expect![[r#"
                UnknownFieldInQuery @ 3:31..3:42
                  message: Поле "ВидДвижения" не найдено в таблице "РегистрСведений.РегистрСведений1" запроса
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn negative_valid_catalog_attribute() {
        check_with_designer(
            "ВЫБРАТЬ Т.Реквизит1 ИЗ Справочник.Справочник1 КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_virtual_presentation_and_data_version() {
        check_with_designer(
            "ВЫБРАТЬ Т.Представление, Т.ВерсияДанных ИЗ Справочник.Справочник1 КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_tabular_section_name() {
        check_with_designer(
            "ВЫБРАТЬ Т.ТабличнаяЧасть1 ИЗ Справочник.Справочник1 КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_select_star() {
        check_with_designer("ВЫБРАТЬ * ИЗ Справочник.Справочник1 КАК Т", expect![[r#""#]]);
    }

    #[test]
    fn negative_extension_object_gate_off() {
        // Resolves as a missing object (QueryToMissingMetadata path), so the
        // field model is incomplete and the unknown-field gate stays off.
        check_with_designer(
            "ВЫБРАТЬ Т.НетТакогоПоля ИЗ Справочник.НесуществующийСправочник КАК Т",
            expect![[r#""#]],
        );
    }

    /// A slice of an information register lists every field the platform gives it, so the
    /// gate is on there too.
    #[test]
    fn positive_unknown_field_on_register_slice() {
        check_with_designer(
            "ВЫБРАТЬ Т.НетТакогоПоля ИЗ РегистрСведений.РегистрСведений1.СрезПоследних КАК Т",
            expect![[r#"
                UnknownFieldInQuery @ 3:31..3:44
                  message: Поле "НетТакогоПоля" не найдено в таблице "РегистрСведений.РегистрСведений1.СрезПоследних" запроса
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn negative_valid_dimension_on_register_slice() {
        check_with_designer(
            "ВЫБРАТЬ Т.Справочник1, Т.Период ИЗ РегистрСведений.РегистрСведений1.СрезПоследних КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_temp_table_gate_off() {
        check_with_designer("ВЫБРАТЬ Т.НетТакогоПоля ИЗ ВременнаяТаблица КАК Т", expect![[r#""#]]);
    }

    #[test]
    fn negative_document_point_in_time() {
        // МоментВремени is a query-only virtual field of document tables.
        check_with_designer(
            "ВЫБРАТЬ Т.МоментВремени ИЗ Документ.Документ1 КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_chart_of_accounts_standard_fields() {
        // Родитель is unconditional (no Hierarchical property in the XML);
        // Вид/Забалансовый/accounting flags/ВидыСубконто are account-row fields.
        check_with_designer(
            "ВЫБРАТЬ Т.Родитель, Т.Порядок, Т.Вид, Т.Забалансовый, Т.Валютный, Т.ВидыСубконто ИЗ ПланСчетов.ПланСчетов1 КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_chart_of_accounts_ext_dimension_table() {
        check_with_designer(
            "ВЫБРАТЬ Т.НомерСтроки, Т.ВидСубконто, Т.Предопределенное, Т.ТолькоОбороты, Т.Суммовой ИЗ ПланСчетов.ПланСчетов1.ВидыСубконто КАК Т",
            expect![[r#""#]],
        );
    }

    #[test]
    fn positive_unknown_field_on_chart_of_accounts() {
        // The chart-of-accounts model is exhaustive, so the gate stays on.
        check_with_designer(
            "ВЫБРАТЬ Т.НетТакогоПоля ИЗ ПланСчетов.ПланСчетов1 КАК Т",
            expect![[r#"
                UnknownFieldInQuery @ 3:31..3:44
                  message: Поле "НетТакогоПоля" не найдено в таблице "ПланСчетов.ПланСчетов1" запроса
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn negative_comment_line_at_the_end_of_the_query() {
        // Комментарий внутри текста запроса — законная конструкция: им
        // отключают условия и помечают правки в типовых.
        check_with_designer(
            "ВЫБРАТЬ\n    |\tТ.Реквизит1 КАК Реквизит1\n    |ИЗ\n    |\tСправочник.Справочник1 КАК Т\n    |ГДЕ\n    |\tНЕ Т.ПометкаУдаления\n    |// комментарий последней строкой",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_comment_line_in_the_middle_of_the_query() {
        check_with_designer(
            "ВЫБРАТЬ\n    |\tТ.Реквизит1 КАК Реквизит1\n    |ИЗ\n    |\tСправочник.Справочник1 КАК Т\n    |ГДЕ\n    |\tНЕ Т.ПометкаУдаления\n    |// комментарий в середине\n    |\tИ Т.Реквизит1 <> \"\"\"\"",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_trailing_comment_with_a_dot_after_a_field() {
        // Точка в комментарии не разделяет части составного имени: иначе
        // двухчастная ссылка выглядела бы трёхчастной.
        check_with_designer(
            "ВЫБРАТЬ\n    |\tТ.Реквизит1 КАК Реквизит1\n    |ИЗ\n    |\tСправочник.Справочник1 КАК Т\n    |ГДЕ\n    |\tНЕ Т.ПометкаУдаления // см. задачу 15",
            expect![[r#""#]],
        );
    }

    #[test]
    fn positive_unknown_field_keeps_its_range_next_to_a_comment() {
        // Комментарий не должен ни попадать в имя, ни растягивать диапазон.
        check_with_designer(
            "ВЫБРАТЬ\n    |\tТ.НетТакогоПоля КАК Поле\n    |ИЗ\n    |\tСправочник.Справочник1 КАК Т // комментарий",
            expect![[r#"
                UnknownFieldInQuery @ 4:9..4:22
                  message: Поле "НетТакогоПоля" не найдено в таблице "Справочник.Справочник1" запроса
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn bilingual_unknown_field_english_table() {
        check_with_designer(
            "SELECT T.NoSuchField FROM Catalog.Справочник1 AS T",
            expect![[r#"
                UnknownFieldInQuery @ 3:30..3:41
                  message: Поле "NoSuchField" не найдено в таблице "Catalog.Справочник1" запроса
                  severity: Blocker"#]],
        );
    }
}
