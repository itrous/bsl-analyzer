use std::fmt;

use paths::{AbsPath, AbsPathBuf};
use stdx::path_exclusion::ExcludedPaths;

#[derive(Debug, Clone)]
pub enum Entry {
    Files(Vec<AbsPathBuf>),
    WatchOnlyFiles(Vec<AbsPathBuf>),
    Directories(Directories),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LoadMode {
    LoadContent,
    WatchOnly,
}

#[derive(Debug, Clone)]
pub struct FileRule {
    pub extensions: Vec<String>,
    pub load_mode: LoadMode,
}

#[derive(Debug, Clone, Default)]
pub struct Directories {
    pub extensions: Vec<String>,
    pub include: Vec<AbsPathBuf>,
    pub exclude: Vec<AbsPathBuf>,
    /// Directories taken out whatever else is said about them: decided before
    /// `include`, so an include root inside one — or equal to one — is out too.
    /// `exclude`, by contrast, yields to a more specific include.
    pub hard_exclude: ExcludedPaths,
    pub rules: Vec<FileRule>,
}

#[derive(Debug)]
pub struct Config {
    pub version: u32,
    pub load: Vec<Entry>,
    pub watch: Vec<usize>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LoadingProgress {
    Scanning,
    Started,
    Progress(usize),
    Finished,
}

pub enum Message {
    Progress {
        n_total: usize,
        n_done: LoadingProgress,
        dir: Option<AbsPathBuf>,
        config_version: u32,
    },
    Loaded {
        files: Vec<(AbsPathBuf, Option<Vec<u8>>)>,
    },
    Changed {
        files: Vec<(AbsPathBuf, Option<Vec<u8>>)>,
    },
    WatchOnly {
        files: Vec<AbsPathBuf>,
    },
    /// One or more paths were removed and may be directories. The loader cannot
    /// enumerate a removed subtree's children (they no longer exist on disk), so
    /// the consumer — which holds the file set — expands each path to its loaded
    /// descendants and removes them. A removed plain file produces no descendants
    /// and is a harmless no-op here (it is already delivered via `Changed`).
    RemovedRecursive {
        paths: Vec<AbsPathBuf>,
    },
}

pub type Sender = crossbeam_channel::Sender<Message>;

pub trait Handle: fmt::Debug {
    fn spawn(sender: Sender) -> Self
    where
        Self: Sized;

    fn set_config(&mut self, config: Config);

    /// Replace the exact list of watch-only files without a scan.
    ///
    /// Every listed file is put under observation as a load pass would. The names
    /// absent from the previous list are announced, in input order, by one
    /// [`Message::WatchOnly`]. No [`Message::Progress`] or [`Message::Loaded`] is
    /// sent, the tree is neither walked nor read, and no registration is released.
    ///
    /// Applied in the same queue as [`Handle::set_config`] and [`Handle::invalidate`],
    /// strictly after everything sent before it.
    fn set_watch_only_files(&mut self, files: Vec<AbsPathBuf>);

    fn invalidate(&mut self, path: AbsPathBuf);

    fn load_sync(&mut self, path: &AbsPath) -> Option<Vec<u8>>;
}

impl Entry {
    pub fn rs_files_recursively(base: AbsPathBuf) -> Entry {
        Entry::Directories(dirs(base, &[".git"]))
    }

    pub fn local_cargo_package(base: AbsPathBuf) -> Entry {
        Entry::Directories(dirs(base, &[".git", "target"]))
    }

    pub fn cargo_package_dependency(base: AbsPathBuf) -> Entry {
        Entry::Directories(dirs(base, &[".git", "/tests", "/examples", "/benches"]))
    }

    pub fn contains_file(&self, path: &AbsPath) -> bool {
        match self {
            Entry::Files(files) | Entry::WatchOnlyFiles(files) => files.iter().any(|it| it == path),
            Entry::Directories(dirs) => dirs.contains_file(path),
        }
    }

    pub fn contains_dir(&self, path: &AbsPath) -> bool {
        match self {
            Entry::Files(_) | Entry::WatchOnlyFiles(_) => false,
            Entry::Directories(dirs) => dirs.contains_dir(path),
        }
    }
}

impl Directories {
    pub fn classify_file(&self, path: &AbsPath) -> Option<LoadMode> {
        if !self.includes_path(path) {
            return None;
        }
        let ext = path.extension().unwrap_or_default();
        if self.extensions.iter().any(|it| it.eq_ignore_ascii_case(ext)) {
            return Some(LoadMode::LoadContent);
        }
        for rule in &self.rules {
            if rule.extensions.iter().any(|it| it.eq_ignore_ascii_case(ext)) {
                return Some(rule.load_mode);
            }
        }
        None
    }

