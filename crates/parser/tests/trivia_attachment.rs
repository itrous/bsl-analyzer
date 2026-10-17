//! Норма привязки тривии: тривия принадлежит общему предку соседних значимых
//! токенов, а не тому правилу грамматики, которое её съело.
//!
//! Норма объявлена в `Sink` и держится его устройством, поэтому свойства ниже
//! проверяются на дереве, а не на отдельных правилах: правило, съевшее тривию
//! ради заглядывания вперёд, на дерево влиять не должно вообще.
//!
//! Входы подобраны так, чтобы нарушение было ВИДНО. Свойство «узел не кончается
//! тривией» зелено на любом входе, где за каждым узлом сразу идёт значимый
//! токен, поэтому корпус дополнен случаями, где тривия стоит на краю узла по
//! построению: незакрытые дословные тела, многострочный литерал, `Исключение`.

mod common;

use common::{generate, Rng};
use parser::{parse, parse_sdbl};
use syntax::{NodeOrToken, SyntaxNode, TextRange};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Bsl,
    Sdbl,
}

struct GateInput {
    name: String,
    lang: Lang,
    text: String,
}

impl GateInput {
    fn tree(&self) -> SyntaxNode {
        match self.lang {
            Lang::Bsl => parse(&self.text).syntax_node(),
            Lang::Sdbl => parse_sdbl(&self.text).syntax_node(),
        }
    }
}

/// Хвостовая тривия дословного тела, у которого не оказалось закрывающего
/// маркера: цикл `preprocessor_insert` допускает конец файла, поэтому перевод
/// строки и пробелы остаются последними токенами узла.
const UNCLOSED_INSERT: &str = "#Вставка\n  ";

/// То же для `#Удаление`, но с непустым телом: важно, что тривия на краю
/// приходит из тела, а не из самой директивы.
const UNCLOSED_DELETE: &str = "#Удаление\n\tА = 1;\n\t";

/// Многострочный литерал: `Newline`, `Whitespace` и `Comment` здесь — ТЕЛО
/// литерала, а не тривия, и по виду токена сток их от тривии не отличает.
/// Вход держит норму только потому, что за ними идёт значимый токен.
const MULTILINE_LITERAL: &str =
    "Процедура П()\n\tА = \"начало\n\t|середина\n\t|конец\";\nКонецПроцедуры\n";

/// Тот же литерал без закрывающей кавычки: за телом уже ничего не идёт.
const UNCLOSED_MULTILINE_LITERAL: &str = "Процедура П()\n\tА = \"начало\n\t|середина\n";

/// Единственный вид с ведущей тривией: `p.start()` вызывается раньше
/// `p.skip_trivia()`, поэтому перевод строки после `Исключение` попадает
/// внутрь уже открытого узла.
const EXCEPT_CLAUSE: &str =
    "Процедура П()\n\tПопытка\n\t\tА = 1;\n\tИсключение\n\t\tБ = 2;\n\tКонецПопытки;\nКонецПроцедуры\n";

/// Комментарий между выражением и точкой с запятой: вход, на котором видно
/// сужение диапазона у потребителей соседства с `;`.
const COMMENT_BEFORE_SEMICOLON: &str =
    "Процедура П()\n\tА = 1 // комментарий\n\t;\nКонецПроцедуры\n";

/// Незакрытое расширение запроса: цикл `query_extension` допускает конец
/// ввода, поэтому хвостовая тривия остаётся внутри узла.
const SDBL_UNCLOSED_EXTENSION: &str = "ВЫБРАТЬ А ИЗ Т {ГДЕ Т.Поле\n  ";

/// Вложенные пустые узлы: `SDBL_SELECT_QUERY > SDBL_SUBQUERY > SDBL_QUERY >
/// ERROR` закрываются подряд, не получив ни одного значимого токена. Если
/// отложенные открытия выполнять по одному за `Finish`, вложенность
/// разложится в плоскую цепочку — свойства диапазонов этого не видят.
const SDBL_EMPTY_NESTED: &str = "FROM Products";

