//! Syntactic evaluation events of one body: which expression trees each
//! statement and header evaluates, which bare names those trees pass straight
//! to a call, what an assignment writes, which loop variable a loop binds and
//! where `Выполнить` runs.
//!
//! The facts come from the body alone — no resolver, database, locality or
//! parameter passing — so every analysis over the shared CFG reads one set of
//! them and adds its own policy on top. Every statement the arena holds is
//! visited, preprocessor branches included, as the CFG walks them all.

use cfg_types::{BindingId, ExprId, IdConversion, StmtId};
use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::CaseExt;

use crate::body::Body;
use crate::hir::{Expr, ExprIdx, Stmt, StmtIdx};

/// Where an expression is evaluated: inside a statement of a basic block, or as
/// a header root (`Если`/`ИначеЕсли`/`Пока` condition, `Для` bound, `Для Каждого`
/// collection), which has a CFG vertex of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvalPoint {
    Stmt(StmtId),
    Header(ExprId),
}

/// One evaluated expression tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRoot {
    pub point: EvalPoint,
    pub root: ExprId,
    /// The root and every expression under it, breadth first.
    pub nodes: Box<[ExprId]>,
    /// The calls of the tree, in the order of `nodes`.
    pub calls: Box<[CallSite]>,
}

/// A `Call`, `MethodCall` or `New` expression of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub call: ExprId,
    /// The callee expression of a `Call`; method and constructor calls name
    /// theirs inline.
    pub callee: Option<ExprId>,
    /// The arguments that are bare names.
    pub path_args: Box<[PathArg]>,
}

/// A bare-name argument at its position in the call's argument list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathArg {
    pub position: u32,
    pub expr: ExprId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// `Имя = …` rebinds the name.
    Direct,
    /// `База[…] = …` stores into the value the base holds.
    Index,
    /// `База.Поле = …` stores into a field; the base keeps its value.
    Field,
}

/// The target of an assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Write {
    pub target: ExprId,
    pub kind: WriteKind,
    /// The bare name a direct write rebinds or an index chain starts from; a
    /// field write, and an index chain over a field, name none.
    pub base: Option<ExprId>,
}

/// The events of one statement. Control statements evaluate their header
/// roots; the trees of their nested statements belong to those statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StmtEvents {
    pub stmt: StmtId,
    pub roots: Box<[EvalRoot]>,
    pub write: Option<Write>,
    pub loop_binding: Option<BindingId>,
    pub execute: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyEvents {
    stmts: Box<[StmtEvents]>,
    points: FxHashMap<ExprId, EvalPoint>,
    names: Box<[String]>,
}

impl BodyEvents {
    pub fn collect(body: &Body) -> Self {
        let mut points = FxHashMap::default();
        let stmts = body
            .stmts_iter()
            .map(|(stmt_id, stmt)| {
                let roots: Box<[EvalRoot]> = stmt_roots(body, stmt_id, stmt)
                    .into_iter()
                    .map(|(point, root)| eval_root(body, point, root))
                    .collect();
                for root in roots.iter() {
                    for &node in root.nodes.iter() {
                        points.entry(node).or_insert(root.point);
                    }
                }
                StmtEvents {
                    stmt: stmt_id,
                    roots,
                    write: match stmt {
                        Stmt::Assign { target, .. } => write_of(body, ExprId::from_idx(*target)),
                        _ => None,
                    },
                    loop_binding: match stmt {
                        Stmt::For { var, .. } | Stmt::ForEach { var, .. } => {
                            Some(BindingId::from_idx(*var))
                        }
                        _ => None,
                    },
                    execute: matches!(stmt, Stmt::Execute { .. }),
                }
            })
            .collect();

        let mut names: Vec<String> = body
            .exprs_iter()
            .filter_map(|(_, expr)| match expr {
                Expr::Path(name) => Some(name.as_str().fold_lower()),
                _ => None,
            })
            .chain(body.bindings_iter().map(|(_, binding)| binding.name.as_str().fold_lower()))
            .collect();
        names.sort_unstable();
        names.dedup();

        Self { stmts, points, names: names.into_boxed_slice() }
    }

    /// Every statement of the body, in arena order.
    pub fn stmts(&self) -> &[StmtEvents] {
        &self.stmts
    }

