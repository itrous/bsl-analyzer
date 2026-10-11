use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{self, Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use mcp_server::McpProfile;
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u32 = 1;
/// How much of the key digest names the record. Wide enough that two projects never
/// collide, short enough to stay readable in an error message.
const DIGEST_CHARS: usize = 32;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ProcessState {
    Starting,
    Running,
    Stopping,
    Stopped,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProcessRecord {
    schema_version: u32,
    instance_id: String,
    pid: u32,
    host: String,
    port: u16,
    profile: String,
    source_dir: Option<PathBuf>,
    executable: PathBuf,
    started_at: String,
    state: ProcessState,
}

#[derive(Debug)]
pub(super) struct ProcessRecordGuard {
    lock_file: File,
    record_file: File,
    path: PathBuf,
    record: ProcessRecord,
}

impl ProcessRecordGuard {
    pub(super) fn acquire(
        profile: McpProfile,
        source_dir: Option<PathBuf>,
        requested_address: SocketAddr,
    ) -> io::Result<Self> {
        let path = record_path(profile, source_dir.as_deref())?;
        if let Some(source) = source_dir.as_deref() {
            let project = mcp_server::project::at(source)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if let Some(root) =
                mcp_server::WorkspaceCacheLayout::overlapping_source_root(&project, &path)?
            {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!(
                    "automatic MCP process record {} overlaps source root {}; choose another source/cache layout",
                    path.display(), root.display()
                )));
            }
        }
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "process record has no parent directory")
        })?;
        std::fs::create_dir_all(parent)?;

        let lock_path = path.with_extension("json.lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let existing =
                    std::fs::read(&path).ok().and_then(|json| serde_json::from_slice(&json).ok());
                return Err(already_running_error(&path, existing.as_ref()));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let record_file =
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;

        let record = ProcessRecord {
            schema_version: SCHEMA_VERSION,
            instance_id: uuid::Uuid::new_v4().to_string(),
            pid: std::process::id(),
            host: requested_address.ip().to_string(),
            port: requested_address.port(),
            profile: profile.as_str().to_owned(),
            source_dir,
            executable: std::env::current_exe().or_else(|_| {
                std::env::args_os().next().map(PathBuf::from).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "current executable is unknown")
                })
            })?,
            started_at: chrono::Utc::now().to_rfc3339(),
            state: ProcessState::Starting,
        };
        let mut guard = Self { lock_file, record_file, path, record };
        if let Err(error) = guard.write_unchecked() {
            guard.record.state = ProcessState::Stopped;
            return Err(error);
        }
        Ok(guard)
    }

    pub(super) fn mark_running(&mut self) -> io::Result<()> {
        self.record.state = ProcessState::Running;
        self.write_owned()
    }

    pub(super) fn write_bound_process(&mut self, address: SocketAddr) -> io::Result<()> {
        self.record.host = address.ip().to_string();
        self.record.port = address.port();
        self.write_owned()
    }

    pub(super) fn mark_stopping(&mut self) -> io::Result<()> {
        self.record.state = ProcessState::Stopping;
        self.write_owned()
    }

    pub(super) fn mark_stopped(&mut self) -> io::Result<()> {
        self.record.state = ProcessState::Stopped;
        self.write_owned()
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }

    fn write_owned(&mut self) -> io::Result<()> {
        let instance_id = self.record.instance_id.clone();
        let existing = read_record(&mut self.record_file)?;
        if existing.instance_id != instance_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("process record ownership changed: {}", self.path.display()),
            ));
        }
        self.write_unchecked()
    }

    fn write_unchecked(&mut self) -> io::Result<()> {
        self.record_file.seek(SeekFrom::Start(0))?;
        self.record_file.set_len(0)?;
        serde_json::to_writer_pretty(&mut self.record_file, &self.record)?;
        self.record_file.write_all(b"\n")?;
        self.record_file.flush()
    }
}

impl Drop for ProcessRecordGuard {
    fn drop(&mut self) {
        if self.record.state != ProcessState::Stopped {
            let _ = self.mark_stopped();
        }
        let _ = self.lock_file.unlock();
    }
}

