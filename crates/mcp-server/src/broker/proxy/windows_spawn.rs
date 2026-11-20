use std::cmp::Ordering;
use std::ffi::OsStr;
use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::ExitStatusExt,
};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::ptr::{null, null_mut};

use anyhow::Context;
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Globalization::CompareStringOrdinal,
    System::{
        Environment::{FreeEnvironmentStringsW, GetEnvironmentStringsW},
        JobObjects::IsProcessInJob,
        Threading::{
            CreateProcessW, GetExitCodeProcess, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB,
            CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS,
            PROCESS_INFORMATION, STARTUPINFOW,
        },
    },
};

/// Specification for re-executing the broker with ordinary arguments and environment overrides.
/// Stream handles and process flags are owned by the launcher.
pub struct BrokerCommand(Command);

impl BrokerCommand {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self(Command::new(program))
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.0.args(args);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.0.env(key, value);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.0.env_remove(key);
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.0.current_dir(dir);
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.0.get_args()
    }

    pub fn get_envs(&self) -> impl Iterator<Item = (&OsStr, Option<&OsStr>)> {
        self.0.get_envs()
    }
}

pub(super) struct BrokerChild {
    handle: OwnedHandle,
    #[cfg(test)]
    pid: u32,
}

impl BrokerChild {
    #[cfg(test)]
    pub(super) fn id(&self) -> u32 {
        self.pid
    }

    fn handle(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.poll(0)
    }

    #[cfg(test)]
    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.poll(windows_sys::Win32::System::Threading::INFINITE)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    }

    fn poll(&self, timeout: u32) -> io::Result<Option<ExitStatus>> {
        // The owned process handle remains valid through the wait and exit-code query.
        match unsafe { WaitForSingleObject(self.handle(), timeout) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// Launch the broker outside the launcher's Windows job, so it keeps serving after the
/// editor that started it is gone. A containing job that forbids breakaway keeps the
/// broker inside: it then ends with that job like any other child of it, the job itself
/// is left as it is, and the editor still gets its shared backend.
pub(super) fn spawn(cmd: &BrokerCommand) -> anyhow::Result<BrokerChild> {
    let child = match create(cmd, CREATE_BREAKAWAY_FROM_JOB) {
        Ok(child) => child,
        Err(breakaway) => create(cmd, 0).with_context(|| {
            format!("cannot launch detached broker (outside its job: {breakaway})")
        })?,
    };
    if remains_in_job(&child) {
        tracing::warn!(
            "a containing Windows job forbids breakaway: the broker ends together with that job"
        );
    }
    Ok(child)
}

fn create(cmd: &BrokerCommand, job_flags: u32) -> io::Result<BrokerChild> {
    let program = cmd.0.get_program();
    if !Path::new(program).is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker executable must be absolute",
        ));
    }
    let mut executable = wide(program)?;
    if executable.contains(&u16::from(b'"')) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid broker executable path"));
    }
    let mut command_line = Vec::new();
    append_argument(&mut command_line, &executable);
    executable.push(0);
    for arg in cmd.get_args() {
        command_line.push(u16::from(b' '));
        append_argument(&mut command_line, &wide(arg)?);
    }
    command_line.push(0);
    let cwd =
        cmd.0.get_current_dir().map(|dir| wide(dir.as_os_str())).transpose()?.map(|mut value| {
            value.push(0);
            value
        });
    let environment = environment_block(cmd)?;
    // A detached process has no console. Leaving STARTF_USESTDHANDLES unset keeps
    // the broker independent of the proxy's standard streams and other handles.
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: All input buffers are valid and terminated for the duration of the
    // call. CreateProcessW writes only into the mutable command line and output.
    let created = unsafe {
        CreateProcessW(
            executable.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT | job_flags,
            environment.as_ptr().cast(),
            cwd.as_ref().map_or(null(), |dir| dir.as_ptr()),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: A successful call returned two handles owned by the caller.
    let _thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
    Ok(BrokerChild {
        handle: unsafe { OwnedHandle::from_raw_handle(process.hProcess) },
        #[cfg(test)]
        pid: process.dwProcessId,
    })
}

fn remains_in_job(child: &BrokerChild) -> bool {
    let mut in_job = 0;
    // SAFETY: The owned process handle stays valid for the query.
    let queried = unsafe { IsProcessInJob(child.handle(), null_mut(), &mut in_job) };
    queried != 0 && in_job != 0
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let units: Vec<_> = value.encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "embedded NUL in broker command"));
    }
    Ok(units)
}

