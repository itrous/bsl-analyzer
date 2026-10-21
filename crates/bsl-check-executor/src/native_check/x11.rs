//! Addressable ownership and recovery for the X11 files used by xvfb-run.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const RECORD_PREFIX: &str = "bsl-check-x11-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    display: u16,
    uid: u32,
    pid: u32,
    start: u64,
    pgid: u32,
    lock_dev: u64,
    lock_ino: u64,
    socket_dev: u64,
    socket_ino: u64,
}

#[derive(Debug)]
pub(crate) struct Ownership {
    root: PathBuf,
    record: PathBuf,
    identity: Identity,
}

pub(crate) struct Tracker {
    root: PathBuf,
    job: PathBuf,
    display: u16,
    pgid: u32,
    ownership: Option<Ownership>,
}

impl Tracker {
    pub(crate) fn new(root: &Path, job: &Path, display: u16, pgid: u32) -> Self {
        Self { root: root.to_owned(), job: job.to_owned(), display, pgid, ownership: None }
    }

    pub(crate) fn observe(&mut self) -> io::Result<()> {
        if self.ownership.is_none() {
            self.ownership = capture(&self.root, &self.job, self.display, self.pgid)?;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<bool> {
        self.observe()?;
        if let Some(ownership) = self.ownership.take() {
            ownership.cleanup()?;
            return Ok(false);
        }
        occupied(&self.root, self.display)
    }
}

pub(crate) fn next_display(pid: u32, attempt: u16) -> u16 {
    100 + ((pid % 900 + attempt as u32) % 900) as u16
}

pub(crate) fn is_startup_conflict(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| {
        let text = String::from_utf8_lossy(&bytes);
        text.contains("Server is already active for display")
            || text.contains("Server is already running for display")
    })
}

pub(crate) fn occupied(root: &Path, display: u16) -> io::Result<bool> {
    let (lock, socket) = paths(root, display);
    Ok(exists_without_following(&lock)? || exists_without_following(&socket)?)
}

pub(crate) fn capture(
    root: &Path,
    job: &Path,
    display: u16,
    expected_pgid: u32,
) -> io::Result<Option<Ownership>> {
    let (lock, socket) = paths(root, display);
    let lock_meta = match checked_metadata(&lock, false)? {
        Some(meta) => meta,
        None => return Ok(None),
    };
    let socket_meta = match checked_metadata(&socket, true)? {
        Some(meta) => meta,
        None => return Ok(None),
    };
    if lock_meta.uid() != effective_uid() || socket_meta.uid() != effective_uid() {
        return Ok(None);
    }
    let lock_content = fs::read_to_string(&lock)?;
    if lock_content.len() > 32 {
        return Ok(None);
    }
    let Some(pid) = lock_content.trim().parse::<u32>().ok().filter(|pid| *pid > 1) else {
        return Ok(None);
    };
    let Some((process_group, start)) = process_identity(pid)? else {
        return Ok(None);
    };
    if process_group != expected_pgid {
        return Ok(None);
    }
    let process_uid = fs::metadata(format!("/proc/{pid}"))?.uid();
    if process_uid != effective_uid() || !listener_exists(root, display)? {
        return Ok(None);
    }
    let identity = Identity {
        display,
        uid: effective_uid(),
        pid,
        start,
        pgid: process_group,
        lock_dev: lock_meta.dev(),
        lock_ino: lock_meta.ino(),
        socket_dev: socket_meta.dev(),
        socket_ino: socket_meta.ino(),
    };
    let record = job.join(format!(".native-check-x11-{display}.owner"));
    write_record(&record, &identity)?;
    Ok(Some(Ownership { root: root.to_owned(), record, identity }))
}

pub(crate) fn recover_job(job: &Path, root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(job)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        if !name.starts_with(".native-check-x11-") || !name.ends_with(".owner") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != effective_uid()
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe X11 manifest"));
        }
        let identity = read_record(&entry.path())?;
        let expected_name = format!(".native-check-x11-{}.owner", identity.display);
        if name != expected_name || identity.uid != effective_uid() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "X11 manifest name mismatch"));
        }
        cleanup(&Ownership { root: root.to_owned(), record: entry.path(), identity })?;
    }
    Ok(())
}

impl Ownership {
    pub(crate) fn cleanup(&self) -> io::Result<()> {
        cleanup(self)
    }
}

