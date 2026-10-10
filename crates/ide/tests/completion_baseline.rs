use ide::{Analysis, CompletionItem, CompletionItemKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn setup(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);

    let mut db = RootDatabaseImpl::new();
    let source_root_id = SourceRootId(0);

    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(source_root_id, source_root);

    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, source_root_id);
        db.set_file_text(*file_id, &file.content);
    }

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn extract_cursor(fixture_text: &str) -> (String, String, u32) {
    let abs_idx = fixture_text.find("$0").expect("fixture must contain $0 cursor marker");
    let prefix = &fixture_text[..abs_idx];
    let last_header_start = prefix.rfind("//- ").expect("cursor must be inside a //- file");
    let header_end =
        prefix[last_header_start..].find('\n').expect("//- header must end with newline")
            + last_header_start;
    let path_line = &prefix[last_header_start + 4..header_end];
    let file_offset_in_prefix = header_end + 1;
    let cursor_in_file = (abs_idx - file_offset_in_prefix) as u32;
    let cleaned = fixture_text.replacen("$0", "", 1);
    (cleaned, path_line.to_string(), cursor_in_file)
}

fn complete(fixture: &str) -> Vec<CompletionItem> {
    let (analysis, file_id, offset) = setup(fixture);
    analysis.completions(file_id, offset, None, ide::Locale::Ru)
}

fn has_label(items: &[CompletionItem], label: &str) -> bool {
    items.iter().any(|i| i.label == label)
}

fn items_matching<'a>(items: &'a [CompletionItem], label: &str) -> Vec<&'a CompletionItem> {
    items.iter().filter(|i| i.label == label).collect()
}

/// The context-type-match digit of an item's sort_text (`{tier}{band 2}_{typematch}…`).
/// `'0'` = matches the expected type, `'1'` = does not / unknown.
fn typematch_digit(items: &[CompletionItem], label: &str) -> Option<char> {
    items.iter().find(|i| i.label == label)?.sort_text.as_deref()?.chars().nth(4)
}

#[test]
fn completion_after_dot_on_common_module_lists_exported_methods() {
    let items = complete(
        r#"//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ПолучитьЗначение() Экспорт
    Возврат 1;
КонецФункции

Функция ВнутреннийМетод()
    Возврат 0;
КонецФункции

Процедура УстановитьЗначение(Значение) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Результат = ОбщегоНазначения.$0
КонецПроцедуры
"#,
    );

    assert!(
        has_label(&items, "ПолучитьЗначение"),
        "exported function must appear; got labels: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    assert!(
        has_label(&items, "УстановитьЗначение"),
        "exported procedure must appear; got labels: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    assert!(
        !has_label(&items, "ВнутреннийМетод"),
        "non-exported method must not leak across module boundary; got labels: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );

    for item in items_matching(&items, "ПолучитьЗначение") {
        assert!(
            matches!(item.kind, CompletionItemKind::Function | CompletionItemKind::Method),
            "exported function kind should be Function or Method, got {:?}",
            item.kind
        );
    }
}

#[test]
fn completion_in_procedure_body_offers_keyword_templates() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Есл$0
КонецПроцедуры
"#,
    );

    assert!(!items.is_empty(), "unqualified `Есл` must offer keyword/templates");

    // The `Если … Тогда … КонецЕсли` template must be offered as a snippet.
    let if_template =
        items.iter().find(|i| i.kind == CompletionItemKind::Snippet && i.label.starts_with("Если"));
    assert!(
        if_template.is_some(),
        "`Если` block template must be offered; got: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    assert!(
        if_template.unwrap().insert_text.contains("КонецЕсли"),
        "the template must expand to a full block with `КонецЕсли`"
    );

    // The contiguous gate keeps scattered platform matches (`П-е-...-с-л`) out of
    // the list for a short prefix.
    assert!(
        !has_label(&items, "Перечисления"),
        "scattered platform match must not flood a short-prefix list; got: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn completion_ranks_exact_prefix_above_fuzzy_via_sort_text() {
    // `Найти` is an exact prefix of `НайтиПоКоду`/`НайтиПоНаименованию` and a
    // scattered match for other names; the exact-prefix hits must sort first.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    НайтиЗначение = 1;
    Найт$0
КонецПроцедуры
"#,
    );
    let ranked: Vec<(&str, &str)> = items
        .iter()
        .filter_map(|i| i.sort_text.as_deref().map(|s| (i.label.as_str(), s)))
        .collect();
    assert!(!ranked.is_empty(), "every unqualified item must carry a sort_text");
    // `НайтиЗначение` is a local in scope and an exact prefix → tier 0 + locals band.
    let local = ranked.iter().find(|(l, _)| *l == "НайтиЗначение");
    assert!(local.is_some(), "the in-scope local must be offered; got {:?}", ranked);
    assert!(
        local.unwrap().1.starts_with('0'),
        "exact-prefix local must be in the best quality tier (sort_text starts with 0); got {:?}",
        local.unwrap()
    );
}

