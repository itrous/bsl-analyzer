use std::path::Path;

use base_db::{SourceDatabase, SourceRoot, SourceRootId};
use hir::{DefDatabase, DefWithBodyId, Expr, IdConversion, ModuleId, Name};
use vfs::{FileId, FileSet, VfsPath};

use crate::metadata::WorkspaceConfigsSnapshot;
use crate::RootDatabaseImpl;

struct Fixture {
    directory: tempfile::TempDir,
    db: RootDatabaseImpl,
    files: FileSet,
    next_file: u32,
    caller: FileId,
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

impl Fixture {
    fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = ["base", "own", "other"].map(|name| directory.path().join(name));
        for root in &paths {
            write(root, "Configuration.xml", "<Configuration/>");
        }
        let mut db = RootDatabaseImpl::new();
        let mut snapshot = WorkspaceConfigsSnapshot::from_paths(vec![
            (None, paths[0].clone()),
            (Some("Own".to_owned()), paths[1].clone()),
            (Some("Other".to_owned()), paths[2].clone()),
        ]);
        snapshot.closures = vec![vec![], vec![], vec![]];
        snapshot.topological_order = vec![0, 1, 2];
        snapshot.extension_scopes = vec![
            None,
            Some(bsl_metadata::ExtensionScope::InfoBase),
            Some(bsl_metadata::ExtensionScope::InfoBase),
        ];
        db.set_workspace_configs_snapshot(snapshot);
        let mut fixture =
            Self { directory, db, files: FileSet::new(), next_file: 0, caller: FileId(0) };
        fixture.caller = fixture.module("own", "Caller", false, source);
        fixture
    }

    fn module(&mut self, root: &str, name: &str, global: bool, source: &str) -> FileId {
        write(&self.directory.path().join(root), &format!("CommonModules/{name}.xml"), &format!(
            "<MetaDataObject xmlns=\"http://v8.1c.ru/8.3/MDClasses\"><CommonModule uuid=\"00000000-0000-0000-0000-000000000001\"><Properties><Name>{name}</Name><Server>true</Server><Global>{global}</Global></Properties></CommonModule></MetaDataObject>"
        ));
        self.body(root, &format!("CommonModules/{name}/Ext/Module.bsl"), source)
    }

    fn body(&mut self, root: &str, relative: &str, source: &str) -> FileId {
        let root = self.directory.path().join(root);
        write(&root, relative, source);
        let file = FileId(self.next_file);
        self.next_file += 1;
        self.files.insert(file, VfsPath::new(root.join(relative).to_string_lossy().as_ref()));
        self.db.set_source_root(SourceRootId(0), SourceRoot::new_local(self.files.clone()));
        self.db.set_file_source_root(file, SourceRootId(0));
        self.db.set_file_text(file, source);
        file
    }

    fn owner(&self, file: FileId, name: &str) -> DefWithBodyId {
        let interface = self.db.module_interface(ModuleId::new(file));
        DefWithBodyId::Method(interface.find_method(&Name::new(name)).unwrap().id.local_id)
    }

    fn names(&self, file: FileId, name: &str) -> Vec<String> {
        let owner = self.owner(file, name);
        let proof = hir::unshadowed_platform_global_calls(&self.db, file, owner, None, None);
        let bodies = self.db.module_bodies(ModuleId::new(file));
        let DefWithBodyId::Method(key) = owner else { unreachable!() };
        names_of(bodies.body(key).unwrap(), proof)
    }
}

fn names_of(body: &hir::Body, proof: rustc_hash::FxHashSet<hir::ExprId>) -> Vec<String> {
    let mut names: Vec<_> = proof
        .into_iter()
        .map(|callee| match body.expr(callee) {
            Expr::Path(name) => name.as_str().to_owned(),
            expression => panic!("proof for a non-bare callee: {expression:?}"),
        })
        .collect();
    names.sort();
    names
}

#[test]
fn platform_call_proof_keeps_the_separate_variable_namespace() {
    let fixture = Fixture::new("Перем XMLString;\nПроцедура Test(Строка)\nСтрСоединить = 1;\nСтрока(1);\nString(1);\nXMLString(1);\nСтрСоединить(Новый Массив);\nStrConcat(Новый Массив);\nТранзакцияАктивна();\nTransactionActive();\nКонецПроцедуры");
    let mut expected = [
        "Строка",
        "String",
        "XMLString",
        "СтрСоединить",
        "StrConcat",
        "ТранзакцияАктивна",
        "TransactionActive",
    ]
    .map(str::to_owned)
    .to_vec();
    expected.sort();
    assert_eq!(fixture.names(fixture.caller, "Test"), expected);
}

#[test]
fn platform_call_proof_rejects_module_methods_and_qualified_callees() {
    let fixture = Fixture::new("Процедура Test()\nСтрока(1);\nString(1);\nОбъект.XMLString(1);\nПользовательскийВызов();\nКонецПроцедуры\nФункция Строка(Значение)\nВозврат Значение;\nКонецФункции");
    assert_eq!(fixture.names(fixture.caller, "Test"), ["String"]);
    let bodies = fixture.db.module_bodies(ModuleId::new(fixture.caller));
    let DefWithBodyId::Method(key) = fixture.owner(fixture.caller, "Test") else { unreachable!() };
    assert!(bodies.body(key).unwrap().exprs_iter().any(|(_, expression)| matches!(expression, Expr::Call { callee, .. } if !matches!(bodies.body(key).unwrap().expr(hir::ExprId::from_idx(*callee)), Expr::Path(_)))));
}

