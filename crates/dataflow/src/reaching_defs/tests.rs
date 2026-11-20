use cfg::{
    BasicBlockVertex, CfgBuilder, CfgEdgeType, CfgVertex, ConditionalVertex, ControlFlowGraph,
};
use hir_def::hir::Literal;
use syntax::SyntaxKind;

use super::*;

fn create_def_index(defs: &[Definition]) -> Arc<DefinitionIndex> {
    let mut index = IndexBuilder::default();
    for def in defs {
        index.add(def.clone());
    }
    index.finish()
}

#[test]
fn test_definition_creation() {
    let name = Name::new("Переменная");
    let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

    let param_def = Definition::parameter(&name, binding_id);
    assert_eq!(param_def.var_name, "переменная");
    assert!(matches!(param_def.def_site, DefSite::Parameter(_)));

    let var_def = Definition::var_decl(&name, binding_id);
    assert!(matches!(var_def.def_site, DefSite::VarDecl(_)));
}

#[test]
fn test_reaching_defs_empty() {
    let def_index = create_def_index(&[]);
    let empty = ReachingDefs::new(def_index);
    assert!(empty.is_empty());
}

#[test]
fn test_reaching_defs_lattice_join() {
    let name = Name::new("x");
    let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

    let def1 = Definition::parameter(&name, binding_id);
    let def2 = Definition::var_decl(&name, binding_id);

    let def_index = create_def_index(&[def1.clone(), def2.clone()]);

    let set1 = ReachingDefs::singleton(def_index.clone(), &def1);
    let set2 = ReachingDefs::singleton(def_index.clone(), &def2);

    let joined = set1.join(&set2);
    assert_eq!(joined.len(), 2);
    assert!(joined.iter().any(|d| d == &def1));
    assert!(joined.iter().any(|d| d == &def2));
}

#[test]
fn test_reaching_defs_lattice_join_idempotent() {
    let name = Name::new("x");
    let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));
    let def = Definition::parameter(&name, binding_id);

    let def_index = create_def_index(std::slice::from_ref(&def));
    let set = ReachingDefs::singleton(def_index, &def);
    let joined = set.join(&set);
    assert_eq!(joined, set);
}

#[test]
fn test_reaching_defs_lattice_join_commutative() {
    let name = Name::new("x");
    let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

    let def1 = Definition::parameter(&name, binding_id);
    let def2 = Definition::var_decl(&name, binding_id);

    let def_index = create_def_index(&[def1.clone(), def2.clone()]);

    let set1 = ReachingDefs::singleton(def_index.clone(), &def1);
    let set2 = ReachingDefs::singleton(def_index.clone(), &def2);

    assert_eq!(set1.join(&set2), set2.join(&set1));
}

#[test]
fn test_reaching_defs_lattice_bottom_identity() {
    let name = Name::new("x");
    let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));
    let def = Definition::parameter(&name, binding_id);

    let def_index = create_def_index(std::slice::from_ref(&def));
    let set = ReachingDefs::singleton(def_index.clone(), &def);
    let bottom = ReachingDefs::new(def_index);

    assert_eq!(set.join(&bottom), set);
    assert_eq!(bottom.join(&set), set);
}

#[test]
fn test_gen_kill() {
    let name = Name::new("x");
    let binding_id1 = BindingId::from_raw(la_arena::RawIdx::from_u32(0));
    let binding_id2 = BindingId::from_raw(la_arena::RawIdx::from_u32(1));

    let def1 = Definition::parameter(&name, binding_id1);
    let def2 = Definition::var_decl(&name, binding_id2);

    let def_index = create_def_index(&[def1.clone(), def2.clone()]);
    let mut state = ReachingDefs::singleton(def_index, &def1);
    assert_eq!(state.len(), 1);

    state.gen_kill("x", &def2);
    assert_eq!(state.len(), 1);
    assert!(!state.iter().any(|d| d == &def1));
    assert!(state.iter().any(|d| d == &def2));
}

#[test]
fn test_case_insensitive() {
    let name_upper = Name::new("ПЕРЕМЕННАЯ");
    let name_lower = Name::new("переменная");
    let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

    let def1 = Definition::parameter(&name_upper, binding_id);
    let def2 = Definition::var_decl(&name_lower, binding_id);

    assert_eq!(def1.var_name, def2.var_name);

    let def_index = create_def_index(&[def1.clone(), def2.clone()]);
    let mut state = ReachingDefs::singleton(def_index, &def1);
    state.gen_kill("ПЕРЕМЕННАЯ", &def2);

    assert_eq!(state.len(), 1);
    assert!(state.iter().any(|d| d == &def2));
}

#[test]
fn test_defs_for_var() {
    let name_x = Name::new("x");
    let name_y = Name::new("y");
    let binding_id = BindingId::from_raw(la_arena::RawIdx::from_u32(0));

    let def_x = Definition::parameter(&name_x, binding_id);
    let def_y = Definition::parameter(&name_y, binding_id);

    let def_index = create_def_index(&[def_x.clone(), def_y.clone()]);
    let state = ReachingDefs::from_definitions(def_index, [def_x.clone(), def_y.clone()]);

    let x_defs: Vec<_> = state.defs_for_var("x").collect();
    assert_eq!(x_defs.len(), 1);
    assert_eq!(x_defs[0], &def_x);

    assert!(state.has_def_for_var("x"));
    assert!(state.has_def_for_var("y"));
    assert!(!state.has_def_for_var("z"));
}

/// One lowered method with its events, graph and index, read through literal
/// markers: `Если x = "м1" Тогда` is a header whose entering state is what a
/// read of `x` there sees.
struct Fixture {
    body: Arc<Body>,
    events: Arc<BodyEvents>,
    cfg: Arc<ControlFlowGraph>,
    index: Arc<DefinitionIndex>,
}

fn fixture(source: &str) -> Fixture {
    let body = lower(source);
    let cfg = Arc::new(CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), &body));
    fixture_with_cfg(body, cfg)
}

