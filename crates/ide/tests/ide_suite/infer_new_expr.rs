use hir::{Builders, DefDatabase, HirDatabase, ModuleId, TypeId, TypeKernelDb, TypeKind};
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

fn query_no_projection(db: &RootDatabaseImpl, ty: TypeId) -> bool {
    match db.lookup_type(ty) {
        TypeKind::Query { projections } => projections.iter().all(Option::is_none),
        _ => false,
    }
}

#[test]
fn new_array_gives_array_ty() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Х = Новый Массив();
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.array(None)),
        "`Новый Массив` must type the RHS as Ty::Array"
    );
}

#[test]
fn new_query_with_no_args_types_as_query_with_no_projection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Х = Новый Запрос();
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    assert!(
        query_no_projection(&db, ty),
        "`Новый Запрос()` without literal text must produce Ty::Query with no projection, got {ty:?}",
    );
}

#[test]
fn new_query_with_dynamic_text_types_as_query_with_no_projection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Текст = "ВЫБРАТЬ 1";
    Х = Новый Запрос(Текст);
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    assert!(
        query_no_projection(&db, ty),
        "`Новый Запрос(<variable>)` must produce Ty::Query with no projection, got {ty:?}",
    );
}

#[test]
fn new_query_with_literal_text_types_as_query_with_projection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Х = Новый Запрос("ВЫБРАТЬ 1 КАК А");
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    let projections = match db.lookup_type(ty) {
        TypeKind::Query { projections } => projections.clone(),
        other => panic!("expected Ty::Query, got {other:?}"),
    };
    assert_eq!(
        projections.len(),
        1,
        "single-query package must yield one slice entry, got {projections:?}",
    );
    let projection = projections[0].as_ref().expect("literal SDBL must produce a projection");
    assert_eq!(
        projection.fields.len(),
        1,
        "single-column SELECT must yield one projection field, got {projection:?}",
    );
    assert_eq!(projection.fields[0].name.as_str(), "А");
    assert_eq!(projection.fields[0].ty, db.number(None, None));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn new_query_chain_propagates_projection_through_execute_select() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ ""abc"" КАК Имя");
    Х = ЗапросОбъект.Выполнить().Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "query.Execute().Select().Имя must resolve to Ty::String",
    );
}

#[test]
fn new_query_with_parse_error_literal_falls_back_to_no_projection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Х = Новый Запрос("это не sdbl");
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    assert!(
        query_no_projection(&db, ty),
        "parse-error SDBL literal must collapse to Ty::Query with no projection, not PlatformObject — got {ty:?}",
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn execute_batch_literal_zero_index_yields_first_subquery_projection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ 1 КАК ПерваяКолонка; ВЫБРАТЬ ""abc"" КАК ВтораяКолонка");
    Х = ЗапросОбъект.ВыполнитьПакет()[0];
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    let projection = match db.lookup_type(ty) {
        TypeKind::QueryResult(facet) => facet.projection.as_ref(),
        other => panic!("expected Ty::QueryResult, got {other:?}"),
    };
    let projection = projection.expect("batch[0] must carry the first sub-query's projection");
    assert_eq!(projection.fields.len(), 1);
    assert_eq!(projection.fields[0].name.as_str(), "ПерваяКолонка");
    assert_eq!(projection.fields[0].ty, db.number(None, None));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn execute_batch_literal_one_index_yields_second_subquery_projection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ 1 КАК ПерваяКолонка; ВЫБРАТЬ ""abc"" КАК ВтораяКолонка");
    Х = ЗапросОбъект.ВыполнитьПакет()[1];
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    let projection = match db.lookup_type(ty) {
        TypeKind::QueryResult(facet) => facet.projection.as_ref(),
        other => panic!("expected Ty::QueryResult, got {other:?}"),
    };
    let projection = projection.expect("batch[1] must carry the second sub-query's projection");
    assert_eq!(projection.fields[0].name.as_str(), "ВтораяКолонка");
    assert_eq!(projection.fields[0].ty, db.string(None, false));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn execute_batch_out_of_range_index_yields_no_projection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ 1 КАК А");
    Х = ЗапросОбъект.ВыполнитьПакет()[5];
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::QueryResult(facet) if facet.projection.is_none()),
        "out-of-range batch index must yield Ty::QueryResult{{None}}, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn execute_batch_dynamic_index_yields_no_projection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Индекс = 0;
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ 1 КАК А");
    Х = ЗапросОбъект.ВыполнитьПакет()[Индекс];
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::QueryResult(facet) if facet.projection.is_none()),
        "non-literal batch index must yield Ty::QueryResult{{None}}, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn execute_batch_chain_propagates_through_select() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ЗапросОбъект = Новый Запрос("ВЫБРАТЬ 1 КАК А; ВЫБРАТЬ ""abc"" КАК Имя");
    Х = ЗапросОбъект.ВыполнитьПакет()[1].Выбрать().Имя;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "batch[1].Выбрать().Имя must resolve to Ty::String",
    );
}

