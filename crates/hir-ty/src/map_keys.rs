//! Value types of `Соответствие.Получить("ключ")` proven from the body's own construction.
//!
//! A local seeded by an argument-less `Новый Соответствие` and filled only by standalone
//! `.Вставить("ключ", литерал)` statements has a knowable value type per key. The collection is
//! purely syntactic (no `db`), walks statement lists in source order and answers each literal-key
//! read by the `ExprId` of its call, so the answer never depends on the inference cache and the
//! `method_return_type` fixpoint converges.
//!
//! The `Структура` collector is not reused: a map's values leave the map, so every written type of a
//! key counts (not the last write), and presence depends on whether the write executes before the
//! read, which needs a source-order walk over statement lists rather than the arena order.

use rustc_hash::FxHashMap;
use stdx::case::CaseExt;

use bsl_types::builders::Builders;
use bsl_types::kind::TypeId;
use cfg_types::{ExprId, IdConversion};
use hir_def::body::Body;
use hir_def::hir::{Expr, ExprIdx, Literal, Stmt, StmtIdx};
use hir_def::Name;

use crate::db::HirDatabase;
use crate::method_lookup::is_platform_name;
use crate::structure_keys::{as_method_call, is_insert_method, scalar_kind_of, ScalarKind};

/// Distinct keys (after case folding) a tracked map may hold; above it the root gives no answers
/// at all rather than answering for some of its keys.
pub(crate) const MAX_MAP_KEYS: usize = 64;

/// The answer for one literal-key read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyAnswer {
    /// Distinct literal kinds written under the key anywhere in the root's scope.
    pub(crate) kinds: Vec<ScalarKind>,
    /// An unconditional write of the key executes before the read.
    pub(crate) present: bool,
}

/// What the body does with a name, for receivers whose keys come from documentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NameStats {
    /// Assignments to the name, loop variables included.
    pub(crate) assignments: usize,
    /// The value was mutated by something other than a literal-key insert, or escaped.
    pub(crate) touched: bool,
    /// A literal-key `Вставить` statement wrote into the value.
    pub(crate) written: bool,
    /// An assignment gave the name something other than a call result — possibly the very map
    /// another name holds, which the body may mutate through that name.
    pub(crate) aliased: bool,
}

/// Facts of one body: constructor answers by read call, and per-name statistics.
#[derive(Debug, Default)]
pub(crate) struct MapKeyFacts {
    opaque: bool,
    names: FxHashMap<String, NameStats>,
    /// Read call → (folded root name, answer).
    answers: FxHashMap<ExprId, (String, KeyAnswer)>,
}

impl MapKeyFacts {
    /// `Выполнить`, `Вычислить` or `Перейти` occurs in the body: nothing about any local is
    /// provable.
    pub(crate) fn opaque(&self) -> bool {
        self.opaque
    }

    pub(crate) fn name_stats(&self, name: &Name) -> NameStats {
        self.names.get(&name.as_str().fold_lower()).copied().unwrap_or_default()
    }

    pub(crate) fn answer(&self, call: ExprId) -> Option<&KeyAnswer> {
        self.answers.get(&call).map(|(_, answer)| answer)
    }

    /// Folded names of the roots that have at least one answer.
    pub(crate) fn answer_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self.answers.values().map(|(root, _)| root.clone()).collect();
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    /// Drops the constructor answers of the roots `keep` rejects; name statistics stay.
    pub(crate) fn retain_roots(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.answers.retain(|_, (root, _)| keep(root));
    }
}

/// The type of an answer: the union of the written kinds, plus `Неопределено` when presence of the
/// key is not proven.
pub(crate) fn answer_type(db: &dyn HirDatabase, answer: &KeyAnswer) -> TypeId {
    let mut members: Vec<TypeId> =
        answer.kinds.iter().map(|kind| crate::structure_keys::scalar_type(db, *kind)).collect();
    if !answer.present {
        members.push(db.undefined());
    }
    db.union(members)
}

/// Cheap gate: does the body read a map by a literal key at all?
pub(crate) fn body_reads_literal_key(body: &Body) -> bool {
    body.exprs_iter().any(|(_, expr)| {
        as_method_call(body, expr).is_some_and(|(_, method, args)| {
            is_get_method(method) && literal_key(body, args).is_some()
        })
    })
}

