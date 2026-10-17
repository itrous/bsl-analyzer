use std::io;
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};

/// A diagnostics-baseline directory pinned below the canonical project root.
/// All managed paths stay relative to opened directory handles.
#[derive(Debug)]
pub struct ManagedBaselineDirectory {
    dir: Dir,
    project_path: PathBuf,
}

/// What a stat says about one managed entry.
///
/// Describes the entry itself: a symlink reads as a symlink rather than as whatever it
/// points at, so a name that swaps a file for a link is a change and not a coincidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedEntryReading {
    pub len: u64,
    pub is_regular_file: bool,
    /// The permission bits, where the platform reports them. An entry that could not be
    /// opened for its mode reads the same as one that could on every other field, so
    /// without this a repair that only grants access moves nothing.
    pub mode: Option<u32>,
    pub modified_nanos: Option<u128>,
    /// `(device, inode)` where the platform reports them. The pair moves when an entry
    /// is replaced rather than rewritten, which a length and a timestamp can miss inside
    /// one timestamp granule.
    pub identity: Option<(u64, u64)>,
}

impl ManagedBaselineDirectory {
    pub fn open_project_root(project_root: &Path) -> io::Result<Self> {
        let canonical_root = std::fs::canonicalize(project_root)?;
        let dir = Dir::open_ambient_dir(canonical_root, ambient_authority())?;
        Ok(Self { dir, project_path: PathBuf::new() })
    }

    pub fn open(project_root: &Path, project_path: &str, create: bool) -> io::Result<Self> {
        let relative = validate_managed_path(project_path)?;
        let canonical_root = std::fs::canonicalize(project_root)?;
        let mut dir = Dir::open_ambient_dir(canonical_root, ambient_authority())?;
        let mut resolved = PathBuf::new();
        for component in relative.components() {
            let Component::Normal(name) = component else { unreachable!() };
            resolved.push(name);
            dir = open_dir_component(&dir, Path::new(name), create).map_err(|error| {
                if error.kind() == io::ErrorKind::InvalidInput {
                    link_error(&resolved)
                } else {
                    error
                }
            })?;
        }
        Ok(Self { dir, project_path: relative })
    }

    /// Validates a stored portable path and returns it relative to the project root.
    pub fn validated_relative_path(&self, path: &str) -> io::Result<PathBuf> {
        Ok(self.project_path.join(validate_managed_path(path)?))
    }

