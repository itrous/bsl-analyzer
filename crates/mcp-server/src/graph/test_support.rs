use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::graph_db::build_graph_database;

use super::build::GRAPH_BUILD_BATCH;
use super::state::lock_recover;
use super::{GraphState, GraphStatus};

/// Leave the graph's file unusable, as a failed replacement does, until a rebuild replaces it.
pub(crate) fn mark_graph_unusable(graph: &GraphState, reason: &str) {
    graph.store.mark_unusable(reason.to_owned());
}

pub(super) fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Minimal common-module metadata descriptor so the module is declared in the
/// configuration (the resolver refuses qualified calls to undeclared modules)
/// and its client/server execution context is known.
pub(super) fn write_common_module(root: &Path, name: &str, server: bool, body: &str) {
    let client = !server;
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="00000000-0000-0000-0000-0000000000{id:02}">
		<Properties>
			<Name>{name}</Name>
			<Global>false</Global>
			<ClientManagedApplication>{client}</ClientManagedApplication>
			<Server>{server}</Server>
			<ExternalConnection>false</ExternalConnection>
			<ClientOrdinaryApplication>{client}</ClientOrdinaryApplication>
			<ServerCall>false</ServerCall>
			<Privileged>false</Privileged>
			<ReturnValuesReuse>DontUse</ReturnValuesReuse>
		</Properties>
	</CommonModule>
</MetaDataObject>"#,
        id = name.len(),
    );
    write(root, &format!("CommonModules/{name}.xml"), &xml);
    write(root, &format!("CommonModules/{name}/Ext/Module.bsl"), body);
}

pub(crate) fn sample_workspace(root: &Path) {
    write_common_module(
        root,
        "Клиент",
        false,
        "&НаКлиенте\nПроцедура Главная() Экспорт\nСервер.Считать();\nКонецПроцедуры",
    );
    write_common_module(root, "Сервер", true, "&НаСервере\nФункция Считать() Экспорт КонецФункции");
}

/// Start the graph test watcher with the same cache exclusion as production.
///
/// Graph builds write their derived SQLite files below `<workspace>/.build`. A raw recursive
/// watcher would report those writes as workspace changes and make an otherwise coherent test
/// publication stale.
pub(crate) fn workspace_hub(root: &Path) -> crate::change_hub::WorkspaceChangeHub {
    let cache = crate::cache::WorkspaceCacheLayout::for_workspace(root);
    crate::change_hub::WorkspaceChangeHub::start_targets_excluding(
        vec![crate::change_hub::WatchTarget::recursive(root.to_path_buf())],
        cache.exclusions(root),
    )
}

/// How often a bounded wait re-reads the state it is waiting on.
pub(crate) const WAIT_POLL: Duration = Duration::from_millis(10);
/// The default ceiling for a bounded wait on graph state.
pub(crate) const WAIT_CEILING: Duration = Duration::from_secs(30);

/// Every pooled handle of `graph`, taken once none is lent. A consumer of a fresh publication —
/// the search hook confirming which generation it serves — borrows one for a moment right after
/// the graph reads as ready, so a stand that must hold them all waits for that to pass.
pub(crate) fn hold_every_handle(graph: &GraphState) -> Vec<super::GraphSnapshot> {
    let deadline = std::time::Instant::now() + WAIT_CEILING;
    loop {
        let held: Vec<_> =
            std::iter::from_fn(|| graph.snapshot()).take(super::SNAPSHOT_POOL_CAP).collect();
        if held.len() == super::SNAPSHOT_POOL_CAP {
            return held;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the pool never had every handle idle: {}",
            graph_state_summary(graph)
        );
        drop(held);
        std::thread::sleep(WAIT_POLL);
    }
}

