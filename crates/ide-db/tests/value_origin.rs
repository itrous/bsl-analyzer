//! End to end: BSL source -> shipped platform data -> inference -> `ValueOriginWalk`.
//!
//! Lives in `ide-db` because it is the first crate that owns a full database; `hir-ty` cannot
//! build one (the dependency points the other way). A synthetic signature would not prove the
//! chain "overlay record -> loaded facts -> `CallParam` -> the walk", so every case here
//! resolves its calls through the shipped data.

use hir::{
    DefDatabase, DefWithBodyId, Expr, ExprId, HirDatabase, IdConversion, MethodId, MethodIdInput,
    ModuleId, Name, Stmt,
};
use hir_ty::value_origin::{Unproven, ValueOrigin, ValueOriginWalk};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::TypeKind;

fn setup(source: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{source}"));
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }
    let file = fixture.files.keys().copied().next().expect("one file");
    (db, file)
}

fn method(db: &RootDatabaseImpl, file_id: FileId, name: &str) -> MethodId {
    db.symbol_tree(ModuleId::new(file_id))
        .find_method(&Name::new(name))
        .unwrap_or_else(|| panic!("method `{name}`"))
        .id
}

/// The origin of the value assigned to `Итог` by its LAST assignment in procedure `П`.
fn origin(source: &str) -> ValueOrigin {
    let (db, file_id) = setup(source);
    let mid = method(&db, file_id, "П");
    let input = MethodIdInput::new(&db, mid);
    let infer = db.infer_method_ref(input);
    let body = db.method_body_ref(input);
    let value = body
        .stmts_iter()
        .filter_map(|(_, stmt)| match stmt {
            Stmt::Assign { target, value }
                if matches!(body.expr_idx(*target), Expr::Path(n) if n.as_str() == "Итог") =>
            {
                Some(ExprId::from_idx(*value))
            }
            _ => None,
        })
        .next_back()
        .expect("an assignment to Итог");
    let mut walk = ValueOriginWalk::new(
        &db,
        file_id,
        DefWithBodyId::Method(mid.local_id),
        body,
        &infer.call_arg_bindings,
        &infer.expr_types,
        &infer.binding_types,
    );
    walk.value_origin_on_every_path(value)
}

const TEXT: ValueOrigin = ValueOrigin::TypeNameText;
const OTHER: ValueOrigin = ValueOrigin::Other;

#[test]
fn anchor_premise_inference_types_typznch_as_type_descriptor() {
    let (db, file_id) = setup("Процедура П(О)\n    Х = ТипЗнч(О);\nКонецПроцедуры\n");
    let mid = method(&db, file_id, "П");
    let input = MethodIdInput::new(&db, mid);
    let infer = db.infer_method_ref(input);
    let body = db.method_body_ref(input);
    let call = body
        .exprs_iter()
        .find_map(|(id, expr)| match expr {
            Expr::Call { callee, .. }
                if matches!(body.expr_idx(*callee), Expr::Path(n) if n.as_str() == "ТипЗнч") =>
            {
                Some(id)
            }
            _ => None,
        })
        .expect("the ТипЗнч call");
    let ty = *infer.expr_types.get(&call).expect("the call is typed");
    let kernel: &dyn TypeKernelDb = &db;
    assert!(
        matches!(kernel.lookup_type(ty), TypeKind::TypeDescriptor),
        "ТипЗнч is typed {:?}, not TypeDescriptor (the platform type Тип)",
        kernel.lookup_type(ty)
    );
}

#[test]
fn direct_call_resolves_through_the_shipped_data() {
    assert_eq!(origin("Процедура П(О)\n    Итог = Строка(ТипЗнч(О));\nКонецПроцедуры\n"), TEXT);
}

#[test]
fn through_a_local() {
    let src = "Процедура П(О)\n    Имя = Строка(ТипЗнч(О));\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), TEXT);
}

#[test]
fn parameter_assigned_in_a_branch_is_other() {
    let src = "Процедура П(О)\n    Имя = Строка(ТипЗнч(О));\n    Если О = 1 Тогда\n        Имя = О;\n    КонецЕсли;\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), OTHER);
}