    /// The point evaluating `expr`; `None` for an expression no statement or
    /// header evaluates, such as a parameter default.
    pub fn point_of(&self, expr: ExprId) -> Option<EvalPoint> {
        self.points.get(&expr).copied()
    }

    /// Every bare name and binding name of the body, folded to lower case,
    /// sorted and without repeats.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Approximate live heap bytes, for Salsa's `memory_usage` report.
    pub fn estimated_heap(&self) -> usize {
        use crate::heap_estimate::{map_table_bytes, vec_bytes};

        let mut bytes = vec_bytes::<StmtEvents>(self.stmts.len());
        for stmt in self.stmts.iter() {
            bytes += vec_bytes::<EvalRoot>(stmt.roots.len());
            for root in stmt.roots.iter() {
                bytes += vec_bytes::<ExprId>(root.nodes.len());
                bytes += vec_bytes::<CallSite>(root.calls.len());
                bytes += root
                    .calls
                    .iter()
                    .map(|call| vec_bytes::<PathArg>(call.path_args.len()))
                    .sum::<usize>();
            }
        }
        bytes += map_table_bytes::<ExprId, EvalPoint>(self.points.len());
        bytes += vec_bytes::<String>(self.names.len());
        bytes += self.names.iter().map(String::capacity).sum::<usize>();
        bytes
    }
}

/// The body may be missing statements or edges, so no path through it is proven.
pub fn hir_suppressed(body: &Body) -> bool {
    body.is_lowering_incomplete()
        || body.stmts_iter().any(|(_, stmt)| {
            matches!(stmt, Stmt::Expr(expr) if body.is_recovered(ExprId::from_idx(*expr)))
        })
        || has_unresolved_jump(body)
}

/// A `Перейти` names a label the body does not have: the graph cannot place the jump, so the
/// paths it should open or close are not known.
pub fn has_unresolved_jump(body: &Body) -> bool {
    let mut labels = FxHashSet::default();
    let mut gotos = Vec::new();
    for (_, stmt) in body.stmts_iter() {
        match stmt {
            Stmt::Label(label) => {
                labels.insert(label);
            }
            Stmt::Goto(label) => gotos.push(label),
            _ => {}
        }
    }
    gotos.into_iter().any(|label| !labels.contains(label))
}

fn stmt_roots(body: &Body, stmt_id: StmtId, stmt: &Stmt) -> Vec<(EvalPoint, ExprIdx)> {
    let header = |root: &ExprIdx| (EvalPoint::Header(ExprId::from_idx(*root)), *root);
    let mut roots = Vec::new();
    match stmt {
        Stmt::If(if_stmt) => {
            roots.push(header(&if_stmt.condition));
            roots.extend(if_stmt.elsif_branches.iter().map(|(condition, _)| header(condition)));
        }
        Stmt::While { condition, .. } => roots.push(header(condition)),
        Stmt::For { from, to, .. } => roots.extend([header(from), header(to)]),
        Stmt::ForEach { collection, .. } => roots.push(header(collection)),
        _ => for_each_stmt_expr_root(body, stmt_id.to_idx(), &mut |root| {
            roots.push((EvalPoint::Stmt(stmt_id), root));
        }),
    }
    roots
}

fn eval_root(body: &Body, point: EvalPoint, root: ExprIdx) -> EvalRoot {
    let mut nodes = vec![root];
    let mut pos = 0;
    while let Some(&node) = nodes.get(pos) {
        for_each_expr_child(body, node, &mut |child| nodes.push(child));
        pos += 1;
    }
    let calls = nodes
        .iter()
        .filter_map(|&node| {
            let (callee, args) = match body.expr_idx(node) {
                Expr::Call { callee, args } => (Some(ExprId::from_idx(*callee)), args),
                Expr::MethodCall { args, .. } | Expr::New { args, .. } => (None, args),
                _ => return None,
            };
            let path_args = args
                .iter()
                .enumerate()
                .filter(|(_, arg)| matches!(body.expr_idx(**arg), Expr::Path(_)))
                .map(|(position, arg)| PathArg {
                    position: position as u32,
                    expr: ExprId::from_idx(*arg),
                })
                .collect();
            Some(CallSite { call: ExprId::from_idx(node), callee, path_args })
        })
        .collect();
    EvalRoot {
        point,
        root: ExprId::from_idx(root),
        nodes: nodes.into_iter().map(ExprId::from_idx).collect(),
        calls,
    }
}

