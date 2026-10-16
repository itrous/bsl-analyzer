//! Conditional groups (`#Если … #КонецЕсли`) as the compiler sees them: a
//! text-level choice of lines, made before the module is parsed.
//!
//! The platform evaluates the conditions for each environment and compiles
//! the lines of the branch that holds, so a group may tear a construct apart:
//! stand inside an expression, between `Если` and its `Тогда`, or give each
//! branch its own header for one shared body and one shared `КонецЕсли`.
//! Every environment then sees a whole construct, while the source as written
//! has none.
//!
//! A tree cannot hold every variant of such text at once. A group whose
//! branches each hold whole constructs keeps the structured reading — every
//! branch parsed, the group a container. A group that tears a construct is
//! read as one variant instead: the lines of one branch as code, its directive
//! lines and the other branches as opaque parts. What decides which group is
//! which, and which branch is read, is computed here once for the whole input.

use crate::parser::input::Sig;

/// A closed group, by positions among significant tokens.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Conditional {
    /// The `#Если`.
    pub(crate) open: usize,
    /// The `#КонецЕсли`.
    pub(crate) close: usize,
    /// Every branch holds whole constructs: what a branch opens it closes, it
    /// closes nothing opened outside it, and no clause word of an outer
    /// construct stands at its top level.
    pub(crate) balanced: bool,
    /// The branch read as code when the group tears a construct apart: from
    /// the first token after its header to the directive that ends it. `None`
    /// when no condition holds and there is no `#Иначе`.
    pub(crate) active: Option<(usize, usize)>,
}

/// One branch: where its code starts and ends, and whether it is taken.
struct Branch {
    body: (usize, usize),
    holds: bool,
}

/// The closed groups of the input, sorted by the position of their `#Если`.
///
/// A group whose header has no `Тогда` before the next directive of the
/// group is left out: its structure is not known, and it keeps whatever the
/// structured reading makes of it, errors included. So is a group without
/// `#КонецЕсли`. Directives inside text an extension removed are not
/// directives of the module and are not counted.
pub(crate) fn conditionals(kind: impl Fn(usize) -> Option<Sig>) -> Vec<Conditional> {
    let mut raw: Vec<(usize, Vec<usize>, usize)> = Vec::new();
    let mut open: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut pos = 0;
    while let Some(k) = kind(pos) {
        match k {
            T![PreIf] => open.push((pos, Vec::new())),
            T![PreElsIf] | T![PreElse] => {
                if let Some((_, clauses)) = open.last_mut() {
                    clauses.push(pos);
                }
            }
            T![PreEndIf] => {
                if let Some((start, clauses)) = open.pop() {
                    raw.push((start, clauses, pos));
                }
            }
            T![PreDelete] => {
                while !matches!(kind(pos + 1), Some(T![PreEndDelete]) | None) {
                    pos += 1;
                }
                pos += 1;
            }
            _ => {}
        }
        pos += 1;
    }

    // `raw` is in the order the groups close, so a nested group is decided
    // before the group around it asks about it.
    let mut decided: Vec<Conditional> = Vec::with_capacity(raw.len());
    let mut by_open: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (start, clauses, close) in raw {
        let Some(branches) = branches(&kind, start, &clauses, close) else {
            continue;
        };
        let active = branches.iter().find(|branch| branch.holds).map(|branch| branch.body);
        let balanced = branches.iter().all(|branch| {
            let mut blocks = Vec::new();
            let whole = walk(&kind, branch.body, &mut blocks, &decided, &by_open);
            whole && blocks.is_empty()
        });
        by_open.insert(start, decided.len());
        decided.push(Conditional { open: start, close, balanced, active });
    }
    decided.sort_unstable_by_key(|group| group.open);
    decided
}

/// Whether the whole input holds whole constructs, read as the parser reads
/// it: what it opens it closes, and it closes or continues nothing else.
///
/// The decision about a group around a method depends on the method's text
/// only through this, so a method edited from one whole text to another
/// leaves every group around it decided as it was.
pub(crate) fn whole(kind: impl Fn(usize) -> Option<Sig>, groups: &[Conditional]) -> bool {
    let by_open = groups.iter().enumerate().map(|(index, group)| (group.open, index)).collect();
    let mut end = 0;
    while kind(end).is_some() {
        end += 1;
    }
    let mut blocks = Vec::new();
    walk(&kind, (0, end), &mut blocks, groups, &by_open) && blocks.is_empty()
}

/// The branches of a group, or `None` when a header has no `Тогда`.
fn branches(
    kind: &impl Fn(usize) -> Option<Sig>,
    start: usize,
    clauses: &[usize],
    close: usize,
) -> Option<Vec<Branch>> {
    let directives: Vec<usize> = std::iter::once(start).chain(clauses.iter().copied()).collect();
    let mut branches = Vec::with_capacity(directives.len());
    for (index, &directive) in directives.iter().enumerate() {
        let end = directives.get(index + 1).copied().unwrap_or(close);
        if kind(directive) == Some(T![PreElse]) {
            branches.push(Branch { body: (directive + 1, end), holds: true });
            continue;
        }
        let then = (directive + 1..end).find(|&at| kind(at) == Some(T![KwThen]))?;
        let holds = Condition { kind, at: directive + 1, end: then }.holds();
        branches.push(Branch { body: (then + 1, end), holds });
    }
    Some(branches)
}

