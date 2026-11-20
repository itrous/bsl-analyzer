//! The words that open, continue and close a construct, for the readers that
//! count constructs over plain tokens instead of parsing them.

use crate::parser::input::Sig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Block {
    If,
    Loop,
    Try,
    Procedure,
    Function,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockWord {
    Opens(Block),
    Continues(Block),
    Closes(Block),
}

/// Whether a keyword after `previous` is a name rather than the word of a
/// construct: behind a dot it names a member (`Условие.Иначе`), behind a
/// tilde a label (`~КонецЦикла:`).
pub(crate) fn is_name_after(previous: Option<Sig>) -> bool {
    matches!(previous, Some(T![Dot] | T![Tilde]))
}

/// What `word` does to the construct around it, `previous` being the
/// significant token before it.
pub(crate) fn block_word(word: Sig, previous: Option<Sig>) -> Option<BlockWord> {
    use Block::*;
    use BlockWord::*;

    if is_name_after(previous) {
        return None;
    }
    Some(match word {
        T![KwIf] => Opens(If),
        T![KwWhile] | T![KwFor] => Opens(Loop),
        T![KwTry] => Opens(Try),
        T![KwProcedure] => Opens(Procedure),
        T![KwFunction] => Opens(Function),
        T![KwElsIf] | T![KwElse] => Continues(If),
        T![KwExcept] => Continues(Try),
        T![KwEndIf] => Closes(If),
        T![KwEndDo] => Closes(Loop),
        T![KwEndTry] => Closes(Try),
        T![KwEndProcedure] => Closes(Procedure),
        T![KwEndFunction] => Closes(Function),
        _ => return None,
    })
}