fn write_of(body: &Body, target: ExprId) -> Option<Write> {
    let kind = match body.expr(target) {
        Expr::Path(_) => WriteKind::Direct,
        Expr::Index { .. } => WriteKind::Index,
        Expr::Field { .. } => WriteKind::Field,
        _ => return None,
    };
    Some(Write { target, kind, base: index_chain_base(body, target) })
}

/// The bare name at the start of a chain of indexes, the expression itself
/// included.
fn index_chain_base(body: &Body, expr: ExprId) -> Option<ExprId> {
    match body.expr(expr) {
        Expr::Path(_) => Some(expr),
        Expr::Index { base, .. } => index_chain_base(body, ExprId::from_idx(*base)),
        _ => None,
    }
}

/// Expression roots a statement contributes to its basic block. Control
/// statements contribute none: their conditions live on dedicated CFG vertices.
pub fn for_each_stmt_expr_root(body: &Body, stmt_idx: StmtIdx, f: &mut impl FnMut(ExprIdx)) {
    match body.stmt_idx(stmt_idx) {
        Stmt::Expr(e) => f(*e),
        Stmt::Assign { target: lhs, value } => {
            f(*lhs);
            f(*value);
        }
        Stmt::Return { value } | Stmt::Raise { value } => {
            if let Some(v) = value {
                f(*v);
            }
        }
        Stmt::Execute { expr } => f(*expr),
        Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
            f(*event);
            f(*handler);
        }
        Stmt::VarDecl { .. } | Stmt::Break | Stmt::Continue | Stmt::Goto(_) | Stmt::Label(_) => {}
        Stmt::If(_)
        | Stmt::PreprocIf(_)
        | Stmt::While { .. }
        | Stmt::For { .. }
        | Stmt::ForEach { .. }
        | Stmt::Try { .. } => {}
    }
}

