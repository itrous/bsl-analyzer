//! What a slot documented as `см. Модуль.Метод` resolves to, checked at the query itself.
//!
//! Every fixture puts the fields in the TARGET's documentation, never in its body. Keys a body
//! proves already reach a caller through the ordinary signature, so a probe resting on them would
//! be green on a build where this feature does not exist at all.

use hir::{
    doc_see_signature_query, Builders, DefDatabase, DocSeeSignature, MethodIdInput, ModuleId, Name,
    TypeId, TypeKernelDb, TypeKind,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

/// Builds the fixture and returns the `FileId` of `/test.bsl`, chosen by path rather than by
/// insertion order so the analysed file does not depend on hash ordering.
fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = vfs::FileSet::default();
    let mut test_file = None;
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
        if file.path.as_path().to_string_lossy().ends_with("/test.bsl") {
            test_file = Some(*file_id);
        }
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    (db, test_file.expect("fixture must contain /test.bsl"))
}

/// The `FileId` of a fixture file chosen by the tail of its path.
fn file_ending_with(fixture_text: &str, suffix: &str) -> FileId {
    let fixture = Fixture::parse(fixture_text);
    *fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(suffix))
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("фикстура должна содержать файл, кончающийся на {suffix}"))
}

fn resolved(db: &RootDatabaseImpl, file_id: FileId, method: &str) -> DocSeeSignature {
    let module = ModuleId::new(file_id);
    let symbol_tree = db.symbol_tree_ref(module);
    let symbol = symbol_tree
        .find_method(&Name::new(method))
        .unwrap_or_else(|| panic!("фикстура должна объявлять метод {method}"));
    doc_see_signature_query(db, MethodIdInput::new(db, symbol.id)).as_ref().clone()
}

/// The index of a parameter by name, so a test names what it means rather than a position.
fn param_index(db: &RootDatabaseImpl, file_id: FileId, method: &str, param: &str) -> usize {
    let symbol_tree = db.symbol_tree_ref(ModuleId::new(file_id));
    let symbol = symbol_tree.find_method(&Name::new(method)).expect("метод объявлен");
    symbol
        .params
        .iter()
        .position(|p| p.name.as_str().eq_ignore_ascii_case(param) || p.name.as_str() == param)
        .unwrap_or_else(|| panic!("у метода {method} нет параметра {param}"))
}

/// The field names a type carries directly, sorted; empty when it carries none.
fn field_names(db: &RootDatabaseImpl, ty: TypeId) -> Vec<String> {
    let TypeKind::Structure(facet) = db.lookup_type(ty) else {
        return Vec::new();
    };
    let Some(projection) = facet.fields.as_ref() else {
        return Vec::new();
    };
    let mut names: Vec<String> = projection.fields.iter().map(|f| f.name.clone()).collect();
    names.sort();
    names
}

fn union_members(db: &RootDatabaseImpl, ty: TypeId) -> Vec<TypeId> {
    match db.lookup_type(ty) {
        TypeKind::Union(members) => members.to_vec(),
        _ => vec![ty],
    }
}

/// A target whose documentation declares two fields, plus one that declares none.
const TARGETS: &str = r#"
//- /CommonModules/База/Ext/Module.bsl
// Возвращаемое значение:
//   Структура:
//    * Таймаут - Число - секунды.
//    * Адрес - Строка - куда.
Функция Создать() Экспорт
	Возврат Новый Структура;
КонецФункции

// Возвращаемое значение:
//   Строка - просто строка.
Функция Строкой() Экспорт
	Возврат "";
КонецФункции

// Параметры:
//   Настройки - Структура:
//    * Режим - Строка - режим.
Процедура Настроить(Настройки) Экспорт
КонецПроцедуры

// Возвращаемое значение:
//   Структура:
//    * Скрытое - Строка - поле.
Функция Скрытый()
	Возврат Новый Структура;
КонецФункции
"#;

#[test]
fn a_reference_gives_the_slot_the_fields_its_target_documents() {
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Параметры - см. База.Создать
// Возвращаемое значение:
//   см. База.Создать
Функция Читает(Параметры) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let signature = resolved(&db, file_id, "Читает");

    let ret = signature.ret.expect("слот возврата разрешён");
    assert_eq!(field_names(&db, ret), vec!["Адрес", "Таймаут"]);
    let param = signature.param(0).expect("слот параметра разрешён");
    assert_eq!(field_names(&db, param), vec!["Адрес", "Таймаут"]);
}