    /// Stat one managed entry through this handle, following nothing on the way to it
    /// and nothing at it.
    ///
    /// The counterpart of [`Self::open_file`] for callers that need to know what an
    /// entry looks like rather than what it contains. Reaching the same entry by
    /// re-assembling an absolute path and stat'ing that would walk intermediate links
    /// this handle was opened to refuse.
    ///
    /// An entry that is absent, or that lies behind a rejected component, is an error
    /// here rather than a reading: the caller decides what an unreadable name means.
    pub fn entry_reading(&self, path: &str) -> io::Result<ManagedEntryReading> {
        let (dir, name) = self.open_parent(path, false)?;
        let metadata = dir.symlink_metadata(&name)?;
        Ok(ManagedEntryReading {
            len: metadata.len(),
            is_regular_file: metadata.is_file(),
            mode: entry_mode(&metadata),
            modified_nanos: metadata.modified().ok().and_then(|modified| {
                modified
                    .into_std()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|since_epoch| since_epoch.as_nanos())
            }),
            identity: entry_identity(&metadata),
        })
    }

    pub fn open_file(&self, path: &str) -> io::Result<std::fs::File> {
        let (dir, name) = self.open_parent(path, false)?;
        open_regular_file(&dir, &name, read_options(), Path::new(path)).map(File::into_std)
    }

    pub fn create_file_new(&self, path: &str) -> io::Result<std::fs::File> {
        let (dir, name) = self.open_parent(path, true)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        open_regular_file(&dir, &name, options, Path::new(path)).map(File::into_std)
    }

    pub fn open_or_create_file(&self, path: &str) -> io::Result<std::fs::File> {
        let (dir, name) = self.open_parent(path, true)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        open_regular_file(&dir, &name, options, Path::new(path)).map(File::into_std)
    }

    #[cfg(windows)]
    pub fn sync_all(&self) -> io::Result<()> {
        match self.dir.try_clone()?.into_std_file().sync_all() {
            // Windows directory handles commonly reject FlushFileBuffers even on NTFS. The files
            // themselves are synced before rename; keep attempting the directory barrier where the
            // filesystem supports it without making the unsupported case fatal.
            Err(error) if matches!(error.raw_os_error(), Some(5 | 87)) => Ok(()),
            result => result,
        }
    }

    #[cfg(not(windows))]
    pub fn sync_all(&self) -> io::Result<()> {
        match self.dir.try_clone()?.into_std_file().sync_all() {
            // Some Unix capability handles use O_PATH and cannot be fsynced.
            Err(error) if error.raw_os_error() == Some(9) => Ok(()),
            result => result,
        }
    }

    /// Atomically replaces `to` with the regular file at `from`.
    pub fn replace_file(&self, from: &str, to: &str) -> io::Result<()> {
        let (from_dir, from_name) = self.open_parent(from, false)?;
        let (to_dir, to_name) = self.open_parent(to, true)?;
        if to_dir.symlink_metadata(&to_name).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(link_error(Path::new(to)));
        }
        replace_regular_file(&from_dir, &from_name, &to_dir, &to_name, Path::new(from))
    }

    /// Atomically publishes `from` at a new `to` name without replacing an existing object.
    pub fn persist_file_new(&self, from: &str, to: &str) -> io::Result<()> {
        let (from_dir, from_name) = self.open_parent(from, false)?;
        let (to_dir, to_name) = self.open_parent(to, true)?;
        let _source = open_regular_file(&from_dir, &from_name, read_options(), Path::new(from))?;
        from_dir.hard_link(&from_name, &to_dir, &to_name)?;
        if let Err(error) = from_dir.remove_file(&from_name) {
            tracing::warn!(%error, path = %Path::new(from).display(), "published file but temporary cleanup failed");
        }
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> io::Result<()> {
        let (dir, name) = self.open_parent(path, false)?;
        reject_link(&dir, &name, Path::new(path))?;
        dir.remove_file(name)
    }

    fn open_parent(&self, path: &str, create: bool) -> io::Result<(Dir, PathBuf)> {
        let relative = validate_managed_path(path)?;
        let name = relative.file_name().expect("validated managed path has a file name").into();
        let mut dir = self.dir.try_clone()?;
        let mut resolved = PathBuf::new();
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                let Component::Normal(component) = component else { unreachable!() };
                resolved.push(component);
                dir = open_dir_component(&dir, Path::new(component), create).map_err(|error| {
                    if error.kind() == io::ErrorKind::InvalidInput {
                        link_error(&resolved)
                    } else {
                        error
                    }
                })?;
            }
        }
        Ok((dir, name))
    }
}

#[cfg(unix)]
fn entry_mode(metadata: &cap_std::fs::Metadata) -> Option<u32> {
    use cap_std::fs::MetadataExt;
    Some(metadata.mode())
}

