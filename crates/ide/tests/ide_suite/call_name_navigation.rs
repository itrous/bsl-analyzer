//! Navigation on the callee name of a bare call `Имя(...)`.
//!
//! A bare call looks among METHODS only: a same-named parameter, `Перем` of the body or of
//! the module, or implicit local leaves the call calling the module method, the global
//! common-module export or the platform function (checked live on 8.3.17 and 8.3.27).
//! Goto, hover, references, highlight and rename must name that method on the call, keep the
//! call out of the variable's references and put it into the method's.
//!
//! Every gate carries its control in the same input: the same variable read as a value, or
//! as the receiver of a member call, still names the variable — a navigation that ignored
//! variables everywhere would pass the call assertions alone.

use ide::{Analysis, Locale};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use syntax::TextRange;
use vfs::{FileId, FileSet, VfsPath};

fn designer_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn abs(rel: &str) -> String {
    designer_path().join(rel).to_string_lossy().to_string()
}

const CALLER_REL: &str = "CommonModules/КлиентскийОбщийМодуль/Ext/Module.bsl";
const GLOBAL_REL: &str = "CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl";
const GLOBAL_BODY: &str = "Процедура ГлобальнаяСервернаяПроцедура() Экспорт\nКонецПроцедуры\n";

const CALLER: FileId = FileId(1);
const GLOBAL: FileId = FileId(2);

/// The caller sits next to a global server module on the real `designer` configuration: the
/// `<Global>` flag lives in metadata, which the inline fixture format cannot carry.
fn setup(caller_text: &str) -> Analysis {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(CALLER, VfsPath::new(abs(CALLER_REL)));
    file_set.insert(GLOBAL, VfsPath::new(abs(GLOBAL_REL)));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (id, text) in [(CALLER, caller_text), (GLOBAL, GLOBAL_BODY)] {
        db.set_file_source_root(id, SourceRootId(0));
        db.set_file_text(id, text);
    }
    db.set_all_config_paths(vec![(None, designer_path())], None);
    Analysis::from_database(db)
}

/// The range of the `nth` (0-based) occurrence of `needle` in `text`.
fn nth(text: &str, needle: &str, nth: usize) -> TextRange {
    let start = text.match_indices(needle).nth(nth).expect("occurrence present").0;
    TextRange::at((start as u32).into(), (needle.len() as u32).into())
}

/// The whole declaration a goto on a method lands on: from its header to its closing keyword.
fn span(text: &str, header: &str, closing: &str) -> TextRange {
    let start = text.find(header).expect("header present");
    let end = start + text[start..].find(closing).expect("closing present") + closing.len();
    TextRange::new((start as u32).into(), (end as u32).into())
}

/// An offset inside the occurrence, past its first character.
fn at(range: TextRange) -> u32 {
    u32::from(range.start()) + 2
}

fn goto(analysis: &Analysis, range: TextRange) -> Option<(FileId, TextRange)> {
    analysis.goto_definition(CALLER, at(range)).map(|target| (target.file_id, target.range))
}

fn hover(analysis: &Analysis, range: TextRange) -> String {
    analysis.hover(CALLER, at(range), Locale::Ru).map(|hover| hover.markup).unwrap_or_default()
}

fn references(analysis: &Analysis, range: TextRange) -> Vec<TextRange> {
    let mut found: Vec<TextRange> = analysis
        .find_references(CALLER, at(range))
        .into_iter()
        .filter(|location| location.file_id == CALLER)
        .map(|location| location.range)
        .collect();
    found.sort_by_key(|range| range.start());
    found
}

fn renamed(analysis: &Analysis, range: TextRange) -> Vec<TextRange> {
    let mut found: Vec<TextRange> = analysis
        .rename(CALLER, at(range), "Другое")
        .expect("renameable")
        .into_iter()
        .filter(|location| location.file_id == CALLER)
        .map(|location| location.range)
        .collect();
    found.sort_by_key(|range| range.start());
    found
}

fn highlighted(analysis: &Analysis, range: TextRange) -> Vec<TextRange> {
    let mut found: Vec<TextRange> = analysis
        .document_highlights(CALLER, at(range))
        .into_iter()
        .map(|highlight| highlight.range)
        .collect();
    found.sort_by_key(|range| range.start());
    found
}

const FUNCTION: &str = "Функция Проба() Экспорт\n\tВозврат \"МЕТОД_МОДУЛЯ\";\nКонецФункции\n\n";