#[test]
fn a_container_slot_gives_the_fields_its_target_documents() {
    // `Структура - см. Модуль.Метод`: the marker stands in the description, where the signature's
    // own parser does not look, so the slot arrives as an untyped structure and the reference has
    // to fill it — 1 244 such slots in one configuration.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Параметры - Структура - см. База.Создать
// Возвращаемое значение:
//   Структура - см. База.Создать
Функция Читает(Параметры) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let signature = resolved(&db, file_id, "Читает");

    let param = signature.param(0).expect("слот параметра разрешён");
    assert_eq!(field_names(&db, param), vec!["Адрес", "Таймаут"]);
    let ret = signature.ret.expect("слот возврата разрешён");
    assert_eq!(field_names(&db, ret), vec!["Адрес", "Таймаут"]);
}

#[test]
fn a_return_union_keeps_the_arm_documented_beside_the_reference() {
    // The return half of `Неопределено, см. X.Y`: the union must survive as a slot, and the
    // reference arm must reach the caller. Asserted on the decomposition, in both orders — the
    // link-first order is the one the hyperlink branch used to swallow whole.
    for (line, order) in [
        ("//   Неопределено, см. База.Создать - необязательные.", "ссылка последней"),
        ("//   см. База.Создать, Неопределено - необязательные.", "ссылка первой"),
    ] {
        let fixture = format!(
            "{TARGETS}
//- /test.bsl
// Возвращаемое значение:
{line}
Функция Читает() Экспорт
	Возврат Неопределено;
КонецФункции
"
        );
        let (db, file_id) = setup(&fixture);

        let ret = resolved(&db, file_id, "Читает").ret.expect("слот возврата разрешён");
        let members = union_members(&db, ret);
        assert!(
            members.iter().any(|m| field_names(&db, *m) == vec!["Адрес", "Таймаут"]),
            "{order}: структура цели не попала в слот: {:?}",
            db.lookup_type(ret),
        );
        assert!(
            members.contains(&db.undefined()),
            "{order}: рукав Неопределено не пережил разрешение: {:?}",
            db.lookup_type(ret),
        );
    }
}

#[test]
fn the_third_segment_names_a_parameter_of_the_target() {
    // The three-segment form is the larger half of the population in the configurations this
    // serves: 7 699 occurrences, headed by the modules an implementation edits daily.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Точно - см. База.Настроить.Настройки
//   Регистр - см. База.Настроить.настройки
//   Нет - см. База.Настроить.НетТакого
Функция Читает(Точно, Регистр, Нет) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let signature = resolved(&db, file_id, "Читает");

    let exact = param_index(&db, file_id, "Читает", "Точно");
    assert_eq!(field_names(&db, signature.param(exact).expect("разрешён")), vec!["Режим"]);
    let folded = param_index(&db, file_id, "Читает", "Регистр");
    assert_eq!(
        field_names(&db, signature.param(folded).expect("разрешён")),
        vec!["Режим"],
        "третий сегмент сверяется без учёта регистра, как и вся документация параметров",
    );
    let missing = param_index(&db, file_id, "Читает", "Нет");
    assert!(signature.param(missing).is_none(), "несуществующий параметр цели ничего не даёт");
}

#[test]
fn only_a_target_that_documents_fields_replaces_the_slot() {
    // Replacing the permissive type by a concrete one buys nothing when the target documents no
    // fields, and costs a narrowing. The last parameter is the positive control: without it the
    // test passes on an implementation that resolves nothing at all.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Строкой - см. База.Строкой
//   Процедурой - см. База.Настроить
//   Скрытая - см. База.Скрытый
//   Полями - см. База.Создать
Функция Читает(Строкой, Процедурой, Скрытая, Полями) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let signature = resolved(&db, file_id, "Читает");

    for (name, why) in [
        ("Строкой", "цель возвращает Строку — полей нет"),
        ("Процедурой", "цель ничего не возвращает"),
        ("Скрытая", "цель не Экспорт — из чужого модуля она не видна"),
    ] {
        let index = param_index(&db, file_id, "Читает", name);
        assert!(signature.param(index).is_none(), "{name}: {why}");
    }
    let control = param_index(&db, file_id, "Читает", "Полями");
    assert_eq!(
        field_names(&db, signature.param(control).expect("разрешён")),
        vec!["Адрес", "Таймаут"]
    );
}

