use crate::hir::{
    BinaryOp, ExprHir, FunctionKind, InValues, LiteralValue, Name, TableRef, UnaryOp,
};
use crate::types::SdblType;
use rustc_hash::FxHashSet;
use syntax::ast::AstNode;
use syntax::{SyntaxKind, WalkEvent};
use text_size::TextRange;

use super::diagnostics::expanded;

/// What the HIR of one query level does not keep, recorded by the range of the node.
///
/// Lowering writes a single-root `В ИЕРАРХИИ` and `ССЫЛКА` as an equality and erases the
/// member of `(...).Поле`. Their null behaviour differs from what those shapes say, and the
/// rest of the crate relies on them, so the facts are read from the syntax here.
#[derive(Default)]
pub(super) struct QueryFacts {
    single_root_hierarchies: FxHashSet<TextRange>,
    reference_checks: FxHashSet<TextRange>,
    dereferenced_parens: Vec<TextRange>,
}

impl QueryFacts {
    pub(super) fn of(query: &syntax::ast::SdblQuery) -> Self {
        let mut facts = Self::default();
        let mut preorder = query.syntax().preorder();
        while let Some(event) = preorder.next() {
            let WalkEvent::Enter(node) = event else {
                continue;
            };
            match node.kind() {
                // A nested query is lowered, and checked, on its own.
                SyntaxKind::SDBL_SUBQUERY | SyntaxKind::SDBL_SUBQUERY_EXPR => {
                    preorder.skip_subtree();
                }
                SyntaxKind::SDBL_IN_HIERARCHY_EXPR => {
                    facts.single_root_hierarchies.insert(node.text_range());
                }
                SyntaxKind::SDBL_REFS_EXPR => {
                    facts.reference_checks.insert(node.text_range());
                }
                SyntaxKind::SDBL_PAREN_EXPR
                    if syntax::ast_utils::direct_name_tokens(&node).next().is_some() =>
                {
                    facts.dereferenced_parens.push(node.text_range());
                }
                _ => {}
            }
        }
        facts
    }

    /// The expression is read through `(...).Поле`, so it is a step along a reference path
    /// even though its HIR looks like a direct field.
    pub(super) fn is_dereferenced(&self, range: TextRange) -> bool {
        self.dereferenced_parens.iter().any(|parens| parens.contains_range(range))
    }
}

/// The set of results a condition may have: true, false, or undefined (a comparison with
/// NULL). Undefined is kept apart from false because `НЕ` leaves it undefined.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Outcomes(u8);

impl Outcomes {
    const TRUE: Self = Self(0b001);
    const FALSE: Self = Self(0b010);
    const UNDEFINED: Self = Self(0b100);
    const ANY: Self = Self(0b111);

    fn of(value: bool) -> Self {
        if value {
            Self::TRUE
        } else {
            Self::FALSE
        }
    }

    fn has(self, outcome: Self) -> bool {
        self.0 & outcome.0 != 0
    }

    pub(super) fn may_be_true(self) -> bool {
        self.has(Self::TRUE)
    }

    pub(super) fn is_always_true(self) -> bool {
        self == Self::TRUE
    }

    fn not(self) -> Self {
        let mut result = self.0 & Self::UNDEFINED.0;
        if self.has(Self::TRUE) {
            result |= Self::FALSE.0;
        }
        if self.has(Self::FALSE) {
            result |= Self::TRUE.0;
        }
        Self(result)
    }

    fn and(self, other: Self) -> Self {
        let not_false = Self(Self::TRUE.0 | Self::UNDEFINED.0);
        let mut result = 0;
        if self.has(Self::TRUE) && other.has(Self::TRUE) {
            result |= Self::TRUE.0;
        }
        if self.has(Self::FALSE) || other.has(Self::FALSE) {
            result |= Self::FALSE.0;
        }
        if (self.has(Self::UNDEFINED) && other.has(not_false))
            || (other.has(Self::UNDEFINED) && self.has(not_false))
        {
            result |= Self::UNDEFINED.0;
        }
        Self(result)
    }

    fn or(self, other: Self) -> Self {
        self.not().and(other.not()).not()
    }
}

#[derive(Clone, Copy)]
enum Constant<'e> {
    Number(i64),
    Boolean(bool),
    String(&'e str),
    MinimalDate,
}

