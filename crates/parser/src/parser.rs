use lexer::{Token, TokenKind};
use parser_error::{ParseError, RecoveryKind};
use smallvec::smallvec;

#[macro_use]
pub(crate) mod input;
pub mod token_set;
mod torn_if;

use crate::event::{Event, NodeKind};
use crate::parser::input::{Input, Sig};

const MAX_ITERATIONS: usize = 1_000_000;

const POSITION_HISTORY_SIZE: usize = 100;

pub struct Parser<'a> {
    input: Input<'a>,
    /// Позиция среди ЗНАЧИМЫХ токенов, а не среди лексем.
    pos: usize,
    events: Vec<Event>,
    iteration_count: usize,
    recent_positions: Vec<usize>,
    paren_depth: u32,
    brace_depth: u32,
    at_grammar_boundary: Option<fn(&Parser) -> bool>,
    enclosing_boundaries: Vec<fn(&Parser) -> bool>,
    names_are_fields: bool,
    open_inline_inserts: u32,
    closed_inserts: std::cell::OnceCell<Vec<usize>>,
    open_preproc_ifs: u32,
    crossed_preproc_ifs: Vec<u32>,
    conditionals: std::cell::OnceCell<Vec<torn_if::Conditional>>,
    /// The torn conditional groups whose branch is being read: where the
    /// branch ends and where the group closes.
    open_torn_ifs: Vec<(usize, usize)>,
}

impl<'a> Parser<'a> {
    pub fn new(tokens: &'a [Token]) -> Self {
        Self {
            input: Input::new(tokens),
            pos: 0,
            events: Vec::new(),
            iteration_count: 0,
            recent_positions: Vec::with_capacity(POSITION_HISTORY_SIZE),
            paren_depth: 0,
            brace_depth: 0,
            at_grammar_boundary: None,
            names_are_fields: false,
            enclosing_boundaries: Vec::new(),
            open_inline_inserts: 0,
            closed_inserts: std::cell::OnceCell::new(),
            open_preproc_ifs: 0,
            crossed_preproc_ifs: Vec::new(),
            conditionals: std::cell::OnceCell::new(),
            open_torn_ifs: Vec::new(),
        }
    }

    pub fn finish(self) -> Vec<Event> {
        self.events
    }

    pub fn current(&self) -> Option<Sig> {
        self.nth(0)
    }

    /// Вид `n`-го значимого токена, считая от текущего.
    pub fn nth(&self, n: usize) -> Option<Sig> {
        self.input.kind(self.pos + n)
    }

    pub fn at(&self, kind: Sig) -> bool {
        self.current() == Some(kind)
    }

    /// Позиция текущего значимого токена — чтобы вернуться к его тексту после разбора.
    pub fn token_pos(&self) -> usize {
        self.pos
    }

    /// Похоже ли слово в позиции `pos` на закрыватель `closer` — то есть похоже ли на
    /// испорченную опечаткой попытку его написать.
    ///
    /// Вопрос стоит только там, где слово УЖЕ разобрано как неизвестный оператор: от
    /// ответа зависит, добавлять ли вторую, производную жалобу о пропущенном
    /// закрывателе. Спрашивают именно про тот закрыватель, о пропаже которого будут
    /// сообщать: опечатка в разделителе ветки — не попытка закрыть конструкцию.
    pub fn resembles_closer(&self, pos: usize, closer: Sig) -> bool {
        let text = self.input.text(pos);
        closer_spellings(closer.kind()).iter().any(|spelling| is_typo_of(text, spelling))
    }

    pub fn at_ts(&self, set: crate::parser::token_set::TokenSet) -> bool {
        self.current().is_some_and(|k| set.contains(k))
    }

    pub fn current_text(&self) -> &str {
        self.input.text(self.pos)
    }

    pub fn at_keyword(&self, text: &str) -> bool {
        self.input.token(self.pos).is_some_and(|t| {
            t.kind == TokenKind::Ident && stdx::case::eq_ignore_case(&t.text, text)
        })
    }

    /// Значимых токенов больше не осталось.
    ///
    /// Хвост промежутков за последним словом сюда не входит: грамматике там
    /// делать нечего, а до дерева его доводит сток.
    pub fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    pub fn eat(&mut self, kind: Sig) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub fn bump(&mut self) {
        let Some(kind) = self.current() else {
            return;
        };
        self.track_group(kind.kind());

        self.events.push(Event::Token { kind: kind.kind() });
        self.pos += 1;
    }