fn append_argument(out: &mut Vec<u16>, argument: &[u16]) {
    const QUOTE: u16 = b'"' as u16;
    const SLASH: u16 = b'\\' as u16;
    out.push(QUOTE);
    let mut slashes = 0;
    for &unit in argument {
        if unit == SLASH {
            slashes += 1;
        } else {
            if unit == QUOTE {
                out.extend(std::iter::repeat_n(SLASH, slashes + 1));
            }
            slashes = 0;
        }
        out.push(unit);
    }
    out.extend(std::iter::repeat_n(SLASH, slashes));
    out.push(QUOTE);
}

struct ParentEnvironment(*mut u16);

impl Drop for ParentEnvironment {
    fn drop(&mut self) {
        // SAFETY: The pointer came from GetEnvironmentStringsW and is freed once.
        unsafe { FreeEnvironmentStringsW(self.0) };
    }
}

fn compare_key(a: &[u16], b: &[u16]) -> Ordering {
    // SAFETY: The lengths describe the two live UTF-16 buffers. Environment
    // strings are bounded by the process environment API.
    let order =
        unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) };
    debug_assert_ne!(order, 0);
    order.cmp(&2)
}

fn environment_block(cmd: &BrokerCommand) -> io::Result<Vec<u16>> {
    let parent = unsafe { GetEnvironmentStringsW() };
    if parent.is_null() {
        return Err(io::Error::last_os_error());
    }
    let parent = ParentEnvironment(parent);
    let mut entries = Vec::new();
    let mut cursor = parent.0;
    // SAFETY: The OS owns a double-NUL-terminated block until parent is dropped.
    unsafe {
        while *cursor != 0 {
            let start = cursor;
            while *cursor != 0 {
                cursor = cursor.add(1);
            }
            let entry = std::slice::from_raw_parts(start, cursor.offset_from(start) as usize);
            // Start after the first unit to retain drive entries such as =C:=C:\.
            let separator = entry
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(index, unit)| (*unit == u16::from(b'=')).then_some(index))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid parent environment entry")
                })?;
            entries.push((entry[..separator].to_vec(), entry[separator + 1..].to_vec()));
            cursor = cursor.add(1);
        }
    }
    for (key, value) in cmd.get_envs() {
        let key = wide(key)?;
        if key.is_empty() || key.contains(&u16::from(b'=')) || key.len() > i32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid broker environment key",
            ));
        }
        entries.retain(|(existing, _)| compare_key(existing, &key) != Ordering::Equal);
        if let Some(value) = value {
            entries.push((key, wide(value)?));
        }
    }
    entries.sort_by(|(a, _), (b, _)| compare_key(a, b));
    let mut block = Vec::new();
    for (key, value) in entries {
        block.extend(key);
        block.push(u16::from(b'='));
        block.extend(value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_empty_unicode_and_backslash_arguments() {
        for (input, expected) in
            [("", r#""""#), ("Аргумент 𐐷", r#""Аргумент 𐐷""#), (r#"a\"b\"#, r#""a\\\"b\\""#)]
        {
            let mut line = Vec::new();
            append_argument(&mut line, &wide(OsStr::new(input)).unwrap());
            assert_eq!(String::from_utf16(&line).unwrap(), expected);
        }
        assert!(wide(OsStr::new("before\0after")).is_err());
    }
}
