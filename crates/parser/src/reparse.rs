//! Переразбор одного метода после правки внутри него.
//!
//! Полный разбор файла стоит столько, сколько весит файл; правка внутри тела
//! метода весит столько, сколько весит метод. Разбирается фрагмент нового
//! текста в границах старого узла метода, и его поддерево вклеивается в старое
//! дерево. Результат обязан быть тождествен полному разбору нового текста —
//! по дереву, по тексту и по ошибкам; всё, что этого не гарантирует, отвергает
//! один из гардов, и вызывающий разбирает файл целиком.

use lexer::{Token, TokenKind};
use syntax::{NodeOrToken, Parse, SyntaxError, SyntaxKind, SyntaxNode, TextRange, TextSize};

use crate::{grammar, sink, Parser};

/// Правка как пара диапазонов: что заменено в старом тексте и чем в новом.
/// Границы не обязаны совпадать с границами символов: диф идёт по байтам, а
/// переразбор режет текст только по границам узла.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEdit {
    pub old_range: TextRange,
    pub new_range: TextRange,
}

/// Правка, переводящая `old` в `new`: общий префикс и общий суффикс.
/// `None` — тексты равны.
pub fn edit_between(old: &str, new: &str) -> Option<TextEdit> {
    let (old_b, new_b) = (old.as_bytes(), new.as_bytes());
    let prefix = common_prefix(old_b, new_b);
    if prefix == old_b.len() && prefix == new_b.len() {
        return None;
    }
    // Суффикс не заходит на префикс: иначе вставка `a` в `aa` даёт
    // отрицательную длину замены.
    let max_suffix = old_b.len().min(new_b.len()) - prefix;
    let suffix = common_suffix(&old_b[prefix..], &new_b[prefix..]).min(max_suffix);
    Some(TextEdit {
        old_range: TextRange::new(size(prefix), size(old_b.len() - suffix)),
        new_range: TextRange::new(size(prefix), size(new_b.len() - suffix)),
    })
}

/// Сравнение блоками: `memcmp` на блок вместо побайтового цикла — на 10 МБ
/// это разница между единицами и десятками миллисекунд на каждую клавишу.
const BLOCK: usize = 256;

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + BLOCK <= n && a[i..i + BLOCK] == b[i..i + BLOCK] {
        i += BLOCK;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + BLOCK <= n
        && a[a.len() - i - BLOCK..a.len() - i] == b[b.len() - i - BLOCK..b.len() - i]
    {
        i += BLOCK;
    }
    while i < n && a[a.len() - 1 - i] == b[b.len() - 1 - i] {
        i += 1;
    }
    i
}

fn size(n: usize) -> TextSize {
    TextSize::from(u32::try_from(n).expect("текст длиннее u32"))
}

/// Почему фрагмент нельзя вклеить. Каждый вариант — свой гард; порядок
/// значений — порядок проверки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Правка не внутри узла метода: между методами, в docstring, в модульном
    /// коде, или касается границы узла.
    OutsideMethod,
    /// Старый узел закрыт не своим словом: его границу поставил контекст
    /// (`#Иначе` снаружи), который фрагменту не воспроизвести.
    OldUnclosed,
    /// Граница токена в новом тексте не совпала с границей узла: правка
    /// дорастила токен, начатый до узла, либо разорвала последний.
    TokenBoundary,
    /// Директивы препроцессора во фрагменте не сбалансированы или
    /// `#ИначеЕсли`/`#Иначе` на нулевой глубине; либо метод стоит внутри
    /// условной группы, а старый или новый его текст не держит целых
    /// конструкций — тогда от правки зависит, разорвана ли группа вокруг.
    Preprocessor,
    /// У корня фрагмента не ровно один ребёнок-узел.
    Shape,
    /// Вид нового узла не равен виду старого.
    Kind,
    /// Новый узел закрыт не своим словом.
    NewUnclosed,
    /// Ошибка контекста: диапазон ошибки старого разбора задевает первый или
    /// последний токен узла либо пересекает узел частично; или ошибка
    /// фрагмента задевает его первый или последний токен.
    ContextError,
}

impl Refusal {
    pub const ALL: [Refusal; 8] = [
        Refusal::OutsideMethod,
        Refusal::OldUnclosed,
        Refusal::TokenBoundary,
        Refusal::Preprocessor,
        Refusal::Shape,
        Refusal::Kind,
        Refusal::NewUnclosed,
        Refusal::ContextError,
    ];

    pub fn index(self) -> usize {
        self as usize
    }
}

/// Набор включённых гардов. Нужен только положительным контролям: тест
/// выключает один гард и обязан увидеть дерево, отличное от полного разбора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guards(u16);

impl Guards {
    pub const ALL: Guards = Guards(u16::MAX);

    pub fn without(self, refusal: Refusal) -> Guards {
        Guards(self.0 & !(1 << refusal.index()))
    }

    fn has(self, refusal: Refusal) -> bool {
        self.0 & (1 << refusal.index()) != 0
    }
}