fn lower(source: &str) -> Arc<Body> {
    let parse = parser::parse(source);
    let method = parse
        .syntax_node()
        .descendants()
        .find(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .expect("the source declares a method");
    hir_def::body::lower_method(&method, false).body
}

fn fixture_with_cfg(body: Arc<Body>, cfg: Arc<ControlFlowGraph>) -> Fixture {
    let events = Arc::new(BodyEvents::collect(&body));
    let index = DefinitionIndex::from_body_with_params(&body, &events, params_of(&body, true));
    Fixture { body, events, cfg, index }
}

impl Fixture {
    fn entry(&self) -> ReachingDefs {
        entry_definitions(&self.index, &self.events, &params_of(&self.body, true))
    }

    fn solve(&self) -> ReachingDefsResult {
        self.solve_with(ExecutableReachingDefsTransfer::new(&self.body))
    }

    fn solve_with<T: Transfer<ExecutableDefs>>(&self, transfer: T) -> ReachingDefsResult {
        self.solve_from(transfer, self.entry())
    }

    fn solve_from<T: Transfer<ExecutableDefs>>(
        &self,
        transfer: T,
        entry: ReachingDefs,
    ) -> ReachingDefsResult {
        let solved =
            solve_executable(transfer, self.cfg.clone(), &self.body, entry, DEFAULT_MAX_ITERATIONS)
                .expect("solves");
        assert!(solved.converged());
        ReachingDefsResult::from_executable(&solved, self.body.clone(), self.events.clone())
    }

    fn mutant(&self, rule: Rule) -> Mutant {
        Mutant {
            rule,
            index: self.index.clone(),
            real: ExecutableReachingDefsTransfer::new(&self.body),
        }
    }

    /// The header root holding the string literal `marker`.
    fn root(&self, marker: &str) -> ExprId {
        self.events
            .stmts()
            .iter()
            .flat_map(|stmt| stmt.roots.iter())
            .filter(|root| matches!(root.point, EvalPoint::Header(_)))
            .find(|root| root.nodes.iter().any(|&node| self.is_literal(node, marker)))
            .unwrap_or_else(|| panic!("no header holds {marker:?}"))
            .root
    }

    fn is_literal(&self, expr: ExprId, text: &str) -> bool {
        matches!(self.body.expr(expr), Expr::Literal(Literal::String(s)) if s.as_str() == text)
    }

    /// What a read of `var` at the header marked `marker` sees; `None` when no
    /// executable path reaches it.
    fn read(&self, result: &ReachingDefsResult, marker: &str, var: &str) -> Option<Vec<String>> {
        result.defs_for_var_at_header(var, self.root(marker)).map(|defs| self.render(&defs))
    }

    /// The statement assigning the string literal `value`.
    fn assignment(&self, value: &str) -> StmtId {
        self.body
            .stmts_iter()
            .find(|(_, stmt)| {
                matches!(stmt, Stmt::Assign { value: v, .. }
                    if self.is_literal(ExprId::from_idx(*v), value))
            })
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("nothing assigns {value:?}"))
    }

    fn after(&self, result: &ReachingDefsResult, stmt: StmtId, var: &str) -> Option<Vec<String>> {
        result
            .defs_after_stmt(stmt)
            .map(|defs| self.render(&defs.defs_for_var(var).cloned().collect::<Vec<_>>()))
    }

    fn before(&self, result: &ReachingDefsResult, stmt: StmtId, var: &str) -> Option<Vec<String>> {
        result
            .defs_before_stmt(stmt)
            .map(|defs| self.render(&defs.defs_for_var(var).cloned().collect::<Vec<_>>()))
    }

    /// A string literal assigned renders as itself, another value as `=` and
    /// what it calls or names; a possible write as `W:` and its callee, or
    /// `Execute`.
    fn render(&self, defs: &[Definition]) -> Vec<String> {
        let mut rendered: Vec<String> = defs
            .iter()
            .map(|def| match def.def_site {
                DefSite::Assignment(stmt) => match self.body.stmt(StmtId::from_raw(stmt)) {
                    Stmt::Assign { value, .. } => match self.body.expr_idx(*value) {
                        Expr::Literal(Literal::String(s)) => s.to_string(),
                        Expr::Call { callee, .. } => format!("={}", self.name(*callee)),
                        _ => format!("={}", self.name(*value)),
                    },
                    _ => unreachable!("an assignment definition names an assignment"),
                },
                DefSite::Parameter(_) => "Param".to_string(),
                DefSite::VarDecl(_) => "Перем".to_string(),
                DefSite::ForLoop(_) | DefSite::ForEachLoop(_) => "Loop".to_string(),
                DefSite::Unknown => "Unknown".to_string(),
                DefSite::MayWrite(id) => match self.index.write(id).kind {
                    WriteEventKind::Argument { call, .. } => match self.body.expr(call) {
                        Expr::Call { callee, .. } => format!("W:{}", self.name(*callee)),
                        _ => "W:?".to_string(),
                    },
                    WriteEventKind::Execute => "Execute".to_string(),
                },
            })
            .collect();
        rendered.sort();
        rendered
    }

    /// The evaluation point of the root holding the string literal `marker`.
    fn point(&self, marker: &str) -> EvalPoint {
        self.events
            .stmts()
            .iter()
            .flat_map(|stmt| stmt.roots.iter())
            .find(|root| root.nodes.iter().any(|&node| self.is_literal(node, marker)))
            .unwrap_or_else(|| panic!("no root holds {marker:?}"))
            .point
    }

    /// What a read made by the statement marked `marker` sees: the state entering
    /// it, replayed through the statements of its block before it.
    fn read_in_stmt(
        &self,
        result: &ReachingDefsResult,
        marker: &str,
        var: &str,
    ) -> Option<Vec<String>> {
        let EvalPoint::Stmt(stmt) = self.point(marker) else {
            panic!("{marker:?} marks a header, not a statement")
        };
        result.defs_for_var_at_stmt(var, stmt).map(|defs| self.render(&defs))
    }

    /// The possible writes of `var` the evaluation at the point marked `marker`
    /// makes itself, each as `callee@position`, or `Execute`.
    fn own_writes(&self, result: &ReachingDefsResult, marker: &str, var: &str) -> Vec<String> {
        let mut rendered: Vec<String> = result
            .possible_writes_at(self.point(marker), var)
            .iter()
            .map(|def| {
                let DefSite::MayWrite(id) = def.def_site else {
                    panic!("a point's own write is a possible write")
                };
                match result.write(id).expect("a published write").kind {
                    WriteEventKind::Argument { call, position, .. } => match self.body.expr(call) {
                        Expr::Call { callee, .. } => format!("{}@{position}", self.name(*callee)),
                        _ => format!("?@{position}"),
                    },
                    WriteEventKind::Execute => "Execute".to_string(),
                }
            })
            .collect();
        rendered.sort();
        rendered
    }

    fn name(&self, expr: hir_def::hir::ExprIdx) -> String {
        match self.body.expr_idx(expr) {
            Expr::Path(name) => name.as_str().to_string(),
            other => format!("{other:?}"),
        }
    }
}

