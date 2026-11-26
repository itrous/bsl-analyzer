//! A semantic notify projection that exhausted its work budget must reach the
//! outgoing-call surface as an explicitly withheld answer, not as an empty
//! complete one: `Analysis::call_hierarchy_outgoing` returns `None` for the whole
//! module while sibling modules with intact projections still answer. The
//! exhausted summary comes from the real production budget on a large module,
//! and keeps a prefix of already-read registrations — the shape whose published
//! edges would be indistinguishable from a complete projection.

use hir::{DefDatabase, ModuleId};
use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use syntax::TextSize;
use vfs::{FileId, FileSet, VfsPath};

fn source(text: String) -> String {
    text
}

fn exhausting_module() -> String {
    // A real registration in its own small method, then enough dynamic
    // unknown-array constructors in a second method to exhaust even the
    // production budget's hard cap. Owners normalize in source order, so the
    // registration is read before the mass burns the budget: exactly the prefix
    // a complete projection would have published as edges.
    let mut text = String::from(
        "Процедура Начать()\n\
         Внутренний();\n\
         Хоро = Новый ОписаниеОповещения(\"Цель\", Второй);\n\
         КонецПроцедуры\n\
         Процедура Внутренний() Экспорт\n\
         КонецПроцедуры\n\
         Процедура Масса(А)\n",
    );
    for i in 0..3_000 {
        text.push_str(&format!("О{i} = Новый(\"CallbackDescription\", А);\n"));
    }
    text.push_str("КонецПроцедуры\n");
    source(text)
}

fn db_with_modules() -> (Analysis, FileId, FileId) {
    let big = exhausting_module();
    let files: [(&str, String); 3] = [
        ("/src/CommonModules/Первый/Ext/Module.bsl", big),
        (
            "/src/CommonModules/Второй/Ext/Module.bsl",
            "Процедура Цель() Экспорт\nКонецПроцедуры\n".to_owned(),
        ),
        (
            "/src/CommonModules/Контроль/Ext/Module.bsl",
            "Процедура Старт()\n\
             Помощник();\n\
             Отклик = Новый ОписаниеОповещения(\"Цель\", Второй);\n\
             КонецПроцедуры\n\n\
             Процедура Помощник()\n\
             КонецПроцедуры\n"
                .to_owned(),
        ),
    ];
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (index, (path, _)) in files.iter().enumerate() {
        file_set.insert(FileId(index as u32), VfsPath::new(*path));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (index, (_, text)) in files.iter().enumerate() {
        let file_id = FileId(index as u32);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, text.as_str());
    }
    (Analysis::from_database(db), FileId(0), FileId(2))
}

fn at(text: &str, needle: &str) -> TextSize {
    TextSize::from(text.match_indices(needle).next().expect("needle present").0 as u32)
}

#[test]
fn an_exhausted_notify_projection_withholds_the_outgoing_answer_end_to_end() {
    let (analysis, big, control) = db_with_modules();
    let db = analysis.database();

    // Control of the shape: the real budget exhausted, and the registration read
    // before the exhaustion survived as a prefix. Without the prefix the
    // withholding below would be indistinguishable from "nothing to publish".
    let summary = db.module_call_summary(ModuleId::new(big));
    assert!(
        summary.notify_projection_incomplete,
        "the fixture must exhaust the production notify budget"
    );
    assert!(
        !summary.notify_regs.is_empty(),
        "the fixture must keep a read registration prefix: regs = {}, gaps = {}",
        summary.notify_regs.len(),
        summary.notify_dynamic_signature_analysis_incomplete.len(),
    );

    // The big module's anchor is withheld: `None` is distinguishable from the
    // empty complete answer a leaf method would legitimately produce.
    let big_text = db.file_text(big);
    let offset = at(&big_text, "Начать");
    assert!(
        analysis.call_hierarchy_outgoing(big, offset.into()).is_none(),
        "an exhausted projection must withhold the outgoing answer"
    );
    // The withholding is module-level: any anchor in the module stays withheld,
    // not just the registering method.
    let offset = at(&big_text, "Внутренний");
    assert!(
        analysis.call_hierarchy_outgoing(big, offset.into()).is_none(),
        "every anchor of the incomplete module stays withheld"
    );

    // A sibling module with an intact projection answers normally, direct calls
    // and the notify reference alike.
    let control_text = db.file_text(control);
    let offset = at(&control_text, "Старт");
    let calls = analysis
        .call_hierarchy_outgoing(control, offset.into())
        .expect("an intact projection publishes the outgoing answer");
    let names: Vec<&str> = calls.iter().map(|call| call.item.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Помощник", "Цель"],
        "the direct call and the notify reference both answer: {calls:?}"
    );
}