const NAMED_INPUTS: &[(&str, Lang, &str)] = &[
    ("unclosed-insert", Lang::Bsl, UNCLOSED_INSERT),
    ("unclosed-delete", Lang::Bsl, UNCLOSED_DELETE),
    ("multiline-literal", Lang::Bsl, MULTILINE_LITERAL),
    ("unclosed-multiline-literal", Lang::Bsl, UNCLOSED_MULTILINE_LITERAL),
    ("except-clause", Lang::Bsl, EXCEPT_CLAUSE),
    ("comment-before-semicolon", Lang::Bsl, COMMENT_BEFORE_SEMICOLON),
    ("sdbl-unclosed-extension", Lang::Sdbl, SDBL_UNCLOSED_EXTENSION),
    ("sdbl-empty-nested", Lang::Sdbl, SDBL_EMPTY_NESTED),
    ("module", Lang::Bsl, include_str!("fixtures/Module.bsl")),
    ("independent-query", Lang::Sdbl, include_str!("fixtures/sdbl_independent_package.sdbl")),
];

fn gate_inputs() -> Vec<GateInput> {
    let mut inputs: Vec<GateInput> = NAMED_INPUTS
        .iter()
        .map(|(name, lang, text)| GateInput {
            name: (*name).to_string(),
            lang: *lang,
            text: (*text).to_string(),
        })
        .collect();

    for seed in 1..=100u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for pieces in [1usize, 2, 3, 5, 8, 13] {
            inputs.push(GateInput {
                name: format!("generated/{seed}/{pieces}"),
                lang: Lang::Sdbl,
                text: generate(&mut rng, pieces),
            });
        }
    }

    inputs
}

/// И1. Ни один узел, кроме корня, не начинается и не кончается тривией.
///
/// Корень исключён по необходимости: он обязан содержать хвост файла, поэтому
/// без исключения свойство ложно на любом входе, кончающемся переводом строки.
///
/// Проверяются прямые дети: если краевой токен поддерева лежит внутри
/// вложенного узла, нарушителем является тот узел, и он проверяется сам.
#[test]
fn no_node_but_the_root_starts_or_ends_with_trivia() {
    let mut breaches = Vec::new();

    for input in gate_inputs() {
        let root = input.tree();
        for node in root.descendants() {
            if node == root {
                continue;
            }
            let mut edges = node.children_with_tokens();
            let first = edges.next();
            let last = node.children_with_tokens().last();
            for (edge, token) in [("начинается", first), ("кончается", last)] {
                let Some(NodeOrToken::Token(token)) = token else {
                    continue;
                };
                if token.kind().is_trivia() {
                    breaches.push(format!(
                        "{}: {:?}@{:?} {} с {:?}",
                        input.name,
                        node.kind(),
                        node.text_range(),
                        edge,
                        token.kind()
                    ));
                }
            }
        }
    }

    assert!(
        breaches.is_empty(),
        "узлов с тривией на краю: {}\n{}",
        breaches.len(),
        head(&breaches)
    );
}

/// И2. Конкатенация текстов всех токенов дерева равна исходному тексту.
///
/// Свойство существует ради перестановки: правка переносит тривию между
/// узлами, и проверка по ДЛИНЕ дерева, которая стоит в
/// `sdbl_parser_invariants.rs`, перестановку не видит вовсе.
///
/// Для SDBL эталон — текст токенов, отданных лексером, а не вход: лексер сам
/// теряет байты на сериях кавычек, и это дефект не парсера.
#[test]
fn tree_text_equals_the_input_text() {
    let mut breaches = Vec::new();

    for input in gate_inputs() {
        let expected = match input.lang {
            Lang::Bsl => input.text.clone(),
            Lang::Sdbl => {
                lexer::sdbl::tokenize_sdbl(&input.text).iter().map(|t| t.text.as_str()).collect()
            }
        };
        let actual = input.tree().text().to_string();
        if actual != expected {
            breaches.push(format!(
                "{}: дерево держит {} байт из {}",
                input.name,
                actual.len(),
                expected.len()
            ));
        }
    }

    assert!(breaches.is_empty(), "текст потерян или переставлен:\n{}", head(&breaches));
}