fn strings(items: &[&str]) -> Option<Vec<String>> {
    Some(items.iter().map(|item| item.to_string()).collect())
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// A forbidden implementation: the executable transfer with one rule broken.
/// Every invariant test runs at least one of them and shows its assertion
/// fails there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// An ordinary write adds its definition without removing the old ones.
    NoKill,
    /// A declaration keeps the entry `Unknown` beside itself.
    VarDeclKeepsUnknown,
    /// A possible write generates nothing.
    NoMayWrite,
    /// A possible write replaces the definitions of its name.
    MayWriteKills,
    /// An ordinary write leaves possible writes standing, as a whole-body flag would.
    MayWriteSurvives,
    /// `Выполнить` covers only names some definition gave a value before it.
    ExecuteOnlyEarlierNames,
    /// An unexecutable edge carries a reached empty state.
    UnexecutableAsEmpty,
    /// An unexecutable edge carries the state like any other edge.
    UnexecutableAsNormal,
    /// A reached state without definitions is taken for an unreached one.
    EmptyAsUnreached,
    /// Header roots are not evaluated.
    SkipHeaderRoots,
    /// An exception edge carries the normal exit.
    NormalOnlyExceptional,
    /// Only the entry of `Попытка` and `ВызватьИсключение` reach a handler.
    OnlyTryEntry,
    /// Entering `Попытка` adds every possible write of the body.
    TryVeto,
    /// The exceptional exit accumulates over every point passed.
    AccumulatedExceptional,
}

struct Mutant {
    rule: Rule,
    index: Arc<DefinitionIndex>,
    real: ExecutableReachingDefsTransfer,
}

impl Mutant {
    fn adjust(
        &self,
        point: EvalPoint,
        body: &Body,
        state: &ExecutableDefs,
        out: ExecutableDefs,
    ) -> ExecutableDefs {
        let Reach::Reached(input) = &state.normal else { return out };
        let Reach::Reached(mut normal) = out.normal.clone() else { return out };
        // A plain assignment cannot raise, so its exceptional exit is unreached. Rules that
        // change the normal exit still apply; the unreached exit stays unreached unless the
        // rule itself writes the exceptional part.
        let exceptional_unreached = matches!(out.exceptional, Reach::Unreached);
        let mut exceptional = match out.exceptional.clone() {
            Reach::Reached(defs) => defs,
            Reach::Unreached => ReachingDefs::new(self.index.clone()),
        };
        let may = self.index.point_writes(point);
        match self.rule {
            Rule::NoKill => normal.bits.union_with(&input.bits),
            Rule::VarDeclKeepsUnknown => {
                if let EvalPoint::Stmt(stmt) = point {
                    if matches!(body.stmt(stmt), Stmt::VarDecl { .. }) {
                        normal.bits.union_with(&input.bits);
                    }
                }
            }
            Rule::NoMayWrite => {
                for &idx in may {
                    if !input.bits.contains(idx as usize) {
                        normal.bits.set(idx as usize, false);
                        exceptional.bits.set(idx as usize, false);
                    }
                }
            }
            Rule::MayWriteKills => {
                for &idx in may {
                    normal.kill(&self.index.get_definition(idx).var_name);
                }
                for &idx in may {
                    normal.bits.insert(idx as usize);
                }
            }
            Rule::MayWriteSurvives => {
                for idx in input.bits.ones() {
                    if matches!(
                        self.index.get_definition(idx as u32).def_site,
                        DefSite::MayWrite(_)
                    ) {
                        normal.bits.insert(idx);
                    }
                }
            }
            Rule::ExecuteOnlyEarlierNames => {
                for &idx in may {
                    let def = self.index.get_definition(idx);
                    let DefSite::MayWrite(id) = def.def_site else { continue };
                    let valued = input.defs_for_var(&def.var_name).any(|earlier| {
                        !matches!(earlier.def_site, DefSite::Unknown | DefSite::MayWrite(_))
                    });
                    if self.index.write(id).kind == WriteEventKind::Execute && !valued {
                        normal.bits.set(idx as usize, false);
                        exceptional.bits.set(idx as usize, false);
                    }
                }
            }
            Rule::AccumulatedExceptional => {
                if let Reach::Reached(earlier) = &state.exceptional {
                    exceptional.bits.union_with(&earlier.bits);
                }
            }
            _ => {}
        }
        let touches_exceptional = matches!(
            self.rule,
            Rule::NoMayWrite | Rule::ExecuteOnlyEarlierNames | Rule::AccumulatedExceptional
        );
        ExecutableDefs {
            normal: Reach::Reached(normal),
            exceptional: if exceptional_unreached && !touches_exceptional {
                Reach::Unreached
            } else {
                Reach::Reached(exceptional)
            },
            skipped: out.skipped,
        }
    }
}

fn all_may_writes(index: &DefinitionIndex) -> Vec<u32> {
    (0..index.len() as u32)
        .filter(|&idx| matches!(index.get_definition(idx).def_site, DefSite::MayWrite(_)))
        .collect()
}

impl Transfer<ExecutableDefs> for Mutant {
    fn transfer_stmt(
        &self,
        stmt_id: RawIdx,
        state: &ExecutableDefs,
        body: &Body,
    ) -> ExecutableDefs {
        let out = self.real.transfer_stmt(stmt_id, state, body);
        self.adjust(EvalPoint::Stmt(StmtId::from_raw(stmt_id)), body, state, out)
    }

    fn transfer_expr(
        &self,
        expr_id: ExprId,
        state: &ExecutableDefs,
        body: &Body,
    ) -> ExecutableDefs {
        if self.rule == Rule::SkipHeaderRoots {
            return ExecutableDefs::entering(state.normal.clone());
        }
        let out = self.real.transfer_expr(expr_id, state, body);
        self.adjust(EvalPoint::Header(expr_id), body, state, out)
    }

    fn transfer_loop_var_bind(&self, loop_var: BindingId, state: &mut ExecutableDefs, body: &Body) {
        self.real.transfer_loop_var_bind(loop_var, state, body);
    }

    fn transfer_edge(&self, edge_kind: CfgEdgeType, state: &ExecutableDefs) -> ExecutableDefs {
        let real = self.real.transfer_edge(edge_kind, state);
        match (self.rule, edge_kind) {
            (Rule::UnexecutableAsEmpty, CfgEdgeType::Unexecutable) => {
                ExecutableDefs::entering(Reach::Reached(ReachingDefs::new(self.index.clone())))
            }
            (Rule::UnexecutableAsNormal, CfgEdgeType::Unexecutable) => {
                ExecutableDefs::entering(state.normal.clone())
            }
            (Rule::NormalOnlyExceptional, CfgEdgeType::Exception) => {
                ExecutableDefs::entering(state.normal.clone())
            }
            (Rule::EmptyAsUnreached, _) => match &real.normal {
                Reach::Reached(defs) if defs.is_empty() => ExecutableDefs::unreached(),
                _ => real,
            },
            (Rule::AccumulatedExceptional, kind) if kind != CfgEdgeType::Exception => {
                let mut carried = real;
                carried.exceptional.join_in_place(&state.exceptional);
                carried
            }
            _ => real,
        }
    }