/// Asserts every surface for one variable kind. Occurrence 0 of `Проба` is the function's
/// own name; `variable` is the occurrence that declares or first writes the variable;
/// `call`, `member` and `read` are the call, the receiver of `.Количество()` and a plain
/// value read.
fn assert_call_names_the_method(
    text: &str,
    variable: usize,
    call: usize,
    member: usize,
    read: usize,
) {
    let analysis = setup(text);
    let function = nth(text, "Проба", 0);
    let declaration = span(text, "Функция Проба", "КонецФункции");
    let variable = nth(text, "Проба", variable);
    let call = nth(text, "Проба", call);
    let member = nth(text, "Проба", member);
    let read = nth(text, "Проба", read);

    assert_eq!(goto(&analysis, call), Some((CALLER, declaration)), "the call names the method");
    assert_eq!(goto(&analysis, member), goto(&analysis, read), "member receiver is the variable");
    assert_eq!(goto(&analysis, read), goto(&analysis, variable), "a value read is the variable");
    assert_ne!(goto(&analysis, read), Some((CALLER, declaration)));

    let on_call = hover(&analysis, call);
    assert!(on_call.contains("функция Проба()"), "hover on the call shows the method: {on_call}");

    let method_refs = references(&analysis, function);
    assert!(method_refs.contains(&call), "the method's references hold the call: {method_refs:?}");
    for not_method in [variable, member, read] {
        assert!(!method_refs.contains(&not_method), "{not_method:?} in {method_refs:?}");
    }
    assert_eq!(references(&analysis, call), method_refs, "asked from the call itself");

    let variable_refs = references(&analysis, variable);
    assert!(!variable_refs.contains(&call), "the variable's references skip the call");
    for of_variable in [variable, member, read] {
        assert!(variable_refs.contains(&of_variable), "{of_variable:?} in {variable_refs:?}");
    }

    assert_eq!(renamed(&analysis, function), method_refs, "rename of the method edits the call");
    assert_eq!(renamed(&analysis, variable), variable_refs, "rename of the variable skips it");

    let highlight = highlighted(&analysis, call);
    assert!(highlight.contains(&function) && highlight.contains(&call), "{highlight:?}");
    assert!(!highlight.contains(&read), "{highlight:?}");
    assert!(!highlighted(&analysis, read).contains(&call), "the variable's highlight skips it");
}

#[test]
fn a_parameter_named_like_the_module_function_does_not_take_its_call() {
    let text = format!(
        "{FUNCTION}Процедура П(Проба) Экспорт\n\tХ = Проба();\n\tК = Проба.Количество();\n\
         \tЧ = Проба;\nКонецПроцедуры\n"
    );
    assert_call_names_the_method(&text, 1, 2, 3, 4);
    let analysis = setup(&text);
    let on_call = hover(&analysis, nth(&text, "Проба", 2));
    assert!(!on_call.contains("**Параметр Проба"), "{on_call}");
    let on_read = hover(&analysis, nth(&text, "Проба", 4));
    assert!(on_read.contains("Параметр Проба"), "the control read stays the parameter: {on_read}");
}

#[test]
fn a_body_perem_named_like_the_module_function_does_not_take_its_call() {
    let text = format!(
        "{FUNCTION}Процедура П() Экспорт\n\tПерем Проба;\n\tХ = Проба();\n\
         \tК = Проба.Количество();\n\tЧ = Проба;\nКонецПроцедуры\n"
    );
    assert_call_names_the_method(&text, 1, 2, 3, 4);
}

#[test]
fn a_module_perem_named_like_the_module_function_does_not_take_its_call() {
    let text = format!(
        "Перем Проба;\n\n{FUNCTION}Процедура П() Экспорт\n\tХ = Проба();\n\
         \tК = Проба.Количество();\n\tЧ = Проба;\nКонецПроцедуры\n"
    );
    // The module `Перем` precedes the function here, so occurrence 0 is the variable.
    let analysis = setup(&text);
    let variable = nth(&text, "Проба", 0);
    let function = nth(&text, "Проба", 1);
    let call = nth(&text, "Проба", 2);
    let member = nth(&text, "Проба", 3);
    let read = nth(&text, "Проба", 4);

    let declaration = span(&text, "Функция Проба", "КонецФункции");
    assert_eq!(goto(&analysis, call), Some((CALLER, declaration)));
    let variable_statement = span(&text, "Перем Проба", ";");
    assert_eq!(goto(&analysis, read), Some((CALLER, variable_statement)), "a read is the `Перем`");
    assert_eq!(goto(&analysis, member), Some((CALLER, variable_statement)));

    let method_refs = references(&analysis, function);
    assert_eq!(method_refs, vec![function, call]);
    let variable_refs = references(&analysis, variable);
    assert_eq!(variable_refs, vec![variable, member, read]);
    assert_eq!(renamed(&analysis, function), method_refs);
    assert_eq!(renamed(&analysis, variable), variable_refs);
    assert!(hover(&analysis, call).contains("функция Проба()"), "{}", hover(&analysis, call));
}