/// Everything a timed-out wait needs to say to be diagnosable: which condition it
/// waited on is the caller's half, the observed state is this one.
pub(crate) fn graph_state_summary(graph: &GraphState) -> String {
    let inner = lock_recover(&graph.inner);
    let published = inner.published.as_ref().map(|published| {
        format!(
            "generation {}, reload {} ({:?}), stale {}, force_stale {}",
            published.generation,
            published.reload.label(),
            published.reload,
            published.stale,
            published.force_stale
        )
    });
    format!(
        "status {:?}; published: {}; owed: change {:?}, forced {:?}, failed {}, marks {}, \
         recovery {}",
        inner.status,
        published.as_deref().unwrap_or("none"),
        graph.owes_change(),
        graph.owes_forced(),
        graph.owes_failed(),
        graph.owes_marks(),
        graph.owes_recovery(),
    )
}

/// Poll until `condition` holds, and on exhausting the ceiling fail with the state
/// actually observed instead of a bare "it did not happen".
///
/// Every bounded wait on graph state goes through here. A wait that reports only its
/// own name leaves a CI failure with nothing to diagnose — which is how the flake this
/// hardening came from arrived: a bare left/right, and no way to tell a slow machine
/// from a torn publication.
#[track_caller]
pub(crate) fn wait_until(graph: &GraphState, what: &str, condition: impl FnMut() -> bool) {
    wait_until_within(graph, WAIT_CEILING, what, condition);
}

/// [`wait_until`] with an explicit ceiling, for waits that legitimately need a
/// different one. The ceiling is a property of the wait, never of the assertion.
#[track_caller]
pub(crate) fn wait_until_within(
    graph: &GraphState,
    ceiling: Duration,
    what: &str,
    mut condition: impl FnMut() -> bool,
) {
    let deadline = std::time::Instant::now() + ceiling;
    loop {
        if condition() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {ceiling:?} waiting for {what}; {}",
            graph_state_summary(graph)
        );
        std::thread::sleep(WAIT_POLL);
    }
}

/// [`wait_until`] for a stand that has no watcher: `drive` plays it, before every look.
///
/// One decision is about one moment, and the moment a stand usually picks — right after
/// a publication it just waited for — is a busy one: the builder still holds its slot
/// while it arms the hub and runs the publish pass, and the lease may be re-read from
/// disk. A decision taken then is "not now", which in the daemon the watcher comes back
/// to revisit. Here nobody would, and the owed work would stand until the ceiling for
/// want of a second look — the slower the machine, the wider that window.
#[track_caller]
pub(crate) fn wait_until_driving(
    graph: &GraphState,
    what: &str,
    mut drive: impl FnMut(),
    mut condition: impl FnMut() -> bool,
) {
    wait_until(graph, what, || {
        if condition() {
            return true;
        }
        drive();
        false
    });
}

/// [`wait_until_driving`] with the graph's own [`GraphState::drive`] as the watcher.
#[track_caller]
pub(crate) fn drive_until(graph: &GraphState, what: &str, condition: impl FnMut() -> bool) {
    wait_until_driving(graph, what, || graph.drive(), condition);
}

/// Wait for the graph's STATUS to reach `Ready`.
///
/// This is a status barrier and nothing more. It is NOT a barrier for the publish pass
/// (`notify_published`), and three independent facts keep it that way:
///
/// 1. the pass runs with `inner` released on purpose — under the lock it would invert
///    the lock order against the search engine;
/// 2. real work sits between the status flip and the pass (`ensure_hub_roots` parses the
///    configuration and rediscovers roots), so the gap is wide, not instantaneous;
/// 3. `try_publish_stale_and_catch_up` sets `Ready` and deliberately never runs the pass
///    at all.
///
/// So a test that waits here and then asserts something a publish pass leaves behind —
/// a hook counter, a consumed mark, a refreshed root table — is asserting a value that
/// need not exist yet. Wait for the asserted quantity itself with [`wait_until`], or for
/// the whole pass with [`wait_publish_pass_within`].
pub(crate) fn wait_ready(graph: &GraphState) {
    wait_until(graph, "the graph to become ready", || match graph.status() {
        GraphStatus::Ready { .. } => true,
        GraphStatus::Failed(msg) => {
            panic!("graph load failed: {msg}; {}", graph_state_summary(graph))
        }
        _ => false,
    });
}