#[cfg(not(unix))]
fn entry_mode(_metadata: &cap_std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn entry_identity(metadata: &cap_std::fs::Metadata) -> Option<(u64, u64)> {
    use cap_std::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn entry_identity(_metadata: &cap_std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn open_dir_component(dir: &Dir, name: &Path, create: bool) -> io::Result<Dir> {
    match dir.open_dir_nofollow(name) {
        Ok(next) => Ok(next),
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            match dir.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            dir.open_dir_nofollow(name)
        }
        Err(error) => Err(error),
    }
}

fn open_regular_file(
    dir: &Dir,
    name: &Path,
    mut options: OpenOptions,
    display: &Path,
) -> io::Result<File> {
    options.follow(FollowSymlinks::No);
    // The type of an EXISTING entry is checked before opening: opening a FIFO with no
    // writer blocks forever, and this runs inside the LSP and MCP request paths, which
    // would then never answer. A missing entry is not an error here — the same helper
    // creates files — and the check after the open stays, closing the window between
    // the two.
    match dir.symlink_metadata(name) {
        Ok(metadata) if !metadata.is_file() => return Err(link_error(display)),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = dir.open_with(name, &options)?;
    if !file.metadata()?.is_file() {
        return Err(link_error(display));
    }
    Ok(file)
}

fn read_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    options
}

#[cfg(not(windows))]
fn replace_regular_file(
    from_dir: &Dir,
    from_name: &Path,
    to_dir: &Dir,
    to_name: &Path,
    display: &Path,
) -> io::Result<()> {
    let _source = open_regular_file(from_dir, from_name, read_options(), display)?;
    from_dir.rename(from_name, to_dir, to_name)
}

#[cfg(windows)]
fn replace_regular_file(
    from_dir: &Dir,
    from_name: &Path,
    to_dir: &Dir,
    to_name: &Path,
    display: &Path,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;

    use cap_fs_ext::OpenOptionsExt;
    use windows_sys::Wdk::Storage::FileSystem::{FileRenameInformation, NtSetInformationFile};
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO};
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let mut options = OpenOptions::new();
    options.access_mode(DELETE | FILE_READ_ATTRIBUTES).follow(FollowSymlinks::No);
    let source = open_regular_file(from_dir, from_name, options, display)?;
    let mut name: Vec<u16> = to_name.as_os_str().encode_wide().collect();
    let name_byte_len = name.len() * std::mem::size_of::<u16>();
    name.push(0);
    let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let byte_len = header + name.len() * std::mem::size_of::<u16>();
    let storage_len = byte_len.max(std::mem::size_of::<FILE_RENAME_INFO>());
    let words = storage_len.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; words];

    // FILE_RENAME_INFO ends in a variable-length UTF-16 name. The aligned backing buffer owns
    // both the header and that tail for the duration of the syscall.
    let status = unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = to_dir.as_raw_handle().cast();
        (*info).FileNameLength = name_byte_len as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        let mut io_status = IO_STATUS_BLOCK::default();
        NtSetInformationFile(
            source.as_raw_handle().cast(),
            &mut io_status,
            info.cast(),
            byte_len as u32,
            FileRenameInformation,
        )
    };
    if status < 0 {
        Err(io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_managed_path(path: &str) -> io::Result<PathBuf> {
    if path.is_empty()
        || path.contains('\\')
        || path.split('/').any(|component| component.is_empty() || component == ".")
    {
        return Err(invalid_path(path));
    }
    let path = PathBuf::from(path);
    if path.is_absolute()
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_path(path.to_string_lossy().as_ref()));
    }
    Ok(path)
}

fn reject_link(dir: &Dir, name: &Path, display: &Path) -> io::Result<()> {
    let metadata = dir.symlink_metadata(name)?;
    if metadata.file_type().is_symlink() {
        return Err(link_error(display));
    }
    Ok(())
}

fn invalid_path(path: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("invalid managed path: {path}"))
}