/// И3. Диапазон узла, кроме корня, равен диапазону его значимых токенов.
///
/// Ловит случай, когда тривия уехала не наружу, а не туда: узел без единого
/// значимого токена обязан быть пустым, а не покрывать чужую тривию.
#[test]
fn node_range_equals_the_range_of_its_significant_tokens() {
    let mut breaches = Vec::new();

    for input in gate_inputs() {
        let root = input.tree();
        significant_span(&root, true, &input.name, &mut breaches);
    }

    assert!(
        breaches.is_empty(),
        "узлов с чужой тривией в диапазоне: {}\n{}",
        breaches.len(),
        head(&breaches)
    );
}

/// Диапазон значимых токенов поддерева, считаемый снизу вверх за один обход:
/// проверка каждого узла отдельным сканом квадратична на фикстуре в мегабайт.
fn significant_span(
    node: &SyntaxNode,
    is_root: bool,
    input: &str,
    breaches: &mut Vec<String>,
) -> Option<TextRange> {
    let mut span: Option<TextRange> = None;

    for edge in node.children_with_tokens() {
        let edge_span = match edge {
            NodeOrToken::Token(token) => (!token.kind().is_trivia()).then(|| token.text_range()),
            NodeOrToken::Node(child) => significant_span(&child, false, input, breaches),
        };
        if let Some(edge_span) = edge_span {
            span = Some(span.map_or(edge_span, |span: TextRange| span.cover(edge_span)));
        }
    }

    if !is_root {
        let expected = span.unwrap_or_else(|| TextRange::empty(node.text_range().start()));
        if span.is_none() && !node.text_range().is_empty() {
            breaches.push(format!(
                "{input}: {:?}@{:?} без единого значимого токена, но не пуст",
                node.kind(),
                node.text_range()
            ));
        } else if span.is_some() && node.text_range() != expected {
            breaches.push(format!(
                "{input}: {:?}@{:?} против значимых {:?}",
                node.kind(),
                node.text_range(),
                expected
            ));
        }
    }

    span
}

fn head(breaches: &[String]) -> String {
    breaches.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
}

/// И4. Сдвиг диапазонов непустых узлов — только сужение, а состав и
/// вложенность узлов не меняются вовсе.
///
/// Свойства И1–И3 говорят, где тривия лежать не должна, но молчат о том, куда
/// она делась: дерево, потерявшее вложенность или узел, их проходит. Эталон
/// снят до правки и хранится обычным файлом, а не снапшотом `expect!`, потому
/// что штатный `UPDATE_EXPECT=1 cargo test` перезаписал бы его вместе с
/// остальными и гейт стал бы зелёным навсегда. Обновлять руками и с причиной.
///
/// Пустые узлы исключены: точку привязки им даёт длина уже отданного билдеру
/// текста, поэтому отложенное открытие их сдвигает, а не сужает.
///
/// Фикстуры `Module.bsl` здесь нет: её 74 674 узла дали бы эталон на два
/// мегабайта. Сужение на ней проверено разово при самой правке, а постоянно
/// её держат И1–И3.
///
/// Для `independent-query` эталон фиксирует форму самостоятельного входа,
/// а не историческое сужение диапазонов другого текста.
#[test]
fn ranges_only_narrow_against_the_recorded_baseline() {
    let baseline = parse_baseline(include_str!("fixtures/trivia_ranges_before.txt"));
    let mut breaches = Vec::new();
    let mut seen = 0usize;

    for input in gate_inputs() {
        if !baseline.iter().any(|(name, _)| name == &input.name) {
            continue;
        }
        seen += 1;
        let before = baseline.iter().find(|(name, _)| name == &input.name).map(|(_, s)| s).unwrap();
        let now = shape(&input.tree());

        if now.len() != before.len() {
            breaches.push(format!(
                "{}: узлов было {}, стало {}",
                input.name,
                before.len(),
                now.len()
            ));
            continue;
        }

        for (was, is) in before.iter().zip(now.iter()) {
            if was.depth != is.depth || was.kind != is.kind {
                breaches.push(format!(
                    "{}: был {} {:?} на глубине {}, стал {} на глубине {}",
                    input.name, was.kind, was.range, was.depth, is.kind, is.depth
                ));
                break;
            }
            if !was.range.is_empty() && !was.range.contains_range(is.range) {
                breaches.push(format!(
                    "{}: {} {:?} не сузился, а стал {:?}",
                    input.name, was.kind, was.range, is.range
                ));
            }
        }
    }

    assert_eq!(seen, baseline.len(), "эталон описывает вход, которого в гейте больше нет");
    assert!(breaches.is_empty(), "сдвиг не является сужением:\n{}", head(&breaches));
}