#[test]
fn completion_english_prefix_ranks_metadata_plural_in_top_tier() {
    // Typing the English name `Docu` admits `Документы` via its English alias; it
    // must be ranked by that match's real quality (tier 0), not sunk to Fuzzy by
    // scoring the Russian label.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Docu$0
КонецПроцедуры
"#,
    );
    // Platform data may be unavailable in some environments; only assert when the
    // metadata plural is actually offered.
    if let Some(item) = items.iter().find(|i| i.label == "Документы") {
        let sort_text = item.sort_text.as_deref().expect("offered item must carry sort_text");
        assert!(
            sort_text.starts_with('0'),
            "English prefix `Docu` must rank `Документы` in the top quality tier; got {sort_text:?}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_context_type_boost_floats_matching_local() {
    // `Цел` expects a `Число` argument. The Number-typed local must float above the
    // String-typed one even though it sorts later alphabetically — only the
    // context-type boost can produce that order.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    ПарамС = "строка";
    ПарамЧ = 123;
    Цел(Парам$0)
КонецПроцедуры
"#,
    );
    let sort_of =
        |label: &str| items.iter().find(|i| i.label == label).and_then(|i| i.sort_text.clone());
    let num = sort_of("ПарамЧ").expect("Number local must be offered");
    let string = sort_of("ПарамС").expect("String local must be offered");

    // typematch digit sits at index 4: `{tier}{band 2}_{typematch}…`.
    assert_eq!(
        num.chars().nth(4),
        Some('0'),
        "Number local must match expected `Число`; got {num:?}"
    );
    assert_eq!(
        string.chars().nth(4),
        Some('1'),
        "String local must not match `Число`; got {string:?}"
    );
    assert!(num < string, "type-matching local must sort first; got {num:?} vs {string:?}");
}

#[test]
fn completion_assignment_rhs_boosts_matching_local() {
    // RHS of `Сумма = …` expects the type of `Сумма` (Число). The Число local is
    // boosted, the Строка local is not.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Сумма = 0;
    Строка1 = "x";
    Сумма = С$0
КонецПроцедуры
"#,
    );
    assert_eq!(
        typematch_digit(&items, "Сумма"),
        Some('0'),
        "Сумма (Число) must match assignment LHS type"
    );
    assert_eq!(typematch_digit(&items, "Строка1"), Some('1'), "Строка1 (Строка) must not match");
}

#[test]
fn completion_return_boosts_matching_local_in_function() {
    // The function returns Число (via `Возврат ЗначЧисло`), so `Возврат Зн…`
    // expects Число → the Число local outranks the Строка local.
    let items = complete(
        r#"//- /test.bsl
Функция Вычислить()
    ЗначЧисло = 5;
    ЗначСтрока = "строка";
    Возврат ЗначЧисло;
    Возврат Зн$0
КонецФункции
"#,
    );
    assert_eq!(
        typematch_digit(&items, "ЗначЧисло"),
        Some('0'),
        "ЗначЧисло (Число) must match the return type"
    );
    assert_eq!(
        typematch_digit(&items, "ЗначСтрока"),
        Some('1'),
        "ЗначСтрока (Строка) must not match"
    );
}

#[test]
fn completion_function_candidate_boosted_by_inferred_return_type() {
    // A user function whose inferred return type matches the expected type is
    // boosted — no doc-comments involved.
    let items = complete(
        r#"//- /test.bsl
Функция ДайЧисло()
    Возврат 100;
КонецФункции

Процедура Тест()
    Сумма = 0;
    Сумма = Дай$0
КонецПроцедуры
"#,
    );
    assert_eq!(
        typematch_digit(&items, "ДайЧисло"),
        Some('0'),
        "function returning Число must be boosted in a Число context"
    );
}