pub fn reparse_method(
    old: &Parse<SyntaxNode>,
    old_text: &str,
    new_text: &str,
    edit: TextEdit,
) -> Result<Parse<SyntaxNode>, Refusal> {
    reparse_method_guarded(old, old_text, new_text, edit, Guards::ALL)
}

pub fn reparse_method_guarded(
    old: &Parse<SyntaxNode>,
    old_text: &str,
    new_text: &str,
    edit: TextEdit,
    guards: Guards,
) -> Result<Parse<SyntaxNode>, Refusal> {
    debug_assert_eq!(usize::from(old.syntax_node().text_range().len()), old_text.len());
    let root = old.syntax_node();
    let node = enclosing_method(&root, edit.old_range).ok_or(Refusal::OutsideMethod)?;
    let old_range = node.text_range();
    // Правка на границе узла принадлежит тривии предка или соседу.
    if guards.has(Refusal::OutsideMethod)
        && !(old_range.start() < edit.old_range.start() && edit.old_range.end() < old_range.end())
    {
        return Err(Refusal::OutsideMethod);
    }
    let closer = closing_word(node.kind());
    if guards.has(Refusal::OldUnclosed) && last_token_kind(&node) != Some(closer) {
        return Err(Refusal::OldUnclosed);
    }

    let delta = new_text.len() as i64 - old_text.len() as i64;
    let start = usize::from(old_range.start());
    let new_end = (usize::from(old_range.end()) as i64 + delta) as usize;
    if guards.has(Refusal::TokenBoundary)
        && !(token_boundary_at(new_text, start, Edge::Start)
            && token_boundary_at(new_text, new_end, Edge::End))
    {
        return Err(Refusal::TokenBoundary);
    }

    let fragment = &new_text[start..new_end];
    let tokens = lexer::tokenize(fragment);
    if guards.has(Refusal::Preprocessor) && !preprocessor_balanced(&tokens) {
        return Err(Refusal::Preprocessor);
    }
    if guards.has(Refusal::Preprocessor)
        && !(Parser::new(&tokens).is_whole()
            && Parser::new(&lexer::tokenize(&old_text[old_range])).is_whole())
        && inside_conditional_group(&root, &node)
    {
        return Err(Refusal::Preprocessor);
    }

    let mut p = Parser::new(&tokens);
    grammar::source_file(&mut p);
    let events = p.finish();
    let fragment_parse = syntax::with_shared_node_cache(|cache| {
        let sink = sink::Sink::with_cache(&tokens, cache);
        sink.finish(events).finish()
    });
    let fragment_root = fragment_parse.syntax_node();
    let mut children = fragment_root.children_with_tokens();
    let new_node = match (children.next(), children.next()) {
        (Some(NodeOrToken::Node(n)), None) => n,
        _ if guards.has(Refusal::Shape) => return Err(Refusal::Shape),
        (Some(NodeOrToken::Node(n)), _) => n,
        _ => return Err(Refusal::Shape),
    };
    if new_node.kind() != node.kind() {
        // `replace_with` паникует на несовпадении вида — этот гард не выключается.
        return Err(Refusal::Kind);
    }
    if guards.has(Refusal::NewUnclosed) && last_token_kind(&new_node) != Some(closer) {
        return Err(Refusal::NewUnclosed);
    }

    let strict = guards.has(Refusal::ContextError);
    let errors = merge_errors(old, &node, &fragment_parse, &new_node, start, delta, strict)?;

    let heap_bytes = old.heap_bytes() - subtree_heap_bytes(&node) + subtree_heap_bytes(&new_node);
    let green = node.replace_with(new_node.green().into_owned());
    Ok(Parse::from_parts(green, errors, heap_bytes))
}

/// Внешний узел метода, накрывающий диапазон. Внешний, а не ближайший: метод
/// внутри `#Если` внутри тела другого метода парсер тоже делает узлом, а
/// граница внешнего метода для него — контекст, которого фрагменту не дать.
fn enclosing_method(root: &SyntaxNode, range: TextRange) -> Option<SyntaxNode> {
    let element = root.covering_element(range);
    let mut node = match element {
        NodeOrToken::Node(n) => n,
        NodeOrToken::Token(t) => t.parent()?,
    };
    let mut found = None;
    loop {
        if matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF) {
            found = Some(node.clone());
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }
    found
}

/// Стоит ли метод внутри условной группы: структурной — тогда она среди
/// предков, или разорванной — тогда её части есть в дереве. Разорвана ли
/// группа, решает текст её ветвей, а метод — часть этого текста.
fn inside_conditional_group(root: &SyntaxNode, node: &SyntaxNode) -> bool {
    node.ancestors().any(|ancestor| {
        matches!(
            ancestor.kind(),
            SyntaxKind::PRE_IF_DIR | SyntaxKind::PRE_ELSIF_CLAUSE | SyntaxKind::PRE_ELSE_CLAUSE
        )
    }) || root.descendants().any(|descendant| descendant.kind() == SyntaxKind::PRE_TORN_IF_PART)
}

