//! Query facts are ordinary inference data that notification projection does not
//! consume, so per-body inference defers them. Reaching-definitions reads needed
//! by receiver inference remain available through the projection's shared budget.

use hir::{
    semantic_notify_candidates_query, semantic_notify_candidates_weaving_query,
    semantic_notify_facts_effective_query, semantic_notify_facts_weaving_with_work_limit,
    semantic_notify_facts_with_work_limit, EffectiveModuleId, NotifySignatureProtection,
    WeavingModuleId,
};
use ide_db::base_db::{FileIdInput, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

fn install_bundled_facts() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let facts =
            bsl_platform::PlatformHelp::without_io(&bsl_platform::PlatformHelpRequest::Bundled)
                .expect("bundled facts need no I/O");
        assert_eq!(
            bsl_platform::install_platform_help(facts),
            Ok(bsl_platform::InstallOutcome::Installed)
        );
    });
}

#[test]
fn effective_notify_projection_does_not_solve_query_facts() {
    install_bundled_facts();
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(
        vec![(None, main_root.clone()), (Some("X".to_string()), ext_root.clone())],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));

    db.set_file_text(main_file, "Процедура Цель() Экспорт\nКонецПроцедуры\n");
    // A static callback plus a method named `Выполнить`: the inference used to
    // judge the query and solve reaching definitions before the notify budget
    // ever saw the body.
    db.set_file_text(
        ext_file,
        "&ИзменениеИКонтроль(\"Цель\")\n\
         Процедура Расш_Цель()\n\
         #Вставка\n\
         \tОписание = Новый ОписаниеОповещения(\"Обработчик\", ЭтотОбъект);\n\
         \tЗапрос.Выполнить();\n\
         #КонецВставки\n\
         КонецПроцедуры\n\
         \n\
         Процедура Обработчик(Параметр)\n\
         КонецПроцедуры\n",
    );

    let eid = EffectiveModuleId::new(&db, main_file, ext_file);
    let (facts, solves) = dataflow::reaching_defs::with_executable_solve_count(|| {
        semantic_notify_facts_effective_query(&db, eid)
    });

    assert!(
        !facts.notify_candidates.is_empty(),
        "the fixture must retain the static notify candidate: {:#?}",
        facts.notify_candidates
    );
    assert_eq!(
        solves, 0,
        "the notify projection's inference must defer query facts instead of solving reaching definitions"
    );
}

