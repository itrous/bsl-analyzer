use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub(super) const LOG_BYTES: u64 = 4 * 1024 * 1024;

pub(super) struct BoundedLog {
    file: File,
    previous: PathBuf,
    limit: u64,
}

impl BoundedLog {
    pub(super) fn open(path: &Path, append: bool) -> io::Result<Self> {
        Self::with_limit(path, append, LOG_BYTES)
    }

    fn with_limit(path: &Path, append: bool, limit: u64) -> io::Result<Self> {
        let file = open_file(path, true)?;
        if !append {
            file.set_len(0)?;
        }
        let mut previous = path.as_os_str().to_owned();
        previous.push(".prev");
        Ok(Self { file, previous: previous.into(), limit })
    }

    fn write_locked(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let size = self.file.metadata()?.len();
            if size > 0 && (size >= self.limit || bytes.len() as u64 > self.limit - size) {
                let retained = size.min(self.limit);
                self.file.seek(SeekFrom::End(-(retained as i64)))?;
                let mut previous = open_file(&self.previous, false)?;
                previous.set_len(0)?;
                io::copy(&mut Read::by_ref(&mut self.file).take(retained), &mut previous)?;
                // Keep the inode: another daemon candidate may already have it open.
                self.file.set_len(0)?;
            }
            let size = self.file.metadata()?.len();
            let count = bytes.len().min((self.limit - size) as usize);
            #[cfg(windows)]
            self.file.seek(SeekFrom::End(0))?;
            self.file.write_all(&bytes[..count])?;
            bytes = &bytes[count..];
        }
        Ok(())
    }
}

fn open_file(path: &Path, append: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    // Windows append handles cannot truncate; append while holding the file lock instead.
    options.create(true).read(true).write(true).append(append && !cfg!(windows));
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
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
    }
    #[cfg(windows)]
    mcp_server::cache_windows::reject_reparse(&metadata)?;
    Ok(file)
}

impl Write for BoundedLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.file.try_lock() {
            Ok(()) => {}
            // Log contention must not stall a client or create a retry queue.
            Err(TryLockError::WouldBlock) => return Ok(bytes.len()),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let result = self.write_locked(bytes);
        let unlocked = self.file.unlock();
        result.and(unlocked).map(|()| bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_writer_preserves_append_and_explicit_truncate_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "previous\n").unwrap();
        BoundedLog::open(&path, true).unwrap().write_all(b"next\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous\nnext\n");
        BoundedLog::open(&path, false).unwrap().write_all(b"explicit\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "explicit\n");
    }

    #[test]
    fn a_live_writer_retains_only_two_segments_and_keeps_new_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.custom.log");
        let mut writer = BoundedLog::with_limit(&path, true, 64).unwrap();
        for index in 0..30 {
            writeln!(writer, "message {index:03}: {}", "x".repeat(16)).unwrap();
            assert!(path.metadata().unwrap().len() <= 64);
            assert!(writer.previous.metadata().map(|m| m.len()).unwrap_or(0) <= 64);
        }
        writer.write_all(b"latest message\n").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("latest message\n"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn an_oversized_initial_file_and_single_write_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, [b'a'; 256]).unwrap();
        let mut writer = BoundedLog::with_limit(&path, true, 64).unwrap();
        writer.write_all(&[b'b'; 192]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [b'b'; 64]);
        assert_eq!(std::fs::read(&writer.previous).unwrap(), [b'b'; 64]);
    }

    #[test]
    fn two_already_open_writers_follow_the_same_live_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut first = BoundedLog::with_limit(&path, true, 32).unwrap();
        let mut second = BoundedLog::with_limit(&path, true, 32).unwrap();
        for _ in 0..20 {
            first.write_all(b"first message\n").unwrap();
            second.write_all(b"second message\n").unwrap();
        }
        second.write_all(b"newest\n").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("newest\n"));
        assert!(path.metadata().unwrap().len() <= 32);
        assert!(first.previous.metadata().unwrap().len() <= 32);
    }

    #[test]
    fn a_busy_other_writer_drops_without_queueing_or_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut writer = BoundedLog::with_limit(&path, true, 64).unwrap();
        writer.write_all(b"before\n").unwrap();
        let lock = File::options().write(true).open(&path).unwrap();
        lock.try_lock().unwrap();
        writer.write_all(b"busy\n").unwrap();
        lock.unlock().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
        writer.write_all(b"after\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\nafter\n");
    }

    #[cfg(unix)]
    #[test]
    fn rotation_does_not_follow_a_previous_segment_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let unrelated = dir.path().join("source.bsl");
        std::fs::write(&unrelated, "untouched").unwrap();
        let mut writer = BoundedLog::with_limit(&path, true, 8).unwrap();
        std::os::unix::fs::symlink(&unrelated, &writer.previous).unwrap();
        writer.write_all(b"12345678").unwrap();
        assert!(writer.write_all(b"next").is_err());
        assert_eq!(std::fs::read_to_string(&unrelated).unwrap(), "untouched");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "12345678");
    }
}
