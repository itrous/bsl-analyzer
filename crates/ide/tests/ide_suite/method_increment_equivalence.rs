//! Incremental diagnostics must equal a cold computation (github#113, I2).
//!
//! Per-method memoisation is only a valid optimisation if it changes nothing
//! observable: after a sequence of edits applied one revision at a time, the
//! file's diagnostics have to be the same multiset a fresh database computes
//! from the final text. The stand edits bodies and one signature so that the
//! memos of every other method are reused across the sequence — exactly the
//! path where a stale or mis-lifted memo would show.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticsConfig};
use test_fixture::SyntheticModuleSpec;
use vfs::{FileId, FileSet, VfsPath};

const METHODS: usize = 120;
const EDITS: usize = 12;

fn database(text: &str) -> (RootDatabaseImpl, FileId) {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    let file_id = FileId(0);
    file_set.insert(file_id, VfsPath::new("/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    (db, file_id)
}

/// The file's diagnostics in a canonical order. Whole `Diagnostic` values are
/// compared, fixes included: a fix edit lifted with the wrong base is as much a
/// divergence as a misplaced finding.
fn diagnostics(db: &RootDatabaseImpl, file_id: FileId) -> Vec<Diagnostic> {
    let mut rows =
        ide_diagnostics::file_diagnostics(db, file_id, &DiagnosticsConfig::all_enabled());
    rows.sort_by_cached_key(|d| {
        (
            d.code.as_str(),
            d.range.start(),
            d.range.end(),
            d.message.clone(),
            format!("{:?}", d.fixes),
        )
    });
    rows
}

/// Deterministic pseudo-random method indices in descending order, so each
/// insertion leaves every earlier offset valid and the edits can be applied
/// to the accumulated text in sequence.
fn edited_methods(seed: u64) -> Vec<usize> {
    let mut state = seed;
    let mut picked = Vec::new();
    while picked.len() < EDITS {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let index = (state >> 33) as usize % METHODS;
        if !picked.contains(&index) {
            picked.push(index);
        }
    }
    picked.sort_unstable_by(|a, b| b.cmp(a));
    picked
}

#[test]
fn incremental_edits_match_a_fresh_database() {
    let module = SyntheticModuleSpec {
        methods: METHODS,
        query_every: 7,
        preproc_inside_every: 5,
        preproc_around_every: 11,
        annotate_every: 3,
        docstring_every: 4,
        call_next_every: 2,
        conditional_return_every: 3,
        module_vars: 2,
        ..Default::default()
    }
    .build();

    let (mut db, file_id) = database(&module.text);
    let initial = diagnostics(&db, file_id);
    assert!(!initial.is_empty(), "the stand must produce diagnostics to compare");

    // Indices descend, so every edit lands above the previous ones and the
    // offsets recorded for the original text stay valid. The first three
    // edits change the method list itself: a duplicate name in front of a
    // method (its namesake below is now the second of two), a removal, and a
    // new name — the edits under which a method's identity must hold while
    // its position does not. The generator names every method uniquely, so
    // the duplicate is made here or the sequence never sees one.
    let mut text = module.text.clone();
    for (step, index) in edited_methods(0x1135).into_iter().enumerate() {
        let method = &module.methods[index];
        match step {
            0 => text.insert_str(method.block.start as usize, &module.namesake_of(index, "\n")),
            1 => text.replace_range(method.block.start as usize..method.block.end as usize, ""),
            2 => text.insert_str(
                method.block.start as usize,
                "Процедура Вставленная() Экспорт\n\tВставка = 1;\nКонецПроцедуры\n\n",
            ),
            _ if step % 4 == 3 => {
                text.insert_str(method.signature_insert_offset as usize, "Новый, ");
            }
            _ => {
                let at = method.body_insert_offset as usize;
                text.insert_str(at, &format!("\tПравка{step} = {step};\n"));
            }
        }
        db.set_file_text(file_id, &text);
        let _ = diagnostics(&db, file_id);
    }
    let duplicated = &module.methods[edited_methods(0x1135)[0]];
    let keyword = if duplicated.is_function { "Функция" } else { "Процедура" };
    assert_eq!(
        text.matches(&format!("{keyword} {}(", duplicated.name)).count(),
        2,
        "the sequence must leave two declarations of one name"
    );

    let incremental = diagnostics(&db, file_id);
    let (fresh_db, fresh_file) = database(&text);
    let fresh = diagnostics(&fresh_db, fresh_file);

    assert_ne!(incremental, initial, "the edits must change the diagnostics");
    assert_eq!(incremental, fresh, "incremental diagnostics diverged from a cold computation");
}