/// Collects the constructor answers and name statistics of the body.
pub(crate) fn collect_map_key_facts(body: &Body) -> MapKeyFacts {
    let mut collector = Collector {
        body,
        next_list: 0,
        opaque: false,
        names: FxHashMap::default(),
        reads: Vec::new(),
    };
    collector.visit_list(body.body_stmts_typed(), &mut Vec::new());
    collector.resolve()
}

/// Statement position: `(list id, index)` pairs from the top-level list down.
type StmtPath = Vec<(u32, u32)>;

struct Write {
    key: String,
    kind: Option<ScalarKind>,
    path: StmtPath,
}

struct Read {
    call: ExprId,
    name: String,
    key: String,
    path: StmtPath,
}

#[derive(Default)]
struct NameEvents {
    stats: NameStats,
    seed: Option<StmtPath>,
    mentions: Vec<StmtPath>,
    writes: Vec<Write>,
}

struct Collector<'b> {
    body: &'b Body,
    next_list: u32,
    opaque: bool,
    names: FxHashMap<String, NameEvents>,
    reads: Vec<Read>,
}

impl<'b> Collector<'b> {
    fn events(&mut self, name: &Name) -> &mut NameEvents {
        self.names.entry(name.as_str().fold_lower()).or_default()
    }

    fn mention(&mut self, name: &Name, path: &StmtPath) {
        self.events(name).mentions.push(path.clone());
    }

    fn touch(&mut self, name: &Name) {
        self.events(name).stats.touched = true;
    }

    fn visit_list(&mut self, stmts: &[StmtIdx], parent: &mut StmtPath) {
        let list = self.next_list;
        self.next_list += 1;
        for (index, stmt) in stmts.iter().enumerate() {
            parent.push((list, index as u32));
            self.visit_stmt(*stmt, parent);
            parent.pop();
        }
    }

