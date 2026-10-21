use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::cache::{
    compute_sha256, get_cache_dir, get_current_version, is_reserved_cache_entry, read_current_link,
    read_stable_version, stable_app_path, sweep_stale_swap_temps, unique_stable_temp_path,
    update_current_link, verify_file_checksum, write_stable_version, SwapLock,
};
use crate::entities::{get_platform_binary, get_platform_launcher, FileInfo};
use crate::http;
use crate::messages::messages;
use crate::provider::ReleaseProvider;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_millis(300);

/// Launcher file names that may sit next to the running launcher, and the release
/// artifact each one holds. The bare name is the artifact of the *running* platform:
/// every installer writes the native launcher under it, so reading it as the Linux
/// artifact would replace a macOS launcher with an ELF binary. The suffixed names stay
/// fixed so a directory can still keep launchers for several platforms side by side.
fn launcher_mappings() -> [(&'static str, &'static str); 3] {
    [
        ("bsl-analyzer", get_platform_launcher()),
        ("bsl-analyzer.exe", "bsl-analyzer-windows-amd64.exe"),
        ("bsl-analyzer-mac", "bsl-analyzer-darwin-arm64"),
    ]
}

fn create_http_client() -> Result<reqwest::blocking::Client> {
    http::build_client(|builder| builder.connect_timeout(CONNECT_TIMEOUT).timeout(READ_TIMEOUT))
}

fn create_download_client() -> Result<reqwest::blocking::Client> {
    http::build_client(|builder| builder.connect_timeout(CONNECT_TIMEOUT))
}

fn fetch_version_verbose(
    provider: &dyn ReleaseProvider,
    client: &reqwest::blocking::Client,
) -> Result<String> {
    let m = messages();
    eprint!("{}", m.connecting);
    match provider.fetch_latest_version(client) {
        Ok(v) => {
            eprintln!("{}", m.ok);
            Ok(v)
        }
        Err(e) => {
            eprintln!("{}", m.failed);
            Err(e)
        }
    }
}

fn try_fetch_latest_version_fast(provider: &dyn ReleaseProvider) -> Result<String> {
    let client = http::build_client(|builder| {
        builder.connect_timeout(UPDATE_CHECK_TIMEOUT).timeout(UPDATE_CHECK_TIMEOUT)
    })?;
    provider.fetch_latest_version(&client)
}

pub fn ensure_analyzer(provider: &dyn ReleaseProvider, sync_update: bool) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;

    sweep_stale_swap_temps(&cache_dir);

    match read_current_link(&cache_dir) {
        None => {
            download_latest(provider, &cache_dir)?;
        }
        Some(target) => {
            let full_path = if target.is_absolute() { target } else { cache_dir.join(&target) };
            if full_path.exists() {
                check_updates_if_needed(provider, &cache_dir, sync_update)?;
            } else {
                download_latest(provider, &cache_dir)?;
            }
        }
    }

    swap_and_resolve(&cache_dir)
}

/// The cached app already is the current version, so starting it swaps nothing.
fn converged_stable(cache_dir: &Path) -> Option<PathBuf> {
    let current_ver = get_current_version(cache_dir)?;
    let stable = stable_app_path(cache_dir);
    (stable.exists() && read_stable_version(cache_dir).as_deref() == Some(current_ver.as_str()))
        .then_some(stable)
}

fn swap_and_resolve(cache_dir: &Path) -> Result<PathBuf> {
    // Taking the swap lock creates a file, so a converged cache must resolve without it:
    // that one write is what otherwise stops the analyzer on a read-only home.
    if let Some(stable) = converged_stable(cache_dir) {
        return Ok(stable);
    }

    let _lock = SwapLock::acquire(cache_dir)?;

    // `perform_swap` moves the binary into place before it writes the sidecar, so a
    // sidecar naming the current version always has that binary beside it. Losing the
    // race against a concurrent swap therefore costs only this second look.
    if let Some(stable) = converged_stable(cache_dir) {
        return Ok(stable);
    }

    let Some(current_ver) = get_current_version(cache_dir) else {
        bail!("bsl-analyzer is not installed and could not be downloaded");
    };

    let stable = stable_app_path(cache_dir);
    let versioned = cache_dir.join(format!("bsl-analyzer-{}", current_ver));

    if versioned.exists() {
        match perform_swap(cache_dir, &versioned, &current_ver) {
            Ok(()) => return Ok(stable),
            Err(e) => eprintln!("warn: stable app swap skipped: {e:#}"),
        }
    }

    if versioned.exists() {
        return Ok(versioned);
    }

    bail!("bsl-analyzer is not installed and could not be downloaded")
}

