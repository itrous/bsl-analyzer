//! Правило сравнения регистра у реестра символов препроцессора обязано
//! совпадать с `(?i)` лексера, иначе анализ и токенизатор расходятся в том,
//! какое слово перед ними.
//!
//! Сверка идёт по всему пространству скалярных значений Unicode и для каждой
//! буквы латиницы и русского блока: `crates/syntax/src/preproc_symbols.rs`
//! держит отдельный тест, требующий, чтобы алфавит реестра не выходил за этот
//! набор.
//!
//! Классы `(?i)` снимаются с самого `logos` — тем же порождателем и тем же
//! флагом, на которых написан `TokenKind`; отдельный тест ниже закрепляет
//! ответ настоящего лексера BSL на представителях краёв.

use lexer::{tokenize, TokenKind};
use logos::Logos;
use stdx::case::fold_case;

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Letter {
    #[regex(r"(?i)a")]
    L0,
    #[regex(r"(?i)b")]
    L1,
    #[regex(r"(?i)c")]
    L2,
    #[regex(r"(?i)d")]
    L3,
    #[regex(r"(?i)e")]
    L4,
    #[regex(r"(?i)f")]
    L5,
    #[regex(r"(?i)g")]
    L6,
    #[regex(r"(?i)h")]
    L7,
    #[regex(r"(?i)i")]
    L8,
    #[regex(r"(?i)j")]
    L9,
    #[regex(r"(?i)k")]
    L10,
    #[regex(r"(?i)l")]
    L11,
    #[regex(r"(?i)m")]
    L12,
    #[regex(r"(?i)n")]
    L13,
    #[regex(r"(?i)o")]
    L14,
    #[regex(r"(?i)p")]
    L15,
    #[regex(r"(?i)q")]
    L16,
    #[regex(r"(?i)r")]
    L17,
    #[regex(r"(?i)s")]
    L18,
    #[regex(r"(?i)t")]
    L19,
    #[regex(r"(?i)u")]
    L20,
    #[regex(r"(?i)v")]
    L21,
    #[regex(r"(?i)w")]
    L22,
    #[regex(r"(?i)x")]
    L23,
    #[regex(r"(?i)y")]
    L24,
    #[regex(r"(?i)z")]
    L25,
    #[regex(r"(?i)а")]
    L26,
    #[regex(r"(?i)б")]
    L27,
    #[regex(r"(?i)в")]
    L28,
    #[regex(r"(?i)г")]
    L29,
    #[regex(r"(?i)д")]
    L30,
    #[regex(r"(?i)е")]
    L31,
    #[regex(r"(?i)ж")]
    L32,
    #[regex(r"(?i)з")]
    L33,
    #[regex(r"(?i)и")]
    L34,
    #[regex(r"(?i)й")]
    L35,
    #[regex(r"(?i)к")]
    L36,
    #[regex(r"(?i)л")]
    L37,
    #[regex(r"(?i)м")]
    L38,
    #[regex(r"(?i)н")]
    L39,
    #[regex(r"(?i)о")]
    L40,
    #[regex(r"(?i)п")]
    L41,
    #[regex(r"(?i)р")]
    L42,
    #[regex(r"(?i)с")]
    L43,
    #[regex(r"(?i)т")]
    L44,
    #[regex(r"(?i)у")]
    L45,
    #[regex(r"(?i)ф")]
    L46,
    #[regex(r"(?i)х")]
    L47,
    #[regex(r"(?i)ц")]
    L48,
    #[regex(r"(?i)ч")]
    L49,
    #[regex(r"(?i)ш")]
    L50,
    #[regex(r"(?i)щ")]
    L51,
    #[regex(r"(?i)ъ")]
    L52,
    #[regex(r"(?i)ы")]
    L53,
    #[regex(r"(?i)ь")]
    L54,
    #[regex(r"(?i)э")]
    L55,
    #[regex(r"(?i)ю")]
    L56,
    #[regex(r"(?i)я")]
    L57,
    #[regex(r"(?i)ё")]
    L58,
}

