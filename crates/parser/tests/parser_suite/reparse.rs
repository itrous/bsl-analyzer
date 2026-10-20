//! Переразбор метода тождествен полному разбору — или отказывает.
//!
//! Корпус: большой модуль-фикстура и набор коротких входов, где есть
//! аннотации, `#Если` вокруг методов и внутри них, docstring, многострочные
//! строки, `#Вставка`, ошибки. Правки: случайные вставки, удаления и замены
//! внутри каждого метода детерминированным ГПСЧ, плюс перечень вставок,
//! подобранных под каждый гард. Проверка на одних безошибочных правках была бы
//! зелена и без гардов, поэтому у каждого гарда есть свой контроль ниже.

use parser::reparse::{
    edit_between, reparse_method, reparse_method_guarded, Guards, Refusal, TextEdit,
};
use syntax::{SyntaxNode, TextRange};

const MODULE: &str = include_str!("../fixtures/Module.bsl");

const SNIPPETS: &[&str] = &[
    "Процедура А()\n\tХ = 1;\nКонецПроцедуры\n\nФункция Б()\n\tВозврат 1;\nКонецФункции\n",
    "&НаСервере\nПроцедура А()\n\tХ = 1;\nКонецПроцедуры\n&НаКлиенте\n&Перед(\"Б\")\nФункция Б()\n\tВозврат 1;\nКонецФункции\n",
    "#Если Сервер Тогда\nПроцедура А()\n\tХ = 1;\nКонецПроцедуры\n#Иначе\nПроцедура А()\n\tХ = 2;\nКонецПроцедуры\n#КонецЕсли\n",
    "// Описание\n//\n// Параметры:\n//  А - Число\nПроцедура А(А)\n\t#Если Клиент Тогда\n\tХ = 1;\n\t#Иначе\n\tХ = 2;\n\t#КонецЕсли\nКонецПроцедуры\n",
    "Процедура А()\n\tХ = \"первая\n\t|вторая\n\t|третья\";\nКонецПроцедуры\n",
    "Процедура А()\n\t#Вставка\n\tХ = 1;\n\t#КонецВставки\n\t#Удаление\n\tУ = 2;\n\t#КонецУдаления\nКонецПроцедуры\n",
    "Перем М;\nПроцедура А()\n\tХ = ;\nКонецПроцедуры\nМ = 1;\nПроцедура Б()\n\tЕсли Х Тогда\nКонецПроцедуры\n",
    "Процедура А()\n\tХ = 1\nКонецПроцедуры Процедура Б() КонецПроцедуры\n",
    "Процедура А()\n\t#Если Сервер Тогда\n\tПроцедура Вложенная()\n\tКонецПроцедуры\n\t#КонецЕсли\nКонецПроцедуры\n",
    "Асинх Процедура А()\n\tЖдать Б();\nКонецПроцедуры\n",
];