fn perform_swap(cache_dir: &Path, versioned: &Path, current_ver: &str) -> Result<()> {
    let temp = unique_stable_temp_path(cache_dir);

    if let Err(e) = fs::copy(versioned, &temp) {
        let _ = fs::remove_file(&temp);
        return Err(e)
            .with_context(|| format!("Failed to copy {} to stable temp", versioned.display()));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm_result = fs::metadata(&temp).and_then(|m| {
            let mut perms = m.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&temp, perms)
        });
        if let Err(e) = perm_result {
            let _ = fs::remove_file(&temp);
            return Err(e).context("Failed to set permissions on stable temp");
        }
    }

    let stable = stable_app_path(cache_dir);
    if let Err(e) = fs::rename(&temp, &stable) {
        let _ = fs::remove_file(&temp);
        return Err(e)
            .with_context(|| format!("Failed to swap stable app to version {}", current_ver));
    }

    write_stable_version(cache_dir, current_ver)?;
    Ok(())
}

pub fn ensure_specific_version(provider: &dyn ReleaseProvider, version: &str) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;

    let resolved_version = if version == "latest" {
        let http_client = create_http_client()?;
        fetch_version_verbose(provider, &http_client)?
    } else {
        version.to_string()
    };

    let binary_name = format!("bsl-analyzer-{}", resolved_version);
    let binary_path = cache_dir.join(&binary_name);

    if binary_path.exists() {
        if verify_existing_binary(provider, &resolved_version, &binary_path).is_ok() {
            cleanup_cached_versions(&cache_dir, std::slice::from_ref(&resolved_version), 1);
            return Ok(binary_path);
        }
        let _ = fs::remove_file(&binary_path);
    }

    let path = download_version_without_linking(provider, &cache_dir, &resolved_version)?;
    cleanup_cached_versions(&cache_dir, &[resolved_version], 1);
    Ok(path)
}

fn check_updates_if_needed(
    provider: &dyn ReleaseProvider,
    cache_dir: &Path,
    sync_update: bool,
) -> Result<()> {
    if sync_update {
        let Ok(latest_version) = try_fetch_latest_version_fast(provider) else {
            return Ok(());
        };

        let current_version = get_current_version(cache_dir);
        if Some(&latest_version) == current_version.as_ref() {
            return Ok(());
        }

        download_version(provider, cache_dir, &latest_version)?;
    } else if let Ok(exe) = env::current_exe() {
        let _ = Command::new(exe)
            .arg("--launcher-update")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    Ok(())
}

pub fn update_analyzer(provider: &dyn ReleaseProvider) -> Result<()> {
    let cache_dir = get_cache_dir()?;
    let http_client = create_http_client()?;

    let latest_version = fetch_version_verbose(provider, &http_client)?;
    let current_version = get_current_version(&cache_dir);

    let m = messages();
    if Some(&latest_version) == current_version.as_ref() {
        eprintln!("{}", m.up_to_date.replace("{}", &latest_version));
        cleanup_cached_versions(&cache_dir, &[], 1);
        return Ok(());
    }

    eprintln!(
        "{}",
        m.updating
            .replace("{:?}", &format!("{:?}", current_version))
            .replace("{}", &latest_version)
    );
    download_version(provider, &cache_dir, &latest_version)?;
    cleanup_cached_versions(&cache_dir, &[], 1);

    Ok(())
}

pub fn verify_installation(provider: &dyn ReleaseProvider) -> Result<()> {
    let cache_dir = get_cache_dir()?;

    let m = messages();
    let target = match read_current_link(&cache_dir) {
        Some(t) => t,
        None => bail!("{}", m.no_installation),
    };
    let version = target
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("bsl-analyzer-"))
        .context("Invalid current link")?;

    eprintln!("{}", m.verifying.replace("{}", version));

    let http_client = create_http_client()?;
    let manifest = provider.fetch_manifest(&http_client, version)?;

    let binary_path = if target.is_absolute() { target } else { cache_dir.join(&target) };

    let platform = get_platform_binary();
    let expected = manifest.files.get(platform).context("Platform not found in manifest")?;

    verify_file_checksum(&binary_path, &expected.sha256)?;

    eprintln!("{}", m.verified);
    Ok(())
}

