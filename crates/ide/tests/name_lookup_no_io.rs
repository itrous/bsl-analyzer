//! Narrowing the question to categories no file can answer must not read files.
//!
//! Proven by substitution rather than by a counter: the module is replaced with
//! a FIFO, so any read of it blocks forever instead of merely being observed.
//! Permissions and deletion would not do — the first turns a read into an error
//! the code may swallow, the second into an empty file that looks like a module
//! with nothing in it.

#![cfg(unix)]

use ide::{lookup_names, NameCategory, NameQuery};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::sync::mpsc;
use std::time::Duration;
use vfs::{file_set::FileSet, FileId, VfsPath};

/// Only bounds a lookup that neither returned nor opened the module. It never
/// decides "the module was read": that verdict comes from the writer below, so a
/// slow machine cannot turn a correct lookup into a reported defect.
const HANG_LIMIT: Duration = Duration::from_secs(60);

struct Stand {
    _dir: tempfile::TempDir,
    module: std::path::PathBuf,
}

fn stand() -> Stand {
    let dir = tempfile::tempdir().expect("temp dir");
    let module = dir.path().join("CommonModules/Настройки/Ext/Module.bsl");
    std::fs::create_dir_all(module.parent().unwrap()).expect("module dir");

    let made = std::process::Command::new("mkfifo").arg(&module).status().expect("mkfifo runs");
    assert!(made.success(), "mkfifo failed for {}", module.display());

    Stand { _dir: dir, module }
}

fn db_over(module: &std::path::Path) -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    let file_id = FileId(0);
    file_set.insert(file_id, VfsPath::new(module.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    // No overlay: the text has to come from disk, which is where the FIFO is.
    db.set_file_revision_from_disk(file_id, 1);
    db
}

enum Outcome {
    /// The lookup opened the module: the writer's `open` returned.
    ReadTheModule,
    /// The lookup returned and nobody ever opened the module.
    LeftItAlone,
    /// Neither happened within `HANG_LIMIT` — slow, not evidence of a read.
    Undecided,
}

/// Runs a lookup and reports whether it opened the module.
///
/// Opening a FIFO for writing blocks until someone opens it for reading, so a
/// writer thread whose `open` returned is direct proof of a read — no deadline
/// stands between the lookup and the verdict. The writer signals before it
/// closes, and the reader only sees EOF after the close, so the signal is
/// already sent by the time a reading lookup returns.
fn probe(categories: Option<&'static [NameCategory]>) -> Outcome {
    let stand = stand();
    let (opened_tx, opened_rx) = mpsc::channel();
    let path = stand.module.clone();
    let writer = std::thread::spawn(move || {
        if let Ok(fifo) = std::fs::OpenOptions::new().write(true).open(&path) {
            let _ = opened_tx.send(());
            drop(fifo);
        }
    });

    let (done_tx, done_rx) = mpsc::channel();
    let path = stand.module.clone();
    std::thread::spawn(move || {
        let db = db_over(&path);
        let mut query = NameQuery::new("СтрНайти", 20);
        query.categories = categories;
        let found = lookup_names(&db, &query, &[]);
        let _ = done_tx.send(found.candidates.len());
    });

    let finished = done_rx.recv_timeout(HANG_LIMIT).is_ok();
    let outcome = if opened_rx.try_recv().is_ok() {
        Outcome::ReadTheModule
    } else if finished {
        Outcome::LeftItAlone
    } else {
        Outcome::Undecided
    };
    if matches!(outcome, Outcome::LeftItAlone) {
        // Nobody read, so nobody let the writer through: it is still parked in `open`,
        // and unlinking the FIFO would not wake it — a blocked open waits on the inode,
        // not the name. The stand opens the read side itself, after the verdict is
        // taken, so the writer finishes and the thread does not outlive the test.
        let _reader = std::fs::File::open(&stand.module).expect("the FIFO opens for reading");
    }
    if !matches!(outcome, Outcome::Undecided) {
        writer.join().expect("the writer thread finishes");
    }
    // The stand outlives the lookup: unlinking the FIFO under a blocked read
    // would make the control pass for the wrong reason.
    drop(stand);
    outcome
}

const PLATFORM_ONLY: &[NameCategory] = &[NameCategory::PlatformMember];

#[test]
fn a_platform_only_question_never_touches_a_module_file() {
    match probe(Some(PLATFORM_ONLY)) {
        Outcome::LeftItAlone => {}
        Outcome::ReadTheModule => {
            panic!("the lookup opened the module it had no reason to read")
        }
        Outcome::Undecided => panic!(
            "the lookup neither returned nor opened the module within {HANG_LIMIT:?}: \
             too slow to judge, which is not the same as reading the file",
        ),
    }
}

/// The control that makes the test above mean something: the same stand, asked
/// the wide question, DOES read the module. Without it a lookup that never reads
/// anything at all would pass both ways.
#[test]
fn the_same_stand_asked_widely_does_read_the_module() {
    assert!(
        matches!(probe(None), Outcome::ReadTheModule),
        "the wide question did not open the module — the stand is not \
         sensitive to file reads, so the narrowed case proves nothing",
    );
}