    /// Maintains the open-group count as tokens go by.
    ///
    /// Kept here because `bump` is the only way a token is consumed and the
    /// position never rewinds, so this is the one place that sees every
    /// token exactly once. Counting it anywhere else means counting it twice
    /// or rescanning, and rescanning a growing prefix on every decision is
    /// quadratic on a long malformed input.
    ///
    /// The two kinds are counted apart so that a closer of one kind cannot
    /// cancel an opener of the other. Parens inside a brace region are not
    /// counted at all: that region's content is taken verbatim, so its
    /// brackets say nothing about the structure around it.
    fn track_group(&mut self, kind: TokenKind) {
        match kind {
            // A group cannot span a statement separator, so consuming one
            // closes whatever was open — wherever it is consumed. Tying this
            // to the one caller that happens to notice the separator would
            // leave every rule that swallows one holding stale depth.
            TokenKind::Semicolon => {
                self.paren_depth = 0;
                self.brace_depth = 0;
            }
            TokenKind::LBrace => self.brace_depth += 1,
            TokenKind::RBrace => self.brace_depth = self.brace_depth.saturating_sub(1),
            TokenKind::LParen if self.brace_depth == 0 => self.paren_depth += 1,
            TokenKind::RParen if self.brace_depth == 0 => {
                self.paren_depth = self.paren_depth.saturating_sub(1)
            }
            _ => {}
        }
    }

    /// Consumes an extension marker that tears an expression apart, keeping
    /// count of the insertions it opens.
    ///
    /// The count is what lets the closer of such an insertion be taken where
    /// the expression has already ended: in front of `Тогда` or `;` nothing
    /// else says that the marker belongs to the construct and not to the
    /// statements after it.
    pub(crate) fn bump_inline_insert_marker(&mut self) {
        match self.current() {
            Some(T![PreInsert]) => self.open_inline_inserts += 1,
            Some(T![PreEndInsert]) => {
                self.open_inline_inserts = self.open_inline_inserts.saturating_sub(1)
            }
            _ => return,
        }
        self.bump();
    }

    /// Whether an insertion opened inside an expression still waits for its
    /// closer.
    pub(crate) fn inline_insert_open(&self) -> bool {
        self.open_inline_inserts > 0
    }

    /// Drops the insertions an expression opened and never closed.
    ///
    /// A method is where such an insertion ends at the latest: without this,
    /// one unclosed marker would make every stray closer to the end of the file
    /// silently acceptable, and a method reparsed on its own would disagree
    /// with the same method in the full parse.
    pub(crate) fn forget_inline_inserts(&mut self) {
        self.open_inline_inserts = 0;
    }

    /// Drops the crossed `#Если` blocks still waiting for their closer, for
    /// the same reason as [`Self::forget_inline_inserts`].
    pub(crate) fn forget_crossed_preproc_ifs(&mut self) {
        self.crossed_preproc_ifs.clear();
    }

    /// Counts the `#Если` blocks being parsed, so that a closer can be told
    /// apart from the closer of a block opened after a crossing.
    pub(crate) fn enter_preproc_if(&mut self) {
        self.open_preproc_ifs += 1;
    }

    pub(crate) fn leave_preproc_if(&mut self) {
        self.open_preproc_ifs = self.open_preproc_ifs.saturating_sub(1);
    }

    /// Records that the `#Если` just parsed ended at the closer of a statement
    /// it was opened in, so its `#Иначе`/`#КонецЕсли` still lie ahead.
    ///
    /// The preprocessor is textual: `#Если Клиент Тогда` may cover an
    /// `ИначеЕсли` clause of the `Если` around it. The block's content ends at
    /// that clause, and without this record its `#КонецЕсли` would close the
    /// first block further out instead — at module level, a region that
    /// wraps whole methods would end in the middle of one.
    pub(crate) fn cross_preproc_if(&mut self) {
        self.crossed_preproc_ifs.push(self.open_preproc_ifs);
    }

    /// Whether a `#ИначеЕсли`, `#Иначе` or `#КонецЕсли` stands here.
    pub(crate) fn at_preproc_if_directive(&self) -> bool {
        matches!(self.current(), Some(T![PreElsIf] | T![PreElse] | T![PreEndIf]))
    }

    /// Whether the directive here continues a block recorded by
    /// [`Self::cross_preproc_if`] rather than a block still being parsed.
    pub(crate) fn at_crossed_preproc_if_directive(&self) -> bool {
        self.at_preproc_if_directive()
            && self.crossed_preproc_ifs.last() == Some(&self.open_preproc_ifs)
    }