/// Строит `Новый <статическая форма>` и `Новый(<динамическая форма>)` в двух
/// отдельных базах и возвращает выведенные типы для сравнения.
fn static_and_dynamic_ctor_ty(static_form: &str, dynamic_form: &str) -> (TypeId, TypeId) {
    let build = |ctor: &str| {
        let fixture = format!(
            "//- /test.bsl\nФункция Тест()\n    Х = Новый {ctor};\n    Возврат Х;\nКонецФункции\n"
        );
        let (db, file_id) = setup(&fixture);
        let ty = var_ty(&db, file_id, "х")
            .unwrap_or_else(|| panic!("`Новый {ctor}` must infer a type for х"));
        (db, ty)
    };
    let (static_db, static_ty) = build(static_form);
    let (dynamic_db, dynamic_ty) = build(&format!("({dynamic_form})"));
    assert_eq!(
        static_db.lookup_type(static_ty),
        dynamic_db.lookup_type(dynamic_ty),
        "`Новый {static_form}` and `Новый({dynamic_form})` must agree on the constructed type"
    );
    (static_ty, dynamic_ty)
}

#[test]
fn new_with_string_literal_name_agrees_with_the_syntactic_form() {
    static_and_dynamic_ctor_ty("ТаблицаЗначений", r#""ТаблицаЗначений""#);
}

#[test]
fn new_with_type_call_name_agrees_with_the_syntactic_form() {
    static_and_dynamic_ctor_ty("ТаблицаЗначений", r#"Тип("ТаблицаЗначений")"#);
}

#[test]
fn new_with_english_type_call_name_agrees_with_the_syntactic_form() {
    static_and_dynamic_ctor_ty("Массив", r#"Type("Массив")"#);
}

/// Пробелы вокруг имени не должны разводить формы: `Запрос` распознаётся до
/// понижения, поэтому обрезка обязана случиться раньше обеих проверок.
#[test]
fn new_with_padded_dynamic_name_agrees_with_the_syntactic_form() {
    static_and_dynamic_ctor_ty("Запрос", r#"" Запрос ""#);
    static_and_dynamic_ctor_ty("Массив", r#"Тип("  Массив  ")"#);
}

#[test]
fn new_with_dynamic_query_name_types_as_query() {
    for ctor in [r#"("Запрос")"#, r#"(Тип("Запрос"))"#] {
        let fixture = format!(
            "//- /test.bsl\nФункция Тест()\n    Х = Новый {ctor};\n    Возврат Х;\nКонецФункции\n"
        );
        let (db, file_id) = setup(&fixture);
        let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
        assert!(
            query_no_projection(&db, ty),
            "`Новый {ctor}` must produce Ty::Query so it cannot contradict `Новый Запрос`, got {ty:?}",
        );
    }
}

#[test]
fn new_with_qualified_string_name_types_as_metadata_ref() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Х = Новый(Тип("СправочникСсылка.Товары"));
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "х").expect("х must be inferred");
    match db.lookup_type(ty) {
        TypeKind::MetadataRef(facet) => {
            assert_eq!(facet.name.as_str(), "Товары");
        }
        other => panic!(
            "a qualified name is only expressible through the string form, so it must still \
             produce a metadata reference; got {other:?}"
        ),
    }
}

/// Имя, пришедшее значением, — гипотеза, а не утверждение автора: нераспознанное
/// остаётся `Unknown`, потому что номинальный фантом в позиции аргумента даёт
/// только ложный `TypeMismatch`.
#[test]
fn new_with_unrecognised_dynamic_name_stays_unknown() {
    let infer_ctor = |ctor: &str| {
        let fixture = format!(
            "//- /test.bsl\nФункция Тест(ИмяТипа, ИмяКомпоненты)\n    Х = Новый {ctor};\n    Возврат Х;\nКонецФункции\n"
        );
        let (db, file_id) = setup(&fixture);
        // Инференс не заносит Unknown в `var_types`, поэтому отсутствие записи и
        // есть «о переменной ничего не известно».
        let ty = var_ty(&db, file_id, "х").unwrap_or_else(|| db.unknown());
        (db.unknown(), ty)
    };

    for ctor in [
        r#"("AddIn.CLON.DbControl")"#,
        r#"("Addin.ЭДОNative.CryptS")"#,
        r#"("ЗаведомоНесуществующийТип")"#,
        r#"(Тип("ЗаведомоНесуществующийТип"))"#,
        // Семейства из `is_known_non_corpus_type_name` угадываются по префиксу и
        // суффиксу — для имени-из-значения этого мало.
        r#"("ЗаведомоНесуществующийDOM")"#,
        r#"("ОбъектМетаданныхЗаведомоНесуществующий")"#,
        r#"("")"#,
        "(ИмяТипа)",
        r#"(Тип(ИмяТипа))"#,
        r#"("AddIn." + ИмяКомпоненты)"#,
    ] {
        let (unknown, ty) = infer_ctor(ctor);
        assert_eq!(ty, unknown, "`Новый {ctor}` names no recognisable type and must stay Unknown");
    }

    // Контроль: та же фикстура с распознаваемым именем типа обязана дать не-Unknown,
    // иначе проверки выше проходили бы и на сломанной фикстуре.
    let (unknown, ty) = infer_ctor(r#"("ТаблицаЗначений")"#);
    assert_ne!(ty, unknown, "the control case must be typed, otherwise the fixture proves nothing");
}

/// Курируемый список реальных типов вне корпуса — тот же источник истины, что и
/// у деградации фантома в аннотации: обе формы обязаны его видеть одинаково.
#[test]
fn new_with_known_non_corpus_name_agrees_with_the_syntactic_form() {
    static_and_dynamic_ctor_ty("УправляемаяФорма", r#""УправляемаяФорма""#);
    static_and_dynamic_ctor_ty("СтрокаТабличнойЧасти", r#"Тип("СтрокаТабличнойЧасти")"#);
}

/// Имя из значения принимается только по точно поименованным типам. Всё, что
/// корпус и точные списки лишь угадывают, остаётся `Unknown`: семейства
/// XDTO/XML/DOM опознаются по форме «кириллический корень + латинский тег», где
/// `ТипXDTO` неотличим от `ЗаведомоНесуществующийDOM`, а произведение видов
/// регистров на суффиксы записи содержит несуществующие сочетания (у регистра
/// накопления нет менеджера записи, у последовательности — ключа записи).
///
/// Размен сознательный и закреплён тестом, чтобы его не «починили» обратно:
/// несовпадение со статической формой стоит невыведенного типа, который никто не
/// видит, а обратный выбор — ложного `TypeMismatch` на фантоме, который видят все.
#[test]
fn new_with_family_guessed_name_stays_unknown_in_the_string_form() {
    for ctor in [
        r#"("ТипXDTO")"#,
        r#"("РегистрНакопленияМенеджерЗаписи")"#,
        r#"("ПоследовательностьКлючЗаписи")"#,
    ] {
        let fixture = format!(
            "//- /test.bsl\nФункция Тест()\n    Х = Новый {ctor};\n    Возврат Х;\nКонецФункции\n"
        );
        let (db, file_id) = setup(&fixture);
        assert_eq!(
            var_ty(&db, file_id, "х"),
            None,
            "`Новый {ctor}` names a guessed family, not an exactly known type"
        );
    }
}

/// Модульная переменная имя вызова не перехватывает: `Тип("Массив")` при `Перем Тип;`
/// в модуле - по-прежнему платформенная функция.
#[test]
fn module_variable_named_type_leaves_the_type_name_wrapper() {
    let fixture = r#"//- /test.bsl
Перем Тип;
Функция Тест()
    Х = Новый(Тип("Массив"));
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.array(None)),
        "a module variable does not take the call name, so the literal still names the type"
    );

    let control = r#"//- /test.bsl
Функция Тест()
    Х = Новый(Тип("Массив"));
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(control);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.array(None)),
        "control: with no shadowing declaration the platform wrapper is still unwrapped"
    );
}

#[test]
fn new_structure_gives_structure_ty() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Х = Новый Структура();
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.structure(None)),
        "`Новый Структура` must type the RHS as Ty::Structure"
    );
}
