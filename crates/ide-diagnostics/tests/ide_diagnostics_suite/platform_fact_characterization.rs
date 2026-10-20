use expect_test::{expect, Expect};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::{RootDatabaseImpl, SalsaProvider};
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use test_fixture::Fixture;

const PLATFORM_DEPRECATED_CODES: &[DiagnosticCode] = &[DiagnosticCode::DeprecatedPlatformApi];

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn deprecated_platform_diagnostic_families_match_current_replacements() {
    let source = r#"Процедура DeprecatedPlatformFacts()
    Дата = ТекущаяДата();
    Позиция = Find("abcdef", "cd");
    Message("legacy user notification");
    Представление = GetShortApplicationCaption();
    Описание = BriefErrorDescription(ИнформацияОбОшибке());
    Форма = GetForm("Форма");
    Если TypeOf(Форма) = Type("ManagedForm") Тогда
    КонецЕсли;
    ChartPlotArea.ShowScale = True;
КонецПроцедуры"#;

    check_deprecated_platform_snapshot(
        source,
        PLATFORM_DEPRECATED_CODES,
        expect![[r#"
            DeprecatedPlatformApi @ 2:12..2:23
              message: Используйте "ТекущаяДатаСеанса" вместо устаревшего "ТекущаяДата"
              severity: Warning
            DeprecatedPlatformApi @ 3:15..3:19
              message: Use "StrFind" instead of deprecated "Find"
              severity: Warning
            DeprecatedPlatformApi @ 4:5..4:12
              message: Use "CommonUse.MessageToUser" instead of deprecated "Message"
              severity: Warning
            DeprecatedPlatformApi @ 5:21..5:49
              message: Method "GetShortApplicationCaption" is deprecated. You should use "ClientApplication.GetShortCaption".
              severity: Warning
            DeprecatedPlatformApi @ 6:16..6:59
              message: Method "BriefErrorDescription" is deprecated. You should use "ErrorProcessing.BriefErrorDescription".
              severity: Warning
            DeprecatedPlatformApi @ 7:13..7:29
              message: Method "GetForm" is deprecated. You should use "OpenForm".
              severity: Warning
            DeprecatedPlatformApi @ 8:31..8:44
              message: Usage of deprecated type "ManagedForm". Recommended to use "ClientApplicationForm"
              severity: Warning
            DeprecatedPlatformApi @ 10:19..10:28
              message: Attribute "ShowScale" is deprecated. Используйте: ShowScales
              severity: Warning"#]],
    );
}

#[test]
fn deprecated_method_call_remains_source_doc_based() {
    let source = r#"Процедура CallsPlatformDeprecatedApis()
    Дата = ТекущаяДата();
    Позиция = Find("abcdef", "cd");
    Message("legacy user notification");
    Форма = GetForm("Форма");
КонецПроцедуры

// Deprecated. Use SourceReplacement().
Procedure SourceDeprecated()
EndProcedure

Procedure Test()
    SourceDeprecated();
EndProcedure"#;

    check_deprecated_platform_snapshot(
        source,
        &[DiagnosticCode::DeprecatedMethodCall],
        expect![[r#"
            DeprecatedMethodCall @ 13:5..13:21
              message: Remove deprecated method "SourceDeprecated" call. Use SourceReplacement().
              severity: Information"#]],
    );
}

#[test]
fn get_form_keeps_deprecated_and_get_form_diagnostics() {
    let source = r#"Процедура DeprecatedGetFormDuplicate()
    Форма = GetForm("Форма");
КонецПроцедуры"#;

    check_deprecated_platform_snapshot(
        source,
        &[DiagnosticCode::DeprecatedPlatformApi, DiagnosticCode::GetFormMethod],
        expect![[r#"
            GetFormMethod @ 2:13..2:20
              message: Использование метода 'GetForm' приводит к ошибкам. Используйте 'ОткрытьФорму()' вместо него
              severity: Major
            DeprecatedPlatformApi @ 2:13..2:29
              message: Method "GetForm" is deprecated. You should use "OpenForm".
              severity: Warning"#]],
    );
}

fn check_deprecated_platform_snapshot(source: &str, codes: &[DiagnosticCode], expected: Expect) {
    let mut diagnostics = diagnostics_for(source)
        .into_iter()
        .filter(|diagnostic| codes.contains(&diagnostic.code))
        .collect::<Vec<_>>();

    diagnostics.sort_by(|left, right| {
        (left.range.start(), left.range.end(), left.code.as_str(), left.message.as_str()).cmp(&(
            right.range.start(),
            right.range.end(),
            right.code.as_str(),
            right.message.as_str(),
        ))
    });

    expected.assert_eq(&format_diagnostics(source, &diagnostics));
}

fn diagnostics_for(source: &str) -> Vec<Diagnostic> {
    let fixture_text = format!("//- /test.bsl\n{source}");
    let fixture = Fixture::parse(&fixture_text);
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

fn format_diagnostics(source: &str, diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| format_diagnostic(source, diagnostic))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_diagnostic(source: &str, diagnostic: &Diagnostic) -> String {
    let (start_line, start_col, end_line, end_col) = range_to_line_col(source, diagnostic.range);
    format!(
        "{} @ {}:{}..{}:{}\n  message: {}\n  severity: {}",
        diagnostic.code.as_str(),
        start_line + 1,
        start_col + 1,
        end_line + 1,
        end_col + 1,
        diagnostic.message,
        diagnostic.severity.as_str()
    )
}

fn range_to_line_col(source: &str, range: ide_db::TextRange) -> (u32, u32, u32, u32) {
    let start_offset = usize::from(range.start());
    let end_offset = usize::from(range.end());
    let mut line = 0;
    let mut col = 0;
    let mut byte_pos = 0;
    let mut start = (0, 0);
    let mut end = (0, 0);

    for ch in source.chars() {
        if byte_pos == start_offset {
            start = (line, col);
        }
        if byte_pos == end_offset {
            end = (line, col);
            break;
        }

        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
        byte_pos += ch.len_utf8();
    }

    if byte_pos == end_offset {
        end = (line, col);
    }

    (start.0, start.1, end.0, end.1)
}