#[test]
fn an_unresolvable_reference_leaves_the_slot_alone() {
    // Prose is the input that matters most here: `см. в описании` and its kin are 16 291 lines in
    // one configuration, and treating them as references would claim a target for every one.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   НетМодуля - см. НетТакогоМодуля.Метод
//   НетМетода - см. База.Отсутствует
//   Проза - см. в описании
//   Ещё - см. также
//   Метаданные - см. Справочники.Назначения.МакетФормы
//   Полями - см. База.Создать
Функция Читает(НетМодуля, НетМетода, Проза, Ещё, Метаданные, Полями) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let signature = resolved(&db, file_id, "Читает");

    for name in ["НетМодуля", "НетМетода", "Проза", "Ещё", "Метаданные"]
    {
        let index = param_index(&db, file_id, "Читает", name);
        assert!(signature.param(index).is_none(), "{name} не называет разрешимую цель");
    }
    let control = param_index(&db, file_id, "Читает", "Полями");
    assert!(signature.param(control).is_some(), "положительный контроль в той же фикстуре");
}

#[test]
fn an_arbitrary_arm_beside_a_reference_keeps_the_slot_permissive() {
    // `Произвольный` is the word that declares no constraint, and losing it is not a cosmetic
    // regression: the kernel drops `Unknown` from a union, so a slot that lost it narrows to
    // whatever stood beside it and starts refusing every real argument.
    //
    // It is also the input that separates the two doc-parsers. `Неопределено` lowers identically
    // in both, so a test using only that arm passes while this one fails.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Любой - Произвольный, см. База.Создать
Функция Читает(Любой) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let signature = resolved(&db, file_id, "Читает");

    // The slot stays the top type, and that is the whole assertion: `Произвольный` dominates a
    // union (`T | Any == Any`), so a documented structure cannot be added beside it — it can only
    // replace it, which is the narrowing this forbids.
    let ty = signature.param(0).unwrap_or_else(|| db.any());
    assert_eq!(ty, db.any(), "слот, объявленный Произвольный, сузился до {:?}", db.lookup_type(ty),);
}

#[test]
fn a_mixed_slot_keeps_the_arm_documented_beside_the_reference() {
    // Both halves are asserted on the decomposition of the resolved type itself. Stating the
    // second half as "no TypeMismatch" would be green on any implementation: the only emitter
    // takes its expected type from the tier-1 parameters, which this feature leaves alone.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Параметры - Неопределено, см. База.Создать - необязательные.
Функция Читает(Параметры) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let signature = resolved(&db, file_id, "Читает");

    let ty = signature.param(0).expect("слот разрешён");
    let members = union_members(&db, ty);
    assert!(
        members.iter().any(|m| field_names(&db, *m) == vec!["Адрес", "Таймаут"]),
        "структура цели попала в слот",
    );
    assert!(
        members.contains(&db.undefined()),
        "рукав Неопределено пережил разрешение: {:?}",
        db.lookup_type(ty),
    );
}

#[test]
fn a_reference_under_an_array_and_inside_a_field_resolves_too() {
    // The gate bit alone does not cover these: an implementation that arms the bit recursively but
    // resolves only at the root leaves both slots exactly as they were, and the promised scope
    // quietly does not work.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   ВМассиве - Массив из см. База.Создать
//   ВПоле - Структура:
//    * Ключ - см. База.Создать - вложенная.
//   БезПолей - Структура:
//    * Ключ - см. База.Строкой - цель без полей.
Функция Читает(ВМассиве, ВПоле, БезПолей) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let signature = resolved(&db, file_id, "Читает");

    let in_array = param_index(&db, file_id, "Читает", "ВМассиве");
    let array_ty = signature.param(in_array).expect("слот с массивом разрешён");
    let TypeKind::Array(facet) = db.lookup_type(array_ty) else {
        panic!("ожидался массив, получено {:?}", db.lookup_type(array_ty));
    };
    let element = facet.element.expect("элемент массива типизирован");
    assert_eq!(field_names(&db, element), vec!["Адрес", "Таймаут"]);

    let in_field = param_index(&db, file_id, "Читает", "ВПоле");
    let field_ty = signature.param(in_field).expect("слот с полем-ссылкой разрешён");
    let TypeKind::Structure(facet) = db.lookup_type(field_ty) else {
        panic!("ожидалась структура, получено {:?}", db.lookup_type(field_ty));
    };
    let fields = facet.fields.as_ref().expect("поля документированы");
    assert_eq!(field_names(&db, fields.fields[0].ty), vec!["Адрес", "Таймаут"]);

    // The negative control for the field form: acceptance belongs to the reference, not to the
    // structure that holds it, or a target resolving to `Строка` would narrow the field.
    let no_fields = param_index(&db, file_id, "Читает", "БезПолей");
    assert!(signature.param(no_fields).is_none(), "цель без полей поле не сужает");
}

