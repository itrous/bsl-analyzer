use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use sdbl_hir;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Sql],
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
    if let sdbl_hir::SdblDiagnostic::UnlimitedStringUsage { range, .. } = diag {
        crate::sdbl_utils::dispatch_simple(
            config,
            DiagnosticCode::UnlimitedLengthStringUsageInQuery,
            &diag.message(),
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
        DiagnosticCode::UnlimitedLengthStringUsageInQuery,
        dispatch,
    )
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::test_utils::format_diags;
    use crate::{DiagnosticsConfig, DiagnosticsContext};
    use expect_test::{expect, Expect};
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use vfs::{FileId, FileSet, VfsPath};

    const CATALOG_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" version="2.10">
	<Catalog uuid="10000000-0000-0000-0000-000000000001">
		<Properties>
			<Name>Лог</Name>
			<Synonym/>
			<Comment/>
		</Properties>
		<ChildObjects>
			<Attribute uuid="10000000-0000-0000-0000-000000000002">
				<Properties>
					<Name>ПредложеноAI</Name>
					<Synonym/>
					<Comment/>
					<Type>
						<v8:Type>xs:string</v8:Type>
						<v8:StringQualifiers>
							<v8:Length>0</v8:Length>
							<v8:AllowedLength>Variable</v8:AllowedLength>
						</v8:StringQualifiers>
					</Type>
				</Properties>
			</Attribute>
			<Attribute uuid="10000000-0000-0000-0000-000000000003">
				<Properties>
					<Name>Номер</Name>
					<Synonym/>
					<Comment/>
					<Type>
						<v8:Type>xs:string</v8:Type>
						<v8:StringQualifiers>
							<v8:Length>10</v8:Length>
							<v8:AllowedLength>Variable</v8:AllowedLength>
						</v8:StringQualifiers>
					</Type>
				</Properties>
			</Attribute>
		</ChildObjects>
	</Catalog>
</MetaDataObject>"#;

    const CONFIGURATION_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
	<Configuration uuid="10000000-0000-0000-0000-000000000000">
		<Properties>
			<Name>ТестНеограниченнаяСтрока</Name>
		</Properties>
		<ChildObjects>
			<Catalog>Лог</Catalog>
		</ChildObjects>
	</Configuration>
</MetaDataObject>"#;

    struct FixtureConfig {
        root: std::path::PathBuf,
    }

    impl FixtureConfig {
        fn materialize() -> Self {
            let root = stdx::test_temp::unique_dir("bsl_unlimited_string_fixture");
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("Catalogs")).expect("create fixture dirs");
            std::fs::write(root.join("Configuration.xml"), CONFIGURATION_XML)
                .expect("write Configuration.xml");
            std::fs::write(root.join("Catalogs/Лог.xml"), CATALOG_XML).expect("write catalog xml");
            Self { root }
        }
    }

    impl Drop for FixtureConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn check_with_unlimited_catalog(query_body: &str, expected: Expect) {
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
        let config = DiagnosticsConfig::all_enabled();
        let ctx = DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = check(&ctx);
        expected.assert_eq(&format_diags(&code, &diagnostics));
    }

    #[test]
    fn positive_comparison_with_parameter() {
        check_with_unlimited_catalog(
            "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.ПредложеноAI <> &Пусто",
            expect![[r#"
                UnlimitedLengthStringUsageInQuery @ 3:65..3:79
                  message: Поле неограниченной длины "Т.ПредложеноAI" нельзя использовать в операции сравнения. Приведите значение к ограниченной длине: ВЫРАЗИТЬ(... КАК СТРОКА(N))
                  severity: Critical"#]],
        );
    }

    #[test]
    fn positive_group_by() {
        check_with_unlimited_catalog(
            "ВЫБРАТЬ Т.ПредложеноAI ИЗ Справочник.Лог КАК Т СГРУППИРОВАТЬ ПО Т.ПредложеноAI",
            expect![[r#"
                UnlimitedLengthStringUsageInQuery @ 3:85..3:99
                  message: Поле неограниченной длины "Т.ПредложеноAI" нельзя использовать в предложении СГРУППИРОВАТЬ ПО. Приведите значение к ограниченной длине: ВЫРАЗИТЬ(... КАК СТРОКА(N))
                  severity: Critical"#]],
        );
    }

    #[test]
    fn negative_bounded_field_comparison() {
        check_with_unlimited_catalog(
            "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.Номер <> &Пусто",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_cast_to_bounded_string() {
        check_with_unlimited_catalog(
            "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ ВЫРАЗИТЬ(Т.ПредложеноAI КАК СТРОКА(100)) <> &Пусто",
            expect![[r#""#]],
        );
    }

    #[test]
    fn negative_like_is_allowed() {
        check_with_unlimited_catalog(
            "ВЫБРАТЬ Т.Номер ИЗ Справочник.Лог КАК Т ГДЕ Т.ПредложеноAI ПОДОБНО &Шаблон",
            expect![[r#""#]],
        );
    }
}
