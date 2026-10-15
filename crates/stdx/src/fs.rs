use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ignore::WalkBuilder;

use crate::path_exclusion::{ExcludedPaths, ResolvedDirs};

pub struct WalkResult<T> {
    items: Vec<T>,
}

impl<T> WalkResult<T> {
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Default)]
pub struct WalkConfig<'a> {
    pub extensions: &'a [&'a str],
    pub excludes: &'a [&'a Path],
    /// Directories taken out of the walk whatever spelling reaches them, the walk's
    /// roots included.
    pub excluded: Option<&'a ExcludedPaths>,
    pub follow_links: bool,
}

/// Whether the walk must not go into (or past) `entry`.
fn skips(config: &WalkConfig<'_>, entry: &ignore::DirEntry, resolved: &mut ResolvedDirs) -> bool {
    let path = entry.path();
    if config.excludes.iter().any(|ex| path.starts_with(ex)) {
        return true;
    }
    let Some(excluded) = config.excluded else { return false };
    let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
    if is_dir || entry.depth() == 0 || entry.path_is_symlink() {
        excluded.prunes_walked_dir(path, entry.path_is_symlink(), resolved)
    } else {
        excluded.excludes_walked_file(path)
    }
}

pub fn parallel_count(roots: &[&Path], config: &WalkConfig<'_>) -> usize {
    parallel_count_cancellable(roots, config, None)
}

pub fn parallel_count_cancellable(
    roots: &[&Path],
    config: &WalkConfig<'_>,
    cancel: Option<&AtomicBool>,
) -> usize {
    let count = AtomicUsize::new(0);

    for root in roots {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }

        let mut builder = WalkBuilder::new(root);
        builder.follow_links(config.follow_links).standard_filters(false).hidden(false);

        let extensions = config.extensions;

        builder.build_parallel().run(|| {
            let count = &count;
            let mut resolved = ResolvedDirs::default();
            Box::new(move |entry| {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    return ignore::WalkState::Quit;
                }

                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };

                if skips(config, &entry, &mut resolved) {
                    return ignore::WalkState::Skip;
                }
                let path = entry.path();

                let Some(file_type) = entry.file_type() else {
                    return ignore::WalkState::Continue;
                };
                if !file_type.is_file() {
                    return ignore::WalkState::Continue;
                }

                let ext = path.extension().and_then(|e| e.to_str());
                if ext.is_some_and(|x| extensions.iter().any(|e| e.eq_ignore_ascii_case(x))) {
                    count.fetch_add(1, Ordering::Relaxed);
                }

                ignore::WalkState::Continue
            })
        });
    }

    count.load(Ordering::Relaxed)
}

pub fn parallel_walk_paths(
    roots: &[&Path],
    config: &WalkConfig<'_>,
) -> WalkResult<std::path::PathBuf> {
    let items = std::sync::Mutex::new(Vec::new());

    for root in roots {
        let mut builder = WalkBuilder::new(root);
        builder.follow_links(config.follow_links).standard_filters(false).hidden(false);

        let extensions = config.extensions;

        builder.build_parallel().run(|| {
            let items = &items;
            let mut resolved = ResolvedDirs::default();
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };

                if skips(config, &entry, &mut resolved) {
                    return ignore::WalkState::Skip;
                }
                let path = entry.path();

                let Some(file_type) = entry.file_type() else {
                    return ignore::WalkState::Continue;
                };
                if !file_type.is_file() {
                    return ignore::WalkState::Continue;
                }

                let ext = path.extension().and_then(|e| e.to_str());
                if ext.is_some_and(|x| extensions.iter().any(|e| e.eq_ignore_ascii_case(x))) {
                    items.lock().unwrap().push(path.to_path_buf());
                }

                ignore::WalkState::Continue
            })
        });
    }

    WalkResult { items: items.into_inner().unwrap() }
}

pub fn parallel_read_files(
    roots: &[&Path],
    config: &WalkConfig<'_>,
) -> WalkResult<(std::path::PathBuf, Vec<u8>)> {
    let items = std::sync::Mutex::new(Vec::new());

    for root in roots {
        let mut builder = WalkBuilder::new(root);
        builder.follow_links(config.follow_links).standard_filters(false).hidden(false);

        let extensions = config.extensions;

        builder.build_parallel().run(|| {
            let items = &items;
            let mut resolved = ResolvedDirs::default();
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };

                if skips(config, &entry, &mut resolved) {
                    return ignore::WalkState::Skip;
                }
                let path = entry.path();

                let Some(file_type) = entry.file_type() else {
                    return ignore::WalkState::Continue;
                };
                if !file_type.is_file() {
                    return ignore::WalkState::Continue;
                }

                let ext = path.extension().and_then(|e| e.to_str());
                if ext.is_some_and(|x| extensions.iter().any(|e| e.eq_ignore_ascii_case(x))) {
                    if let Ok(contents) = std::fs::read(path) {
                        items.lock().unwrap().push((path.to_path_buf(), contents));
                    }
                }

                ignore::WalkState::Continue
            })
        });
    }

    WalkResult { items: items.into_inner().unwrap() }
}