    fn follows_possible_failures(&self) -> bool {
        self.rule != Rule::OnlyTryEntry
    }

    fn transfer_try_entry(&self, _try_stmt: StmtId, state: &mut ExecutableDefs, _body: &Body) {
        if self.rule != Rule::TryVeto {
            return;
        }
        if let Reach::Reached(defs) = &mut state.normal {
            for idx in all_may_writes(&self.index) {
                defs.bits.insert(idx as usize);
            }
        }
        state.exceptional = state.normal.clone();
    }
}

#[test]
fn dataflow_writes_i01_strong_write() {
    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    W(x);\n    x = \"B\";\n    Если x = \"м1\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    assert_eq!(f.read(&f.solve(), "м1", "x"), strings(&["B"]));
    assert_eq!(
        f.read(&f.solve_with(f.mutant(Rule::NoKill)), "м1", "x"),
        strings(&["A", "B", "Unknown", "W:W"])
    );

    let f = fixture(
        "Процедура Т(Усл)\n    Если Усл Тогда\n        x = \"A\";\n    КонецЕсли;\n    Если x = \"м1\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    assert_eq!(f.read(&f.solve(), "м1", "x"), strings(&["A", "Unknown"]));
    let mut params_only = ReachingDefs::new(f.index.clone());
    for (name, binding_id) in params_of(&f.body, true) {
        params_only.insert(&Definition::parameter(&name, binding_id));
    }
    assert_eq!(
        f.read(
            &f.solve_from(ExecutableReachingDefsTransfer::new(&f.body), params_only),
            "м1",
            "x",
        ),
        strings(&["A"]),
        "without the entry Unknown the one write passes for every path",
    );
    assert_eq!(f.read(&f.solve(), "м1", "усл"), strings(&["Param"]));

    let f = fixture(
        "Процедура Т()\n    Перем x;\n    Если x = \"м1\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    assert_eq!(f.read(&f.solve(), "м1", "x"), strings(&["Перем"]));
    assert_eq!(
        f.read(&f.solve_with(f.mutant(Rule::VarDeclKeepsUnknown)), "м1", "x"),
        strings(&["Unknown", "Перем"])
    );
}

#[test]
fn dataflow_writes_i02_argument_before_read() {
    let f = fixture(
        "Процедура Т(П)\n    x = \"A\";\n    W(П, x);\n    Если x = \"м1\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м1", "x"), strings(&["A", "W:W"]));

    let write = result
        .defs_for_var_at_header("x", f.root("м1"))
        .unwrap()
        .into_iter()
        .find_map(|def| match def.def_site {
            DefSite::MayWrite(id) => Some(f.index.write(id).clone()),
            _ => None,
        })
        .expect("the call may write x");
    let call_stmt = f
        .events
        .stmts()
        .iter()
        .find(|stmt| stmt.roots.iter().any(|root| !root.calls.is_empty()))
        .expect("the call statement");
    let call = &call_stmt.roots[0].calls[0];
    assert_eq!(
        write,
        WriteEvent {
            point: EvalPoint::Stmt(call_stmt.stmt),
            target: SmolStr::new("x"),
            kind: WriteEventKind::Argument {
                call: call.call,
                arg: call.path_args[1].expr,
                position: 1
            },
        }
    );

    assert_eq!(f.read(&f.solve_with(f.mutant(Rule::NoMayWrite)), "м1", "x"), strings(&["A"]));
    assert_eq!(f.read(&f.solve_with(f.mutant(Rule::MayWriteKills)), "м1", "x"), strings(&["W:W"]));

    // A read later in the same block: the statement query replays the block
    // with the solver's own transfer, not with one blind to possible writes.
    let f = fixture(
        "Процедура Т(Код)\n    x = \"A\";\n    W(x);\n    Выполнить(Код);\n    y = \"sentinel\";\nКонецПроцедуры\n",
    );
    let result = solve_executable_reaching_definitions(
        f.cfg.clone(),
        &f.body,
        f.events.clone(),
        true,
        RecoveredBody::Refuse,
    )
    .expect("published");
    let sentinel = f.assignment("sentinel");
    assert_eq!(
        Some(f.render(&result.defs_for_var_at_stmt("x", sentinel).expect("reached"))),
        strings(&["A", "Execute", "W:W"])
    );
    let block = result.stmt_to_block[&sentinel];
    let mut legacy = result.block_in[&block].clone();
    for &stmt in result.block_stmts[&block].iter().take_while(|&&stmt| stmt != sentinel.into_raw())
    {
        legacy = ReachingDefsTransfer.transfer_stmt(stmt, &legacy, &f.body);
    }
    assert_eq!(
        Some(f.render(&legacy.defs_for_var("x").cloned().collect::<Vec<_>>())),
        strings(&["A"]),
        "replaying with the ordinary transfer loses the possible writes",
    );
}

#[test]
fn dataflow_writes_i03_argument_after_read() {
    // In the same basic block: the statement `р1` reads `x` before the call that takes it.
    let in_block = |call: &str| {
        fixture(&format!(
            "Процедура Т()\n    x = \"A\";\n    y = \"р1\";\n{call}    z = \"р2\";\nКонецПроцедуры\n"
        ))
    };
    let (absent, late) = (in_block(""), in_block("    W(x);\n"));
    let (absent_result, late_result) = (absent.solve(), late.solve());
    assert_eq!(absent.read_in_stmt(&absent_result, "р1", "x"), strings(&["A"]));
    assert_eq!(
        late.read_in_stmt(&late_result, "р1", "x"),
        absent.read_in_stmt(&absent_result, "р1", "x"),
        "a call later in the block leaves the read as it was"
    );
    // The call is what the paired read after it sees, so the equality above compares two
    // answers that could differ.
    assert_eq!(late.read_in_stmt(&late_result, "р2", "x"), strings(&["A", "W:W"]));
    assert_eq!(absent.read_in_stmt(&absent_result, "р2", "x"), strings(&["A"]));

    // In a block after the read, with no path back to it.
    let next_block = |before: &str, after: &str| {
        fixture(&format!(
            "Процедура Т(Усл)\n    x = \"A\";\n{before}    Если x = \"м1\" Тогда КонецЕсли;\n{after}КонецПроцедуры\n"
        ))
    };
    let call_in_branch = "    Если Усл Тогда\n        W(x);\n    КонецЕсли;\n";
    let (absent, late, early) = (
        next_block("", "    Если Усл Тогда\n        y = 1;\n    КонецЕсли;\n"),
        next_block("", call_in_branch),
        next_block(call_in_branch, ""),
    );
    assert_eq!(absent.read(&absent.solve(), "м1", "x"), strings(&["A"]));
    assert_eq!(late.read(&late.solve(), "м1", "x"), strings(&["A"]));
    assert_eq!(
        early.read(&early.solve(), "м1", "x"),
        strings(&["A", "W:W"]),
        "the same call before the read does reach it"
    );

    // A flag over the whole body would put the call on the read that precedes it.
    let mut whole_body = late.entry();
    for idx in all_may_writes(&late.index) {
        whole_body.bits.insert(idx as usize);
    }
    let vetoed = late.solve_from(late.mutant(Rule::MayWriteSurvives), whole_body);
    assert_eq!(late.read(&vetoed, "м1", "x"), strings(&["A", "W:W"]));
}

#[test]
fn dataflow_writes_i04_back_edge_and_restore() {
    let looped = fixture(
        "Процедура Т(Усл)\n    x = \"A\";\n    Пока Усл Цикл\n        Если x = \"м1\" Тогда КонецЕсли;\n        W(x);\n    КонецЦикла;\nКонецПроцедуры\n",
    );
    assert_eq!(looped.read(&looped.solve(), "м1", "x"), strings(&["A", "W:W"]));
    // The same text without the back-edge: an answer by textual order would
    // be the same for both.
    let straight = fixture(
        "Процедура Т(Усл)\n    x = \"A\";\n    Если Усл Тогда\n        Если x = \"м1\" Тогда КонецЕсли;\n        W(x);\n    КонецЕсли;\nКонецПроцедуры\n",
    );
    assert_eq!(straight.read(&straight.solve(), "м1", "x"), strings(&["A"]));

    let restored = fixture(
        "Процедура Т(Усл)\n    Пока Усл Цикл\n        x = \"A\";\n        Если x = \"м1\" Тогда КонецЕсли;\n        W(x);\n    КонецЦикла;\nКонецПроцедуры\n",
    );
    assert_eq!(restored.read(&restored.solve(), "м1", "x"), strings(&["A"]));
    assert_eq!(
        restored.read(&restored.solve_with(restored.mutant(Rule::MayWriteSurvives)), "м1", "x"),
        strings(&["A", "W:W"])
    );
}

#[test]
fn dataflow_writes_i06_execute_order() {
    let f = fixture(
        "Процедура Т(Код)\n    x = \"A\";\n    Выполнить(Код);\n    Если x = \"м1\" Тогда КонецЕсли;\n    Если Позднее = \"м2\" Тогда КонецЕсли;\n    x = \"B\";\n    Если x = \"м3\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м1", "x"), strings(&["A", "Execute"]));
    assert_eq!(f.read(&result, "м2", "позднее"), strings(&["Execute", "Unknown"]));
    assert_eq!(f.read(&result, "м3", "x"), strings(&["B"]));
    assert_eq!(f.read(&f.solve_with(f.mutant(Rule::NoMayWrite)), "м1", "x"), strings(&["A"]));
    assert_eq!(
        f.read(&f.solve_with(f.mutant(Rule::ExecuteOnlyEarlierNames)), "м2", "позднее"),
        strings(&["Unknown"])
    );

    // After the read, and the read of its own argument: neither sees the
    // effect, which a flag over the whole body would put everywhere.
    let late = fixture(
        "Процедура Т(Усл)\n    x = \"A\";\n    Если x = \"м1\" Тогда КонецЕсли;\n    Выполнить(x);\nКонецПроцедуры\n",
    );
    let execute = late
        .body
        .stmts_iter()
        .find(|(_, stmt)| matches!(stmt, Stmt::Execute { .. }))
        .map(|(id, _)| id)
        .expect("the execute statement");
    let result = late.solve();
    assert_eq!(late.read(&result, "м1", "x"), strings(&["A"]));
    assert_eq!(late.before(&result, execute, "x"), strings(&["A"]));
    let mut whole_body = late.entry();
    for idx in all_may_writes(&late.index) {
        whole_body.bits.insert(idx as usize);
    }
    let vetoed = late.solve_from(late.mutant(Rule::MayWriteSurvives), whole_body);
    assert_eq!(late.read(&vetoed, "м1", "x"), strings(&["A", "Execute"]));
    assert_eq!(late.before(&vetoed, execute, "x"), strings(&["A", "Execute"]));
}

