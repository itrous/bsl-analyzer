//! Серии комментариев: строки `//`, стоящие на соседних строках.
//!
//! Правила серии нужны и складке `foldingRange`, и диагностике
//! закомментированного кода, но требования у них разные: складке нужен
//! комментарий, владеющий своей строкой, диагностике — все подряд идущие,
//! включая хвостовой на строке с кодом. Поэтому примитив не решает за
//! потребителя, а отдаёт факты о тексте — соседство строк и владение строкой.
//!
//! Соседство выражается через переводы строки в токенах — `NEWLINE` и
//! одиночные `\r`, — а не через индекс строк: обход дерева и так идёт по
//! тексту, и номера строк тут ничего не добавляют.

use crate::{LineToken, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, TextRange};

/// Одна строка серии: диапазон токена `COMMENT` в тексте, откуда серия взята.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentLine {
    pub range: TextRange,
    /// `//` — первый непробельный символ своей строки.
    pub owns_line: bool,
}

/// Комментарии, стоящие на соседних строках.
#[derive(Debug, Clone)]
pub struct CommentRun {
    lines: Vec<CommentLine>,
    range: TextRange,
}

impl CommentRun {
    pub fn lines(&self) -> &[CommentLine] {
        &self.lines
    }

    /// От начала первого комментария до конца последнего; строка ниже серии не
    /// захватывается, иначе диапазон пересёкся бы с объявлением под шапкой.
    pub fn range(&self) -> TextRange {
        self.range
    }
}

/// A `//` token the lexer emitted as text of a multiline literal, not as a
/// comment. A continuation line that starts with `//` (no leading `|`) is still
/// `Comment`; what makes it string text is the open literal on the left and the
/// continuing literal, or the method's end, on the right.
pub fn comment_token_is_string_text(token: &SyntaxToken) -> bool {
    if token.kind() != SyntaxKind::COMMENT {
        return false;
    }
    is_string_text_between(non_trivia_kind(token, false), non_trivia_kind(token, true))
}

/// A `//` line of a multiline literal sits a few tokens from the string boundary.
/// Past that, the walk is a comment run: scanning it again from every line is
/// quadratic, and such a run is not string text.
const NEIGHBOR_WALK_LIMIT: u32 = 32;

fn non_trivia_kind(token: &SyntaxToken, forward: bool) -> Option<SyntaxKind> {
    let mut cursor = if forward { token.next_token() } else { token.prev_token() };
    let mut steps = 0u32;
    while let Some(current) = cursor {
        if !current.kind().is_trivia() {
            return Some(current.kind());
        }
        steps += 1;
        if steps >= NEIGHBOR_WALK_LIMIT {
            return None;
        }
        cursor = if forward { current.next_token() } else { current.prev_token() };
    }
    None
}

/// Все серии комментариев дерева в порядке текста.
///
/// Считается по токенам дерева тем же правилом, что и [`comment_runs_of`] по
/// токенам текста: у серии нет свойства, которого не было бы у потока
/// токенов, и два способа получить поток обязаны давать одну серию.
pub fn comment_runs(root: &SyntaxNode) -> Vec<CommentRun> {
    let mut tokens = root
        .descendants_with_tokens()
        .filter_map(|element| match element {
            NodeOrToken::Token(token) => Some(token),
            NodeOrToken::Node(_) => None,
        })
        .peekable();
    let mut line_tokens = Vec::new();
    let mut line_breaks = Vec::new();
    while let Some(token) = tokens.next() {
        let next = tokens.peek().map(SyntaxToken::kind);
        line_breaks.push(line_breaks_of(token.kind(), token.text(), next));
        line_tokens.push(LineToken { kind: token.kind(), range: token.text_range() });
    }
    runs_of(&line_tokens, &line_breaks)
}

/// Серии комментариев по токенам текста без дерева: для блока строк,
/// разобранного лексером отдельно от файла; `text` — текст, по которому
/// посчитаны `tokens`.
pub fn comment_runs_of(text: &str, tokens: &[LineToken]) -> Vec<CommentRun> {
    let line_breaks: Vec<usize> = tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            let next = tokens.get(index + 1).map(|next| next.kind);
            line_breaks_of(token.kind, &text[token.range], next)
        })
        .collect();
    runs_of(tokens, &line_breaks)
}

fn runs_of(tokens: &[LineToken], line_breaks: &[usize]) -> Vec<CommentRun> {
    let string_text = string_text_flags(tokens);
    collect_runs(
        tokens
            .iter()
            .zip(line_breaks)
            .zip(string_text)
            .map(|((token, &breaks), string_text)| (token.kind, token.range, breaks, string_text)),
    )
}

/// Переводы строки, которые несёт токен. Кроме `NEWLINE` это одиночные `\r`
/// пробельного токена: платформа считает их переводом строки, и комментарий
/// на них кончается. `\r` в конце пробельного токена перед `NEWLINE` — часть
/// того же перевода, как и в лексеме комментария.
fn line_breaks_of(kind: SyntaxKind, text: &str, next: Option<SyntaxKind>) -> usize {
    match kind {
        SyntaxKind::NEWLINE => 1,
        SyntaxKind::WHITESPACE => {
            let carriage_returns = text.matches('\r').count();
            if next == Some(SyntaxKind::NEWLINE) {
                carriage_returns - (text.len() - text.trim_end_matches('\r').len())
            } else {
                carriage_returns
            }
        }
        _ => 0,
    }
}