fn closing_word(kind: SyntaxKind) -> SyntaxKind {
    match kind {
        SyntaxKind::FUNCTION_DEF => SyntaxKind::KW_END_FUNCTION,
        _ => SyntaxKind::KW_END_PROCEDURE,
    }
}

fn last_token_kind(node: &SyntaxNode) -> Option<SyntaxKind> {
    node.last_token().map(|t| t.kind())
}

#[derive(Clone, Copy)]
enum Edge {
    Start,
    End,
}

/// Граница токена в `text` на `offset`, установленная лексированием строки,
/// в которой лежит `offset`: ни один токен не пересекает `\n`, поэтому
/// токены строки не зависят от соседних строк.
fn token_boundary_at(text: &str, offset: usize, edge: Edge) -> bool {
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
    let local = offset - line_start;
    if local == 0 || offset == line_end {
        return true;
    }
    lexer::tokenize(&text[line_start..line_end]).iter().any(|t| match edge {
        Edge::Start => t.offset == local,
        Edge::End => t.offset + t.text.len() == local,
    })
}

fn preprocessor_balanced(tokens: &[Token]) -> bool {
    let (mut ifs, mut inserts, mut deletes) = (0i32, 0i32, 0i32);
    for token in tokens {
        match token.kind {
            TokenKind::PreIf => ifs += 1,
            TokenKind::PreElsIf | TokenKind::PreElse if ifs == 0 => return false,
            TokenKind::PreElsIf | TokenKind::PreElse => {}
            TokenKind::PreEndIf => ifs -= 1,
            TokenKind::PreInsert => inserts += 1,
            TokenKind::PreEndInsert => inserts -= 1,
            TokenKind::PreDelete => deletes += 1,
            TokenKind::PreEndDelete => deletes -= 1,
            _ => {}
        }
        if ifs < 0 || inserts < 0 || deletes < 0 {
            return false;
        }
    }
    ifs == 0 && inserts == 0 && deletes == 0
}

/// Диапазон задевает первый или последний токен узла.
fn touches_edge_tokens(range: TextRange, node: &SyntaxNode) -> bool {
    let first = node.first_token().map(|t| t.text_range());
    let last = node.last_token().map(|t| t.text_range());
    [first, last].into_iter().flatten().any(|edge| {
        // Пустой диапазон на границе тоже считается касанием.
        range.start() <= edge.end() && edge.start() <= range.end()
    })
}

/// Ошибки нового дерева: ошибки старого разбора вне узла (после узла — со
/// сдвигом) и ошибки фрагмента вместо ошибок узла, на том же месте списка —
/// сток выдаёт ошибки в порядке событий, и полный разбор дал бы их там же.
/// Со `strict` ошибка, задевающая первый или последний токен узла либо
/// пересекающая узел частично, — отказ: её выдало правило снаружи узла, и
/// фрагмент её не воспроизведёт.
fn merge_errors(
    old: &Parse<SyntaxNode>,
    old_node: &SyntaxNode,
    fragment: &Parse<SyntaxNode>,
    new_node: &SyntaxNode,
    start: usize,
    delta: i64,
    strict: bool,
) -> Result<Vec<SyntaxError>, Refusal> {
    let old_range = old_node.text_range();
    let mut fragment_errors = Vec::with_capacity(fragment.errors().len());
    for e in fragment.errors() {
        if strict && touches_edge_tokens(e.range(), new_node) {
            return Err(Refusal::ContextError);
        }
        fragment_errors.push(shifted(e, start as i64));
    }
    let mut errors = Vec::with_capacity(old.errors().len() + fragment_errors.len());
    let mut inserted = false;
    for e in old.errors() {
        let r = e.range();
        if strict && touches_edge_tokens(r, old_node) {
            return Err(Refusal::ContextError);
        }
        let inside = old_range.contains_range(r);
        let after = r.start() >= old_range.end();
        let before = r.end() <= old_range.start();
        if (inside || after) && !inserted {
            errors.append(&mut fragment_errors);
            inserted = true;
        }
        if inside {
            continue;
        }
        if after {
            errors.push(shifted(e, delta));
        } else if before {
            errors.push(e.clone());
        } else if strict {
            return Err(Refusal::ContextError);
        }
    }
    if !inserted {
        errors.append(&mut fragment_errors);
    }
    Ok(errors)
}

fn shifted(e: &SyntaxError, delta: i64) -> SyntaxError {
    let r = e.range();
    let shift = |x: TextSize| size((u32::from(x) as i64 + delta) as usize);
    SyntaxError::new(TextRange::new(shift(r.start()), shift(r.end())), e.structured().clone())
}

/// Та же формула, что у билдера: константа на элемент плюс байты токена.
fn subtree_heap_bytes(node: &SyntaxNode) -> usize {
    node.descendants_with_tokens()
        .map(|el| match el {
            NodeOrToken::Node(_) => syntax::HEAP_PER_ELEMENT,
            NodeOrToken::Token(t) => syntax::HEAP_PER_ELEMENT + t.text().len(),
        })
        .sum()
}