#[test]
fn dataflow_writes_i07_unreached_does_not_generate() {
    // The dead write after `Возврат` closes the loop body, and the builder
    // takes the back-edge from it as executable.
    let f = fixture(
        "Процедура Т(Усл)\n    Пока Усл Цикл\n        Если x = \"м1\" Тогда КонецЕсли;\n        Если Усл Тогда\n            Прервать;\n        КонецЕсли;\n        Если Усл Тогда\n            ВызватьИсключение;\n        КонецЕсли;\n        Если Усл Тогда\n            Продолжить;\n        КонецЕсли;\n        Возврат;\n        Если x = \"м2\" Тогда КонецЕсли;\n        x = \"D\";\n    КонецЦикла;\n    Если x = \"м3\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м1", "x"), strings(&["Unknown"]));
    assert_eq!(f.read(&result, "м2", "x"), None, "a dead read is unreached");
    assert_eq!(f.read(&result, "м3", "x"), strings(&["Unknown"]));
    let dead = f.assignment("D");
    assert_eq!(f.before(&result, dead, "x"), None);
    for rule in [Rule::UnexecutableAsEmpty, Rule::UnexecutableAsNormal] {
        assert_eq!(f.read(&f.solve_with(f.mutant(rule)), "м1", "x"), strings(&["D", "Unknown"]));
    }
    assert_eq!(f.read(&f.solve_with(f.mutant(Rule::UnexecutableAsEmpty)), "м2", "x"), strings(&[]));

    // entry → dead over an unexecutable edge, dead → live and entry → live
    // over executable ones, with nothing defined on entry.
    let body = lower(
        "Процедура Т()\n    x = \"D\";\n    Если x = \"м1\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    let (assign, condition) = {
        let mut stmts = body.body_stmts();
        let assign = stmts.next().unwrap();
        let Stmt::If(if_stmt) = body.stmt(stmts.next().unwrap()) else { panic!("an if") };
        (assign, ExprId::from_idx(if_stmt.condition))
    };
    let mut cfg = ControlFlowGraph::new();
    let start = cfg.add_vertex(CfgVertex::BasicBlock(BasicBlockVertex::new()));
    let mut dead_block = BasicBlockVertex::new();
    dead_block.add_statement(assign);
    let dead = cfg.add_vertex(CfgVertex::BasicBlock(dead_block));
    let live = cfg.add_vertex(CfgVertex::Conditional(ConditionalVertex::new(condition)));
    let exit = cfg.exit();
    cfg.set_start(start);
    cfg.add_edge(start, dead, CfgEdgeType::Unexecutable);
    cfg.add_edge(dead, live, CfgEdgeType::Unconditional);
    cfg.add_edge(start, live, CfgEdgeType::Unconditional);
    cfg.add_edge(live, exit, CfgEdgeType::TrueBranch);
    cfg.add_edge(live, exit, CfgEdgeType::FalseBranch);
    let f = fixture_with_cfg(body, Arc::new(cfg));
    let empty = || ReachingDefs::new(f.index.clone());

    let result = f.solve_from(ExecutableReachingDefsTransfer::new(&f.body), empty());
    assert_eq!(f.read(&result, "м1", "x"), strings(&[]), "the live entry is reached, though empty");
    assert!(!result.block_in.contains_key(&dead), "the dead block is unreached");
    assert_eq!(
        f.read(&f.solve_from(f.mutant(Rule::UnexecutableAsEmpty), empty()), "м1", "x"),
        strings(&["D"])
    );
    assert_eq!(
        f.read(&f.solve_from(f.mutant(Rule::UnexecutableAsNormal), empty()), "м1", "x"),
        strings(&["D"])
    );
    assert_eq!(f.read(&f.solve_from(f.mutant(Rule::EmptyAsUnreached), empty()), "м1", "x"), None);
}

