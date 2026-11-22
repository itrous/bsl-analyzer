use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use ide_db::TextRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Info,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::CommandModule],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Clumsy],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::CommandModuleExportMethods;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    let metadata = ctx.module_metadata();

    if metadata.module_type != bsl_metadata::ModuleType::CommandModule {
        return Vec::new();
    }

    let item_tree = ctx.item_tree();
    let module = hir::ModuleId::new(ctx.file_id);
    let summary = ctx.call_summary(module);
    let mut diagnostics = Vec::new();

    for (_, proc) in item_tree.procedures() {
        let method = hir::MethodId { module, local_id: proc.key };
        if proc.is_export
            && !registered_on_this_object(ctx, &summary.notify_regs, &proc.name, method)
        {
            diagnostics.push(make_diagnostic(proc.name_range, code, ctx));
        }
    }

    for (_, func) in item_tree.functions() {
        let method = hir::MethodId { module, local_id: func.key };
        if func.is_export
            && !registered_on_this_object(ctx, &summary.notify_regs, &func.name, method)
        {
            diagnostics.push(make_diagnostic(func.name_range, code, ctx));
        }
    }

    diagnostics
}

// Only a normalized ThisObject registration proves a local callback reference. Unsupported
// and not-compiled receivers must not hide an export diagnostic by name alone.
fn registered_on_this_object(
    ctx: &DiagnosticsContext,
    registrations: &[hir::call_graph::NotifyReg],
    name: &hir::Name,
    method: hir::MethodId,
) -> bool {
    registrations.iter().any(|registration| {
        matches!(&registration.target, hir::call_graph::NotifyTarget::ThisObject)
            && registration.callback_name.eq_ignore_case(name)
            && ctx.notify_handler_availability_in_registration(registration, &[method])
                == hir::NotifyHandlerAvailability::Exported
    })
}

fn make_diagnostic(range: TextRange, code: DiagnosticCode, ctx: &DiagnosticsContext) -> Diagnostic {
    Diagnostic {
        code,
        message: "Экспортные методы в модулях команд не имеют смысла".to_string(),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::format_diags;
    use crate::DiagnosticsConfig;
    use expect_test::expect;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use std::rc::Rc;
    use vfs::{FileId, FileSet, VfsPath};

    const OBJECT_COMMAND: &str =
        "Documents/ПриходнаяНакладная/Commands/ПечатьЭтикеток/Ext/CommandModule.bsl";
    const COMMON_COMMAND: &str = "CommonCommands/ОбновитьОстатки/Ext/CommandModule.bsl";
    const REGULAR_MODULE: &str = "/СкладскойУчет.bsl";

    fn check_command_at(code: &str) -> Vec<Diagnostic> {
        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        crate::test_utils::check_cfe_at_with_db(
            COMMON_COMMAND,
            code,
            fixture,
            &[],
            DiagnosticsConfig::default(),
            |fixture| {
                let command_dir = fixture.root().join("CommonCommands/ОбновитьОстатки");
                std::fs::create_dir_all(command_dir.join("Ext")).unwrap();
                std::fs::write(command_dir.join("Ext/CommandModule.bsl"), code).unwrap();
                std::fs::write(
                    fixture.root().join("CommonCommands/ОбновитьОстатки.xml"),
                    r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonCommand uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>ОбновитьОстатки</Name></Properties>
    </CommonCommand>
</MetaDataObject>"#,
                )
                .unwrap();
            },
            |_, ctx| {
                assert_eq!(
                    ctx.module_metadata().module_type,
                    bsl_metadata::ModuleType::CommandModule
                );
                check(ctx)
            },
        )
    }

    fn check_at(path: &str, code: &str) -> Vec<Diagnostic> {
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId::from_raw(1);

        let mut file_set = FileSet::default();
        file_set.insert(file_id, VfsPath::new(path));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, code);

        let config = Rc::new(DiagnosticsConfig::default());
        let provider = ide_db::SalsaProvider::new(&db, None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        check(&ctx)
    }

    const EXPORTED: &str = r#"&НаКлиенте
Процедура ОбработкаКоманды(Накладная, ПараметрыВыполнения) Экспорт
	НапечататьЭтикетки(Накладная);
КонецПроцедуры

&НаСервере
Функция ЧислоЭтикеток(Накладная) Экспорт
	Возврат Накладная.Товары.Количество();
КонецФункции

&НаКлиенте
Процедура НапечататьЭтикетки(Накладная)
КонецПроцедуры
"#;

    #[test]
    fn test_exported_procedure_and_function_detected() {
        let diagnostics = check_at(OBJECT_COMMAND, EXPORTED);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 7:9..7:22
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(EXPORTED, &diagnostics));
    }

    #[test]
    fn registered_this_object_handler_is_not_reported() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Оповещение = Новый ОписаниеОповещения("послекоманды", ЭтотОбъект);
КонецПроцедуры

&НаКлиенте
Процедура ПослеКоманды() Экспорт
КонецПроцедуры

Процедура ОбычныйЭкспорт() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 10:11..10:25
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn client_registration_does_not_exempt_server_only_handler() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Оповещение = Новый ОписаниеОповещения("ПослеКоманды", ЭтотОбъект);
КонецПроцедуры

&НаСервере
Процедура ПослеКоманды() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 7:11..7:23
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn external_receiver_does_not_hide_export_diagnostic() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
    Оповещение = Новый ОписаниеОповещения("ПослеКоманды", ДругойОбъект);
КонецПроцедуры

Процедура ПослеКоманды() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 6:11..6:23
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn unavailable_or_shadowed_registration_does_not_hide_export_diagnostic() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Команда, Параметры) Экспорт
#Если Сервер И Не Сервер Тогда
    Оповещение = Новый ОписаниеОповещения("ПослеНедоступнойКоманды", ЭтотОбъект);
#КонецЕсли
    ЭтотОбъект = Неопределено;
    Оповещение = Новый ОписаниеОповещения("ПослеЗатенения", ЭтотОбъект);
КонецПроцедуры

Процедура ПослеНедоступнойКоманды() Экспорт
КонецПроцедуры

Процедура ПослеЗатенения() Экспорт
КонецПроцедуры
"#;
        let diagnostics = check_command_at(code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:27
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 10:11..10:34
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint
            CommandModuleExportMethods @ 13:11..13:25
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_non_exported_ignored() {
        let code = r#"&НаКлиенте
Процедура ОбработкаКоманды(Накладная, ПараметрыВыполнения)
	НапечататьЭтикетки(Накладная);
КонецПроцедуры

&НаСервере
Функция ЧислоЭтикеток(Накладная)
	Возврат Накладная.Товары.Количество();
КонецФункции
"#;
        let diagnostics = check_at(OBJECT_COMMAND, code);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_common_command_module() {
        let code = r#"&НаКлиенте
Procedure CommandProcessing(CommandParameter, ExecutionParameters) Export
EndProcedure
"#;
        let diagnostics = check_at(COMMON_COMMAND, code);
        expect![[r#"
            CommandModuleExportMethods @ 2:11..2:28
              message: Экспортные методы в модулях команд не имеют смысла
              severity: Hint"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_regular_module_not_checked() {
        let diagnostics = check_at(REGULAR_MODULE, EXPORTED);
        expect![[r#""#]].assert_eq(&format_diags(EXPORTED, &diagnostics));
    }
}