/// Where the record for one `(profile, source_dir)` pair lives.
///
/// Deliberately outside the project. A file lock belongs to an inode rather than to a
/// name, and `.build` is a cache directory users are told to clear — clearing it while
/// a server ran would leave the owner holding an unlinked inode and let a second server
/// claim a freshly created file. Keeping the record in our own state directory also
/// means the path can never be a symlink planted by the repository being analyzed.
fn record_path(profile: McpProfile, source_dir: Option<&Path>) -> io::Result<PathBuf> {
    if matches!(profile, McpProfile::Workspace) && source_dir.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace profile requires a source directory for its process record",
        ));
    }
    let base = dirs::state_dir().or_else(dirs::data_local_dir).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "application state directory is unavailable")
    })?;

    // Both key components are hashed: the reference profile also loads project
    // configuration from `--source-dir`, so two reference servers over different
    // projects are different servers and must not exclude each other.
    let mut hasher = blake3::Hasher::new();
    hasher.update(profile.as_str().as_bytes());
    hasher.update(b"\0");
    if let Some(source_dir) = source_dir {
        hasher.update(source_dir.as_os_str().as_encoded_bytes());
    }
    let digest = hasher.finalize().to_hex();

    Ok(base
        .join("bsl-analyzer")
        .join("mcp-http")
        .join(format!("{}.pid.json", &digest[..DIGEST_CHARS])))
}