fn cleanup(ownership: &Ownership) -> io::Result<()> {
    let id = &ownership.identity;
    if id.uid != effective_uid() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "X11 owner UID changed"));
    }
    match process_identity(id.pid)? {
        None => {}
        Some((pgid, start)) if pgid == id.pgid && start == id.start => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "owned X11 server is still alive",
            ));
        }
        Some(_) => {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "X11 PID was reused"))
        }
    }
    if listener_exists(&ownership.root, id.display)? {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "X11 listener is still present"));
    }
    let (lock, socket) = paths(&ownership.root, id.display);
    unlink_if_same(&socket, true, id.uid, id.socket_dev, id.socket_ino)?;
    unlink_if_same(&lock, false, id.uid, id.lock_dev, id.lock_ino)?;
    let record_meta = fs::symlink_metadata(&ownership.record)?;
    if !record_meta.is_file() || record_meta.file_type().is_symlink() || record_meta.uid() != id.uid
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "X11 manifest identity changed",
        ));
    }
    fs::remove_file(&ownership.record)
}

fn unlink_if_same(path: &Path, socket: bool, uid: u32, dev: u64, ino: u64) -> io::Result<()> {
    let Some(before) = checked_metadata(path, socket)? else { return Ok(()) };
    if before.uid() != uid || before.dev() != dev || before.ino() != ino {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "X11 pathname identity changed",
        ));
    }
    if listener_path_active(path)? {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "X11 listener remains active"));
    }
    let Some(before_unlink) = checked_metadata(path, socket)? else { return Ok(()) };
    if before_unlink.uid() != uid || before_unlink.dev() != dev || before_unlink.ino() != ino {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "X11 pathname was replaced"));
    }
    fs::remove_file(path)
}

fn write_record(path: &Path, id: &Identity) -> io::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    writeln!(
        file,
        "{RECORD_PREFIX}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        id.display,
        id.uid,
        id.pid,
        id.start,
        id.pgid,
        id.lock_dev,
        id.lock_ino,
        id.socket_dev,
        id.socket_ino
    )?;
    file.sync_all()
}

fn read_record(path: &Path) -> io::Result<Identity> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > 256
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe X11 manifest"));
    }
    let data = fs::read_to_string(path)?;
    let mut values = data.lines();
    if values.next() != Some(RECORD_PREFIX) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid X11 manifest"));
    }
    let mut next = || values.next().and_then(|value| value.parse::<u64>().ok());
    let display = next().and_then(|value| u16::try_from(value).ok());
    let uid = next().and_then(|value| u32::try_from(value).ok());
    let pid = next().and_then(|value| u32::try_from(value).ok());
    let start = next();
    let pgid = next().and_then(|value| u32::try_from(value).ok());
    let lock_dev = next();
    let lock_ino = next();
    let socket_dev = next();
    let socket_ino = next();
    if values.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid X11 manifest"));
    }
    let identity = Identity {
        display: display.ok_or_else(invalid_manifest)?,
        uid: uid.ok_or_else(invalid_manifest)?,
        pid: pid.ok_or_else(invalid_manifest)?,
        start: start.ok_or_else(invalid_manifest)?,
        pgid: pgid.ok_or_else(invalid_manifest)?,
        lock_dev: lock_dev.ok_or_else(invalid_manifest)?,
        lock_ino: lock_ino.ok_or_else(invalid_manifest)?,
        socket_dev: socket_dev.ok_or_else(invalid_manifest)?,
        socket_ino: socket_ino.ok_or_else(invalid_manifest)?,
    };
    if identity.display < 100
        || identity.pid <= 1
        || identity.start == 0
        || identity.pgid <= 1
        || identity.lock_ino == 0
        || identity.socket_ino == 0
    {
        return Err(invalid_manifest());
    }
    Ok(identity)
}

fn invalid_manifest() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid X11 manifest")
}

fn process_identity(pid: u32) -> io::Result<Option<(u32, u64)>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let close = stat
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process stat"))?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // An exited server waiting to be reaped holds neither the display nor its socket.
    if matches!(fields.first(), Some(&"Z") | Some(&"X")) {
        return Ok(None);
    }
    let pgid = fields
        .get(2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process PGID"))?;
    let start = fields
        .get(19)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process start time"))?;
    Ok(Some((pgid, start)))
}

