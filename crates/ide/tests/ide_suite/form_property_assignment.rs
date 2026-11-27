use hir::{HirDatabase, TypeKernelDb, TypeKind};
use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

const REPORTED: &str = include_str!("fixtures/form_property_assignment.bsl");

fn setup(source: &str, form: &str) -> (RootDatabaseImpl, FileId) {
    let root =
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"));
    let module = root.join(format!("Catalogs/Справочник1/Forms/{form}/Ext/Form/Module.bsl"));
    assert!(module.exists());
    let mut db = RootDatabaseImpl::new();
    let file = FileId(0);
    let mut files = FileSet::default();
    files.insert(file, VfsPath::new(module.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file, SourceRootId(0));
    db.set_file_text(file, source);
    db.set_all_config_paths(vec![(None, root)], None);
    (db, file)
}

fn mismatches(db: &RootDatabaseImpl, file: FileId) -> Vec<ide_diagnostics::Diagnostic> {
    ide_diagnostics::file_diagnostics(db, file, &DiagnosticsConfig::default())
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::TypeMismatch)
        .collect()
}

#[test]
fn managed_form_bare_write_rejects_value_list_item_even_with_optional_undefined() {
    let (db, file) = setup(REPORTED, "ФормаЭлемента");
    let inference = db.infer(file);
    assert!(inference
        .implicit_locals_by_body
        .values()
        .all(|locals| !locals.contains_key("текущийэлемент")));
    let diagnostics = mismatches(&db, file);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].message.contains("ЭлементСпискаЗначений"), "{diagnostics:?}");
    assert!(diagnostics[0].message.contains("Неопределено"), "{diagnostics:?}");
    let range = diagnostics[0].range;
    assert_eq!(
        &REPORTED[usize::from(range.start())..usize::from(range.end())],
        "Список.НайтиПоЗначению(\"\")"
    );
}

#[test]
fn form_property_reads_hover_and_navigation_keep_the_property_identity() {
    let source = format!(
        "{}\n&НаКлиенте\nПроцедура Прочитать()\n    Сообщить(ТекущийЭлемент);\nКонецПроцедуры\n",
        REPORTED
    );
    let (db, file) = setup(&source, "ФормаЭлемента");
    let analysis = Analysis::from_database(db);
    for (offset, _) in source.match_indices("ТекущийЭлемент") {
        let hover = analysis.hover(file, offset as u32, ide::Locale::Ru).expect("property hover");
        assert!(hover.markup.contains("ПолеФормы"), "{}", hover.markup);
        assert!(!hover.markup.contains("ЭлементСпискаЗначений"), "{}", hover.markup);
        assert!(analysis.prepare_rename(file, offset as u32).is_none());
        assert!(analysis.find_references(file, offset as u32).is_empty());
    }
}

#[test]
fn real_form_item_and_permitted_undefined_are_valid_writes() {
    for directive in ["НаКлиенте", "НаСервере"] {
        let source = format!("&{directive}\nПроцедура Тест()\n    ТекущийЭлемент = Элементы.Код;\n    ТекущийЭлемент = Неопределено;\nКонецПроцедуры");
        let (db, file) = setup(&source, "ФормаЭлемента");
        assert!(mismatches(&db, file).is_empty());
        assert!(db
            .infer(file)
            .implicit_locals_by_body
            .values()
            .all(|locals| !locals.contains_key("текущийэлемент")));
    }
}