fn read_record(file: &mut File) -> io::Result<ProcessRecord> {
    file.seek(SeekFrom::Start(0))?;
    let mut json = Vec::new();
    file.read_to_end(&mut json)?;
    serde_json::from_slice(&json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn already_running_error(path: &Path, existing: Option<&ProcessRecord>) -> io::Error {
    let message = match existing {
        Some(record) => format!(
            "MCP HTTP already serves this profile or project (pid {}, address {}:{}, record {})",
            record.pid,
            record.host,
            record.port,
            path.display()
        ),
        None => {
            format!("MCP HTTP already serves this profile or project (record {})", path.display())
        }
    };
    io::Error::new(io::ErrorKind::AlreadyExists, message)
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::{Path, PathBuf},
    };

    use mcp_server::McpProfile;

    use super::{record_path, ProcessRecord, ProcessRecordGuard, ProcessState};

    fn address(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    /// Records live in the user's state directory and outlive the process that wrote
    /// them, so a test keyed on a temporary project must take its own away again.
    struct RecordCleanup(PathBuf);

    impl RecordCleanup {
        fn for_key(profile: McpProfile, source_dir: Option<&Path>) -> Self {
            Self(record_path(profile, source_dir).expect("record path should resolve"))
        }
    }

    impl Drop for RecordCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("json.lock"));
        }
    }

    #[test]
    fn clearing_the_build_cache_does_not_release_the_lock() {
        let project = tempfile::tempdir().expect("temporary project should be created");
        let source_dir = project.path().canonicalize().expect("project path should canonicalize");
        let _cleanup = RecordCleanup::for_key(McpProfile::Workspace, Some(&source_dir));
        let _first = ProcessRecordGuard::acquire(
            McpProfile::Workspace,
            Some(source_dir.clone()),
            address(8021),
        )
        .expect("first server should acquire the project record");

        let build_dir = source_dir.join(".build");
        std::fs::create_dir_all(&build_dir).expect(".build should be creatable");
        std::fs::remove_dir_all(&build_dir).expect(".build should be removable");

        ProcessRecordGuard::acquire(McpProfile::Workspace, Some(source_dir), address(9021))
            .expect_err("clearing the derived-cache directory must not admit a second server");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_in_the_project_is_never_written() {
        let project = tempfile::tempdir().expect("temporary project should be created");
        let source_dir = project.path().canonicalize().expect("project path should canonicalize");
        let victim = project.path().join("victim");
        std::fs::write(&victim, "keep").expect("victim file should be written");
        let build_dir = source_dir.join(".build");
        std::fs::create_dir_all(&build_dir).expect(".build should be created");
        for planted in ["bsl-analyzer-mcp-http.pid.json", "bsl-analyzer-mcp-http.pid.json.lock"] {
            std::os::unix::fs::symlink(
                victim.canonicalize().expect("victim should canonicalize"),
                build_dir.join(planted),
            )
            .expect("symlink should be created");
        }

        let _cleanup = RecordCleanup::for_key(McpProfile::Workspace, Some(&source_dir));
        let _guard =
            ProcessRecordGuard::acquire(McpProfile::Workspace, Some(source_dir), address(8021))
                .expect("record should be acquired");

        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim should still be readable"),
            "keep",
            "the analyzed project must not be able to redirect the record write"
        );
    }

    #[test]
    fn reference_servers_over_different_projects_do_not_exclude_each_other() {
        let first_project = tempfile::tempdir().expect("first project should be created");
        let second_project = tempfile::tempdir().expect("second project should be created");

        let _first_cleanup =
            RecordCleanup::for_key(McpProfile::Reference, Some(first_project.path()));
        let _second_cleanup =
            RecordCleanup::for_key(McpProfile::Reference, Some(second_project.path()));
        let _first = ProcessRecordGuard::acquire(
            McpProfile::Reference,
            Some(first_project.path().to_path_buf()),
            address(8021),
        )
        .expect("first reference server should acquire its record");

        ProcessRecordGuard::acquire(
            McpProfile::Reference,
            Some(second_project.path().to_path_buf()),
            address(8022),
        )
        .expect("a reference server over a different project is a different server");
    }

    #[test]
    fn exclusive_record_lock_rejects_a_second_server_and_is_released_on_drop() {
        let project = tempfile::tempdir().expect("temporary project should be created");
        let source_dir = project.path().canonicalize().expect("project path should canonicalize");
        let _cleanup = RecordCleanup::for_key(McpProfile::Workspace, Some(&source_dir));
        let first = ProcessRecordGuard::acquire(
            McpProfile::Workspace,
            Some(source_dir.clone()),
            address(8021),
        )
        .expect("first server should acquire the project record");

        let error = ProcessRecordGuard::acquire(
            McpProfile::Workspace,
            Some(source_dir.clone()),
            address(9021),
        )
        .expect_err("second server for the same project must be rejected");
        let message = error.to_string();
        assert!(message.contains(&std::process::id().to_string()));
        assert!(message.contains("8021"));

        drop(first);
        ProcessRecordGuard::acquire(McpProfile::Workspace, Some(source_dir), address(9021))
            .expect("dropping the owner should release the system lock");
    }

    #[test]
    fn process_record_tracks_actual_address_and_stopped_state() {
        let project = tempfile::tempdir().expect("temporary project should be created");
        let source_dir = project.path().canonicalize().expect("project path should canonicalize");
        let _cleanup = RecordCleanup::for_key(McpProfile::Workspace, Some(&source_dir));
        let mut guard = ProcessRecordGuard::acquire(
            McpProfile::Workspace,
            Some(source_dir.clone()),
            address(8021),
        )
        .expect("record should be acquired");

        guard.write_bound_process(address(32123)).expect("bound address should be written");
        guard.mark_running().expect("running record should be written");
        let path = guard.path().to_owned();
        let running: ProcessRecord = serde_json::from_slice(
            &std::fs::read(&path).expect("running record should be readable"),
        )
        .expect("running record should be valid JSON");
        assert_eq!(running.pid, std::process::id());
        assert_eq!(running.host, "127.0.0.1");
        assert_eq!(running.port, 32123);
        assert_eq!(running.profile, "workspace");
        assert_eq!(running.source_dir.as_deref(), Some(source_dir.as_path()));
        assert_eq!(running.state, ProcessState::Running);

        guard.mark_stopping().expect("stopping state should be written");
        drop(guard);

        let stopped: ProcessRecord = serde_json::from_slice(
            &std::fs::read(path).expect("stopped record should remain on disk"),
        )
        .expect("stopped record should be valid JSON");
        assert_eq!(stopped.instance_id, running.instance_id);
        assert_eq!(stopped.state, ProcessState::Stopped);
    }
}