    /// Consumes the `#КонецЕсли` of a crossed block, closing it.
    pub(crate) fn bump_crossed_preproc_if_end(&mut self) {
        if self.at(T![PreEndIf]) && self.at_crossed_preproc_if_directive() {
            self.crossed_preproc_ifs.pop();
            self.bump();
        }
    }

    /// Whether the `#Вставка` here has its `#КонецВставки` before its method
    /// ends, counting neither marker inside text an extension removed.
    ///
    /// The pairs are matched once for the whole input: asked at every marker,
    /// a search to the end of the method would make a method of unclosed
    /// markers quadratic.
    pub(crate) fn insert_here_is_closed(&self) -> bool {
        self.closed_inserts.get_or_init(|| self.match_inserts()).binary_search(&self.pos).is_ok()
    }

    fn match_inserts(&self) -> Vec<usize> {
        let mut open = Vec::new();
        let mut closed = Vec::new();
        let mut pos = 0;
        while let Some(kind) = self.input.kind(pos) {
            match kind {
                T![PreInsert] => open.push(pos),
                T![PreEndInsert] => closed.extend(open.pop()),
                T![PreDelete] => {
                    while !matches!(self.input.kind(pos + 1), Some(T![PreEndDelete]) | None) {
                        pos += 1;
                    }
                    pos += 1;
                }
                T![KwEndProcedure] | T![KwEndFunction] => open.clear(),
                _ => {}
            }
            pos += 1;
        }
        closed.sort_unstable();
        closed
    }

    /// The closed conditional group whose `#Если` stands at `pos`.
    ///
    /// The groups are decided once for the whole input, as the inserts are:
    /// asked at every directive, a scan to the group's end would make deeply
    /// nested groups quadratic.
    fn conditional_at(&self, pos: usize) -> Option<torn_if::Conditional> {
        let groups = self.conditionals();
        groups.binary_search_by_key(&pos, |group| group.open).ok().map(|index| groups[index])
    }

    fn conditionals(&self) -> &[torn_if::Conditional] {
        self.conditionals.get_or_init(|| torn_if::conditionals(|at| self.input.kind(at)))
    }

    /// Whether the input holds whole constructs as the parser reads it — see
    /// [`torn_if::whole`].
    pub(crate) fn is_whole(&self) -> bool {
        torn_if::whole(|at| self.input.kind(at), self.conditionals())
    }

    /// Whether the `#Если` here, standing where a statement may begin, tears
    /// a construct apart: one of its branches opens what it does not close,
    /// or closes or continues what it did not open.
    ///
    /// A group whose branches each hold whole statements keeps the structured
    /// reading, every branch parsed.
    pub(crate) fn at_torn_if_statement(&self) -> bool {
        self.at(T![PreIf]) && self.conditional_at(self.pos).is_some_and(|group| !group.balanced)
    }

    /// Whether a closed `#Если` stands here. Inside an expression no
    /// statement can begin, so any such group tears the expression apart.
    pub(crate) fn at_torn_if_inline(&self) -> bool {
        self.at(T![PreIf]) && self.conditional_at(self.pos).is_some()
    }

    /// Whether the directive here ends the branch being read of a torn group.
    pub(crate) fn at_torn_if_end(&self) -> bool {
        matches!(self.current(), Some(T![PreElsIf] | T![PreElse] | T![PreEndIf]))
            && self.open_torn_ifs.iter().any(|&(end, _)| end == self.pos)
    }

    /// The group whose `#Если` stands here.
    pub(crate) fn torn_if_here(&self) -> Option<torn_if::Conditional> {
        if self.at(T![PreIf]) {
            self.conditional_at(self.pos)
        } else {
            None
        }
    }

    /// Starts reading the branch of a torn group: it ends at `end`, and the
    /// group closes at `close`.
    pub(crate) fn open_torn_if(&mut self, end: usize, close: usize) {
        self.open_torn_ifs.push((end, close));
    }

    /// Stops reading the branch that ends here and returns where its group
    /// closes.
    pub(crate) fn close_torn_if(&mut self) -> Option<usize> {
        let index = self.open_torn_ifs.iter().position(|&(end, _)| end == self.pos)?;
        Some(self.open_torn_ifs.remove(index).1)
    }

