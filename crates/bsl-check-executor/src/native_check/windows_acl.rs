use std::{
    fs, io,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
    ptr,
};

use win_security_identifier::{GetCurrentSid, SecurityIdentifier};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_ALREADY_EXISTS, ERROR_SUCCESS},
    Security::{
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_FILE_OBJECT,
        },
        CreateWellKnownSid, EqualSid, GetAce, GetSecurityDescriptorControl,
        GetSecurityDescriptorOwner, WinBuiltinAdministratorsSid, WinLocalSystemSid,
        ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED,
    },
    Storage::FileSystem::{
        CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT,
    },
};

pub(super) struct Descriptor(pub(super) PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

pub(super) fn descriptor() -> io::Result<Descriptor> {
    let sid = SecurityIdentifier::get_current_user_sid()
        .map_err(|_| io::Error::last_os_error())?
        .to_string();
    let sddl: Vec<u16> =
        format!("O:{sid}D:P(A;OICI;FA;;;{sid})").encode_utf16().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Descriptor(descriptor))
}

pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    let descriptor = descriptor()?;
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { CreateDirectoryW(wide.as_ptr(), &security) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
    }
    verify_private_directory(path)
}

pub(crate) fn verify_private_directory(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;

    super::windows_paths::validate_canonical(path, true)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid());
    }
    let expected = descriptor()?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut actual = ptr::null_mut();
    let mut owner = ptr::null_mut();
    let mut acl = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            directory.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut actual,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let actual = Descriptor(actual);
    unsafe {
        let mut expected_owner = ptr::null_mut();
        let mut defaulted = 0;
        let mut control = 0;
        let mut revision = 0;
        if GetSecurityDescriptorOwner(expected.0, &mut expected_owner, &mut defaulted) == 0
            || owner.is_null()
            || EqualSid(owner, expected_owner) == 0
            || acl.is_null()
            || (*acl).AceCount == 0
            || GetSecurityDescriptorControl(actual.0, &mut control, &mut revision) == 0
            || control & SE_DACL_PROTECTED == 0
        {
            return Err(invalid());
        }
        let mut system = [0u32; SECURITY_MAX_SID_SIZE as usize / 4];
        let mut admin = [0u32; SECURITY_MAX_SID_SIZE as usize / 4];
        for (kind, bytes) in
            [(WinLocalSystemSid, &mut system), (WinBuiltinAdministratorsSid, &mut admin)]
        {
            let mut size = std::mem::size_of_val(bytes) as u32;
            if CreateWellKnownSid(kind, ptr::null_mut(), bytes.as_mut_ptr().cast(), &mut size) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let current =
            SecurityIdentifier::get_current_user_sid().map_err(|_| io::Error::last_os_error())?;
        let mut current_trustee = false;
        for index in 0..(*acl).AceCount {
            let mut ace = ptr::null_mut();
            if GetAce(acl, u32::from(index), &mut ace) == 0 {
                return Err(io::Error::last_os_error());
            }
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
            let is_current = EqualSid(sid, current.as_sid() as *const _ as _) != 0;
            if ace.Header.AceType != 0
                || (!is_current
                    && EqualSid(sid, system.as_mut_ptr().cast()) == 0
                    && EqualSid(sid, admin.as_mut_ptr().cast()) == 0)
            {
                return Err(invalid());
            }
            current_trustee |= is_current;
        }
        if !current_trustee {
            return Err(invalid());
        }
    }
    Ok(())
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "work root DACL is not private")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_directory_has_current_sid_and_protected_dacl() {
        let temp = super::super::tests::canonical_tempdir();
        let private = temp.path().join("private");
        private_directory(&private).unwrap();
        verify_private_directory(&private).unwrap();
    }
}
