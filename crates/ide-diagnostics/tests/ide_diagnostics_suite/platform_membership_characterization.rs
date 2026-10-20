use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::{RootDatabaseImpl, SalsaProvider};
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use test_fixture::Fixture;

#[test]
fn use_system_information_membership_is_constructor_only() {
    let source = r#"Процедура Тест()
    RuCtor = Новый СистемнаяИнформация;
    RuString = Новый("СистемнаяИнформация");
    EnCtor = Новый SystemInfo;
    EnString = Новый("SystemInfo");
    TypeName = "СистемнаяИнформация";
    DynamicCtor = Новый(TypeName);
КонецПроцедуры"#;

    expect_test::expect![[r#"
        UseSystemInformation line 2 `RuCtor = Новый СистемнаяИнформация;`
          message: Use of system information
          severity: Warning
        UseSystemInformation line 3 `RuString = Новый("СистемнаяИнформация");`
          message: Use of system information
          severity: Warning
        UseSystemInformation line 4 `EnCtor = Новый SystemInfo;`
          message: Use of system information
          severity: Warning
        UseSystemInformation line 5 `EnString = Новый("SystemInfo");`
          message: Use of system information
          severity: Warning"#]]
    .assert_eq(&format_filtered(source, DiagnosticCode::UseSystemInformation));
}

#[test]
fn using_object_not_available_unix_membership_respects_platform_guards() {
    let source = r#"Процедура Тест()
    RuCom = Новый COMОбъект("Scripting.FileSystemObject");
    RuMail = Новый("Почта");
    EnCom = Новый COMObject("Scripting.FileSystemObject");
    EnMail = Новый("Mail");
    Internet = Новый ИнтернетПочта();
    Если ТипПлатформы.Windows Тогда
        Guarded = Новый COMОбъект("Scripting.FileSystemObject");
    КонецЕсли;
КонецПроцедуры"#;

    expect_test::expect![[r#"
        UsingObjectNotAvailableUnix line 2 `RuCom = Новый COMОбъект("Scripting.FileSystemObject");`
          message: Проверить, что задействованы аналоги "COMОбъект" при работе в Unix-клиенте.
          severity: Critical
        UsingObjectNotAvailableUnix line 3 `RuMail = Новый("Почта");`
          message: Проверить, что задействованы аналоги "Почта" при работе в Unix-клиенте.
          severity: Critical
        UsingObjectNotAvailableUnix line 4 `EnCom = Новый COMObject("Scripting.FileSystemObject");`
          message: Проверить, что задействованы аналоги "COMObject" при работе в Unix-клиенте.
          severity: Critical
        UsingObjectNotAvailableUnix line 5 `EnMail = Новый("Mail");`
          message: Проверить, что задействованы аналоги "Mail" при работе в Unix-клиенте.
          severity: Critical"#]]
    .assert_eq(&format_filtered(source, DiagnosticCode::UsingObjectNotAvailableUnix));
}

#[test]
fn temp_files_dir_membership_is_global_function_only() {
    let source = r#"Процедура Тест()
    RuPath = КаталогВременныхФайлов();
    EnPath = TempFilesDir();
    Ignored = Модуль.TempFilesDir();
КонецПроцедуры"#;

    expect_test::expect![[r#"
        TempFilesDir line 2 `RuPath = КаталогВременныхФайлов();`
          message: Не рекомендуемый вызов функции КаталогВременныхФайлов()
          severity: Warning
        TempFilesDir line 3 `EnPath = TempFilesDir();`
          message: Not recommended TempFilesDir() call
          severity: Warning"#]]
    .assert_eq(&format_filtered(source, DiagnosticCode::TempFilesDir));
}

#[test]
fn form_data_to_value_membership_skips_no_context_methods() {
    let source = r#"Процедура Тест()
    ДанныеФормыВЗначение(Объект, Тип("ТаблицаЗначений"));
    Форма.ДанныеФормыВЗначение(Объект, Тип("ТаблицаЗначений"));
    FormDataToValue(Object, Type("ValueTable"));
    Form.FormDataToValue(Object, Type("ValueTable"));
КонецПроцедуры

&НаСервереБезКонтекста
Процедура БезКонтекста()
    ДанныеФормыВЗначение(Объект, Тип("ТаблицаЗначений"));
    FormDataToValue(Object, Type("ValueTable"));
КонецПроцедуры"#;

    expect_test::expect![[r#"
        FormDataToValue line 2 `ДанныеФормыВЗначение(Объект, Тип("ТаблицаЗначений"));`
          message: Обнаружено использование метода ДанныеФормыВЗначение
          severity: Hint
        FormDataToValue line 4 `FormDataToValue(Object, Type("ValueTable"));`
          message: Обнаружено использование метода ДанныеФормыВЗначение
          severity: Hint"#]]
    .assert_eq(&format_filtered(source, DiagnosticCode::FormDataToValue));
}

#[test]
fn get_form_method_membership_covers_global_and_member_calls() {
    let source = r#"Процедура Тест()
    GlobalRu = ПолучитьФорму("Форма");
    ObjectRu = Док.ПолучитьФорму("Форма");
    GlobalEn = GetForm("Form");
    ObjectEn = Doc.GetForm("Form");
    Safe = ОткрытьФорму("Форма");
КонецПроцедуры"#;

    expect_test::expect![[r#"
        GetFormMethod line 2 `GlobalRu = ПолучитьФорму("Форма");`
          message: Использование метода 'ПолучитьФорму' приводит к ошибкам. Используйте 'ОткрытьФорму()' вместо него
          severity: Major
        GetFormMethod line 3 `ObjectRu = Док.ПолучитьФорму("Форма");`
          message: Использование метода 'ПолучитьФорму' приводит к ошибкам. Используйте 'ОткрытьФорму()' вместо него
          severity: Major
        GetFormMethod line 4 `GlobalEn = GetForm("Form");`
          message: Использование метода 'GetForm' приводит к ошибкам. Используйте 'ОткрытьФорму()' вместо него
          severity: Major
        GetFormMethod line 5 `ObjectEn = Doc.GetForm("Form");`
          message: Использование метода 'GetForm' приводит к ошибкам. Используйте 'ОткрытьФорму()' вместо него
          severity: Major"#]]
    .assert_eq(&format_filtered(source, DiagnosticCode::GetFormMethod));
}

fn format_filtered(source: &str, code: DiagnosticCode) -> String {
    let mut diagnostics =
        run_diagnostics(source).into_iter().filter(|diag| diag.code == code).collect::<Vec<_>>();
    diagnostics.sort_by(|left, right| {
        (line_number(source, left), left.message.as_str())
            .cmp(&(line_number(source, right), right.message.as_str()))
    });

    let mut output = String::new();
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        let line_number = line_number(source, diagnostic);
        let source_line =
            source.lines().nth(line_number.saturating_sub(1)).map(str::trim).unwrap_or_default();

        use std::fmt::Write as _;
        writeln!(output, "{} line {} `{}`", diagnostic.code.as_str(), line_number, source_line)
            .expect("writing to String should not fail");
        writeln!(output, "  message: {}", diagnostic.message)
            .expect("writing to String should not fail");
        write!(output, "  severity: {}", diagnostic.severity.as_str())
            .expect("writing to String should not fail");
    }
    output
}

fn line_number(source: &str, diagnostic: &Diagnostic) -> usize {
    let start: usize = diagnostic.range.start().into();
    source[..start].matches('\n').count() + 1
}

fn run_diagnostics(source: &str) -> Vec<Diagnostic> {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{source}"));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();

    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    let file_id = *fixture.files.keys().last().expect("fixture should contain a test file");
    let config = DiagnosticsConfig::all_enabled();
    let provider = SalsaProvider::new(&db, None);
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);

    ide_diagnostics::diagnostics(&ctx)
}