#[test]
fn a_by_value_global_between_write_and_use_keeps_the_verdict() {
    // СтрДлина takes its argument by value and is not among the 23 functions the passing
    // attestation excludes (checked against `unverified_functions` of the shipped overlay).
    let src = "Процедура П(О)\n    Имя = Строка(ТипЗнч(О));\n    Длина = СтрДлина(Имя);\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), TEXT);
}

#[test]
fn a_call_proven_by_reference_is_rewritten() {
    let src = "Процедура П(О)\n    Имя = Строка(ТипЗнч(О));\n    ВвестиСтроку(Имя);\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), ValueOrigin::Unproven(Unproven::Rewritten));
}

#[test]
fn an_unattested_global_leaves_the_direction_unknown() {
    // ОткрытьФорму is one of the 23 variant-bearing functions the attestation excludes: its
    // parameters carry no passing record, so the call is neither by-value nor by-reference.
    let src = "Процедура П(О)\n    Имя = Строка(ТипЗнч(О));\n    ОткрытьФорму(Имя);\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), ValueOrigin::Unproven(Unproven::RewriteDirectionUnknown));
}

#[test]
fn an_unresolved_callee_leaves_the_direction_unknown() {
    let src = "Процедура П(О)\n    Имя = Строка(ТипЗнч(О));\n    НеизвестнаяПроцедура(Имя);\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), ValueOrigin::Unproven(Unproven::RewriteDirectionUnknown));
}

#[test]
fn an_unresolved_call_as_the_written_value_is_opaque() {
    let src = "Процедура П(О)\n    Имя = НеизвестнаяФункция(О);\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), ValueOrigin::Unproven(Unproven::OpaqueDefinition));
}

#[test]
fn execute_is_rewritten_not_a_matter_of_direction() {
    let src = "Процедура П(О)\n    Имя = Строка(ТипЗнч(О));\n    Выполнить(\"Имя = 1\");\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), ValueOrigin::Unproven(Unproven::Rewritten));
}

#[test]
fn text_transformations_keep_the_verdict() {
    assert_eq!(
        origin("Процедура П(О)\n    Итог = ВРег(Строка(ТипЗнч(О)));\nКонецПроцедуры\n"),
        TEXT
    );
}

#[test]
fn a_result_not_following_the_text_is_other() {
    assert_eq!(
        origin("Процедура П(О)\n    Итог = СтрДлина(Строка(ТипЗнч(О)));\nКонецПроцедуры\n"),
        OTHER
    );
}

#[test]
fn one_branch_a_type_name_the_other_a_literal_is_other() {
    let src = "Процедура П(О)\n    Если О = 1 Тогда\n        Имя = Строка(ТипЗнч(О));\n    Иначе\n        Имя = \"текст\";\n    КонецЕсли;\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), OTHER);
}

#[test]
fn other_wins_over_an_unproven_definition() {
    let src = "Процедура П(О)\n    Если О = 1 Тогда\n        Имя = \"текст\";\n    Иначе\n        Имя = НеизвестнаяФункция(О);\n    КонецЕсли;\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), OTHER);
}

#[test]
fn a_value_defined_through_itself_stops_the_walk() {
    let src = "Процедура П(О)\n    Имя = Строка(ТипЗнч(О));\n    Пока О = 1 Цикл\n        Имя = ВРег(Имя);\n    КонецЦикла;\n    Итог = Имя;\nКонецПроцедуры\n";
    assert_eq!(origin(src), ValueOrigin::Unproven(Unproven::WalkLimit));
}

#[test]
fn a_module_variable_is_other() {
    let src = "Перем Глобальная;\n\nПроцедура П(О)\n    Итог = Глобальная;\nКонецПроцедуры\n";
    assert_eq!(origin(src), OTHER);
}

#[test]
fn a_literal_is_other() {
    assert_eq!(origin("Процедура П(О)\n    Итог = \"текст\";\nКонецПроцедуры\n"), OTHER);
}

#[test]
fn a_type_description_is_not_a_type_name_text() {
    // `ОписаниеТипов` is a different notion from the platform type `Тип`: the anchor must not
    // swallow it.
    let src = "Процедура П(О)
    Итог = Строка(Новый ОписаниеТипов(\"Строка\"));
КонецПроцедуры
";
    assert_ne!(origin(src), TEXT);
    assert_eq!(origin(src), OTHER);
}