#[test]
fn different_local_and_explicit_binding_remain_locals_and_renameable() {
    for (name, declaration) in
        [("ВыбранныйЭлемент", ""), ("ТекущийЭлемент", "Перем ТекущийЭлемент;\n")]
    {
        let source = format!("&НаКлиенте\nПроцедура Тест()\n{declaration}    Список = Новый СписокЗначений;\n    {name} = Список.НайтиПоЗначению(\"\");\n    Сообщить({name});\nКонецПроцедуры");
        let (db, file) = setup(&source, "ФормаЭлемента");
        assert!(mismatches(&db, file).is_empty());
        let analysis = Analysis::from_database(db);
        let offset = source.find(&format!("{name} =")).unwrap() as u32;
        let read =
            source.find(&format!("Сообщить({name})")).unwrap() as u32 + "Сообщить(".len() as u32;
        if declaration.is_empty() {
            // A managed form's implicit member surface is never attested complete, even
            // with bundled facts, so an undeclared write target may still be a form
            // member. The write token therefore stays an indeterminate bare name by
            // design — the verdict the accepted corpus numbers were measured with — and
            // not a lost check: the read side below still proves what the assignment
            // holds, and the name stays renameable.
            assert!(
                analysis.hover(file, offset, ide::Locale::Ru).is_none(),
                "an unattested form surface cannot prove a local whose write may be a member"
            );
        } else {
            // An explicitly declared local is a local at its write token, but the write
            // itself carries no flow type: inference types the read off the assignment.
            // The read assertion below is what proves the assigned type.
            let hover =
                analysis.hover(file, offset, ide::Locale::Ru).expect("declared local hover");
            assert!(
                hover.markup.contains("Локальная переменная ТекущийЭлемент"),
                "{}",
                hover.markup
            );
        }
        let read_hover = analysis.hover(file, read, ide::Locale::Ru).expect("local read hover");
        assert!(read_hover.markup.contains("ЭлементСпискаЗначений"), "{}", read_hover.markup);
        assert!(analysis.prepare_rename(file, offset).is_some());
        assert_eq!(
            analysis.rename(file, offset, "Выбор").unwrap().len(),
            if declaration.is_empty() { 2 } else { 3 }
        );
    }
}

#[test]
fn ordinary_form_and_context_free_method_do_not_acquire_a_managed_form_property() {
    for (source, form) in [
        (REPORTED.to_string(), "ФормаОбычная"),
        (REPORTED.replace("НаКлиенте", "НаСервереБезКонтекста"), "ФормаЭлемента"),
    ] {
        let (db, file) = setup(&source, form);
        assert!(mismatches(&db, file).is_empty());
        assert!(db
            .infer(file)
            .implicit_locals_by_body
            .values()
            .any(|locals| locals.contains_key("текущийэлемент")));
    }
}

#[test]
fn qualified_and_bilingual_properties_use_the_same_write_contract() {
    let source = "&AtClient\nProcedure Test()\n    List = New ValueList;\n    CurrentItem = List.FindByValue(\"\");\n    ThisObject.CurrentItem = List.FindByValue(\"\");\n    Title = List.Add(\"\");\nEndProcedure";
    let (db, file) = setup(source, "ФормаЭлемента");
    assert_eq!(mismatches(&db, file).len(), 3);
}

#[test]
fn unknown_values_and_readonly_property_keep_their_existing_verdicts() {
    let source = "&НаКлиенте\nПроцедура Тест(Значение)\n    ТекущийЭлемент = Значение;\n    Элементы = Новый СписокЗначений;\nКонецПроцедуры";
    let (db, file) = setup(source, "ФормаЭлемента");
    let diagnostics = ide_diagnostics::file_diagnostics(&db, file, &DiagnosticsConfig::default());
    assert_eq!(
        diagnostics.iter().filter(|d| d.code == DiagnosticCode::ReadOnlyPropertyAssignment).count(),
        1,
        "{diagnostics:?}"
    );
    assert!(diagnostics.iter().all(|d| d.code != DiagnosticCode::TypeMismatch), "{diagnostics:?}");
    assert!(db
        .infer(file)
        .expr_types_by_body
        .values()
        .flat_map(|types| types.values())
        .any(|ty| matches!(db.lookup_type(*ty), TypeKind::Unknown)));
}

#[test]
fn excluded_or_unproved_preprocessor_branches_do_not_prove_invalid_writes() {
    let source = "&НаКлиенте\nПроцедура Тест()\n#Если Сервер Тогда\n    ТекущийЭлемент = Новый СписокЗначений;\n#КонецЕсли\n#Если НеизвестныйСимвол Тогда\n    ТекущийЭлемент = Новый СписокЗначений;\n#КонецЕсли\n#Если Клиент Тогда\n    ТекущийЭлемент = Новый СписокЗначений;\n#КонецЕсли\nКонецПроцедуры";
    let (db, file) = setup(source, "ФормаЭлемента");
    let diagnostics = mismatches(&db, file);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(source[..usize::from(diagnostics[0].range.start())].lines().count(), 10);
}
