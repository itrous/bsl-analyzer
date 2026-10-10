use criterion::{black_box, criterion_group, criterion_main, Criterion};
use hir::DefDatabase;
use ide_db::{
    base_db::{RootQueryDb, SourceDatabase},
    RootDatabaseImpl,
};
use vfs::{file_set::FileSet, FileId, VfsPath};

fn setup_db(num_files: u32) -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();

    let mut file_set = FileSet::new();
    for i in 0..num_files {
        let file_id = FileId(i);
        file_set.insert(file_id, VfsPath::new(format!("/test{}.bsl", i)));
    }

    let source_root = ide_db::base_db::SourceRoot::new_local(file_set);
    db.set_source_root(ide_db::base_db::SourceRootId(0), source_root);

    for i in 0..num_files {
        let file_id = FileId(i);
        db.set_file_source_root(file_id, ide_db::base_db::SourceRootId(0));
        db.set_file_text(
            file_id,
            &format!(
                r#"
Процедура Процедура{}()
    Сообщить("Тест {}", {});
КонецПроцедуры

Функция Функция{}() Экспорт
    Возврат {};
КонецФункции
"#,
                i, i, i, i, i
            ),
        );
    }

    db
}

fn bench_cache_hit(c: &mut Criterion) {
    let db = setup_db(100);
    let file_id = FileId(50);

    let _ = db.parse(file_id);

    c.bench_function("cache_hit", |b| {
        b.iter(|| {
            let _ = db.parse(black_box(file_id));
        });
    });
}

fn bench_incremental_update(c: &mut Criterion) {
    c.bench_function("incremental_update", |b| {
        let mut db = setup_db(100);
        let file_id = FileId(50);

        let _ = db.parse(file_id);

        let mut counter = 0;
        b.iter(|| {
            counter += 1;
            db.set_file_text(
                file_id,
                black_box(&format!(
                    r#"
Процедура ТестоваяПроцедура{}()
    Сообщить("Изменение {}", {});
КонецПроцедуры
"#,
                    counter, counter, counter
                )),
            );
            let _ = db.parse(file_id);
        });
    });
}

fn bench_item_tree_cache_hit(c: &mut Criterion) {
    let db = setup_db(100);
    let file_id = FileId(50);

    let _ = db.item_tree(file_id);

    c.bench_function("item_tree_cache_hit", |b| {
        b.iter(|| {
            let _ = db.item_tree(black_box(file_id));
        });
    });
}

/// Same warm read as `item_tree_cache_hit`, through the borrowed accessor:
/// the delta between the two is the per-read `Arc` clone the owned wrapper pays.
fn bench_item_tree_ref_cache_hit(c: &mut Criterion) {
    let db = setup_db(100);
    let file_id = FileId(50);

    let _ = db.item_tree_ref(file_id);

    c.bench_function("item_tree_ref_cache_hit", |b| {
        b.iter(|| {
            let _ = black_box(db.item_tree_ref(black_box(file_id)));
        });
    });
}

fn bench_item_tree_incremental(c: &mut Criterion) {
    c.bench_function("item_tree_incremental", |b| {
        let mut db = setup_db(100);
        let file_id = FileId(50);

        let _ = db.item_tree(file_id);

        let mut counter = 0;
        b.iter(|| {
            counter += 1;
            db.set_file_text(
                file_id,
                black_box(&format!(
                    r#"
Процедура НоваяПроцедура{}()
КонецПроцедуры

Функция НоваяФункция{}() Экспорт
    Возврат {};
КонецФункции
"#,
                    counter, counter, counter
                )),
            );
            let _ = db.item_tree(file_id);
        });
    });
}

fn bench_symbol_tree_cache_hit(c: &mut Criterion) {
    let db = setup_db(100);
    let module_id = hir::ModuleId::new(FileId(50));

    let _ = db.symbol_tree(module_id);

    c.bench_function("symbol_tree_cache_hit", |b| {
        b.iter(|| {
            let _ = db.symbol_tree(black_box(module_id));
        });
    });
}

/// Borrowed counterpart of `symbol_tree_cache_hit`; see `item_tree_ref_cache_hit`.
fn bench_symbol_tree_ref_cache_hit(c: &mut Criterion) {
    let db = setup_db(100);
    let module_id = hir::ModuleId::new(FileId(50));

    let _ = db.symbol_tree_ref(module_id);

    c.bench_function("symbol_tree_ref_cache_hit", |b| {
        b.iter(|| {
            let _ = black_box(db.symbol_tree_ref(black_box(module_id)));
        });
    });
}

fn bench_large_file_set(c: &mut Criterion) {
    c.bench_function("large_file_set_lru", |b| {
        let db = setup_db(200);

        b.iter(|| {
            for i in 0..200 {
                let file_id = FileId(black_box(i % 200));
                let _ = db.parse(file_id);
            }
        });
    });
}