#[test]
fn an_implicit_local_named_like_the_module_function_does_not_take_its_call() {
    let text = format!(
        "{FUNCTION}Процедура П() Экспорт\n\tПроба = Проба();\n\tК = Проба.Количество();\n\
         \tЧ = Проба;\nКонецПроцедуры\n"
    );
    assert_call_names_the_method(&text, 1, 2, 3, 4);
}

#[test]
fn a_procedure_statement_call_is_not_taken_by_a_same_named_parameter() {
    let text = "Процедура ПробаП() Экспорт\nКонецПроцедуры\n\n\
                Процедура П(ПробаП) Экспорт\n\tПробаП();\n\tЧ = ПробаП;\nКонецПроцедуры\n";
    let analysis = setup(text);
    let procedure = nth(text, "ПробаП", 0);
    let parameter = nth(text, "ПробаП", 1);
    let call = nth(text, "ПробаП", 2);
    let read = nth(text, "ПробаП", 3);

    let declaration = span(text, "Процедура ПробаП", "КонецПроцедуры");
    assert_eq!(goto(&analysis, call), Some((CALLER, declaration)));
    assert_eq!(goto(&analysis, read), Some((CALLER, parameter)));
    assert_eq!(references(&analysis, procedure), vec![procedure, call]);
    assert_eq!(references(&analysis, parameter), vec![parameter, read]);
    assert_eq!(renamed(&analysis, parameter), vec![parameter, read]);
    assert!(hover(&analysis, call).contains("процедура ПробаП()"), "{}", hover(&analysis, call));
}

#[test]
fn a_parameter_named_like_a_global_export_does_not_take_its_call() {
    let text = "Процедура П(ГлобальнаяСервернаяПроцедура) Экспорт\n\
                \tГлобальнаяСервернаяПроцедура();\n\tЧ = ГлобальнаяСервернаяПроцедура;\n\
                КонецПроцедуры\n";
    let analysis = setup(text);
    let parameter = nth(text, "ГлобальнаяСервернаяПроцедура", 0);
    let call = nth(text, "ГлобальнаяСервернаяПроцедура", 1);
    let read = nth(text, "ГлобальнаяСервернаяПроцедура", 2);

    let export = nth(GLOBAL_BODY, "ГлобальнаяСервернаяПроцедура", 0);
    let declaration = span(GLOBAL_BODY, "Процедура", "КонецПроцедуры");
    assert_eq!(goto(&analysis, call), Some((GLOBAL, declaration)), "the call names the export");
    assert_eq!(goto(&analysis, read), Some((CALLER, parameter)));
    assert_eq!(references(&analysis, parameter), vec![parameter, read]);

    let export_refs: Vec<(FileId, TextRange)> = analysis
        .find_references(GLOBAL, at(export))
        .into_iter()
        .map(|location| (location.file_id, location.range))
        .collect();
    assert!(export_refs.contains(&(CALLER, call)), "the export's references: {export_refs:?}");
    assert!(!export_refs.contains(&(CALLER, read)), "{export_refs:?}");
}

#[test]
fn a_parameter_named_like_a_platform_function_does_not_take_its_call() {
    let text = "Процедура П(СтрДлина) Экспорт\n\tХ = СтрДлина(\"abcd\");\n\tЧ = СтрДлина;\n\
                КонецПроцедуры\n";
    let analysis = setup(text);
    let parameter = nth(text, "СтрДлина", 0);
    let call = nth(text, "СтрДлина", 1);
    let read = nth(text, "СтрДлина", 2);

    let on_call = hover(&analysis, call);
    assert!(on_call.contains("**Глобальная функция:** СтрДлина"), "{on_call}");
    assert!(!on_call.contains("**Параметр СтрДлина"), "{on_call}");
    assert_ne!(goto(&analysis, call), Some((CALLER, parameter)));
    assert_eq!(goto(&analysis, read), Some((CALLER, parameter)));
    assert_eq!(references(&analysis, parameter), vec![parameter, read]);
    assert_eq!(renamed(&analysis, parameter), vec![parameter, read]);
    assert!(hover(&analysis, read).contains("Параметр СтрДлина"), "{}", hover(&analysis, read));
}

/// A name only a variable holds, called: the module does not compile (checked live), and
/// `UnresolvedBareCall` reports it. Navigation keeps naming the variable there — there is no
/// method to name, and a variable is what the reader sees declared under that name.
#[test]
fn a_call_no_method_owns_keeps_naming_the_variable() {
    let text = "Процедура П(НетТакойФункции) Экспорт\n\tНетТакойФункции();\nКонецПроцедуры\n";
    let analysis = setup(text);
    let parameter = nth(text, "НетТакойФункции", 0);
    let call = nth(text, "НетТакойФункции", 1);

    assert_eq!(goto(&analysis, call), Some((CALLER, parameter)));
    assert_eq!(references(&analysis, parameter), vec![parameter, call]);
}
