use std::ffi::OsString;
use std::io;
use std::path::Path;

use super::{link_error, open_dir_component, validate_managed_path, ManagedBaselineDirectory};

/// The entry's own type, without following a symlink to its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedDirectoryEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// One directly listed member of an opened directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedDirectoryEntry {
    pub name: OsString,
    pub kind: ManagedDirectoryEntryKind,
}

impl ManagedBaselineDirectory {
    /// Stages bytes without recreating a parent another writer removed after validation.
    pub fn create_file_new_in_existing_directory(&self, path: &str) -> io::Result<std::fs::File> {
        let (directory, name) = self.open_parent(path, false)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        super::open_regular_file(&directory, &name, options, Path::new(path))
            .map(cap_std::fs::File::into_std)
    }

    /// Replaces staged bytes without recreating a removed source directory.
    pub fn replace_file_in_existing_directory(&self, from: &str, to: &str) -> io::Result<()> {
        let (from_directory, from_name) = self.open_parent(from, false)?;
        let (to_directory, to_name) = self.open_parent(to, false)?;
        match to_directory.symlink_metadata(&to_name) {
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                return Err(link_error(Path::new(to)));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        super::replace_regular_file(
            &from_directory,
            &from_name,
            &to_directory,
            &to_name,
            Path::new(from),
        )
    }

    /// Lists the opened source namespace without traversing a symlink in the directory path.
    pub fn read_directory(&self, path: &str) -> io::Result<Vec<ManagedDirectoryEntry>> {
        let mut directory = self.dir.try_clone()?;
        if !path.is_empty() {
            for component in validate_managed_path(path)?.components() {
                let std::path::Component::Normal(name) = component else { unreachable!() };
                directory = open_dir_component(&directory, Path::new(name), false)?;
            }
        }
        let mut entries = Vec::new();
        for entry in directory.entries()? {
            let entry = entry?;
            let kind = entry.file_type()?;
            entries.push(ManagedDirectoryEntry {
                name: entry.file_name(),
                kind: if kind.is_symlink() {
                    ManagedDirectoryEntryKind::Symlink
                } else if kind.is_file() {
                    ManagedDirectoryEntryKind::File
                } else if kind.is_dir() {
                    ManagedDirectoryEntryKind::Directory
                } else {
                    ManagedDirectoryEntryKind::Other
                },
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    /// Moves a regular file or directory only if its destination is still absent.
    pub fn rename_entry_new(&self, from: &str, to: &str) -> io::Result<()> {
        let (from_dir, from_name) = self.open_parent(from, false)?;
        let (to_dir, to_name) = self.open_parent(to, false)?;
        let metadata = from_dir.symlink_metadata(&from_name)?;
        if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
            return Err(link_error(Path::new(from)));
        }
        rename_new(&from_dir, &from_name, &to_dir, &to_name, metadata.is_dir())
    }
}

#[cfg(target_os = "linux")]
fn rename_new(
    from_dir: &cap_std::fs::Dir,
    from: &Path,
    to_dir: &cap_std::fs::Dir,
    to: &Path,
    _directory: bool,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;

    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // A separate existence check would let another writer's empty directory be replaced.
    let result = unsafe {
        libc::renameat2(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_new(
    from_dir: &cap_std::fs::Dir,
    from: &Path,
    to_dir: &cap_std::fs::Dir,
    to: &Path,
    directory: bool,
) -> io::Result<()> {
    use cap_fs_ext::{
        FollowSymlinks, OpenOptionsExt, OpenOptionsFollowExt, OpenOptionsMaybeDirExt,
    };
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES};

    let mut options = cap_std::fs::OpenOptions::new();
    options.access_mode(DELETE | FILE_READ_ATTRIBUTES).maybe_dir(true).follow(FollowSymlinks::No);
    let source = from_dir.open_with(from, &options)?;
    let metadata = source.metadata()?;
    if metadata.file_type().is_symlink()
        || metadata.is_dir() != directory
        || !(metadata.is_dir() || metadata.is_file())
    {
        return Err(link_error(from));
    }
    super::rename_windows_handle(&source, to_dir, to, false)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn rename_new(
    _from_dir: &cap_std::fs::Dir,
    _from: &Path,
    _to_dir: &cap_std::fs::Dir,
    _to: &Path,
    _directory: bool,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace metadata rename is unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_neither_recreates_a_removed_parent_nor_overwrites_an_existing_file() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open_project_root(temporary.path()).unwrap();
        std::fs::write(temporary.path().join("existing.tmp"), "foreign").unwrap();

        assert!(directory.create_file_new_in_existing_directory("Removed/stage.tmp").is_err());
        assert!(directory.create_file_new_in_existing_directory("existing.tmp").is_err());

        assert!(!temporary.path().join("Removed").exists());
        assert_eq!(
            std::fs::read_to_string(temporary.path().join("existing.tmp")).unwrap(),
            "foreign"
        );
    }

    #[test]
    fn staged_replacement_does_not_recreate_a_removed_target_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = ManagedBaselineDirectory::open_project_root(temporary.path()).unwrap();
        std::fs::write(temporary.path().join("stage.tmp"), "after").unwrap();

        assert!(directory
            .replace_file_in_existing_directory("stage.tmp", "Removed/Module.bsl")
            .is_err());
        assert!(!temporary.path().join("Removed").exists());
        assert_eq!(std::fs::read_to_string(temporary.path().join("stage.tmp")).unwrap(), "after");

        std::fs::write(temporary.path().join("Module.bsl"), "before").unwrap();
        directory.replace_file_in_existing_directory("stage.tmp", "Module.bsl").unwrap();
        assert_eq!(std::fs::read_to_string(temporary.path().join("Module.bsl")).unwrap(), "after");
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn a_resource_move_never_replaces_a_foreign_file_or_directory() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("Old.xml"), "source").unwrap();
        std::fs::write(temporary.path().join("New.xml"), "foreign").unwrap();
        std::fs::create_dir(temporary.path().join("Old")).unwrap();
        std::fs::write(temporary.path().join("Old/Module.bsl"), "body").unwrap();
        std::fs::create_dir(temporary.path().join("New")).unwrap();
        let directory = ManagedBaselineDirectory::open_project_root(temporary.path()).unwrap();

        assert!(directory.rename_entry_new("Old.xml", "New.xml").is_err());
        assert!(directory.rename_entry_new("Old", "New").is_err());
        assert_eq!(std::fs::read_to_string(temporary.path().join("New.xml")).unwrap(), "foreign");
        assert_eq!(
            std::fs::read_to_string(temporary.path().join("Old/Module.bsl")).unwrap(),
            "body"
        );
        assert!(temporary.path().join("New").is_dir());
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn an_owned_descriptor_and_its_source_container_move_without_changing_bytes() {
        assert_owned_descriptor_and_container_moves("");
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn a_nested_owned_descriptor_and_its_source_container_move_without_changing_bytes() {
        assert_owned_descriptor_and_container_moves("Catalogs");
    }

    #[cfg(any(target_os = "linux", windows))]
    fn assert_owned_descriptor_and_container_moves(parent: &str) {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join(parent);
        std::fs::create_dir_all(&source).unwrap();
        let original = b"\xef\xbb\xbf<Metadata uuid=\"unchanged\"/>\r\n";
        std::fs::write(source.join("Old.xml"), original).unwrap();
        std::fs::create_dir(source.join("Old")).unwrap();
        std::fs::write(source.join("Old/Module.bsl"), "source\r\n").unwrap();
        let directory = ManagedBaselineDirectory::open_project_root(temporary.path()).unwrap();
        let relative = |name: &str| {
            if parent.is_empty() {
                name.to_owned()
            } else {
                format!("{parent}/{name}")
            }
        };

        directory
            .rename_entry_new(&relative("Old.xml"), &relative("New.xml"))
            .unwrap_or_else(|error| panic!("descriptor under {parent:?}: {error:?}"));
        directory
            .rename_entry_new(&relative("Old"), &relative("New"))
            .unwrap_or_else(|error| panic!("container under {parent:?}: {error:?}"));

        assert_eq!(std::fs::read(source.join("New.xml")).unwrap(), original);
        assert_eq!(std::fs::read_to_string(source.join("New/Module.bsl")).unwrap(), "source\r\n");
        assert!(!source.join("Old.xml").exists());
        assert!(!source.join("Old").exists());
    }

    #[cfg(unix)]
    #[test]
    fn directory_listing_refuses_a_linked_parent_and_resource_move_refuses_a_link() {
        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("Foreign.xml"), "foreign").unwrap();
        std::os::unix::fs::symlink(outside.path(), temporary.path().join("Linked")).unwrap();
        let directory = ManagedBaselineDirectory::open_project_root(temporary.path()).unwrap();

        assert_eq!(
            directory.read_directory("").unwrap()[0].kind,
            ManagedDirectoryEntryKind::Symlink
        );
        assert!(directory.read_directory("Linked").is_err());
        assert!(directory.rename_entry_new("Linked", "Renamed").is_err());
        assert_eq!(std::fs::read_to_string(outside.path().join("Foreign.xml")).unwrap(), "foreign");
    }
}
