use std::process::{Child, Command};

use anyhow::Result;

pub struct LifecycleGuard {
    #[cfg(windows)]
    #[allow(dead_code)]
    job: windows_impl::JobHandle,
}

#[cfg(target_os = "linux")]
pub fn configure_parent_death(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    let expected_parent_pid = unsafe { libc::getpid() };

    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != expected_parent_pid {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub fn configure_parent_death(_cmd: &mut Command) {}

#[cfg(windows)]
pub fn adopt_child(child: &Child) -> Result<LifecycleGuard> {
    Ok(LifecycleGuard { job: windows_impl::assign_to_kill_on_close_job(child)? })
}

#[cfg(not(windows))]
pub fn adopt_child(_child: &Child) -> Result<LifecycleGuard> {
    Ok(LifecycleGuard {})
}

#[cfg(windows)]
mod windows_impl {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::ptr;

    use anyhow::{bail, Result};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct JobHandle(HANDLE);

    impl Drop for JobHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn assign_to_kill_on_close_job(child: &Child) -> Result<JobHandle> {
        unsafe {
            let handle = CreateJobObjectW(ptr::null(), ptr::null());
            if handle.is_null() {
                bail!("CreateJobObjectW failed: {}", std::io::Error::last_os_error());
            }
            let job = JobHandle(handle);

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            // Only the resident broker requests breakaway; ordinary descendants stay bound.
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;

            let ok = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                bail!("SetInformationJobObject failed: {}", std::io::Error::last_os_error());
            }

            let process_handle = child.as_raw_handle() as HANDLE;
            if AssignProcessToJobObject(handle, process_handle) == 0 {
                bail!("AssignProcessToJobObject failed: {}", std::io::Error::last_os_error());
            }

            Ok(job)
        }
    }
}
