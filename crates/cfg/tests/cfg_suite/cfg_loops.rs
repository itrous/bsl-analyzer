use cfg::{CfgBuilder, ControlFlowGraph};
use expect_test::{expect, Expect};
use hir_def::{Binding, Body, Expr, Literal, Name, Stmt};

fn snapshot(body: &Body, expect: Expect) {
    expect.assert_eq(&cfg::test_utils::format_cfg(&build(body), body));
}

fn build(body: &Body) -> ControlFlowGraph {
    CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body)
}

#[test]
fn while_continue_and_break_edges() {
    let mut body = Body::default();
    let condition = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let before_continue = body.stmts_mut().alloc(Stmt::Expr(condition));
    let continue_stmt = body.stmts_mut().alloc(Stmt::Continue);
    let after_continue = body.stmts_mut().alloc(Stmt::Expr(condition));
    let break_stmt = body.stmts_mut().alloc(Stmt::Break);
    let while_stmt = body.stmts_mut().alloc(Stmt::While {
        condition,
        body: vec![before_continue, continue_stmt, after_continue, break_stmt].into(),
    });
    body.set_body_stmts(vec![while_stmt].into());

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:EMPTY:0
              EXIT:EMPTY:3
              NORMAL:BREAK_STMT:3
              NORMAL:CONTINUE_STMT:2
              NORMAL:EMPTY:2
              NORMAL:EMPTY:4
              NORMAL:WHILE_STMT:1
            edges:
              ENTRY:EMPTY:0 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:BREAK_STMT:3 -> NORMAL:EMPTY:2 [Unconditional]
              NORMAL:BREAK_STMT:3 -> NORMAL:EMPTY:4 [Unexecutable]
              NORMAL:CONTINUE_STMT:2 -> NORMAL:BREAK_STMT:3 [Unexecutable]
              NORMAL:CONTINUE_STMT:2 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:EMPTY:2 -> EXIT:EMPTY:3 [Unconditional]
              NORMAL:EMPTY:4 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:WHILE_STMT:1 -> NORMAL:CONTINUE_STMT:2 [TrueBranch]
              NORMAL:WHILE_STMT:1 -> NORMAL:EMPTY:2 [FalseBranch]
        "#]],
    );
}

#[test]
fn nested_for_and_while_back_edges() {
    let mut body = Body::default();
    let condition = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let loop_var = body.bindings_mut().alloc(Binding::var(Name::new("И")));
    let inner_continue = body.stmts_mut().alloc(Stmt::Continue);
    let inner_break = body.stmts_mut().alloc(Stmt::Break);
    let inner_while = body
        .stmts_mut()
        .alloc(Stmt::While { condition, body: vec![inner_continue, inner_break].into() });
    let outer_break = body.stmts_mut().alloc(Stmt::Break);
    let for_stmt = body.stmts_mut().alloc(Stmt::For {
        var: loop_var,
        from: condition,
        to: condition,
        body: vec![inner_while, outer_break].into(),
    });
    body.set_body_stmts(vec![for_stmt].into());

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:EMPTY:0
              EXIT:EMPTY:3
              NORMAL:BREAK_STMT:4
              NORMAL:BREAK_STMT:5
              NORMAL:CONTINUE_STMT:4
              NORMAL:EMPTY:2:#eb5a85f6
              NORMAL:EMPTY:2:#fae6e901
              NORMAL:EMPTY:5
              NORMAL:EMPTY:6
              NORMAL:FOR_STMT:1
              NORMAL:WHILE_STMT:3
            edges:
              ENTRY:EMPTY:0 -> NORMAL:FOR_STMT:1 [Unconditional]
              NORMAL:BREAK_STMT:4 -> NORMAL:EMPTY:2:#eb5a85f6 [Unconditional]
              NORMAL:BREAK_STMT:4 -> NORMAL:EMPTY:5 [Unexecutable]
              NORMAL:BREAK_STMT:5 -> NORMAL:BREAK_STMT:4 [Unconditional]
              NORMAL:BREAK_STMT:5 -> NORMAL:EMPTY:6 [Unexecutable]
              NORMAL:CONTINUE_STMT:4 -> NORMAL:BREAK_STMT:5 [Unexecutable]
              NORMAL:CONTINUE_STMT:4 -> NORMAL:WHILE_STMT:3 [Unconditional]
              NORMAL:EMPTY:2:#eb5a85f6 -> EXIT:EMPTY:3 [Unconditional]
              NORMAL:EMPTY:2:#fae6e901 -> NORMAL:WHILE_STMT:3 [Unconditional]
              NORMAL:EMPTY:5 -> NORMAL:FOR_STMT:1 [Unconditional]
              NORMAL:EMPTY:6 -> NORMAL:WHILE_STMT:3 [Unconditional]
              NORMAL:FOR_STMT:1 -> NORMAL:EMPTY:2:#eb5a85f6 [FalseBranch]
              NORMAL:FOR_STMT:1 -> NORMAL:EMPTY:2:#fae6e901 [TrueBranch]
              NORMAL:WHILE_STMT:3 -> NORMAL:BREAK_STMT:4 [FalseBranch]
              NORMAL:WHILE_STMT:3 -> NORMAL:CONTINUE_STMT:4 [TrueBranch]
        "#]],
    );
}
