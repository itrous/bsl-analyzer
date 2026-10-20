//! A caller's inference survives edits in the callee's module (github#113).
//!
//! `infer_method(A)` reads the callee module's declarations and B's return
//! type, and nothing else from that file. An edit that moves B's text without
//! changing what B declares or returns must leave A's memo untouched; an edit
//! that changes B's return type must not.

use std::sync::Arc;

use hir::{infer_method_query, DefDatabase, MethodId, MethodIdInput, ModuleId, Name};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

const CALLER: &str = "Процедура A()\n    Х = Модуль2.B(1);\nКонецПроцедуры\n";

fn callee(body: &str) -> String {
    format!("Функция B(Пар) Экспорт\n{body}КонецФункции\n")
}

struct Stand {
    db: RootDatabaseImpl,
    callee_file: FileId,
    a: MethodId,
}

impl Stand {
    fn new(callee_body: &str) -> Self {
        let fixture = Fixture::parse(&format!(
            "//- /CommonModules/Модуль2/Ext/Module.bsl\n{}\n//- /test.bsl\n{CALLER}",
            callee(callee_body)
        ));
        let mut db = RootDatabaseImpl::new();
        let mut file_set = vfs::FileSet::default();
        let mut caller_file = None;
        let mut callee_file = None;
        for (file_id, file) in &fixture.files {
            file_set.insert(*file_id, file.path.clone());
            db.set_file_text(*file_id, &file.content);
            let path = file.path.as_path().to_string_lossy();
            if path.ends_with("/test.bsl") {
                caller_file = Some(*file_id);
            } else {
                callee_file = Some(*file_id);
            }
        }
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        for file_id in fixture.files.keys() {
            db.set_file_source_root(*file_id, SourceRootId(0));
        }
        let caller_file = caller_file.expect("caller file");
        let a = db
            .symbol_tree(ModuleId::new(caller_file))
            .find_method(&Name::new("A"))
            .expect("A declared")
            .id;
        Stand { db, callee_file: callee_file.expect("callee file"), a }
    }

    fn infer_a(&self) -> Arc<hir::BodyInferenceResult> {
        infer_method_query(&self.db, MethodIdInput::new(&self.db, self.a)).clone()
    }

    fn x_type(&self, result: &hir::BodyInferenceResult) -> Option<hir::TypeId> {
        result.var_types.get("х").copied()
    }

    fn edit_callee(&mut self, callee_body: &str) {
        self.db.set_file_text(self.callee_file, &callee(callee_body));
    }
}

/// I3: a body edit in the callee's module that moves text must not re-run the
/// caller's inference — the memo is the same `Arc`, not merely an equal value.
#[test]
fn length_changing_callee_body_edit_keeps_the_caller_memo() {
    let mut stand = Stand::new("    Возврат 1;\n");
    let before = stand.infer_a();
    assert!(stand.x_type(&before).is_some(), "the call must resolve through the module");

    stand.edit_callee("    Промежуточное = Пар * 2;\n    Возврат 1;\n");
    let after = stand.infer_a();
    assert!(
        Arc::ptr_eq(&before, &after),
        "a longer callee body moved B's declaration; the caller's memo must survive"
    );
}

/// Positive control: the same stand does notice a return type change.
#[test]
fn callee_return_type_change_reruns_the_caller() {
    let mut stand = Stand::new("    Возврат 1;\n");
    let before = stand.infer_a();

    stand.edit_callee("    Возврат \"строка\";\n");
    let after = stand.infer_a();
    assert!(!Arc::ptr_eq(&before, &after), "a changed return type must re-infer the caller");
    assert_ne!(stand.x_type(&before), stand.x_type(&after), "Х must follow B's return type");
}