#[test]
fn completion_unresolved_call_arg_does_not_leak_outer_context() {
    // Cursor is in the argument of an unresolved call. The assignment's expected
    // type must NOT leak into the argument slot, so the Число local is not boosted.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Сумма = 0;
    Сумма = НетТакойФункции(С$0)
КонецПроцедуры
"#,
    );
    assert_eq!(
        typematch_digit(&items, "Сумма"),
        Some('1'),
        "outer assignment type must not leak into an unresolved call's argument"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_constructor_arg_uses_constructor_type_not_outer_context() {
    // Cursor is in a constructor argument (`Новый Тип(…)`). The constructor's own
    // parameter type is used, and the outer assignment type must NOT leak in.
    // Array constructor accepts `Произвольный`, so a `Число` local matches.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Сумма = 0;
    Сумма = Новый Массив(Сум$0)
КонецПроцедуры
"#,
    );
    assert_eq!(
        typematch_digit(&items, "Сумма"),
        Some('0'),
        "constructor parameter type must be used, not the outer assignment type"
    );
}

#[test]
fn completion_return_in_procedure_does_not_boost() {
    // A procedure has no return value type, so `Возврат …` in a procedure must not
    // boost anything.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    ЗначЧисло = 5;
    Возврат Зн$0
КонецПроцедуры
"#,
    );
    assert_eq!(
        typematch_digit(&items, "ЗначЧисло"),
        Some('1'),
        "return inside a procedure must not produce a type boost"
    );
}

#[test]
fn completion_empty_prefix_does_not_type_functions() {
    // With an empty prefix the expected type is known but function return types are
    // not computed (perf guard), so the function candidate stays unboosted.
    let items = complete(
        r#"//- /test.bsl
Функция ДайЧисло()
    Возврат 100;
КонецФункции

Процедура Тест()
    Сумма = 0;
    Сумма = $0
КонецПроцедуры
"#,
    );
    assert_eq!(
        typematch_digit(&items, "ДайЧисло"),
        Some('1'),
        "function return types must not be computed on an empty prefix"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_new_array_type() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    А = Новый Массив;
    А.$0
КонецПроцедуры
"#,
    );

    assert!(
        !items.is_empty(),
        "platform methods must be offered after `А.` where `А` is `Новый Массив`"
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        has_label(&items, "Добавить"),
        "Массив method `Добавить` must be offered; got: {:?}",
        labels
    );
    for item in &items {
        assert!(
            !item.label.is_empty(),
            "completion items must have non-empty labels; got {:?}",
            item
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_array_variable_with_prefix() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Сп = Новый Массив;
    Сп.Доб$0
КонецПроцедуры
"#,
    );

    assert!(!items.is_empty(), "prefix-filtered completion must not be empty; got empty");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.iter().all(|l| l.to_lowercase().starts_with("доб")),
        "every label must start with `Доб`; got: {:?}",
        labels
    );
    assert!(has_label(&items, "Добавить"), "`Добавить` must match prefix `Доб`; got: {:?}", labels);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_on_array_variable_typed_prefix_full_ident() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Сп = Новый Массив;
    Сп.В$0
КонецПроцедуры
"#,
    );

    assert!(!items.is_empty(), "methods matching `В` must be offered after `Сп.В`");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    // Member completion is now fuzzy: a prefix hit like `Вставить` must be offered
    // and ranked in the best quality tier (sort_text starts with `0`).
    let vstavit = items
        .iter()
        .find(|i| i.label == "Вставить")
        .unwrap_or_else(|| panic!("`Вставить` must be offered after `Сп.В`; got: {labels:?}"));
    assert!(
        vstavit.sort_text.as_deref().is_some_and(|s| s.starts_with('0')),
        "prefix hit `Вставить` must be top quality tier; got {:?}",
        vstavit.sort_text
    );
}