#[test]
fn dataflow_writes_i08_header_identity() {
    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Если F(x, \"м0\") Тогда\n    ИначеЕсли x = \"м1\" Тогда\n    КонецЕсли;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м0", "x"), strings(&["A"]));
    assert_eq!(
        f.read(&result, "м1", "x"),
        strings(&["A", "W:F"]),
        "the first condition's write reaches ИначеЕсли"
    );

    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Пока F(x, \"м0\") Цикл\n    КонецЦикла;\nКонецПроцедуры\n",
    );
    assert_eq!(
        f.read(&f.solve(), "м0", "x"),
        strings(&["A", "W:F"]),
        "the condition's write comes round"
    );
    assert_eq!(f.read(&f.solve_with(f.mutant(Rule::SkipHeaderRoots)), "м0", "x"), strings(&["A"]));

    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Для Каждого Э Из G(x, \"м0\") Цикл\n        Если x = \"м1\" Тогда КонецЕсли;\n        x = \"B\";\n    КонецЦикла;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(
        f.read(&result, "м0", "x"),
        strings(&["A"]),
        "the collection is read once, before the loop"
    );
    assert_eq!(f.read(&result, "м1", "x"), strings(&["A", "B", "W:G"]));
    assert_eq!(f.read(&result, "м1", "э"), strings(&["Loop"]));
    assert_eq!(
        f.after(&result, f.assignment("B"), "x"),
        strings(&["B"]),
        "the back-edge keeps the restored value"
    );
    assert_eq!(
        f.read(&f.solve_with(f.mutant(Rule::SkipHeaderRoots)), "м1", "x"),
        strings(&["A", "B"])
    );

    let f = fixture(
        "Процедура Т(Усл)\n    x = \"A\";\n    Для Сч = 1 По F(x, \"м0\") Цикл\n        Если x = \"м1\" Тогда КонецЕсли;\n        x = \"B\";\n        Если Усл Тогда\n            Продолжить;\n        КонецЕсли;\n        x = \"C\";\n    КонецЦикла;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м0", "x"), strings(&["A"]));
    assert_eq!(f.read(&result, "м1", "x"), strings(&["A", "B", "C", "W:F"]));
    assert_eq!(f.read(&result, "м1", "сч"), strings(&["Loop"]));

    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Для Сч = F(x, \"м0\") По G(x, \"м1\") Цикл\n    КонецЦикла;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м0", "x"), strings(&["A"]));
    assert_eq!(
        f.read(&result, "м1", "x"),
        strings(&["A", "W:F"]),
        "the upper bound is read after the lower bound has run"
    );

    // An empty collection leaves the loop without binding its variable; a
    // counted loop gives its counter the start value even without a pass.
    let f = fixture(
        "Процедура Т(Список)\n    x = \"A\";\n    Для Каждого x Из Список Цикл\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецЦикла;\n    Если x = \"м2\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м1", "x"), strings(&["Loop"]));
    assert_eq!(f.read(&result, "м2", "x"), strings(&["A", "Loop"]));
    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Для x = 1 По 0 Цикл\n    КонецЦикла;\n    Если x = \"м1\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    assert_eq!(f.read(&f.solve(), "м1", "x"), strings(&["Loop"]));
}

#[test]
fn dataflow_writes_i09_same_point_order() {
    // What a read at a point sees is the state entering it: neither its own call's
    // writeback, nor its target, nor a nested or neighbouring call of the same root is in
    // it. Those effects are the point's own, listed apart with the call each belongs to, so a
    // reader can keep the nested and neighbouring ones and leave out only the call holding
    // the read.
    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    y = \"п1\";\n    Р = F(W(x), x, \"п2\");\n    Q = G(x, \"п3\");\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(
        f.read_in_stmt(&result, "п1", "x"),
        strings(&["A"]),
        "a later statement adds nothing"
    );
    assert_eq!(f.read_in_stmt(&result, "п2", "x"), strings(&["A"]), "nothing the point makes");
    assert_eq!(f.own_writes(&result, "п2", "x"), owned(&["F@1", "W@0"]));
    assert_eq!(f.own_writes(&result, "п3", "x"), owned(&["G@0"]));
    assert_eq!(
        f.read_in_stmt(&result, "п3", "x"),
        strings(&["A", "W:F", "W:W"]),
        "the next statement sees what the point made"
    );

    // An assignment to the name the call takes: the read still precedes the target, the
    // call's effects stay listed, and the next statement sees the target alone.
    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    x = F(W(x), x, \"п2\");\n    y = \"п3\";\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(
        f.read_in_stmt(&result, "п2", "x"),
        strings(&["A"]),
        "the target is written after the read"
    );
    assert_eq!(f.own_writes(&result, "п2", "x"), owned(&["F@1", "W@0"]));
    assert_eq!(f.read_in_stmt(&result, "п3", "x"), strings(&["=F"]));

    // Two bare uses of one name in one call are two events.
    let f = fixture("Процедура Т()\n    Р = F(x, x, \"п2\");\nКонецПроцедуры\n");
    assert_eq!(f.own_writes(&f.solve(), "п2", "x"), owned(&["F@0", "F@1"]));

    // The previous turn's effects of the same point still reach its read through the
    // back-edge, unless the name is written again first.
    let looped = |restore: &str| {
        fixture(&format!(
            "Процедура Т(Усл)\n    x = \"A\";\n    Пока Усл Цикл\n{restore}        Р = F(W(x), x, \"п2\");\n    КонецЦикла;\nКонецПроцедуры\n"
        ))
    };
    let f = looped("");
    assert_eq!(f.read_in_stmt(&f.solve(), "п2", "x"), strings(&["A", "W:F", "W:W"]));
    let f = looped("        x = \"B\";\n");
    assert_eq!(f.read_in_stmt(&f.solve(), "п2", "x"), strings(&["B"]));

    // The same holds for a condition: its own call is not in what it reads, and a call of
    // an earlier branch condition is.
    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Если F(W(x), x, \"м0\") Тогда\n    ИначеЕсли G(x, \"м1\") Тогда\n    КонецЕсли;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м0", "x"), strings(&["A"]));
    assert_eq!(f.read(&result, "м1", "x"), strings(&["A", "W:F", "W:W"]));
    assert_eq!(
        result.possible_writes_at(EvalPoint::Header(f.root("м0")), "x").len(),
        2,
        "both calls of the condition are its own"
    );
}