    /// Where a look-ahead resumes past the torn-group part starting `n`
    /// tokens ahead, as an offset from here; `None` — no such part there.
    ///
    /// `pending` holds the branches the look-ahead itself entered: their ends
    /// are parts just as the ends of the branches being read are.
    pub(crate) fn torn_if_skip(
        &self,
        n: usize,
        pending: &mut Vec<(usize, usize)>,
    ) -> Option<usize> {
        let at = self.pos + n;
        match self.input.kind(at)? {
            T![PreIf] => {
                let group = self.conditional_at(at)?;
                let resume = match group.active {
                    Some((start, end)) => {
                        pending.push((end, group.close));
                        start
                    }
                    None => group.close + 1,
                };
                Some(resume - self.pos)
            }
            T![PreElsIf] | T![PreElse] | T![PreEndIf] => self
                .open_torn_ifs
                .iter()
                .chain(pending.iter())
                .find(|&&(end, _)| end == at)
                .map(|&(_, close)| close + 1 - self.pos),
            _ => None,
        }
    }

    /// Consumes, as text not compiled in the variant read, every token before
    /// the one at `next`.
    pub(crate) fn bump_removed_until(&mut self, next: usize) {
        while self.pos < next && !self.at_end() {
            self.check_iteration_limit();
            self.bump_removed();
        }
    }

    /// How many insertions opened inside an expression still wait for their
    /// closers.
    pub(crate) fn open_inline_inserts(&self) -> u32 {
        self.open_inline_inserts
    }

    /// Consumes a token of text the extension removed.
    ///
    /// Group tracking does not see it: a bracket or separator that no longer
    /// exists in the module says nothing about the expression it was cut out
    /// of, and counting it would leave that expression with a group it never
    /// opened or closed.
    pub(crate) fn bump_removed(&mut self) {
        let Some(kind) = self.current() else {
            return;
        };
        self.events.push(Event::Token { kind: kind.kind() });
        self.pos += 1;
    }

    /// How many groups are open at the current position.
    ///
    /// Tells a caller whether the rule it just ran left something open —
    /// which is the only way to know that the position it was handed is not
    /// top level.
    pub(crate) fn open_group_count(&self) -> u32 {
        self.paren_depth + self.brace_depth
    }

    /// Declares that whatever was open before this point is closed as far as
    /// grouping is concerned.
    ///
    /// A statement separator is that point: a group cannot span one. Without
    /// saying so, an unclosed group in a bad statement outlives it — an
    /// unclosed `{` would go on suppressing the paren count in every
    /// statement after it, and a depth left standing would let the next
    /// statement close it and open its own without the total ever changing.
    pub(crate) fn reset_group_tracking(&mut self) {
        self.paren_depth = 0;
        self.brace_depth = 0;
    }

    pub fn expect(&mut self, kind: Sig) -> bool {
        // A grammar boundary belongs to a rule further out, and no rule may
        // require it here. It matters because kinds are coarser than words:
        // every keyword of a language whose keywords are not reserved
        // arrives as an identifier, so `expect(Ident)` would take the word
        // that begins the next construct and cost the caller that construct.
        // The separator is not covered — a rule asking for `;` means it.
        if !self.at_declared_boundary() && self.eat(kind) {
            return true;
        }

        let found = self.current();
        let recovery = if found.is_none() || self.at_statement_separator() {
            RecoveryKind::MissingToken
        } else {
            RecoveryKind::BumpToken
        };
        let err = ParseError::Expected {
            expected: smallvec![kind.kind()],
            found: found.map(Sig::kind),
            recovery,
        };

        if recovery == RecoveryKind::MissingToken {
            self.emit_missing(err);
        } else {
            self.emit_error(err);
        }

        false
    }