const LETTERS: &[(Letter, char)] = &[
    (Letter::L0, 'a'),
    (Letter::L1, 'b'),
    (Letter::L2, 'c'),
    (Letter::L3, 'd'),
    (Letter::L4, 'e'),
    (Letter::L5, 'f'),
    (Letter::L6, 'g'),
    (Letter::L7, 'h'),
    (Letter::L8, 'i'),
    (Letter::L9, 'j'),
    (Letter::L10, 'k'),
    (Letter::L11, 'l'),
    (Letter::L12, 'm'),
    (Letter::L13, 'n'),
    (Letter::L14, 'o'),
    (Letter::L15, 'p'),
    (Letter::L16, 'q'),
    (Letter::L17, 'r'),
    (Letter::L18, 's'),
    (Letter::L19, 't'),
    (Letter::L20, 'u'),
    (Letter::L21, 'v'),
    (Letter::L22, 'w'),
    (Letter::L23, 'x'),
    (Letter::L24, 'y'),
    (Letter::L25, 'z'),
    (Letter::L26, 'а'),
    (Letter::L27, 'б'),
    (Letter::L28, 'в'),
    (Letter::L29, 'г'),
    (Letter::L30, 'д'),
    (Letter::L31, 'е'),
    (Letter::L32, 'ж'),
    (Letter::L33, 'з'),
    (Letter::L34, 'и'),
    (Letter::L35, 'й'),
    (Letter::L36, 'к'),
    (Letter::L37, 'л'),
    (Letter::L38, 'м'),
    (Letter::L39, 'н'),
    (Letter::L40, 'о'),
    (Letter::L41, 'п'),
    (Letter::L42, 'р'),
    (Letter::L43, 'с'),
    (Letter::L44, 'т'),
    (Letter::L45, 'у'),
    (Letter::L46, 'ф'),
    (Letter::L47, 'х'),
    (Letter::L48, 'ц'),
    (Letter::L49, 'ч'),
    (Letter::L50, 'ш'),
    (Letter::L51, 'щ'),
    (Letter::L52, 'ъ'),
    (Letter::L53, 'ы'),
    (Letter::L54, 'ь'),
    (Letter::L55, 'э'),
    (Letter::L56, 'ю'),
    (Letter::L57, 'я'),
    (Letter::L58, 'ё'),
];

/// Класс `(?i)`, к которому `logos` относит один символ, или `None`.
fn lexer_class(c: char) -> Option<char> {
    let mut buf = [0u8; 4];
    let text: &str = c.encode_utf8(&mut buf);
    let mut lex = Letter::lexer(text);
    match lex.next() {
        Some(Ok(matched)) if lex.span() == (0..text.len()) => {
            LETTERS.iter().find(|(v, _)| *v == matched).map(|(_, letter)| *letter)
        }
        _ => None,
    }
}

/// Класс, к которому символ относит `fold_case`, среди тех же букв.
fn folded_class(c: char) -> Option<char> {
    let folded = fold_case(c);
    LETTERS.iter().find(|(_, letter)| fold_case(*letter) == folded).map(|(_, letter)| *letter)
}

/// Каждое скалярное значение Unicode складывается `fold_case` в тот же класс,
/// в какой его кладёт `(?i)`.
///
/// Обе стороны обязательны: одно только «буква узнаётся» зелено и у
/// реализации, которая складывает слишком много. Положительный контроль —
/// `ſ` (U+017F), `K` (U+212A) и историческая кириллица U+1C80..U+1C86, которые
/// `to_lowercase` в класс не кладёт, и `ı` (U+0131), который в класс `i` не
/// входит, хотя в верхнем регистре даёт `I`.
#[test]
fn fold_case_agrees_with_the_lexers_case_insensitive_flag() {
    let mut mismatches = Vec::new();
    for c in ('\0'..=char::MAX).filter(|c| !(0xD800..=0xDFFF).contains(&(*c as u32))) {
        let (by_lexer, by_fold) = (lexer_class(c), folded_class(c));
        if by_lexer != by_fold {
            mismatches.push(format!(
                "U+{:04X} {c:?}: лексер {by_lexer:?}, fold_case {by_fold:?}",
                c as u32
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "правило сравнения разошлось с (?i):\n{}",
        mismatches.join("\n")
    );

    // Контроль чувствительности: перечисленные края действительно есть в
    // проверенном пространстве, а не отсутствуют в нём вместе с проблемой.
    assert_eq!(lexer_class('\u{17F}'), Some('s'));
    assert_eq!(lexer_class('\u{212A}'), Some('k'));
    assert_eq!(lexer_class('\u{1C83}'), Some('с'));
    assert_eq!(lexer_class('\u{131}'), None);
}

/// Настоящий лексер BSL на тех же краях: сверка выше снимает классы с `logos`
/// напрямую, и без этой пары `TokenKind` мог бы жить по другому правилу.
#[test]
fn the_bsl_lexer_itself_folds_by_the_same_rule() {
    for (input, expected) in [
        ("#El\u{17F}if", TokenKind::PreElsIf),
        ("#Е\u{1C83}ли", TokenKind::PreIf),
        ("El\u{17F}e", TokenKind::KwElse),
        ("fal\u{17F}e", TokenKind::KwFalse),
        ("brea\u{212A}", TokenKind::KwBreak),
        ("#Если", TokenKind::PreIf),
        ("Else", TokenKind::KwElse),
    ] {
        let tokens = tokenize(input);
        assert_eq!(tokens[0].kind, expected, "{input:?} разобран иначе");
        assert_eq!(tokens[0].text.as_str(), input, "{input:?} разобран не целиком");
    }
    // Турецкая точечная `ı` ключевым словом не становится.
    assert_eq!(tokenize("\u{131}f")[0].kind, TokenKind::Ident);
}