fn collect_runs(
    tokens: impl Iterator<Item = (SyntaxKind, TextRange, usize, bool)>,
) -> Vec<CommentRun> {
    let mut runs = Vec::new();
    let mut current: Vec<CommentLine> = Vec::new();
    let mut at_line_start = true;
    let mut newlines = 0usize;

    for (kind, range, line_breaks, string_text) in tokens {
        match kind {
            // Отступ строку не начинает и не кончает; `\r` в CRLF приходит
            // отдельным пробельным токеном ровно на пустой строке.
            SyntaxKind::NEWLINE | SyntaxKind::WHITESPACE | SyntaxKind::BOM => {
                if line_breaks > 0 {
                    newlines += line_breaks;
                    at_line_start = true;
                }
            }
            SyntaxKind::COMMENT => {
                // Строку многострочного литерала, начинающуюся с `//`, лексер
                // отдаёт токеном комментария; комментарием она от этого не
                // становится. Счётчик строк не сбрасываем — иначе серия
                // перепрыгнула бы через такую строку.
                if string_text {
                    at_line_start = false;
                    continue;
                }
                if !current.is_empty() && newlines != 1 {
                    push_run(&mut runs, std::mem::take(&mut current));
                }
                current.push(CommentLine { owns_line: at_line_start, range });
                at_line_start = false;
                newlines = 0;
            }
            // Код серию не закрывает: комментарий кончается вместе со строкой,
            // поэтому между комментариями соседних строк может стоять только
            // код той же строки, а строка кода между сериями и так даёт два
            // перевода строки.
            _ => at_line_start = false,
        }
    }

    push_run(&mut runs, current);
    runs
}

/// Комментарий между частями одного многострочного литерала — текст строки,
/// а не комментарий. Слева его открывает `STRING_START` или продолжает
/// `STRING_PART`; справа строку продолжает `STRING_PART`, закрывает
/// `STRING_TAIL` — или закрывает тело метода `КонецПроцедуры`/`КонецФункции`,
/// когда литерал не закрыт вовсе: тело кончилось, строка так и осталась
/// открытой, и всё до его конца — её текст. Любой другой значимый токен
/// справа означает, что строка оборвана и литерал ею не продолжен, — это
/// настоящий комментарий. Правило лексическое, потому что судить его
/// приходится и там, где дерева нет, — по токенам одного блока строк.
fn is_string_text_between(left: Option<SyntaxKind>, right: Option<SyntaxKind>) -> bool {
    matches!(left, Some(SyntaxKind::STRING_START | SyntaxKind::STRING_PART))
        && matches!(
            right,
            Some(
                SyntaxKind::STRING_PART
                    | SyntaxKind::STRING_TAIL
                    | SyntaxKind::KW_END_PROCEDURE
                    | SyntaxKind::KW_END_FUNCTION
            )
        )
}

/// Значимые соседи каждого токена — за один проход в каждую сторону: внутри
/// серии значимых токенов нет, и искать их от каждой её строки было бы
/// квадратично по длине серии.
fn string_text_flags(tokens: &[LineToken]) -> Vec<bool> {
    let mut next_significant = vec![None; tokens.len()];
    let mut right = None;
    for (index, token) in tokens.iter().enumerate().rev() {
        next_significant[index] = right;
        if !token.kind.is_trivia() {
            right = Some(token.kind);
        }
    }
    let mut left = None;
    tokens
        .iter()
        .zip(next_significant)
        .map(|(token, right)| {
            let string_text = is_string_text_between(left, right);
            if !token.kind.is_trivia() {
                left = Some(token.kind);
            }
            string_text
        })
        .collect()
}

fn push_run(runs: &mut Vec<CommentRun>, lines: Vec<CommentLine>) {
    let range = match (lines.first(), lines.last()) {
        (Some(first), Some(last)) => TextRange::new(first.range.start(), last.range.end()),
        _ => return,
    };
    runs.push(CommentRun { lines, range });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Определение правила: от токена ищем значимых соседей по всему потоку.
    fn string_text_by_scanning(tokens: &[LineToken], index: usize) -> bool {
        let left = tokens[..index].iter().rev().map(|t| t.kind).find(|k| !k.is_trivia());
        let right = tokens[index + 1..].iter().map(|t| t.kind).find(|k| !k.is_trivia());
        is_string_text_between(left, right)
    }

    #[test]
    fn string_text_flags_agree_with_scanning_on_every_short_stream() {
        const KINDS: [SyntaxKind; 7] = [
            SyntaxKind::COMMENT,
            SyntaxKind::NEWLINE,
            SyntaxKind::STRING_START,
            SyntaxKind::STRING_PART,
            SyntaxKind::STRING_TAIL,
            SyntaxKind::KW_END_PROCEDURE,
            SyntaxKind::IDENT,
        ];
        let mut string_text_seen = 0;
        for length in 0..=6u32 {
            for mut code in 0..KINDS.len().pow(length) {
                let tokens: Vec<LineToken> = (0..length)
                    .map(|_| {
                        let kind = KINDS[code % KINDS.len()];
                        code /= KINDS.len();
                        LineToken { kind, range: TextRange::default() }
                    })
                    .collect();
                let flags = string_text_flags(&tokens);
                for (index, flag) in flags.iter().enumerate() {
                    assert_eq!(
                        *flag,
                        string_text_by_scanning(&tokens, index),
                        "token {index} of {tokens:?}"
                    );
                    string_text_seen += usize::from(*flag);
                }
                assert_eq!(flags.len(), tokens.len());
            }
        }
        assert!(string_text_seen > 1_000, "only {string_text_seen} flags were set");
    }
}
