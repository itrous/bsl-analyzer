use super::{check_error, NativeCheckError};
use crate::native_check::root_lock::{file_identity, FileIdentity};
use crate::native_check::source::canonical_file_source;
#[cfg(not(windows))]
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// A byte-verified private database copy. The source must be offline while it is copied:
/// repeated hashes detect drift, but cannot make reads atomic against an external writer.
pub(super) struct FileSourceSnapshot {
    original: PathBuf,
    directory: FileIdentity,
    reader: File,
    state: FileState,
    hash: blake3::Hash,
    path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct FileState {
    identity: FileIdentity,
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileSourceSnapshot {
    pub(super) async fn create(
        original: &Path,
        attempt: &Path,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Self, NativeCheckError> {
        check_budget(deadline, cancel)?;
        validate_locations(original, attempt)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let metadata = fs::symlink_metadata(attempt).map_err(|_| unavailable())?;
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o777 != 0o700
            {
                return Err(unavailable());
            }
        }
        #[cfg(windows)]
        crate::native_check::root_lock::verify_directory_owner(attempt)
            .map_err(|_| unavailable())?;
        let directory = directory_identity(original).map_err(|_| unavailable())?;
        let reader = open_reader(&original.join("1Cv8.1CD")).map_err(|_| unavailable())?;
        let state = file_state(&reader).map_err(|_| unavailable())?;
        let path = attempt.join("file-source");
        new_private_directory(&path).map_err(|_| unavailable())?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let target = options.open(path.join("1Cv8.1CD")).map_err(|_| unavailable())?;
        let target_identity = file_identity(&target).map_err(|_| unavailable())?;
        if target_identity == state.identity {
            return Err(unavailable());
        }
        let mut input = tokio::fs::File::from_std(reader.try_clone().map_err(|_| unavailable())?);
        let mut output = tokio::fs::File::from_std(target);
        let hash = transfer(&mut input, Some(&mut output), state.size, deadline, cancel).await?;
        output.flush().await.map_err(|_| unavailable())?;
        let snapshot = Self { original: original.to_owned(), directory, reader, state, hash, path };
        snapshot.verify_original(deadline, cancel).await?;
        let copied_hash =
            transfer(&mut output, None, snapshot.state.size, deadline, cancel).await?;
        if copied_hash != snapshot.hash {
            return Err(stale());
        }
        drop(output);
        let copied = open_reader(&snapshot.path.join("1Cv8.1CD")).map_err(|_| unavailable())?;
        let copied_state = file_state(&copied).map_err(|_| unavailable())?;
        if copied_state.identity != target_identity || copied_state.size != snapshot.state.size {
            return Err(stale());
        }
        // The copy is verified before any platform process is allowed to change its service data.
        snapshot.check_identity().map_err(|_| stale())?;
        check_budget(deadline, cancel)?;
        Ok(snapshot)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) async fn verify_original(
        &self,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<(), NativeCheckError> {
        check_budget(deadline, cancel)?;
        self.check_identity().map_err(|_| stale())?;
        let mut input = tokio::fs::File::from_std(self.reader.try_clone().map_err(|_| stale())?);
        let hash = transfer(&mut input, None, self.state.size, deadline, cancel).await?;
        self.check_identity().map_err(|_| stale())?;
        if hash != self.hash {
            return Err(stale());
        }
        check_budget(deadline, cancel)
    }

    fn check_identity(&self) -> io::Result<()> {
        if canonical_file_source(&self.original).as_deref() != Some(self.original.as_path())
            || directory_identity(&self.original)? != self.directory
            || file_state(&self.reader)? != self.state
            || file_state(&open_reader(&self.original.join("1Cv8.1CD"))?)? != self.state
        {
            return Err(unsafe_source());
        }
        Ok(())
    }
}

pub(super) fn validate_locations(
    original: &Path,
    destination: &Path,
) -> Result<(), NativeCheckError> {
    if canonical_file_source(original).as_deref() != Some(original) {
        return Err(unavailable());
    }
    validate_canonical(destination, true).map_err(|_| unavailable())?;
    let original = original.canonicalize().map_err(|_| unavailable())?;
    let destination = destination.canonicalize().map_err(|_| unavailable())?;
    if original.starts_with(&destination) || destination.starts_with(&original) {
        return Err(unavailable());
    }
    Ok(())
}

fn check_budget(deadline: Instant, cancel: &CancellationToken) -> Result<(), NativeCheckError> {
    if cancel.is_cancelled() {
        return Err(check_error("cancelled", "Native module check was cancelled"));
    }
    if Instant::now() >= deadline {
        return Err(check_error("deadline_exceeded", "Native module check exceeded its deadline"));
    }
    Ok(())
}

async fn transfer(
    input: &mut tokio::fs::File,
    mut output: Option<&mut tokio::fs::File>,
    size: u64,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<blake3::Hash, NativeCheckError> {
    check_budget(deadline, cancel)?;
    input.rewind().await.map_err(|_| unavailable())?;
    let mut total = 0_u64;
    let mut hash = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        check_budget(deadline, cancel)?;
        // Read no more than the admitted length plus one byte, even if a writer grows the file.
        let bound = (size - total).saturating_add(1).min(buffer.len() as u64) as usize;
        let count = input.read(&mut buffer[..bound]).await.map_err(|_| unavailable())?;
        check_budget(deadline, cancel)?;
        if count == 0 {
            break;
        }
        total = total.checked_add(count as u64).ok_or_else(stale)?;
        if total > size {
            return Err(stale());
        }
        hash.update(&buffer[..count]);
        if let Some(writer) = output.as_mut() {
            writer.write_all(&buffer[..count]).await.map_err(|_| unavailable())?;
            // Finish Tokio's buffered write before cancellation can release the workspace.
            writer.flush().await.map_err(|_| unavailable())?;
        }
    }
    if total != size {
        return Err(stale());
    }
    check_budget(deadline, cancel)?;
    Ok(hash.finalize())
}

fn open_reader(path: &Path) -> io::Result<File> {
    validate_canonical(path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };
        // Deny new write/delete opens for the lifetime of the original reader.
        options.share_mode(FILE_SHARE_READ).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    file_state(&file)?;
    Ok(file)
}

fn file_state(file: &File) -> io::Result<FileState> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(unsafe_source());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(unsafe_source());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the borrowed File owns the handle and info is a valid output buffer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.nNumberOfLinks != 1 || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(unsafe_source());
        }
    }
    Ok(FileState {
        identity: file_identity(file)?,
        size: metadata.len(),
        modified: metadata.modified()?,
        #[cfg(unix)]
        changed: {
            use std::os::unix::fs::MetadataExt;
            (metadata.ctime(), metadata.ctime_nsec())
        },
    })
}

fn directory_identity(path: &Path) -> io::Result<FileIdentity> {
    validate_canonical(path, true)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    file_identity(&options.open(path)?)
}

fn validate_canonical(path: &Path, directory: bool) -> io::Result<()> {
    #[cfg(windows)]
    return crate::native_check::windows_paths::validate_canonical(path, directory);
    #[cfg(not(windows))]
    {
        let metadata = fs::symlink_metadata(path)?;
        if !path.is_absolute()
            || path.canonicalize()? != path
            || (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
        {
            return Err(unsafe_source());
        }
        Ok(())
    }
}

fn new_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return crate::native_check::windows_acl::new_private_directory(path);
    #[cfg(not(windows))]
    {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)
    }
}

fn unsafe_source() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe file source snapshot")
}

fn unavailable() -> NativeCheckError {
    check_error("source_unavailable", "The private file source snapshot is unavailable")
}

fn stale() -> NativeCheckError {
    check_error("source_snapshot_stale", "The file source changed during native compilation")
}