#[test]
fn ordinary_notify_projection_does_not_solve_query_facts() {
    install_bundled_facts();
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::new();
    files.insert(file_id, VfsPath::new("/src/CommonModules/М/Ext/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(
        file_id,
        "Процедура Регистрация()\n\
         \tОписание = Новый ОписаниеОповещения(\"Обработчик\", ЭтотОбъект);\n\
         \tЗапрос.Выполнить();\n\
         КонецПроцедуры\n\
         \n\
         Процедура Обработчик(Параметр)\n\
         КонецПроцедуры\n",
    );

    let (candidates, solves) = dataflow::reaching_defs::with_executable_solve_count(|| {
        semantic_notify_candidates_query(&db, FileIdInput::new(&db, file_id))
    });
    assert!(!candidates.is_empty(), "the static notify candidate stays projected: {candidates:#?}");
    assert_eq!(
        solves, 0,
        "ordinary notify inference must not run its own reaching-definitions solve"
    );
}

#[test]
fn weaving_notify_projection_does_not_solve_query_facts_or_base_returns() {
    install_bundled_facts();
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(
        vec![(None, main_root.clone()), (Some("X".to_string()), ext_root.clone())],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );

    let base_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let base_path = main_root.join("CommonModules/В/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/В/Ext/Module.bsl");
    file_set.insert(base_file, VfsPath::new(base_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(base_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));

    // The base return query would run a full base inference, including the
    // reaching-definitions solve for its `Запрос.Выполнить()`.
    db.set_file_text(
        base_file,
        "Процедура Цель() Экспорт\n\tЗапрос.Выполнить();\nКонецПроцедуры\n",
    );
    db.set_file_text(
        ext_file,
        "&Вместо(\"Цель\")\n\
         Процедура Расш_Цель()\n\
         \tОписание = Новый ОписаниеОповещения(\"Обработчик\", ЭтотОбъект);\n\
         \tЗапрос.Выполнить();\n\
         КонецПроцедуры\n\
         \n\
         Процедура Обработчик(Параметр)\n\
         КонецПроцедуры\n",
    );

    let wid = WeavingModuleId::new(&db, ext_file, base_file);
    let (candidates, solves) = dataflow::reaching_defs::with_executable_solve_count(|| {
        semantic_notify_candidates_weaving_query(&db, wid)
    });
    assert!(!candidates.is_empty(), "the static notify candidate stays projected: {candidates:#?}");
    assert_eq!(
        solves, 0,
        "weaving notify inference and its base return must not solve reaching definitions"
    );
}

#[test]
fn ordinary_local_read_keeps_a_complete_projection_with_enough_budget() {
    install_bundled_facts();
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::new();
    files.insert(file_id, VfsPath::new("/src/CommonModules/М/Ext/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(
        file_id,
        "Процедура Регистрация()\n\
         \tПерем Локальная;\n\
         \tЛокальная = \"Обработчик\";\n\
         \tДругая = Локальная;\n\
         \tОписание = Новый ОписаниеОповещения(\"Обработчик\", ЭтотОбъект);\n\
         КонецПроцедуры\n\
         \n\
         Процедура Обработчик(Параметр)\n\
         КонецПроцедуры\n",
    );

    let (facts, solves) = dataflow::reaching_defs::with_executable_solve_count(|| {
        semantic_notify_facts_with_work_limit(&db, FileIdInput::new(&db, file_id), 2_000_000)
    });
    assert_eq!(solves, 0, "an ordinary local read must not run the executable solve");
    assert!(
        !facts.summary.notify_projection_incomplete,
        "a sufficient budget must keep the ordinary projection complete"
    );
    let callback = facts
        .notify_candidates
        .iter()
        .find(|candidate| candidate.callback_name.as_str() == "Обработчик")
        .expect("the literal callback is projected");
    assert_eq!(
        callback.signature_protection(),
        NotifySignatureProtection::Protected,
        "the local read must not turn a ThisObject callback into a foreign one"
    );
}

#[test]
fn effective_local_read_keeps_a_complete_projection_with_enough_budget() {
    install_bundled_facts();
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(
        vec![(None, main_root.clone()), (Some("X".to_string()), ext_root.clone())],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );

    let main_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
    file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(main_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));

    db.set_file_text(main_file, "Процедура Цель() Экспорт\nКонецПроцедуры\n");
    db.set_file_text(
        ext_file,
        "&ИзменениеИКонтроль(\"Цель\")\n\
         Процедура Расш_Цель()\n\
         #Вставка\n\
         \tПерем Локальная;\n\
         \tЛокальная = \"Обработчик\";\n\
         \tДругая = Локальная;\n\
         \tОписание = Новый ОписаниеОповещения(\"Обработчик\", ЭтотОбъект);\n\
         #КонецВставки\n\
         КонецПроцедуры\n\
         \n\
         Процедура Обработчик(Параметр)\n\
         КонецПроцедуры\n",
    );

    let eid = EffectiveModuleId::new(&db, main_file, ext_file);
    let facts = semantic_notify_facts_effective_query(&db, eid);
    assert!(
        !facts.summary.notify_projection_incomplete,
        "the effective local read must not turn a complete projection incomplete"
    );
    let callback = facts
        .notify_candidates
        .iter()
        .find(|candidate| candidate.callback_name.as_str() == "Обработчик")
        .expect("the literal callback is projected");
    assert_eq!(
        callback.signature_protection(),
        NotifySignatureProtection::Protected,
        "the effective local read must not turn a ThisObject callback into a foreign one"
    );
}

#[test]
fn weaving_base_returns_share_one_projection_budget() {
    install_bundled_facts();
    let temp = tempfile::tempdir().unwrap();
    let main_root = temp.path().join("src/cf");
    let ext_root = temp.path().join("src/cfe/X");
    std::fs::create_dir_all(&main_root).unwrap();
    std::fs::create_dir_all(&ext_root).unwrap();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(
        vec![(None, main_root.clone()), (Some("X".to_string()), ext_root.clone())],
        Some(bsl_metadata::ExtensionScope::InfoBase),
    );

    let base_file = FileId(0);
    let ext_file = FileId(1);
    let mut file_set = FileSet::new();
    let base_path = main_root.join("CommonModules/В/Ext/Module.bsl");
    let ext_path = ext_root.join("CommonModules/В/Ext/Module.bsl");
    file_set.insert(base_file, VfsPath::new(base_path.to_string_lossy().as_ref()));
    file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(base_file, SourceRootId(0));
    db.set_file_source_root(ext_file, SourceRootId(0));

    // Each base return needs its own deferred reaching-definitions solve; two
    // around interceptors must draw that work from one projection allowance.
    let mut base = String::new();
    for target in ["Цель1", "Цель2"] {
        base.push_str(&format!("Процедура {target}() Экспорт\n"));
        base.push_str("\tЕсли Истина Тогда\n");
        for index in 0..2_000 {
            base.push_str(&format!("\t\tA{index} = {index};\n"));
        }
        base.push_str("\tКонецЕсли;\n\tЧитать = A0;\nКонецПроцедуры\n");
    }
    db.set_file_text(base_file, &base);
    db.set_file_text(
        ext_file,
        "&Вместо(\"Цель1\")\n\
         Процедура Расш_Цель1()\n\
         \tОписание1 = Новый ОписаниеОповещения(\"Обработчик1\", ЭтотОбъект);\n\
         КонецПроцедуры\n\
         \n\
         &Вместо(\"Цель2\")\n\
         Процедура Расш_Цель2()\n\
         \tОписание2 = Новый ОписаниеОповещения(\"Обработчик2\", ЭтотОбъект);\n\
         КонецПроцедуры\n\
         \n\
         Процедура Обработчик1(Параметр)\n\
         КонецПроцедуры\n\
         \n\
         Процедура Обработчик2(Параметр)\n\
         КонецПроцедуры\n",
    );

    let wid = WeavingModuleId::new(&db, ext_file, base_file);
    let small = semantic_notify_facts_weaving_with_work_limit(&db, wid, 100);
    assert!(
        small.summary.notify_projection_incomplete,
        "two around interceptors must exhaust one small shared allowance"
    );

    let generous = semantic_notify_facts_weaving_with_work_limit(&db, wid, 2_000_000);
    assert!(
        !generous.summary.notify_projection_incomplete,
        "a sufficient shared allowance keeps the weaving projection complete"
    );
    for callback in ["Обработчик1", "Обработчик2"] {
        assert!(
            generous
                .notify_candidates
                .iter()
                .any(|candidate| candidate.callback_name.as_str() == callback),
            "the complete projection keeps {callback}: {:#?}",
            generous.notify_candidates
        );
    }
}
