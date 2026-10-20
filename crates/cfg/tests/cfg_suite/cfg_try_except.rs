use cfg::{CfgBuilder, ControlFlowGraph};
use expect_test::{expect, Expect};
use hir_def::{Body, Expr, Literal, Stmt};

fn snapshot(body: &Body, expect: Expect) {
    expect.assert_eq(&cfg::test_utils::format_cfg(&build(body), body));
}

fn build(body: &Body) -> ControlFlowGraph {
    CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body)
}

#[test]
fn try_except_fallthrough_merges_try_and_except_exits() {
    let mut body = Body::default();
    let value = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let try_stmt_body = body.stmts_mut().alloc(Stmt::Expr(value));
    let except_stmt = body.stmts_mut().alloc(Stmt::Expr(value));
    let after_try = body.stmts_mut().alloc(Stmt::Expr(value));
    let try_stmt = body
        .stmts_mut()
        .alloc(Stmt::Try { body: vec![try_stmt_body].into(), except: vec![except_stmt].into() });
    body.set_body_stmts(vec![try_stmt, after_try].into());

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:EMPTY:0
              EXIT:EMPTY:3
              NORMAL:CALL_STMT:2:#1f799033
              NORMAL:CALL_STMT:2:#750007e2
              NORMAL:CALL_STMT:2:#b9d7f32e
              NORMAL:TRY_STMT:1
            edges:
              ENTRY:EMPTY:0 -> NORMAL:TRY_STMT:1 [Unconditional]
              NORMAL:CALL_STMT:2:#1f799033 -> EXIT:EMPTY:3 [Unconditional]
              NORMAL:CALL_STMT:2:#750007e2 -> NORMAL:CALL_STMT:2:#1f799033 [Unconditional]
              NORMAL:CALL_STMT:2:#b9d7f32e -> NORMAL:CALL_STMT:2:#1f799033 [Unconditional]
              NORMAL:TRY_STMT:1 -> NORMAL:CALL_STMT:2:#750007e2 [Unconditional]
              NORMAL:TRY_STMT:1 -> NORMAL:CALL_STMT:2:#b9d7f32e [Exception]
        "#]],
    );
}

#[test]
fn nested_try_routes_raise_to_nearest_except() {
    let mut body = Body::default();
    let value = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let inner_raise = body.stmts_mut().alloc(Stmt::Raise { value: None });
    let inner_dead_stmt = body.stmts_mut().alloc(Stmt::Expr(value));
    let inner_except_stmt = body.stmts_mut().alloc(Stmt::Expr(value));
    let inner_try = body.stmts_mut().alloc(Stmt::Try {
        body: vec![inner_raise, inner_dead_stmt].into(),
        except: vec![inner_except_stmt].into(),
    });
    let outer_raise = body.stmts_mut().alloc(Stmt::Raise { value: None });
    let outer_except_stmt = body.stmts_mut().alloc(Stmt::Expr(value));
    let outer_try = body.stmts_mut().alloc(Stmt::Try {
        body: vec![inner_try, outer_raise].into(),
        except: vec![outer_except_stmt].into(),
    });
    body.set_body_stmts(vec![outer_try].into());

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:EMPTY:0
              EXIT:EMPTY:3
              NORMAL:CALL_STMT:2
              NORMAL:CALL_STMT:4
              NORMAL:CALL_STMT:5
              NORMAL:EMPTY:2:#750007e2
              NORMAL:EMPTY:2:#abe4a1ce
              NORMAL:EMPTY:5
              NORMAL:RAISE_STMT:4:#0e455eb4
              NORMAL:RAISE_STMT:4:#dfa84cce
              NORMAL:TRY_STMT:1
              NORMAL:TRY_STMT:3
            edges:
              ENTRY:EMPTY:0 -> NORMAL:TRY_STMT:1 [Unconditional]
              NORMAL:CALL_STMT:2 -> NORMAL:EMPTY:2:#abe4a1ce [Unconditional]
              NORMAL:CALL_STMT:4 -> NORMAL:RAISE_STMT:4:#dfa84cce [Unconditional]
              NORMAL:CALL_STMT:5 -> NORMAL:RAISE_STMT:4:#dfa84cce [Unexecutable]
              NORMAL:EMPTY:2:#750007e2 -> NORMAL:TRY_STMT:3 [Unconditional]
              NORMAL:EMPTY:2:#abe4a1ce -> EXIT:EMPTY:3 [Unconditional]
              NORMAL:EMPTY:5 -> NORMAL:EMPTY:2:#abe4a1ce [Unexecutable]
              NORMAL:RAISE_STMT:4:#0e455eb4 -> NORMAL:CALL_STMT:4 [Exception]
              NORMAL:RAISE_STMT:4:#0e455eb4 -> NORMAL:CALL_STMT:5 [Unexecutable]
              NORMAL:RAISE_STMT:4:#dfa84cce -> NORMAL:CALL_STMT:2 [Exception]
              NORMAL:RAISE_STMT:4:#dfa84cce -> NORMAL:EMPTY:5 [Unexecutable]
              NORMAL:TRY_STMT:1 -> NORMAL:CALL_STMT:2 [Exception]
              NORMAL:TRY_STMT:1 -> NORMAL:EMPTY:2:#750007e2 [Unconditional]
              NORMAL:TRY_STMT:3 -> NORMAL:CALL_STMT:4 [Exception]
              NORMAL:TRY_STMT:3 -> NORMAL:RAISE_STMT:4:#0e455eb4 [Unconditional]
        "#]],
    );
}
