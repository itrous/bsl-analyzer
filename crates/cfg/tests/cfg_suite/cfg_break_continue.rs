use cfg::{CfgBuilder, ControlFlowGraph};
use expect_test::{expect, Expect};
use hir_def::{Body, Expr, IfStmt, Literal, Stmt};

fn snapshot(body: &Body, expect: Expect) {
    expect.assert_eq(&cfg::test_utils::format_cfg(&build(body), body));
}

fn build(body: &Body) -> ControlFlowGraph {
    CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body)
}

#[test]
fn if_branches_inside_loop_model_switch_like_jumps() {
    let mut body = Body::default();
    let condition = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let break_stmt = body.stmts_mut().alloc(Stmt::Break);
    let continue_stmt = body.stmts_mut().alloc(Stmt::Continue);
    let after_if = body.stmts_mut().alloc(Stmt::Expr(condition));
    let if_stmt = body.stmts_mut().alloc(Stmt::If(Box::new(IfStmt {
        condition,
        then_branch: vec![break_stmt].into(),
        elsif_branches: vec![(condition, vec![continue_stmt].into())].into(),
        else_branch: Some(vec![after_if].into()),
    })));
    let while_stmt = body.stmts_mut().alloc(Stmt::While { condition, body: vec![if_stmt].into() });
    body.set_body_stmts(vec![while_stmt].into());

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:EMPTY:0
              EXIT:EMPTY:3
              NORMAL:BREAK_STMT:4
              NORMAL:CALL_STMT:5
              NORMAL:CONTINUE_STMT:5
              NORMAL:EMPTY:2:#45e4262f
              NORMAL:EMPTY:2:#dfef8bcc
              NORMAL:EMPTY:4
              NORMAL:EMPTY:5
              NORMAL:EMPTY:6
              NORMAL:IF_STMT:3
              NORMAL:IF_STMT:4
              NORMAL:WHILE_STMT:1
            edges:
              ENTRY:EMPTY:0 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:BREAK_STMT:4 -> NORMAL:EMPTY:2:#dfef8bcc [Unconditional]
              NORMAL:BREAK_STMT:4 -> NORMAL:EMPTY:5 [Unexecutable]
              NORMAL:CALL_STMT:5 -> NORMAL:EMPTY:4 [Unconditional]
              NORMAL:CONTINUE_STMT:5 -> NORMAL:EMPTY:6 [Unexecutable]
              NORMAL:CONTINUE_STMT:5 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:EMPTY:2:#45e4262f -> NORMAL:IF_STMT:3 [Unconditional]
              NORMAL:EMPTY:2:#dfef8bcc -> EXIT:EMPTY:3 [Unconditional]
              NORMAL:EMPTY:4 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:EMPTY:5 -> NORMAL:EMPTY:4 [Unexecutable]
              NORMAL:EMPTY:6 -> NORMAL:EMPTY:4 [Unexecutable]
              NORMAL:IF_STMT:3 -> NORMAL:BREAK_STMT:4 [TrueBranch]
              NORMAL:IF_STMT:3 -> NORMAL:IF_STMT:4 [FalseBranch]
              NORMAL:IF_STMT:4 -> NORMAL:CALL_STMT:5 [FalseBranch]
              NORMAL:IF_STMT:4 -> NORMAL:CONTINUE_STMT:5 [TrueBranch]
              NORMAL:WHILE_STMT:1 -> NORMAL:EMPTY:2:#45e4262f [TrueBranch]
              NORMAL:WHILE_STMT:1 -> NORMAL:EMPTY:2:#dfef8bcc [FalseBranch]
        "#]],
    );
}

#[test]
fn break_and_continue_in_try_body_use_enclosing_loop_targets() {
    let mut body = Body::default();
    let condition = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let break_stmt = body.stmts_mut().alloc(Stmt::Break);
    let continue_stmt = body.stmts_mut().alloc(Stmt::Continue);
    let except_stmt = body.stmts_mut().alloc(Stmt::Expr(condition));
    let try_stmt = body.stmts_mut().alloc(Stmt::Try {
        body: vec![break_stmt, continue_stmt].into(),
        except: vec![except_stmt].into(),
    });
    let while_stmt = body.stmts_mut().alloc(Stmt::While { condition, body: vec![try_stmt].into() });
    body.set_body_stmts(vec![while_stmt].into());

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:EMPTY:0
              EXIT:EMPTY:3
              NORMAL:BREAK_STMT:4
              NORMAL:CALL_STMT:4
              NORMAL:CONTINUE_STMT:5
              NORMAL:EMPTY:2:#45e4262f
              NORMAL:EMPTY:2:#dfef8bcc
              NORMAL:EMPTY:4
              NORMAL:EMPTY:6
              NORMAL:TRY_STMT:3
              NORMAL:WHILE_STMT:1
            edges:
              ENTRY:EMPTY:0 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:BREAK_STMT:4 -> NORMAL:CONTINUE_STMT:5 [Unexecutable]
              NORMAL:BREAK_STMT:4 -> NORMAL:EMPTY:2:#dfef8bcc [Unconditional]
              NORMAL:CALL_STMT:4 -> NORMAL:EMPTY:4 [Unconditional]
              NORMAL:CONTINUE_STMT:5 -> NORMAL:EMPTY:6 [Unexecutable]
              NORMAL:CONTINUE_STMT:5 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:EMPTY:2:#45e4262f -> NORMAL:TRY_STMT:3 [Unconditional]
              NORMAL:EMPTY:2:#dfef8bcc -> EXIT:EMPTY:3 [Unconditional]
              NORMAL:EMPTY:4 -> NORMAL:WHILE_STMT:1 [Unconditional]
              NORMAL:EMPTY:6 -> NORMAL:EMPTY:4 [Unexecutable]
              NORMAL:TRY_STMT:3 -> NORMAL:BREAK_STMT:4 [Unconditional]
              NORMAL:TRY_STMT:3 -> NORMAL:CALL_STMT:4 [Exception]
              NORMAL:WHILE_STMT:1 -> NORMAL:EMPTY:2:#45e4262f [TrueBranch]
              NORMAL:WHILE_STMT:1 -> NORMAL:EMPTY:2:#dfef8bcc [FalseBranch]
        "#]],
    );
}