#[test]
fn multiline_bsl_literal_preserves_tree_text_and_following_identifier_ranges() {
    for newline in ["\n", "\r\n"] {
        let input = format!(
            "Процедура П(){newline}    А = \"начало{newline}    |середина\";{newline}    После = 1;{newline}КонецПроцедуры{newline}"
        );
        let root = parse(&input).syntax_node();
        assert_eq!(root.text().to_string(), input);
        assert_eq!(u32::from(root.text_range().len()) as usize, input.len());

        let expected_start = input.find("После").unwrap();
        let token = root
            .descendants_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .find(|token| token.kind() == syntax::SyntaxKind::IDENT && token.text() == "После")
            .unwrap();
        assert_eq!(
            token.text_range(),
            TextRange::new(
                (expected_start as u32).into(),
                ((expected_start + "После".len()) as u32).into()
            ),
            "newline={newline:?}"
        );
    }
}

struct NodeShape {
    depth: usize,
    kind: String,
    range: TextRange,
}

fn shape(root: &SyntaxNode) -> Vec<NodeShape> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    for event in root.preorder() {
        match event {
            syntax::WalkEvent::Enter(node) => {
                out.push(NodeShape {
                    depth,
                    kind: format!("{:?}", node.kind()),
                    range: node.text_range(),
                });
                depth += 1;
            }
            syntax::WalkEvent::Leave(_) => depth -= 1,
        }
    }
    out
}

/// Пересъёмка эталона И4.
///
/// Ручка своя, а не `UPDATE_EXPECT`: тот принимает новые снапшоты пачкой и
/// заодно принял бы эталон, ради которого гейт и заведён. Снимать заново
/// только осознанно и с объяснением, зачем диапазоны разъехались.
#[test]
fn baseline_is_rerecorded_only_on_demand() {
    if std::env::var_os("RECORD_TRIVIA_BASELINE").is_none() {
        return;
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/trivia_ranges_before.txt");
    std::fs::write(path, render_baseline(&gate_inputs())).expect("эталон не записан");
}

fn render_baseline(inputs: &[GateInput]) -> String {
    let mut out = String::new();
    for input in inputs {
        if input.name == "module" {
            continue;
        }
        out.push_str(&format!("# {}\n", input.name));
        for node in shape(&input.tree()) {
            out.push_str(&format!(
                "{} {} {} {}\n",
                node.depth,
                node.kind,
                u32::from(node.range.start()),
                u32::from(node.range.end())
            ));
        }
    }
    out
}

fn parse_baseline(text: &str) -> Vec<(String, Vec<NodeShape>)> {
    let mut out: Vec<(String, Vec<NodeShape>)> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("# ") {
            out.push((name.to_string(), Vec::new()));
            continue;
        }
        let mut parts = line.split(' ');
        let depth = parts.next().expect("эталон: глубина").parse().expect("эталон: глубина");
        let kind = parts.next().expect("эталон: вид узла").to_string();
        let start: u32 = parts.next().expect("эталон: начало").parse().expect("эталон: начало");
        let end: u32 = parts.next().expect("эталон: конец").parse().expect("эталон: конец");
        let range = TextRange::new(start.into(), end.into());
        out.last_mut().expect("эталон: узел вне секции").1.push(NodeShape { depth, kind, range });
    }
    out
}