#[test]
fn a_cycle_stops_and_leaves_the_slots_permissive() {
    let fixture = r#"
//- /CommonModules/А/Ext/Module.bsl
// Возвращаемое значение:
//   см. Б.Получить
Функция Получить() Экспорт
	Возврат Неопределено;
КонецФункции

//- /CommonModules/Б/Ext/Module.bsl
// Возвращаемое значение:
//   см. А.Получить
Функция Получить() Экспорт
	Возврат Неопределено;
КонецФункции

//- /CommonModules/Сам/Ext/Module.bsl
// Возвращаемое значение:
//   см. Сам.Получить
Функция Получить() Экспорт
	Возврат Неопределено;
КонецФункции

//- /CommonModules/Цель/Ext/Module.bsl
// Возвращаемое значение:
//   Структура:
//    * Поле - Строка - поле.
Функция Получить() Экспорт
	Возврат Новый Структура;
КонецФункции

//- /test.bsl
// Параметры:
//   ЧерезА - см. А.Получить
//   Сам - см. Сам.Получить
//   Обычная - см. Цель.Получить
Функция Читает(ЧерезА, Сам, Обычная) Экспорт
	Возврат Неопределено;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);

    let signature = resolved(&db, file_id, "Читает");

    for name in ["ЧерезА", "Сам"] {
        let index = param_index(&db, file_id, "Читает", name);
        assert!(signature.param(index).is_none(), "{name}: цикл не даёт типа");
    }
    // Without this the test passes on an implementation that answers `None` to everything.
    let control = param_index(&db, file_id, "Читает", "Обычная");
    assert_eq!(field_names(&db, signature.param(control).expect("разрешён")), vec!["Поле"]);
}

#[test]
fn a_cut_descent_does_not_poison_a_later_direct_reference() {
    // A symmetric cycle cannot show this: a node cut by re-entry gets the same answer it gets on
    // its own. The asymmetry here is the structure wrapper — and it has to be a wrapper that
    // survives acceptance, so an array of a permissive type would not do.
    //
    // Резолвя `ЧерезПервый`, обход спускается Первый → Цель → Первый и обрывает `Цель` пустым.
    // Прямая ссылка на `Цель` обязана пересчитать её в структуру Первого.
    let fixture = r#"
//- /CommonModules/Первый/Ext/Module.bsl
// Возвращаемое значение:
//   Структура:
//    * Поле - см. Цель.Получить - вложенная ссылка.
Функция Получить() Экспорт
	Возврат Новый Структура;
КонецФункции

//- /CommonModules/Цель/Ext/Module.bsl
// Возвращаемое значение:
//   см. Первый.Получить
Функция Получить() Экспорт
	Возврат Неопределено;
КонецФункции

//- /test.bsl
// Параметры:
//   ЧерезПервый - см. Первый.Получить
//   Напрямую - см. Цель.Получить
Функция Читает(ЧерезПервый, Напрямую) Экспорт
	Возврат Неопределено;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);

    let signature = resolved(&db, file_id, "Читает");

    let through = param_index(&db, file_id, "Читает", "ЧерезПервый");
    assert_eq!(
        field_names(&db, signature.param(through).expect("разрешён")),
        vec!["Поле"],
        "обрыв цикла касается только вложенного поля, сама структура остаётся",
    );
    let direct = param_index(&db, file_id, "Читает", "Напрямую");
    assert_eq!(
        field_names(&db, signature.param(direct).expect("прямая ссылка пересчитывается")),
        vec!["Поле"],
        "значение, оборванное под чужим спуском, не должно было попасть в памятку",
    );
}

