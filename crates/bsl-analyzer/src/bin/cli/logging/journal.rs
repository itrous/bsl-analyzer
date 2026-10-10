//! Scoped, best-effort JSONL journal. All disk work belongs to the single worker.
use crossbeam_channel::{bounded, Receiver, Sender};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};
use tracing::{
    field::{Field, Visit},
    Event, Subscriber,
};
use tracing_subscriber::{filter::Targets, layer::Context, Layer};

#[cfg(windows)]
use mcp_server::cache_windows as windows;

const TARGET: &str = "bsl_vector_lifecycle";
const RECORD_BYTES: usize = 8192;
const QUEUE_RECORDS: usize = 511;
const SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
const SLOTS: usize = 8;
const DRAIN: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_millis(20);
const RETRY_LIMIT: Duration = Duration::from_secs(1);
static ACTIVE: OnceLock<Arc<Shared>> = OnceLock::new();

#[derive(Default)]
struct Shared {
    sender: OnceLock<Sender<Box<[u8]>>>,
    done: OnceLock<Receiver<()>>,
    started: AtomicBool,
    stop: AtomicBool,
    dropped: AtomicU64,
}

pub(super) struct JournalLayer(Arc<Shared>);
pub struct JournalGuard(Arc<Shared>);

pub(super) fn layer() -> (JournalLayer, JournalGuard) {
    let shared = Arc::new(Shared::default());
    let _ = ACTIVE.set(shared.clone());
    (JournalLayer(shared.clone()), JournalGuard(shared))
}

pub(super) fn filter(user: &str) -> Targets {
    // Only exact lifecycle directives override its own default; a global warn
    // or an unrelated target must not turn off this separate sink.
    let directive = user
        .split(',')
        .filter_map(|s| s.trim().split_once('='))
        .filter(|(target, _)| *target == TARGET)
        .map(|(_, level)| level)
        .next_back()
        .unwrap_or("info");
    format!("{TARGET}={directive}")
        .parse()
        .unwrap_or_else(|_| format!("{TARGET}=info").parse().expect("constant target filter"))
}

pub(super) fn activate(source: Option<&Path>) {
    let Some(shared) = ACTIVE.get() else { return };
    if shared.started.swap(true, Ordering::AcqRel) {
        return;
    }
    let source = source.map(Path::to_path_buf);
    let (sender, receiver) = bounded(QUEUE_RECORDS);
    let (finished, done) = bounded(1);
    let worker_shared = shared.clone();
    let spawn = std::thread::Builder::new().name("vector-journal".into()).spawn(move || {
        let mut diagnostics = Diagnostics::default();
        match journal_directory(source.as_deref()) {
            Ok(directory) => worker(receiver, &worker_shared, &directory, &mut diagnostics),
            Err(error) => diagnostics.report(error.kind()),
        }
        let _ = finished.send(());
    });
    match spawn {
        Ok(_) => {
            let _ = shared.done.set(done);
            let _ = shared.sender.set(sender);
        }
        Err(error) => Diagnostics::default().report(error.kind()),
    }
}

impl Drop for JournalGuard {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Release);
        if let Some(done) = self.0.done.get() {
            let _ = done.recv_timeout(DRAIN);
        }
    }
}

impl<S: Subscriber> Layer<S> for JournalLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() != TARGET {
            return;
        }
        if let Some(sender) = self.0.sender.get() {
            let mut visitor = RecordVisitor { sender, dropped: &self.0.dropped };
            event.record(&mut visitor);
        }
    }
}

struct RecordVisitor<'a> {
    sender: &'a Sender<Box<[u8]>>,
    dropped: &'a AtomicU64,
}
impl Visit for RecordVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() != "record" {
            return;
        }
        // The lifecycle serializer owns allowlisting and fixed-buffer encoding.
        // Reject malformed framing rather than persist a partial/oversized record.
        if value.len() + 1 > RECORD_BYTES || value.contains(['\n', '\r']) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut bytes = Vec::with_capacity(value.len() + 1);
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(b'\n');
        if self.sender.try_send(bytes.into_boxed_slice()).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

