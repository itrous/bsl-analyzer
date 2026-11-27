use std::{cell::Cell, rc::Rc};

use ide_db::base_db::{FileIdInput, SourceDatabase, SourceRoot, SourceRootId};
use vfs::{file_set::FileSet, FileId, VfsPath};

use super::{
    build_call_hierarchy_index, build_call_hierarchy_index_with_observer,
    reject_incomplete_notify_projection, reproject_call_hierarchy_index_modules, BatchObserver,
    CallHierarchyBatchEvent, CallHierarchyBatchEventKind, CallHierarchyBatchPhase,
    CallHierarchyIndexBuildError, CallHierarchyIndexBuildRequest, CallHierarchyIndexBuildResult,
};
use crate::graph::{run_batch_db, BatchDbRelease};
use crate::RootDatabaseImpl;
use hir::{semantic_notify_facts_with_work_limit, DefDatabase, MethodId, ModuleId};

const ROOT: SourceRootId = SourceRootId(0);

#[derive(Clone, Copy)]
struct FixtureFile {
    path: &'static str,
    text: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
enum ProbeEvent {
    Started(CallHierarchyBatchPhase, usize),
    Opened,
    Dropped(CallHierarchyBatchPhase, usize),
    CacheCleared(CallHierarchyBatchPhase, usize),
    Completed(CallHierarchyBatchPhase, usize),
}

struct Probe {
    events: Rc<std::cell::RefCell<Vec<ProbeEvent>>>,
    live: Rc<Cell<usize>>,
    cache_clears: usize,
}

impl BatchObserver for Probe {
    fn on_event(&mut self, event: &CallHierarchyBatchEvent) {
        let probe_event = match event.kind {
            CallHierarchyBatchEventKind::Started => {
                ProbeEvent::Started(event.phase, event.batch_index)
            }
            CallHierarchyBatchEventKind::DatabaseDropped => {
                let live = self.live.get();
                assert!(live > 0, "a database must be live until its drop event");
                self.live.set(live - 1);
                ProbeEvent::Dropped(event.phase, event.batch_index)
            }
            CallHierarchyBatchEventKind::Completed => {
                ProbeEvent::Completed(event.phase, event.batch_index)
            }
        };
        self.events.borrow_mut().push(probe_event);
    }