fn listener_exists(root: &Path, display: u16) -> io::Result<bool> {
    let socket = root.join(format!(".X11-unix/X{display}"));
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        return Ok(true);
    }
    listener_path_active(&socket)
}

fn listener_path_active(path: &Path) -> io::Result<bool> {
    let expected = path.to_string_lossy();
    let abstract_path = format!("@{expected}");
    let sockets = fs::read_to_string("/proc/net/unix")?;
    Ok(sockets.lines().skip(1).any(|line| {
        let mut fields = line.split_whitespace();
        let _number = fields.next();
        let _ref_count = fields.next();
        let _protocol = fields.next();
        let _flags = fields.next();
        let _socket_type = fields.next();
        let state = fields.next();
        let _inode = fields.next();
        let pathname = fields.next();
        state == Some("01")
            && pathname.is_some_and(|name| name == expected || name == abstract_path)
    }))
}

fn paths(root: &Path, display: u16) -> (PathBuf, PathBuf) {
    (root.join(format!(".X{display}-lock")), root.join(format!(".X11-unix/X{display}")))
}

fn checked_metadata(path: &Path, socket: bool) -> io::Result<Option<fs::Metadata>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if meta.file_type().is_symlink()
        || (socket && !meta.file_type().is_socket())
        || (!socket && !meta.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unexpected X11 pathname type",
        ));
    }
    Ok(Some(meta))
}