    pub fn contains_file(&self, path: &AbsPath) -> bool {
        self.classify_file(path).is_some()
    }

    pub fn contains_dir(&self, path: &AbsPath) -> bool {
        self.includes_path(path)
    }

    /// Whether `path` lies in a directory of [`Self::hard_exclude`].
    pub fn is_hard_excluded(&self, path: &AbsPath) -> bool {
        self.hard_exclude.is_excluded(path.as_ref())
    }

    fn includes_path(&self, path: &AbsPath) -> bool {
        if self.is_hard_excluded(path) {
            return false;
        }
        let mut include: Option<&AbsPathBuf> = None;
        for incl in &self.include {
            if path.starts_with(incl) {
                include = Some(match include {
                    Some(prev) if prev.starts_with(incl) => prev,
                    _ => incl,
                });
            }
        }

        let include = match include {
            Some(it) => it,
            None => return false,
        };

        !self.exclude.iter().any(|excl| path.starts_with(excl) && excl.starts_with(include))
    }
}

fn dirs(base: AbsPathBuf, exclude: &[&str]) -> Directories {
    let exclude = exclude.iter().map(|it| base.join(it)).collect::<Vec<_>>();
    Directories {
        extensions: vec!["rs".to_owned()],
        include: vec![base],
        exclude,
        hard_exclude: ExcludedPaths::default(),
        rules: Vec::new(),
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Message::Loaded { files } => {
                f.debug_struct("Loaded").field("n_files", &files.len()).finish()
            }
            Message::Changed { files } => {
                f.debug_struct("Changed").field("n_files", &files.len()).finish()
            }
            Message::WatchOnly { files } => {
                f.debug_struct("WatchOnly").field("n_files", &files.len()).finish()
            }
            Message::RemovedRecursive { paths } => {
                f.debug_struct("RemovedRecursive").field("n_paths", &paths.len()).finish()
            }
            Message::Progress { n_total, n_done, dir, config_version } => f
                .debug_struct("Progress")
                .field("n_total", n_total)
                .field("n_done", n_done)
                .field("dir", dir)
                .field("config_version", config_version)
                .finish(),
        }
    }
}

#[test]
fn handle_is_dyn_compatible() {
    fn _assert(_: &dyn Handle) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(path: &str) -> AbsPathBuf {
        AbsPathBuf::assert_utf8(
            std::path::absolute(std::path::Path::new("target/vfs-loader-tests").join(path))
                .unwrap(),
        )
    }

    #[test]
    fn a_hard_exclusion_beats_equal_and_more_specific_includes() {
        let root = abs("root");
        let hidden = root.join(".tmp");
        let nested = hidden.join("declared-root");
        let dirs = Directories {
            extensions: vec!["bsl".to_owned()],
            include: vec![root.clone(), hidden.clone(), nested.clone()],
            exclude: Vec::new(),
            hard_exclude: ExcludedPaths::new([hidden.as_path()]),
            rules: vec![FileRule {
                extensions: vec!["xml".to_owned()],
                load_mode: LoadMode::WatchOnly,
            }],
        };

        assert!(!dirs.contains_dir(&hidden));
        assert!(!dirs.contains_dir(&nested));
        assert_eq!(dirs.classify_file(&nested.join("Module.bsl")), None);
        assert_eq!(dirs.classify_file(&hidden.join("Object.xml")), None);
        assert_eq!(dirs.classify_file(&root.join(".tmp2/Module.bsl")), Some(LoadMode::LoadContent));
        assert_eq!(dirs.classify_file(&root.join("allowed/Object.xml")), Some(LoadMode::WatchOnly));
    }

    #[test]
    fn the_legacy_exclude_still_yields_to_a_nested_include() {
        let root = abs("legacy");
        let hole = root.join("cache");
        let carved = hole.join("vendor");
        let dirs = Directories {
            extensions: vec!["bsl".to_owned()],
            include: vec![root.clone(), carved.clone()],
            exclude: vec![hole.clone()],
            hard_exclude: ExcludedPaths::default(),
            rules: Vec::new(),
        };

        assert!(!dirs.contains_file(&hole.join("Index.bsl")));
        assert!(dirs.contains_file(&carved.join("Module.bsl")));
    }
}