    fn on_node_caches_cleared(&mut self, phase: CallHierarchyBatchPhase, batch_index: usize) {
        self.cache_clears += 1;
        self.events.borrow_mut().push(ProbeEvent::CacheCleared(phase, batch_index));
    }
}

// The first module exercises every cross-module pair shape the fused build
// retains as intents: a direct local call, a qualified cross-module call, a
// module-receiver notify callback, and a current-module idle handler (whose
// pair collapses into the direct local one after dedup).
fn fixture() -> [FixtureFile; 3] {
    [
        FixtureFile {
            path: "/src/CommonModules/Первый/Ext/Module.bsl",
            text: "Процедура Начать()\nВнутренний();\nВторой.Продолжить();\nОп = Новый ОписаниеОповещения(\"Завершить\", Второй);\nПодключитьОбработчикОжидания(\"Внутренний\", 1);\nКонецПроцедуры\n\nПроцедура Внутренний() Экспорт\nКонецПроцедуры",
        },
        FixtureFile {
            path: "/src/CommonModules/Второй/Ext/Module.bsl",
            text: "Процедура Продолжить() Экспорт\nЗавершить();\nКонецПроцедуры\n\nПроцедура Завершить() Экспорт\nКонецПроцедуры",
        },
        FixtureFile {
            path: "/src/CommonModules/Третий/Ext/Module.bsl",
            text: "Процедура Закрыть()\nОчистить();\nКонецПроцедуры\n\nПроцедура Очистить()\nКонецПроцедуры",
        },
    ]
}

fn batch_database(files: &[FixtureFile]) -> RootDatabaseImpl {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::new();
    for (index, file) in files.iter().enumerate() {
        file_set.insert(FileId(index as u32), VfsPath::new(file.path));
    }
    db.set_source_root(ROOT, SourceRoot::new_local(file_set));
    // Inference can read a cross-module target before the index projects the call.
    for (index, file) in files.iter().enumerate() {
        let file_id = FileId(index as u32);
        db.set_file_source_root(file_id, ROOT);
        db.set_file_text(file_id, file.text);
    }
    db
}

// Two lanes, regardless of the host's core count, so event sequences and
// live-database bounds are machine-independent.
fn build(batch_size: usize) -> (CallHierarchyIndexBuildResult, Vec<ProbeEvent>, usize) {
    build_with_concurrency(batch_size, 2)
}

fn build_with_concurrency(
    batch_size: usize,
    concurrency: usize,
) -> (CallHierarchyIndexBuildResult, Vec<ProbeEvent>, usize) {
    let files = fixture();
    let modules = [ModuleId::new(FileId(0)), ModuleId::new(FileId(1)), ModuleId::new(FileId(2))];
    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    let live = Rc::new(Cell::new(0usize));
    let max_live = concurrency.max(1);
    let open_events = Rc::clone(&events);
    let open_live = Rc::clone(&live);
    let mut open_batch = move |_batch: &[ModuleId]| {
        let live = open_live.get() + 1;
        assert!(live <= max_live, "at most {max_live} batch databases may be live, saw {live}");
        open_live.set(live);
        open_events.borrow_mut().push(ProbeEvent::Opened);
        batch_database(&files)
    };
    let mut probe = Probe { events: Rc::clone(&events), live: Rc::clone(&live), cache_clears: 0 };

    let result = build_call_hierarchy_index_with_observer(
        CallHierarchyIndexBuildRequest::new(&modules, batch_size).with_concurrency(concurrency),
        &mut open_batch,
        &mut probe,
    )
    .expect("fixture builder should succeed");

    assert_eq!(live.get(), 0, "every batch database must be dropped");
    let recorded_events = std::mem::take(&mut *events.borrow_mut());
    (result, recorded_events, probe.cache_clears)
}

mod parity;

#[derive(Debug, PartialEq, Eq)]
struct IndexDigest {
    layout_hashes: Vec<(ModuleId, u64)>,
    callers: Vec<(MethodId, Vec<MethodId>)>,
}

fn digest(result: &CallHierarchyIndexBuildResult) -> IndexDigest {
    let key = hir::MethodKey::first;
    let methods = [
        MethodId { module: ModuleId::new(FileId(0)), local_id: key("Начать") },
        MethodId { module: ModuleId::new(FileId(0)), local_id: key("Внутренний") },
        MethodId { module: ModuleId::new(FileId(1)), local_id: key("Продолжить") },
        MethodId { module: ModuleId::new(FileId(1)), local_id: key("Завершить") },
        MethodId { module: ModuleId::new(FileId(2)), local_id: key("Закрыть") },
        MethodId { module: ModuleId::new(FileId(2)), local_id: key("Очистить") },
    ];
    let modules = [ModuleId::new(FileId(0)), ModuleId::new(FileId(1)), ModuleId::new(FileId(2))];

    IndexDigest {
        layout_hashes: modules
            .into_iter()
            .map(|module| (module, result.index.layout_hash(module).expect("layout hash")))
            .collect(),
        callers: methods
            .into_iter()
            .map(|method| (method, result.index.callers(method).to_vec()))
            .collect(),
    }
}

#[test]
fn budget_truncated_notify_projection_rejects_publication_of_the_incoming_index() {
    // Given: the same module projected twice — once under the production budget,
    // once with the budget exhausted through the production work-limit hook, so
    // the truncated summary is produced by the projection itself, not assembled
    // by hand.
    let files = fixture();
    let db = batch_database(&files);
    let module = ModuleId::new(FileId(0));

    let complete = db.module_call_summary(module).method_pair_subset();
    assert!(!complete.notify_projection_incomplete);
    let truncated =
        semantic_notify_facts_with_work_limit(&db, FileIdInput::new(&db, module.file_id), 0)
            .summary
            .method_pair_subset();
    assert!(truncated.notify_projection_incomplete);

    // A completed projection publishes.
    assert!(reject_incomplete_notify_projection(&[(module, complete)]).is_ok());

    // When: pass 1 retains the truncated subset.
    let result = reject_incomplete_notify_projection(&[(module, truncated)]);

    // Then: the build fails naming the module instead of serving a reverse index
    // whose registrations were only partly read.
    assert!(matches!(
        result,
        Err(CallHierarchyIndexBuildError::IncompleteNotifyProjection(found)) if found == module
    ));
}

#[test]
fn named_unknown_array_gaps_publish_while_only_budget_truncation_rejects() {
    // Given: a projection whose alias analysis stops at the intentional
    // unknown-array boundary — an array whose value depends on a branch the
    // analysis must not guess.
    let files = [FixtureFile {
        path: "/src/CommonModules/Первый/Ext/Module.bsl",
        text: "Процедура Зарегистрировать(А, Флаг)\nА.Добавить(\"Завершить\");\nА.Добавить(ЭтотОбъект);\nЕсли Флаг Тогда\nА = Новый Массив;\nА.Добавить(\"Другое\");\nА.Добавить(ЭтотОбъект);\nКонецЕсли;\nО = Новый(\"CallbackDescription\", А);\nКонецПроцедуры\n\nПроцедура Завершить(Параметр) Экспорт\nКонецПроцедуры",
    }];
    let db = batch_database(&files);
    let module = ModuleId::new(FileId(0));
    let source_summary = db.module_call_summary(module);
    assert!(!source_summary.notify_dynamic_signature_analysis_incomplete.is_empty());
    let summary = source_summary.method_pair_subset();

    assert!(!summary.notify_projection_incomplete);
    assert!(summary.notify_dynamic_signature_analysis_incomplete.is_empty());

    // When/Then: the conservative boundary publishes; the budget marker is the
    // only rejection trigger.
    assert!(reject_incomplete_notify_projection(&[(module, summary)]).is_ok());
}

/// A module whose dynamic callback constructors alone drive the semantic notify
/// projection past its capped production work budget, so `module_call_summary`
/// reports it incomplete through the real budgeted query, with no test hook.
fn budget_exhausting_files() -> [FixtureFile; 1] {
    let mut text = String::from("Процедура Начать(А)\n");
    for i in 0..20_000 {
        text.push_str(&format!("О{i} = Новый(\"CallbackDescription\", А);\n"));
    }
    text.push_str("КонецПроцедуры\n\nПроцедура Завершить(Параметр) Экспорт\nКонецПроцедуры\n");
    [FixtureFile {
        path: "/src/CommonModules/Первый/Ext/Module.bsl",
        text: Box::leak(text.into_boxed_str()),
    }]
}

#[test]
fn budget_exhausted_module_stops_the_whole_build_before_publication() {
    // Given: a module whose dynamic callback constructors exhaust even the
    // production budget's hard cap, so its projection is incomplete.
    let files = budget_exhausting_files();
    let module = ModuleId::new(FileId(0));

    // When: the full build projects the module for publication.
    let modules = [module];
    let mut open_batch = |_batch: &[ModuleId]| batch_database(&files);
    let result = build_call_hierarchy_index(
        CallHierarchyIndexBuildRequest::new(&modules, 1),
        &mut open_batch,
    );

    // Then: the build fails naming the module instead of publishing a reverse
    // index built from partly read registrations.
    assert!(matches!(
        result,
        Err(CallHierarchyIndexBuildError::IncompleteNotifyProjection(found)) if found == module
    ));
}

#[test]
fn budget_exhausted_module_refuses_catch_up_publication() {
    // Given: an index built while the module stayed well inside the budget, and
    // the same module edited until its constructors exhaust the production
    // notify work budget's hard cap — the real catch-up shape for an edit that
    // turns a module pathological.
    let module = ModuleId::new(FileId(0));
    let base_files = [FixtureFile {
        path: "/src/CommonModules/Первый/Ext/Module.bsl",
        text: "Процедура Начать(А)\nО = Новый(\"CallbackDescription\", А);\nКонецПроцедуры\n\nПроцедура Завершить(Параметр) Экспорт\nКонецПроцедуры",
    }];
    let base_db = batch_database(&base_files);
    let mut graph_index = hir::graph_index::GraphIndex::new();
    graph_index.add_module(&base_db, module);

    let edited_files = budget_exhausting_files();
    let mut open_batch = |_batch: &[ModuleId]| batch_database(&edited_files);

    // When: catch-up reprojects the edited module.
    let result =
        reproject_call_hierarchy_index_modules(&graph_index, 1, &[module], &mut open_batch);

    // Then: the projection is refused naming the module, so the published index
    // is superseded instead of gaining partly read entries.
    assert!(matches!(
        result,
        Err(CallHierarchyIndexBuildError::IncompleteNotifyProjection(found)) if found == module
    ));
}

#[test]
fn bounded_call_hierarchy_index_builder_drops_batches_and_clears_node_caches() {
    // Given: three modules processed one at a time.
    let (result, events, cache_clears) = build(1);

    // When: the builder completes the extraction batches and the resolve step.

    // Then: two batches may be in flight, every batch is folded (drop + cache
    // clear) in batch order, and pair resolution runs as one whole-workspace
    // lifecycle batch.
    assert_eq!(result.method_count, 6);
    assert_eq!(result.pair_count, 5);
    assert_eq!(cache_clears, 4);
    assert_eq!(result.rss_samples.len(), 4);
    assert_eq!(
        events,
        vec![
            ProbeEvent::Started(CallHierarchyBatchPhase::Index, 0),
            ProbeEvent::Opened,
            ProbeEvent::Started(CallHierarchyBatchPhase::Index, 1),
            ProbeEvent::Opened,
            ProbeEvent::Dropped(CallHierarchyBatchPhase::Index, 0),
            ProbeEvent::CacheCleared(CallHierarchyBatchPhase::Index, 0),
            ProbeEvent::Completed(CallHierarchyBatchPhase::Index, 0),
            ProbeEvent::Started(CallHierarchyBatchPhase::Index, 2),
            ProbeEvent::Opened,
            ProbeEvent::Dropped(CallHierarchyBatchPhase::Index, 1),
            ProbeEvent::CacheCleared(CallHierarchyBatchPhase::Index, 1),
            ProbeEvent::Completed(CallHierarchyBatchPhase::Index, 1),
            ProbeEvent::Dropped(CallHierarchyBatchPhase::Index, 2),
            ProbeEvent::CacheCleared(CallHierarchyBatchPhase::Index, 2),
            ProbeEvent::Completed(CallHierarchyBatchPhase::Index, 2),
            ProbeEvent::Started(CallHierarchyBatchPhase::MethodPairs, 0),
            ProbeEvent::Opened,
            ProbeEvent::Dropped(CallHierarchyBatchPhase::MethodPairs, 0),
            ProbeEvent::CacheCleared(CallHierarchyBatchPhase::MethodPairs, 0),
            ProbeEvent::Completed(CallHierarchyBatchPhase::MethodPairs, 0),
        ],
    );
}

#[test]
fn bounded_call_hierarchy_index_builder_serializes_batches_without_concurrency() {
    // Given: three modules processed one at a time with a single lane.
    let (result, events, cache_clears) = build_with_concurrency(1, 1);

    // Then: every database is dropped before the next open.
    assert_eq!(result.method_count, 6);
    assert_eq!(result.pair_count, 5);
    assert_eq!(cache_clears, 4);
    assert_eq!(result.rss_samples.len(), 4);
    assert_eq!(
        events,
        vec![
            ProbeEvent::Started(CallHierarchyBatchPhase::Index, 0),
            ProbeEvent::Opened,
            ProbeEvent::Dropped(CallHierarchyBatchPhase::Index, 0),
            ProbeEvent::CacheCleared(CallHierarchyBatchPhase::Index, 0),
            ProbeEvent::Completed(CallHierarchyBatchPhase::Index, 0),
            ProbeEvent::Started(CallHierarchyBatchPhase::Index, 1),
            ProbeEvent::Opened,
            ProbeEvent::Dropped(CallHierarchyBatchPhase::Index, 1),
            ProbeEvent::CacheCleared(CallHierarchyBatchPhase::Index, 1),
            ProbeEvent::Completed(CallHierarchyBatchPhase::Index, 1),
            ProbeEvent::Started(CallHierarchyBatchPhase::Index, 2),
            ProbeEvent::Opened,
            ProbeEvent::Dropped(CallHierarchyBatchPhase::Index, 2),
            ProbeEvent::CacheCleared(CallHierarchyBatchPhase::Index, 2),
            ProbeEvent::Completed(CallHierarchyBatchPhase::Index, 2),
            ProbeEvent::Started(CallHierarchyBatchPhase::MethodPairs, 0),
            ProbeEvent::Opened,
            ProbeEvent::Dropped(CallHierarchyBatchPhase::MethodPairs, 0),
            ProbeEvent::CacheCleared(CallHierarchyBatchPhase::MethodPairs, 0),
            ProbeEvent::Completed(CallHierarchyBatchPhase::MethodPairs, 0),
        ],
    );
}

#[test]
fn bounded_call_hierarchy_index_builder_has_the_same_digest_for_every_batch_size() {
    // Given: the same modules with one, two, and all modules in a batch, with and
    // without pipelined lanes.
    let (single, _, _) = build(1);
    let (two, _, _) = build(2);
    let (all, _, _) = build(fixture().len());
    let (single_lane, _, _) = build_with_concurrency(1, 1);

    // When: each build completes.

    // Then: batching and pipelining change residency only, not the compact index.
    assert_eq!(digest(&single), digest(&two));
    assert_eq!(digest(&single), digest(&all));
    assert_eq!(digest(&single), digest(&single_lane));
}

#[test]
fn bounded_call_hierarchy_index_builder_does_not_enter_query_or_sdbl_phases() {
    // Given: a bounded method-only build.
    let (result, _, _) = build(2);

    // When: its phase log is inspected.

    // Then: the closed phase set proves only indexing and method-pair projection ran.
    for event in result.batch_events {
        match event.phase {
            CallHierarchyBatchPhase::Index | CallHierarchyBatchPhase::MethodPairs => {}
        }
    }
}

#[test]
fn run_batch_db_emits_database_dropped_before_node_caches_cleared() {
    // Given: a single empty batch and a fresh pool.
    let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
    let mut opened = 0;
    let mut open_batch = |_batch: &[ModuleId]| {
        opened += 1;
        RootDatabaseImpl::default()
    };
    let mut releases = Vec::new();

    // When: the batch runner completes successfully.
    let summary = run_batch_db(
        &[],
        &mut open_batch,
        &pool,
        |_db| 42,
        |release| {
            let marker = match release {
                BatchDbRelease::DatabaseDropped(s) => {
                    assert_eq!(*s, 42);
                    "dropped"
                }
                BatchDbRelease::NodeCachesCleared(s) => {
                    assert_eq!(*s, 42);
                    "cleared"
                }
            };
            releases.push(marker);
        },
    );

    // Then: the database is opened once, the summary is returned, and release
    // events arrive in the order the helper promises.
    assert_eq!(summary, 42);
    assert_eq!(opened, 1);
    assert_eq!(releases, vec!["dropped", "cleared"]);
}

#[test]
fn run_batch_db_cleans_up_even_when_run_returns_an_error() {
    // Given: a batch whose work fails.
    let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
    let mut opened = 0;
    let mut open_batch = |_batch: &[ModuleId]| {
        opened += 1;
        RootDatabaseImpl::default()
    };
    let mut dropped = false;
    let mut cleared = false;

    // When: the runner is invoked with a failing work closure.
    let result: Result<i32, &'static str> = run_batch_db(
        &[],
        &mut open_batch,
        &pool,
        |_db| Err("simulated batch failure"),
        |release| match release {
            BatchDbRelease::DatabaseDropped(_) => dropped = true,
            BatchDbRelease::NodeCachesCleared(_) => cleared = true,
        },
    );

    // Then: the error is returned, but both cleanup steps still ran.
    assert_eq!(result, Err("simulated batch failure"));
    assert_eq!(opened, 1);
    assert!(dropped, "database must be dropped before the error propagates");
    assert!(cleared, "node caches must be cleared before the error propagates");
}