    /// Runs `f` with a bare name read as a field of a source.
    ///
    /// Where the query says what to read — the field list, a filter, a join
    /// condition — a bare name is a field, and the source closes that position
    /// to keywords. Where it says how to arrange what was read, the same word
    /// may be an alias the select list declared, and an alias is allowed to
    /// spell a keyword; those rules leave this off, so `УПОРЯДОЧИТЬ ПО Итоги`
    /// stays a reference rather than becoming an error.
    ///
    /// Scoped like a boundary, and for the same reason: the fact belongs to
    /// the rule that knows it, and has to end when that rule returns.
    pub fn within_field_names<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.names_are_fields;
        self.names_are_fields = true;
        let result = f(self);
        self.names_are_fields = outer;
        result
    }

    /// Runs `f` with the field-name scope of whatever holds it dropped.
    ///
    /// A nested query is its own query. The scope belongs to the clause that
    /// declared it, and a subquery standing inside a filter must read its own
    /// `УПОРЯДОЧИТЬ ПО` the way a top-level one does — by an alias its own
    /// selection declared, which may spell a keyword.
    pub fn outside_field_names<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.names_are_fields;
        self.names_are_fields = false;
        let result = f(self);
        self.names_are_fields = outer;
        result
    }

    pub fn names_are_fields(&self) -> bool {
        self.names_are_fields
    }

    /// Requires `kind`, and consumes nothing when it is absent.
    ///
    /// A rule that gives up once it sees its opener is missing must not spend
    /// a token on the way out. Whatever stands there is the continuation of
    /// something else — an operator of the expression around it, the bracket
    /// of the construct holding it — and plain `expect` would take it as its
    /// recovery, which costs the caller exactly what the caller was about to
    /// parse.
    pub fn expect_no_bump(&mut self, kind: Sig) -> bool {
        if !self.at_declared_boundary() && self.eat(kind) {
            return true;
        }

        let err = ParseError::Expected {
            expected: smallvec![kind.kind()],
            found: self.current().map(Sig::kind),
            recovery: RecoveryKind::MissingToken,
        };
        self.emit_missing(err);
        false
    }

    /// The last token before this one that was not trivia.
    ///
    /// A rule that must know what it is standing behind — whether a dot
    /// made the word here a field name, say — would otherwise walk the
    /// token list itself, and every rule that walked it got a different
    /// part of it wrong.
    pub fn prev_significant(&self) -> Option<Sig> {
        self.pos.checked_sub(1).and_then(|prev| self.input.kind(prev))
    }

    /// Whether a line break stands between the previous significant token and
    /// the next one.
    ///
    /// This is the whole of what the grammar is told about lines. The kind of
    /// a token is not that channel: a rule that reads one gets the norm from
    /// nowhere, and the next rule to need the same decision writes it a fourth
    /// way. Which constructs may cross a line, and on what grounds, is
    /// declared in `docs/architecture/adr/ADR-02-line-sensitivity.md`.
    ///
    /// A comment in the gap counts as a line break: it runs to the end of its
    /// line, so behind it stands either a newline or the end of the file.
    ///
    /// Nothing is consumed. Where no significant token precedes the gap —
    /// the prologue of a file — there is no pair of tokens to stand between,
    /// and the answer is `false`.
    pub fn a_line_break_precedes(&self) -> bool {
        self.input.a_line_break_precedes(self.pos)
    }

    /// Соприкасается ли токен под курсором с предыдущим.
    ///
    /// Нужен там, где лексер режет одно слово языка на несколько токенов
    /// вплотную: склеить обратно надо ровно их, а соседнее слово, отделённое
    /// хоть пробелом, — уже другое слово.
    pub fn a_gap_precedes(&self) -> bool {
        self.input.a_gap_precedes(self.pos)
    }

    pub fn eat_keyword(&mut self, text: &str) -> bool {
        if self.at_keyword(text) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub fn start(&mut self) -> Marker {
        let pos = self.events.len();
        // Индекс СЫРОЙ: сток считает диапазоны по потоку лексем, а `self.pos`
        // считает слова. Позиция за последним словом штатна — ошибка о
        // пропущенном токене на конце входа ставит маркер именно там.
        let start_token_pos = self.input.raw_at(self.pos);
        self.events.push(Event::Placeholder);
        Marker { pos, start_token_pos }
    }

    pub fn error(&mut self) {
        let found = self.current().map(Sig::kind);
        let recovery = if found.is_none() || self.at_statement_separator() {
            RecoveryKind::MissingToken
        } else {
            RecoveryKind::BumpToken
        };
        let err = ParseError::Unexpected { found, recovery };

        if recovery == RecoveryKind::MissingToken {
            self.emit_missing(err);
        } else {
            self.emit_error(err);
        }
    }

    pub fn error_unexpected(&mut self) {
        self.error();
    }

    pub fn error_custom(&mut self, msg: &'static str) {
        let recovery = if self.current().is_none() || self.at_statement_separator() {
            RecoveryKind::MissingToken
        } else {
            RecoveryKind::BumpToken
        };
        let err = ParseError::Custom { message: msg, recovery };
        if recovery == RecoveryKind::MissingToken {
            self.emit_missing(err);
        } else {
            self.emit_error(err);
        }
    }

    pub fn error_custom_no_bump(&mut self, msg: &'static str) {
        let err = ParseError::Custom { message: msg, recovery: RecoveryKind::MissingToken };
        self.emit_missing(err);
    }

    /// Сообщает, что на месте курсора ожидался `kind`, и подчёркивает место.
    ///
    /// Правило, собирающее `ParseError` руками, обходит выбор `RecoveryKind` и
    /// разметку диапазона — решения уровня парсера, а не грамматики, — и
    /// каждое такое место обходит их по-своему. Здесь они приняты один раз:
    /// диапазон открывается маркером и закрывается узлом ошибки, то есть это
    /// [`RecoveryKind::RecoverySpan`], и норма привязки из
    /// `docs/architecture/adr/ADR-03-error-range-attribution.md` выполняется
    /// по построению.
    pub fn error_expected(&mut self, kind: Sig) {
        let m = self.start();
        let found = self.current().map(Sig::kind);
        self.emit_error_at_marker(
            m,
            ParseError::Expected {
                expected: smallvec![kind.kind()],
                found,
                recovery: RecoveryKind::RecoverySpan,
            },
        );
    }

    /// Стоит ли под курсором то, что лексер прочесть не смог.
    ///
    /// Правилу это нужно, чтобы промолчать: текст, который лексер уже отверг,
    /// о своём содержимом ничего не обещает, и вторая жалоба на него говорит о
    /// нём не больше первой.
    pub fn at_error(&self) -> bool {
        self.current() == Some(T![Error])
    }

    /// Подчёркивает уже открытый маркером участок и называет причину.
    ///
    /// Единственный путь грамматики к ошибке с диапазоном. Собирать `ParseError`
    /// руками правило не может: тип ошибки хранит СЫРОЙ вид токена, и открытая
    /// грамматике дверь с таким параметром обошла бы алфавит `Sig` мимо всех
    /// десяти переведённых дверей — то есть вернула бы ровно ту дыру, ради
    /// которой алфавит и заведён.
    pub fn error_custom_at_marker(&mut self, m: Marker, message: &'static str) {
        self.emit_error_at_marker(
            m,
            ParseError::Custom { message, recovery: RecoveryKind::RecoverySpan },
        );
    }

    fn emit_error_at_marker(&mut self, m: Marker, err: ParseError) {
        debug_assert!(err.recovery() == RecoveryKind::RecoverySpan);
        self.events.push(Event::ErrorWithSpan { start_token: m.start_token_pos, err });
        m.complete(self, NodeKind::Error);
    }

    /// Names the words that are never anything but a boundary, for the whole
    /// parse.
    ///
    /// A rule deep inside a construct cannot know what encloses it, so it
    /// cannot know which token it must not take. What holds everywhere the
    /// grammar can say once, here — and only what holds everywhere belongs
    /// here, because this reaches even the positions where a rule is asking
    /// for a name.
    pub fn set_grammar_boundary(&mut self, at_boundary: fn(&Parser) -> bool) {
        self.at_grammar_boundary = Some(at_boundary);
    }

    /// Runs `f` with one more token pattern that no rule inside it may report
    /// an error by consuming.
    ///
    /// This is what the whole-parse boundary cannot express: the closer of
    /// *this* block, the clause keywords of *this* query. The rule that owns
    /// the construct knows them, states them where it descends into the body,
    /// and they stop applying when it returns — which is why they are pushed
    /// here rather than by hand, since a rule with an early return would
    /// otherwise leave one behind.
    ///
    /// Unlike the whole-parse boundary, an enclosing one does not reach a
    /// rule that is asking for a name: a clause keyword is a boundary where a
    /// clause may start and an ordinary alias where a name may stand.
    pub fn within_boundary<R>(
        &mut self,
        at_boundary: fn(&Parser) -> bool,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.enclosing_boundaries.push(at_boundary);
        let result = f(self);
        self.enclosing_boundaries.pop();
        result
    }

    /// Whether the current token belongs to a construct further out.
    ///
    /// A loop that parses items until its own terminator has to ask this too.
    /// Rules inside it refuse to consume such a token, so a loop that waits
    /// only for its own terminator waits for a token nothing will reach — the
    /// boundary turns a cascade of false errors into a parse that never ends.
    pub fn at_enclosing_boundary(&self) -> bool {
        self.enclosing_boundaries.iter().any(|at_boundary| at_boundary(self))
    }

    /// A boundary token is never the token an error is *about*. It is what
    /// lets whatever comes after it still be parsed, so reporting at one
    /// behaves like reporting at end of input: the complaint is recorded
    /// where the missing element should have been, and the token stays for
    /// the caller.
    ///
    /// `#Если` is one too. It opens a block whose `#КонецЕсли` is parsed as a
    /// closer wherever it stands, so an error that consumed the opener alone
    /// would hand that closer to the enclosing `#Если` and end it early: every
    /// method after the spot would fall outside a region it lies inside. Left
    /// for the statement rules, the opener pairs with its own closer.
    fn at_statement_separator(&self) -> bool {
        self.at(T![Semicolon])
            || self.at(T![PreIf])
            || self.at_declared_boundary()
            || self.at_enclosing_boundary()
            || self.at_torn_if_end()
    }

    /// The part of the boundary that holds for the whole parse, without the
    /// separator every grammar has.
    ///
    /// Kept apart from the enclosing ones because this is the only part a
    /// successful `expect` may refuse: a word that is never a name is safe to
    /// decline anywhere, and a word that is a name somewhere is not.
    fn at_declared_boundary(&self) -> bool {
        self.at_grammar_boundary.is_some_and(|at_boundary| at_boundary(self))
    }

    fn emit_error(&mut self, err: ParseError) {
        debug_assert!(matches!(err.recovery(), RecoveryKind::BumpToken | RecoveryKind::Custom));
        let m = self.start();
        if !self.at_end() {
            self.bump();
        }
        self.events.push(Event::Error(err));
        m.complete(self, NodeKind::Error);
    }

    fn emit_missing(&mut self, err: ParseError) {
        debug_assert!(err.recovery() == RecoveryKind::MissingToken);
        let m = self.start();
        self.events.push(Event::Error(err));
        m.complete(self, NodeKind::Error);
    }

    pub fn at_declaration_start(&self) -> bool {
        matches!(self.current(), Some(T![KwFunction] | T![KwProcedure]))
            && self.nth(1) == Some(T![Ident])
    }

    pub fn check_iteration_limit(&mut self) {
        self.iteration_count += 1;

        if self.recent_positions.len() >= POSITION_HISTORY_SIZE {
            self.recent_positions.remove(0);
        }
        self.recent_positions.push(self.pos);

        if self.iteration_count < MAX_ITERATIONS {
            return;
        }

        let unique_positions: std::collections::HashSet<_> = self.recent_positions.iter().collect();
        let stuck = unique_positions.len() < 5;

        if !stuck {
            tracing::debug!(
                position = self.pos,
                unique_in_window = unique_positions.len(),
                "parser guard: large input, resetting iteration counter"
            );
            self.iteration_count = 0;
            return;
        }

        let last_10: Vec<_> = self.recent_positions.iter().rev().take(10).rev().copied().collect();

        panic!(
            "Parser exceeded maximum iteration limit ({} iterations).\n\
            Position: {}, Token: {:?}\n\
            Status: STUCK (infinite loop)\n\
            Last 10 positions: {:?}\n\
            Unique positions in last {}: {}\n\
            This is a bug - the parser should always make progress.",
            MAX_ITERATIONS,
            self.pos,
            self.current(),
            last_10,
            POSITION_HISTORY_SIZE,
            unique_positions.len()
        );
    }
}

