//! `BuiltinFunctionNameAsDeclaration` as a language-server client receives it: an Error on the
//! declared name of `Функция Найти`, and nothing on the declarations and the call beside it.

mod common;

use common::*;
use serde_json::Value;

/// The declaration of the issue, then its two controls: a function named `ПолучитьЗначение`
/// and a function that calls `Массив.Найти(...)`.
const SOURCE: &str = "\
Функция Найти(Значение) Экспорт
    Возврат Значение;
КонецФункции

Функция ПолучитьЗначение(Значение) Экспорт
    Возврат Значение;
КонецФункции

Функция ИндексВМассиве(Массив, Значение) Экспорт
    Возврат Массив.Найти(Значение);
КонецФункции
";

/// LSP `DiagnosticSeverity.Error`.
const SEVERITY_ERROR: u64 = 1;

fn with_code<'a>(published: &'a Value, code: &str) -> Vec<&'a Value> {
    published["params"]["diagnostics"]
        .as_array()
        .unwrap_or_else(|| panic!("diagnostics array: {published}"))
        .iter()
        .filter(|diagnostic| diagnostic["code"] == code)
        .collect()
}

#[test]
fn the_declared_name_is_published_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/Main.bsl"), SOURCE).unwrap();
    // The configuration of the issue: the target platform and a source root.
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        "target_platform_version = \"8.3.27.2074\"\n\n[source]\nroot = \"src\"\n",
    )
    .unwrap();
    let path = dir.path().join("src/Main.bsl");

    let mut lsp = Lsp::start_stdio(dir.path());
    let published = lsp.open(&path, SOURCE);

    let found = with_code(&published, "BuiltinFunctionNameAsDeclaration");
    assert_eq!(found.len(), 1, "one declaration of the pair, one diagnostic: {published}");
    let diagnostic = found[0];
    assert_eq!(diagnostic["severity"], SEVERITY_ERROR, "{diagnostic}");
    assert_eq!(
        diagnostic["range"],
        serde_json::json!({
            "start": { "line": 0, "character": 8 },
            "end": { "line": 0, "character": 13 },
        }),
        "the range is the name `Найти` on the first line: {diagnostic}"
    );
    assert_eq!(
        diagnostic["message"],
        "Имя \"Найти\" совпадает с именем встроенной функции платформы и не может использоваться как имя объявляемой функции"
    );

    // The pair is an identifier, so the keyword rule stays silent on it.
    assert!(with_code(&published, "ReservedWordAsMethodName").is_empty(), "{published}");
}