#[derive(Default)]
struct Diagnostics(Vec<(io::ErrorKind, Instant)>);
impl Diagnostics {
    fn report(&mut self, class: io::ErrorKind) {
        let now = Instant::now();
        if let Some((_, previous)) = self.0.iter_mut().find(|(kind, _)| *kind == class) {
            if now.duration_since(*previous) < Duration::from_secs(60) {
                return;
            }
            *previous = now;
        } else {
            self.0.push((class, now));
        }
        // Fixed class only: no paths, OS message, source content or record echo.
        tracing::warn!(target: "bsl_vector_journal_diagnostic", error_class = ?class,
            "vector journal unavailable; diagnostic history may have gaps");
    }
}

fn worker(
    receiver: Receiver<Box<[u8]>>,
    shared: &Shared,
    directory: &Path,
    diagnostics: &mut Diagnostics,
) {
    worker_with(receiver, shared, directory, diagnostics, |directory, record, dropped| {
        append(directory, record, dropped, SEGMENT_BYTES)
    })
}

fn worker_with(
    receiver: Receiver<Box<[u8]>>,
    shared: &Shared,
    directory: &Path,
    diagnostics: &mut Diagnostics,
    mut append: impl FnMut(&Path, &[u8], &mut u64) -> io::Result<()>,
) {
    let mut pending = None;
    let mut stopping = None;
    let mut backoff = RETRY;
    let mut refused = false;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            let since = stopping.get_or_insert_with(Instant::now);
            // Draining is for a sink that is merely busy; a refused one will not accept the
            // backlog in the remaining time either.
            if refused || since.elapsed() >= DRAIN {
                return;
            }
        }
        if pending.is_none() {
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(record) => pending = Some(record),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                Err(_) if stopping.is_some() => return,
                Err(_) => continue,
            }
        }
        let mut dropped = shared.dropped.swap(0, Ordering::AcqRel);
        match append(directory, pending.as_ref().unwrap(), &mut dropped) {
            Ok(()) => {
                pending = None;
                backoff = RETRY;
                refused = false;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                shared.dropped.fetch_add(dropped, Ordering::Relaxed);
                refused = false;
                std::thread::sleep(RETRY);
            }
            Err(error) => {
                shared.dropped.fetch_add(dropped, Ordering::Relaxed);
                diagnostics.report(error.kind());
                refused = true;
                // Nothing in this process repairs a refused target, so it is not retried at
                // lock-contention pace; the wait still ends as soon as shutdown begins.
                let until = Instant::now() + backoff;
                while Instant::now() < until && !shared.stop.load(Ordering::Acquire) {
                    std::thread::sleep(RETRY);
                }
                backoff = (backoff * 2).min(RETRY_LIMIT);
            }
        }
    }
}

fn journal_directory(source: Option<&Path>) -> io::Result<PathBuf> {
    let state = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "state directory unavailable"))?;
    let key = match source {
        Some(source) => {
            blake3::hash(source.canonicalize()?.as_os_str().as_encoded_bytes()).to_hex().to_string()
        }
        None => blake3::hash(b"reference-only").to_hex().to_string(),
    };
    let target = journal_target(&state, &key);
    if let Some(source) = source {
        let project = mcp_server::project::at(source)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        validate_journal_target(&project, &target)?;
    }
    // Validate the existing ancestry before creating anything below it.
    if let Some(existing) = state.ancestors().find(|path| path.exists()) {
        validate_ancestors(existing)?;
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&state)?;
    validate_ancestors(&state)?;
    prepare_directory(&state, &key)
}

fn journal_target(state: &Path, key: &str) -> PathBuf {
    state.join("bsl-analyzer").join("vector-journal").join(key)
}