#[test]
fn platform_call_proof_uses_visible_global_exports_and_dependency_closure() {
    let mut fixture = Fixture::new("Процедура Test()\nString(1);\nXMLString(1);\nКонецПроцедуры");
    fixture.module(
        "base",
        "BaseGlobal",
        true,
        "Функция String(Значение) Экспорт\nВозврат Значение;\nКонецФункции",
    );
    fixture.module(
        "other",
        "OtherGlobal",
        true,
        "Функция XMLString(Значение) Экспорт\nВозврат Значение;\nКонецФункции",
    );
    assert_eq!(fixture.names(fixture.caller, "Test"), ["XMLString"]);
    let mut snapshot = WorkspaceConfigsSnapshot::from_paths(vec![
        (None, fixture.directory.path().join("base")),
        (Some("Own".to_owned()), fixture.directory.path().join("own")),
        (Some("Other".to_owned()), fixture.directory.path().join("other")),
    ]);
    snapshot.closures = vec![vec![], vec![2], vec![]];
    snapshot.topological_order = vec![0, 2, 1];
    snapshot.extension_scopes = vec![
        None,
        Some(bsl_metadata::ExtensionScope::InfoBase),
        Some(bsl_metadata::ExtensionScope::InfoBase),
    ];
    fixture.db.set_workspace_configs_snapshot(snapshot);
    assert!(fixture.names(fixture.caller, "Test").is_empty());
}

#[test]
fn platform_call_proof_rejects_unread_global_and_application_surfaces() {
    for application in [false, true] {
        let mut fixture = Fixture::new("Процедура Test()\nString(1);\nКонецПроцедуры");
        let source = "Функция СвояФункция() Экспорт\nВозврат 1;\nКонецФункции";
        let file = if application {
            fixture.body("base", "Ext/ManagedApplicationModule.bsl", source)
        } else {
            fixture.module("base", "Global", true, source)
        };
        assert_eq!(fixture.names(fixture.caller, "Test"), ["String"]);
        fixture.db.set_file_unreadable(file);
        assert!(fixture.names(fixture.caller, "Test").is_empty(), "application={application}");
    }
}

#[test]
fn platform_call_proof_rejects_unread_implicit_receiver_and_boot_window() {
    let mut fixture = Fixture::new("Процедура Test()\nString(1);\nКонецПроцедуры");
    let form = fixture.body(
        "own",
        "CommonForms/Форма/Ext/Form/Module.bsl",
        "Процедура Test()\nString(1);\nКонецПроцедуры",
    );
    assert_eq!(
        fixture.db.module_metadata(ModuleId::new(form)).module_type,
        bsl_metadata::ModuleType::FormModule
    );
    assert!(fixture.names(form, "Test").is_empty());
    assert_eq!(fixture.names(fixture.caller, "Test"), ["String"]);
    fixture.db.set_workspace_load_complete(false);
    assert!(fixture.names(fixture.caller, "Test").is_empty());
}

#[test]
fn platform_call_proof_uses_effective_body_ids_and_keeps_copied_base_context() {
    let mut fixture = Fixture::new("&ИзменениеИКонтроль(\"Test\")\nФункция Changed()\n#Вставка\nString(1);\nXMLString(1);\n#КонецВставки\nВозврат 0;\nКонецФункции\nФункция XMLString(Значение)\nВозврат Значение;\nКонецФункции");
    let base = fixture.module("base", "Caller", false, "Функция Test()\nВозврат 0;\nКонецФункции\nФункция String(Значение)\nВозврат Значение;\nКонецФункции\nПроцедура Copied()\nXMLString(1);\nКонецПроцедуры");
    let route = hir::EffectiveModuleId::new(&fixture.db, base, fixture.caller);
    let changed = fixture.owner(base, "Test");
    assert!(hir::unshadowed_platform_global_calls(
        &fixture.db,
        fixture.caller,
        changed,
        Some(route),
        None
    )
    .is_empty());
    let copied = fixture.owner(base, "Copied");
    let proof = hir::unshadowed_platform_global_calls(
        &fixture.db,
        fixture.caller,
        copied,
        Some(route),
        None,
    );
    let bodies = hir::module_bodies_effective(&fixture.db, route);
    let DefWithBodyId::Method(key) = copied else { unreachable!() };
    assert_eq!(names_of(bodies.body(key).unwrap(), proof), ["XMLString"]);
}

#[test]
fn platform_call_proof_honors_weaving_base_siblings() {
    let mut fixture = Fixture::new(
        "&Вместо(\"Test\")\nПроцедура Changed()\nString(1);\nXMLString(1);\nКонецПроцедуры",
    );
    let base = fixture.module("base", "Caller", false, "Процедура Test()\nКонецПроцедуры\nФункция String(Значение)\nВозврат Значение;\nКонецФункции");
    assert_eq!(fixture.names(fixture.caller, "Changed"), ["String", "XMLString"]);
    let owner = fixture.owner(fixture.caller, "Changed");
    let foreign_route = hir::WeavingModuleId::new(&fixture.db, base, fixture.caller);
    assert!(hir::unshadowed_platform_global_calls(
        &fixture.db,
        fixture.caller,
        owner,
        None,
        Some(foreign_route),
    )
    .is_empty());
    let route = hir::WeavingModuleId::new(&fixture.db, fixture.caller, base);
    let proof = hir::unshadowed_platform_global_calls(
        &fixture.db,
        fixture.caller,
        owner,
        None,
        Some(route),
    );
    let bodies = fixture.db.module_bodies(ModuleId::new(fixture.caller));
    let DefWithBodyId::Method(key) = owner else { unreachable!() };
    assert_eq!(names_of(bodies.body(key).unwrap(), proof), ["XMLString"]);
}