fn exists_without_following(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    #[test]
    fn listener_path_active_reads_path_after_inode_column() {
        let temp = private_test_root();
        fs::create_dir(temp.path().join(".X11-unix")).unwrap();
        let socket = temp.path().join(".X11-unix/X381");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(listener_path_active(&socket).unwrap());
        drop(listener);
    }

    #[test]
    fn recovery_removes_only_dead_owned_socket_and_lock_by_identity() {
        let temp = private_test_root();
        let root = temp.path().to_owned();
        let job = temp.path().join("job");
        fs::create_dir(root.join(".X11-unix")).unwrap();
        fs::create_dir(&job).unwrap();
        let display = 381;
        let (lock, socket) = paths(&root, display);
        let python = "import os,socket,sys,time; open(sys.argv[1], 'w').write(str(os.getpid())+'\\n'); s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[2]); s.listen(); time.sleep(60)";
        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg(python)
            .arg(&lock)
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        super::super::tests::wait_until_sync(
            "fake X server socket",
            Duration::from_secs(1),
            Duration::from_millis(10),
            || socket.exists(),
        );
        assert!(
            socket.exists(),
            "fake X server did not create its pathname socket: {:?}",
            child.try_wait().unwrap()
        );
        assert_eq!(lock_content_pid(&lock), Some(pid));
        assert!(listener_exists(&root, display).unwrap());
        let owner = capture(&root, &job, display, pid).unwrap().unwrap();
        let record_path = owner.record.clone();
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        super::super::tests::wait_child(child, "fake X server exit").unwrap();
        owner.cleanup().unwrap();
        assert!(!socket.exists());
        assert!(!lock.exists());
        assert!(!record_path.exists());
    }

    #[test]
    fn recovery_preserves_replaced_socket_inode_and_symlinks() {
        let temp = private_test_root();
        let root = temp.path().to_owned();
        let job = temp.path().join("job");
        fs::create_dir(root.join(".X11-unix")).unwrap();
        fs::create_dir(&job).unwrap();
        let display = 382;
        let (lock, socket) = paths(&root, display);
        let python = "import os,socket,sys,time; open(sys.argv[1], 'w').write(str(os.getpid())+'\\n'); s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[2]); s.listen(); time.sleep(60)";
        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg(python)
            .arg(&lock)
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        super::super::tests::wait_until_sync(
            "fake X server socket",
            Duration::from_secs(1),
            Duration::from_millis(10),
            || socket.exists(),
        );
        assert!(
            socket.exists(),
            "fake X server did not create its pathname socket: {:?}",
            child.try_wait().unwrap()
        );
        assert_eq!(lock_content_pid(&lock), Some(pid));
        assert!(listener_exists(&root, display).unwrap());
        let owner = capture(&root, &job, display, pid).unwrap().unwrap();
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        super::super::tests::wait_child(child, "fake X server exit").unwrap();
        fs::remove_file(&socket).unwrap();
        let foreign = Command::new("python3")
            .arg("-c")
            .arg("import socket,sys,time; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(); time.sleep(60)")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        super::super::tests::wait_until_sync(
            "replacement fake X server listener",
            Duration::from_secs(1),
            Duration::from_millis(10),
            || listener_exists(&root, display).unwrap_or(false),
        );
        assert!(listener_exists(&root, display).unwrap());
        assert!(owner.cleanup().is_err());
        unsafe {
            libc::kill(foreign.id() as i32, libc::SIGKILL);
        }
        super::super::tests::wait_child(foreign, "replacement fake X server exit").unwrap();
        fs::remove_file(&socket).unwrap();
        fs::write(&socket, b"replacement").unwrap();
        assert!(owner.cleanup().is_err());
        assert!(socket.is_file());
        assert!(lock.is_file());
        fs::remove_file(&socket).unwrap();
        let target = temp.path().join("foreign-target");
        fs::write(&target, b"preserve").unwrap();
        std::os::unix::fs::symlink(&target, &socket).unwrap();
        assert!(owner.cleanup().is_err());
        assert!(fs::symlink_metadata(&socket).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(target).unwrap(), b"preserve");
    }

    #[test]
    fn recovery_rejects_pid_reuse_without_unlinking_owned_paths() {
        let temp = private_test_root();
        let root = temp.path().to_owned();
        let job = temp.path().join("job");
        fs::create_dir(root.join(".X11-unix")).unwrap();
        fs::create_dir(&job).unwrap();
        let display = 383;
        let (lock, socket) = paths(&root, display);
        fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let pgid = unsafe { libc::getpgid(0) } as u32;
        let mut owner = capture(&root, &job, display, pgid).unwrap().unwrap();
        let original_start = owner.identity.start;
        owner.identity.start = owner.identity.start.saturating_add(1);
        assert!(owner.cleanup().is_err());
        owner.identity.start = original_start;
        owner.identity.uid = owner.identity.uid.saturating_add(1);
        assert!(owner.cleanup().is_err());
        assert!(lock.is_file());
        assert!(socket.exists());
        assert!(owner.record.exists());
        drop(listener);
    }

    #[test]
    fn cleanup_treats_an_unreaped_zombie_owner_as_gone() {
        let temp = private_test_root();
        let root = temp.path().to_owned();
        let job = temp.path().join("job");
        fs::create_dir(root.join(".X11-unix")).unwrap();
        fs::create_dir(&job).unwrap();
        let child = Command::new("true").process_group(0).spawn().unwrap();
        let pid = child.id();
        // Keep the completed child unreaped so cleanup observes its zombie identity.
        super::super::tests::wait_until_sync(
            "test child exit",
            Duration::from_secs(60),
            Duration::from_millis(10),
            || {
                fs::read_to_string(format!("/proc/{pid}/stat")).ok().is_some_and(|stat| {
                    let close = stat.rfind(')').unwrap_or(0);
                    stat[close.saturating_add(1)..].split_whitespace().next() == Some("Z")
                })
            },
        );
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe {
            libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT | libc::WNOHANG)
        };
        assert_eq!(waited, 0);
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 1..].split_whitespace().collect();
        assert_eq!(fields[0], "Z");
        let record = job.join(".native-check-x11-384.owner");
        fs::write(&record, b"").unwrap();
        let owner = Ownership {
            root,
            record: record.clone(),
            identity: Identity {
                display: 384,
                uid: effective_uid(),
                pid,
                start: fields[19].parse().unwrap(),
                pgid: pid,
                lock_dev: 0,
                lock_ino: 1,
                socket_dev: 0,
                socket_ino: 1,
            },
        };

        let cleaned = owner.cleanup();
        super::super::tests::wait_child(child, "unreaped zombie child").unwrap();
        cleaned.unwrap();
        assert!(!record.exists());
    }

    fn lock_content_pid(path: &Path) -> Option<u32> {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    fn private_test_root() -> tempfile::TempDir {
        let temp = tempfile::Builder::new().prefix("x").tempdir().unwrap();
        let socket_path = temp.path().join(".X11-unix/X381");
        assert!(socket_path.as_os_str().len() < 100, "AF_UNIX pathname must stay bounded");
        temp
    }
}
