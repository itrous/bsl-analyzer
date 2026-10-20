//! `line_tokens` против токенов дерева: лексер без состояния и вид у лексемы.
//!
//! Плита строчных диагностик судит блок целых строк по токенам его текста, не
//! разбирая файл. Это законно ровно настолько, насколько токены текста,
//! начатого с начала строки, совпадают с токенами файла на тех же строках, —
//! и это свойство держит корпус, а не рассуждение.

use syntax::{LineToken, NodeOrToken, SyntaxKind, TextRange, TextSize};

const MODULE: &str = include_str!("../fixtures/Module.bsl");

/// Входы, на которых лексер мог бы зависеть от контекста: BOM не в начале,
/// `\r` без `\n`, незакрытые строки и голые продолжения, `//` внутри
/// литерала, даты с апострофами, `#`/`&`/`~`/`?` вне своих конструкций,
/// два метода на одной строке, пустой текст и одни переводы строк.
const SNIPPETS: &[&str] = &[
    "Процедура А()\r\n\tХ = \"abc\r\n\t|def\";\r\nКонецПроцедуры\r\n",
    "\u{feff}Процедура А()\nКонецПроцедуры",
    "Процедура А()\n\tХ = \"abc\n// внутри строки\n\t|def\";\nКонецПроцедуры // хвост\n// ещё\n",
    "Х = 'abc' + '20240101';\nД = '2024\\01\\01';",
    "Процедура А()\n#Вставка\nХ = 1;\n#КонецВставки\nКонецПроцедуры\n",
    "#Если Сервер Тогда\nПроцедура А()\nКонецПроцедуры\n#КонецЕсли",
    "Х = ;\nЕсли Тогда\n(((\n",
    "Процедура А() КонецПроцедуры Процедура Б()\nКонецПроцедуры",
    "Х = \"незакрытая",
    "|голая часть\n|\"хвост\"",
    "# не директива\n& не аннотация\n~метка:\n?\n",
    "Х = 1 \r без перевода строки \r Y = 2",
    "// комментарий без перевода строки в конце",
    "Х = 1;\n\u{feff}Y = 2;",
    "",
    "\n\n\n",
];

fn tree_tokens(text: &str) -> Vec<LineToken> {
    parser::parse(text)
        .syntax_node()
        .descendants_with_tokens()
        .filter_map(|element| match element {
            NodeOrToken::Token(token) => {
                Some(LineToken { kind: token.kind(), range: token.text_range() })
            }
            NodeOrToken::Node(_) => None,
        })
        .collect()
}

fn shifted(tokens: Vec<LineToken>, by: usize) -> Vec<LineToken> {
    let by = TextSize::new(by as u32);
    tokens.into_iter().map(|t| LineToken { kind: t.kind, range: t.range + by }).collect()
}

fn line_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    text.char_indices().filter(|&(_, ch)| ch == '\n').map(|(i, _)| i + 1)
}

#[test]
fn line_tokens_equal_the_tree_tokens() {
    let mut kinds = std::collections::BTreeSet::new();
    for (name, text) in
        std::iter::once(("Module.bsl", MODULE)).chain(SNIPPETS.iter().map(|s| ("snippet", *s)))
    {
        let tree = tree_tokens(text);
        let lexed = parser::line_tokens(text);
        assert_eq!(lexed, tree, "{name}: токены текста и дерева разошлись:\n{text}");
        kinds.extend(lexed.iter().map(|t| t.kind));
    }
    // Вакуумность: корпус обязан содержать все классы токенов, на которых
    // лексер мог бы вести себя контекстно.
    for kind in [
        SyntaxKind::STRING_START,
        SyntaxKind::STRING_PART,
        SyntaxKind::STRING_TAIL,
        SyntaxKind::COMMENT,
        SyntaxKind::BOM,
        SyntaxKind::DATE,
        SyntaxKind::ERROR,
    ] {
        assert!(kinds.contains(&kind), "в корпусе нет токена {kind:?}");
    }
}

/// Разрез по началу любой строки не меняет токенов: две половины дают ровно
/// токены целого. На фикстуре — каждое 97-е начало строки, на сниппетах — все.
#[test]
fn tokens_are_invariant_under_cuts_at_line_starts() {
    let mut cuts = 0usize;
    for (every, text) in
        std::iter::once((97usize, MODULE)).chain(SNIPPETS.iter().map(|s| (1usize, *s)))
    {
        let whole = parser::line_tokens(text);
        for (n, start) in line_starts(text).enumerate() {
            if start >= text.len() || n % every != 0 {
                continue;
            }
            let mut joined = parser::line_tokens(&text[..start]);
            joined.extend(shifted(parser::line_tokens(&text[start..]), start));
            assert_eq!(joined, whole, "разрез по смещению {start} изменил токены");
            cuts += 1;
        }
    }
    assert!(cuts > 100, "разрезов слишком мало, чтобы что-то сторожить: {cuts}");
}

#[test]
fn ranges_cover_the_text_without_gaps() {
    for text in std::iter::once(MODULE).chain(SNIPPETS.iter().copied()) {
        let mut cursor = TextSize::new(0);
        for token in parser::line_tokens(text) {
            assert_eq!(token.range.start(), cursor, "разрыв перед {:?}", token.range);
            cursor = token.range.end();
        }
        assert_eq!(cursor, TextSize::of(text));
        let _ = TextRange::empty(cursor);
    }
}
