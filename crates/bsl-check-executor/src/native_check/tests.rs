use super::*;
use std::path::PathBuf;

pub(super) fn canonical_tempdir() -> tempfile::TempDir {
    #[cfg(windows)]
    {
        let canonical = std::env::temp_dir().canonicalize().unwrap();
        let canonical = canonical.to_string_lossy();
        let dos_path = if let Some(unc_path) = canonical.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc_path}")
        } else if let Some(dos_path) = canonical.strip_prefix(r"\\?\") {
            dos_path.to_owned()
        } else {
            canonical.into_owned()
        };
        tempfile::tempdir_in(dos_path).unwrap()
    }

    #[cfg(not(windows))]
    {
        tempfile::tempdir().unwrap()
    }
}

fn profile() -> NativeProfile {
    NativeProfile {
        designer_path: PathBuf::from("/unused/1cv8"),
        python_path: PathBuf::from("/unused/python3"),
        xvfb_run_path: None,
        expected_build: "unused".to_owned(),
        work_root: PathBuf::from("/unused/work"),
        source_kind: profile::SourceKind::File,
        source_connection_env: "BSL_SOURCE".to_owned(),
        runtime_url_env: None,
        user_env: None,
        password_env: Some("BSL_PASSWORD".to_owned()),
        max_input_bytes: 1024,
        deadline_ms: 1000,
        max_log_bytes: 4096,
    }
}

#[tokio::test]
async fn cancelling_a_queued_check_does_not_take_the_active_slot() {
    let guard = ACTIVE_CHECK.lock().await;
    let cancel = CancellationToken::new();
    let waiter_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let client = onec_client::Client::new("http://localhost", "", "");
        check(&client, &profile(), &CheckRequest::default(), "x = 1;", waiter_cancel).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    cancel.cancel();

    let result = task.await.unwrap();
    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.failure.unwrap().code, "cancelled");
    assert!(ACTIVE_CHECK.try_lock().is_err());
    drop(guard);
}

#[tokio::test]
async fn queued_check_deadline_includes_waiting_for_the_native_slot() {
    let guard = ACTIVE_CHECK.lock().await;
    let mut short_profile = profile();
    short_profile.deadline_ms = 10;

    let client = onec_client::Client::new("http://localhost", "", "");
    let result = check(
        &client,
        &short_profile,
        &CheckRequest::default(),
        "x = 1;",
        CancellationToken::new(),
    )
    .await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.failure.unwrap().code, "deadline_exceeded");
    assert!(ACTIVE_CHECK.try_lock().is_err());
    drop(guard);
}

#[tokio::test]
async fn module_without_an_explicit_context_is_not_reported_as_invalid_code() {
    let request = CheckRequest { input_kind: InputKind::Module, ..CheckRequest::default() };
    let client = onec_client::Client::new("http://localhost", "", "");
    let result = check(
        &client,
        &profile(),
        &request,
        "Процедура Тест()\nКонецПроцедуры",
        CancellationToken::new(),
    )
    .await;

    assert_eq!(result.status, CheckStatus::ContextRequired);
    assert_eq!(result.valid, None);
    assert_eq!(result.compilation_status, None);
    assert_eq!(result.failure.unwrap().code, "module_context_required");
}

#[cfg(unix)]
#[tokio::test]
async fn public_check_reports_dirty_owned_root_as_error_without_starting_a_platform() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let work_root = root.path().join("work");
    let jobs = work_root.join("owned-jobs");
    let dirty_job = jobs.join("123-456-0");
    std::fs::create_dir(&work_root).unwrap();
    std::fs::set_permissions(&work_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(&jobs).unwrap();
    std::fs::set_permissions(&jobs, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(&dirty_job).unwrap();
    std::fs::set_permissions(&dirty_job, std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = dirty_job.join(".native-check-owner");
    std::fs::write(&owner, b"malformed ownership record\n").unwrap();
    std::fs::set_permissions(owner, std::fs::Permissions::from_mode(0o600)).unwrap();

    let mut configured = profile();
    configured.work_root = work_root;
    let client = onec_client::Client::new("http://localhost", "", "");
    let result =
        check(&client, &configured, &CheckRequest::default(), "x = 1;", CancellationToken::new())
            .await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.failure.unwrap().code, "cleanup_failed");
    assert_eq!(result.cleanup_status, CleanupStatus::Pending);
}