#[test]
fn the_signature_call_resolution_uses_stays_on_tier_one() {
    // The whole reason this lives in a query of its own: the signature is pure, cheap and read by
    // every call site, and it must not acquire a cross-module dependency. Both halves are asserted
    // together — the first alone would pass if the feature were dead, the second alone if it were
    // wired into the signature.
    // The referring method has to live in a common module: resolving a qualified call is exactly
    // the path under test, and `/test.bsl` is not addressable that way.
    let fixture = format!(
        "{TARGETS}
//- /CommonModules/Читающий/Ext/Module.bsl
// Параметры:
//   Параметры - см. База.Создать
// Возвращаемое значение:
//   см. База.Создать
Функция Читает(Параметры) Экспорт
	Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
КонецПроцедуры
"
    );
    let (db, file_id) = setup(&fixture);

    let resolver = hir::Resolver::with_workspace_scope(ModuleId::new(file_id));
    let resolution =
        hir::resolve_qualified_call(&db, &Name::new("Читающий"), &Name::new("Читает"), &resolver)
            .expect("вызов Читающий.Читает обязан разрешаться, иначе половина гейта холостая");

    assert_eq!(
        resolution.signature.params[0],
        db.any(),
        "параметр в сигнатуре остаётся проницаемым — ссылку разрешает не она",
    );
    assert_eq!(resolution.signature.ret, db.any(), "и возврат тоже");

    // The other half: the same slots DO carry fields once the query is asked.
    let referring = resolution.method_id;
    let resolved =
        doc_see_signature_query(&db, MethodIdInput::new(&db, referring)).as_ref().clone();
    assert_eq!(field_names(&db, resolved.ret.expect("возврат разрешён")), vec!["Адрес", "Таймаут"]);
    assert_eq!(
        field_names(&db, resolved.param(0).expect("параметр разрешён")),
        vec!["Адрес", "Таймаут"]
    );
}

#[test]
fn editing_the_targets_documentation_reaches_the_referring_slot() {
    // Invalidation, and it has to happen inside ONE database: building a fresh one per assertion
    // asks salsa nothing at all and passes on an implementation that caches the target forever.
    // The referring module is never edited — only the file it points at.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Параметры - см. База.Создать
Функция Читает(Параметры) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (mut db, file_id) = setup(&fixture);
    let target_file = file_ending_with(&fixture, "/CommonModules/База/Ext/Module.bsl");

    assert_eq!(
        field_names(&db, resolved(&db, file_id, "Читает").param(0).expect("разрешён")),
        vec!["Адрес", "Таймаут"],
    );

    let edited = Fixture::parse(&fixture)
        .files
        .get(&target_file)
        .expect("файл цели есть в фикстуре")
        .content
        .replace("//    * Адрес - Строка - куда.", "//    * Порт - Число - порт.");
    db.set_file_text(target_file, &edited);

    assert_eq!(
        field_names(&db, resolved(&db, file_id, "Читает").param(0).expect("разрешён")),
        vec!["Порт", "Таймаут"],
        "правка документации цели обязана дойти до ссылающегося слота",
    );
}

#[test]
fn an_alternative_this_parser_cannot_read_keeps_the_slot_permissive() {
    // `Соответствие из Строка` is one of the forms `parse_type_expr` answers `None` for. Dropping
    // it would narrow the slot from the permissive type it has today down to the target structure,
    // and the seeded parameter inside the body would inherit that narrowing.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Данные - Соответствие из Строка, см. База.Создать
Функция Читает(Данные) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    let ty = resolved(&db, file_id, "Читает").param(0).unwrap_or_else(|| db.any());

    assert_eq!(ty, db.any(), "нечитаемая альтернатива сузила слот до {:?}", db.lookup_type(ty));
}

