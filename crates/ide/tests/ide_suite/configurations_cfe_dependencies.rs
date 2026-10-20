//! Dependency-aware CFE visibility: a file sees the base, its extension's
//! declared transitive dependencies and its own extension — never an
//! unrelated sibling. Exercises the acceptance matrix on the
//! `cfe_dependencies` fixture: base + yaxunit + tests (dependsOn yaxunit)
//! + independent.

use hir::ConfigsDatabase;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::WorkspaceConfigsSnapshot;
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/cfe_dependencies"))
        .join(name)
}

struct Files {
    base: FileId,
    yaxunit: FileId,
    tests: FileId,
    independent: FileId,
}

fn setup() -> (RootDatabaseImpl, Files) {
    let mut db = RootDatabaseImpl::new();

    let files = Files {
        base: FileId::from_raw(1),
        yaxunit: FileId::from_raw(2),
        tests: FileId::from_raw(3),
        independent: FileId::from_raw(4),
    };
    let mut file_set = FileSet::default();
    let mut add = |file_id: FileId, root: &str, module: &str| {
        let path = fixture_root(root).join(format!("CommonModules/{module}/Ext/Module.bsl"));
        file_set.insert(file_id, VfsPath::new(path.to_string_lossy().to_string()));
    };
    add(files.base, "base", "БазовыйМодуль");
    add(files.yaxunit, "yaxunit", "МодульЮнит");
    add(files.tests, "tests_ext", "МодульТестов");
    add(files.independent, "independent", "МодульНезависимый");
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in [files.base, files.yaxunit, files.tests, files.independent] {
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, "Процедура Тест() КонецПроцедуры");
    }

    let paths = vec![
        (None, fixture_root("base")),
        (Some("yaxunit".to_string()), fixture_root("yaxunit")),
        (Some("tests".to_string()), fixture_root("tests_ext")),
        (Some("independent".to_string()), fixture_root("independent")),
    ];
    let canonical_paths =
        paths.iter().map(|(_, p)| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect();
    // `tests` (slot 2) depends on `yaxunit` (slot 1); everything else is independent.
    let snapshot = WorkspaceConfigsSnapshot {
        kinds: ide_db::metadata::RootKind::from_labels(&paths),
        extension_scopes: vec![
            None,
            Some(bsl_metadata::ExtensionScope::InfoBase),
            Some(bsl_metadata::ExtensionScope::InfoBase),
            Some(bsl_metadata::ExtensionScope::InfoBase),
        ],
        paths,
        canonical_paths,
        closures: vec![Vec::new(), Vec::new(), vec![1], Vec::new()],
        topological_order: vec![0, 1, 2, 3],
        fingerprint: None,
        source_exclusions: Default::default(),
    };
    db.set_workspace_configs_snapshot(snapshot);
    (db, files)
}

fn config_names(db: &RootDatabaseImpl, file_id: FileId) -> Vec<Option<String>> {
    db.configurations(file_id).into_iter().map(|c| c.name).collect()
}

#[test]
fn visible_configurations_follow_the_dependency_matrix() {
    let (db, files) = setup();

    assert_eq!(config_names(&db, files.base), [None], "a base file sees only the base");
    assert_eq!(
        config_names(&db, files.yaxunit),
        [None, Some("yaxunit".to_string())],
        "a dependency does not see its dependents",
    );
    assert_eq!(
        config_names(&db, files.tests),
        [None, Some("yaxunit".to_string()), Some("tests".to_string())],
        "a dependent sees base, its declared dependency, then itself",
    );
    assert_eq!(
        config_names(&db, files.independent),
        [None, Some("independent".to_string())],
        "an independent extension sees only base + itself",
    );
}