/// The status report of a published graph, with its published fields present.
///
/// The report samples its locks without blocking and, while any owner holds one — the
/// watcher, the publish pass, the hub's poll — leaves the published fields out. A single
/// read right after a publication is therefore a race; the wait keeps the sample that
/// carries them.
#[track_caller]
pub(crate) fn published_report(graph: &GraphState) -> super::GraphStatusReport {
    let mut sample = None;
    wait_until(graph, "a status report carrying the published revision", || {
        let report = graph.status_report();
        let published = report.revision.is_some();
        sample = Some(report);
        published
    });
    sample.expect("the wait captured a published report")
}

/// Wait until at least `passes` completed publish passes have been counted.
///
/// The barrier for everything `notify_published` leaves behind, including the part that
/// runs AFTER its hook returns: the ledger prune and the obligation it settles. Waiting on
/// the hook alone lands before them; waiting here lands after. `ceiling` is explicit: every
/// caller's setup makes the build legitimately slower than the default allows.
pub(crate) fn wait_publish_pass_within(graph: &GraphState, ceiling: Duration, passes: usize) {
    wait_until_within(graph, ceiling, &format!("{passes} completed publish pass(es)"), || {
        graph.publish_passes.load(std::sync::atomic::Ordering::SeqCst) >= passes
    });
}

/// A workspace with two declared extensions; `depends_on` toggles the one
/// dependency edge without touching any `.bsl`/`.xml` file.
pub(super) fn write_extension_workspace(root: &Path, depends_on: bool) {
    sample_workspace(root);
    // The base marker keeps the configured `root = "."` resolving to ROOT itself;
    // without it source discovery walks on and picks the first extension dir.
    write(root, "Configuration.xml", "<Configuration/>");
    write(root, "ext/a/Configuration.xml", "<Configuration/>");
    write(root, "ext/b/Configuration.xml", "<Configuration/>");
    write_extension_config(root, depends_on);
}

/// Rewrite ONLY the analyzer config — the drift under test must never come from
/// a scanned file's stat moving.
pub(super) fn write_extension_config(root: &Path, depends_on: bool) {
    let deps = if depends_on { ", dependsOn = [\"a\"]" } else { "" };
    fs::write(
        root.join("bsl-analyzer.toml"),
        format!(
            "[source]\nroot = \".\"\nextensions = [\n  \
             {{ name = \"a\", path = \"ext/a\" }},\n  \
             {{ name = \"b\", path = \"ext/b\"{deps} }},\n]\n"
        ),
    )
    .unwrap();
}

pub(super) fn seed_cache(root: &Path, fingerprint: crate::graph_db::GraphFp) {
    seed_cache_with_layout(
        root,
        &crate::cache::WorkspaceCacheLayout::for_workspace(root),
        fingerprint,
    );
}

pub(super) fn seed_cache_with_layout(
    root: &Path,
    cache: &crate::cache::WorkspaceCacheLayout,
    fingerprint: crate::graph_db::GraphFp,
) {
    let out = cache.graph_db_path();
    fs::create_dir_all(out.parent().unwrap()).unwrap();
    let excluded = cache.exclusions(root);
    let project = crate::graph::ProjectSnapshot::load_excluding(root, &excluded);
    let universe = crate::graph::universe::ScannedUniverse::scan_excluding(
        &project.scan_roots,
        &project.excluded,
    );
    build_graph_database(
        &project,
        &universe,
        &out,
        GRAPH_BUILD_BATCH,
        &crate::graph_db::GraphMeta {
            revision: 7,
            fingerprint,
            files: 0,
            built_at: "cached-build-sentinel".to_string(),
            publication_id: "test-1".to_owned(),
        },
    )
    .expect("seed cache builds");
}

pub(crate) fn meta_string(path: &Path, key: &str) -> String {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| row.get(0))
        .unwrap()
}