    fn visit_stmt(&mut self, stmt: StmtIdx, path: &mut StmtPath) {
        let body = self.body;
        match body.stmt_idx(stmt) {
            Stmt::Expr(expr) => {
                if !self.literal_insert_statement(*expr, path) {
                    self.visit_expr(*expr, false, path);
                }
            }
            Stmt::Assign { target, value } => {
                self.visit_assign(*target, *value, path);
            }
            Stmt::If(if_stmt) => {
                self.visit_expr(if_stmt.condition, false, path);
                for (condition, _) in if_stmt.elsif_branches.iter() {
                    self.visit_expr(*condition, false, path);
                }
                self.visit_list(&if_stmt.then_branch, path);
                for (_, branch) in if_stmt.elsif_branches.iter() {
                    self.visit_list(branch, path);
                }
                if let Some(branch) = &if_stmt.else_branch {
                    self.visit_list(branch, path);
                }
            }
            Stmt::PreprocIf(preproc) => {
                self.visit_list(&preproc.then_branch, path);
                for (_, _, branch) in preproc.elsif_branches.iter() {
                    self.visit_list(branch, path);
                }
                if let Some(branch) = &preproc.else_branch {
                    self.visit_list(branch, path);
                }
            }
            Stmt::While { condition, body: branch } => {
                self.visit_expr(*condition, false, path);
                self.visit_list(branch, path);
            }
            Stmt::For { var, from, to, body: branch } => {
                self.visit_loop_binding(body.binding_idx(*var).name.clone(), path);
                self.visit_expr(*from, false, path);
                self.visit_expr(*to, false, path);
                self.visit_list(branch, path);
            }
            Stmt::ForEach { var, collection, body: branch } => {
                self.visit_loop_binding(body.binding_idx(*var).name.clone(), path);
                self.visit_expr(*collection, false, path);
                self.visit_list(branch, path);
            }
            Stmt::Try { body: branch, except } => {
                self.visit_list(branch, path);
                self.visit_list(except, path);
            }
            Stmt::Return { value } | Stmt::Raise { value } => {
                if let Some(value) = value {
                    self.visit_expr(*value, true, path);
                }
            }
            Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
                self.visit_expr(*event, true, path);
                self.visit_expr(*handler, true, path);
            }
            Stmt::Execute { .. } | Stmt::Goto(_) => self.opaque = true,
            Stmt::VarDecl { .. } | Stmt::Break | Stmt::Continue | Stmt::Label(_) => {}
        }
    }

    fn visit_loop_binding(&mut self, name: Name, path: &StmtPath) {
        let stats = &mut self.events(&name).stats;
        stats.assignments += 1;
        stats.aliased = true;
        self.mention(&name, path);
    }

    fn visit_assign(&mut self, target: ExprIdx, value: ExprIdx, path: &StmtPath) {
        let body = self.body;
        match body.expr_idx(target) {
            Expr::Path(name) => {
                let is_seed = is_empty_map_constructor(body, value);
                let from_call =
                    matches!(body.expr_idx(value), Expr::Call { .. } | Expr::MethodCall { .. });
                let events = self.events(name);
                events.stats.assignments += 1;
                events.stats.aliased |= !from_call;
                if is_seed && events.seed.is_none() {
                    events.seed = Some(path.clone());
                } else {
                    events.mentions.push(path.clone());
                }
            }
            Expr::Index { base, index } => {
                if let Expr::Path(name) = body.expr_idx(*base) {
                    self.mention(name, path);
                    self.touch(name);
                } else {
                    self.visit_expr(*base, false, path);
                }
                self.visit_expr(*index, false, path);
            }
            Expr::Field { base, .. } => {
                if let Expr::Path(name) = body.expr_idx(*base) {
                    self.mention(name, path);
                    self.touch(name);
                } else {
                    self.visit_expr(*base, false, path);
                }
            }
            _ => self.visit_expr(target, false, path),
        }
        self.visit_expr(value, true, path);
    }

    /// `Имя.Вставить("ключ"[, значение])` as a standalone statement: the only write that keeps a
    /// root tracked. Returns `false` when the statement has another form.
    fn literal_insert_statement(&mut self, expr: ExprIdx, path: &StmtPath) -> bool {
        let body = self.body;
        let Some((receiver, method, args)) = as_method_call(body, body.expr_idx(expr)) else {
            return false;
        };
        let Expr::Path(name) = body.expr(receiver) else { return false };
        if !is_insert_method(method) || !(1..=2).contains(&args.len()) {
            return false;
        }
        let Expr::Literal(Literal::String(key)) = body.expr_idx(args[0]) else { return false };
        let kind = args.get(1).and_then(|value| value_kind(body, *value));
        let write = Write { key: key.clone(), kind, path: path.clone() };
        let events = self.events(name);
        events.stats.written = true;
        events.writes.push(write);
        events.mentions.push(path.clone());
        if let Some(value) = args.get(1) {
            self.visit_expr(*value, true, path);
        }
        true
    }

    fn visit_expr(&mut self, expr: ExprIdx, escapes: bool, path: &StmtPath) {
        let body = self.body;
        let node = body.expr_idx(expr);
        if let Some((receiver, method, args)) = as_method_call(body, node) {
            self.visit_method_call(ExprId::from_idx(expr), receiver, method, args, path);
            return;
        }
        match node {
            Expr::Path(name) => {
                self.mention(name, path);
                if escapes {
                    self.touch(name);
                }
            }
            Expr::Call { callee, args } => {
                match body.expr_idx(*callee) {
                    Expr::Path(name) => {
                        if is_platform_name(name, "Вычислить", "Eval") {
                            self.opaque = true;
                        }
                    }
                    _ => self.visit_expr(*callee, false, path),
                }
                for arg in args.iter() {
                    self.visit_expr(*arg, true, path);
                }
            }
            Expr::New { args, .. } | Expr::Array(args) => {
                for arg in args.iter() {
                    self.visit_expr(*arg, true, path);
                }
            }
            Expr::Index { base, index } => {
                self.visit_expr(*base, false, path);
                self.visit_expr(*index, false, path);
            }
            Expr::Field { base, .. } => self.visit_expr(*base, false, path),
            _ => crate::narrow::for_each_expr_child(body, expr, &mut |child| {
                self.visit_expr(child, escapes, path);
            }),
        }
    }

    fn visit_method_call(
        &mut self,
        call: ExprId,
        receiver: ExprId,
        method: &Name,
        args: &[ExprIdx],
        path: &StmtPath,
    ) {
        let body = self.body;
        let preserving = is_get_method(method) || is_count_method(method);
        match body.expr(receiver) {
            Expr::Path(name) => {
                self.mention(name, path);
                if is_get_method(method) {
                    if let Some(key) = literal_key(body, args) {
                        self.reads.push(Read {
                            call,
                            name: name.as_str().fold_lower(),
                            key: key.to_owned(),
                            path: path.clone(),
                        });
                    }
                }
                if !preserving {
                    self.touch(name);
                }
            }
            _ => {
                self.visit_expr(receiver.to_idx(), false, path);
                if !preserving {
                    let mut names = Vec::new();
                    values_of(body, receiver.to_idx(), &mut names);
                    for name in names {
                        self.touch(&name);
                    }
                }
            }
        }
        for arg in args.iter() {
            self.visit_expr(*arg, true, path);
        }
    }

    fn resolve(self) -> MapKeyFacts {
        let mut facts = MapKeyFacts { opaque: self.opaque, ..MapKeyFacts::default() };
        for (name, events) in &self.names {
            facts.names.insert(name.clone(), events.stats);
        }
        if self.opaque {
            return facts;
        }
        for read in &self.reads {
            let Some(events) = self.names.get(&read.name) else { continue };
            let Some(scope) = root_scope(events) else { continue };
            if let Some(answer) = answer_read(events, scope, read) {
                facts.answers.insert(read.call, (read.name.clone(), answer));
            }
        }
        facts
    }
}