fn validate_journal_target(project: &project_model::Project, target: &Path) -> io::Result<()> {
    if let Some(root) = mcp_server::WorkspaceCacheLayout::overlapping_source_root(project, target)?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "optional vector journal {} overlaps source root {}",
                target.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

fn prepare_directory(state: &Path, key: &str) -> io::Result<PathBuf> {
    let mut directory = state.join("bsl-analyzer");
    shared_directory(&directory)?;
    for component in ["vector-journal", key] {
        directory.push(component);
        private_directory(&directory)?;
    }
    Ok(directory)
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe journal target")
}

fn validate_ancestors(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid());
        }
        #[cfg(windows)]
        windows::reject_reparse(&metadata)?;
    }
    Ok(())
}

/// Process records already use this ancestor. Preserve safe existing access;
/// only the journal children contain diagnostic payload and need private modes.
fn shared_directory(path: &Path) -> io::Result<()> {
    if !path.try_exists()? {
        return private_directory(path);
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != current_uid() || metadata.mode() & 0o022 != 0 {
            return Err(invalid());
        }
    }
    #[cfg(windows)]
    {
        windows::reject_reparse(&metadata)?;
        windows::verify_shared(path)?;
    }
    Ok(())
}

fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.mode() & 0o077 != 0 || metadata.uid() != current_uid() {
            return Err(invalid());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows::private_directory(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(invalid())
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory effects.
    unsafe { libc::geteuid() }
}

fn file(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 || metadata.nlink() != 1
        {
            return Err(invalid());
        }
    }
    #[cfg(windows)]
    {
        windows::reject_reparse(&metadata)?;
        windows::verify_private(path)?;
    }
    Ok(file)
}

/// Every handle is closed before the permanent lock is dropped (also on Windows).
fn append(directory: &Path, record: &[u8], dropped: &mut u64, limit: u64) -> io::Result<()> {
    private_directory(directory)?;
    let lock = file(&directory.join("journal.lock"), true)?;
    if lock.metadata()?.len() != 0 {
        return Err(invalid());
    }
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        std::fs::TryLockError::Error(error) => error,
    })?;
    let result = (|| {
        // Reject foreign layout instead of deleting or trusting files we do not own.
        for entry in fs::read_dir(directory)? {
            let name = entry?.file_name();
            if name != "journal.lock"
                && !(0..SLOTS).any(|slot| name == format!("{slot}.jsonl").as_str())
            {
                return Err(invalid());
            }
        }
        if *dropped > 0 {
            let gap = bsl_search::lifecycle::journal_gap_record(*dropped, "queue_overflow");
            append_locked(directory, &gap, limit)?;
            *dropped = 0;
        }
        append_locked(directory, record, limit)
    })();
    // Explicit unlock also releases a lock transiently inherited by a concurrent
    // fork before exec; relying solely on close leaves that child holding it.
    let unlocked = lock.unlock();
    result.and(unlocked)
}

fn header(file: &mut File) -> io::Result<Option<u64>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = [0u8; 96];
    let len = file.read(&mut bytes)?;
    let Some(end) = bytes[..len].iter().position(|byte| *byte == b'\n') else { return Ok(None) };
    let value: serde_json::Value = match serde_json::from_slice(&bytes[..end]) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    Ok((value.get("journal_version").and_then(|v| v.as_u64()) == Some(1))
        .then(|| value.get("generation").and_then(|v| v.as_u64()))
        .flatten())
}

