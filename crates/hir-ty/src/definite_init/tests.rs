//! I15: the collector over the shared syntactic events gives the same reads,
//! same-point initializations, events and uninitialized-read findings as the
//! collector that walked the body itself, on one CFG and one passing verdict.

use std::sync::Arc;

use dataflow::definite_init::analyze_definite_init;
use hir_def::body::events::{hir_suppressed, BodyEvents};
use syntax::SyntaxKind;

use super::*;

fn body_of(source: &str) -> Arc<Body> {
    let parse = parser::parse(source);
    let method = parse
        .syntax_node()
        .descendants()
        .find(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .expect("the source declares a method");
    hir_def::body::lower_method(&method, false).body
}

/// Passing by the callee's spelling, standing in for resolution: `Знач…` takes
/// its arguments by value, `Неизв…` and every method or constructor call leave
/// the direction unknown, any other bare call takes them by reference.
fn passing_of(body: &Body, arg: ExprId) -> Option<ParamPassing> {
    let arg_idx = arg.to_idx();
    for (_, expr) in body.exprs_iter() {
        match expr {
            Expr::Call { callee, args } if args.contains(&arg_idx) => {
                let Expr::Path(name) = body.expr_idx(*callee) else { return None };
                let name = name.as_str();
                return if name.starts_with("Знач") {
                    Some(ParamPassing::ByVal)
                } else if name.starts_with("Неизв") {
                    None
                } else {
                    Some(ParamPassing::ByRef)
                };
            }
            Expr::MethodCall { args, .. } | Expr::New { args, .. } if args.contains(&arg_idx) => {
                return None
            }
            _ => {}
        }
    }
    None
}

/// Every bare name is offered as an implicit local; `Tracked` drops the
/// parameters and `Перем` names itself.
fn tracked_of(body: &Body) -> Tracked {
    let names: Vec<Name> = body
        .exprs_iter()
        .filter_map(|(_, expr)| match expr {
            Expr::Path(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    Tracked::new(body, tracked_name_sources(body, names.iter()))
}

/// Every bare name but a direct assignment target, which inference does not
/// count as a read.
fn local_reads_of(body: &Body) -> FxHashSet<ExprId> {
    let targets: FxHashSet<ExprId> = body
        .stmts_iter()
        .filter_map(|(_, stmt)| match stmt {
            Stmt::Assign { target, .. } => Some(ExprId::from_idx(*target)),
            _ => None,
        })
        .collect();
    body.exprs_iter()
        .filter(|(id, expr)| matches!(expr, Expr::Path(_)) && !targets.contains(id))
        .map(|(id, _)| id)
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct Normalized {
    candidates: Vec<(u32, String, u32, InitPoint, StmtId)>,
    during: Vec<(InitPoint, Vec<u32>)>,
    events: InitEvents,
}

fn normalize(collected: &Collected) -> Normalized {
    let mut candidates: Vec<_> = collected
        .candidates
        .iter()
        .map(|c| {
            (c.expr.into_raw().into_u32(), c.name.as_str().to_string(), c.bit, c.point, c.owner)
        })
        .collect();
    candidates.sort_by_key(|c| c.0);
    let point_key = |point: &InitPoint| match point {
        InitPoint::Stmt(stmt) => (0, stmt.into_raw().into_u32()),
        InitPoint::Header(expr) => (1, expr.into_raw().into_u32()),
    };
    let mut during: Vec<_> = collected
        .during
        .iter()
        .map(|(point, bits)| {
            let mut bits: Vec<u32> = bits.iter().copied().collect();
            bits.sort_unstable();
            (*point, bits)
        })
        .collect();
    during.sort_by_key(|(point, _)| point_key(point));
    Normalized { candidates, during, events: collected.events.clone() }
}

/// The reads the check reports, by the same rule as `check_definite_init`.
fn findings(body: &Body, cfg: &Arc<cfg::ControlFlowGraph>, collected: Collected) -> Vec<String> {
    let Collected { events, during, mut candidates } = collected;
    let Some(result) = analyze_definite_init(
        Arc::clone(cfg),
        body.clone(),
        Arc::new(events),
        dataflow::DEFAULT_MAX_ITERATIONS,
    ) else {
        return vec!["no convergence".to_string()];
    };
    candidates.sort_by_key(|candidate| candidate.expr.into_raw().into_u32());
    let mut out = Vec::new();
    for candidate in candidates {
        let unassigned = result
            .state_before(candidate.point)
            .is_some_and(|state| state.is_initialized(candidate.bit) == Some(false));
        if !unassigned
            || during.get(&candidate.point).is_some_and(|bits| bits.contains(&candidate.bit))
        {
            continue;
        }
        let Some(witness) = result.witness(candidate.point, candidate.bit) else { continue };
        let path = path_kind(body, &result, &witness, candidate.owner, candidate.bit);
        out.push(format!("{} {path:?}", candidate.name.as_str()));
    }
    out
}

struct Case {
    name: &'static str,
    source: &'static str,
    /// The reported reads, from the collector that walked the body itself.
    expected: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        name: "call, method call and constructor arguments",
        source: "Процедура Т()
    ЗначВызов(А);
    СсылВызов(Б);
    В = Б;
    НеизвВызов(Г);
    Д = Г + Е;
    Объект.Метод(Ж);
    Х = Новый Массив(З);
    Ю = Ж + З;
    СсылВызов(СсылВызов(Я), Я);
КонецПроцедуры
",
        expected: &["А OtherPath", "Е OtherPath", "Объект OtherPath"],
    },
    Case {
        name: "execute",
        source: "Процедура Т()
    Если Ф Тогда
        Выполнить(Код);
    КонецЕсли;
    Б = А;
    Выполнить(\"В = 1\");
    Г = В + А;
КонецПроцедуры
",
        expected: &["Ф OtherPath", "Код OtherPath", "А Branch"],
    },
    Case {
        name: "index target",
        source: "Процедура Т()
    М[0] = 1;
    Н = М;
    К[Инд] = 2;
    С.Поле = Н;
КонецПроцедуры
",
        expected: &["Инд OtherPath", "С OtherPath"],
    },
    Case {
        name: "loops",
        source: "Процедура Т()
    Для Сч = 1 По Н Цикл
        А = Сч;
    КонецЦикла;
    Б = А;
    Для Каждого Э Из СсылВызов(В) Цикл
        Ц = В;
    КонецЦикла;
    Пока Истина Цикл
        Г = 1;
        Прервать;
    КонецЦикла;
    Д = Г;
    Пока ЗначВызов(Р) Цикл
        Р = 1;
    КонецЦикла;
КонецПроцедуры
",
        expected: &["Н OtherPath", "А LoopNotRun", "Р OtherPath"],
    },
    Case {
        name: "parameters and Перем",
        source: "Процедура Т(Парам)
    Перем Объявл;
    А = Парам + Объявл;
    Если Условие Тогда
        Б = 1;
    ИначеЕсли СсылВызов(Б) Тогда
        Парам = Б;
    КонецЕсли;
    Объявл = Б;
КонецПроцедуры
",
        expected: &["Условие OtherPath"],
    },
    Case {
        name: "preprocessor branches",
        source: "Процедура Т()
#Если Сервер Тогда
    А = 1;
#Иначе
    СсылВызов(Б);
#КонецЕсли
    В = А + Б;
КонецПроцедуры
",
        expected: &["А OtherPath", "Б OtherPath"],
    },
];

/// `Р.Метод(А); Б = А;` built by hand: lowering spells a qualified call as a
/// call of a field, so text never yields a `MethodCall`.
fn method_call_body() -> Arc<Body> {
    let mut body = Body::new();
    let path = |body: &mut Body, name: &str| body.alloc_expr(Expr::Path(Name::new(name))).to_idx();
    let receiver = path(&mut body, "Р");
    let arg = path(&mut body, "А");
    let call = body
        .alloc_expr(Expr::MethodCall {
            receiver,
            method: Name::new("Метод"),
            args: Box::new([arg]),
        })
        .to_idx();
    let first = body.alloc_stmt(Stmt::Expr(call)).to_idx();
    let target = path(&mut body, "Б");
    let value = path(&mut body, "А");
    let second = body.alloc_stmt(Stmt::Assign { target, value }).to_idx();
    body.set_body_stmts(Box::new([first, second]));
    Arc::new(body)
}

fn check_parity(name: &str, body: &Arc<Body>, expected: &[&str]) {
    assert!(!hir_suppressed(body), "{name}: the body must be analysable");
    let tracked = tracked_of(body);
    let local_reads = local_reads_of(body);
    let passing = |arg: ExprId| passing_of(body, arg);
    let cfg = Arc::new(cfg::CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body));

    let legacy = collect_legacy(body, &tracked, &local_reads, &passing);
    let shared = collect(body, &BodyEvents::collect(body), &tracked, &local_reads, &passing);
    assert_eq!(normalize(&shared), normalize(&legacy), "{name}: collected facts");

    let legacy_findings = findings(body, &cfg, legacy);
    let shared_findings = findings(body, &cfg, shared);
    assert_eq!(shared_findings, legacy_findings, "{name}: findings");
    assert_eq!(legacy_findings, expected, "{name}: findings of the reference");
}

#[test]
fn dataflow_writes_i15_collector_parity() {
    for case in CASES {
        check_parity(case.name, &body_of(case.source), case.expected);
    }
    check_parity("method call", &method_call_body(), &["Р OtherPath"]);
}

/// The parity above holds on facts these bodies actually produce: arguments
/// not known as `Знач` initialize while their point runs, `Выполнить` sets
/// every bit, a header call initializes in its own header, loops bind, and
/// parameters and `Перем` names are never judged.
#[test]
fn dataflow_writes_i15_cases_exercise_every_event() {
    let mut during_stmt = false;
    let mut during_header = false;
    let mut execute_sets_all = false;
    let mut loop_var = false;
    for case in CASES {
        let body = body_of(case.source);
        let tracked = tracked_of(&body);
        let local_reads = local_reads_of(&body);
        let collected =
            collect(&body, &BodyEvents::collect(&body), &tracked, &local_reads, &|arg| {
                passing_of(&body, arg)
            });
        during_stmt |= collected.during.keys().any(|point| matches!(point, InitPoint::Stmt(_)));
        during_header |= !collected.events.header.is_empty();
        execute_sets_all |= body.stmts_iter().any(|(stmt, s)| {
            matches!(s, Stmt::Execute { .. })
                && collected.events.stmt.get(&stmt).map(Vec::len) == Some(tracked.width())
        });
        loop_var |= !collected.events.loop_var.is_empty();
        for candidate in &collected.candidates {
            let name = candidate.name.as_str();
            assert!(
                name != "Парам" && name != "Объявл",
                "{}: a parameter or `Перем` read is not this check's question",
                case.name
            );
        }
    }
    assert!(during_stmt && during_header && execute_sets_all && loop_var);
}