/// The constructor's list and index when the name is a trackable root.
fn root_scope(events: &NameEvents) -> Option<(u32, u32)> {
    let seed = events.seed.as_ref()?;
    if events.stats.assignments != 1 || events.stats.touched {
        return None;
    }
    let mut keys: Vec<String> = events.writes.iter().map(|w| w.key.fold_lower()).collect();
    keys.sort_unstable();
    keys.dedup();
    if keys.len() > MAX_MAP_KEYS {
        return None;
    }
    let scope = *seed.last()?;
    let in_scope = |path: &StmtPath| index_in(path, scope.0).is_some_and(|i| i > scope.1);
    events.mentions.iter().all(in_scope).then_some(scope)
}

fn answer_read(events: &NameEvents, scope: (u32, u32), read: &Read) -> Option<KeyAnswer> {
    let read_index = index_in(&read.path, scope.0)?;
    let key = read.key.fold_lower();
    let matching: Vec<&Write> =
        events.writes.iter().filter(|w| w.key.fold_lower() == key).collect();
    if matching.is_empty() {
        return None;
    }
    let mut kinds: Vec<ScalarKind> = Vec::new();
    for write in &matching {
        let kind = write.kind?;
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    let unconditional = |write: &Write| write.path.last().is_some_and(|(list, _)| *list == scope.0);
    let present = events.writes.iter().all(unconditional)
        && matching.iter().any(|write| {
            write.path.last().is_some_and(|(list, index)| *list == scope.0 && *index < read_index)
        });
    Some(KeyAnswer { kinds, present })
}

fn index_in(path: &StmtPath, list: u32) -> Option<u32> {
    path.iter().find(|(l, _)| *l == list).map(|(_, index)| *index)
}

/// Names a receiver expression may evaluate to: a path, and the branches of expressions that pass
/// a value through. The result of a call, `Новый`, an index or a field read is another value.
fn values_of(body: &Body, expr: ExprIdx, out: &mut Vec<Name>) {
    match body.expr_idx(expr) {
        Expr::Path(name) => out.push(name.clone()),
        Expr::Ternary { then_expr, else_expr, .. } => {
            values_of(body, *then_expr, out);
            values_of(body, *else_expr, out);
        }
        Expr::Call { .. }
        | Expr::MethodCall { .. }
        | Expr::New { .. }
        | Expr::Index { .. }
        | Expr::Field { .. }
        | Expr::Literal(_)
        | Expr::Array(_)
        | Expr::Missing => {}
        _ => crate::narrow::for_each_expr_child(body, expr, &mut |child| {
            values_of(body, child, out);
        }),
    }
}

/// The literal kind of an inserted value. Lowering folds a leading minus into the number literal,
/// so a negative number is the signed expression `-1` in the source, not a literal, and gets no
/// kind.
fn value_kind(body: &Body, value: ExprIdx) -> Option<ScalarKind> {
    if matches!(body.expr_idx(value), Expr::Literal(Literal::Number(n)) if n.is_sign_negative()) {
        return None;
    }
    scalar_kind_of(body, ExprId::from_idx(value))
}

fn is_empty_map_constructor(body: &Body, expr: ExprIdx) -> bool {
    matches!(
        body.expr_idx(expr),
        Expr::New { type_name: Some(name), args }
            if args.is_empty() && is_platform_name(name, "Соответствие", "Map")
    )
}

fn literal_key<'b>(body: &'b Body, args: &[ExprIdx]) -> Option<&'b str> {
    match args {
        [only] => match body.expr_idx(*only) {
            Expr::Literal(Literal::String(key)) => Some(key.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn is_get_method(name: &Name) -> bool {
    is_platform_name(name, "Получить", "Get")
}

fn is_count_method(name: &Name) -> bool {
    is_platform_name(name, "Количество", "Count")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hir_def::body::lower_method;
    use syntax::SyntaxKind;

    /// The body of the first method of `source` and its facts.
    fn facts_of(source: &str) -> (std::sync::Arc<Body>, MapKeyFacts) {
        let parse = parser::parse(source);
        let method = parse
            .syntax_node()
            .descendants()
            .find(|node| {
                matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF)
            })
            .expect("the source declares a method");
        let body = lower_method(&method, false).body;
        let facts = collect_map_key_facts(&body);
        (body, facts)
    }

    fn procedure(statements: &str) -> String {
        format!("Процедура Тест(Условие, Список)\n    Д = Новый Соответствие;\n{statements}\nКонецПроцедуры\n")
    }

    /// The answer of every literal-key read, in the order the reads were lowered.
    fn answers(statements: &str) -> Vec<Option<KeyAnswer>> {
        let (body, facts) = facts_of(&procedure(statements));
        body.exprs_iter()
            .filter(|(_, expr)| {
                as_method_call(&body, expr).is_some_and(|(_, method, args)| {
                    is_get_method(method) && literal_key(&body, args).is_some()
                })
            })
            .map(|(idx, _)| facts.answer(idx).cloned())
            .collect()
    }

    fn proven(kinds: Vec<ScalarKind>) -> Option<KeyAnswer> {
        Some(KeyAnswer { kinds, present: true })
    }

    #[test]
    fn a_null_and_an_undefined_literal_each_give_an_answer_of_their_own_kind() {
        // The result type of `Неопределено` equals the platform's, so only the answer itself
        // tells "proven" from "no answer".
        let found = answers(
            r#"
    Д.Вставить("н", Null);
    Д.Вставить("н", Неопределено);
    Д.Вставить("и", Неопределено);
    А = Д.Получить("н");
    Б = Д.Получить("и");"#,
        );
        assert_eq!(
            found,
            [
                proven(vec![ScalarKind::Null, ScalarKind::Undefined]),
                proven(vec![ScalarKind::Undefined])
            ]
        );
    }

    #[test]
    fn every_literal_kind_has_its_answer_and_equal_kinds_are_listed_once() {
        let found = answers(
            r#"
    Д.Вставить("с", "т");
    Д.Вставить("ч", 1);
    Д.Вставить("ч", 2);
    Д.Вставить("б", Истина);
    Д.Вставить("д", '20240101');
    А = Д.Получить("с");
    Б = Д.Получить("ч");
    В = Д.Получить("б");
    Г = Д.Получить("д");"#,
        );
        assert_eq!(
            found,
            [
                proven(vec![ScalarKind::String]),
                proven(vec![ScalarKind::Number]),
                proven(vec![ScalarKind::Boolean]),
                proven(vec![ScalarKind::Date]),
            ]
        );
    }

    #[test]
    fn presence_needs_an_unconditional_write_before_the_read_in_the_constructors_list() {
        let found = answers(
            r#"
    А = Д.Получить("к");
    Д.Вставить("к", 1);
    Б = Д.Получить("к");"#,
        );
        assert_eq!(
            found,
            [
                Some(KeyAnswer { kinds: vec![ScalarKind::Number], present: false }),
                proven(vec![ScalarKind::Number]),
            ]
        );
    }

    #[test]
    fn a_write_that_is_not_a_literal_leaves_its_key_without_an_answer() {
        let found = answers(
            r#"
    Д.Вставить("а", Список);
    Д.Вставить("б", 1);
    Д.Вставить("в");
    А = Д.Получить("а");
    Б = Д.Получить("б");
    В = Д.Получить("в");"#,
        );
        assert_eq!(found, [None, proven(vec![ScalarKind::Number]), None]);
    }

    #[test]
    fn the_size_limit_counts_keys_after_folding() {
        let inserts = |count: usize, extra: &str| -> String {
            let writes: String =
                (0..count).map(|index| format!("    Д.Вставить(\"к{index}\", 1);\n")).collect();
            format!("\n{writes}{extra}    А = Д.Получить(\"к0\");")
        };
        assert_eq!(answers(&inserts(64, "")), [proven(vec![ScalarKind::Number])]);
        assert_eq!(answers(&inserts(65, "")), [None], "65 distinct keys: no answer at all");
        assert_eq!(
            answers(&inserts(64, "    Д.Вставить(\"К0\", 1);\n")),
            [proven(vec![ScalarKind::Number])],
            "a case duplicate is not a 65th key"
        );
    }

    #[test]
    fn execute_goto_and_eval_make_the_body_opaque_and_leave_no_answers() {
        for statement in
            ["Выполнить(\"Х = 1\");", "Х = Вычислить(\"1\");", "Х = Eval(\"1\");", "Перейти ~М;"]
        {
            let source = procedure(&format!(
                "\n    Д.Вставить(\"к\", 1);\n    {statement}\n    А = Д.Получить(\"к\");"
            ));
            let (_, facts) = facts_of(&source);
            assert!(facts.opaque(), "{statement}");
            assert!(facts.answer_roots().is_empty(), "{statement}");
        }
        let (_, calm) =
            facts_of(&procedure("\n    Д.Вставить(\"к\", 1);\n    А = Д.Получить(\"к\");"));
        assert!(!calm.opaque());
        assert_eq!(calm.answer_roots(), ["д"]);
    }

    #[test]
    fn name_statistics_are_kept_for_names_without_a_constructor() {
        let source = "Процедура Тест(Данные, Другие, Третьи, Четвёртые)\n    \
                      Данные.Вставить(\"к\", 1);\n    Другие.Очистить();\n    \
                      Копия = Третьи;\n    Для Каждого Эл Из Четвёртые Цикл\n    КонецЦикла;\n    \
                      Х = Данные.Получить(\"к\");\nКонецПроцедуры\n";
        let (_, facts) = facts_of(source);
        let name = |text: &str| hir_def::Name::new(text);

        let written = facts.name_stats(&name("Данные"));
        assert!(written.written && !written.touched && written.assignments == 0, "{written:?}");

        let mutated = facts.name_stats(&name("Другие"));
        assert!(mutated.touched && !mutated.written, "{mutated:?}");

        let released = facts.name_stats(&name("Третьи"));
        assert!(released.touched, "an assignment's right side releases the value: {released:?}");

        let copy = facts.name_stats(&name("Копия"));
        assert_eq!((copy.assignments, copy.aliased), (1, true), "{copy:?}");

        let binding = facts.name_stats(&name("Эл"));
        assert_eq!((binding.assignments, binding.aliased), (1, true), "{binding:?}");

        assert_eq!(facts.name_stats(&name("НеЗдесь")), NameStats::default());
        assert!(facts.answer_roots().is_empty(), "no constructor, no root");
    }

    #[test]
    fn a_local_taking_its_value_from_a_call_is_not_marked_as_a_copy() {
        let (_, facts) = facts_of(
            "Процедура Тест()\n    К = Ф();\n    М = Объект.Метод();\n    Н = Другая;\nКонецПроцедуры\n",
        );
        let name = |text: &str| hir_def::Name::new(text);
        assert!(!facts.name_stats(&name("К")).aliased);
        assert!(!facts.name_stats(&name("М")).aliased);
        assert!(facts.name_stats(&name("Н")).aliased);
    }

    #[test]
    fn the_gate_asks_for_a_literal_key_read() {
        let reads = |statements: &str| {
            let (body, _) = facts_of(&procedure(statements));
            body_reads_literal_key(&body)
        };
        assert!(reads("\n    А = Д.Получить(\"к\");"));
        assert!(reads("\n    А = Д.Get(\"к\");"));
        assert!(!reads("\n    Д.Вставить(\"к\", 1);"), "a write is not a read");
        assert!(!reads("\n    А = Д.Получить(Ключ);"), "a dynamic key");
        assert!(!reads("\n    А = Д.Получить();"), "no key");
        assert!(!reads("\n    А = Д.Получить(\"к\", 1);"), "two arguments");
        assert!(!reads("\n    А = Д.Количество();"));
        assert!(!reads("\n    А = Д[\"к\"];"), "an index read");
    }
}