impl Constant<'_> {
    /// `None` when the constants are of different kinds or their strings are spelled
    /// differently: whether those are equal depends on the database collation.
    fn equals(&self, other: &Self) -> Option<bool> {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => Some(a == b),
            (Self::Boolean(a), Self::Boolean(b)) => Some(a == b),
            (Self::MinimalDate, Self::MinimalDate) => Some(true),
            (Self::String(a), Self::String(b)) if a == b => Some(true),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Value<'e> {
    Null,
    Constant(Constant<'e>),
    /// Any value, NULL included.
    Unknown,
}

/// How a condition names the side. A `ВЫБОР` matches Latin aliases regardless of case and
/// `ГДЕ` matches them as spelled, as the rule has always done.
#[derive(Clone, Copy)]
pub(super) enum AliasMatch {
    AsciiFold,
    Exact,
}

/// Evaluates conditions of the current query for a row in which `side` is missing: its
/// direct fields are NULL, everything else is unknown. Nothing here runs the query; an
/// expression it cannot decide has every outcome.
#[derive(Clone, Copy)]
pub(super) struct MissingRowConditions<'a> {
    pub(super) side: &'a TableRef,
    pub(super) facts: &'a QueryFacts,
    pub(super) aliases: AliasMatch,
}

impl MissingRowConditions<'_> {
    /// The outcomes of a `КОГДА`; a simple `ВЫБОР` compares its operand with the value.
    pub(super) fn of_when(&self, operand: Option<&ExprHir>, when: &ExprHir) -> Outcomes {
        match operand {
            Some(operand) => compare(BinaryOp::Eq, self.value(operand), self.value(when)),
            None => self.of(when),
        }
    }

    pub(super) fn of(&self, condition: &ExprHir) -> Outcomes {
        match condition {
            ExprHir::BinaryOp { lhs, op: BinaryOp::And, rhs, .. } => self.of(lhs).and(self.of(rhs)),
            ExprHir::BinaryOp { lhs, op: BinaryOp::Or, rhs, .. } => self.of(lhs).or(self.of(rhs)),
            ExprHir::UnaryOp { op: UnaryOp::Not, expr, .. } => self.of(expr).not(),
            ExprHir::BinaryOp { lhs, op: BinaryOp::Eq, rhs, range, .. }
                if self.facts.single_root_hierarchies.contains(range) =>
            {
                self.membership(lhs, std::slice::from_ref(rhs))
            }
            ExprHir::BinaryOp { lhs, op: BinaryOp::Eq, range, .. }
                if self.facts.reference_checks.contains(range) =>
            {
                match self.value(lhs) {
                    Value::Null => Outcomes::UNDEFINED,
                    _ => Outcomes::ANY,
                }
            }
            ExprHir::BinaryOp { lhs, op, rhs, .. } if op.is_comparison() => {
                // An operand left unwritten proves nothing about the comparison.
                if matches!(**lhs, ExprHir::Missing { .. })
                    || matches!(**rhs, ExprHir::Missing { .. })
                {
                    return Outcomes::ANY;
                }
                compare(*op, self.value(lhs), self.value(rhs))
            }
            ExprHir::IsNull { expr, negated, .. } => match self.value(expr) {
                Value::Null => Outcomes::of(!negated),
                Value::Constant(_) => Outcomes::of(*negated),
                Value::Unknown => Outcomes::ANY,
            },
            ExprHir::In { expr, negated, values, .. } => {
                let items = match values {
                    InValues::List(items) => items.as_slice(),
                    InValues::Subquery(_) => &[],
                };
                negate_if(*negated, self.membership(expr, items))
            }
            ExprHir::Between { expr, negated, low, high, .. } => {
                let tested = self.value(expr);
                let inside = compare(BinaryOp::Ge, tested, self.value(low)).and(compare(
                    BinaryOp::Le,
                    tested,
                    self.value(high),
                ));
                negate_if(*negated, inside)
            }
            ExprHir::Like { expr, negated, pattern, .. } => {
                let compared = match (self.value(expr), self.value(pattern)) {
                    (Value::Null, _) | (_, Value::Null) => Outcomes::UNDEFINED,
                    _ => Outcomes::ANY,
                };
                negate_if(*negated, compared)
            }
            _ => match self.value(condition) {
                Value::Null => Outcomes::UNDEFINED,
                Value::Constant(Constant::Boolean(value)) => Outcomes::of(value),
                _ => Outcomes::ANY,
            },
        }
    }

    /// NULL against a list that holds a scalar is undefined. A parameter or a nested query
    /// may hold nothing, and then the membership is false, which `НЕ` turns into true.
    fn membership(&self, tested: &ExprHir, items: &[ExprHir]) -> Outcomes {
        match self.value(tested) {
            Value::Null if items.iter().any(is_scalar_item) => Outcomes::UNDEFINED,
            Value::Null => Outcomes(Outcomes::FALSE.0 | Outcomes::UNDEFINED.0),
            _ => Outcomes::ANY,
        }
    }

    fn value<'e>(&self, expr: &'e ExprHir) -> Value<'e> {
        if self.facts.is_dereferenced(expr.range()) {
            return Value::Unknown;
        }
        match expr {
            ExprHir::ColumnRef { parts, .. } if self.is_direct_field(parts) => Value::Null,
            ExprHir::Literal { value: LiteralValue::Null, .. } => Value::Null,
            ExprHir::BinaryOp { lhs, op, rhs, .. } if op.is_arithmetic() => {
                self.propagates_null([&**lhs, &**rhs])
            }
            ExprHir::UnaryOp { op: UnaryOp::Neg | UnaryOp::Pos, expr, .. } => {
                self.propagates_null([&**expr])
            }
            ExprHir::FunctionCall {
                function: FunctionKind::Isnull, args, member_access, ..
            } if member_access.is_empty() => self.isnull(args),
            ExprHir::FunctionCall { function, args, member_access, .. }
                if member_access.is_empty()
                    && is_function(
                        function,
                        &FunctionKind::DateDiff,
                        "РАЗНОСТЬДАТ",
                        "DATEDIFF",
                    ) =>
            {
                self.propagates_null(args.iter().take(2))
            }
            _ => constant(expr).map_or(Value::Unknown, Value::Constant),
        }
    }

    fn propagates_null<'e>(&self, operands: impl IntoIterator<Item = &'e ExprHir>) -> Value<'e> {
        if operands.into_iter().any(|operand| matches!(self.value(operand), Value::Null)) {
            Value::Null
        } else {
            Value::Unknown
        }
    }

    /// `ЕСТЬNULL(Поле, З)` of a missing row is `З` only if the value survives the conversion
    /// to the type of the field; that is known for the field itself, not for an expression.
    fn isnull<'e>(&self, args: &'e [ExprHir]) -> Value<'e> {
        let [field @ ExprHir::ColumnRef { .. }, replacement] = args else {
            return Value::Unknown;
        };
        if !matches!(self.value(field), Value::Null) {
            return Value::Unknown;
        }
        match constant(replacement) {
            Some(constant) if self.replacement_survives(field.ty(), &constant) => {
                Value::Constant(constant)
            }
            _ => Value::Unknown,
        }
    }

    fn replacement_survives(&self, field: &SdblType, replacement: &Constant<'_>) -> bool {
        match (expanded(field), replacement) {
            (SdblType::Number { .. }, Constant::Number(0)) => true,
            (
                SdblType::Number { precision: Some(precision), scale: Some(scale) },
                Constant::Number(number),
            ) => {
                let integer_digits = usize::from(precision.saturating_sub(*scale));
                number.unsigned_abs().to_string().len() <= integer_digits
            }
            (SdblType::Boolean, Constant::Boolean(_)) => true,
            (SdblType::Date | SdblType::DateTime, Constant::MinimalDate) => true,
            (SdblType::String { length }, Constant::String(text)) => {
                // Observed on the platform for a temporary table only: its field keeps the
                // whole replacement, however short the field is.
                self.side_is_temporary_table()
                    || match length {
                        Some(0) => true,
                        Some(length) => text.chars().count() <= *length as usize,
                        None => false,
                    }
            }
            _ => false,
        }
    }

    fn side_is_temporary_table(&self) -> bool {
        matches!(self.side.metadata, Some(crate::hir::ResolvedTable::TempTable { .. }))
            && self.side.subquery.is_empty()
    }

    fn is_direct_field(&self, parts: &[Name]) -> bool {
        let [alias, _] = parts else {
            return false;
        };
        match self.aliases {
            AliasMatch::AsciiFold => {
                alias.as_str().eq_ignore_ascii_case(self.side.effective_name())
            }
            AliasMatch::Exact => alias.as_str() == self.side.effective_name(),
        }
    }
}

