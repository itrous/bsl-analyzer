use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use bsl_metadata::{EventSubscription, EventSubscriptionHandler};
use hir::{ModuleId, Name};
use ide_db::TextRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: crate::metadata::DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::SessionModule],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::MissingEventSubscriptionHandler;

    if ctx.is_disabled_with_metadata(code) {
        return Vec::new();
    }

    if !is_session_module(ctx) {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    for event_sub in ctx.main_event_subscriptions() {
        check_event_subscription(ctx, &event_sub, code, &mut diagnostics);
    }

    diagnostics
}

fn is_session_module(ctx: &DiagnosticsContext) -> bool {
    let file_path = match ctx.file_path() {
        Some(path) => path,
        None => return false,
    };

    bsl_conventions::path_ends_with_ext_child(
        &file_path,
        bsl_conventions::ConventionalName::SessionModule,
    )
}

fn check_event_subscription(
    ctx: &DiagnosticsContext,
    event_sub: &EventSubscription,
    code: DiagnosticCode,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if event_sub.handler_string().is_empty() {
        diagnostics.push(create_diagnostic(
            ctx,
            DiagnosticType::EmptyHandler,
            event_sub.name(),
            "",
            code,
        ));
        return;
    }

    let handler = match event_sub.parse_handler() {
        Some(h) if h.method_name.is_empty() => {
            diagnostics.push(create_diagnostic(
                ctx,
                DiagnosticType::IncorrectFormat,
                event_sub.name(),
                event_sub.handler_string(),
                code,
            ));
            return;
        }
        Some(h) => h,
        None => return,
    };

    let common_module = match ctx.resolve_common_module(&handler.module_name) {
        Some(found) => found,
        None => {
            diagnostics.push(create_diagnostic(
                ctx,
                DiagnosticType::MissingModule,
                event_sub.name(),
                &handler.module_name,
                code,
            ));
            return;
        }
    };

    if !common_module.is_server() {
        diagnostics.push(create_diagnostic(
            ctx,
            DiagnosticType::ShouldBeServer,
            event_sub.name(),
            &handler.module_name,
            code,
        ));
    }

    check_method(ctx, event_sub, &handler, code, diagnostics);
}

fn check_method(
    ctx: &DiagnosticsContext,
    event_sub: &EventSubscription,
    handler: &EventSubscriptionHandler,
    code: DiagnosticCode,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let bodies = ctx.common_module_bodies(&handler.module_name);
    if bodies.is_empty() {
        return;
    }

    let method_name_obj = Name::new(&handler.method_name);
    let mut saw_non_export = false;

    match bodies.search_merged_surface(|module_file_id| {
        let symbol_tree = ctx.symbol_tree_for(ModuleId::new(module_file_id));
        let method = symbol_tree.find_method(&method_name_obj)?;
        if !method.is_export {
            saw_non_export = true;
            return None;
        }
        Some(())
    }) {
        // The handler is there and exported: nothing to report.
        hir::BodySearch::Found(()) => return,
        hir::BodySearch::Absent => {}
        // "This module declares no such handler" is a claim about the whole module, and
        // part of it could not be read: the handler may be declared exactly there.
        hir::BodySearch::Unread => return,
    }

    let detail = format!("{}.{}", handler.module_name, handler.method_name);
    let dtype = if saw_non_export {
        DiagnosticType::NonExportMethod
    } else {
        DiagnosticType::MissingMethod
    };
    diagnostics.push(create_diagnostic(ctx, dtype, event_sub.name(), &detail, code));
}

#[derive(Debug, Clone, Copy)]
enum DiagnosticType {
    EmptyHandler,
    IncorrectFormat,
    MissingModule,
    ShouldBeServer,
    MissingMethod,
    NonExportMethod,
}