pub fn self_update_launcher(provider: &dyn ReleaseProvider) -> Result<()> {
    let m = messages();
    let http_client = create_http_client()?;

    let latest_version = fetch_version_verbose(provider, &http_client)?;
    let manifest = provider.fetch_manifest(&http_client, &latest_version)?;

    eprintln!("{}", m.self_update_downloading.replace("{}", &latest_version));

    let current_exe = env::current_exe().context("Cannot determine current executable path")?;
    let launcher_dir = current_exe.parent().context("Cannot determine launcher directory")?;

    let download_client = create_download_client()?;
    let mut updated_count = 0;

    for (local_name, remote_name) in launcher_mappings() {
        let local_path = launcher_dir.join(local_name);

        if !local_path.exists() {
            continue;
        }

        let file_info = match manifest.files.get(remote_name) {
            Some(info) => info,
            None => {
                eprintln!("  {} -> {} (not in manifest, skipped)", local_name, remote_name);
                continue;
            }
        };

        if verify_file_checksum(&local_path, &file_info.sha256).is_ok() {
            eprintln!(
                "  {} ({:.1} MB) ... up to date",
                local_name,
                file_info.size as f64 / 1_048_576.0
            );
            continue;
        }

        eprint!("  {} ({:.1} MB) ... ", local_name, file_info.size as f64 / 1_048_576.0);

        let bytes = download_launcher_binary(
            &download_client,
            provider,
            &latest_version,
            remote_name,
            file_info,
        )?;

        let is_current_exe = local_path == current_exe;
        update_launcher_file(&local_path, &bytes, is_current_exe)?;

        eprintln!("{}", m.ok);
        updated_count += 1;
    }

    if updated_count > 0 {
        eprintln!("{}", m.self_update_done);
    } else {
        eprintln!("{}", m.self_update_up_to_date.replace("{}", &latest_version));
    }

    Ok(())
}

pub fn cleanup_versions(args: &[String]) -> Result<()> {
    let cache_dir = get_cache_dir()?;
    let m = messages();

    let keep_count = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--keep="))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);

    let current_version = get_current_version(&cache_dir);

    let mut versions: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&cache_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if is_reserved_cache_entry(name_str) {
            continue;
        }
        if let Some(version) = name_str.strip_prefix("bsl-analyzer-") {
            versions.push((version.to_string(), entry.path()));
        }
    }

    versions.sort_by(|a, b| {
        let time_a = fs::metadata(&a.1).and_then(|m| m.modified()).ok();
        let time_b = fs::metadata(&b.1).and_then(|m| m.modified()).ok();
        time_b.cmp(&time_a)
    });

    let mut kept = 0;
    let mut removed = 0;
    for (version, path) in &versions {
        let is_current = current_version.as_ref() == Some(version);

        if is_current || kept < keep_count {
            eprintln!("  {} {}", version, if is_current { "(current)" } else { "" });
            if !is_current {
                kept += 1;
            }
        } else if let Err(e) = fs::remove_file(path) {
            eprintln!("  {} - {} {}: {}", version, m.cleanup_error, path.display(), e);
        } else {
            eprintln!("  {} - {}", version, m.cleanup_removed);
            removed += 1;
        }
    }

    eprintln!("{}", m.cleanup_done.replace("{}", &removed.to_string()));
    Ok(())
}

fn download_latest(provider: &dyn ReleaseProvider, cache_dir: &Path) -> Result<PathBuf> {
    let http_client = create_http_client()?;
    let version = fetch_version_verbose(provider, &http_client)?;
    download_version(provider, cache_dir, &version)
}