/// The direct subexpressions of `root`.
pub fn for_each_expr_child(body: &Body, root: ExprIdx, f: &mut impl FnMut(ExprIdx)) {
    match body.expr_idx(root) {
        Expr::Missing | Expr::Path(_) | Expr::Literal(_) => {}
        Expr::BinaryOp { lhs, rhs, .. } => {
            f(*lhs);
            f(*rhs);
        }
        Expr::UnaryOp { expr, .. } => f(*expr),
        Expr::Ternary { condition, then_expr, else_expr } => {
            f(*condition);
            f(*then_expr);
            f(*else_expr);
        }
        Expr::Call { callee, args } => {
            f(*callee);
            args.iter().copied().for_each(f);
        }
        Expr::MethodCall { receiver, args, .. } => {
            f(*receiver);
            args.iter().copied().for_each(f);
        }
        Expr::Index { base, index } => {
            f(*base);
            f(*index);
        }
        Expr::Field { base, .. } => f(*base),
        Expr::New { args, .. } => args.iter().copied().for_each(f),
        Expr::Array(elems) => elems.iter().copied().for_each(f),
        Expr::Await { expr } => f(*expr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntax::SyntaxKind;

    fn body_of(source: &str) -> std::sync::Arc<Body> {
        let parse = parser::parse(source);
        let method = parse
            .syntax_node()
            .descendants()
            .find(|node| {
                matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF)
            })
            .expect("the source declares a method");
        crate::body::lower_method(&method, false).body
    }

    fn path_named(body: &Body, expr: ExprId) -> String {
        match body.expr(expr) {
            Expr::Path(name) => name.as_str().to_string(),
            other => panic!("not a path: {other:?}"),
        }
    }

    #[test]
    fn calls_list_bare_name_arguments_with_positions() {
        let body = body_of(
            "Процедура Т(П)\n    Обработать(А, 1, Б.Поле, В);\n    Объект.Метод(Г);\n    Х = Новый Массив(Д);\nКонецПроцедуры\n",
        );
        let events = BodyEvents::collect(&body);
        let mut seen = Vec::new();
        for stmt in events.stmts() {
            for root in stmt.roots.iter() {
                for call in root.calls.iter() {
                    let args: Vec<(u32, String)> = call
                        .path_args
                        .iter()
                        .map(|arg| (arg.position, path_named(&body, arg.expr)))
                        .collect();
                    seen.push((call.callee.is_some(), args));
                }
            }
        }
        assert_eq!(
            seen,
            vec![
                (true, vec![(0, "А".to_string()), (3, "В".to_string())]),
                // A qualified call lowers to a call of the field.
                (true, vec![(0, "Г".to_string())]),
                (false, vec![(0, "Д".to_string())]),
            ]
        );
    }

    #[test]
    fn writes_tell_direct_index_and_field_targets_apart() {
        let body = body_of(
            "Процедура Т()\n    А = 1;\n    Б[0][1] = 2;\n    В.Поле = 3;\n    Г.Поле[0] = 4;\nКонецПроцедуры\n",
        );
        let events = BodyEvents::collect(&body);
        let writes: Vec<(WriteKind, Option<String>)> = events
            .stmts()
            .iter()
            .filter_map(|stmt| stmt.write)
            .map(|write| (write.kind, write.base.map(|base| path_named(&body, base))))
            .collect();
        assert_eq!(
            writes,
            vec![
                (WriteKind::Direct, Some("А".to_string())),
                (WriteKind::Index, Some("Б".to_string())),
                (WriteKind::Field, None),
                (WriteKind::Index, None),
            ]
        );
    }

    #[test]
    fn headers_loops_and_execute_are_their_own_events() {
        let body = body_of(
            "Процедура Т(Список)\n    Если А Тогда\n    ИначеЕсли Б Тогда\n    КонецЕсли;\n    Для Сч = 1 По Н Цикл\n    КонецЦикла;\n    Для Каждого Э Из Список Цикл\n    КонецЦикла;\n    Выполнить(Код);\nКонецПроцедуры\n",
        );
        let events = BodyEvents::collect(&body);
        let header_names: Vec<String> = events
            .stmts()
            .iter()
            .flat_map(|stmt| stmt.roots.iter())
            .filter(|root| matches!(root.point, EvalPoint::Header(_)))
            .map(|root| path_named_or_literal(&body, root.root))
            .collect();
        assert_eq!(header_names, vec!["А", "Б", "1", "Н", "Список"]);
        let loop_vars: Vec<String> = events
            .stmts()
            .iter()
            .filter_map(|stmt| stmt.loop_binding)
            .map(|var| body.binding(var).name.as_str().to_string())
            .collect();
        assert_eq!(loop_vars, vec!["Сч", "Э"]);
        let executes = events.stmts().iter().filter(|stmt| stmt.execute).count();
        assert_eq!(executes, 1);
        let code = body
            .exprs_iter()
            .find(|(_, expr)| matches!(expr, Expr::Path(name) if name.as_str() == "Код"))
            .map(|(id, _)| id)
            .expect("the execute argument is a path");
        assert!(matches!(events.point_of(code), Some(EvalPoint::Stmt(_))));
    }

    fn path_named_or_literal(body: &Body, expr: ExprId) -> String {
        match body.expr(expr) {
            Expr::Literal(_) => "1".to_string(),
            _ => path_named(body, expr),
        }
    }

    #[test]
    fn preprocessor_branches_are_walked() {
        let body = body_of(
            "Процедура Т()\n#Если Сервер Тогда\n    А = 1;\n#Иначе\n    Б = 2;\n#КонецЕсли\nКонецПроцедуры\n",
        );
        let events = BodyEvents::collect(&body);
        let written: Vec<String> = events
            .stmts()
            .iter()
            .filter_map(|stmt| stmt.write.and_then(|write| write.base))
            .map(|base| path_named(&body, base))
            .collect();
        assert_eq!(written, vec!["А", "Б"]);
    }

    #[test]
    fn names_are_folded_and_unique() {
        let body =
            body_of("Процедура Т(Парам)\n    Перем Х;\n    х = ПАРАМ + Ещё;\nКонецПроцедуры\n");
        let events = BodyEvents::collect(&body);
        assert_eq!(events.names(), ["ещё", "парам", "х"]);
    }
}
