use std::{
    cell::RefCell,
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::Instant,
};

use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender, TrySendError};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use paths::{AbsPath, AbsPathBuf, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::loader::{self, LoadingProgress};
use walkdir::WalkDir;

const LOADED_CHUNK_BYTES: usize = 64 * 1024 * 1024;

const WATCH_ONLY_CHUNK_PATHS: usize = 4096;

/// Env override for the directory-scan reader pool size (see [`reader_count`]).
const READERS_ENV: &str = "BSL_VFS_READERS";

/// Bound on paths queued to the reader pool. A path is a cheap `AbsPathBuf`; a
/// generous queue keeps readers fed past short walk stalls without unbounded
/// memory. The loader drains a result whenever this fills (backpressure).
const READER_PATHS_BOUND: usize = 512;

/// Bound on read results buffered back from the pool. Each is `(path, bytes)`;
/// at ~57 KB average that caps in-flight read memory at ~15 MB plus outliers,
/// on top of the single 64 MB chunk buffer the loader accumulates.
const READER_RESULTS_BOUND: usize = 256;

/// Number of reader threads for a directory scan: `available_parallelism`
/// clamped to `[2, 8]`, overridable via `BSL_VFS_READERS`. These are dedicated
/// `std`/`stdx` threads, never the global rayon pool — a pool thread parked on
/// the bounded loader channel is a work-stealing deadlock window (see the
/// comment in `NotifyActor::run`).
fn reader_count() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        if let Ok(raw) = std::env::var(READERS_ENV) {
            match raw.parse::<usize>() {
                Ok(parsed) => {
                    let clamped = parsed.clamp(1, 32);
                    tracing::info!(readers = clamped, raw = %raw, "Resolved BSL_VFS_READERS");
                    return clamped;
                }
                Err(_) => tracing::warn!(
                    raw = %raw,
                    "BSL_VFS_READERS is not a valid usize; falling back to default",
                ),
            }
        }
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 8)
    })
}

#[derive(Debug)]
pub struct NotifyHandle {
    sender: Sender<Message>,
    shutdown: Arc<AtomicBool>,
    _thread: stdx::thread::JoinHandle,
}

impl Drop for NotifyHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
enum Message {
    Config(loader::Config),
    Invalidate(AbsPathBuf),
}

impl loader::Handle for NotifyHandle {
    fn spawn(sender: loader::Sender) -> NotifyHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let actor = NotifyActor::new(sender, Arc::clone(&shutdown));
        let (sender, receiver) = unbounded::<Message>();
        let thread = stdx::thread::Builder::new(stdx::thread::ThreadIntent::Worker, "VfsLoader")
            .spawn(move || actor.run(receiver))
            .expect("failed to spawn thread");
        NotifyHandle { sender, shutdown, _thread: thread }
    }

    fn set_config(&mut self, config: loader::Config) {
        self.sender.send(Message::Config(config)).unwrap();
    }

    fn invalidate(&mut self, path: AbsPathBuf) {
        self.sender.send(Message::Invalidate(path)).unwrap();
    }

    fn load_sync(&mut self, path: &AbsPath) -> Option<Vec<u8>> {
        read(path)
    }
}

type NotifyEvent = notify::Result<notify::Event>;

/// A live watcher together with the registrations standing on it. One value because
/// they share a fate: a registration exists only for as long as the watcher holding
/// it, so a dropped watcher must take every record with it.
struct WatchState {
    watcher: RecommendedWatcher,
    registered: FxHashMap<PathBuf, RecursiveMode>,
}

struct NotifyActor {
    sender: loader::Sender,
    shutdown: Arc<AtomicBool>,
    watched_file_entries: FxHashSet<AbsPathBuf>,
    watched_only_file_entries: FxHashSet<AbsPathBuf>,
    watched_dir_entries: Vec<loader::Directories>,
    /// Behind a `RefCell` because registration happens from inside the scan, where
    /// the actor is borrowed immutably by the closures reporting progress — and
    /// registering as the scan reaches each path is the whole point: a path
    /// registered afterwards is blind to everything written in between.
    watcher: Option<(RefCell<WatchState>, Receiver<NotifyEvent>)>,
}

#[derive(Debug)]
enum Event {
    Message(Message),
    NotifyEvent(NotifyEvent),
}

/// What a single path from a filesystem event should produce, derived from the
/// path's current on-disk state plus the watch configuration. A REMOVED path
/// (no longer stat-able) is still classified by path so its deletion is
/// delivered instead of being silently dropped.
#[derive(Debug, PartialEq, Eq)]
enum EventPathAction {
    /// Not watched, or an irrelevant directory event — ignore.
    Ignore,
    /// A directory under a watched root appeared — start watching it.
    WatchDir,
    /// A watched content file that exists — (re)load its bytes.
    LoadContent,
    /// A watched watch-only file changed (exists or removed) — signal only.
    WatchOnly,
    /// A watched content file was removed — deliver as a deletion.
    Delete,
    /// A path under a watched root was removed but is not a watched file — most
    /// likely a removed directory whose subtree must be expanded by the consumer
    /// (the loader cannot enumerate children that no longer exist on disk).
    DeleteSubtree,
}

impl NotifyActor {
    fn new(sender: loader::Sender, shutdown: Arc<AtomicBool>) -> NotifyActor {
        NotifyActor {
            sender,
            shutdown,
            watched_dir_entries: Vec::new(),
            watched_file_entries: FxHashSet::default(),
            watched_only_file_entries: FxHashSet::default(),
            watcher: None,
        }
    }

    fn next_event(&self, receiver: &Receiver<Message>) -> Option<Event> {
        let Some((_, watcher_receiver)) = &self.watcher else {
            return receiver.recv().ok().map(Event::Message);
        };

        select! {
            recv(receiver) -> it => it.ok().map(Event::Message),
            recv(watcher_receiver) -> it => Some(Event::NotifyEvent(it.unwrap())),
        }
    }