fn download_version(
    provider: &dyn ReleaseProvider,
    cache_dir: &Path,
    version: &str,
) -> Result<PathBuf> {
    let binary_name = format!("bsl-analyzer-{}", version);
    let binary_path = cache_dir.join(&binary_name);

    if binary_path.exists() {
        if verify_existing_binary(provider, version, &binary_path).is_ok() {
            update_current_link(cache_dir, &binary_path)?;
            cleanup_cached_versions(cache_dir, &[], 1);
            return Ok(binary_path);
        }
        let _ = fs::remove_file(&binary_path);
    }

    let path = do_download(provider, cache_dir, version)?;
    update_current_link(cache_dir, &path)?;

    let m = messages();
    eprintln!("{}", m.installed.replace("{}", version));

    cleanup_cached_versions(cache_dir, &[], 1);

    Ok(path)
}

fn download_version_without_linking(
    provider: &dyn ReleaseProvider,
    cache_dir: &Path,
    version: &str,
) -> Result<PathBuf> {
    let path = do_download(provider, cache_dir, version)?;

    let m = messages();
    eprintln!("{}", m.installed.replace("{}", version));
    Ok(path)
}

fn cleanup_cached_versions(cache_dir: &Path, preserve_versions: &[String], keep_count: usize) {
    let current_version = get_current_version(cache_dir);

    let mut versions: Vec<(String, PathBuf)> = Vec::new();
    let Ok(entries) = fs::read_dir(cache_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if is_reserved_cache_entry(name_str) {
            continue;
        }
        if let Some(version) = name_str.strip_prefix("bsl-analyzer-") {
            versions.push((version.to_string(), entry.path()));
        }
    }

    versions.sort_by(|a, b| {
        let time_a = fs::metadata(&a.1).and_then(|m| m.modified()).ok();
        let time_b = fs::metadata(&b.1).and_then(|m| m.modified()).ok();
        time_b.cmp(&time_a)
    });

    let ordered_versions: Vec<String> =
        versions.iter().map(|(version, _)| version.clone()).collect();
    let versions_to_remove = select_versions_to_remove(
        &ordered_versions,
        current_version.as_deref(),
        preserve_versions,
        keep_count,
    );

    for (version, path) in &versions {
        if versions_to_remove.contains(version) {
            let _ = fs::remove_file(path);
        }
    }
}

fn select_versions_to_remove(
    ordered_versions: &[String],
    current_version: Option<&str>,
    preserve_versions: &[String],
    keep_count: usize,
) -> std::collections::HashSet<String> {
    let mut preserve: std::collections::HashSet<&str> =
        preserve_versions.iter().map(String::as_str).collect();
    if let Some(current) = current_version {
        preserve.insert(current);
    }

    let mut kept = 0;
    let mut versions_to_remove = std::collections::HashSet::new();

    for version in ordered_versions {
        if preserve.contains(version.as_str()) {
            continue;
        }

        if kept < keep_count {
            kept += 1;
        } else {
            versions_to_remove.insert(version.clone());
        }
    }

    versions_to_remove
}

fn do_download(provider: &dyn ReleaseProvider, cache_dir: &Path, version: &str) -> Result<PathBuf> {
    let binary_name = format!("bsl-analyzer-{}", version);
    let binary_path = cache_dir.join(&binary_name);

    let m = messages();
    eprintln!("{}", m.downloading.replace("{}", version));

    let http_client = create_http_client()?;
    let manifest = provider.fetch_manifest(&http_client, version)?;

    let platform = get_platform_binary();
    let file_info = manifest
        .files
        .get(platform)
        .context(format!("Platform {} not found in manifest", platform))?;

    eprint!(
        "{}",
        m.downloading_binary
            .replace("{:.1}", &format!("{:.1}", file_info.size as f64 / 1_048_576.0))
    );
    let download_client = create_download_client()?;
    let url = provider.download_url(version, platform);
    let response = download_client
        .get(&url)
        .send()
        .with_context(|| format!("Failed to download from {}", url))?;

    if !response.status().is_success() {
        eprintln!("{}", m.failed);
        bail!("Download failed: HTTP {}", response.status());
    }

    let bytes = response.bytes()?;
    eprintln!("{}", m.ok);

    if bytes.len() as u64 != file_info.size {
        bail!("Size mismatch: expected {}, got {}", file_info.size, bytes.len());
    }

    let hash = compute_sha256(&bytes);
    if hash != file_info.sha256 {
        bail!("Checksum mismatch!\nExpected: {}\nGot: {}", file_info.sha256, hash);
    }

    fs::write(&binary_path, &bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&binary_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_path, perms)?;
    }

    Ok(binary_path)
}

