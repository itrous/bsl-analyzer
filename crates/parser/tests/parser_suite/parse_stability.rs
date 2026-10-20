//! Дерево, текст и диапазоны ошибок не зависят от повторного разбора и кеша.
//!
//! Provenance: `docs/legal/bsl-clean-room-slice-b3.md`.

const INPUTS: &[&str] = &[
    "&Перед(&НаКлиенте)\nПроцедура Т() КонецПроцедуры",
    "&Перед(\"Тест\")\nПроцедура Т() КонецПроцедуры",
    "&НаКлиенте\n&Перед(\"Т\")\nПроцедура Т() КонецПроцедуры",
    "Процедура П(Знач А = 1, Б)\n  Х = ?(А > 0, -Б.В[0], Новый Массив(2));\nКонецПроцедуры",
    "#Если (Сервер Или Клиент) И Не ВебКлиент Тогда\nПерем А, Б Экспорт;\n#КонецЕсли",
    "Функция Ф()\n  Попытка\n    Выполнить Х;\n  Исключение\n    ВызватьИсключение;\n  КонецПопытки;\nКонецФункции",
    "Х = \"первая\"\n  \"вторая\";",
    "Перейти ~М;\n~М: Х = 1;",
    "Х = а = б = в;",
    "Х = -а.б + Ф();",
    "Процедура П(Знач",
    "Процедура П()\nЕсли А Тогда\nХ = Ф(\nКонецПроцедуры",
    "Х = \"первая\n|вторая",
    "Процедура П()\nХ = А.\nКонецПроцедуры",
    "Х = Новый Файл(\"а\").Имя;",
    "Х = а = Не б;",
];

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    tree: String,
    errors: Vec<String>,
}

fn observe(parsed: syntax::Parse<syntax::SyntaxNode>) -> Observation {
    Observation {
        tree: format!("{:#?}", parsed.syntax_node()),
        errors: parsed.errors().iter().map(|error| format!("{error:?}")).collect(),
    }
}

#[test]
fn parsing_the_same_input_twice_gives_the_same_tree() {
    for input in INPUTS {
        assert_eq!(observe(parser::parse(input)), observe(parser::parse(input)), "{input:?}");
    }
}

#[test]
fn the_shared_cache_does_not_change_the_tree() {
    let plain: Vec<_> = INPUTS.iter().map(|input| observe(parser::parse(input))).collect();
    for index in (0..INPUTS.len()).chain((0..INPUTS.len()).rev()) {
        let input = INPUTS[index];
        assert_eq!(plain[index], observe(parser::parse_with_shared_cache(input)), "{input:?}");
    }
}

#[test]
fn the_stability_observer_sees_changed_trees_error_messages_and_ranges() {
    let clean = observe(parser::parse("Х = а Или б И в;"));
    let changed_tree = observe(parser::parse("Х = а И б Или в;"));
    assert_ne!(clean.tree, changed_tree.tree);
    assert_eq!(clean.errors, changed_tree.errors);

    let original = parser::parse("Х = а = Не б;");
    assert!(original.has_errors());
    let mut changed_message = observe(original.clone());
    changed_message.errors[0].push_str("changed");
    assert_ne!(observe(original), changed_message);

    let shifted = observe(parser::parse("  Х = а = Не б;"));
    let plain = observe(parser::parse("Х = а = Не б;"));
    assert_ne!(plain.errors, shifted.errors);
}
