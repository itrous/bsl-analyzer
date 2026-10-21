use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) struct RootLock {
    _file: File,
    path: PathBuf,
    identity: FileIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    first: u64,
    second: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockWaitError {
    Cancelled,
    Deadline,
    Unsafe,
}

impl RootLock {
    pub(crate) async fn acquire(
        root: &Path,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Self, LockWaitError> {
        #[cfg(windows)]
        {
            super::windows_paths::validate_canonical(root, true)
                .and_then(|_| verify_directory_owner(root))
                .map_err(|_| LockWaitError::Unsafe)?;
        }
        let path = root.join(".native-check-root.lock");
        loop {
            if cancel.is_cancelled() {
                return Err(LockWaitError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(LockWaitError::Deadline);
            }
            match try_acquire(&path) {
                Ok(Some(lock)) => {
                    lock.verify_path().map_err(|_| LockWaitError::Unsafe)?;
                    return Ok(lock);
                }
                Ok(None) => {}
                Err(_) => return Err(LockWaitError::Unsafe),
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(LockWaitError::Cancelled),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    return Err(LockWaitError::Deadline);
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    }

    pub(crate) fn verify_path(&self) -> io::Result<()> {
        #[cfg(windows)]
        {
            let root = self.path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "work root lock has no parent")
            })?;
            super::windows_paths::validate_canonical(root, true)?;
            verify_directory_owner(root)?;
        }
        let path_file = open_lock_file(&self.path, false)?;
        let identity = file_identity(&path_file)?;
        if identity != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "work root lock was replaced",
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) fn verify_directory_owner(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    super::windows_acl::verify_private_directory(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe work root directory"));
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    verify_current_sid_owner(&directory)
}

fn try_acquire(path: &Path) -> io::Result<Option<RootLock>> {
    let file = open_lock_file(path, true)?;
    validate_lock_file(path, &file)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if status == 0 {
            return Ok(Some(RootLock {
                identity: file_identity(&file)?,
                _file: file,
                path: path.to_owned(),
            }));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        Err(error)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        };
        let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED =
            unsafe { std::mem::zeroed() };
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if locked != 0 {
            return Ok(Some(RootLock {
                identity: file_identity(&file)?,
                _file: file,
                path: path.to_owned(),
            }));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32)
        {
            return Ok(None);
        }
        Err(error)
    }
}

fn open_lock_file(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        if create {
            return create_private_lock_file(path);
        }
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(windows)]
fn create_private_lock_file(path: &Path) -> io::Result<File> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_ALWAYS,
        },
    };

    let descriptor = super::windows_acl::descriptor()?;
    let mut security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "work root lock has no parent")
    })?;
    super::windows_paths::validate_canonical(parent, true)?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "work root lock has no file name")
    })?;
    let canonical_path = parent.canonicalize()?.join(file_name);
    let wide: Vec<u16> = canonical_path.as_os_str().encode_wide().chain(Some(0)).collect();
    // OPEN_ALWAYS applies this descriptor only when it creates a new file; existing locks are validated unchanged.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &mut security,
            OPEN_ALWAYS,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return Err(error);
    }
    // SAFETY: CreateFileW returned a valid owned handle, and File assumes its sole close responsibility.
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

fn validate_lock_file(path: &Path, file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    let path_metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || !path_metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "invalid work root lock type"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
            || metadata.dev() != path_metadata.dev()
            || metadata.ino() != path_metadata.ino()
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe work root lock"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
            || path_metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "work root lock is a reparse point",
            ));
        }
        verify_current_sid_owner(file)?;
    }
    Ok(())
}

fn file_identity(file: &File) -> io::Result<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(FileIdentity { first: metadata.dev(), second: metadata.ino() })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let result = unsafe {
            GetFileInformationByHandle(
                file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                &mut info,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileIdentity {
            first: info.dwVolumeSerialNumber as u64,
            second: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        })
    }
}

#[cfg(windows)]
fn verify_current_sid_owner(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use win_security_identifier::{GetCurrentSid, SecurityIdentifier};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            EqualSid, OWNER_SECURITY_INFORMATION,
        },
    };
    let current =
        SecurityIdentifier::get_current_user_sid().map_err(|_| io::Error::last_os_error())?;
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let equal =
        !owner.is_null() && unsafe { EqualSid(owner, current.as_sid() as *const _ as _) != 0 };
    unsafe {
        LocalFree(descriptor);
    }
    if equal {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "work root lock owner SID mismatch"))
    }
}

impl Drop for RootLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
            let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED =
                unsafe { std::mem::zeroed() };
            let _ = unsafe {
                UnlockFileEx(
                    self._file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                    0,
                    u32::MAX,
                    u32::MAX,
                    &mut overlapped,
                )
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[tokio::test]
    async fn independent_root_lock_wait_is_cancelled_and_releases_after_owner_drop() {
        let temp = super::super::tests::canonical_tempdir();
        let root = temp.path().join("private-root");
        #[cfg(windows)]
        super::super::windows_acl::private_directory(&root).unwrap();
        #[cfg(not(windows))]
        fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        #[cfg(windows)]
        {
            let lock_path = root.join(".native-check-root.lock");
            let default_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .expect("default-created fixture lock should be new");
            let default_owner_is_current = verify_current_sid_owner(&default_file).is_ok();
            drop(default_file);
            if !default_owner_is_current {
                assert!(matches!(
                    super::super::tests::guard_future(
                        RootLock::acquire(&root, deadline, &CancellationToken::new()),
                        "root-lock acquisition",
                    )
                    .await,
                    Err(LockWaitError::Unsafe)
                ));
                let existing = open_lock_file(&lock_path, false)
                    .expect("pre-existing fixture lock should remain readable");
                assert!(
                    verify_current_sid_owner(&existing).is_err(),
                    "opening a pre-existing lock must not repair its owner"
                );
                drop(existing);
                fs::remove_file(&lock_path).expect("remove only the test-owned fixture lock");
            }
        }
        let first = super::super::tests::guard_future(
            RootLock::acquire(&root, deadline, &CancellationToken::new()),
            "first root-lock acquisition",
        )
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let cancel_wait = cancel.clone();
        let root_path = root.to_owned();
        let waiter = RootLock::acquire(
            &root_path,
            Instant::now() + test_utils::wait::hang_guard(std::time::Duration::from_secs(5)),
            &cancel_wait,
        );
        let mut waiter = std::pin::pin!(waiter);
        std::future::poll_fn(|cx| {
            assert!(waiter.as_mut().poll(cx).is_pending(), "held root lock must park its waiter");
            std::task::Poll::Ready(())
        })
        .await;
        cancel.cancel();
        assert!(matches!(
            super::super::tests::guard_future(waiter, "cancelled root-lock waiter").await,
            Err(LockWaitError::Cancelled)
        ));
        first.verify_path().unwrap();
        drop(first);
        super::super::tests::guard_future(
            RootLock::acquire(
                &root,
                Instant::now() + std::time::Duration::from_secs(1),
                &CancellationToken::new(),
            ),
            "root-lock reacquisition after release",
        )
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn root_lock_fails_closed_when_its_path_is_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private-root");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let lock = super::super::tests::guard_future(
            RootLock::acquire(
                &root,
                Instant::now() + std::time::Duration::from_secs(1),
                &CancellationToken::new(),
            ),
            "root-lock acquisition before path replacement",
        )
        .await
        .unwrap();
        let path = root.join(".native-check-root.lock");
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(lock.verify_path().is_err());
    }
}