#[test]
fn an_alternative_only_the_signatures_parser_reads_keeps_the_slot_whole() {
    // `Соответствие из Строка` has a space in its name: the signature's parser reads it, this
    // module's does not. Rebuilding the slot here turns that arm into the top type, and the top
    // type dominates the structure documented beside it — the slot the caller had would be
    // replaced by one that says nothing at all.
    let fixture = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Данные - Соответствие из Строка
//          - Структура:
//    * Ключ - см. База.Создать - вложенная.
Функция Читает(Данные) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&fixture);

    assert_eq!(
        resolved(&db, file_id, "Читает").param(0),
        None,
        "слот пересобран парсером, который читает не тот же язык, что сигнатура",
    );

    // The control: the same slot without that arm does get resolved, so the assertion above
    // measures the disagreement and not the absence of a reference.
    let control = format!(
        "{TARGETS}
//- /test.bsl
// Параметры:
//   Данные - Структура:
//    * Ключ - см. База.Создать - вложенная.
Функция Читает(Данные) Экспорт
	Возврат Неопределено;
КонецФункции
"
    );
    let (db, file_id) = setup(&control);
    let ty = resolved(&db, file_id, "Читает").param(0).expect("слот без спорного рукава разрешён");
    assert_eq!(field_names(&db, ty), ["Ключ"]);
}

/// A chain of `length` modules, each referring to the next; the last documents two fields.
/// `М0` is the head, so a slot pointing at it walks the whole chain.
fn chain(length: usize) -> String {
    let mut text = String::new();
    for step in 0..length {
        let body = if step + 1 == length {
            "// Возвращаемое значение:\n//   Структура:\n//    * Таймаут - Число - секунды.\n//    * Адрес - Строка - куда.".to_string()
        } else {
            format!("// Возвращаемое значение:\n//   см. М{}.Дай", step + 1)
        };
        text.push_str(&format!(
            "//- /CommonModules/М{step}/Ext/Module.bsl\n{body}\nФункция Дай() Экспорт\n\tВозврат Неопределено;\nКонецФункции\n\n"
        ));
    }
    text.push_str("//- /test.bsl\n// Возвращаемое значение:\n//   см. М0.Дай\nФункция Читает() Экспорт\n\tВозврат Неопределено;\nКонецФункции\n");
    text
}

#[test]
fn a_chain_longer_than_the_depth_ceiling_leaves_the_slot_alone() {
    // The ceiling is 32 nodes. A chain of 8 is answered; one of 40 is cut, and a cut descent
    // gives the slot nothing rather than a half-walked answer.
    let (db, file_id) = setup(&chain(8));
    let short = resolved(&db, file_id, "Читает").ret.expect("короткая цепочка проходится");
    assert_eq!(field_names(&db, short), ["Адрес", "Таймаут"]);

    let (db, file_id) = setup(&chain(40));
    assert_eq!(
        resolved(&db, file_id, "Читает").ret,
        None,
        "цепочка длиннее потолка глубины обязана обрываться, а не доходить",
    );
}

#[test]
fn more_references_than_the_expansion_ceiling_leave_the_later_slots_alone() {
    // The ceiling is 1024 references followed per query, counted across all slots. Each parameter
    // here points at a DISTINCT target, so the settled-value cache cannot pay for the next one.
    fn fixture(targets: usize) -> String {
        let mut text = String::from("//- /CommonModules/База/Ext/Module.bsl\n");
        for index in 0..targets {
            text.push_str(&format!(
                "// Возвращаемое значение:\n//   Структура:\n//    * Поле{index} - Строка - поле.\nФункция Дай{index}() Экспорт\n\tВозврат Неопределено;\nКонецФункции\n\n"
            ));
        }
        text.push_str("//- /test.bsl\n// Параметры:\n");
        for index in 0..targets {
            text.push_str(&format!("//   П{index} - см. База.Дай{index}\n"));
        }
        let params: Vec<String> = (0..targets).map(|index| format!("П{index}")).collect();
        text.push_str(&format!(
            "Функция Читает({}) Экспорт\n\tВозврат Неопределено;\nКонецФункции\n",
            params.join(", ")
        ));
        text
    }

    let (db, file_id) = setup(&fixture(4));
    let sig = resolved(&db, file_id, "Читает");
    assert_eq!(field_names(&db, sig.param(3).expect("четвёртый слот разрешён")), ["Поле3"]);

    let over = 1100;
    let (db, file_id) = setup(&fixture(over));
    let sig = resolved(&db, file_id, "Читает");
    assert!(sig.param(0).is_some(), "первые слоты обязаны разрешаться до исчерпания бюджета");
    assert_eq!(
        sig.param(over - 1),
        None,
        "слоты за потолком раскрытий обязаны оставаться нетронутыми",
    );
}