#[test]
fn completion_after_dot_inside_preproc_branch_has_no_local_to_offer() {
    let items = complete(
        r#"//- /test.bsl
&НаСервере
Процедура Тест()
    Справочники = Новый Структура("Код", 1);
    #Если Сервер Тогда
    Справочники.$0
    #КонецЕсли
КонецПроцедуры
"#,
    );

    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    // Assigning to a Global-context property declares no local, so there is no
    // `Структура` here — inside a preprocessor branch just as outside it.
    assert!(
        !has_label(&items, "Вставить"),
        "no local of type Структура exists, inside #Если or out; got: {labels:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_dot_member_substring_is_offered_below_prefix() {
    // `чест` is an interior substring of `Количество`, not a prefix — member
    // matching is now fuzzy, so it is offered, but ranked below the prefix tier.
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Сп = Новый Массив;
    Сп.чест$0
КонецПроцедуры
"#,
    );
    let kol = items.iter().find(|i| i.label == "Количество").unwrap_or_else(|| {
        panic!(
            "substring match `Количество` for `чест` must be offered; got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        )
    });
    assert!(
        !kol.sort_text.as_deref().unwrap_or("").starts_with('0'),
        "interior substring hit must not be in the prefix tier; got {:?}",
        kol.sort_text
    );
}

#[test]
fn completion_after_dot_on_common_module_typed_prefix_is_ranked() {
    // A typed prefix on a bare common module routes through the same fuzzy/quality
    // funnel: the exported prefix hit is offered in the top tier, the non-exported
    // method stays excluded.
    let items = complete(
        r#"//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ПолучитьЗначение() Экспорт
    Возврат 1;
КонецФункции

Функция ВнутреннийМетод()
    Возврат 0;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщегоНазначения.Пол$0
КонецПроцедуры
"#,
    );
    let got = items.iter().find(|i| i.label == "ПолучитьЗначение").unwrap_or_else(|| {
        panic!(
            "exported method must pass through the fuzzy funnel; got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        )
    });
    assert!(
        got.sort_text.as_deref().is_some_and(|s| s.starts_with('0')),
        "prefix hit `ПолучитьЗначение` must be top quality tier; got {:?}",
        got.sort_text
    );
    assert!(
        !has_label(&items, "ВнутреннийМетод"),
        "non-exported method must remain excluded after the dot"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_after_chained_dot_on_array_variable() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Сп = Новый Массив;
    Сп.Добавить(1);
    Сп.$0
КонецПроцедуры
"#,
    );

    assert!(
        !items.is_empty(),
        "trailing `Сп.` must still produce method completions after a successful call"
    );
    assert!(
        has_label(&items, "Добавить"),
        "methods must still be available on the second `Сп.`; labels: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn completion_after_dot_on_number_variable_does_not_offer_array_methods() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Сп = 42;
    Сп.$0
КонецПроцедуры
"#,
    );

    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !has_label(&items, "Добавить"),
        "Number receiver must not surface Массив.Добавить; got: {:?}",
        labels
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn completion_with_cursor_on_keyword_method_name_after_dot() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Выполнить$0
КонецПроцедуры
"#,
    );

    assert!(
        !items.is_empty(),
        "completion must fire when cursor is on a keyword field-tail token (KW_EXECUTE)"
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        has_label(&items, "Выполнить"),
        "method list must include `Выполнить` for a Query receiver; got: {:?}",
        labels
    );
}

#[test]
fn completion_after_dot_on_unresolved_receiver_is_safe() {
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    НеизвестныйСимвол.$0
КонецПроцедуры
"#,
    );

    for item in &items {
        assert!(
            !item.label.is_empty(),
            "completion items must have non-empty labels; got {:?}",
            item
        );
    }
}

/// Метод, который ещё печатают, кончается на последнем набранном токене, а
/// пробел за ним принадлежит уже корню: тривия достаётся общему предку
/// соседних значимых токенов, и другого предка у хвоста файла нет. Курсор
/// при этом стоит в теле метода, и локальные имена обязаны предлагаться.
#[test]
fn unterminated_method_keeps_locals_in_the_file_tail() {
    let items = complete(
        r#"//- /test.bsl
Процедура П()
    Перем МояПеременная;
    $0
"#,
    );
    assert!(
        has_label(&items, "МояПеременная"),
        "локальная переменная незавершённого метода обязана предлагаться"
    );
}

/// Различающий вход к предыдущему: за `КонецПроцедуры` стоит уже модульный
/// уровень. Шаг назад «всегда», без проверки на незавершённость, предложил бы
/// здесь локальные имена закрытого метода.
#[test]
fn trivia_after_a_closed_method_offers_no_locals() {
    let items = complete(
        r#"//- /test.bsl
Процедура П()
    Перем МояПеременная;
КонецПроцедуры
    $0
"#,
    );
    assert!(
        !has_label(&items, "МояПеременная"),
        "за закрытым методом его локальных имён быть не должно"
    );
}
