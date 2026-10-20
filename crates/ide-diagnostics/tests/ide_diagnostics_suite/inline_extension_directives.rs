use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig};
use vfs::{FileId, FileSet, VfsPath};

const ISSUE_SOURCE: &str =
    include_str!("../../../parser/tests/fixtures/issue150_ext_directives.bsl");

fn diagnostics(source: &str) -> Vec<Diagnostic> {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
}

fn claims(source: &str) -> Vec<(String, String)> {
    let mut claims: Vec<_> = diagnostics(source)
        .into_iter()
        .map(|diagnostic| (diagnostic.code.as_str().to_string(), diagnostic.message))
        .collect();
    claims.sort();
    claims
}

fn canonical_issue_source() -> String {
    ISSUE_SOURCE
        .replace("#Вставка\n\t|\tТ.Договор КАК Договор,\n#КонецВставки\n", "\t|\tТ.Договор КАК Договор,\n")
        .replace("\t\t#Вставка\n\t\tИ Не Данные.Флаг\n\t\t#КонецВставки\n", "\t\tИ Не Данные.Флаг\n")
        .replace(
            "\t\t#Удаление\n\t\tДанные.Ссылка,\n\t\t#КонецУдаления\n\t\t#Вставка\n\t\tДанные.НоваяСсылка,\n\t\t#КонецВставки\n",
            "\t\tДанные.НоваяСсылка,\n",
        )
}

#[test]
fn issue_example_has_the_same_full_findings_as_canonical_code() {
    let independent_error = "\nПроцедура Контроль()\n\tА = А;\nКонецПроцедуры\n";
    let with_directives = format!("{ISSUE_SOURCE}{independent_error}");
    let canonical = format!("{}{independent_error}", canonical_issue_source());

    let actual = claims(&with_directives);
    let expected = claims(&canonical);
    assert_eq!(actual, expected, "inline directives must not add or suppress findings");
    assert!(
        actual.iter().any(|(code, _)| code == "SelfAssign"),
        "the independent positive-control diagnostic must survive: {actual:?}"
    );
    assert!(
        actual.iter().all(|(code, _)| code != "ParseError" && code != "QueryParseError"),
        "the three issue forms must not create parse findings: {actual:?}"
    );
}

#[test]
fn invalid_active_query_is_reported_at_its_original_character() {
    let source = r#"Функция Тест()
    Текст = "ВЫБРАТЬ
    #Удаление
    |Т.Старое КАК Старое,
    #КонецУдаления
    #Вставка
    |Т. КАК Ошибка
    #КонецВставки
    |ИЗ Таблица КАК Т";
    Возврат Текст;
КонецФункции"#;
    let query_errors: Vec<_> = diagnostics(source)
        .into_iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::QueryParseError)
        .collect();
    assert!(!query_errors.is_empty(), "invalid query is the positive control");
    let active_boundary = source.find("КАК Ошибка").expect("bad active path boundary");
    assert!(
        query_errors.iter().any(|diagnostic| {
            usize::from(diagnostic.range.start()) == active_boundary && diagnostic.range.is_empty()
        }),
        "the missing-field error must point before the active КАК, not at a marker/deletion: {query_errors:?}"
    );
    for diagnostic in query_errors {
        let text = &source[diagnostic.range];
        assert!(!text.contains('#') && !text.contains("Старое"));
    }
}

#[test]
fn malformed_bsl_and_extension_markers_still_emit_parse_errors() {
    let controls = [
        (
            "missing Тогда",
            r#"Функция Тест()
    Если Истина
        #Вставка
        И Ложь
        #КонецВставки
        Возврат 1;
    КонецЕсли;
КонецФункции"#,
        ),
        ("orphan closer", "Функция Тест()\n    Возврат 1 #КонецВставки;\nКонецФункции"),
        ("unclosed deletion", "Функция Тест()\n    Возврат 1 + #Удаление 2;\nКонецФункции"),
        ("unclosed insertion", "Функция Тест()\n    Возврат 1 + #Вставка 2;\nКонецФункции"),
    ];
    for (label, source) in controls {
        let found = diagnostics(source);
        assert!(
            found.iter().any(|diagnostic| diagnostic.code == DiagnosticCode::ParseError),
            "{label} must remain a real ParseError, got {found:?}"
        );
    }
}

#[test]
fn one_statement_per_line_still_sees_real_crowding_after_inline_directives() {
    let source = r#"Процедура Тест()
    Значение = 1
    #Вставка
    + 2
    #КонецВставки;
    А = 1; Б = 2;
КонецПроцедуры"#;
    let findings: Vec<_> = diagnostics(source)
        .into_iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::OneStatementPerLine)
        .collect();
    assert_eq!(findings.len(), 1, "only the genuinely crowded statement is flagged");
    assert_eq!(&source[findings[0].range], "Б = 2");
}