fn link_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("managed path is a link or has the wrong type: {}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::tempdir;

    /// Bound of a wait for an event that a regression would never deliver: 60 s under
    /// `timing_contract`, and a century otherwise, so an ordinary run waits for the event
    /// however long a loaded machine takes. A century still fits in an `Instant` sum.
    const HANG_WATCHDOG: std::time::Duration = if cfg!(timing_contract) {
        std::time::Duration::from_secs(60)
    } else {
        std::time::Duration::from_secs(100 * 365 * 24 * 60 * 60)
    };

    /// Opening a FIFO that has no writer blocks forever, and these calls run inside
    /// LSP and MCP request handling. The type must be rejected before the open, not
    /// after it — a check that never runs rejects nothing.
    #[cfg(unix)]
    #[test]
    fn a_fifo_in_the_managed_directory_does_not_block_the_reader() {
        let root = tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let fifo = root.path().join("baselines/manifest.json");
        let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0, "mkfifo failed");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let directory =
                ManagedBaselineDirectory::open(root.path(), "baselines", false).unwrap();
            let _ = tx.send(directory.open_file("manifest.json").is_err());
            drop(root);
        });
        match rx.recv_timeout(HANG_WATCHDOG) {
            Ok(rejected) => assert!(rejected, "a FIFO must be rejected, not opened"),
            Err(_) => panic!("open blocked on a FIFO instead of rejecting it"),
        }
        drop(directory);
    }

    #[test]
    fn partitioned_baseline_path_security_rejects_non_portable_paths() {
        for path in ["", ".", "a//b", "a/./b", "../b", "/tmp/b", "a\\b"] {
            assert!(validate_managed_path(path).is_err(), "{path}");
        }
        assert_eq!(
            validate_managed_path("objects/abcd/file.json").unwrap(),
            Path::new("objects/abcd/file.json")
        );
    }

    #[test]
    fn partitioned_baseline_path_security_uses_pinned_relative_operations() {
        let root = tempdir().unwrap();
        let managed = ManagedBaselineDirectory::open(root.path(), "state/baselines", true).unwrap();
        let mut file = managed.create_file_new("objects/key/value.json").unwrap();
        file.write_all(b"ok").unwrap();
        drop(file);
        let mut value = String::new();
        managed.open_file("objects/key/value.json").unwrap().read_to_string(&mut value).unwrap();
        assert_eq!(value, "ok");
        managed.replace_file("objects/key/value.json", "objects/key/renamed.json").unwrap();
        managed.sync_all().unwrap();
        managed.remove_file("objects/key/renamed.json").unwrap();
        assert_eq!(
            managed.validated_relative_path("objects/key/value.json").unwrap(),
            Path::new("state/baselines/objects/key/value.json")
        );
        assert!(managed.validated_relative_path("../outside.json").is_err());
    }

    #[test]
    fn partitioned_baseline_publish_replaces_manifest_but_not_immutable_object() {
        let root = tempdir().unwrap();
        let managed = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        let mut old = managed.create_file_new("manifest.json").unwrap();
        old.write_all(b"old").unwrap();
        drop(old);
        let mut replacement = managed.create_file_new(".manifest.tmp").unwrap();
        replacement.write_all(b"new").unwrap();
        drop(replacement);
        managed.replace_file(".manifest.tmp", "manifest.json").unwrap();

        let mut manifest = String::new();
        managed.open_file("manifest.json").unwrap().read_to_string(&mut manifest).unwrap();
        assert_eq!(manifest, "new");

        let mut existing = managed.create_file_new("objects/key/value.json").unwrap();
        existing.write_all(b"winner").unwrap();
        drop(existing);
        let mut candidate = managed.create_file_new("objects/key/.value.tmp").unwrap();
        candidate.write_all(b"candidate").unwrap();
        drop(candidate);
        let error = managed
            .persist_file_new("objects/key/.value.tmp", "objects/key/value.json")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        let mut value = String::new();
        managed.open_file("objects/key/value.json").unwrap().read_to_string(&mut value).unwrap();
        assert_eq!(value, "winner");
    }

    /// A reading describes the entry, not what it resolves to, and refuses to walk a
    /// link on the way to it. Stat'ing a re-assembled absolute path would do both.
    #[cfg(unix)]
    #[test]
    fn a_reading_describes_the_entry_and_walks_no_link_to_reach_it() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("target.json"), b"outside this project").unwrap();
        let managed = ManagedBaselineDirectory::open(root.path(), "baselines", true).unwrap();
        managed.create_file_new("objects/real.json").unwrap().write_all(b"ok").unwrap();

        let real = managed.entry_reading("objects/real.json").unwrap();
        assert!(real.is_regular_file);
        assert_eq!(real.len, 2);

        symlink(outside.path().join("target.json"), root.path().join("baselines/link.json"))
            .unwrap();
        let link = managed.entry_reading("link.json").unwrap();
        assert!(!link.is_regular_file, "a link reads as a link, not as its target");
        assert_ne!(link.len, "outside this project".len() as u64);

        symlink(outside.path(), root.path().join("baselines/objects/away")).unwrap();
        assert!(
            managed.entry_reading("objects/away/target.json").is_err(),
            "a name whose parent is a link must not be reached through it"
        );
        assert!(managed.entry_reading("objects/absent.json").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn partitioned_baseline_path_security_rejects_links() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::create_dir(root.path().join("state")).unwrap();
        symlink(outside.path(), root.path().join("state/baselines")).unwrap();
        assert!(ManagedBaselineDirectory::open(root.path(), "state/baselines", false).is_err());

        std::fs::remove_file(root.path().join("state/baselines")).unwrap();
        let managed = ManagedBaselineDirectory::open(root.path(), "state/baselines", true).unwrap();
        std::fs::create_dir_all(root.path().join("state/baselines/objects")).unwrap();
        symlink(
            outside.path().join("escape.json"),
            root.path().join("state/baselines/objects/escape.json"),
        )
        .unwrap();
        assert!(managed.open_file("objects/escape.json").is_err());
        assert!(managed.create_file_new("objects/escape.json").is_err());
    }
}
