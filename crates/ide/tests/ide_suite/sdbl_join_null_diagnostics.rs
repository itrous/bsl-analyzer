//! End-to-end check of the optional-join-side diagnostic through a BSL module.
//!
//! The query lives in a BSL string literal; the file goes through the production path
//! (literal recognition, query extraction, lowering, the IDE adapter) and only the final
//! file diagnostics are inspected. Sources: ITS pubqlang «Внутреннее соединение», «Левое
//! внешнее соединение» (fields of the unmatched side are NULL) and metod8dev «Использование
//! функции ЕСТЬNULL()». The diagnostic range — the field reference projected into the BSL
//! file — is the local adapter contract.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use vfs::{FileId, FileSet, VfsPath};

const CODE: DiagnosticCode = DiagnosticCode::FieldsFromJoinsWithoutIsNull;

/// The texts under every `FieldsFromJoinsWithoutIsNull` diagnostic of the module.
fn reported(source: &str) -> Vec<String> {
    let file_id = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new("/test.bsl"));
    db.set_file_text(file_id, source);
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    let config = DiagnosticsConfig { enabled: vec![CODE], ..DiagnosticsConfig::default() };
    ide_diagnostics::file_diagnostics(&db, file_id, &config)
        .into_iter()
        .filter(|d| d.code == CODE)
        .map(|d| source[usize::from(d.range.start())..usize::from(d.range.end())].to_string())
        .collect()
}

fn module(query: &str) -> String {
    format!(
        "Процедура Остатки()\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{query}\";\n    Выборка = Запрос.Выполнить().Выбрать();\nКонецПроцедуры\n"
    )
}

/// Positive control: the same path reports a field of the optional side used as a value,
/// at the field reference inside the BSL literal.
#[test]
fn an_unprotected_field_of_the_left_joined_side_is_reported_at_its_reference() {
    let source = module(
        "ВЫБРАТЬ Товары.Наименование КАК Наименование, Цены.Цена КАК Цена ИЗ Справочник.Товары КАК Товары ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Товары.Ссылка",
    );
    assert_eq!(reported(&source), vec!["Цены.Цена"]);
}

/// A table joined with ВНУТРЕННЕЕ is never NULL, even when a left join hangs off it:
/// selecting its field is not reported. The left-joined side is used only through
/// ЕСТЬNULL.
#[test]
fn a_field_of_an_inner_joined_source_with_a_nested_left_join_is_not_reported() {
    let source = module(
        "ВЫБРАТЬ Заказ.Номер КАК Номер, Состав.Количество КАК Количество, ЕСТЬNULL(Цены.Цена, 0) КАК Цена ИЗ Документ.Заказ КАК Заказ ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК Состав ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Состав.Товар ПО Состав.Ссылка = Заказ.Ссылка",
    );
    assert_eq!(reported(&source), Vec::<String>::new());
}

/// The same nested form with the optional side used unprotected is still reported,
/// so the negative case above is not silent because the nesting hides the join.
#[test]
fn the_nested_left_join_itself_is_still_checked() {
    let source = module(
        "ВЫБРАТЬ Состав.Количество КАК Количество, Цены.Цена КАК Цена ИЗ Документ.Заказ КАК Заказ ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Состав КАК Состав ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Цены.Товар = Состав.Товар ПО Состав.Ссылка = Заказ.Ссылка",
    );
    assert_eq!(reported(&source), vec!["Цены.Цена"]);
}
