//! Narrow native ACL adapter; allocation ownership follows the Win32 API.
use std::{
    fs, io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    ptr,
};
use win_security_identifier::{GetCurrentSid, SecurityIdentifier};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_ALREADY_EXISTS, ERROR_SUCCESS},
    Security::{
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        CreateWellKnownSid, EqualSid, GetAce, GetSecurityDescriptorControl,
        GetSecurityDescriptorOwner, WinBuiltinAdministratorsSid, WinLocalSystemSid,
        ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED,
    },
    Storage::FileSystem::{CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT},
};

struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe private directory")
}

fn descriptor() -> io::Result<Descriptor> {
    let sid = SecurityIdentifier::get_current_user_sid().map_err(|_| invalid())?.to_string();
    let sddl: Vec<u16> =
        format!("O:{sid}D:P(A;OICI;FA;;;{sid})").encode_utf16().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: NUL terminated SDDL and writable out pointer; Win32 allocates with LocalAlloc.
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

pub fn reject_reparse(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid());
    }
    Ok(())
}

pub fn private_directory(path: &Path) -> io::Result<()> {
    let descriptor = descriptor()?;
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both inputs remain alive for the synchronous call.
    if unsafe { CreateDirectoryW(wide.as_ptr(), &security) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(invalid());
    }
    reject_reparse(&metadata)?;
    verify_private(path)
}

pub fn verify_private(path: &Path) -> io::Result<()> {
    verify_access(path, true)
}
pub fn verify_shared(path: &Path) -> io::Result<()> {
    verify_access(path, false)
}

fn verify_access(path: &Path, protected: bool) -> io::Result<()> {
    let expected = descriptor()?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut actual = ptr::null_mut();
    let mut owner = ptr::null_mut();
    let mut acl = ptr::null_mut();
    // SAFETY: out pointers are valid and returned pointers belong to `actual`.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
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
    let mut expected_owner = ptr::null_mut();
    let mut defaulted = 0;
    // SAFETY: valid security descriptors from the OS; no pointers escape this scope.
    unsafe {
        if GetSecurityDescriptorOwner(expected.0, &mut expected_owner, &mut defaulted) == 0
            || owner.is_null()
            || EqualSid(owner, expected_owner) == 0
            || acl.is_null()
            || (*acl).AceCount == 0
        {
            return Err(invalid());
        }
        if protected && fs::symlink_metadata(path)?.is_dir() {
            let mut control = 0;
            let mut revision = 0;
            if GetSecurityDescriptorControl(actual.0, &mut control, &mut revision) == 0
                || control & SE_DACL_PROTECTED == 0
            {
                return Err(invalid());
            }
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
        for index in 0..(*acl).AceCount {
            let mut ace = ptr::null_mut();
            if GetAce(acl, u32::from(index), &mut ace) == 0 {
                return Err(io::Error::last_os_error());
            }
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            // SYSTEM and administrators are outside the confidentiality boundary.
            // Reject all other trustees and nonstandard/object/callback ACEs.
            let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
            if ace.Header.AceType != 0
                || (EqualSid(sid, expected_owner) == 0
                    && EqualSid(sid, system.as_mut_ptr().cast()) == 0
                    && EqualSid(sid, admin.as_mut_ptr().cast()) == 0)
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
        System::IO::DeviceIoControl,
    };

    #[test]
    fn workspace_cache_scope_private_dacl_and_unprivileged_junction_rejection() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        private_directory(&target).unwrap();
        verify_private(&target).unwrap();
        let junction = temp.path().join("junction");
        fs::create_dir(&junction).unwrap();
        let target_text = target.canonicalize().unwrap().to_string_lossy().into_owned();
        let print = target_text.strip_prefix(r"\\?\").unwrap_or(&target_text);
        let substitute: Vec<u16> = format!(r"\??\{print}").encode_utf16().collect();
        let print: Vec<u16> = print.encode_utf16().collect();
        let mut data = Vec::new();
        data.extend_from_slice(&0xA0000003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
        let path_bytes = (substitute.len() + print.len() + 2) * 2;
        data.extend_from_slice(&((8 + path_bytes) as u16).to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        for value in [0, substitute.len() * 2, (substitute.len() + 1) * 2, print.len() * 2] {
            data.extend_from_slice(&(value as u16).to_le_bytes());
        }
        for value in substitute.into_iter().chain(Some(0)).chain(print).chain(Some(0)) {
            data.extend_from_slice(&value.to_le_bytes());
        }
        let handle = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&junction)
            .unwrap();
        let mut returned = 0;
        // SAFETY: owned writable directory handle and initialized mount-point buffer;
        // junctions, unlike symbolic links, need no administrator/symlink privilege.
        let result = unsafe {
            DeviceIoControl(
                handle.as_raw_handle(),
                0x000900A4,
                data.as_ptr().cast(),
                data.len() as u32,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        assert_ne!(result, 0, "{}", io::Error::last_os_error());
        drop(handle);
        assert!(reject_reparse(&fs::symlink_metadata(&junction).unwrap()).is_err());
        assert!(private_directory(&junction).is_err());
        verify_private(&target).unwrap();
    }
}