#[test]
fn common_module_resolution_follows_the_dependency_matrix() {
    let (db, files) = setup();
    let resolve = |file_id: FileId, name: &str| db.resolve_common_module(file_id, name).is_some();

    assert!(resolve(files.tests, "БазовыйМодуль"));
    assert!(resolve(files.tests, "МодульЮнит"), "TESTS must see its dependency's module");
    assert!(resolve(files.tests, "МодульТестов"));
    assert!(!resolve(files.tests, "МодульНезависимый"), "TESTS must not see unrelated CFE");

    assert!(!resolve(files.yaxunit, "МодульТестов"), "a dependency must not see its dependent");
    assert!(resolve(files.yaxunit, "МодульЮнит"));

    assert!(!resolve(files.independent, "МодульЮнит"));
    assert!(!resolve(files.independent, "МодульТестов"));
    assert!(resolve(files.independent, "МодульНезависимый"));

    assert!(!resolve(files.base, "МодульЮнит"), "a base file sees no CFE modules");
    assert!(resolve(files.base, "БазовыйМодуль"));
}

fn catalog_attribute_names(db: &RootDatabaseImpl, file_id: FileId, catalog: &str) -> Vec<String> {
    db.merged_visible_configuration(file_id)
        .and_then(|config| {
            config
                .find_metadata_object(bsl_metadata::MdoType::Catalog, catalog)
                .map(|mdo| mdo.attributes.iter().map(|a| a.name.clone()).collect())
        })
        .unwrap_or_default()
}

#[test]
fn chain_merge_composes_forward_and_preserves_inherited_fields() {
    let (db, files) = setup();

    let tests_attrs = catalog_attribute_names(&db, files.tests, "Товары");
    for attr in ["Цвет", "Вес", "Габарит"] {
        assert!(
            tests_attrs.iter().any(|a| a == attr),
            "TESTS must see base + dependency + own overlay attributes; missing {attr} in {tests_attrs:?}",
        );
    }

    let yaxunit_attrs = catalog_attribute_names(&db, files.yaxunit, "Товары");
    assert!(yaxunit_attrs.iter().any(|a| a == "Цвет"), "base attribute must survive the overlay");
    assert!(yaxunit_attrs.iter().any(|a| a == "Вес"));
    assert!(
        !yaxunit_attrs.iter().any(|a| a == "Габарит"),
        "a dependent's overlay must not leak into its dependency",
    );

    let independent_attrs = catalog_attribute_names(&db, files.independent, "Товары");
    assert!(
        !independent_attrs.iter().any(|a| a == "Вес"),
        "an unrelated extension's overlay must not leak",
    );
    assert!(
        db.merged_visible_configuration(files.independent)
            .and_then(|c| c
                .find_metadata_object(bsl_metadata::MdoType::Catalog, "СвойОбъект")
                .map(|_| ()))
            .is_some(),
        "the independent extension still sees its own objects",
    );

    let base_attrs = catalog_attribute_names(&db, files.base, "Товары");
    assert!(base_attrs.iter().any(|a| a == "Цвет"));
    assert!(
        !base_attrs.iter().any(|a| a == "Вес" || a == "Габарит"),
        "a base file sees the un-overlaid base object: {base_attrs:?}",
    );
}

/// The diagnostics provider enumeration (`ProtectedModule`,
/// `PrivilegedModuleMethodCall` and `MissedRequiredParameter` all iterate it)
/// must follow the same per-file matrix — a registered but unrelated sibling
/// extension must not contribute to a file's diagnostics.
#[test]
fn provider_visible_configurations_follow_the_dependency_matrix() {
    use ide_db::AnalysisProvider;
    let (db, files) = setup();
    let provider = ide_db::SalsaProvider::new(&db, None);
    let names = |file_id: FileId| -> Vec<Option<String>> {
        provider.visible_configurations(file_id).into_iter().map(|vc| vc.config.name).collect()
    };

    assert_eq!(names(files.base), [None]);
    assert_eq!(names(files.yaxunit), [None, Some("yaxunit".to_string())]);
    assert_eq!(names(files.tests), [None, Some("yaxunit".to_string()), Some("tests".to_string())],);
    assert_eq!(names(files.independent), [None, Some("independent".to_string())]);
}