/// A workspace graph with a change hub and a running drift watcher — the graph every workspace
/// boot builds. A graph nobody watches reports itself stale, so a test about any OTHER reason
/// for staleness starts from this one.
pub(crate) fn watched_graph(
    root: &Path,
) -> (GraphState, crate::change_hub::WorkspaceChangeHub, crate::state::OwnerStop) {
    let hub = workspace_hub(root);
    assert!(hub.wait_until_watching(Duration::from_secs(5)), "the hub did not arm");
    let graph = GraphState::for_workspace(root.to_path_buf()).with_change_hub(hub.clone());
    let stop = crate::state::OwnerStop::default();
    assert!(super::watcher::start(&graph, &hub, None, stop.clone()));
    wait_until(&graph, "the watcher to run", || {
        graph.watch_state().0 == super::watcher::WatchPhase::Running
    });
    (graph, hub, stop)
}

/// Wait until the hub's sequence has moved past `floor`, and return it.
///
/// A delivered write becomes a hub fact asynchronously; a test that needs the fact NUMBER has
/// to wait for the number, not for a duration.
pub(crate) fn wait_for_hub_seq_above(
    hub: &crate::change_hub::WorkspaceChangeHub,
    floor: u64,
) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let seq = hub.seq();
        if seq > floor {
            return seq;
        }
        assert!(std::time::Instant::now() < deadline, "the hub never saw the write");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The workspace graph's first build, held by a test.
///
/// The claim is production's own single flight ([`GraphState::try_begin_external_build`]): while
/// it is held, every other claimant — the boot's fused pass, a request, the drift watcher — is
/// refused exactly as it refuses whichever claimant loses the race. So a source is unconsulted
/// here because nothing may build it, not because no build has got there yet, and the verdict
/// holds for as long as the hold does instead of for as long as the machine is slow. Dropped, it
/// returns the graph to `Idle` and the ordinary lifecycle owns the build again.
pub(crate) struct HeldFirstBuild {
    graph: GraphState,
}

impl HeldFirstBuild {
    /// Whether the graph is still what this hold makes it: claimed, and nothing published.
    pub(crate) fn holds_unconsulted(&self) -> bool {
        matches!(self.graph.status(), GraphStatus::Loading) && self.graph.snapshot().is_none()
    }
}

impl Drop for HeldFirstBuild {
    fn drop(&mut self) {
        self.graph.abort_external_build();
    }
}

thread_local! {
    /// Armed by [`holding_the_first_build`] for the boot that runs on this thread. The graph is
    /// created deep inside that boot, where no parameter reaches, and the claim has to be taken
    /// there: by the time the constructor returns, the search init it spawned is already racing
    /// for the same claim.
    static FIRST_BUILD_HOLD: std::cell::RefCell<Option<Option<HeldFirstBuild>>> =
        const { std::cell::RefCell::new(None) };
}

/// Taken by the boot, once, as soon as the workspace graph exists.
pub(crate) fn hold_the_first_build(graph: &GraphState) {
    FIRST_BUILD_HOLD.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(held) = slot.as_mut() else { return };
        if held.is_none() {
            *held =
                graph.try_begin_external_build().then(|| HeldFirstBuild { graph: graph.clone() });
        }
    });
}

/// Run `boot` with this thread armed to hold its graph's first build from the moment the graph
/// exists. The whole construction is the argument so that nothing can take the claim between
/// arming and booting.
pub(crate) fn holding_the_first_build<T>(boot: impl FnOnce() -> T) -> (T, HeldFirstBuild) {
    FIRST_BUILD_HOLD.with(|slot| {
        let mut slot = slot.borrow_mut();
        // One boot, one reservation. A nested one took the outer boot's slot and the outer
        // boot then found its own reservation gone, far from the call that took it.
        assert!(slot.is_none(), "holding_the_first_build does not nest");
        *slot = Some(None);
    });
    let booted = boot();
    let held = FIRST_BUILD_HOLD.with(|slot| slot.borrow_mut().take()).flatten();
    (booted, held.expect("the boot reached its graph with the first build free to claim"))
}