/// A condition evaluated with every symbol defined.
///
/// Any fixed assignment would do for parsing: what matters is that the same
/// condition always picks the same branch, so a construct torn by several
/// groups with one condition is put together from halves that belong
/// together. Taking every symbol as defined is the assignment that needs no
/// list of symbols.
struct Condition<'k, K> {
    kind: &'k K,
    at: usize,
    end: usize,
}

impl<K: Fn(usize) -> Option<Sig>> Condition<'_, K> {
    fn holds(mut self) -> bool {
        self.or()
    }

    fn peek(&self) -> Option<Sig> {
        if self.at < self.end {
            (self.kind)(self.at)
        } else {
            None
        }
    }

    fn or(&mut self) -> bool {
        let mut value = self.and();
        while self.peek() == Some(T![KwOr]) {
            self.at += 1;
            let rhs = self.and();
            value = value || rhs;
        }
        value
    }

    fn and(&mut self) -> bool {
        let mut value = self.operand();
        while self.peek() == Some(T![KwAnd]) {
            self.at += 1;
            let rhs = self.operand();
            value = value && rhs;
        }
        value
    }

    fn operand(&mut self) -> bool {
        match self.peek() {
            Some(T![KwNot]) => {
                self.at += 1;
                !self.operand()
            }
            Some(T![LParen]) => {
                self.at += 1;
                let value = self.or();
                if self.peek() == Some(T![RParen]) {
                    self.at += 1;
                }
                value
            }
            Some(_) => {
                self.at += 1;
                true
            }
            None => true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    If,
    Loop,
    Try,
    Procedure,
    Function,
}

/// Walks code from `range.0` to `range.1`, keeping the constructs it opens on
/// `blocks`. Returns `false` when the code closes or continues a construct it
/// did not open.
///
/// A nested group counts as the parser will read it: a balanced one as
/// nothing, since each of its branches is whole; a torn one as its branch.
///
/// An annotation counts as opening the declaration it belongs to: a range
/// that ends between the two gives one declaration a header per branch.
fn walk(
    kind: &impl Fn(usize) -> Option<Sig>,
    range: (usize, usize),
    blocks: &mut Vec<Block>,
    decided: &[Conditional],
    by_open: &std::collections::HashMap<usize, usize>,
) -> bool {
    let mut whole = true;
    let mut annotated = false;
    let mut at = range.0;
    while at < range.1 {
        let Some(k) = kind(at) else { break };
        if k == T![PreIf] {
            if let Some(group) = by_open.get(&at).map(|&index| decided[index]) {
                if !group.balanced {
                    if let Some(branch) = group.active {
                        whole &= walk(kind, branch, blocks, decided, by_open);
                    }
                }
                at = group.close + 1;
                continue;
            }
        }
        if k == T![PreDelete] {
            while at < range.1 && kind(at) != Some(T![PreEndDelete]) {
                at += 1;
            }
            at += 1;
            continue;
        }
        // A keyword behind a dot is a member name and one behind a tilde a
        // label (`~КонецЦикла:`), not the word of a construct.
        let name = at > 0 && matches!(kind(at - 1), Some(T![Dot] | T![Tilde]));
        if !name {
            let opens = match k {
                T![KwIf] => Some(Block::If),
                T![KwWhile] | T![KwFor] => Some(Block::Loop),
                T![KwTry] => Some(Block::Try),
                T![KwProcedure] => Some(Block::Procedure),
                T![KwFunction] => Some(Block::Function),
                _ => None,
            };
            let closes = match k {
                T![KwEndIf] => Some(Block::If),
                T![KwEndDo] => Some(Block::Loop),
                T![KwEndTry] => Some(Block::Try),
                T![KwEndProcedure] => Some(Block::Procedure),
                T![KwEndFunction] => Some(Block::Function),
                _ => None,
            };
            let continues = match k {
                T![KwElsIf] | T![KwElse] => Some(Block::If),
                T![KwExcept] => Some(Block::Try),
                _ => None,
            };
            if matches!(
                k,
                T![AnnAtClient]
                    | T![AnnAtServer]
                    | T![AnnAtServerNoContext]
                    | T![AnnAtClientAtServer]
                    | T![AnnAtClientAtServerNoContext]
                    | T![AnnBefore]
                    | T![AnnAfter]
                    | T![AnnAround]
                    | T![AnnChangeAndValidate]
                    | T![AnnCustom]
            ) {
                annotated = true;
            } else if matches!(k, T![KwProcedure] | T![KwFunction] | T![KwVar]) {
                annotated = false;
            }
            if let Some(block) = opens {
                blocks.push(block);
            } else if let Some(block) = closes {
                if blocks.last() == Some(&block) {
                    blocks.pop();
                } else {
                    whole = false;
                }
            } else if let Some(block) = continues {
                if blocks.last() != Some(&block) {
                    whole = false;
                }
            }
        }
        at += 1;
    }
    whole && !annotated
}