fn create_diagnostic(
    ctx: &DiagnosticsContext,
    diagnostic_type: DiagnosticType,
    event_sub_name: &str,
    detail: &str,
    code: DiagnosticCode,
) -> Diagnostic {
    let message = match diagnostic_type {
        DiagnosticType::EmptyHandler => {
            format!("Заполните обработчик подписки на событие \"{}\"", event_sub_name)
        }
        DiagnosticType::IncorrectFormat => {
            format!(
                "Исправьте некорректный обработчик \"{}\" у подписки на событие \"{}\"",
                detail, event_sub_name
            )
        }
        DiagnosticType::MissingModule => {
            format!(
                "Создайте модуль \"{}\" или исправьте некорректный обработчик подписки на событие \"{}\"",
                detail, event_sub_name
            )
        }
        DiagnosticType::ShouldBeServer => {
            format!(
                "Добавьте \"Сервер\" модулю \"{}\" или исправьте некорректный обработчик подписки на событие \"{}\"",
                detail, event_sub_name
            )
        }
        DiagnosticType::MissingMethod => {
            format!(
                "Создайте процедуру \"{}\" или исправьте некорректный обработчик подписки на событие \"{}\"",
                detail, event_sub_name
            )
        }
        DiagnosticType::NonExportMethod => {
            format!(
                "Добавьте \"Экспорт\" процедуре \"{}\"  или исправьте некорректный обработчик подписки на событие \"{}\"",
                detail, event_sub_name
            )
        }
    };

    let file_text = ctx.file_text();
    let file_len = file_text.len();

    let end_offset = std::cmp::min(14, file_len);
    let range = TextRange::new(0.into(), (end_offset as u32).into());

    Diagnostic {
        code,
        message,
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
    use std::path::PathBuf;
    use vfs::{FileId, FileSet, VfsPath};
    fn check_diagnostic(code: &str, fixtures_dir: &str) -> (Vec<Diagnostic>, String) {
        let mut db = RootDatabaseImpl::new();

        let workspace_root = PathBuf::from(fixtures_dir);

        let mut file_set = FileSet::default();

        let file_id = FileId(0);
        let session_module_path = VfsPath::new(format!("{}/Ext/SessionModule.bsl", fixtures_dir));
        file_set.insert(file_id, session_module_path);

        let common_module_file_id = FileId(1);
        let common_module_path = VfsPath::new(format!(
            "{}/CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl",
            fixtures_dir
        ));
        file_set.insert(common_module_file_id, common_module_path);

        let source_root_id = SourceRootId(0);
        let source_root = SourceRoot::new_local(file_set);

        db.set_source_root(source_root_id, source_root);
        db.set_file_source_root(file_id, source_root_id);
        db.set_file_source_root(common_module_file_id, source_root_id);
        db.set_file_text(file_id, code);

        let common_module_code = std::fs::read_to_string(format!(
            "{}/CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl",
            fixtures_dir
        ))
        .unwrap_or_default();
        db.set_file_text(common_module_file_id, &common_module_code);

        let configuration_path_input = ide_db::metadata::ConfigurationPathInput::new(
            &db,
            workspace_root.to_string_lossy().to_string(),
            0,
        );

        let provider = ide_db::SalsaProvider::new(&db, Some(configuration_path_input));
        let config = DiagnosticsConfig::default();
        let ctx = DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = check(&ctx);
        (diagnostics, code.to_string())
    }

    #[test]
    fn test_missing_event_subscription_handler() {
        let fixtures_dir =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");

        let code = "Функция Маркер()\nКонецФункции\n";
        let (diagnostics, file_content) = check_diagnostic(code, fixtures_dir);

        expect![[r#"
            MissingEventSubscriptionHandler @ 1:1..1:8
              message: Добавьте "Сервер" модулю "КлиентскийОбщийМодуль" или исправьте некорректный обработчик подписки на событие "ПередЗаписьюДокумента"
              severity: Blocker
            MissingEventSubscriptionHandler @ 1:1..1:8
              message: Добавьте "Экспорт" процедуре "ПервыйОбщийМодуль.РегистрацияИзмененийПередУдалением"  или исправьте некорректный обработчик подписки на событие "РегистрацияИзмененийПередУдалением"
              severity: Blocker
            MissingEventSubscriptionHandler @ 1:1..1:8
              message: Заполните обработчик подписки на событие "ПередЗаписьюКонстанты"
              severity: Blocker
            MissingEventSubscriptionHandler @ 1:1..1:8
              message: Исправьте некорректный обработчик "CommonModule.ОбщийПодпискиНаСобытия" у подписки на событие "ПриЗаписиДокумента"
              severity: Blocker
            MissingEventSubscriptionHandler @ 1:1..1:8
              message: Создайте модуль "ОбщийПодпискиНаСобытия" или исправьте некорректный обработчик подписки на событие "ПриЗаписиСправочника"
              severity: Blocker
            MissingEventSubscriptionHandler @ 1:1..1:8
              message: Создайте процедуру "ПервыйОбщийМодуль.ПодпискаНаСобытиеПриУстановкеНовогоКода" или исправьте некорректный обработчик подписки на событие "ПриУстановкеНовогоКода"
              severity: Blocker"#]].assert_eq(&format_diags(&file_content, &diagnostics));
    }

    #[test]
    fn test_not_session_module() {
        let fixtures_dir =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");

        let mut db = RootDatabaseImpl::new();

        let vfs_path = VfsPath::new(format!("{}/CommonModules/Test/Ext/Module.bsl", fixtures_dir));

        let mut file_set = FileSet::default();
        let file_id = FileId(0);
        file_set.insert(file_id, vfs_path.clone());

        let source_root_id = SourceRootId(0);
        let source_root = SourceRoot::new_local(file_set);

        db.set_source_root(source_root_id, source_root);
        db.set_file_source_root(file_id, source_root_id);
        db.set_file_text(file_id, "Процедура Тест()\nКонецПроцедуры");

        let provider = ide_db::SalsaProvider::new(&db, None);
        let config = DiagnosticsConfig::default();
        let ctx = DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = check(&ctx);

        expect![[r#""#]].assert_eq(&format_diags("Процедура Тест()\nКонецПроцедуры", &diagnostics));
    }

    /// "The module does not declare this handler" is a claim about the whole module. A
    /// body whose bytes could not be read makes that claim underivable: the handler may
    /// well be declared there, and the readable extension body lacking it proves nothing
    /// about the body ahead of it in priority.
    #[test]
    fn an_unread_base_body_bars_the_missing_handler_verdict() {
        use crate::test_utils::check_cfe_at_with_unreadable;
        use test_fixture::CfeFixtureBuilder;

        let subscription_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <EventSubscription uuid="6daf85b1-5fbf-411c-9bdd-4eddd85bd65a">
        <Properties>
            <Name>ПриЗаписи</Name>
            <Event>OnWrite</Event>
            <Handler>CommonModule.Сервер.Обработчик</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#;

        // Both bodies lack the handler, so the verdict is derivable — until one of them
        // stops being readable. Only the unreadable flag differs between the two runs.
        let fixture = || {
            let mut builder = CfeFixtureBuilder::new("");
            builder
                .add_base_module("Сервер", "Процедура Иное() Экспорт КонецПроцедуры")
                .add_extension("Расш", "")
                .add_extension_module("Расш", "Сервер", "Процедура Иное() Экспорт КонецПроцедуры");
            let fixture = builder.build();
            let dir = fixture.root().join("EventSubscriptions");
            std::fs::create_dir_all(&dir).expect("create EventSubscriptions directory");
            std::fs::write(dir.join("ПриЗаписи.xml"), subscription_xml)
                .expect("write EventSubscription");
            fixture
        };
        let session_module = "Процедура Маркер()\nКонецПроцедуры\n";

        let control = check_cfe_at_with_unreadable(
            "Ext/SessionModule.bsl",
            session_module,
            fixture(),
            &[],
            check,
        );
        assert!(
            control.iter().any(|d| d.code == DiagnosticCode::MissingEventSubscriptionHandler),
            "control: a readable module without the handler must be reported, got {control:?}"
        );

        let unread = check_cfe_at_with_unreadable(
            "Ext/SessionModule.bsl",
            session_module,
            fixture(),
            &["CommonModules/Сервер/Ext/Module.bsl"],
            check,
        );
        assert!(
            unread.iter().all(|d| d.code != DiagnosticCode::MissingEventSubscriptionHandler),
            "an unread body may declare the handler, so absence is not provable: {unread:?}"
        );
    }

    /// Проект из одного расширения: главной конфигурации нет, и проверка
    /// обработчиков обязана увидеть подписку расширения и сообщить о её
    /// негодном обработчике (github#172).
    #[test]
    fn a_broken_handler_of_an_extension_only_project_is_reported() {
        let root = stdx::test_temp::unique_dir("bsl_missing_handler_extension_only");
        std::fs::create_dir_all(root.join("Ext")).unwrap();
        std::fs::create_dir_all(root.join("EventSubscriptions")).unwrap();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
        std::fs::write(
            root.join("EventSubscriptions/ПередЗаписью.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <EventSubscription uuid="00000000-0000-0000-0000-000000000172">
        <Properties>
            <Name>ПередЗаписью</Name>
            <Source><Type>CatalogRef.Номенклатура</Type></Source>
            <Event>BeforeWrite</Event>
            <Handler>CommonModule.Сервер.Обработчик</Handler>
        </Properties>
    </EventSubscription>
</MetaDataObject>"#,
        )
        .unwrap();

        let mut db = RootDatabaseImpl::new();
        let session_file = FileId(0);
        let mut file_set = FileSet::default();
        let session_path =
            VfsPath::new(root.join("Ext/SessionModule.bsl").to_string_lossy().as_ref());
        file_set.insert(session_file, session_path);
        let source_root_id = SourceRootId(0);
        db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
        db.set_file_source_root(session_file, source_root_id);
        db.set_file_text(session_file, "Функция Маркер()\nКонецФункции\n");

        // Единственная конфигурация — расширение X: главной нет вовсе.
        db.set_all_config_paths(
            vec![(Some("X".to_string()), root.clone())],
            Some(bsl_metadata::ExtensionScope::InfoBase),
        );

        let configuration_path_input = ide_db::metadata::ConfigurationPathInput::new(
            &db,
            root.to_string_lossy().to_string(),
            0,
        );
        let provider = ide_db::SalsaProvider::new(&db, Some(configuration_path_input));
        let config = DiagnosticsConfig::default();
        let ctx = DiagnosticsContext::new(&config, session_file, &provider);

        let diagnostics = check(&ctx);
        assert_eq!(diagnostics.len(), 1, "one broken handler must be reported: {diagnostics:#?}");
        assert!(
            diagnostics[0].message.contains("Создайте модуль")
                && diagnostics[0].message.contains("ПередЗаписью"),
            "the report must name the extension's subscription: {diagnostics:#?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