/// Cost of re-validating the metadata resolution chain after a plain `.bsl`
/// edit. The edit is a LOW-durability write; the metadata cone (XML texts,
/// structure listing, config revisions) is MEDIUM, so `config_index` /
/// `parse_mdo_query` / `resolve_metadata_object` memos should shallow-verify
/// in O(1) instead of re-walking their dependency edges per keystroke.
fn bench_metadata_revalidation_after_bsl_edit(c: &mut Criterion) {
    use ide_db::metadata::{MdoEntry, MetadataListingData};
    use std::path::PathBuf;

    const NUM_MDOS: u32 = 200;

    fn catalog_xml(name: &str, seq: u32) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Catalog uuid="00000000-0000-0000-0000-{seq:012}">
        <Properties><Name>{name}</Name><CodeLength>9</CodeLength></Properties>
    </Catalog>
</MetaDataObject>"#
        )
    }

    let mut db = RootDatabaseImpl::new();
    let bsl_file = FileId(0);

    let mut bsl_set = FileSet::new();
    bsl_set.insert(bsl_file, VfsPath::new("/cfg/CommonModules/Модуль/Ext/Module.bsl"));
    db.set_source_root(
        ide_db::base_db::SourceRootId(0),
        ide_db::base_db::SourceRoot::new_local(bsl_set),
    );
    db.set_file_source_root(bsl_file, ide_db::base_db::SourceRootId(0));
    db.set_file_text(bsl_file, "Процедура А() КонецПроцедуры");

    let mut metadata_set = FileSet::new();
    for i in 0..NUM_MDOS {
        metadata_set
            .insert(FileId(1000 + i), VfsPath::new(format!("/cfg/Catalogs/Справочник{i}.xml")));
    }
    db.set_source_root(
        ide_db::base_db::METADATA_SOURCE_ROOT,
        ide_db::base_db::SourceRoot::new_metadata(metadata_set),
    );
    for i in 0..NUM_MDOS {
        db.set_file_source_root(FileId(1000 + i), ide_db::base_db::METADATA_SOURCE_ROOT);
        db.set_file_text(FileId(1000 + i), &catalog_xml(&format!("Справочник{i}"), i));
    }

    db.set_all_config_paths(vec![(None, PathBuf::from("/cfg"))]);
    let entries: Vec<MdoEntry> = (0..NUM_MDOS)
        .map(|i| MdoEntry {
            kind: bsl_metadata::MdoType::Catalog,
            name: format!("Справочник{i}"),
            main: FileId(1000 + i),
            predefined: None,
        })
        .collect();
    db.set_metadata_listing(
        "/cfg",
        MetadataListingData {
            common_attributes: Vec::new(),
            entries,
            defined_types: Vec::new(),
            common_modules: Vec::new(),
            event_subscriptions: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
            subsystems: Vec::new(),
        },
    );

    for i in 0..NUM_MDOS {
        let name = format!("Справочник{i}");
        let resolved =
            db.resolve_metadata_object_for_file(bsl_file, bsl_metadata::MdoType::Catalog, &name);
        assert!(resolved.is_some(), "warm-up resolve must succeed for {name}");
    }

    c.bench_function("metadata_revalidation_after_bsl_edit", |b| {
        let mut counter = 0;
        b.iter(|| {
            counter += 1;
            db.set_file_text(
                bsl_file,
                black_box(&format!("Процедура А{counter}() КонецПроцедуры")),
            );
            for i in 0..NUM_MDOS {
                let name = format!("Справочник{i}");
                let resolved = db.resolve_metadata_object_for_file(
                    black_box(bsl_file),
                    bsl_metadata::MdoType::Catalog,
                    &name,
                );
                assert!(resolved.is_some());
            }
        });
    });
}

/// Cost of a "new revision, nothing changed" pass — the dominant LSP shape:
/// a single keystroke opens a revision under which thousands of untouched memos
/// must revalidate. `synthetic_write` opens that revision and reports a write of
/// the given durability without mutating any input, so the following reads take
/// the `maybe_changed_after` validate path (a dependency-edge walk that
/// backdates) instead of re-executing. `bench_incremental_update` above measures
/// re-execution after a real edit; this measures the far more frequent
/// validate-only path, which was previously unmeasured.
///
/// The `low` variant models a plain code edit (LOW-durability input). The `high`
/// variant bumps every durability tier at once, exposing the ceiling cost paid
/// when a high-durability input (a library root, metadata) changes and the whole
/// memo graph must revalidate.
fn bench_validate_no_change(c: &mut Criterion) {
    use salsa::{Database, Durability};

    const NUM_FILES: u32 = 200;

    let mut group = c.benchmark_group("validate_no_change");
    for (label, durability) in [("low", Durability::LOW), ("high", Durability::HIGH)] {
        let mut db = setup_db(NUM_FILES);
        for i in 0..NUM_FILES {
            let file_id = FileId(i);
            let _ = db.parse(file_id);
            let _ = db.item_tree(file_id);
            let _ = db.symbol_tree(hir::ModuleId::new(file_id));
        }

        group.bench_function(label, |b| {
            b.iter(|| {
                db.synthetic_write(black_box(durability));
                for i in 0..NUM_FILES {
                    let file_id = FileId(i);
                    let _ = db.parse(black_box(file_id));
                    let _ = db.item_tree(black_box(file_id));
                    let _ = db.symbol_tree(black_box(hir::ModuleId::new(file_id)));
                }
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_cache_hit,
    bench_incremental_update,
    bench_item_tree_cache_hit,
    bench_item_tree_ref_cache_hit,
    bench_item_tree_incremental,
    bench_symbol_tree_cache_hit,
    bench_symbol_tree_ref_cache_hit,
    bench_large_file_set,
    bench_metadata_revalidation_after_bsl_edit,
    bench_validate_no_change
);
criterion_main!(benches);