    fn run(mut self, inbox: Receiver<Message>) {
        while let Some(event) = self.next_event(&inbox) {
            tracing::debug!(?event, "vfs-notify event");
            match event {
                Event::Message(msg) => match msg {
                    Message::Config(config) => {
                        // The watcher SURVIVES a reconfiguration. Baseline object names
                        // carry a content hash, so every baseline write changes the list
                        // of watched paths and brings another config here; taking the
                        // watcher down to build a new one leaves the whole tree
                        // unobserved until the registrations are placed again. On
                        // FSEvents the cost is not even confined to that gap: `watch`
                        // does not extend a running stream, it rebuilds one starting
                        // from "now", so whatever happened during the swap is reported
                        // to nobody, ever.
                        if config.watch.is_empty() {
                            // Nothing is to be observed after this config, so there is
                            // no coverage to lose by dropping the watcher.
                            self.watcher = None;
                        } else if self.watcher.is_none() {
                            let (watcher_sender, watcher_receiver) = unbounded();
                            let watcher = log_notify_error(RecommendedWatcher::new(
                                move |event| {
                                    _ = watcher_sender.send(event);
                                },
                                Config::default(),
                            ));
                            self.watcher = watcher.map(|it| {
                                let state =
                                    WatchState { watcher: it, registered: FxHashMap::default() };
                                (RefCell::new(state), watcher_receiver)
                            });
                        }

                        let config_version = config.version;

                        // Filled BEFORE the load pass instead of from its outcome:
                        // `watch_target` reads these sets to decide what to register for
                        // a path, and the pass now registers each path as it reaches it.
                        self.watched_dir_entries.clear();
                        self.watched_file_entries.clear();
                        self.watched_only_file_entries.clear();
                        for (i, entry) in config.load.iter().enumerate() {
                            if !config.watch.contains(&i) {
                                continue;
                            }
                            match entry {
                                loader::Entry::Files(files) => {
                                    self.watched_file_entries.extend(files.iter().cloned())
                                }
                                loader::Entry::WatchOnlyFiles(files) => {
                                    self.watched_only_file_entries.extend(files.iter().cloned())
                                }
                                loader::Entry::Directories(dirs) => {
                                    self.watched_dir_entries.push(dirs.clone())
                                }
                            }
                        }

                        self.release_watches_no_longer_requested();

                        self.send(loader::Message::Progress {
                            n_total: 0,
                            n_done: LoadingProgress::Scanning,
                            config_version,
                            dir: None,
                        });

                        if self.shutdown.load(Ordering::Relaxed) {
                            continue;
                        }

                        let count_start = Instant::now();
                        let n_total: usize = config
                            .load
                            .iter()
                            .map(|e| Self::count_files_in_entry(e, self.shutdown.as_ref()))
                            .sum();
                        if self.shutdown.load(Ordering::Relaxed) {
                            tracing::debug!("count pass aborted by shutdown latch");
                            continue;
                        }
                        tracing::info!(
                            n_total,
                            elapsed_ms = count_start.elapsed().as_millis() as u64,
                            "vfs: count pass complete",
                        );

                        self.send(loader::Message::Progress {
                            n_total,
                            n_done: LoadingProgress::Started,
                            config_version,
                            dir: None,
                        });

                        let processed = AtomicUsize::new(0);
                        let last_reported = AtomicUsize::new(0);
                        const PROGRESS_BATCH_SIZE: usize = 50;

                        let load_start = Instant::now();
                        let shutdown: &AtomicBool = self.shutdown.as_ref();
                        // Entries load sequentially on this dedicated thread, NOT
                        // on the global rayon pool. These loads block on the
                        // bounded loader channel (backpressure against the
                        // consumer's event loop), and a pool thread parked in such
                        // a send is a deadlock window: a rayon wait elsewhere in
                        // the process (e.g. the metadata loader's scope inside a
                        // Salsa query) can steal the load task and park a thread
                        // whose db clone that same event loop is waiting on. There
                        // are only one or two entries (the workspace walk plus
                        // config files), so entry-level parallelism bought nothing.
                        for (i, entry) in config.load.into_iter().enumerate() {
                            if shutdown.load(Ordering::Relaxed) {
                                break;
                            }
                            let do_watch = config.watch.contains(&i);
                            Self::load_entry(
                                |path| self.watch(path),
                                entry,
                                do_watch,
                                |file| {
                                    self.send(loader::Message::Progress {
                                        n_total,
                                        n_done: LoadingProgress::Progress(
                                            processed.load(std::sync::atomic::Ordering::Relaxed),
                                        ),
                                        dir: Some(file),
                                        config_version,
                                    });
                                },
                                || {
                                    let current = processed
                                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                                        + 1;
                                    let last =
                                        last_reported.load(std::sync::atomic::Ordering::Relaxed);

                                    if (current - last >= PROGRESS_BATCH_SIZE || current == n_total)
                                        && last_reported
                                            .compare_exchange(
                                                last,
                                                current,
                                                std::sync::atomic::Ordering::AcqRel,
                                                std::sync::atomic::Ordering::Relaxed,
                                            )
                                            .is_ok()
                                    {
                                        self.send(loader::Message::Progress {
                                            n_total,
                                            n_done: LoadingProgress::Progress(current),
                                            config_version,
                                            dir: None,
                                        });
                                    }
                                },
                                |files| self.send(loader::Message::Loaded { files }),
                                LOADED_CHUNK_BYTES,
                                |files| self.send(loader::Message::WatchOnly { files }),
                                WATCH_ONLY_CHUNK_PATHS,
                                shutdown,
                            );
                        }

                        tracing::info!(
                            n_total,
                            elapsed_ms = load_start.elapsed().as_millis() as u64,
                            watch_targets = self
                                .watcher
                                .as_ref()
                                .map_or(0, |(state, _)| state.borrow().registered.len()),
                            "vfs: parallel read pass complete, sending LoadingProgress::Finished",
                        );
                        // LAST. The consumer reads this as "the tree is observed from
                        // here on", and a write landing before the registrations are in
                        // place produces no event at all — not a late one.
                        self.send(loader::Message::Progress {
                            n_total,
                            n_done: LoadingProgress::Finished,
                            config_version,
                            dir: None,
                        });
                    }
                    Message::Invalidate(path) => {
                        let contents = read(path.as_path());
                        let files = vec![(path, contents)];
                        self.send(loader::Message::Changed { files });
                    }
                },
                Event::NotifyEvent(event) => {
                    if let Some(event) = log_notify_error(event) {
                        if matches!(
                            event.kind,
                            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                        ) {
                            let mut changed_files: Vec<(AbsPathBuf, Option<Vec<u8>>)> = Vec::new();
                            let mut watch_only_files: Vec<AbsPathBuf> = Vec::new();
                            let mut removed_recursive: Vec<AbsPathBuf> = Vec::new();
                            let appeared = matches!(
                                event.kind,
                                EventKind::Create(_)
                                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
                            );
                            for path in event.paths {
                                let Some(path) = Utf8PathBuf::from_path_buf(path)
                                    .ok()
                                    .and_then(|p| AbsPathBuf::try_from(p).ok())
                                else {
                                    continue;
                                };
                                self.release_watch_if_gone(&path);
                                match self.classify_event_path(&path) {
                                    EventPathAction::WatchDir => {
                                        if self.takes_dir(&path) {
                                            self.watch(path.as_ref());
                                        } else {
                                            for root in self.include_roots_below(&path) {
                                                self.watch(root.as_ref());
                                            }
                                        }
                                        // Only a directory that appeared holds files no
                                        // event will name; a chmod or touch on one that
                                        // stood there all along must not re-read it.
                                        if appeared {
                                            self.scan_appeared_dir(&path);
                                        }
                                    }
                                    EventPathAction::LoadContent => {
                                        let contents = read(&path);
                                        changed_files.push((path, contents));
                                    }
                                    EventPathAction::WatchOnly => watch_only_files.push(path),
                                    // Deliver the removal as a deletion (`None`
                                    // contents) so the consumer can tombstone it.
                                    EventPathAction::Delete => changed_files.push((path, None)),
                                    EventPathAction::DeleteSubtree => removed_recursive.push(path),
                                    EventPathAction::Ignore => {}
                                }
                            }
                            if !changed_files.is_empty() {
                                self.send(loader::Message::Changed { files: changed_files });
                            }
                            if !watch_only_files.is_empty() {
                                self.send(loader::Message::WatchOnly { files: watch_only_files });
                            }
                            if !removed_recursive.is_empty() {
                                self.send(loader::Message::RemovedRecursive {
                                    paths: removed_recursive,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn load_entry(
        mut watch: impl FnMut(&Path),
        entry: loader::Entry,
        do_watch: bool,
        send_message: impl Fn(AbsPathBuf),
        on_file_loaded: impl Fn(),
        send_loaded: impl Fn(Vec<(AbsPathBuf, Option<Vec<u8>>)>),
        chunk_threshold: usize,
        send_watch_only: impl Fn(Vec<AbsPathBuf>),
        watch_chunk_paths: usize,
        shutdown: &AtomicBool,
    ) {
        let mut loaded_buf: Vec<(AbsPathBuf, Option<Vec<u8>>)> = Vec::new();
        let mut loaded_bytes: usize = 0;
        let mut watch_only_buf: Vec<AbsPathBuf> = Vec::new();

        let mut push_loaded = |file: AbsPathBuf, contents: Option<Vec<u8>>| {
            loaded_bytes += contents.as_ref().map(|c| c.len()).unwrap_or(0);
            loaded_buf.push((file, contents));
            if loaded_bytes >= chunk_threshold {
                send_loaded(std::mem::take(&mut loaded_buf));
                loaded_bytes = 0;
            }
        };

        let mut push_watch_only = |file: AbsPathBuf| {
            watch_only_buf.push(file);
            if watch_only_buf.len() >= watch_chunk_paths {
                send_watch_only(std::mem::take(&mut watch_only_buf));
            }
        };

        match entry {
            loader::Entry::Files(files) => {
                for file in files {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    if do_watch {
                        watch(file.as_ref());
                    }
                    let contents = read(file.as_path());
                    on_file_loaded();
                    push_loaded(file, contents);
                }
            }
            loader::Entry::WatchOnlyFiles(files) => {
                for file in files {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    if do_watch {
                        watch(file.as_ref());
                    }
                    on_file_loaded();
                    push_watch_only(file);
                }
            }
            loader::Entry::Directories(dirs) => {
                // A scan-owned pool of dedicated reader threads turns the
                // sequential per-file `read` into parallel I/O. The walk stays on
                // this loader thread (it warms dentries and the mixed
                // walk+read pipeline is walk-bound); only the `read` syscalls fan
                // out. The pool is created here and joined before this arm
                // returns, so readers never outlive their scan — a repeat
                // `Message::Config` (processed only after this returns) and a
                // shutdown both leave no stragglers.
                let readers = reader_count();
                let (paths_tx, paths_rx) = bounded::<AbsPathBuf>(READER_PATHS_BOUND);
                let (results_tx, results_rx) =
                    bounded::<(AbsPathBuf, Option<Vec<u8>>)>(READER_RESULTS_BOUND);
                let mut handles = Vec::with_capacity(readers);
                for i in 0..readers {
                    let paths_rx = paths_rx.clone();
                    let results_tx = results_tx.clone();
                    let handle = stdx::thread::Builder::new(
                        stdx::thread::ThreadIntent::Worker,
                        format!("VfsReader{i}"),
                    )
                    .spawn(move || {
                        // Read until the loader drops `paths_tx` (scan done) or the
                        // results channel disconnects (loader gone). No other
                        // side effects: the reader never touches the loader
                        // channel, only this private results channel.
                        while let Ok(path) = paths_rx.recv() {
                            let contents = read(path.as_path());
                            if results_tx.send((path, contents)).is_err() {
                                break;
                            }
                        }
                    })
                    .expect("failed to spawn vfs reader thread");
                    handles.push(handle);
                }
                // The loader keeps only the producing end of `paths` and the
                // consuming end of `results`; dropping the others lets the
                // channels disconnect once the readers exit, terminating the
                // drain loop below.
                drop(paths_rx);
                drop(results_tx);

                let mut submitted: usize = 0;
                let mut delivered: usize = 0;

                'walk: for root in &dirs.include {
                    if shutdown.load(Ordering::Relaxed) {
                        break 'walk;
                    }
                    if dirs.is_hard_excluded(root) {
                        continue;
                    }
                    if do_watch {
                        watch(root.as_ref());
                    }

                    send_message(root.clone());
                    let mut resolved = stdx::path_exclusion::ResolvedDirs::default();
                    let walkdir =
                        WalkDir::new(root).follow_links(true).into_iter().filter_entry(|entry| {
                            if shutdown.load(Ordering::Relaxed) {
                                return false;
                            }
                            let path = entry.path();
                            if !entry.file_type().is_dir() {
                                // A file symlink may point into an exclusion.
                                if entry.path_is_symlink() {
                                    return !dirs.hard_exclude.prunes_walked_dir(
                                        path,
                                        true,
                                        &mut resolved,
                                    );
                                }
                                return !dirs.hard_exclude.excludes_walked_file(path);
                            }

                            if entry.path_is_symlink() && symlink_might_be_cyclic(path) {
                                return false;
                            }

                            dirs.exclude.iter().all(|it| it != path)
                                && (root == path || dirs.include.iter().all(|it| it != path))
                                && !dirs.hard_exclude.prunes_walked_dir(
                                    path,
                                    entry.path_is_symlink(),
                                    &mut resolved,
                                )
                        });

                    let files = walkdir.filter_map(|it| it.ok()).filter_map(|entry| {
                        let depth = entry.depth();
                        let is_dir = entry.file_type().is_dir();
                        let is_file = entry.file_type().is_file();
                        let abs_path = AbsPathBuf::try_from(
                            Utf8PathBuf::from_path_buf(entry.into_path()).ok()?,
                        )
                        .ok()?;
                        if depth < 2 && is_dir {
                            send_message(abs_path.clone());
                        }
                        if !is_file {
                            return None;
                        }
                        let ext = abs_path.extension().unwrap_or_default();
                        if dirs.extensions.iter().any(|it| it.eq_ignore_ascii_case(ext)) {
                            return Some((abs_path, loader::LoadMode::LoadContent));
                        }
                        for rule in &dirs.rules {
                            if rule.extensions.iter().any(|it| it.eq_ignore_ascii_case(ext)) {
                                return Some((abs_path, rule.load_mode));
                            }
                        }
                        None
                    });

                    for (file, mode) in files {
                        if shutdown.load(Ordering::Relaxed) {
                            break 'walk;
                        }
                        match mode {
                            loader::LoadMode::LoadContent => {
                                // Hand the path to the pool. On a full queue, the
                                // loader (not a reader) drains one finished read —
                                // this is the only place `push_loaded` runs, so
                                // the blocking loader-channel send always stays on
                                // this thread, and the in-flight memory is bounded.
                                let mut file = file;
                                loop {
                                    match paths_tx.try_send(file) {
                                        Ok(()) => {
                                            submitted += 1;
                                            break;
                                        }
                                        Err(TrySendError::Full(f)) => {
                                            match results_rx.recv() {
                                                Ok((path, contents)) => {
                                                    on_file_loaded();
                                                    push_loaded(path, contents);
                                                    delivered += 1;
                                                }
                                                // Readers vanished — abandon the
                                                // scan; the drain below joins them.
                                                Err(_) => break 'walk,
                                            }
                                            file = f;
                                        }
                                        Err(TrySendError::Disconnected(_)) => break 'walk,
                                    }
                                }
                            }
                            loader::LoadMode::WatchOnly => {
                                on_file_loaded();
                                push_watch_only(file);
                            }
                        }
                    }
                }

                // Stop submitting and drain every in-flight read. Draining to
                // disconnect (not to a submitted==delivered count) keeps a reader
                // blocked on a full results channel from hanging the join on an
                // aborted scan; once `paths_tx` is dropped the readers finish the
                // queue, emit their last results, and exit.
                drop(paths_tx);
                while let Ok((path, contents)) = results_rx.recv() {
                    on_file_loaded();
                    push_loaded(path, contents);
                    delivered += 1;
                }
                debug_assert_eq!(submitted, delivered, "every queued read must be delivered once");
                for handle in handles {
                    let () = handle.join();
                }
            }
        }

        let aborted = shutdown.load(Ordering::Relaxed);
        if !loaded_buf.is_empty() && !aborted {
            send_loaded(loaded_buf);
        }
        if !watch_only_buf.is_empty() && !aborted {
            send_watch_only(watch_only_buf);
        }
    }

    /// Decide what a single changed path should produce, tolerating removals.
    /// Existing paths are routed by their on-disk type; a path that no longer
    /// exists (a removal) is classified by path alone — a watched content file
    /// becomes a [`EventPathAction::Delete`] (so the consumer tombstones it)
    /// rather than being dropped because `fs::metadata` failed.
    ///
    /// Known limitation: a *coalesced* directory removal (where a watch backend
    /// reports only the directory path, not each child) classifies as `Ignore` —
    /// the directory has no extension, so its descendants are not individually
    /// tombstoned. On inotify (Linux) each child file under a recursive watch
    /// emits its own removal event, which IS handled, so this only affects
    /// backends that collapse a subtree delete into one directory event.
    fn classify_event_path(&self, path: &AbsPathBuf) -> EventPathAction {
        if self.watched_only_file_entries.contains(path) {
            return EventPathAction::WatchOnly;
        }
        // Symlinks are followed here, as the initial scan follows them (`follow_links`):
        // a `.bsl` reached through a link is loaded at startup, so its later edits must
        // arrive too. Baseline files, the reason the type is inspected at all, are
        // answered by the exact-match check above and never reach this point.
        match fs::metadata(path).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Err(error)
            } else {
                fs::symlink_metadata(path)
            }
        }) {
            Ok(meta) => {
                if meta.file_type().is_dir() {
                    // A directory the configuration does not take can still hold an include
                    // root of its own — a source root under a legacy exclude — which the
                    // load walks separately and which must not be lost when the tree
                    // around it appears.
                    if self.takes_dir(path) || !self.include_roots_below(path).is_empty() {
                        return EventPathAction::WatchDir;
                    }
                    return EventPathAction::Ignore;
                }
                if !meta.file_type().is_file() {
                    return EventPathAction::Ignore;
                }
                // A link created under a watched directory may lead into an exclusion.
                let is_link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
                if is_link && self.is_hard_excluded(path.as_ref()) {
                    return EventPathAction::Ignore;
                }
                match self.classify_watched_path(path) {
                    Some(loader::LoadMode::LoadContent) => EventPathAction::LoadContent,
                    Some(loader::LoadMode::WatchOnly) => EventPathAction::WatchOnly,
                    None => EventPathAction::Ignore,
                }
            }
            // Only an actual absence (`NotFound`) is a removal. A transient stat
            // error (permissions, interrupted, a momentary race) must NOT tombstone
            // an existing file, so anything else is ignored and left as-is.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                match self.classify_watched_path(path) {
                    Some(loader::LoadMode::LoadContent) => EventPathAction::Delete,
                    Some(loader::LoadMode::WatchOnly) => EventPathAction::WatchOnly,
                    // An extension-less path under a watched root that is gone is
                    // most likely a removed directory; hand it to the consumer to
                    // expand into its loaded descendants. (Removed unwatched FILES
                    // have an extension and are ignored — nothing tracks them.)
                    None if path.extension().is_none() && self.within_watched_root(path) => {
                        EventPathAction::DeleteSubtree
                    }
                    None => EventPathAction::Ignore,
                }
            }
            Err(_) => EventPathAction::Ignore,
        }
    }

    /// Whether `path` lies under one of the watched directory roots (used to
    /// recognize a removed directory, which has no extension to classify by).
    fn within_watched_root(&self, path: &AbsPathBuf) -> bool {
        self.watched_dir_entries.iter().any(|dir| dir.contains_dir(path))
    }

    fn classify_watched_path(&self, path: &AbsPathBuf) -> Option<loader::LoadMode> {
        if self.watched_file_entries.contains(path) {
            return Some(loader::LoadMode::LoadContent);
        }
        if self.watched_only_file_entries.contains(path) {
            return Some(loader::LoadMode::WatchOnly);
        }
        let mut found: Option<loader::LoadMode> = None;
        for dir in &self.watched_dir_entries {
            if let Some(m) = dir.classify_file(path) {
                match m {
                    loader::LoadMode::LoadContent => {
                        return Some(loader::LoadMode::LoadContent);
                    }
                    loader::LoadMode::WatchOnly => {
                        found.get_or_insert(loader::LoadMode::WatchOnly);
                    }
                }
            }
        }
        found
    }

    fn count_files_in_entry(entry: &loader::Entry, cancel: &AtomicBool) -> usize {
        match entry {
            loader::Entry::Files(files) | loader::Entry::WatchOnlyFiles(files) => files.len(),
            loader::Entry::Directories(dirs) => {
                let roots: Vec<&std::path::Path> =
                    dirs.include.iter().map(|p| p.as_path().as_ref()).collect();
                let excludes: Vec<&std::path::Path> =
                    dirs.exclude.iter().map(|p| p.as_path().as_ref()).collect();
                let mut extensions: Vec<&str> =
                    dirs.extensions.iter().map(|s| s.as_str()).collect();
                for rule in &dirs.rules {
                    for ext in &rule.extensions {
                        let s = ext.as_str();
                        if !extensions.contains(&s) {
                            extensions.push(s);
                        }
                    }
                }

                stdx::fs::parallel_count_cancellable(
                    &roots,
                    &stdx::fs::WalkConfig {
                        extensions: &extensions,
                        excludes: &excludes,
                        excluded: Some(&dirs.hard_exclude),
                        follow_links: true,
                    },
                    Some(cancel),
                )
            }
        }
    }

    /// What to hand `notify` for `path`, or `None` when it is already covered.
    ///
    /// A single file cannot be watched on its own, so an exactly-listed one is watched
    /// through its directory. That directory must NOT be registered non-recursively when
    /// a recursive watch over it already exists: `notify` replaces the mode of a path it
    /// already watches, so the narrower registration would silently drop events for
    /// every subdirectory — and the baseline lives in the project root by default,
    /// which is exactly such a directory. Events for the file arrive through the
    /// recursive watch anyway; `classify_event_path` recognises it by its own list.
    ///
    /// A directory with a hard exclusion anywhere beneath it — existing or not yet
    /// created — is watched non-recursively: a recursive watch would reach into the
    /// exclusion. [`Self::watch`] then covers its permitted children one by one.
    fn watch_target(&self, path: &Path) -> Option<(std::path::PathBuf, RecursiveMode)> {
        if self.is_hard_excluded(path) {
            return None;
        }
        let exact = self
            .watched_file_entries
            .iter()
            .chain(&self.watched_only_file_entries)
            .any(|entry| entry.as_path() == path);
        if !exact {
            return Some((path.to_path_buf(), self.directory_mode(path)));
        }
        let parent = path.parent().unwrap_or(path);
        let covered = Utf8PathBuf::from_path_buf(parent.to_path_buf())
            .ok()
            .and_then(|parent| AbsPathBuf::try_from(parent).ok())
            .is_some_and(|parent| {
                self.watched_dir_entries.iter().any(|dir| dir.contains_dir(&parent))
            });
        (!covered).then(|| (parent.to_path_buf(), RecursiveMode::NonRecursive))
    }

    /// Register `path` with the running watcher unless a registration already
    /// standing covers it.
    ///
    /// The skip needs no platform gate, unlike the coverage question the workspace
    /// change hub answers per backend: what is skipped here is a target THIS actor
    /// registered and never released, so the registration is in place on every
    /// backend and re-issuing it can only cost. On FSEvents it costs the tree —
    /// `watch` rebuilds the single stream and starts it from "now".
    ///
    /// A directory narrowed to a non-recursive watch because of an exclusion below it
    /// replaces any recursive registration it had — the one case where a registration
    /// already standing does not answer — and its permitted child directories are
    /// watched in turn, each again recursively unless an exclusion lies below it too.
    /// Only the branches leading to an exclusion are descended; the excluded directory
    /// itself is never listed.
    fn watch(&self, path: &Path) {
        self.watch_visiting(path, &mut FxHashSet::default());
    }

    /// [`Self::watch`], remembering in `visited` the resolved directories a narrowed
    /// descent has entered: a symlink back into one of them — a loop — is not entered
    /// again.
    fn watch_visiting(&self, path: &Path, visited: &mut FxHashSet<PathBuf>) {
        let Some((target, mode)) = self.watch_target(path) else { return };
        let Some((state, _)) = &self.watcher else { return };
        let narrowed = mode == RecursiveMode::NonRecursive && self.has_exclusion_below(&target);
        if narrowed && !visited.insert(fs::canonicalize(&target).unwrap_or_else(|_| target.clone()))
        {
            return;
        }
        {
            let mut state = state.borrow_mut();
            let stands = if narrowed {
                state.registered.get(&target) == Some(&RecursiveMode::NonRecursive)
            } else {
                registration_stands(&state.registered, &target, mode)
            };
            if !stands && log_notify_error(state.watcher.watch(&target, mode)).is_some() {
                state.registered.insert(target.clone(), mode);
            }
        }
        if narrowed {
            let Ok(entries) = fs::read_dir(&target) else { return };
            for entry in entries.flatten() {
                let child = entry.path();
                if fs::metadata(&child).is_ok_and(|meta| meta.is_dir())
                    && !self.is_hard_excluded(&child)
                {
                    self.watch_visiting(&child, visited);
                }
            }
        }
    }

    /// Deliver what a directory that just appeared under a watched root already holds.
    ///
    /// A tree created elsewhere and moved in arrives as one event for its top: the files
    /// inside it were never created here, so no event will ever name them. The walk
    /// prunes exactly what the load does — a directory the configuration does not take,
    /// a hard exclusion under any spelling, a link that may lead back into itself, an
    /// include root of its own, which is walked separately as the load walks it — and
    /// classifies each file the way an event for it would be. Delivery is chunked like
    /// the load's and stops with it on shutdown: a moved-in tree can be a whole
    /// configuration.
    fn scan_appeared_dir(&self, dir: &AbsPathBuf) {
        let mut starts = if self.takes_dir(dir) { vec![dir.clone()] } else { Vec::new() };
        for include in self.include_roots_below(dir) {
            if !starts.contains(&include) {
                starts.push(include);
            }
        }
        let mut changed: Vec<(AbsPathBuf, Option<Vec<u8>>)> = Vec::new();
        let mut changed_bytes = 0usize;
        let mut watch_only: Vec<AbsPathBuf> = Vec::new();
        for start in &starts {
            let mut resolved = stdx::path_exclusion::ResolvedDirs::default();
            let walk = WalkDir::new(start).follow_links(true).into_iter().filter_entry(|entry| {
                if self.shutdown.load(Ordering::Relaxed) {
                    return false;
                }
                let path = entry.path();
                if entry.depth() == 0 {
                    return !self.watched_dir_entries.iter().any(|dirs| {
                        dirs.hard_exclude.prunes_walked_dir(
                            path,
                            entry.path_is_symlink(),
                            &mut resolved,
                        )
                    });
                }
                let is_dir = entry.file_type().is_dir();
                if !is_dir && !entry.path_is_symlink() {
                    return !self
                        .watched_dir_entries
                        .iter()
                        .any(|dirs| dirs.hard_exclude.excludes_walked_file(path));
                }
                if entry.path_is_symlink() && is_dir && symlink_might_be_cyclic(path) {
                    return false;
                }
                let taken = Utf8PathBuf::from_path_buf(path.to_path_buf())
                    .ok()
                    .and_then(|path| AbsPathBuf::try_from(path).ok())
                    .is_some_and(|path| {
                        !is_dir
                            || (!starts.contains(&path)
                                && self.watched_dir_entries.iter().any(|dirs| {
                                    dirs.contains_dir(&path) && !dirs.include.contains(&path)
                                }))
                    });
                taken
                    && !self.watched_dir_entries.iter().any(|dirs| {
                        dirs.hard_exclude.prunes_walked_dir(
                            path,
                            entry.path_is_symlink(),
                            &mut resolved,
                        )
                    })
            });
            for entry in walk.filter_map(Result::ok) {
                if self.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                let Some(path) = Utf8PathBuf::from_path_buf(entry.into_path())
                    .ok()
                    .and_then(|path| AbsPathBuf::try_from(path).ok())
                else {
                    continue;
                };
                match self.classify_watched_path(&path) {
                    Some(loader::LoadMode::LoadContent) => {
                        let contents = read(&path);
                        changed_bytes += contents.as_ref().map_or(0, Vec::len);
                        changed.push((path, contents));
                        if changed_bytes >= LOADED_CHUNK_BYTES {
                            self.send(loader::Message::Changed {
                                files: std::mem::take(&mut changed),
                            });
                            changed_bytes = 0;
                        }
                    }
                    Some(loader::LoadMode::WatchOnly) => {
                        watch_only.push(path);
                        if watch_only.len() >= WATCH_ONLY_CHUNK_PATHS {
                            self.send(loader::Message::WatchOnly {
                                files: std::mem::take(&mut watch_only),
                            });
                        }
                    }
                    None => {}
                }
            }
        }
        if !changed.is_empty() {
            self.send(loader::Message::Changed { files: changed });
        }
        if !watch_only.is_empty() {
            self.send(loader::Message::WatchOnly { files: watch_only });
        }
    }

    /// Whether the configuration takes the directory at `dir`.
    fn takes_dir(&self, dir: &AbsPathBuf) -> bool {
        self.watched_dir_entries.iter().any(|dirs| dirs.contains_dir(dir))
    }

    /// The include roots strictly below `dir` that are not hard-excluded.
    fn include_roots_below(&self, dir: &AbsPathBuf) -> Vec<AbsPathBuf> {
        let mut roots: Vec<AbsPathBuf> = Vec::new();
        for dirs in &self.watched_dir_entries {
            for include in &dirs.include {
                if include != dir
                    && include.starts_with(dir)
                    && !dirs.is_hard_excluded(include)
                    && !roots.contains(include)
                {
                    roots.push(include.clone());
                }
            }
        }
        roots
    }

    /// The mode a directory under a watched root is registered with.
    fn directory_mode(&self, dir: &Path) -> RecursiveMode {
        if self.has_exclusion_below(dir) {
            RecursiveMode::NonRecursive
        } else {
            RecursiveMode::Recursive
        }
    }

    fn is_hard_excluded(&self, path: &Path) -> bool {
        self.watched_dir_entries.iter().any(|dir| dir.hard_exclude.is_excluded_resolved(path))
    }

    fn has_exclusion_below(&self, dir: &Path) -> bool {
        self.watched_dir_entries
            .iter()
            .any(|dirs| dirs.hard_exclude.has_exclusion_below_resolved(dir))
    }

    /// Give up the registrations at and beneath `path` once it is actually gone.
    ///
    /// The release cannot hang on how the path classifies: a registered target may be a
    /// directory the watched sets name nowhere — the uncovered parent of an
    /// exactly-listed file — and such a path classifies as `Ignore` when it disappears,
    /// so nothing would ever release it and a directory recreated under that name would
    /// look already watched. The disk is asked only when a record is actually held, and
    /// only an outright absence counts: a transient stat error must never drop a live
    /// watch.
    fn release_watch_if_gone(&self, path: &AbsPath) {
        let Some((state, _)) = &self.watcher else { return };
        let target: &Path = path.as_ref();
        if !state.borrow().registered.contains_key(target) {
            return;
        }
        let gone = fs::symlink_metadata(target)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if gone {
            forget_registrations_beneath(&mut state.borrow_mut().registered, target);
        }
    }

    /// Give up the registrations this configuration no longer asks for.
    ///
    /// A watcher that survives a reconfiguration would otherwise hold every target it
    /// was ever given: on inotify a descriptor per directory of a tree nobody watches
    /// any more, and a wake-up for each of its events. The sweep finds nothing while
    /// the configuration is stable — the case that arrives on every baseline write — so
    /// the stream rebuild an `unwatch` costs on FSEvents is paid only when the
    /// requested set genuinely shrank, and paid BEFORE the scan, which then re-reads
    /// whatever the rebuild dropped.
    fn release_watches_no_longer_requested(&self) {
        let Some((state, _)) = &self.watcher else { return };
        let stale: Vec<PathBuf> = state
            .borrow()
            .registered
            .iter()
            .filter(|(target, mode)| {
                !self.registration_is_requested(target)
                    || (**mode == RecursiveMode::Recursive && self.has_exclusion_below(target))
            })
            .map(|(target, _)| target.clone())
            .collect();
        if stale.is_empty() {
            return;
        }
        let mut state = state.borrow_mut();
        for target in stale {
            log_notify_error(state.watcher.unwatch(&target));
            state.registered.remove(&target);
        }
    }

    /// Whether the current configuration still asks for a registration on `target`.
    ///
    /// Every watch this actor places has one of two shapes, and the answer enumerates
    /// both: a directory under a watched root, or the directory holding an
    /// exactly-listed file. Neither may lie in a hard exclusion. The caller also drops
    /// a recursive registration that now reaches one: an unchanged path is no proof of
    /// an unchanged coverage.
    fn registration_is_requested(&self, target: &Path) -> bool {
        if self.is_hard_excluded(target) {
            return false;
        }
        let Some(target) = Utf8PathBuf::from_path_buf(target.to_path_buf())
            .ok()
            .and_then(|path| AbsPathBuf::try_from(path).ok())
        else {
            return false;
        };
        if self.watched_dir_entries.iter().any(|dir| dir.contains_dir(&target)) {
            return true;
        }
        self.watched_file_entries
            .iter()
            .chain(&self.watched_only_file_entries)
            .any(|file| file.parent() == Some(target.as_path()))
    }

    #[track_caller]
    fn send(&self, msg: loader::Message) {
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        if let Err(crossbeam_channel::SendError(_)) = self.sender.send(msg) {
            tracing::debug!("loader receiver disconnected, aborting vfs scan");
            self.shutdown.store(true, Ordering::Release);
        }
    }
}

/// Whether the registration already placed on `target` answers for `mode`.
///
/// A recursive one answers for both; a non-recursive one only for itself, because
/// `notify` REPLACES the mode of a path it already watches — so a widening request
/// has to go through.
fn registration_stands(
    registered: &FxHashMap<PathBuf, RecursiveMode>,
    target: &Path,
    mode: RecursiveMode,
) -> bool {
    match registered.get(target) {
        Some(RecursiveMode::Recursive) => true,
        Some(RecursiveMode::NonRecursive) => mode == RecursiveMode::NonRecursive,
        None => false,
    }
}

/// Drop the records for `path` and everything below it.
///
/// A registration lives on the directory it was placed on: once that directory is
/// gone the backend has released it (inotify drops the descriptor), so a surviving
/// record would make a directory recreated under the same name look already watched
/// and leave it blind for the life of the process.
fn forget_registrations_beneath(registered: &mut FxHashMap<PathBuf, RecursiveMode>, path: &Path) {
    registered.retain(|target, _| !target.starts_with(path));
}

fn read(path: &AbsPath) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn log_notify_error<T>(res: notify::Result<T>) -> Option<T> {
    res.map_err(|err| tracing::warn!("notify error: {}", err)).ok()
}

fn symlink_might_be_cyclic(path: &Path) -> bool {
    let Ok(destination) = std::fs::read_link(path) else {
        return false;
    };

    let is_relative_parent =
        destination.components().all(|c| matches!(c, Component::CurDir | Component::ParentDir));

    is_relative_parent || path.starts_with(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;
    use stdx::path_exclusion::ExcludedPaths;
    use vfs::loader::Handle;

    fn fixture(count: usize, bytes_per_file: usize) -> (tempfile::TempDir, loader::Directories) {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..count {
            let path = dir.path().join(format!("file_{i}.txt"));
            std::fs::write(&path, vec![b'x'; bytes_per_file]).expect("write fixture");
        }
        let abs_root = AbsPathBuf::assert_utf8(dir.path().to_path_buf());
        let dirs = loader::Directories {
            extensions: vec!["txt".to_string()],
            include: vec![abs_root],
            exclude: vec![],
            hard_exclude: Default::default(),
            rules: Vec::new(),
        };
        (dir, dirs)
    }

    #[derive(Default)]
    struct LoadCapture {
        loaded: Vec<Vec<(AbsPathBuf, usize)>>,
        watch_only: Vec<Vec<AbsPathBuf>>,
        walked_dirs: Vec<AbsPathBuf>,
    }

    fn run(dirs: loader::Directories, threshold: usize) -> Vec<Vec<(AbsPathBuf, usize)>> {
        let shutdown = AtomicBool::new(false);
        run_with_shutdown(dirs, threshold, &shutdown)
    }

    fn run_with_shutdown(
        dirs: loader::Directories,
        threshold: usize,
        shutdown: &AtomicBool,
    ) -> Vec<Vec<(AbsPathBuf, usize)>> {
        run_full_with_shutdown(dirs, threshold, WATCH_ONLY_CHUNK_PATHS, shutdown).loaded
    }

    fn run_full(dirs: loader::Directories, threshold: usize, watch_chunk: usize) -> LoadCapture {
        let shutdown = AtomicBool::new(false);
        run_full_with_shutdown(dirs, threshold, watch_chunk, &shutdown)
    }

    fn run_full_with_shutdown(
        dirs: loader::Directories,
        threshold: usize,
        watch_chunk: usize,
        shutdown: &AtomicBool,
    ) -> LoadCapture {
        let loaded: Mutex<Vec<Vec<(AbsPathBuf, usize)>>> = Mutex::new(Vec::new());
        let watch_only: Mutex<Vec<Vec<AbsPathBuf>>> = Mutex::new(Vec::new());
        let walked_dirs: Mutex<Vec<AbsPathBuf>> = Mutex::new(Vec::new());
        NotifyActor::load_entry(
            |_| {},
            loader::Entry::Directories(dirs),
            false,
            |path| walked_dirs.lock().unwrap().push(path),
            || {},
            |files| {
                let summary =
                    files.into_iter().map(|(p, c)| (p, c.map(|c| c.len()).unwrap_or(0))).collect();
                loaded.lock().unwrap().push(summary);
            },
            threshold,
            |files| {
                watch_only.lock().unwrap().push(files);
            },
            watch_chunk,
            shutdown,
        );
        LoadCapture {
            loaded: loaded.into_inner().unwrap(),
            watch_only: watch_only.into_inner().unwrap(),
            walked_dirs: walked_dirs.into_inner().unwrap(),
        }
    }

    #[test]
    fn under_threshold_emits_single_chunk() {
        let (_guard, dirs) = fixture(5, 4 * 1024);
        let chunks = run(dirs, 1024 * 1024);
        assert_eq!(chunks.len(), 1, "expected one chunk under threshold, got {chunks:#?}");
        assert_eq!(chunks[0].len(), 5);
    }

    #[test]
    fn over_threshold_emits_multiple_chunks() {
        let (_guard, dirs) = fixture(8, 4 * 1024);
        let chunks = run(dirs, 10 * 1024);
        assert!(
            chunks.len() >= 3,
            "expected multiple chunks over threshold, got {} chunks: {:#?}",
            chunks.len(),
            chunks,
        );
        let total_files: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total_files, 8, "all files must be emitted exactly once across chunks");
    }

    #[test]
    fn empty_entry_emits_no_chunks() {
        let (_guard, dirs) = fixture(0, 0);
        let chunks = run(dirs, 1024);
        assert!(chunks.is_empty(), "empty entry must not emit any Loaded message");
    }

    #[test]
    fn oversized_single_file_becomes_own_chunk() {
        let (_guard, dirs) = fixture(1, 8 * 1024);
        let chunks = run(dirs, 1024);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1);
        assert_eq!(chunks[0][0].1, 8 * 1024);
    }

    #[test]
    fn parallel_pool_delivers_every_file_exactly_once() {
        // With N>1 readers, every walked file must be delivered once: no loss,
        // no duplication, regardless of read-completion order.
        let (_guard, dirs) = fixture(500, 64);
        let chunks = run(dirs, 1024 * 1024);
        let all: Vec<AbsPathBuf> = chunks.into_iter().flatten().map(|(p, _)| p).collect();
        let unique: std::collections::HashSet<AbsPathBuf> = all.iter().cloned().collect();
        assert_eq!(all.len(), 500, "each file must be delivered exactly once");
        assert_eq!(unique.len(), 500, "all 500 distinct files must be delivered");
    }

    #[test]
    fn slow_consumer_backpressure_does_not_deadlock() {
        // A consumer that sleeps on every `send_loaded` parks the loader while
        // readers fill the bounded results channel; the loader must keep draining
        // and still deliver every file (backpressure smoke test).
        let (_guard, dirs) = fixture(300, 4 * 1024);
        let shutdown = AtomicBool::new(false);
        let delivered = Mutex::new(0usize);
        NotifyActor::load_entry(
            |_| {},
            loader::Entry::Directories(dirs),
            false,
            |_| {},
            || {},
            |files| {
                std::thread::sleep(std::time::Duration::from_millis(1));
                *delivered.lock().unwrap() += files.len();
            },
            8 * 1024,
            |_| {},
            WATCH_ONLY_CHUNK_PATHS,
            &shutdown,
        );
        assert_eq!(*delivered.lock().unwrap(), 300, "every file delivered despite slow consumer");
    }

    #[test]
    fn shutdown_midscan_joins_readers_without_hang() {
        // Flipping the shutdown latch partway through the scan must terminate the
        // walk, drain in-flight reads, and join the reader pool (reaching the
        // assertion at all proves the join did not hang). The aborted scan
        // suppresses the tail flush, so not all files are delivered.
        let (_guard, dirs) = fixture(500, 64);
        let shutdown = AtomicBool::new(false);
        let seen = AtomicUsize::new(0);
        let delivered = Mutex::new(0usize);
        NotifyActor::load_entry(
            |_| {},
            loader::Entry::Directories(dirs),
            false,
            |_| {},
            || {
                if seen.fetch_add(1, Ordering::AcqRel) + 1 >= 10 {
                    shutdown.store(true, Ordering::Release);
                }
            },
            |files| {
                *delivered.lock().unwrap() += files.len();
            },
            1024 * 1024,
            |_| {},
            WATCH_ONLY_CHUNK_PATHS,
            &shutdown,
        );
        assert!(*delivered.lock().unwrap() <= 500);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_delivered_as_none() {
        // A file that exists at walk time but whose bytes cannot be read (here a
        // 0o000 file) is delivered with `None` contents, so the consumer can
        // degrade it rather than the read being silently dropped.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let readable = dir.path().join("ok.txt");
        std::fs::write(&readable, b"hello").expect("write readable");
        let blocked = dir.path().join("blocked.txt");
        std::fs::write(&blocked, b"secret").expect("write blocked");
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");

        // Root bypasses permission bits — if we can still read it, skip.
        if std::fs::read(&blocked).is_ok() {
            let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644));
            return;
        }

        let abs_root = AbsPathBuf::assert_utf8(dir.path().to_path_buf());
        let dirs = loader::Directories {
            extensions: vec!["txt".to_string()],
            include: vec![abs_root],
            exclude: vec![],
            hard_exclude: Default::default(),
            rules: Vec::new(),
        };

        let shutdown = AtomicBool::new(false);
        let loaded: Mutex<Vec<(AbsPathBuf, Option<usize>)>> = Mutex::new(Vec::new());
        NotifyActor::load_entry(
            |_| {},
            loader::Entry::Directories(dirs),
            false,
            |_| {},
            || {},
            |files| {
                let mut g = loaded.lock().unwrap();
                for (p, c) in files {
                    g.push((p, c.map(|v| v.len())));
                }
            },
            1024 * 1024,
            |_| {},
            WATCH_ONLY_CHUNK_PATHS,
            &shutdown,
        );

        let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644));