pub struct Marker {
    pos: usize,
    start_token_pos: usize,
}

impl Marker {
    fn at_event_pos(pos: usize, start_token_pos: usize) -> Self {
        Self { pos, start_token_pos }
    }

    pub fn complete(self, p: &mut Parser, kind: NodeKind) -> CompletedMarker {
        let event = &mut p.events[self.pos];
        *event = Event::Start { kind, forward_parent: None };
        p.events.push(Event::Finish);
        CompletedMarker::at_event_pos(self.pos, self.start_token_pos)
    }

    pub fn abandon(self, p: &mut Parser) {
        if self.pos == p.events.len() - 1 {
            if let Some(Event::Placeholder) = p.events.last() {
                p.events.pop();
            }
        }
    }
}

pub struct CompletedMarker {
    pos: usize,
    start_token_pos: usize,
}

impl CompletedMarker {
    fn at_event_pos(pos: usize, start_token_pos: usize) -> Self {
        Self { pos, start_token_pos }
    }

    pub fn precede(self, p: &mut Parser) -> Marker {
        let new_pos = p.events.len();
        p.events.push(Event::Placeholder);

        if let Event::Start { forward_parent, .. } = &mut p.events[self.pos] {
            *forward_parent = Some(new_pos - self.pos);
        }

        Marker::at_event_pos(new_pos, self.start_token_pos)
    }
}