fn verify_existing_binary(
    provider: &dyn ReleaseProvider,
    version: &str,
    path: &Path,
) -> Result<()> {
    let http_client = create_http_client()?;
    let manifest = provider.fetch_manifest(&http_client, version)?;
    let platform = get_platform_binary();
    let expected = manifest.files.get(platform).context("Platform not found in manifest")?;
    verify_file_checksum(path, &expected.sha256)
}

fn download_launcher_binary(
    client: &reqwest::blocking::Client,
    provider: &dyn ReleaseProvider,
    version: &str,
    remote_name: &str,
    file_info: &FileInfo,
) -> Result<Vec<u8>> {
    let m = messages();
    let url = provider.download_url(version, remote_name);

    let response =
        client.get(&url).send().with_context(|| format!("Failed to download from {}", url))?;

    if !response.status().is_success() {
        eprintln!("{}", m.failed);
        bail!("Download failed: HTTP {}", response.status());
    }

    let bytes = response.bytes()?.to_vec();

    if bytes.len() as u64 != file_info.size {
        bail!("Size mismatch: expected {}, got {}", file_info.size, bytes.len());
    }

    let hash = compute_sha256(&bytes);
    if hash != file_info.sha256 {
        bail!("Checksum mismatch!\nExpected: {}\nGot: {}", file_info.sha256, hash);
    }

    Ok(bytes)
}

