use hir::{Builders, DefDatabase, HirDatabase, ModuleId, TypeId};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
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
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    let _ = db.module_bodies(ModuleId::new(test_file));
    (db, test_file)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn helper_returning_refined_query_propagates_projection_to_caller() {
    let fixture = r#"//- /test.bsl
Функция СоздатьЗапрос() Экспорт
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    Возврат Зап;
КонецФункции

Функция Тест()
    Х = СоздатьЗапрос().Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "Phase F target: dataflow-refined query projection propagates through helper return",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn helper_with_constructor_literal_propagates_projection() {
    let fixture = r#"//- /test.bsl
Функция СоздатьЗапрос() Экспорт
    Возврат Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
КонецФункции

Функция Тест()
    Х = СоздатьЗапрос().Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "constructor-time projection must propagate through helper's return type",
    );
}

#[test]
fn divergent_text_writes_collapse_to_unknown() {
    let fixture = r#"//- /test.bsl
Функция СоздатьЗапрос(Флаг) Экспорт
    Зап = Новый Запрос;
    Если Флаг Тогда
        Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    Иначе
        Зап.Текст = "ВЫБРАТЬ ""def"" КАК Цена";
    КонецЕсли;
    Возврат Зап;
КонецФункции

Функция Тест(Флаг)
    Х = СоздатьЗапрос(Флаг).Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        None,
        "divergent reaching writes must collapse the chain so `Х` stays unrefined — \
         conservative var_types drops Unknown RHS, leaving the binding absent",
    );
}

#[test]
fn dynamic_text_write_collapses_to_unknown() {
    let fixture = r#"//- /test.bsl
Функция ПолучитьТекст()
    Возврат "ВЫБРАТЬ ""abc"" КАК Имя";
КонецФункции

Функция СоздатьЗапрос() Экспорт
    Зап = Новый Запрос;
    Зап.Текст = ПолучитьТекст();
    Возврат Зап;
КонецФункции

Функция Тест()
    Х = СоздатьЗапрос().Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        None,
        "dynamic text RHS collapses the projection and the trailing `.Имя` resolves \
         to Unknown — conservative var_types drops Unknown so `Х` stays absent",
    );
}

#[test]
fn no_text_write_keeps_projection_none() {
    let fixture = r#"//- /test.bsl
Функция СоздатьЗапрос() Экспорт
    Зап = Новый Запрос;
    Возврат Зап;
КонецФункции

Функция Тест()
    Х = СоздатьЗапрос().Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        None,
        "no `.Текст` write keeps the projection unresolved and the trailing `.Имя` \
         types as Unknown — conservative var_types drops the Unknown so `Х` is absent",
    );
}

#[test]
#[ignore = "Phase F follow-up: loop-carried `.Текст += ...` append idiom needs \
            string-concatenation reasoning in projection_from_text_assignment; \
            today the +=-style write fails the `Expr::Literal(String)` gate and \
            collapses to None (acceptable, but no caller-side wire-through yet)."]
fn loop_carried_text_append_recovers_to_projection() {
    let fixture = r#"//- /test.bsl
Функция СоздатьЗапрос(Условия) Экспорт
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя ИЗ Справочник.Товары ГДЕ ";
    Для Каждого У Из Условия Цикл
        Зап.Текст = Зап.Текст + " И " + У;
    КонецЦикла;
    Возврат Зап;
КонецФункции

Функция Тест(Условия)
    Х = СоздатьЗапрос(Условия).Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "Phase F follow-up target: loop-append builder recovers projection",
    );
}

#[test]
#[ignore = "Phase F follow-up: parameter-as-Query has no module-local reaching \
            def for the param binding's `.Текст`, so refinement returns None. \
            Closing this gap needs callee/caller cross-method dataflow."]
fn parameter_query_with_text_write_in_caller_propagates() {
    let fixture = r#"//- /test.bsl
Функция Выполнить(Зап) Экспорт
    Возврат Зап.Выполнить().Выбрать().Имя;
КонецФункции

Функция Тест()
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    Х = Выполнить(Зап);
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "Phase F follow-up target: param-as-Query refinement across call boundary",
    );
}

#[test]
#[ignore = "Phase F follow-up: a binding name that collides with a CFE-visible \
            symbol needs scope resolution before refinement; today the gate \
            only checks var_types and may miss shadowing. Out of Phase F scope."]
fn cfe_shadowed_binding_refines_through_local_only() {
    let fixture = r#"//- /test.bsl
Функция СоздатьЗапрос() Экспорт
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ ""abc"" КАК Имя";
    Возврат Запрос;
КонецФункции

Функция Тест()
    Х = СоздатьЗапрос().Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "Phase F follow-up: refinement under name shadowing CFE-visible globals",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn batched_text_assignment_picks_last_query_projection() {
    let fixture = r#"//- /test.bsl
Функция ПолучитьТЗ() Экспорт
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ 1 КАК Игнор ПОМЕСТИТЬ ВТ; ВЫБРАТЬ ""abc"" КАК Имя ИЗ ВТ КАК ВТ";
    Возврат Зап.Выполнить().Выгрузить();
КонецФункции

Функция Тест()
    Для Каждого Стр Из ПолучитьТЗ() Цикл
        Х = Стр.Имя;
        Возврат Х;
    КонецЦикла;
    Возврат Неопределено;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "batched query: last-SELECT's `КАК Имя` projection must reach the iteration row",
    );
}

#[test]
fn batched_text_assignment_drops_staging_columns() {
    let fixture = r#"//- /test.bsl
Функция ПолучитьТЗ() Экспорт
    Зап = Новый Запрос;
    Зап.Текст = "ВЫБРАТЬ 1 КАК Игнор ПОМЕСТИТЬ ВТ; ВЫБРАТЬ ""abc"" КАК Имя ИЗ ВТ КАК ВТ";
    Возврат Зап.Выполнить().Выгрузить();
КонецФункции

Функция Тест()
    Для Каждого Стр Из ПолучитьТЗ() Цикл
        Х = Стр.Игнор;
        Возврат Х;
    КонецЦикла;
    Возврат Неопределено;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        None,
        "staging-only column `Игнор` must not leak into the final SELECT's projection",
    );
}
