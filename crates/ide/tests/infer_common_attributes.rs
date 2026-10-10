//! Common attributes (`ОбщиеРеквизиты`) in the object model: a field of every object, reference
//! and register record their composition reaches, although no object XML declares it.
//!
//! Fixture composition: the separator `ОбластьДанныхОсновныеДанные` reaches everything except
//! `СправочникБезРазделения`; `Организация` reaches only `Справочник1`, `Документ1` and
//! `РегистрСведений1`; `ОбщийКомментарий` reaches everything except `Документ1`.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;

fn unresolved_fields(code: &str) -> Vec<String> {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{code}"));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(
        None,
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/common_attributes"
        )),
    )]);
    let (file_id, _) = fixture.files.iter().next().expect("one file");

    db.infer(*file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_reference_and_an_object_carry_their_common_attributes() {
    let code = r#"
Процедура Тест()
    С = Справочники.Справочник1.ПустаяСсылка();
    А = С.Организация;
    Б = С.ОбластьДанныхОсновныеДанные;
    В = С.ОбщийКомментарий;
    Г = С.Реквизит1;
    Д = Документы.Документ1.СоздатьДокумент();
    Д.Организация = А;
    Д.ОбластьДанныхОсновныеДанные = 0;
    Е = С.НетТакогоПоля;
КонецПроцедуры
"#;
    // The deliberate miss proves the receivers are typed, so the silence above is a verdict.
    assert_eq!(unresolved_fields(code), ["НетТакогоПоля"]);
}

#[test]
fn an_object_outside_the_composition_has_no_such_field() {
    let code = r#"
Процедура Тест()
    С = Справочники.СправочникБезРазделения.ПустаяСсылка();
    А = С.Организация;
    Б = С.ОбластьДанныхОсновныеДанные;
    В = С.ОбщийКомментарий;
    Д = Документы.Документ1.СоздатьДокумент();
    Г = Д.ОбщийКомментарий;
КонецПроцедуры
"#;
    let mut found = unresolved_fields(code);
    found.sort();
    assert_eq!(found, ["ОбластьДанныхОсновныеДанные", "ОбщийКомментарий", "Организация"]);
}

#[test]
fn a_register_record_carries_the_separator_and_its_filter_names_it() {
    let code = r#"
Процедура Тест()
    Набор = РегистрыНакопления.РегистрНакопления1.СоздатьНаборЗаписей();
    Набор.Отбор.ОбластьДанныхОсновныеДанные.Установить(1);
    Запись = Набор.Добавить();
    Запись.ОбластьДанныхОсновныеДанные = 1;
    Запись.ОбщийКомментарий = "";
    Запись.Измерение1 = "";
    Х = Запись.Организация;
КонецПроцедуры
"#;
    assert_eq!(unresolved_fields(code), ["Организация"]);
}
