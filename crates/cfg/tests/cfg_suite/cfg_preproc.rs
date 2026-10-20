use cfg::{CfgBuilder, ControlFlowGraph};
use expect_test::{expect, Expect};
use hir_def::{hir::PreprocIfStmt, Body, Expr, Literal, LocalRange, Stmt};
use syntax::{SyntaxKind, TextRange, TextSize};

fn snapshot(body: &Body, expect: Expect) {
    expect.assert_eq(&cfg::test_utils::format_cfg(&build(body), body));
}

fn build(body: &Body) -> ControlFlowGraph {
    CfgBuilder::new().build_graph_from_hir(body.body_stmts_typed(), body)
}

fn range(start: u32, end: u32) -> LocalRange {
    LocalRange::of_detached_node(TextRange::new(TextSize::new(start), TextSize::new(end)))
}

#[test]
fn pre_if_elsif_else_materializes_preproc_conditions() {
    let mut body = Body::default();
    let value = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let then_stmt = body.stmts_mut().alloc(Stmt::Expr(value));
    let elsif_stmt = body.stmts_mut().alloc(Stmt::Expr(value));
    let else_stmt = body.stmts_mut().alloc(Stmt::Expr(value));
    let after_preproc = body.stmts_mut().alloc(Stmt::Expr(value));
    let preproc = body.stmts_mut().alloc(Stmt::PreprocIf(Box::new(PreprocIfStmt {
        condition: hir_def::preproc_condition::PreprocCondition::Unknown,
        elsif_conditions: Box::new([hir_def::preproc_condition::PreprocCondition::Unknown]),
        condition_range: range(1, 7),
        directive_range: range(0, 13),
        full_range: range(0, 44),
        then_branch: vec![then_stmt].into(),
        elsif_branches: vec![(range(14, 20), range(13, 26), vec![elsif_stmt].into())].into(),
        else_branch: Some(vec![else_stmt].into()),
    })));
    body.set_body_stmts(vec![preproc, after_preproc].into());

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:EMPTY:0
              EXIT:EMPTY:3
              NORMAL:CALL_STMT:2:#1644ee6a
              NORMAL:CALL_STMT:2:#c4480dec
              NORMAL:CALL_STMT:3:#00a972c0
              NORMAL:CALL_STMT:3:#9e03b807
              NORMAL:PRE_IF_DIR:1
              NORMAL:PRE_IF_DIR:2
            edges:
              ENTRY:EMPTY:0 -> NORMAL:PRE_IF_DIR:1 [Unconditional]
              NORMAL:CALL_STMT:2:#1644ee6a -> NORMAL:CALL_STMT:2:#c4480dec [Unconditional]
              NORMAL:CALL_STMT:2:#c4480dec -> EXIT:EMPTY:3 [Unconditional]
              NORMAL:CALL_STMT:3:#00a972c0 -> NORMAL:CALL_STMT:2:#c4480dec [Unconditional]
              NORMAL:CALL_STMT:3:#9e03b807 -> NORMAL:CALL_STMT:2:#c4480dec [Unconditional]
              NORMAL:PRE_IF_DIR:1 -> NORMAL:CALL_STMT:2:#1644ee6a [TrueBranch]
              NORMAL:PRE_IF_DIR:1 -> NORMAL:PRE_IF_DIR:2 [FalseBranch]
              NORMAL:PRE_IF_DIR:2 -> NORMAL:CALL_STMT:3:#00a972c0 [FalseBranch]
              NORMAL:PRE_IF_DIR:2 -> NORMAL:CALL_STMT:3:#9e03b807 [TrueBranch]
        "#]],
    );
}

#[test]
fn pre_region_dir_kind_smoke_does_not_change_plain_cfg() {
    assert_eq!(SyntaxKind::PRE_REGION_DIR.to_string(), "PRE_REGION_DIR");

    let mut body = Body::default();
    let value = body.exprs_mut().alloc(Expr::Literal(Literal::Bool(true)));
    let stmt = body.stmts_mut().alloc(Stmt::Expr(value));
    body.set_body_stmts(vec![stmt].into());

    snapshot(
        &body,
        expect![[r#"
            blocks:
              ENTRY:CALL_STMT:0
              EXIT:EMPTY:1
            edges:
              ENTRY:CALL_STMT:0 -> EXIT:EMPTY:1 [Unconditional]
        "#]],
    );
}