fn negate_if(negated: bool, outcomes: Outcomes) -> Outcomes {
    if negated {
        outcomes.not()
    } else {
        outcomes
    }
}

/// A comparison with NULL is undefined whatever stands on the other side. Of constants only
/// equality of one kind is decided, which is all a replacement of `ЕСТЬNULL` needs.
fn compare(op: BinaryOp, lhs: Value<'_>, rhs: Value<'_>) -> Outcomes {
    match (lhs, rhs) {
        (Value::Null, _) | (_, Value::Null) => Outcomes::UNDEFINED,
        (Value::Constant(lhs), Value::Constant(rhs)) => match (op, lhs.equals(&rhs)) {
            (BinaryOp::Eq, Some(equal)) => Outcomes::of(equal),
            (BinaryOp::Ne, Some(equal)) => Outcomes::of(!equal),
            _ => Outcomes::ANY,
        },
        _ => Outcomes::ANY,
    }
}

/// shortcut: a fractional or signed literal stays unknown as a constant; modelling the
/// rounding of a fraction and the conversion of a sign to the field's type would extend it.
fn constant(expr: &ExprHir) -> Option<Constant<'_>> {
    match expr {
        ExprHir::Literal { value: LiteralValue::Integer(number), .. } => {
            Some(Constant::Number(*number))
        }
        ExprHir::Literal { value: LiteralValue::Boolean(value), .. } => {
            Some(Constant::Boolean(*value))
        }
        ExprHir::Literal {
            value: LiteralValue::String(text), ty: SdblType::String { .. }, ..
        } => {
            // A string the lexer split into several tokens keeps its quotes in the HIR.
            let content = text.strip_prefix('"').and_then(|text| text.strip_suffix('"'));
            Some(Constant::String(content.unwrap_or(text)))
        }
        // Lowering recognises a Cyrillic boolean only in lower case and keeps any other
        // spelling as text of an unknown type.
        ExprHir::Literal { value: LiteralValue::String(text), .. }
            if stdx::case::eq_ignore_case(text, "ИСТИНА") =>
        {
            Some(Constant::Boolean(true))
        }
        ExprHir::Literal { value: LiteralValue::String(text), .. }
            if stdx::case::eq_ignore_case(text, "ЛОЖЬ") =>
        {
            Some(Constant::Boolean(false))
        }
        _ if is_minimal_date(expr) => Some(Constant::MinimalDate),
        _ => None,
    }
}