/// Написания закрывателей, на которые смотрит [`Parser::resembles_any_closer`].
///
/// Таблица дублирует знание лексера, потому что обратного отображения вида в слово он не
/// даёт. Расхождение ловится тестом `every_closer_spelling_lexes_to_its_kind`: каждое
/// написание обязано разбираться именно в свой вид.
fn closer_spellings(kind: TokenKind) -> &'static [&'static str] {
    match kind {
        TokenKind::KwEndIf => &["КонецЕсли", "EndIf"],
        TokenKind::KwEndDo => &["КонецЦикла", "EndDo"],
        TokenKind::KwEndTry => &["КонецПопытки", "EndTry"],
        TokenKind::KwExcept => &["Исключение", "Except"],
        TokenKind::KwEndProcedure => &["КонецПроцедуры", "EndProcedure"],
        TokenKind::KwEndFunction => &["КонецФункции", "EndFunction"],
        TokenKind::KwElse => &["Иначе", "Else"],
        TokenKind::KwElsIf => &["ИначеЕсли", "ElsIf"],
        _ => &[],
    }
}

/// Опечатка в слове: тот же зачин и не более двух правок.
///
/// Один зачин без предела правок роднил бы `КонецЕсли` с любым словом на «Кон», один
/// предел правок без зачина — короткие закрыватели со случайными именами той же длины.
fn is_typo_of(text: &str, spelling: &str) -> bool {
    const PREFIX: usize = 3;
    const MAX_EDITS: usize = 2;

    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let spelling: Vec<char> = spelling.chars().flat_map(char::to_lowercase).collect();
    if text == spelling {
        return false;
    }
    if text.len() < PREFIX || spelling.len() < PREFIX || text[..PREFIX] != spelling[..PREFIX] {
        return false;
    }
    edit_distance_within(&text, &spelling, MAX_EDITS)
}