#[test]
fn dataflow_writes_i10_exceptional_sources() {
    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Попытка\n        x = F(x);\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\n    Если x = \"м2\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м1", "x"), strings(&["=F", "A", "W:F"]));
    assert_eq!(f.read(&result, "м2", "x"), strings(&["=F", "A", "W:F"]));
    assert_eq!(
        f.read(&f.solve_with(f.mutant(Rule::NormalOnlyExceptional)), "м1", "x"),
        strings(&["=F", "A"])
    );
    assert_eq!(f.read(&f.solve_with(f.mutant(Rule::OnlyTryEntry)), "м1", "x"), strings(&["A"]));

    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Попытка\n        Если x = \"м1\" Тогда КонецЕсли;\n        W(x);\n        Ы = 1;\n    Исключение\n        Если x = \"м2\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(
        f.read(&result, "м1", "x"),
        strings(&["A"]),
        "an early read does not see a later write"
    );
    assert_eq!(
        f.read(&result, "м2", "x"),
        strings(&["A", "W:W"]),
        "W may write and then raise; the plain assignment after it does not run"
    );
    assert_eq!(f.read(&f.solve_with(f.mutant(Rule::TryVeto)), "м1", "x"), strings(&["A", "W:W"]));
    assert_eq!(f.read(&f.solve_with(f.mutant(Rule::OnlyTryEntry)), "м2", "x"), strings(&["A"]));

    let f = fixture(
        "Процедура Т(Усл)\n    x = \"A\";\n    Попытка\n        Пока Усл Цикл\n            Возврат;\n            W(x);\n        КонецЦикла;\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    // The dead write gets no exception edge of its own, but the back-edge
    // from it enters the protected loop header, which has one.
    assert_eq!(f.read(&f.solve(), "м1", "x"), strings(&["A"]), "a dead write reaches no handler");
    assert_eq!(
        f.read(&f.solve_with(f.mutant(Rule::UnexecutableAsEmpty)), "м1", "x"),
        strings(&["A", "W:W"])
    );

    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    Попытка\n        Попытка\n            Ы = 1;\n        Исключение\n            W(x);\n        КонецПопытки;\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    assert_eq!(
        f.read(&f.solve(), "м1", "x"),
        strings(&["A", "W:W"]),
        "the inner handler is protected by the outer one"
    );

    let f = fixture(
        "Процедура Т()\n    W(x);\n    x = \"A\";\n    Попытка\n        x = \"B\";\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    assert_eq!(
        f.read(&f.solve(), "м1", "x"),
        strings(&["A"]),
        "a literal assignment cannot raise, so it does not reach the handler; the Try entry keeps its input"
    );
    assert_eq!(
        f.read(&f.solve_with(f.mutant(Rule::AccumulatedExceptional)), "м1", "x"),
        strings(&["A", "Unknown", "W:W"]),
        "accumulation drags earlier exceptional bits along the normal edge, but not the assignment itself"
    );
}

