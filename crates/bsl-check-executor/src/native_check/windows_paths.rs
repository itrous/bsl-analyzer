use std::fs;
use std::io;
use std::os::windows::{
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
use std::path::{Component, Path};

use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
};

pub(crate) fn validate_canonical(path: &Path, directory: bool) -> io::Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid());
    }

    let mut prefix = Path::new("").to_path_buf();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&prefix)?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid());
        }
    }

    let canonical = path.canonicalize()?;
    if normalize(&canonical) != normalize(path) {
        return Err(invalid());
    }
    let metadata = fs::symlink_metadata(path)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(invalid());
    }

    let handle = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT | if directory { FILE_FLAG_BACKUP_SEMANTICS } else { 0 },
        )
        .open(path)?;
    let reopened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT | if directory { FILE_FLAG_BACKUP_SEMANTICS } else { 0 },
        )
        .open(&canonical)?;
    if identity(handle.as_raw_handle())? != identity(reopened.as_raw_handle())? {
        return Err(invalid());
    }
    Ok(())
}

fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = if let Some(tail) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{tail}")
    } else if let Some(tail) = text.strip_prefix(r"\\?\") {
        tail.to_owned()
    } else {
        text
    };
    text.trim_end_matches('\\').to_ascii_lowercase()
}

fn identity(handle: std::os::windows::io::RawHandle) -> io::Result<(u32, u64)> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid());
    }
    Ok((
        info.dwVolumeSerialNumber,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    ))
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "non-canonical or reparse path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_windows_profile_paths_accept_verbatim_canonicalization() {
        validate_canonical(&std::env::current_exe().unwrap(), false).unwrap();
        let temp = super::super::tests::canonical_tempdir();
        validate_canonical(temp.path(), true).unwrap();
    }
}