/// `ДАТАВРЕМЯ(1, 1, 1)`, the empty date; no other call of the constructor is computed.
fn is_minimal_date(expr: &ExprHir) -> bool {
    let ExprHir::FunctionCall { function, args, member_access, .. } = expr else {
        return false;
    };
    let is_one =
        |arg: &ExprHir| matches!(arg, ExprHir::Literal { value: LiteralValue::Integer(1), .. });
    member_access.is_empty()
        && is_function(function, &FunctionKind::DateTime, "ДАТАВРЕМЯ", "DATETIME")
        && args.len() == 3
        && args.iter().all(is_one)
}

/// Lowering leaves functions it does not list as `Unknown` with the name as written.
fn is_function(function: &FunctionKind, kind: &FunctionKind, ru: &str, en: &str) -> bool {
    function == kind
        || matches!(function, FunctionKind::Unknown(name)
            if stdx::case::eq_ignore_case(name, ru) || stdx::case::eq_ignore_case(name, en))
}

/// An item that is one value for certain, unlike a parameter, which may be an empty array.
fn is_scalar_item(item: &ExprHir) -> bool {
    match item {
        ExprHir::Literal { .. } => true,
        ExprHir::FunctionCall { function: FunctionKind::Value, .. } => true,
        _ => is_minimal_date(item),
    }
}