#[test]
fn dataflow_writes_i05_i11_argument_events_and_names() {
    // A possible write is made by a bare name only: a field, an index and an expression
    // pass a value, not the name.
    let f = fixture(
        "Процедура Т()\n    Р = F(x.f, x[0], x + 1, \"п1\");\n    Р = F(X, \"п2\");\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.own_writes(&result, "п1", "x"), owned(&[]));
    assert_eq!(
        f.own_writes(&result, "п2", "x"),
        owned(&["F@0"]),
        "the spelling of the name is folded"
    );
    assert_eq!(f.own_writes(&result, "п2", "X"), owned(&["F@0"]));

    // Each name of a call has its own event at its own position.
    let f = fixture("Процедура Т()\n    Р = F(x, y, \"п1\");\nКонецПроцедуры\n");
    let result = f.solve();
    assert_eq!(f.own_writes(&result, "п1", "x"), owned(&["F@0"]));
    assert_eq!(f.own_writes(&result, "п1", "y"), owned(&["F@1"]));

    // A field write does not rebind its base, and an assignment is found by any spelling.
    let f = fixture(
        "Процедура Т()\n    x = \"A\";\n    x.f = \"B\";\n    Результат = \"C\";\n    Если x = \"м1\" Тогда КонецЕсли;\n    Если x.f = \"м2\" Тогда КонецЕсли;\n    Если РЕЗУЛЬТАТ = \"м3\" Тогда КонецЕсли;\nКонецПроцедуры\n",
    );
    let result = f.solve();
    assert_eq!(f.read(&result, "м1", "x"), strings(&["A"]), "the base keeps its value");
    assert_eq!(f.read(&result, "м2", "x.f"), strings(&["B"]));
    assert_eq!(f.read(&result, "м3", "результат"), strings(&["C"]));
    assert_eq!(f.read(&result, "м3", "Результат"), strings(&["C"]));
}

#[test]
fn dataflow_writes_i12_unavailable_is_not_proof() {
    let source = "Процедура Т(Усл)\n    x = \"A\";\n    Пока Усл Цикл\n        Если x = \"м1\" Тогда КонецЕсли;\n        W(x);\n    КонецЦикла;\n    Возврат;\n    Если x = \"м2\" Тогда КонецЕсли;\nКонецПроцедуры\n";
    let f = fixture(source);
    let capped = solve_executable(
        ExecutableReachingDefsTransfer::new(&f.body),
        f.cfg.clone(),
        &f.body,
        f.entry(),
        2,
    )
    .expect("solves");
    assert!(!capped.converged());
    assert!(publish_executable(&capped, &f.body, f.events.clone()).is_none());
    let partial = ReachingDefsResult::from_executable(&capped, f.body.clone(), f.events.clone());
    assert_ne!(
        f.read(&partial, "м1", "x"),
        strings(&["A", "W:W"]),
        "a partial state would vouch for less"
    );

    let published = solve_executable_reaching_definitions(
        f.cfg.clone(),
        &f.body,
        f.events.clone(),
        true,
        RecoveredBody::Refuse,
    )
    .expect("a complete converging body is published");
    assert_eq!(f.read(&published, "м1", "x"), strings(&["A", "W:W"]));
    assert_eq!(f.read(&published, "м2", "x"), None, "an unreachable read is unreached, not empty");
    let not_a_header = f.assignment("A");
    let Stmt::Assign { value, .. } = f.body.stmt(not_a_header) else { unreachable!() };
    assert!(published.defs_for_var_at_header("x", ExprId::from_idx(*value)).is_none());

    let f = fixture("Процедура Т(Усл)\n    Если Усл Тогда\n        x = \"A\";\n    КонецЕсли;\n    Если x = \"м1\" Тогда КонецЕсли;\nКонецПроцедуры\n");
    let published = solve_executable_reaching_definitions(
        f.cfg.clone(),
        &f.body,
        f.events.clone(),
        true,
        RecoveredBody::Refuse,
    )
    .unwrap();
    assert_eq!(
        f.read(&published, "м1", "x"),
        strings(&["A", "Unknown"]),
        "the path without a write stays unknown"
    );

    for (source, why) in [
        (
            "Процедура Т()\n    x = \"A\";\n    Если x = \"м1\" Тогда КонецЕсли;\n    Перейти ~Нет;\nКонецПроцедуры\n",
            "an unresolved jump",
        ),
        (
            "Процедура Т()\n    x = \"A\";\n    Если x = \"м1\" Тогда КонецЕсли;\n    x = ;\nКонецПроцедуры\n",
            "an incomplete lowering",
        ),
        (
            "Процедура Т()\n    x = \"A\";\n    Если x = \"м1\" Тогда КонецЕсли;\n    x.W(;\nКонецПроцедуры\n",
            "a recovered call",
        ),
    ] {
        let f = fixture(source);
        assert!(hir_suppressed(&f.body), "{why}");
        let solve = |recovered| {
            solve_executable_reaching_definitions(
                f.cfg.clone(),
                &f.body,
                f.events.clone(),
                true,
                recovered,
            )
        };
        assert!(solve(RecoveredBody::Refuse).is_none(), "{why} publishes nothing");
        let kept = solve(RecoveredBody::Solve).unwrap_or_else(|| panic!("{why}: solved on request"));
        assert_eq!(
            f.read(&kept, "м1", "x"),
            strings(&["A"]),
            "{why}: the statements the lowering kept are solved"
        );
        assert!(
            solve_executable(
                ExecutableReachingDefsTransfer::new(&f.body),
                f.cfg.clone(),
                &f.body,
                f.entry(),
                DEFAULT_MAX_ITERATIONS
            )
            .is_some_and(|solved| solved.converged()),
            "{why}: only the gate keeps the solution back",
        );
    }
}

/// A method of nested loops, each holding branches that write a ring of locals.
fn nested_loops(depth: usize, width: usize) -> String {
    let mut source = String::from("Процедура Т(Усл)\n");
    for level in 0..depth {
        source.push_str(&format!("    Для Каждого Э{level} Из Усл Цикл\n"));
        for at in 0..width {
            let next = (at + 1) % width;
            source.push_str(&format!(
                "        Если Усл Тогда\n            x{at} = x{next} + {level};\n        КонецЕсли;\n"
            ));
        }
    }
    source.push_str(&"    КонецЦикла;\n".repeat(depth));
    source.push_str("КонецПроцедуры\n");
    source
}

#[test]
fn dataflow_writes_i12_long_method_converges() {
    let f = fixture(&nested_loops(10, 60));
    let fixed_bound = solve_executable(
        ExecutableReachingDefsTransfer::new(&f.body),
        f.cfg.clone(),
        &f.body,
        f.entry(),
        DEFAULT_MAX_ITERATIONS,
    )
    .expect("solves");
    assert!(!fixed_bound.converged(), "the method needs more visits than one bound for every body");

    let published = solve_executable_reaching_definitions(
        f.cfg.clone(),
        &f.body,
        f.events.clone(),
        true,
        RecoveredBody::Refuse,
    );
    let published = published.expect("a long method that converges is solved");
    // The budget decides only whether the fixed point is reached, not which one it is.
    let unbounded = solve_executable(
        ExecutableReachingDefsTransfer::new(&f.body),
        f.cfg.clone(),
        &f.body,
        f.entry(),
        usize::MAX,
    )
    .expect("solves");
    assert!(unbounded.converged());
    assert_eq!(
        *published,
        ReachingDefsResult::from_executable(&unbounded, f.body.clone(), f.events.clone())
    );
}

/// A write followed only by assignments that cannot raise is not on any path into the handler.
/// A by-reference write followed by a call is: the call can still raise after the write.
#[test]
fn a_write_followed_only_by_non_raising_assignments_does_not_reach_the_handler() {
    let safe = fixture(
        "Процедура Т()\n    Попытка\n        x = F(x);\n        x = \"B\";\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    assert_eq!(
        safe.read(&safe.solve(), "м1", "x"),
        strings(&["Unknown", "W:F"]),
        "the ordinary assignment does not reach; the call may have written its argument and then raised"
    );

    let raising = fixture(
        "Процедура Т()\n    Попытка\n        x = F(x);\n        Ф();\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    assert_eq!(raising.read(&raising.solve(), "м1", "x"), strings(&["=F", "Unknown", "W:F"]));

    let written = fixture(
        "Процедура Т()\n    x = \"A\";\n    Попытка\n        W(x);\n        Ф();\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    assert_eq!(written.read(&written.solve(), "м1", "x"), strings(&["A", "W:W"]));

    // A call may write its argument and then raise. The plain assignment after it does not
    // run on that path, so the handler still sees the possible write.
    let raises_after_write = fixture(
        "Процедура Т()\n    x = \"A\";\n    Попытка\n        W(x);\n        x = \"B\";\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    assert_eq!(
        raises_after_write.read(&raises_after_write.solve(), "м1", "x"),
        strings(&["A", "W:W"]),
        "the plain assignment cannot raise, so it is not what the handler sees"
    );

    // The assignment is the last statement of a branch, and the branch's statement is followed
    // only by an assignment that cannot raise. Once the call returns, nothing left can enter
    // the handler, so the ordinary write does not reach it. The call may still have written
    // its argument and then raised.
    let nested = fixture(
        "Процедура Т(Усл)\n    Попытка\n        Если Усл Тогда\n            x = F(x);\n        КонецЕсли;\n        y = \"C\";\n    Исключение\n        Если x = \"м1\" Тогда КонецЕсли;\n    КонецПопытки;\nКонецПроцедуры\n",
    );
    assert_eq!(nested.read(&nested.solve(), "м1", "x"), strings(&["Unknown", "W:F"]));
}