pub fn parallel_read_transform<T, F>(
    roots: &[&Path],
    config: &WalkConfig<'_>,
    transform: F,
) -> WalkResult<T>
where
    T: Send,
    F: Fn(&Path, Vec<u8>) -> Option<T> + Sync,
{
    let items = std::sync::Mutex::new(Vec::new());

    for root in roots {
        let mut builder = WalkBuilder::new(root);
        builder.follow_links(config.follow_links).standard_filters(false).hidden(false);

        let extensions = config.extensions;

        builder.build_parallel().run(|| {
            let items = &items;
            let transform = &transform;
            let mut resolved = ResolvedDirs::default();
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };

                if skips(config, &entry, &mut resolved) {
                    return ignore::WalkState::Skip;
                }
                let path = entry.path();

                let Some(file_type) = entry.file_type() else {
                    return ignore::WalkState::Continue;
                };
                if !file_type.is_file() {
                    return ignore::WalkState::Continue;
                }

                let ext = path.extension().and_then(|e| e.to_str());
                if ext.is_some_and(|x| extensions.iter().any(|e| e.eq_ignore_ascii_case(x))) {
                    if let Ok(contents) = std::fs::read(path) {
                        if let Some(item) = transform(path, contents) {
                            items.lock().unwrap().push(item);
                        }
                    }
                }

                ignore::WalkState::Continue
            })
        });
    }

    WalkResult { items: items.into_inner().unwrap() }
}

pub fn parallel_read_transform_with_progress<T, F, P>(
    roots: &[&Path],
    config: &WalkConfig<'_>,
    transform: F,
    on_progress: P,
) -> WalkResult<T>
where
    T: Send,
    F: Fn(&Path, Vec<u8>) -> Option<T> + Sync,
    P: Fn() + Sync,
{
    let items = std::sync::Mutex::new(Vec::new());

    for root in roots {
        let mut builder = WalkBuilder::new(root);
        builder.follow_links(config.follow_links).standard_filters(false).hidden(false);

        let extensions = config.extensions;

        builder.build_parallel().run(|| {
            let items = &items;
            let transform = &transform;
            let on_progress = &on_progress;
            let mut resolved = ResolvedDirs::default();
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };

                if skips(config, &entry, &mut resolved) {
                    return ignore::WalkState::Skip;
                }
                let path = entry.path();

                let Some(file_type) = entry.file_type() else {
                    return ignore::WalkState::Continue;
                };
                if !file_type.is_file() {
                    return ignore::WalkState::Continue;
                }

                let ext = path.extension().and_then(|e| e.to_str());
                if ext.is_some_and(|x| extensions.iter().any(|e| e.eq_ignore_ascii_case(x))) {
                    if let Ok(contents) = std::fs::read(path) {
                        if let Some(item) = transform(path, contents) {
                            items.lock().unwrap().push(item);
                        }
                    }
                    on_progress();
                }

                ignore::WalkState::Continue
            })
        });
    }

    WalkResult { items: items.into_inner().unwrap() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parallel_count_empty() {
        let count = parallel_count(&[], &WalkConfig::default());
        assert_eq!(count, 0);
    }

    #[test]
    fn test_walk_config_default() {
        let config = WalkConfig::default();
        assert!(config.extensions.is_empty());
        assert!(config.excludes.is_empty());
        assert!(!config.follow_links);
    }

    #[test]
    fn test_walk_result_methods() {
        let result = WalkResult { items: vec![1, 2, 3] };
        assert_eq!(result.len(), 3);
        assert!(!result.is_empty());
        assert_eq!(result.into_vec(), vec![1, 2, 3]);
    }

    #[test]
    fn parallel_count_cancellable_quits_when_pre_set() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let baseline = parallel_count(
            &[here],
            &WalkConfig { extensions: &["rs"], excludes: &[], excluded: None, follow_links: false },
        );
        assert!(baseline >= 1, "expected at least one .rs file under stdx/, got {baseline}");

        let cancel = AtomicBool::new(true);
        let cancelled = parallel_count_cancellable(
            &[here],
            &WalkConfig { extensions: &["rs"], excludes: &[], excluded: None, follow_links: false },
            Some(&cancel),
        );
        assert_eq!(cancelled, 0, "pre-set cancel must short-circuit before any fetch_add");
    }

    #[test]
    fn parallel_count_cancellable_matches_uncancelled_when_disabled() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let cfg =
            WalkConfig { extensions: &["rs"], excludes: &[], excluded: None, follow_links: false };
        let plain = parallel_count(&[here], &cfg);
        let with_none = parallel_count_cancellable(&[here], &cfg, None);
        assert_eq!(plain, with_none);
    }
}
