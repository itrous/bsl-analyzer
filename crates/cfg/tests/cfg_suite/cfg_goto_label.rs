use cfg::{CfgBuilder, ControlFlowGraph};
use expect_test::{expect, Expect};
use hir_def::{Body, Expr, Literal, Name, Stmt};

fn snapshot(body: &Body, expect: Expect) {
    expect.assert_eq(&cfg::test_utils::format_cfg(&build(body), body));
}

fn build(body: &Body) -> ControlFlowGraph {
    CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body)
}

#[test]
fn forward_and_backward_goto_edges() {
    let mut body = Body::default();
    let value = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let goto_forward = body.stmts_mut().alloc(Stmt::Goto(Name::new("Конец")));
    let middle = body.stmts_mut().alloc(Stmt::Expr(value));
    let label_start = body.stmts_mut().alloc(Stmt::Label(Name::new("Начало")));
    let goto_backward = body.stmts_mut().alloc(Stmt::Goto(Name::new("Начало")));
    let label_end = body.stmts_mut().alloc(Stmt::Label(Name::new("Конец")));
    let tail = body.stmts_mut().alloc(Stmt::Expr(value));
    body.set_body_stmts(
        vec![goto_forward, middle, label_start, goto_backward, label_end, tail].into(),
    );

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:GOTO_STMT:0
              EXIT:EMPTY:3
              NORMAL:CALL_STMT:1
              NORMAL:CALL_STMT:2
              NORMAL:EMPTY:4
              NORMAL:GOTO_STMT:3
              NORMAL:LABEL_STMT:1
              NORMAL:LABEL_STMT:2
            edges:
              ENTRY:GOTO_STMT:0 -> NORMAL:CALL_STMT:1 [Unexecutable]
              ENTRY:GOTO_STMT:0 -> NORMAL:LABEL_STMT:1 [Unconditional]
              NORMAL:CALL_STMT:1 -> NORMAL:LABEL_STMT:2 [Unconditional]
              NORMAL:CALL_STMT:2 -> EXIT:EMPTY:3 [Unconditional]
              NORMAL:EMPTY:4 -> NORMAL:LABEL_STMT:1 [Unconditional]
              NORMAL:GOTO_STMT:3 -> NORMAL:EMPTY:4 [Unexecutable]
              NORMAL:GOTO_STMT:3 -> NORMAL:LABEL_STMT:2 [Unconditional]
              NORMAL:LABEL_STMT:1 -> NORMAL:CALL_STMT:2 [Unconditional]
              NORMAL:LABEL_STMT:2 -> NORMAL:GOTO_STMT:3 [Unconditional]
        "#]],
    );
}

#[test]
fn adjacent_labels_and_multi_label_gotos() {
    let mut body = Body::default();
    let value = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let first_label = body.stmts_mut().alloc(Stmt::Label(Name::new("Первый")));
    let second_label = body.stmts_mut().alloc(Stmt::Label(Name::new("Второй")));
    let goto_second = body.stmts_mut().alloc(Stmt::Goto(Name::new("Второй")));
    let after_goto = body.stmts_mut().alloc(Stmt::Expr(value));
    let goto_first = body.stmts_mut().alloc(Stmt::Goto(Name::new("Первый")));
    body.set_body_stmts(
        vec![first_label, second_label, goto_second, after_goto, goto_first].into(),
    );

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:EMPTY:0
              EXIT:EMPTY:7
              NORMAL:EMPTY:2
              NORMAL:EMPTY:6
              NORMAL:GOTO_STMT:4
              NORMAL:GOTO_STMT:5
              NORMAL:LABEL_STMT:1
              NORMAL:LABEL_STMT:3
            edges:
              ENTRY:EMPTY:0 -> NORMAL:LABEL_STMT:1 [Unconditional]
              NORMAL:EMPTY:2 -> NORMAL:LABEL_STMT:3 [Unconditional]
              NORMAL:EMPTY:6 -> EXIT:EMPTY:7 [Unexecutable]
              NORMAL:GOTO_STMT:4 -> NORMAL:GOTO_STMT:5 [Unexecutable]
              NORMAL:GOTO_STMT:4 -> NORMAL:LABEL_STMT:3 [Unconditional]
              NORMAL:GOTO_STMT:5 -> NORMAL:EMPTY:6 [Unexecutable]
              NORMAL:GOTO_STMT:5 -> NORMAL:LABEL_STMT:1 [Unconditional]
              NORMAL:LABEL_STMT:1 -> NORMAL:EMPTY:2 [Unconditional]
              NORMAL:LABEL_STMT:3 -> NORMAL:GOTO_STMT:4 [Unconditional]
        "#]],
    );
}