/// The full production path — `Project::with_config` → `ExtensionTopology` →
/// `WorkspaceConfigsSnapshot::from_project` — with a TRANSITIVE chain
/// (deep → tests → yaxunit): the two-edge closure must arrive in topological
/// order, the diamond-shared dependency must appear once, and a same-name
/// attribute redeclared by the most-dependent overlay must win (reversing the
/// forward fold would keep the base declaration and fail this).
#[test]
fn project_built_snapshot_resolves_a_transitive_chain() {
    use project_model::{ProjectConfig, StructuredExtensionDecl};
    let decl = |name: &str, dir: &str, deps: &[&str]| {
        project_model::ExtensionDecl::Structured(StructuredExtensionDecl {
            name: name.to_string(),
            path: format!("../bsl-metadata/fixtures/cfe_dependencies/{dir}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            scope: Some(bsl_metadata::ExtensionScope::InfoBase),
        })
    };
    let config = ProjectConfig {
        configuration_root: Some("../bsl-metadata/fixtures/cfe_dependencies/base".to_string()),
        extensions: Some(vec![
            decl("yaxunit", "yaxunit", &[]),
            decl("tests", "tests_ext", &["yaxunit"]),
            decl("deep", "deep", &["tests"]),
        ]),
        ..Default::default()
    };
    let project = project_model::Project::with_config(env!("CARGO_MANIFEST_DIR"), config)
        .expect("fixture project builds");
    let snapshot = WorkspaceConfigsSnapshot::from_project(&project);

    let mut db = RootDatabaseImpl::new();
    let deep_file = FileId::from_raw(1);
    let mut file_set = FileSet::default();
    file_set.insert(
        deep_file,
        VfsPath::new(
            fixture_root("deep")
                .join("CommonModules/МодульГлубокий/Ext/Module.bsl")
                .to_string_lossy()
                .to_string(),
        ),
    );
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(deep_file, SourceRootId(0));
    db.set_file_text(deep_file, "Процедура Тест() КонецПроцедуры");
    db.set_workspace_configs_snapshot(snapshot);

    assert_eq!(
        config_names(&db, deep_file),
        [None, Some("yaxunit".to_string()), Some("tests".to_string()), Some("deep".to_string())],
        "the two-edge transitive closure arrives in topological order, the \
         diamond-shared yaxunit exactly once",
    );
    assert!(
        db.resolve_common_module(deep_file, "МодульЮнит").is_some(),
        "a transitive dependency's module resolves through two edges",
    );

    let merged = db.merged_visible_configuration(deep_file).expect("merged configuration for deep");
    let goods = merged
        .find_metadata_object(bsl_metadata::MdoType::Catalog, "Товары")
        .expect("merged Товары");
    let color =
        goods.attributes.iter().find(|a| a.name == "Цвет").expect("Цвет present in merged view");
    assert!(
        matches!(color.attr_type, bsl_metadata::AttributeType::Number { .. }),
        "the most-dependent overlay's redeclaration must win over the base \
         (forward composition, own last); got {:?}",
        color.attr_type,
    );
}

#[test]
fn editing_an_unrelated_extension_does_not_invalidate_the_chain() {
    let (mut db, files) = setup();

    let before = db.merged_visible_configuration(files.tests).expect("merged config");
    db.bump_config_for_path(&fixture_root("independent"));
    let after_unrelated = db.merged_visible_configuration(files.tests).expect("merged config");
    assert!(
        std::sync::Arc::ptr_eq(&before, &after_unrelated),
        "an unrelated extension's change must not recompute TESTS' merged configuration",
    );

    db.bump_config_for_path(&fixture_root("yaxunit"));
    let after_dependency = db.merged_visible_configuration(files.tests).expect("merged config");
    assert!(
        !std::sync::Arc::ptr_eq(&before, &after_dependency),
        "a dependency's change must invalidate its dependents",
    );
}