        let loaded = loaded.into_inner().unwrap();
        let find =
            |name: &str| loaded.iter().find(|(p, _)| p.file_name() == Some(name)).map(|(_, c)| *c);
        assert_eq!(find("ok.txt"), Some(Some(5)), "readable file delivers its bytes");
        assert_eq!(find("blocked.txt"), Some(None), "unreadable file delivers None");
    }

    fn fixture_mixed(
        bsl_count: usize,
        xml_count: usize,
        bytes_per_file: usize,
    ) -> (tempfile::TempDir, AbsPathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..bsl_count {
            let path = dir.path().join(format!("module_{i}.bsl"));
            std::fs::write(&path, vec![b'x'; bytes_per_file]).expect("write bsl fixture");
        }
        for i in 0..xml_count {
            let path = dir.path().join(format!("form_{i}.xml"));
            std::fs::write(&path, vec![b'x'; bytes_per_file]).expect("write xml fixture");
        }
        let abs_root = AbsPathBuf::assert_utf8(dir.path().to_path_buf());
        (dir, abs_root)
    }

    #[test]
    fn watch_only_rule_dispatches_to_separate_buffer() {
        let (_guard, root) = fixture_mixed(3, 5, 1024);
        let dirs = loader::Directories {
            extensions: vec!["bsl".to_string()],
            include: vec![root],
            exclude: vec![],
            hard_exclude: Default::default(),
            rules: vec![loader::FileRule {
                extensions: vec!["xml".to_string()],
                load_mode: loader::LoadMode::WatchOnly,
            }],
        };

        let cap = run_full(dirs, 1024 * 1024, 1024);
        let total_loaded: usize = cap.loaded.iter().map(|c| c.len()).sum();
        let total_watch_only: usize = cap.watch_only.iter().map(|c| c.len()).sum();
        assert_eq!(total_loaded, 3, "exactly the 3 .bsl files should be loaded");
        assert_eq!(total_watch_only, 5, "exactly the 5 .xml files should be watch-only");

        for chunk in &cap.loaded {
            for (path, _) in chunk {
                assert_ne!(path.extension(), Some("xml"), "xml must not be in loaded buffer");
            }
        }
        for chunk in &cap.watch_only {
            for path in chunk {
                assert_ne!(path.extension(), Some("bsl"), "bsl must not be in watch-only buffer");
            }
        }
    }

    #[test]
    fn pure_watch_only_emits_only_watch_only_chunks() {
        let (_guard, root) = fixture_mixed(0, 4, 1024);
        let dirs = loader::Directories {
            extensions: vec![],
            include: vec![root],
            exclude: vec![],
            hard_exclude: Default::default(),
            rules: vec![loader::FileRule {
                extensions: vec!["xml".to_string()],
                load_mode: loader::LoadMode::WatchOnly,
            }],
        };

        let cap = run_full(dirs, 1024 * 1024, 1024);
        assert!(cap.loaded.is_empty(), "no content extensions configured, got {:#?}", cap.loaded);
        let total_watch_only: usize = cap.watch_only.iter().map(|c| c.len()).sum();
        assert_eq!(total_watch_only, 4);
    }

    #[test]
    fn explicit_watch_only_files_are_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = AbsPathBuf::assert_utf8(dir.path().join("baseline.json"));
        std::fs::write(path.as_path(), b"secret").unwrap();
        let loaded = Mutex::new(Vec::new());
        let watched = Mutex::new(Vec::new());
        NotifyActor::load_entry(
            |_| {},
            loader::Entry::WatchOnlyFiles(vec![path.clone()]),
            false,
            |_| {},
            || {},
            |files| loaded.lock().unwrap().extend(files),
            LOADED_CHUNK_BYTES,
            |files| watched.lock().unwrap().extend(files),
            WATCH_ONLY_CHUNK_PATHS,
            &AtomicBool::new(false),
        );
        assert!(loaded.into_inner().unwrap().is_empty());
        assert_eq!(watched.into_inner().unwrap(), vec![path]);
    }

    #[test]
    fn watch_only_chunking_respects_path_threshold() {
        let (_guard, root) = fixture_mixed(0, 7, 64);
        let dirs = loader::Directories {
            extensions: vec![],
            include: vec![root],
            exclude: vec![],
            hard_exclude: Default::default(),
            rules: vec![loader::FileRule {
                extensions: vec!["xml".to_string()],
                load_mode: loader::LoadMode::WatchOnly,
            }],
        };

        let cap = run_full(dirs, 1024 * 1024, 3);
        assert!(
            cap.watch_only.len() >= 3,
            "expected ≥3 watch-only chunks at threshold 3, got {} chunks",
            cap.watch_only.len(),
        );
        let total: usize = cap.watch_only.iter().map(|c| c.len()).sum();
        assert_eq!(total, 7, "all watch-only paths emitted exactly once");
    }

    #[test]
    fn extensions_win_over_rules_for_same_extension() {
        let (_guard, root) = fixture_mixed(0, 2, 64);
        let dirs = loader::Directories {
            extensions: vec!["xml".to_string()],
            include: vec![root],
            exclude: vec![],
            hard_exclude: Default::default(),
            rules: vec![loader::FileRule {
                extensions: vec!["xml".to_string()],
                load_mode: loader::LoadMode::WatchOnly,
            }],
        };

        let cap = run_full(dirs, 1024 * 1024, 1024);
        let total_loaded: usize = cap.loaded.iter().map(|c| c.len()).sum();
        assert_eq!(total_loaded, 2, "extensions must win — both .xml load as content");
        assert!(cap.watch_only.is_empty(), "watch-only buffer must stay empty");
    }

    #[test]
    fn count_files_unions_extensions_and_rules() {
        let (_guard, root) = fixture_mixed(3, 5, 64);
        let dirs = loader::Directories {
            extensions: vec!["bsl".to_string()],
            include: vec![root],
            exclude: vec![],
            hard_exclude: Default::default(),
            rules: vec![loader::FileRule {
                extensions: vec!["xml".to_string()],
                load_mode: loader::LoadMode::WatchOnly,
            }],
        };
        let cancel = AtomicBool::new(false);
        let count = NotifyActor::count_files_in_entry(&loader::Entry::Directories(dirs), &cancel);
        assert_eq!(count, 8, "count must union extensions + rules");
    }

    #[test]
    fn hard_exclusions_agree_across_count_and_initial_load() {
        let guard = tempfile::tempdir().unwrap();
        let root = AbsPathBuf::assert_utf8(guard.path().canonicalize().unwrap());
        let write = |relative: &str| {
            let path = AsRef::<Path>::as_ref(&root).join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        };
        write("Allowed.bsl");
        write(".tmp/Hidden.bsl");
        write(".tmp/Hidden.xml");
        write(".tmp2/Visible.bsl");
        write(".git/Legacy.bsl");
        write("build/Legacy.bsl");
        write(".vscode/Legacy.bsl");
        write(".buildfoo/Visible.bsl");
        let root_path = AsRef::<Path>::as_ref(&root);
        let dirs = loader::Directories {
            extensions: vec!["bsl".to_owned()],
            include: vec![root.clone()],
            exclude: [".git", "build", ".vscode"]
                .into_iter()
                .map(|name| AbsPathBuf::assert_utf8(root_path.join(name)))
                .collect(),
            hard_exclude: ExcludedPaths::new([root_path.join(".tmp")]),
            rules: vec![loader::FileRule {
                extensions: vec!["xml".to_owned()],
                load_mode: loader::LoadMode::WatchOnly,
            }],
        };

        let count = NotifyActor::count_files_in_entry(
            &loader::Entry::Directories(dirs.clone()),
            &AtomicBool::new(false),
        );
        let cap = run_full(dirs, 1024 * 1024, 1024);
        let loaded: Vec<_> = cap.loaded.into_iter().flatten().collect();
        let watch_only: Vec<_> = cap.watch_only.into_iter().flatten().collect();

        assert_eq!(count, 3);
        assert_eq!(loaded.len(), 3, "{loaded:?}");
        assert!(loaded.iter().all(|path| {
            !AsRef::<Path>::as_ref(&path.0).starts_with(AsRef::<Path>::as_ref(&root).join(".tmp"))
        }));
        assert!(watch_only.is_empty(), "an excluded XML reached the watch-only batch");
        assert!(loaded
            .iter()
            .any(|path| AsRef::<Path>::as_ref(&path.0).ends_with(".tmp2/Visible.bsl")));
        assert!(loaded
            .iter()
            .any(|path| AsRef::<Path>::as_ref(&path.0).ends_with(".buildfoo/Visible.bsl")));
        assert!(loaded.iter().all(|path| {
            [".git", "build", ".vscode"]
                .into_iter()
                .all(|legacy| !AsRef::<Path>::as_ref(&path.0).starts_with(root_path.join(legacy)))
        }));
        assert!(cap.walked_dirs.iter().any(|path| path.as_path() == root_path));
        assert!(
            cap.walked_dirs
                .iter()
                .all(|path| { !AsRef::<Path>::as_ref(path).starts_with(root_path.join(".tmp")) }),
            "the initial loader entered the hard-excluded directory: {:?}",
            cap.walked_dirs
        );
        assert!(
            cap.walked_dirs.iter().any(|path| AsRef::<Path>::as_ref(path).ends_with(".tmp2")),
            "the initial loader did not visit the allowed prefix sibling: {:?}",
            cap.walked_dirs
        );
    }

    #[test]
    fn initial_walk_skips_files_and_directories_matched_by_exclusion_patterns() {
        let guard = tempfile::tempdir().unwrap();
        let root = AbsPathBuf::assert_utf8(guard.path().canonicalize().unwrap());
        let root_path = AsRef::<Path>::as_ref(&root);
        let write = |relative: &str| {
            let path = root_path.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        };
        write("Allowed.bsl");
        write("temp/Hidden.bsl");
        write(".tool/Hidden.bsl");
        write("Reports/R/Templates/T/Ext/Template.Queries.bsl");
        write("Reports/R/Ext/ObjectModule.bsl");
        write("temporary/Visible.bsl");
        let patterns = ["temp", ".tool", "**/*.Queries.bsl"];
        let dirs = loader::Directories {
            extensions: vec!["bsl".to_owned()],
            include: vec![root.clone()],
            exclude: Vec::new(),
            hard_exclude: ExcludedPaths::default().with_patterns(root_path, patterns).unwrap(),
            rules: Vec::new(),
        };

        let count = NotifyActor::count_files_in_entry(
            &loader::Entry::Directories(dirs.clone()),
            &AtomicBool::new(false),
        );
        let cap = run_full(dirs, 1024 * 1024, 1024);
        let mut loaded: Vec<String> = cap
            .loaded
            .into_iter()
            .flatten()
            .map(|entry| {
                let path = AsRef::<Path>::as_ref(&entry.0).strip_prefix(root_path).unwrap();
                path.to_string_lossy().replace('\\', "/")
            })
            .collect();
        loaded.sort();

        assert_eq!(
            loaded,
            ["Allowed.bsl", "Reports/R/Ext/ObjectModule.bsl", "temporary/Visible.bsl"]
        );
        assert_eq!(count, 3, "the counting walk disagrees with the loading walk");
        assert!(
            cap.walked_dirs.iter().all(|path| {
                let path = AsRef::<Path>::as_ref(path);
                !path.starts_with(root_path.join("temp"))
                    && !path.starts_with(root_path.join(".tool"))
            }),
            "the walk entered a directory a pattern excludes: {:?}",
            cap.walked_dirs
        );
    }

    fn actor_with_watched_dirs(dirs: Vec<loader::Directories>) -> NotifyActor {
        let (tx, _rx) = crossbeam_channel::unbounded::<loader::Message>();
        let mut actor = NotifyActor::new(tx, Arc::new(AtomicBool::new(false)));
        actor.watched_dir_entries = dirs;
        actor
    }

    fn dirs_for(
        root: &AbsPathBuf,
        content_ext: &[&str],
        watch_ext: &[&str],
    ) -> loader::Directories {
        let rules = if watch_ext.is_empty() {
            Vec::new()
        } else {
            vec![loader::FileRule {
                extensions: watch_ext.iter().map(|s| (*s).to_string()).collect(),
                load_mode: loader::LoadMode::WatchOnly,
            }]
        };
        loader::Directories {
            extensions: content_ext.iter().map(|s| (*s).to_string()).collect(),
            include: vec![root.clone()],
            exclude: vec![],
            hard_exclude: Default::default(),
            rules,
        }
    }

    #[test]
    fn classify_watched_path_dispatches_load_modes() {
        let (_g, root) = fixture_mixed(0, 0, 0);
        let dirs = dirs_for(&root, &["bsl"], &["xml"]);
        let actor = actor_with_watched_dirs(vec![dirs]);

        let bsl =
            AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join("Module.bsl"));
        let xml = AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join("Form.xml"));
        let other =
            AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join("README.md"));

        assert_eq!(actor.classify_watched_path(&bsl), Some(loader::LoadMode::LoadContent));
        assert_eq!(actor.classify_watched_path(&xml), Some(loader::LoadMode::WatchOnly));
        assert_eq!(actor.classify_watched_path(&other), None);
    }

    /// The initial scan follows symlinks, so a source file reached through one is
    /// loaded — and its later edits have to arrive as content, not be ignored. Baseline
    /// files keep their exact-match handling and are unaffected.
    #[cfg(unix)]
    #[test]
    fn classify_event_path_follows_a_symlinked_source() {
        let (_g, root) = fixture_mixed(0, 0, 0);
        let dirs = dirs_for(&root, &["bsl"], &["json"]);
        let mut actor = actor_with_watched_dirs(vec![dirs]);
        let join = |name: &str| {
            AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join(name))
        };

        let real = join("real.bsl");
        std::fs::write(AsRef::<std::path::Path>::as_ref(&real), b"x").unwrap();
        let link = join("Link.bsl");
        std::os::unix::fs::symlink("real.bsl", AsRef::<std::path::Path>::as_ref(&link)).unwrap();
        assert_eq!(actor.classify_event_path(&real), EventPathAction::LoadContent);
        assert_eq!(
            actor.classify_event_path(&link),
            EventPathAction::LoadContent,
            "a symlinked source must still deliver its content"
        );

        // A baseline file listed exactly stays watch-only, symlink or not.
        let baseline = join("baseline.json");
        std::fs::write(AsRef::<std::path::Path>::as_ref(&baseline), b"{}").unwrap();
        actor.watched_only_file_entries.insert(baseline.clone());
        assert_eq!(actor.classify_event_path(&baseline), EventPathAction::WatchOnly);
    }

    /// An exactly-listed file inside a recursively watched directory must not
    /// re-register that directory: `notify` would replace the recursive mode with the
    /// narrower one and stop delivering events from every subdirectory. The baseline
    /// file sits in the project root by default, so this is the ordinary case.
    #[test]
    fn watch_target_keeps_a_recursive_directory_recursive() {
        let (_g, root) = fixture_mixed(0, 0, 0);
        let dirs = dirs_for(&root, &["bsl"], &["json"]);
        let mut actor = actor_with_watched_dirs(vec![dirs]);
        let inside =
            AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join("baseline.json"));
        actor.watched_only_file_entries.insert(inside.clone());

        assert_eq!(
            actor.watch_target(inside.as_ref() as &std::path::Path),
            None,
            "the recursive watch already covers it"
        );

        // A directory is still registered recursively.
        let subdir = AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join("sub"));
        assert_eq!(
            actor.watch_target(subdir.as_ref() as &std::path::Path),
            Some(((subdir.as_ref() as &std::path::Path).to_path_buf(), RecursiveMode::Recursive))
        );

        // A file OUTSIDE every watched directory still needs its own parent watch.
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = AbsPathBuf::assert_utf8(outside_dir.path().join("baseline.json"));
        actor.watched_only_file_entries.insert(outside.clone());
        assert_eq!(
            actor.watch_target(outside.as_ref() as &std::path::Path),
            Some((outside_dir.path().to_path_buf(), RecursiveMode::NonRecursive))
        );
    }

    #[test]
    fn a_future_hard_exclusion_narrows_ancestors_and_is_never_watched() {
        let guard = tempfile::tempdir().unwrap();
        let root = AbsPathBuf::assert_utf8(guard.path().canonicalize().unwrap());
        let hidden = AsRef::<Path>::as_ref(&root).join("hidden");
        let future = AsRef::<Path>::as_ref(&root).join("future/deep");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::create_dir_all(AsRef::<Path>::as_ref(&root).join("allowed")).unwrap();
        let mut dirs = dirs_for(&root, &["bsl"], &["xml"]);
        dirs.hard_exclude = ExcludedPaths::new([hidden.clone(), future.clone()]);
        let mut actor = actor_with_watched_dirs(vec![dirs]);

        assert_eq!(
            actor.watch_target(AsRef::<Path>::as_ref(&root)),
            Some((AsRef::<Path>::as_ref(&root).to_path_buf(), RecursiveMode::NonRecursive))
        );
        assert_eq!(actor.watch_target(&hidden), None);
        assert_eq!(
            actor.watch_target(&AsRef::<Path>::as_ref(&root).join("allowed")),
            Some((AsRef::<Path>::as_ref(&root).join("allowed"), RecursiveMode::Recursive))
        );

        give_a_live_watcher(&mut actor);
        actor.watch(AsRef::<Path>::as_ref(&root));
        let registered = registered_of(&actor);
        assert!(
            registered.iter().all(|(path, mode)| {
                *mode != RecursiveMode::Recursive
                    || (!hidden.starts_with(path) && !future.starts_with(path))
            }),
            "recursive watch crosses an exclusion: {registered:?}"
        );
        assert!(registered.keys().all(|path| !path.starts_with(&hidden)));

        let hidden_runtime = hidden.join("RuntimeHidden.bsl");
        let allowed_runtime = AsRef::<Path>::as_ref(&root).join("allowed/RuntimeAllowed.bsl");
        std::fs::write(&hidden_runtime, "").unwrap();
        std::fs::write(&allowed_runtime, "").unwrap();
        let (_, receiver) = actor.watcher.as_ref().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut delivered = Vec::new();
        while std::time::Instant::now() < deadline
            && !delivered.iter().any(|path: &PathBuf| path.ends_with("RuntimeAllowed.bsl"))
        {
            if let Ok(Ok(event)) = receiver.recv_timeout(Duration::from_millis(50)) {
                delivered.extend(event.paths);
            }
        }
        while let Ok(Ok(event)) = receiver.recv_timeout(Duration::from_millis(100)) {
            delivered.extend(event.paths);
        }
        assert!(
            delivered.iter().any(|path| path.ends_with("RuntimeAllowed.bsl")),
            "positive control: the real loader watcher missed the allowed write: {delivered:?}"
        );
        assert!(
            delivered.iter().all(|path| !path.ends_with("RuntimeHidden.bsl")),
            "the real loader watcher delivered an excluded write: {delivered:?}"
        );
    }

    #[test]
    fn a_hard_exclusion_equal_to_the_watch_root_registers_nothing() {
        let guard = tempfile::tempdir().unwrap();
        let root = AbsPathBuf::assert_utf8(guard.path().canonicalize().unwrap());
        let root_path = AsRef::<Path>::as_ref(&root);
        let mut dirs = dirs_for(&root, &["bsl"], &["xml"]);
        dirs.hard_exclude = ExcludedPaths::new([root_path.to_path_buf()]);
        let mut actor = actor_with_watched_dirs(vec![dirs.clone()]);
        give_a_live_watcher(&mut actor);

        actor.watch(root.as_path().as_ref());

        assert!(registered_of(&actor).is_empty(), "an excluded root was watched");
        assert_eq!(
            NotifyActor::count_files_in_entry(
                &loader::Entry::Directories(dirs.clone()),
                &AtomicBool::new(false),
            ),
            0
        );
        let loaded = run_full(dirs, 1024 * 1024, 1024);
        assert!(loaded.loaded.is_empty());
        assert!(loaded.watch_only.is_empty());
        assert!(loaded.walked_dirs.is_empty(), "an excluded root was traversed");
    }

    #[cfg(windows)]
    #[test]
    fn an_excluded_v8_import_directory_can_be_deleted_and_recreated_on_windows() {
        let guard = tempfile::tempdir().unwrap();
        let root = AbsPathBuf::assert_utf8(guard.path().canonicalize().unwrap());
        let root_path = AsRef::<Path>::as_ref(&root);
        let service_parent = root_path.join(".v8vscedit");
        let import = service_parent.join("import-temp");
        let allowed = root_path.join("allowed");
        std::fs::create_dir_all(&import).unwrap();
        std::fs::create_dir_all(&allowed).unwrap();
        let mut dirs = dirs_for(&root, &["bsl"], &[]);
        dirs.hard_exclude = ExcludedPaths::new([import.clone()]);
        let mut actor = actor_with_watched_dirs(vec![dirs]);
        give_a_live_watcher(&mut actor);
        actor.watch(root_path);

        for round in 0..3 {
            std::fs::write(import.join(format!("Hidden{round}.bsl")), "").unwrap();
            std::fs::remove_dir_all(&service_parent)
                .expect("the server must not hold a handle inside the excluded service tree");
            std::fs::create_dir_all(&import).unwrap();
        }

        let allowed_file = allowed.join("StillWatched.bsl");
        std::fs::write(&allowed_file, "").unwrap();
        let (_, receiver) = actor.watcher.as_ref().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut delivered = Vec::new();
        while std::time::Instant::now() < deadline
            && !delivered.iter().any(|path: &PathBuf| path == &allowed_file)
        {
            if let Ok(Ok(event)) = receiver.recv_timeout(Duration::from_millis(50)) {
                delivered.extend(event.paths);
            }
        }
        assert!(delivered.iter().any(|path| path == &allowed_file), "{delivered:?}");
        assert!(
            delivered.iter().all(|path| path == &import || !path.starts_with(&import)),
            "{delivered:?}"
        );
    }

    #[test]
    fn classify_event_path_delivers_removals_and_loads_existing() {
        let (_g, root) = fixture_mixed(0, 0, 0);
        let dirs = dirs_for(&root, &["bsl"], &["xml"]);
        let actor = actor_with_watched_dirs(vec![dirs]);

        let join = |name: &str| {
            AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join(name))
        };

        // A removed (no longer stat-able) watched content file must be delivered
        // as a deletion, not dropped — this is the regression guard.
        assert_eq!(actor.classify_event_path(&join("Gone.bsl")), EventPathAction::Delete);
        // A removed watch-only file routes to metadata invalidation.
        assert_eq!(actor.classify_event_path(&join("Gone.xml")), EventPathAction::WatchOnly);
        // A removed unwatched FILE (has an extension) is ignored.
        assert_eq!(actor.classify_event_path(&join("Gone.md")), EventPathAction::Ignore);
        // A removed extension-less path under a watched root looks like a removed
        // directory → handed to the consumer to expand into descendants.
        assert_eq!(actor.classify_event_path(&join("RemovedDir")), EventPathAction::DeleteSubtree);

        // An existing watched content file still loads its bytes.
        let existing = join("Module.bsl");
        std::fs::write(AsRef::<std::path::Path>::as_ref(&existing), b"x").unwrap();
        assert_eq!(actor.classify_event_path(&existing), EventPathAction::LoadContent);
    }

    #[test]
    fn classify_watched_path_is_case_insensitive() {
        let (_g, root) = fixture_mixed(0, 0, 0);
        let dirs = dirs_for(&root, &["bsl"], &["xml"]);
        let actor = actor_with_watched_dirs(vec![dirs]);

        let bsl_upper =
            AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join("Module.BSL"));
        let xml_upper =
            AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join("Form.XML"));
        assert_eq!(actor.classify_watched_path(&bsl_upper), Some(loader::LoadMode::LoadContent));
        assert_eq!(actor.classify_watched_path(&xml_upper), Some(loader::LoadMode::WatchOnly));
    }

    #[test]
    fn classify_watched_path_load_content_wins_across_entries() {
        let (_g, root) = fixture_mixed(0, 0, 0);
        let load_first = vec![dirs_for(&root, &["xml"], &[]), dirs_for(&root, &[], &["xml"])];
        let load_last = vec![dirs_for(&root, &[], &["xml"]), dirs_for(&root, &["xml"], &[])];
        let xml = AbsPathBuf::assert_utf8(AsRef::<std::path::Path>::as_ref(&root).join("a.xml"));

        let actor1 = actor_with_watched_dirs(load_first);
        let actor2 = actor_with_watched_dirs(load_last);
        assert_eq!(actor1.classify_watched_path(&xml), Some(loader::LoadMode::LoadContent));
        assert_eq!(actor2.classify_watched_path(&xml), Some(loader::LoadMode::LoadContent));
    }

    #[test]
    fn pre_set_shutdown_skips_scan_and_flush() {
        let (_guard, dirs) = fixture(50, 1024);
        let shutdown = AtomicBool::new(true);
        let chunks = run_with_shutdown(dirs, 100 * 1024, &shutdown);
        assert!(chunks.is_empty(), "shutdown latch must suppress all sends, got {chunks:#?}");
    }

    /// A repeat registration is not a harmless no-op on every backend, so the record
    /// must answer for the target exactly — including the one case where it must NOT
    /// suppress the call: widening a directory from non-recursive to recursive.
    #[test]
    fn a_standing_registration_answers_only_for_what_it_covers() {
        let mut registered = FxHashMap::default();
        registered.insert(PathBuf::from("/root"), RecursiveMode::Recursive);
        registered.insert(PathBuf::from("/root/leaf"), RecursiveMode::NonRecursive);

        assert!(registration_stands(&registered, Path::new("/root"), RecursiveMode::Recursive));
        assert!(registration_stands(&registered, Path::new("/root"), RecursiveMode::NonRecursive));
        assert!(registration_stands(
            &registered,
            Path::new("/root/leaf"),
            RecursiveMode::NonRecursive
        ));
        assert!(
            !registration_stands(&registered, Path::new("/root/leaf"), RecursiveMode::Recursive),
            "a narrower registration must not suppress the widening call",
        );
        assert!(
            !registration_stands(&registered, Path::new("/other"), RecursiveMode::Recursive),
            "an unknown target has nothing standing on it",
        );
    }

    /// Only what the removed directory took with it: a sibling whose name merely shares
    /// a prefix keeps its registration, and so does everything above.
    #[test]
    fn a_removed_subtree_releases_the_records_beneath_it() {
        let mut registered = FxHashMap::default();
        for path in ["/root", "/root/gone", "/root/gone/deep", "/root/gone-not"] {
            registered.insert(PathBuf::from(path), RecursiveMode::Recursive);
        }

        forget_registrations_beneath(&mut registered, Path::new("/root/gone"));

        let mut left: Vec<_> =
            registered.keys().map(|p| p.to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, vec!["/root".to_string(), "/root/gone-not".to_string()]);
    }

    fn give_a_live_watcher(actor: &mut NotifyActor) {
        let (watcher_sender, watcher_receiver) = unbounded();
        let watcher = RecommendedWatcher::new(
            move |event| {
                _ = watcher_sender.send(event);
            },
            Config::default(),
        )
        .expect("watcher");
        let state = WatchState { watcher, registered: FxHashMap::default() };
        actor.watcher = Some((RefCell::new(state), watcher_receiver));
    }

    fn registered_of(actor: &NotifyActor) -> FxHashMap<PathBuf, RecursiveMode> {
        let (state, _) = actor.watcher.as_ref().expect("watcher");
        let registered = state.borrow().registered.clone();
        registered
    }

    /// A registration is released when its directory is gone, and only then. The
    /// directory at stake is the uncovered parent of an exactly-listed file: no watched
    /// root names it, so its removal classifies as `Ignore` and a rule keyed on the
    /// classification would never reach it — leaving a directory recreated under the
    /// same name looking already watched.
    #[test]
    fn a_registration_is_released_once_its_directory_is_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        let live = root.join("live");
        std::fs::create_dir(&live).expect("create dir");
        let gone = root.join("gone");

        let mut actor = actor_with_watched_dirs(Vec::new());
        actor.watched_only_file_entries.insert(AbsPathBuf::assert_utf8(gone.join("manifest.json")));
        give_a_live_watcher(&mut actor);
        {
            let (state, _) = actor.watcher.as_ref().expect("watcher");
            let mut state = state.borrow_mut();
            state.registered.insert(live.clone(), RecursiveMode::NonRecursive);
            state.registered.insert(gone.clone(), RecursiveMode::NonRecursive);
        }

        assert_eq!(
            actor.classify_event_path(&AbsPathBuf::assert_utf8(gone.clone())),
            EventPathAction::Ignore,
            "the case only matters because the classification says nothing about it",
        );

        actor.release_watch_if_gone(&AbsPathBuf::assert_utf8(gone.clone()));
        actor.release_watch_if_gone(&AbsPathBuf::assert_utf8(live.clone()));

        let registered = registered_of(&actor);
        assert!(!registered.contains_key(&gone), "a directory that is gone must release its watch");
        assert!(registered.contains_key(&live), "a directory that is there must keep its watch");
    }

    /// Every watch this actor places has one of two shapes, and what the sweep lets go
    /// is whatever matches neither — a root the new configuration stopped asking for.
    #[test]
    fn only_what_the_configuration_still_asks_for_stays_requested() {
        let (_guard, root) = fixture_mixed(0, 0, 0);
        let raw: &std::path::Path = root.as_ref();
        let baseline = raw.join("baseline");
        let dropped = raw.parent().expect("parent").join("extension");

        let mut actor = actor_with_watched_dirs(vec![dirs_for(&root, &["bsl"], &[])]);
        actor
            .watched_only_file_entries
            .insert(AbsPathBuf::assert_utf8(baseline.join("manifest.json")));

        assert!(actor.registration_is_requested(raw), "a watched root asks for itself");
        assert!(
            actor.registration_is_requested(&raw.join("sub")),
            "a directory under a watched root is covered by it",
        );
        assert!(
            actor.registration_is_requested(&baseline),
            "the directory holding an exactly-listed file is asked for",
        );
        assert!(
            !actor.registration_is_requested(&dropped),
            "a root this configuration no longer names is asked for by nothing",
        );
    }

    /// A rendezvous loader channel: every message the actor sends waits to be taken
    /// here. That is what makes the moment of the writes below exact — while this
    /// side holds off, the actor is parked in a send and cannot have moved on.
    fn spawn_watching(root: &Path, version: u32) -> (NotifyHandle, Receiver<loader::Message>) {
        let (tx, rx) = bounded::<loader::Message>(0);
        let mut handle = NotifyHandle::spawn(tx);
        handle.set_config(watching_config(root, version));
        (handle, rx)
    }

    fn watching_config(root: &Path, version: u32) -> loader::Config {
        loader::Config {
            version,
            load: vec![loader::Entry::Directories(loader::Directories {
                extensions: vec!["txt".to_string()],
                include: vec![AbsPathBuf::assert_utf8(root.to_path_buf())],
                exclude: vec![],
                hard_exclude: Default::default(),
                rules: Vec::new(),
            })],
            watch: vec![0],
        }
    }

    fn received(
        rx: &Receiver<loader::Message>,
        mut wanted: impl FnMut(&loader::Message) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(msg) => {
                    if wanted(&msg) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
        false
    }

    fn carries(
        files: &[(AbsPathBuf, Option<Vec<u8>>)],
        path: &AbsPathBuf,
        contents: &[u8],
    ) -> bool {
        files.iter().any(|(p, c)| p == path && c.as_deref() == Some(contents))
    }

    /// A write that lands after its file has been read but before loading is announced
    /// finished must still reach the consumer. Nothing re-reads the tree afterwards, so
    /// a change made while no watch stands is not late — it is lost for good.
    #[test]
    fn a_write_during_the_scan_is_still_delivered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        let file = root.join("a.txt");
        std::fs::write(&file, b"before").expect("write fixture");
        let watched = AbsPathBuf::assert_utf8(file.clone());

        let (_handle, rx) = spawn_watching(&root, 1);

        // Waiting for the file's own contents proves the scan has read it: from here
        // on only a watch can carry the next version.
        assert!(
            received(&rx, |msg| {
                matches!(msg, loader::Message::Loaded { files } if carries(files, &watched, b"before"))
            }),
            "the scan never delivered the fixture",
        );
        std::fs::write(&file, b"after").expect("rewrite fixture");

        assert!(
            received(&rx, |msg| {
                matches!(msg, loader::Message::Changed { files } if carries(files, &watched, b"after"))
            }),
            "a write made before loading was announced finished never arrived",
        );
    }

    /// Every baseline write brings another configuration here, because baseline object
    /// names carry a content hash. The watch placed by the previous one must survive it:
    /// a write during the rescan reaches the consumer only through a registration that
    /// was never taken down.
    #[test]
    fn a_reconfiguration_does_not_take_the_watch_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical root");
        let file = root.join("a.txt");
        std::fs::write(&file, b"before").expect("write fixture");
        let watched = AbsPathBuf::assert_utf8(file.clone());

        let (mut handle, rx) = spawn_watching(&root, 1);
        assert!(
            received(&rx, |msg| {
                matches!(msg, loader::Message::Progress { n_done: LoadingProgress::Finished, .. })
            }),
            "the first configuration never finished loading",
        );

        handle.set_config(watching_config(&root, 2));
        assert!(
            received(&rx, |msg| {
                matches!(
                    msg,
                    loader::Message::Progress {
                        n_done: LoadingProgress::Scanning,
                        config_version: 2,
                        ..
                    }
                )
            }),
            "the second configuration never started",
        );
        std::fs::write(&file, b"after").expect("rewrite fixture");

        assert!(
            received(&rx, |msg| {
                matches!(msg, loader::Message::Changed { files } if carries(files, &watched, b"after"))
            }),
            "a write during a reconfiguration never arrived",
        );
    }

    #[test]
    fn handle_drop_latches_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let (sender, _receiver) = unbounded::<Message>();
        let thread = stdx::thread::Builder::new(stdx::thread::ThreadIntent::Worker, "test-drop")
            .spawn(|| {})
            .expect("spawn test worker");

        let handle = NotifyHandle { sender, shutdown: Arc::clone(&shutdown), _thread: thread };

        assert!(!shutdown.load(Ordering::Relaxed), "flag is false before drop");
        drop(handle);
        assert!(shutdown.load(Ordering::Relaxed), "Drop must latch shutdown");
    }
}
