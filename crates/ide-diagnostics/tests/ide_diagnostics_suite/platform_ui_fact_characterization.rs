use expect_test::{expect, Expect};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::{RootDatabaseImpl, SalsaProvider};
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, DiagnosticsContext};
use test_fixture::Fixture;

#[test]
fn using_modal_windows_reports_ru_and_en_modal_replacements() {
    let source = r#"Процедура ModalFactsRu()
    Вопрос(""Продолжить?"", РежимДиалогаВопрос.ДаНет, 0);
КонецПроцедуры

Procedure ModalFactsEn()
    DoMessageBox(""Select item"", 10);
EndProcedure"#;

    check_platform_fact_snapshot(
        source,
        DiagnosticCode::UsingModalWindows,
        expect![[r#"
            UsingModalWindows @ 2:5..2:24
              message: Вместо модального метода "Вопрос" необходимо использовать "ПоказатьВопрос"
              severity: Warning
            UsingModalWindows @ 6:5..6:26
              message: Вместо модального метода "DoMessageBox" необходимо использовать "ShowMessageBox"
              severity: Warning"#]],
    );
}

#[test]
fn using_synchronous_calls_reports_ru_and_en_sync_only_replacements() {
    let source = r#"Процедура SyncFactsRu()
    УдалитьФайлы(""C:\temp\a.txt"");
КонецПроцедуры

Procedure SyncFactsEn()
    DeleteFiles(""C:\temp\a.txt"");
EndProcedure"#;

    check_platform_fact_snapshot(
        source,
        DiagnosticCode::UsingSynchronousCalls,
        expect![[r#"
            UsingSynchronousCalls @ 2:5..2:21
              message: Вместо синхронного вызова "УдалитьФайлы" необходимо использовать "НачатьУдалениеФайлов"
              severity: Warning
            UsingSynchronousCalls @ 6:5..6:20
              message: Вместо синхронного вызова "DeleteFiles" необходимо использовать "BeginDeletingFiles"
              severity: Warning"#]],
    );
}

#[test]
fn code_after_async_call_reports_ru_and_en_async_entries() {
    let source = r#"Процедура AsyncFactsRu()
    НачатьУдалениеФайлов(, ""C:\temp\a.txt"");
    Сообщить(""after"");
КонецПроцедуры

Procedure AsyncFactsEn()
    BeginDeletingFiles(, ""C:\temp\a.txt"");
    Message(""after"");
EndProcedure"#;

    check_platform_fact_snapshot(
        source,
        DiagnosticCode::CodeAfterAsyncCall,
        expect![[r#"
            CodeAfterAsyncCall @ 2:5..2:31
              message: После вызова асинхронного метода 'НачатьУдалениеФайлов' есть строки кода. Код выполнится немедленно, не дожидаясь завершения асинхронной операции
              severity: Warning
            CodeAfterAsyncCall @ 7:5..7:29
              message: После вызова асинхронного метода 'BeginDeletingFiles' есть строки кода. Код выполнится немедленно, не дожидаясь завершения асинхронной операции
              severity: Warning"#]],
    );
}

fn check_platform_fact_snapshot(source: &str, code: DiagnosticCode, expected: Expect) {
    let diagnostics = diagnostics_for(source)
        .into_iter()
        .filter(|diagnostic| diagnostic.code == code)
        .collect::<Vec<_>>();

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
