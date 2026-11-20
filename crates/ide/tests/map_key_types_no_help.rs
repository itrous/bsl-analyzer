//! `Соответствие.Получить` is refined only when the platform data finds the method.
//!
//! With no platform help (`source = "none"`) the platform serves no methods, so the read stays what
//! the engine made of it before the profile existed, with the profile on as well. The platform
//! singleton is process-wide, hence a target of its own; the refinement itself is checked, with
//! the default platform data, in the `map_key_types` module of `ide_suite`.

use bsl_platform::{install_platform_help, PlatformData, PlatformHelp, PlatformHelpRequest};
use hir::{Semantics, TypeKernelDb};
use ide_db::base_db::{RootQueryDb, SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use syntax::ast::{self, AstNode};
use test_fixture::Fixture;
use vfs::FileSet;

const SOURCE: &str = "//- /test.bsl\nПроцедура Тест()\n    Данные = Новый Соответствие;\n    \
                      Данные.Вставить(\"сумма\", 1);\n    Р = Данные.Получить(\"сумма\");\n\
                      КонецПроцедуры\n";

fn install_none() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    let _ = install_platform_help(help);
    assert_eq!(PlatformData::instance().help_request(), &PlatformHelpRequest::None);
}

/// The type of the one `.Получить(...)` call, as the kernel's debug text.
fn read_type(map_keys: bool) -> String {
    let fixture = Fixture::parse(SOURCE);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_map_key_types_enabled(map_keys);
    let (file, _) = fixture.files.iter().next().expect("one file");
    let sema = Semantics::new(&db);
    let root = db.parse(*file).syntax_node();
    let call = root
        .descendants()
        .filter_map(ast::CallExpr::cast)
        .find(|call| call.syntax().text().to_string().contains("Получить"))
        .expect("the read");
    let ty = sema.type_of_expr(*file, call.syntax());
    format!("{:?}", db.lookup_type(ty))
}

#[test]
fn a_read_is_not_refined_when_the_platform_finds_no_method() {
    install_none();
    assert!(
        PlatformData::instance().get_method("Соответствие", "Получить").is_none()
            && PlatformData::instance().get_method("Map", "Get").is_none(),
        "the premise: no help serves the method"
    );

    let with_profile = read_type(true);
    let without_profile = read_type(false);
    assert_eq!(with_profile, without_profile, "the profile must not invent a method");
    assert!(
        !with_profile.contains("Number"),
        "no value type without a found method: {with_profile}"
    );
}