fn update_launcher_file(path: &Path, bytes: &[u8], is_current_exe: bool) -> Result<()> {
    if is_current_exe {
        let temp_path = path.with_extension("new");
        fs::write(&temp_path, bytes)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&temp_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&temp_path, perms)?;
        }

        self_replace::self_replace(&temp_path).context("Failed to replace executable")?;

        let _ = fs::remove_file(&temp_path);
    } else {
        fs::write(path, bytes)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::thread::sleep;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const HANG_GUARD: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

    #[test]
    fn bare_launcher_name_maps_to_the_running_platform() {
        let mappings = launcher_mappings();
        let (_, remote) =
            mappings.iter().find(|(local, _)| *local == "bsl-analyzer").expect("bare name mapped");

        // Self-update replaces the file in place, so a foreign artifact here would hand
        // the host a binary it cannot execute.
        assert_eq!(*remote, get_platform_launcher());
    }

    #[test]
    fn suffixed_launcher_names_stay_pinned_to_their_platform() {
        let mappings = launcher_mappings();
        for (local, remote) in mappings.iter().filter(|(local, _)| *local != "bsl-analyzer") {
            match *local {
                "bsl-analyzer.exe" => assert_eq!(*remote, "bsl-analyzer-windows-amd64.exe"),
                "bsl-analyzer-mac" => assert_eq!(*remote, "bsl-analyzer-darwin-arm64"),
                other => panic!("unexpected launcher name {other}"),
            }
        }
    }

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            // A clock does not make a name unique: `SystemTime::now` is quantised to about
            // a microsecond here, so two stands started at once read the same instant, take
            // the same directory, and reap each other's files. The counter is what makes
            // the name unique; the clock only keeps it clear of an earlier run's leftovers.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = format!(
                "bsl-launcher-test-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system time before unix epoch")
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir_all(&path).expect("failed to create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn create_version_file(cache_dir: &Path, version: &str) -> PathBuf {
        let path = cache_dir.join(format!("bsl-analyzer-{version}"));
        fs::write(&path, version).expect("failed to create version file");
        sleep(Duration::from_millis(20));
        path
    }

    fn version_exists(cache_dir: &Path, version: &str) -> bool {
        cache_dir.join(format!("bsl-analyzer-{version}")).exists()
    }

    #[test]
    fn select_versions_to_remove_keeps_current_and_latest_previous() {
        let versions =
            vec!["0.1.2", "0.1.1", "0.1.0"].into_iter().map(str::to_string).collect::<Vec<_>>();

        let to_remove = select_versions_to_remove(&versions, Some("0.1.2"), &[], 1);

        assert!(!to_remove.contains("0.1.2"));
        assert!(!to_remove.contains("0.1.1"));
        assert!(to_remove.contains("0.1.0"));
    }

    #[test]
    fn select_versions_to_remove_preserves_requested_version_for_pinned_run() {
        let versions = vec!["0.1.2", "0.1.1", "0.1.0", "0.0.9"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let preserve = ["0.1.2".to_string()];

        let to_remove = select_versions_to_remove(&versions, Some("0.1.0"), &preserve, 1);

        assert!(!to_remove.contains("0.1.2"));
        assert!(!to_remove.contains("0.1.1"));
        assert!(!to_remove.contains("0.1.0"));
        assert!(to_remove.contains("0.0.9"));
    }

    #[test]
    fn cleanup_keeps_current_and_latest_previous() {
        let dir = TestDir::new();
        create_version_file(dir.path(), "0.1.0");
        create_version_file(dir.path(), "0.1.1");
        let current = create_version_file(dir.path(), "0.1.2");
        update_current_link(dir.path(), &current).expect("failed to update current link");

        cleanup_cached_versions(dir.path(), &[], 1);

        assert!(version_exists(dir.path(), "0.1.2"));
        assert!(version_exists(dir.path(), "0.1.1"));
        assert!(!version_exists(dir.path(), "0.1.0"));
    }

    #[test]
    fn cleanup_preserves_requested_version_for_pinned_run() {
        let dir = TestDir::new();
        create_version_file(dir.path(), "0.0.9");
        let current = create_version_file(dir.path(), "0.1.0");
        create_version_file(dir.path(), "0.1.1");
        create_version_file(dir.path(), "0.1.2");
        update_current_link(dir.path(), &current).expect("failed to update current link");

        cleanup_cached_versions(dir.path(), &["0.1.2".to_string()], 1);

        assert!(version_exists(dir.path(), "0.1.0"));
        assert!(version_exists(dir.path(), "0.1.2"));
        assert!(version_exists(dir.path(), "0.1.1"));
        assert!(!version_exists(dir.path(), "0.0.9"));
    }

    #[test]
    fn cleanup_does_not_touch_stable_app_or_sidecar() {
        let dir = TestDir::new();
        create_version_file(dir.path(), "0.0.1");
        create_version_file(dir.path(), "0.1.0");
        let current = create_version_file(dir.path(), "0.1.5");
        update_current_link(dir.path(), &current).expect("failed to update current link");

        let stable = stable_app_path(dir.path());
        fs::write(&stable, b"stable bytes").expect("failed to write stable app");
        write_stable_version(dir.path(), "0.1.5").expect("failed to write sidecar");
        let temp = dir.path().join("bsl-analyzer-app.new-99999-12345");
        fs::write(&temp, b"temp bytes").expect("failed to write temp");

        cleanup_cached_versions(dir.path(), &[], 1);

        assert!(stable.exists(), "stable app must survive cleanup");
        assert!(dir.path().join(".app-version").exists(), "sidecar must survive cleanup");
        assert!(temp.exists(), "swap temp must survive cleanup (caller-owned)");
        assert!(version_exists(dir.path(), "0.1.5"));
        assert!(version_exists(dir.path(), "0.1.0"));
        assert!(
            !version_exists(dir.path(), "0.0.1"),
            "oldest non-current version should be reaped"
        );
    }

    #[test]
    fn swap_and_resolve_creates_stable_on_cold_cache() {
        let dir = TestDir::new();
        let current = create_version_file(dir.path(), "0.1.5");
        update_current_link(dir.path(), &current).expect("failed to update current link");

        let path = swap_and_resolve(dir.path()).expect("swap must succeed");
        let stable = stable_app_path(dir.path());

        assert_eq!(path, stable, "must return stable path after successful swap");
        assert!(stable.exists(), "stable app must exist after swap");
        assert_eq!(fs::read(&stable).expect("read stable"), b"0.1.5");
        assert_eq!(read_stable_version(dir.path()).as_deref(), Some("0.1.5"));
    }

    #[test]
    fn swap_and_resolve_is_noop_when_in_sync() {
        let dir = TestDir::new();
        let current = create_version_file(dir.path(), "0.1.5");
        update_current_link(dir.path(), &current).expect("failed to update current link");

        let stable = stable_app_path(dir.path());
        fs::write(&stable, b"existing-stable").expect("seed stable");
        write_stable_version(dir.path(), "0.1.5").expect("seed sidecar");

        let path = swap_and_resolve(dir.path()).expect("noop in sync");

        assert_eq!(path, stable);
        assert_eq!(fs::read(&stable).expect("read stable"), b"existing-stable");
    }

    #[test]
    fn swap_and_resolve_overwrites_on_divergence() {
        let dir = TestDir::new();
        let current = create_version_file(dir.path(), "0.1.6");
        update_current_link(dir.path(), &current).expect("failed to update current link");

        let stable = stable_app_path(dir.path());
        fs::write(&stable, b"old-stable").expect("seed old stable");
        write_stable_version(dir.path(), "0.1.5").expect("seed old sidecar");

        let path = swap_and_resolve(dir.path()).expect("swap on divergence");

        assert_eq!(path, stable);
        assert_eq!(fs::read(&stable).expect("read stable"), b"0.1.6");
        assert_eq!(read_stable_version(dir.path()).as_deref(), Some("0.1.6"));
    }

    #[test]
    fn swap_and_resolve_returns_versioned_when_swap_path_unwritable() {
        let dir = TestDir::new();
        let current = create_version_file(dir.path(), "0.1.5");
        update_current_link(dir.path(), &current).expect("link");

        let stable = stable_app_path(dir.path());
        fs::create_dir(&stable).expect("seed stable as a directory to block rename");

        let path = swap_and_resolve(dir.path()).expect("must fall back");

        assert_eq!(path, current);
    }

    #[test]
    fn swap_and_resolve_bails_without_current() {
        let dir = TestDir::new();
        let err = swap_and_resolve(dir.path()).expect_err("must error without current");
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn update_current_link_is_concurrency_safe() {
        use std::sync::Arc;
        use std::thread;

        let dir = Arc::new(TestDir::new());
        let v1 = create_version_file(dir.path(), "0.1.0");
        let v2 = create_version_file(dir.path(), "0.2.0");
        let targets = Arc::new(vec![v1, v2]);

        const THREADS: usize = 8;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut starts = Vec::new();
        let mut handles = Vec::new();
        for i in 0..THREADS {
            let dir = Arc::clone(&dir);
            let targets = Arc::clone(&targets);
            let ready_tx = ready_tx.clone();
            let done_tx = done_tx.clone();
            let (start_tx, start_rx) = std::sync::mpsc::channel();
            starts.push(start_tx);
            handles.push(thread::spawn(move || {
                ready_tx.send(()).expect("test receives relink worker readiness");
                start_rx.recv_timeout(HANG_GUARD).expect("test releases relink worker");
                let target = &targets[i % targets.len()];
                for _ in 0..32 {
                    update_current_link(dir.path(), target).expect("atomic relink");
                }
                done_tx.send(()).expect("test receives relink worker completion");
            }));
        }
        drop(ready_tx);
        drop(done_tx);
        for _ in 0..THREADS {
            ready_rx.recv_timeout(HANG_GUARD).expect("all relink workers are ready");
        }
        for start in starts {
            start.send(()).expect("relink worker waits for release");
        }
        for _ in 0..THREADS {
            done_rx.recv_timeout(HANG_GUARD).expect("relink worker completes");
        }
        for handle in handles {
            handle.join().expect("relink worker exits");
        }

        let ver = get_current_version(dir.path()).expect("current readable");
        assert!(ver == "0.1.0" || ver == "0.2.0");
    }

    #[test]
    fn swap_stable_concurrent_writes_stay_consistent() {
        use std::sync::Arc;
        use std::thread;

        let dir = Arc::new(TestDir::new());
        let current = create_version_file(dir.path(), "0.1.5");
        update_current_link(dir.path(), &current).expect("link");

        const THREADS: usize = 8;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut starts = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let dir = Arc::clone(&dir);
            let ready_tx = ready_tx.clone();
            let done_tx = done_tx.clone();
            let (start_tx, start_rx) = std::sync::mpsc::channel();
            starts.push(start_tx);
            handles.push(thread::spawn(move || {
                ready_tx.send(()).expect("test receives swap worker readiness");
                start_rx.recv_timeout(HANG_GUARD).expect("test releases swap worker");
                for _ in 0..16 {
                    swap_and_resolve(dir.path()).expect("swap under lock");
                }
                done_tx.send(()).expect("test receives swap worker completion");
            }));
        }
        drop(ready_tx);
        drop(done_tx);
        for _ in 0..THREADS {
            ready_rx.recv_timeout(HANG_GUARD).expect("all swap workers are ready");
        }
        for start in starts {
            start.send(()).expect("swap worker waits for release");
        }
        for _ in 0..THREADS {
            done_rx.recv_timeout(HANG_GUARD).expect("swap worker completes");
        }
        for handle in handles {
            handle.join().expect("swap worker exits");
        }

        let stable_ver = read_stable_version(dir.path()).expect("sidecar present");
        let stable_bytes = fs::read(stable_app_path(dir.path())).expect("read stable bytes");
        let expected_bytes = fs::read(dir.path().join(format!("bsl-analyzer-{stable_ver}")))
            .expect("read versioned bytes");
        assert_eq!(
            stable_bytes, expected_bytes,
            "stable bytes must match version named in sidecar"
        );
        assert_eq!(stable_ver, "0.1.5");
    }

    #[cfg(unix)]
    use crate::cache::SWAP_LOCK_FILENAME;
    use crate::cache::{data_dir, DATA_DIR_ENV};

    /// `BSL_ANALYZER_HOME` is process-wide, so the tests that move it take turns.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_data_dir_env<T>(value: Option<&Path>, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = env::var_os(DATA_DIR_ENV);
        match value {
            Some(path) => env::set_var(DATA_DIR_ENV, path),
            None => env::set_var(DATA_DIR_ENV, ""),
        }
        let outcome = body();
        match previous {
            Some(prev) => env::set_var(DATA_DIR_ENV, prev),
            None => env::remove_var(DATA_DIR_ENV),
        }
        outcome
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        // Root ignores the permission bits these tests rely on.
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(unix)]
    fn set_dir_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).expect("read dir permissions").permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms).expect("set dir permissions");
    }

    #[test]
    fn data_dir_follows_the_env_override() {
        let dir = TestDir::new();
        let (resolved, cache) = with_data_dir_env(Some(dir.path()), || {
            (data_dir().expect("override resolves"), get_cache_dir().expect("cache dir created"))
        });

        assert_eq!(resolved, dir.path());
        assert_eq!(cache, dir.path().join("bin"));
        assert!(cache.is_dir(), "the override must be created, not just returned");
    }

    #[test]
    fn an_empty_override_falls_back_to_the_home_directory() {
        let resolved = with_data_dir_env(None, || data_dir().expect("fallback resolves"));

        assert!(
            resolved.ends_with(".bsl-analyzer"),
            "an unset override must keep the historical location, got {}",
            resolved.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_converged_cache_starts_without_writing_to_the_data_dir() {
        if running_as_root() {
            return;
        }

        let dir = TestDir::new();
        let current = create_version_file(dir.path(), "0.2.80");
        update_current_link(dir.path(), &current).expect("link");
        swap_and_resolve(dir.path()).expect("warm the stable app");
        fs::remove_file(dir.path().join(SWAP_LOCK_FILENAME)).expect("drop the warmed lock file");

        set_dir_mode(dir.path(), 0o555);
        let resolved = swap_and_resolve(dir.path());
        set_dir_mode(dir.path(), 0o755);

        assert_eq!(
            resolved.expect("a cache with nothing to swap must start on a read-only home"),
            stable_app_path(dir.path())
        );
        assert!(
            !dir.path().join(SWAP_LOCK_FILENAME).exists(),
            "resolving a converged cache must not create the swap lock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_swap_lock_names_the_variable_that_moves_the_data_dir() {
        if running_as_root() {
            return;
        }

        let dir = TestDir::new();
        let current = create_version_file(dir.path(), "0.2.80");
        update_current_link(dir.path(), &current).expect("link");

        set_dir_mode(dir.path(), 0o555);
        let err = swap_and_resolve(dir.path()).expect_err("a swap needs the lock it cannot open");
        set_dir_mode(dir.path(), 0o755);

        let rendered = format!("{err:#}");
        assert!(rendered.contains(SWAP_LOCK_FILENAME), "must name the path refused: {rendered}");
        assert!(rendered.contains(DATA_DIR_ENV), "must name the way out: {rendered}");
    }
}