/// Вставки, каждая метит в свой гард; полный список — §3 плана яруса.
const ADVERSARIAL: &[&str] = &[
    "КонецПроцедуры",
    "КонецФункции",
    "Процедура X()",
    "Функция Y()",
    "#КонецЕсли",
    "#Если Сервер Тогда",
    "#Иначе",
    "#ИначеЕсли Клиент Тогда",
    "#Вставка",
    "#КонецВставки",
    "#Удаление",
    "\"",
    "|",
    "//",
    "'",
    "&НаСервере",
    "\n",
    ";",
    "(",
    ")",
    "Экспорт",
    "#Область Р",
    "#КонецОбласти",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*: детерминированно и без зависимостей.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn char_boundary_at_or_before(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn method_ranges(text: &str) -> Vec<TextRange> {
    let parse = parser::parse(text);
    parse
        .syntax_node()
        .descendants()
        .filter(|n| {
            matches!(n.kind(), syntax::SyntaxKind::PROCEDURE_DEF | syntax::SyntaxKind::FUNCTION_DEF)
        })
        .map(|n| n.text_range())
        .collect()
}

fn dump(parse: &syntax::Parse<SyntaxNode>) -> String {
    format!("{:#?}\n{:?}", parse.syntax_node(), parse.errors())
}

/// Вклеенное дерево против полного разбора: дерево, текст, ошибки, оценка
/// памяти. Дамп строится только для сообщения о расхождении.
fn assert_same(
    spliced: &syntax::Parse<SyntaxNode>,
    full: &syntax::Parse<SyntaxNode>,
    new_text: &str,
    label: &str,
) {
    let same = syntax::green_eq(spliced.green(), full.green())
        && spliced.errors() == full.errors()
        && spliced.heap_bytes() == full.heap_bytes();
    assert!(
        same,
        "{label}: вклейка разошлась с полным разбором\n--- вклейка ---\n{}\n--- полный ---\n{}",
        dump(spliced),
        dump(full)
    );
    assert_eq!(spliced.syntax_node().text().to_string(), new_text, "{label}: текст дерева");
    assert_eq!(*spliced, *full, "{label}: Parse == Parse");
}

/// Один вход: либо отказ, либо тождество по дереву, тексту и ошибкам.
fn check(old_text: &str, new_text: &str, label: &str) -> Result<(), Refusal> {
    let old = parser::parse_with_shared_cache(old_text);
    let Some(edit) = edit_between(old_text, new_text) else { return Ok(()) };
    let spliced = reparse_method(&old, old_text, new_text, edit)?;
    let full = parser::parse_with_shared_cache(new_text);
    assert_same(&spliced, &full, new_text, &format!("{label} (правка {edit:?})"));
    Ok(())
}

fn random_edits(
    text: &str,
    rng: &mut Rng,
    per_method: usize,
    stride: usize,
    refusals: &mut [u64; 8],
    spliced: &mut u64,
) {
    for range in method_ranges(text).into_iter().step_by(stride) {
        let (s, e) = (usize::from(range.start()), usize::from(range.end()));
        for _ in 0..per_method {
            let a = char_boundary_at_or_before(text, s + 1 + rng.below(e - s - 1));
            let len = rng.below(40).min(e - a);
            let b = char_boundary_at_or_before(text, a + len);
            let insert: String = match rng.below(4) {
                0 => String::new(),
                1 => "Х = 1;".to_string(),
                2 => ADVERSARIAL[rng.below(ADVERSARIAL.len())].to_string(),
                _ => (0..rng.below(8))
                    .map(|_| "аб1 ;\n\t=".chars().nth(rng.below(8)).unwrap())
                    .collect(),
            };
            let new_text = format!("{}{}{}", &text[..a], insert, &text[b..]);
            match check(text, &new_text, &format!("случайная правка [{a}..{b}) -> {insert:?}"))
            {
                Ok(()) => *spliced += 1,
                Err(r) => refusals[r.index()] += 1,
            }
        }
    }
}

#[test]
fn reparse_matches_full_parse_on_random_edits() {
    let mut rng = Rng(0x1134_2026_0903);
    let mut refusals = [0u64; 8];
    let mut spliced = 0u64;
    for snippet in SNIPPETS {
        random_edits(snippet, &mut rng, 40, 1, &mut refusals, &mut spliced);
    }
    // Каждый восьмой метод большого модуля: полный разбор мегабайта на
    // каждую правку в отладочной сборке стоит сотни миллисекунд.
    random_edits(MODULE, &mut rng, 2, 8, &mut refusals, &mut spliced);
    assert!(spliced > 150, "стенд вхолостую: вклеек {spliced}, отказов {refusals:?}");
    assert!(refusals.iter().sum::<u64>() > 20, "стенд вхолостую: ни одного отказа");
}

#[test]
fn reparse_matches_full_parse_on_adversarial_insertions() {
    let mut refusals = [0u64; 8];
    let mut spliced = 0u64;
    for snippet in SNIPPETS {
        for range in method_ranges(snippet) {
            let (s, e) = (usize::from(range.start()), usize::from(range.end()));
            // Три точки: сразу после первого токена, середина, перед закрывателем.
            let mid = char_boundary_at_or_before(snippet, (s + e) / 2);
            let before_closer = snippet[..e].rfind('\n').map_or(e - 1, |i| i + 1);
            for at in [s + "Процедура".len().min(e - s - 1), mid, before_closer] {
                let at = char_boundary_at_or_before(snippet, at.clamp(s + 1, e - 1));
                for insert in ADVERSARIAL {
                    let new_text = format!("{}{}{}", &snippet[..at], insert, &snippet[at..]);
                    match check(snippet, &new_text, &format!("вставка {insert:?} на {at}"))
                    {
                        Ok(()) => spliced += 1,
                        Err(r) => refusals[r.index()] += 1,
                    }
                }
            }
        }
    }
    assert!(
        spliced > 50 && refusals.iter().any(|&n| n > 0),
        "стенд вхолостую: {spliced} / {refusals:?}"
    );
}

/// У каждого гарда есть вход, на котором он отказывает, и без гардов этот
/// вход даёт дерево, отличное от полного разбора: отказ не вхолостую. Два
/// гарда определяют саму операцию и не выключаются: `OutsideMethod` без узла
/// метода (вклеивать некуда) и `Kind` (`replace_with` паникует на чужом
/// виде). Три других — вторая линия: `TokenBoundary` доказывает границу
/// лексером, но токен, вросший через границу узла, на известных входах
/// ломает и закрыватель; `NewUnclosed` — незакрытый фрагмент всегда несёт
/// ошибку на своём последнем токене, и её ловит `ContextError`;
/// `OldUnclosed` — метод, которому границу поставил `#Иначе` снаружи,
/// стоит в разорванной условной группе, и её директивы внутри узла ловит
/// `Preprocessor`. Для них проверяется только сам отказ и то, что без ВСЕХ
/// гардов вход опасен.
#[test]
fn each_guard_refuses_an_input_that_would_otherwise_splice_wrong() {
    // (гард, старый текст, якорь — вставка идёт перед ним, вставка)
    let cases: &[(Refusal, &str, &str, &str)] = &[
        (
            Refusal::OldUnclosed,
            "#Если Сервер Тогда\nПроцедура А()\n\tХ = Ы\n#Иначе\nКонецПроцедуры\n#КонецЕсли\n",
            "Ы\n#Иначе",
            ";\nКонецПроцедур",
        ),
        (
            // Строка за границей узла: кавычка перед закрывателем даёт строковый
            // токен до конца строки, и граница узла попадает внутрь него.
            Refusal::TokenBoundary,
            "Процедура А()\n\tХ = 1;\nКонецПроцедуры Процедура Б()\nКонецПроцедуры\n",
            "КонецПроцедуры Процедура Б",
            "\"",
        ),
        (
            Refusal::Preprocessor,
            "#Если Сервер Тогда\nПроцедура А()\n\tХ = 1;\nКонецПроцедуры\n#КонецЕсли\n",
            "\tХ = 1;",
            "#КонецЕсли\n",
        ),
        (
            Refusal::Shape,
            "Процедура А()\n\tХ = 1;\nКонецПроцедуры\n",
            "\tХ = 1;",
            "КонецПроцедуры\nПроцедура Б()\n",
        ),
        (
            Refusal::NewUnclosed,
            "Процедура А()\n\tХ = 1;\nКонецПроцедуры\nПроцедура Б()\nКонецПроцедуры\n",
            "КонецПроцедуры\nПроцедура Б",
            "\"",
        ),
        (
            // `#Если` без `#КонецЕсли` и без перевода строки в конце: ошибку
            // внешнего правила сток ставит на конец последнего токена метода,
            // и фрагмент её не воспроизведёт.
            Refusal::ContextError,
            "#Если Сервер Тогда\nПроцедура А()\n\tУ = 2;\nКонецПроцедуры",
            "\tУ = 2;",
            "З = 3;",
        ),
    ];
    const SECOND_LINE: &[Refusal] =
        &[Refusal::OldUnclosed, Refusal::TokenBoundary, Refusal::NewUnclosed];
    let mut without_all = Guards::ALL;
    for refusal in Refusal::ALL {
        without_all = without_all.without(refusal);
    }

    for (refusal, old_text, anchor, insert) in cases {
        let at = old_text.find(anchor).expect("якорь есть в тексте");
        let new_text = format!("{}{}{}", &old_text[..at], insert, &old_text[at..]);
        let old = parser::parse_with_shared_cache(old_text);
        let edit = edit_between(old_text, &new_text).unwrap();
        let full = parser::parse_with_shared_cache(&new_text);

        let guarded = reparse_method(&old, old_text, &new_text, edit);
        assert_eq!(
            guarded.err(),
            Some(*refusal),
            "{refusal:?}: гард обязан отказать на {new_text:?}"
        );

        let unguarded = reparse_method_guarded(&old, old_text, &new_text, edit, without_all)
            .unwrap_or_else(|other| {
                panic!("{refusal:?}: без гардов отказал {other:?} на {new_text:?}")
            });
        assert!(
            !(syntax::green_eq(unguarded.green(), full.green())
                && unguarded.errors() == full.errors()),
            "{refusal:?}: без гардов дерево всё равно совпало с полным разбором — вход не опасен"
        );

        if !SECOND_LINE.contains(refusal) {
            let alone = reparse_method_guarded(&old, old_text, &new_text, edit, Guards::ALL.without(*refusal))
                .unwrap_or_else(|other| panic!("{refusal:?}: без этого гарда отказал другой ({other:?}) — вход ловит не только он"));
            assert!(
                !(syntax::green_eq(alone.green(), full.green()) && alone.errors() == full.errors()),
                "{refusal:?}: без этого гарда дерево совпало с полным разбором — гард лишний"
            );
        }
    }

    // Определяющие отказы: узла метода нет — вклеивать некуда; вид сменился.
    // Вставка целого метода между методами сюда не годится: диф переякоривает
    // её внутрь соседа по общему префиксу «Процедура », и ловит уже `Shape`.
    let old_text = "Перем А;\n\nПроцедура Б()\n\tХ = 1;\nКонецПроцедуры\n";
    let new_text = format!("Перем В;\n{old_text}");
    let old = parser::parse_with_shared_cache(old_text);
    let edit = edit_between(old_text, &new_text).unwrap();
    assert_eq!(
        reparse_method_guarded(&old, old_text, &new_text, edit, without_all).err(),
        Some(Refusal::OutsideMethod)
    );
    let new_text = old_text.replacen("Процедура Б", "Функция Б", 1);
    let edit = edit_between(old_text, &new_text).unwrap();
    assert_eq!(
        reparse_method_guarded(&old, old_text, &new_text, edit, without_all).err(),
        Some(Refusal::Kind)
    );
}

#[test]
fn inline_directive_edit_is_reparsed_locally_and_matches_full_parse() {
    let old_text = r#"Функция Тест(Данные)
    Возврат Новый Структура(
        "Ссылка",
        #Удаление
        Данные.Ссылка,
        #КонецУдаления
        #Вставка
        Данные.НоваяСсылка);
        #КонецВставки
КонецФункции"#;
    let new_text = old_text.replace("НоваяСсылка", "АктуальнаяСсылка");
    let old = parser::parse_with_shared_cache(old_text);
    let edit = edit_between(old_text, &new_text).expect("name edit");
    let spliced = reparse_method(&old, old_text, &new_text, edit)
        .expect("an active-name edit inside balanced markers must use local reparse");
    let full = parser::parse_with_shared_cache(&new_text);
    assert_same(&spliced, &full, &new_text, "inline active-name edit");
    assert!(spliced.syntax_node().text().to_string().contains("АктуальнаяСсылка"));
}

#[test]
fn deleting_inline_closing_marker_refuses_local_reparse() {
    let old_text = r#"Функция Тест()
    Возврат 1
    #Вставка
    + 2
    #КонецВставки;
КонецФункции"#;
    let new_text = old_text.replace("#КонецВставки", "");
    let old = parser::parse_with_shared_cache(old_text);
    let edit = edit_between(old_text, &new_text).expect("marker deletion");
    assert_eq!(
        reparse_method(&old, old_text, &new_text, edit).err(),
        Some(Refusal::Preprocessor),
        "an unbalanced method must fall back to the authoritative full parse"
    );
    assert!(
        parser::parse_with_shared_cache(&new_text).has_errors(),
        "the full parse must expose the now-unclosed insertion"
    );
}

#[test]
fn edit_between_finds_the_change() {
    assert_eq!(edit_between("абв", "абв"), None);
    let e = edit_between("aa", "aaa").unwrap();
    assert_eq!(usize::from(e.old_range.len()), 0);
    assert_eq!(usize::from(e.new_range.len()), 1);
    let e = edit_between("", "x").unwrap();
    assert_eq!(
        (e.old_range, e.new_range),
        (TextRange::new(0.into(), 0.into()), TextRange::new(0.into(), 1.into()))
    );
    let e = edit_between("x", "").unwrap();
    assert_eq!(
        (e.old_range, e.new_range),
        (TextRange::new(0.into(), 1.into()), TextRange::new(0.into(), 0.into()))
    );
    // Замена внутри длинного текста находится блоками.
    let old: String = "а".repeat(5000) + "x" + &"б".repeat(5000);
    let new: String = "а".repeat(5000) + "yz" + &"б".repeat(5000);
    let e = edit_between(&old, &new).unwrap();
    assert_eq!(usize::from(e.old_range.start()), 10000);
    assert_eq!(usize::from(e.old_range.len()), 1);
    assert_eq!(usize::from(e.new_range.len()), 2);
    let _ = TextEdit { old_range: e.old_range, new_range: e.new_range };
}

#[test]
fn heap_bytes_equals_the_tree_walk() {
    for text in SNIPPETS.iter().copied().chain(std::iter::once(MODULE)) {
        let parse = parser::parse(text);
        let walked: usize = parse
            .syntax_node()
            .descendants_with_tokens()
            .map(|el| match el {
                syntax::NodeOrToken::Node(_) => syntax::HEAP_PER_ELEMENT,
                syntax::NodeOrToken::Token(t) => syntax::HEAP_PER_ELEMENT + t.text().len(),
            })
            .sum();
        assert_eq!(parse.heap_bytes(), walked);
    }
}

/// `green_eq` отвечает как структурное `==`: равные деревья с разными
/// указателями — `true`, отличающиеся — `false`, один и тот же узел — `true`
/// без обхода.
#[test]
fn green_eq_agrees_with_structural_equality() {
    for text in SNIPPETS.iter().copied().chain(std::iter::once(MODULE)) {
        // Без общего кэша узлов два разбора не делят ни одного указателя.
        let a = parser::parse(text);
        let b = parser::parse(text);
        let (a_data, b_data): (&syntax::GreenNodeData, &syntax::GreenNodeData) =
            (a.green(), b.green());
        assert!(!std::ptr::eq(a_data, b_data), "контроль: указатели различны");
        assert!(syntax::green_eq(a.green(), b.green()));
        assert_eq!(a.green(), b.green());
        assert_eq!(a, b);
        let edited = format!("{text}\nПроцедура Ещё()\nКонецПроцедуры\n");
        let c = parser::parse(&edited);
        assert!(!syntax::green_eq(a.green(), c.green()));
        assert_ne!(a.green(), c.green());
        assert_ne!(a, c);
        assert!(syntax::green_eq(a.green(), &a.green().clone()));
    }
}