/// Расстояние Дамерау — Левенштейна, обрезанное сверху: точное значение здесь не нужно.
fn edit_distance_within(a: &[char], b: &[char], max: usize) -> bool {
    if a.len().abs_diff(b.len()) > max {
        return false;
    }
    let mut prev_prev: Vec<usize> = Vec::new();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let substitution = usize::from(a[i - 1] != b[j - 1]);
            current[j] = (prev[j] + 1).min(current[j - 1] + 1).min(prev[j - 1] + substitution);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                current[j] = current[j].min(prev_prev[j - 2] + 1);
            }
        }
        prev_prev =
            std::mem::replace(&mut prev, std::mem::replace(&mut current, vec![0; b.len() + 1]));
    }
    prev[b.len()] <= max
}

#[cfg(test)]
mod closer_typo_tests {
    use super::{closer_spellings, is_typo_of};
    use lexer::TokenKind;

    const CLOSERS: &[TokenKind] = &[
        TokenKind::KwEndIf,
        TokenKind::KwEndDo,
        TokenKind::KwEndTry,
        TokenKind::KwExcept,
        TokenKind::KwEndProcedure,
        TokenKind::KwEndFunction,
        TokenKind::KwElse,
        TokenKind::KwElsIf,
    ];

    /// Таблица написаний живёт отдельно от лексера, поэтому расхождение с ним обязано
    /// ловиться: каждое написание разбирается ровно в свой вид.
    #[test]
    fn every_closer_spelling_lexes_to_its_kind() {
        for &kind in CLOSERS {
            let spellings = closer_spellings(kind);
            assert!(!spellings.is_empty(), "{kind:?} остался без написаний");
            for spelling in spellings {
                let tokens = lexer::tokenize(spelling);
                let significant: Vec<_> =
                    tokens.iter().filter(|token| token.kind != TokenKind::Whitespace).collect();
                assert_eq!(significant.len(), 1, "{spelling} разобралось не одним токеном");
                assert_eq!(significant[0].kind, kind, "{spelling} разобралось не в {kind:?}");
            }
        }
    }

    #[test]
    fn a_typo_of_a_closer_is_recognised_in_both_languages() {
        assert!(is_typo_of("КонецЕслли", "КонецЕсли"));
        assert!(is_typo_of("конецесл", "КонецЕсли"));
        assert!(is_typo_of("КонцеЕсли", "КонецЕсли"));
        assert!(is_typo_of("EndIff", "EndIf"));
    }

    /// Поблажку получает попытка написать закрыватель, а не всякое слово рядом с ним.
    #[test]
    fn an_ordinary_name_is_not_a_typo_of_a_closer() {
        for name in ["А", "HHH", "Результат", "КонецМесяца", "Конец", "EndOfMonth"]
        {
            for &kind in CLOSERS {
                for spelling in closer_spellings(kind) {
                    assert!(!is_typo_of(name, spelling), "{name} принято за опечатку {spelling}");
                }
            }
        }
    }

    /// Само слово опечаткой не считается: закрыватель на месте разбирается закрывателем,
    /// и до восстановления дело не доходит.
    #[test]
    fn the_closer_itself_is_not_a_typo() {
        assert!(!is_typo_of("КонецЕсли", "КонецЕсли"));
        assert!(!is_typo_of("конецесли", "КонецЕсли"));
    }
}