fn append_locked(directory: &Path, record: &[u8], limit: u64) -> io::Result<()> {
    let mut newest = None;
    for slot in 0..SLOTS {
        let path = directory.join(format!("{slot}.jsonl"));
        let mut segment = match file(&path, false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if segment.metadata()?.len() > limit {
            return Err(invalid());
        }
        if let Some(generation) = header(&mut segment)? {
            if newest.is_none_or(|(_, current)| generation > current) {
                newest = Some((slot, generation));
            }
        }
    }
    let (slot, generation) = newest.unwrap_or((0, 0));
    let mut segment = file(&directory.join(format!("{slot}.jsonl")), true)?;
    let mut recovery = if newest.is_none() && segment.metadata()?.len() > 0 {
        Some("corrupt_segment")
    } else {
        None
    };
    if newest.is_none() {
        segment.set_len(0)?;
        segment.seek(SeekFrom::Start(0))?;
        writeln!(segment, "{{\"journal_version\":1,\"generation\":0}}")?;
    }
    let size = segment.metadata()?.len();
    // Inspect only one bounded record tail, never copy a whole segment.
    let start = size.saturating_sub(RECORD_BYTES as u64);
    segment.seek(SeekFrom::Start(start))?;
    let mut tail = [0u8; RECORD_BYTES];
    let len = segment.read(&mut tail)?;
    let complete = tail[..len]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|offset| start + offset as u64 + 1)
        .ok_or_else(invalid)?;
    if complete != size {
        segment.set_len(complete)?;
        recovery = Some("partial_record");
    }
    // Reserve room for one possible recovery marker before choosing a slot.
    let reserve = if recovery.is_some() { RECORD_BYTES as u64 } else { 0 };
    if complete + record.len() as u64 + reserve > limit {
        drop(segment);
        let next = (slot + 1) % SLOTS;
        segment = file(&directory.join(format!("{next}.jsonl")), true)?;
        if segment.metadata()?.len() > 0 && header(&mut segment)?.is_none() {
            recovery = Some("corrupt_segment");
        }
        segment.set_len(0)?;
        segment.seek(SeekFrom::Start(0))?;
        let generation = generation.checked_add(1).ok_or_else(invalid)?;
        writeln!(segment, "{{\"journal_version\":1,\"generation\":{generation}}}")?;
    }
    segment.seek(SeekFrom::End(0))?;
    if let Some(reason) = recovery {
        segment.write_all(&bsl_search::lifecycle::journal_gap_record(1, reason))?;
    }
    segment.write_all(record)?;
    segment.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::{layer::SubscriberExt, Registry};

    struct TestDirectory {
        _temp: tempfile::TempDir,
        path: PathBuf,
    }
    impl TestDirectory {
        fn path(&self) -> &Path {
            &self.path
        }
    }
    fn private_temp() -> TestDirectory {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private");
        private_directory(&path).unwrap();
        TestDirectory { _temp: temp, path }
    }

    #[test]
    fn workspace_cache_scope_journal_gate_checks_the_created_leaf_before_mkdir() {
        let workspace = tempfile::tempdir().unwrap();
        let project = mcp_server::project::at(workspace.path()).unwrap();
        let state = workspace.path().join("state");
        let target = journal_target(&state, "scope-key");

        assert!(validate_journal_target(&project, &target).is_err());
        assert!(!state.exists(), "journal refusal must precede state-directory creation");
    }

    fn records(path: &Path) -> Vec<serde_json::Value> {
        let mut segments = Vec::new();
        for slot in 0..SLOTS {
            let path = path.join(format!("{slot}.jsonl"));
            if !path.exists() {
                continue;
            }
            let data = fs::read_to_string(path).unwrap();
            let mut lines = data.lines();
            let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
            segments.push((
                header["generation"].as_u64().unwrap(),
                lines
                    .map(|line| {
                        assert!(line.len() < RECORD_BYTES);
                        serde_json::from_str(line).unwrap()
                    })
                    .collect::<Vec<_>>(),
            ));
        }
        segments.sort_by_key(|(generation, _)| *generation);
        segments.into_iter().flat_map(|(_, records)| records).collect()
    }

    #[test]
    fn target_filter_and_record_boundary() {
        for (user, expected) in [
            ("warn", 1),
            ("warn,bsl_vector_lifecycle=off", 0),
            ("error,bsl_vector_lifecycle=debug", 2),
        ] {
            let shared = Arc::new(Shared::default());
            let (tx, rx) = bounded(QUEUE_RECORDS);
            shared.sender.set(tx).unwrap();
            let subscriber =
                Registry::default().with(JournalLayer(shared.clone()).with_filter(filter(user)));
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(target: "bsl_vector_lifecycle", record = "{\"kind\":\"info\"}");
                tracing::debug!(target: "bsl_vector_lifecycle", record = "{\"kind\":\"debug\"}");
                tracing::info!(target: "other", record = "CANARY_SOURCE_URL_TOKEN");
                tracing::info!(target: "bsl_vector_lifecycle", raw_error = "CANARY_RAW_ERROR");
            });
            let values: Vec<_> = rx.try_iter().collect();
            assert_eq!(values.len(), expected);
            for value in values {
                assert!(!String::from_utf8_lossy(&value).contains("CANARY"));
            }
        }
        let (tx, rx) = bounded(QUEUE_RECORDS);
        // Obtain an actual tracing Field rather than invent a second validator.
        let shared = Arc::new(Shared::default());
        shared.sender.set(tx.clone()).unwrap();
        let subscriber = Registry::default().with(JournalLayer(shared.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let huge = "x".repeat(RECORD_BYTES);
            tracing::info!(target: "bsl_vector_lifecycle", record = huge.as_str());
            tracing::info!(target: "bsl_vector_lifecycle", record = "{}\n{}");
        });
        assert!(rx.is_empty());
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn restart_tail_repair_overflow_gap_and_private_files() {
        let directory = private_temp();
        append(directory.path(), b"{\"run\":1}\n", &mut 0, SEGMENT_BYTES).unwrap();
        let path = directory.path().join("0.jsonl");
        let mut segment = file(&path, false).unwrap();
        segment.seek(SeekFrom::End(0)).unwrap();
        segment.write_all(b"{interrupted").unwrap();
        drop(segment);
        append(directory.path(), b"{\"run\":2}\n", &mut 9, SEGMENT_BYTES).unwrap();
        let values = records(directory.path());
        assert_eq!(values.first().unwrap()["run"], 1);
        assert_eq!(values.last().unwrap()["run"], 2);
        assert!(values.iter().any(|v| v["gap_reason"] == "partial_record"));
        assert!(values.iter().any(|v| v["dropped"] == 9));
        assert_eq!(fs::metadata(directory.path().join("journal.lock")).unwrap().len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for entry in fs::read_dir(directory.path()).unwrap() {
                assert_eq!(entry.unwrap().metadata().unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
        #[cfg(windows)]
        {
            windows::verify_private(&path).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn safe_existing_application_directory_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let application = root.path().join("bsl-analyzer");
        fs::create_dir(&application).unwrap();
        fs::set_permissions(&application, fs::Permissions::from_mode(0o755)).unwrap();
        let journal = prepare_directory(root.path(), "fixture-workspace").unwrap();
        assert_eq!(fs::metadata(&application).unwrap().permissions().mode() & 0o777, 0o755);
        for path in [application.join("vector-journal"), journal] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
        }
        fs::set_permissions(&application, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(prepare_directory(root.path(), "another-workspace").is_err());
        assert!(!application.join("vector-journal/another-workspace").exists());
    }

    #[test]
    fn unsafe_targets_fail_closed() {
        let directory = private_temp();
        let foreign = directory.path().join("foreign");
        fs::write(&foreign, b"preserve").unwrap();
        assert!(append(directory.path(), b"{}\n", &mut 0, SEGMENT_BYTES).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"preserve");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            let directory = private_temp();
            let target = directory.path().join("0.jsonl");
            symlink(&foreign, &target).unwrap();
            assert!(append(directory.path(), b"{}\n", &mut 0, SEGMENT_BYTES).is_err());
            assert_eq!(fs::read(&foreign).unwrap(), b"preserve");
            let directory = private_temp();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
            assert!(append(directory.path(), b"{}\n", &mut 0, SEGMENT_BYTES).is_err());
        }
    }

    #[test]
    fn queue_is_nonblocking_and_shutdown_is_bounded_with_locked_disk() {
        let directory = private_temp();
        let lock = file(&directory.path().join("journal.lock"), true).unwrap();
        lock.lock().unwrap();
        let shared = Arc::new(Shared::default());
        let (sender, receiver) = bounded(QUEUE_RECORDS);
        shared.sender.set(sender).unwrap();
        let subscriber = Registry::default().with(JournalLayer(shared.clone()));
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..1000 {
                tracing::info!(target: "bsl_vector_lifecycle", record = "{}");
            }
        });
        assert_eq!(receiver.len(), QUEUE_RECORDS);
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 1000 - QUEUE_RECORDS as u64);
        let (done_tx, done_rx) = bounded(1);
        shared.done.set(done_rx).unwrap();
        let worker_shared = shared.clone();
        let path = directory.path().to_owned();
        let handle = std::thread::spawn(move || {
            worker(receiver, &worker_shared, &path, &mut Diagnostics::default());
            done_tx.send(()).unwrap();
        });
        let (guard_done, guard_observed) = bounded(1);
        let guard = std::thread::spawn(move || {
            drop(JournalGuard(shared));
            guard_done.send(()).unwrap();
        });
        let result = guard_observed.recv_timeout(DRAIN + Duration::from_secs(1));
        drop(lock);
        assert!(result.is_ok(), "shutdown must finish while the disk lock remains held");
        guard.join().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn a_refused_journal_neither_spins_nor_delays_shutdown() {
        let directory = private_temp();
        // A foreign entry makes every append fail closed until someone removes it.
        fs::write(directory.path().join(".DS_Store"), b"x").unwrap();
        let shared = Arc::new(Shared::default());
        let (sender, receiver) = bounded(QUEUE_RECORDS);
        sender.send(Vec::from(&b"{}\n"[..]).into_boxed_slice()).unwrap();
        shared.sender.set(sender).unwrap();
        let (done_tx, done_rx) = bounded(1);
        shared.done.set(done_rx).unwrap();
        let attempts = Arc::new(AtomicU64::new(0));
        let worker_shared = shared.clone();
        let worker_attempts = attempts.clone();
        let path = directory.path().to_owned();
        let handle = std::thread::spawn(move || {
            let mut diagnostics = Diagnostics::default();
            worker_with(
                receiver,
                &worker_shared,
                &path,
                &mut diagnostics,
                |directory, record, dropped| {
                    worker_attempts.fetch_add(1, Ordering::Relaxed);
                    append(directory, record, dropped, SEGMENT_BYTES)
                },
            );
            done_tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_secs(1));
        // A fixed 20 ms retry makes about fifty attempts in this second.
        assert!(attempts.load(Ordering::Relaxed) < 15, "{attempts:?}");
        let started = Instant::now();
        drop(JournalGuard(shared));
        assert!(started.elapsed() < DRAIN / 2, "{:?}", started.elapsed());
        handle.join().unwrap();
    }

    #[test]
    fn failed_or_full_sink_preserves_real_store_and_private_payloads() {
        const CANARY: &str = "source-token-https://private.invalid/raw-error-canary";
        fn exercise() -> String {
            let directory = tempfile::tempdir().unwrap();
            let mut store = bsl_search::Store::open(&directory.path().join("search.db")).unwrap();
            let documents = [bsl_search::Document {
                title: "fixture".into(),
                body: CANARY.into(),
                kind: "text".into(),
            }];
            store
                .reindex_documents("code", "fixture", b"old", &documents, Some(&[vec![12345.875]]))
                .unwrap();
            store.reindex_documents("code", "fixture", b"new", &documents, None).unwrap();
            let ids = store
                .chunk_ids_for_file("code", bsl_search::CONFIGURATION_ROOT_ID, "fixture")
                .unwrap();
            store.set_chunk_embeddings(&[(ids[0], vec![12345.875])]).unwrap();
            store.clear_chunk_embedding(ids[0]).unwrap();
            store.set_chunk_embeddings(&[(ids[0], vec![12345.875])]).unwrap();
            // The subscriber accepts only the typed lifecycle record field.
            let mut record = bsl_search::lifecycle::Record::new(
                Path::new("fixture"),
                "fixture",
                bsl_search::lifecycle::Reason::Unknown,
            );
            let bytes = record.encode();
            let text = std::str::from_utf8(&bytes).unwrap().trim_end();
            tracing::info!(target: "bsl_vector_lifecycle", record = text, source = CANARY, token = CANARY, endpoint = CANARY, error = CANARY, vector = ?[12345.875]);
            format!(
                "{:?}",
                (
                    store.file_count().unwrap(),
                    store.chunk_count().unwrap(),
                    store.embedding_generation().unwrap(),
                    store.load_all_embeddings(1).unwrap(),
                    store.all_files().unwrap(),
                    store.chunks_by_ids(&ids).unwrap(),
                )
            )
        }
        let control = tracing::subscriber::with_default(
            tracing::subscriber::NoSubscriber::default(),
            exercise,
        );
        for disconnected in [false, true] {
            let shared = Arc::new(Shared::default());
            let (sender, receiver) = bounded(1);
            sender.send(Vec::from(b"{}\n").into_boxed_slice()).unwrap();
            shared.sender.set(sender).unwrap();
            let receiver = if disconnected {
                drop(receiver);
                None
            } else {
                Some(receiver)
            };
            let worker_shared = shared.clone();
            let (finished, completion) = bounded(1);
            let worker = std::thread::spawn(move || {
                let result = tracing::subscriber::with_default(
                    Registry::default().with(JournalLayer(worker_shared)),
                    exercise,
                );
                finished.send(result).unwrap();
            });
            // Watchdog only: a blocking send cannot finish while the full receiver stays undrained.
            assert_eq!(completion.recv_timeout(Duration::from_secs(10)).unwrap(), control);
            worker.join().unwrap();
            assert!(shared.dropped.load(Ordering::Relaxed) > 0);
            if let Some(receiver) = receiver {
                assert_eq!(receiver.len(), 1);
            }
        }
        let shared = Arc::new(Shared::default());
        let (sender, receiver) = bounded(QUEUE_RECORDS);
        shared.sender.set(sender).unwrap();
        assert_eq!(
            tracing::subscriber::with_default(
                Registry::default().with(JournalLayer(shared.clone())),
                exercise
            ),
            control
        );
        let records: Vec<_> = receiver.try_iter().collect();
        assert!(!records.is_empty());
        for bytes in records {
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(!text.contains(CANARY));
            assert!(!text.contains("12345.875"));
            let _: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        }
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn worker_delivers_overflow_gap_before_queued_data() {
        let directory = private_temp();
        let shared = Arc::new(Shared::default());
        let (sender, receiver) = bounded(QUEUE_RECORDS);
        shared.sender.set(sender).unwrap();
        let subscriber = Registry::default().with(JournalLayer(shared.clone()));
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..1000 {
                tracing::info!(target: "bsl_vector_lifecycle", record = "{}");
            }
        });
        let (done_tx, done_rx) = bounded(1);
        shared.done.set(done_rx).unwrap();
        let (appended_tx, appended_rx) = bounded(1);
        let worker_shared = shared.clone();
        let path = directory.path().to_owned();
        let handle = std::thread::spawn(move || {
            let mut appended_records = 0;
            worker_with(
                receiver,
                &worker_shared,
                &path,
                &mut Diagnostics::default(),
                |directory, record, dropped| {
                    let includes_gap = *dropped > 0;
                    append(directory, record, dropped, SEGMENT_BYTES)?;
                    appended_records += 1 + usize::from(includes_gap);
                    if appended_records == QUEUE_RECORDS + 1 {
                        let _ = appended_tx.send(());
                    }
                    Ok(())
                },
            );
            done_tx.send(()).unwrap();
        });
        let drained = appended_rx.recv_timeout(Duration::from_secs(30));
        drop(JournalGuard(shared));
        handle.join().unwrap();
        drained.expect("the worker should append the overflow gap and every queued record");
        let values = records(directory.path());
        assert_eq!(values.len(), QUEUE_RECORDS + 1);
        assert_eq!(values[0]["gap_reason"], "queue_overflow");
        assert_eq!(values[0]["dropped"], 1000 - QUEUE_RECORDS);
    }

    #[test]
    fn corrupt_headers_are_replaced_only_on_their_rotation_turn() {
        let directory = private_temp();
        let mut corrupt = file(&directory.path().join("0.jsonl"), true).unwrap();
        corrupt.write_all(b"broken header\n").unwrap();
        drop(corrupt);
        append(directory.path(), b"{\"run\":1}\n", &mut 0, 32768).unwrap();
        assert!(records(directory.path()).iter().any(|v| v["gap_reason"] == "corrupt_segment"));
        let mut corrupt = file(&directory.path().join("1.jsonl"), true).unwrap();
        corrupt.write_all(b"broken next slot\n").unwrap();
        drop(corrupt);
        append(directory.path(), b"{}\n", &mut 0, 32768).unwrap();
        assert_eq!(fs::read(directory.path().join("1.jsonl")).unwrap(), b"broken next slot\n");
        let record = format!("{{\"padding\":\"{}\"}}\n", "x".repeat(7900));
        for _ in 0..5 {
            append(directory.path(), record.as_bytes(), &mut 0, 32768).unwrap();
        }
        assert_eq!(
            records(directory.path())
                .iter()
                .filter(|v| v["gap_reason"] == "corrupt_segment")
                .count(),
            2
        );
    }

    #[test]
    fn fallback_is_rate_limited_nonrecursive_and_tolerates_a_broken_writer() {
        #[derive(Clone)]
        struct Capture(Arc<AtomicU64>);
        impl<S: Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
                assert_eq!(event.metadata().target(), "bsl_vector_journal_diagnostic");
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        }
        let count = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(Shared::default());
        let (sender, receiver) = bounded(QUEUE_RECORDS);
        shared.sender.set(sender).unwrap();
        let subscriber = Registry::default()
            .with(Capture(count.clone()))
            .with(JournalLayer(shared))
            .with(tracing_subscriber::fmt::layer().with_writer(|| BrokenWriter));
        tracing::subscriber::with_default(subscriber, || {
            let mut diagnostics = Diagnostics::default();
            diagnostics.report(io::ErrorKind::PermissionDenied);
            diagnostics.report(io::ErrorKind::PermissionDenied);
            diagnostics.report(io::ErrorKind::Other);
        });
        assert_eq!(count.load(Ordering::Relaxed), 2);
        assert!(receiver.is_empty());
    }

    #[test]
    fn journal_writer_subprocess() {
        let Some(path) = std::env::var_os("BSL_TEST_JOURNAL_DIRECTORY") else { return };
        let payload = "x".repeat(7900);
        for index in 0..4500 {
            let record = format!(
                "{{\"pid\":{},\"index\":{index},\"padding\":\"{payload}\"}}\n",
                std::process::id()
            );
            loop {
                match append(Path::new(&path), record.as_bytes(), &mut 0, SEGMENT_BYTES) {
                    Ok(()) => break,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::yield_now()
                    }
                    Err(error) => panic!("journal child failed: {:?}", error.kind()),
                }
            }
        }
    }

    #[test]
    fn concurrent_process_ring_has_exact_global_byte_and_file_bound() {
        let directory = private_temp();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..2 {
            children.push(
                std::process::Command::new(&executable)
                    .args([
                        "--exact",
                        "cli::logging::journal::tests::journal_writer_subprocess",
                        "--nocapture",
                    ])
                    .env("BSL_TEST_JOURNAL_DIRECTORY", directory.path())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        let entries: Vec<_> = fs::read_dir(directory.path()).unwrap().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 9);
        assert!(entries.iter().all(|entry| entry.metadata().unwrap().len() <= SEGMENT_BYTES));
        assert!(
            entries.iter().map(|entry| entry.metadata().unwrap().len()).sum::<u64>()
                <= 32 * 1024 * 1024
        );
        let values = records(directory.path());
        assert!(!values.is_empty());
        append(directory.path(), b"{\"restart\":true}\n", &mut 0, SEGMENT_BYTES).unwrap();
        assert_eq!(records(directory.path()).last().unwrap()["restart"], true);
    }
}
