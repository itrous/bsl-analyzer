use super::*;
use std::future::Future;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Child, ExitStatus};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

pub(super) const HANG_GUARD: Duration = test_utils::wait::hang_guard(Duration::from_secs(60));

pub(super) async fn guard_future<F: Future>(future: F, description: &'static str) -> F::Output {
    tokio::time::timeout(HANG_GUARD, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
}

#[cfg(unix)]
pub(super) async fn wait_until(
    description: &'static str,
    bound: Duration,
    mut condition: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + test_utils::wait::hang_guard(bound);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {description}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
pub(super) async fn guard_task<T: Send + 'static>(
    mut task: tokio::task::JoinHandle<T>,
    description: &'static str,
) -> T {
    match tokio::time::timeout(HANG_GUARD, &mut task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => panic!("{description} task failed: {error}"),
        Err(_) => {
            task.abort();
            panic!("timed out waiting for {description} task");
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn wait_until_sync(
    description: &'static str,
    bound: Duration,
    step: Duration,
    condition: impl FnMut() -> bool,
) {
    assert!(
        test_utils::wait::poll_until(bound, step, condition),
        "timed out waiting for {description}"
    );
}

#[cfg(unix)]
pub(super) fn wait_child(
    mut child: Child,
    description: &'static str,
) -> std::io::Result<ExitStatus> {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = std::thread::Builder::new().name("native-check-test-reaper".to_owned()).spawn(
                move || {
                    let _ = child.wait();
                },
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out waiting for {description}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

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
    let guard = guard_future(ACTIVE_CHECK.lock(), "active native-check slot").await;
    let cancel = CancellationToken::new();
    let waiter_cancel = cancel.clone();
    let task = async move {
        let client = onec_client::Client::new("http://localhost", "", "");
        check(&client, &profile(), &CheckRequest::default(), "x = 1;", 2_000, waiter_cancel).await
    };
    let mut task = std::pin::pin!(task);
    std::future::poll_fn(|cx| {
        assert!(task.as_mut().poll(cx).is_pending(), "held native slot must park the queued check");
        std::task::Poll::Ready(())
    })
    .await;
    cancel.cancel();

    let result = guard_future(task, "cancelled queued check").await;
    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.failure.unwrap().code, "cancelled");
    assert!(ACTIVE_CHECK.try_lock().is_err());
    drop(guard);
}

#[tokio::test]
async fn queued_check_deadline_includes_waiting_for_the_native_slot() {
    let guard = guard_future(ACTIVE_CHECK.lock(), "active native-check slot").await;
    let mut short_profile = profile();
    short_profile.deadline_ms = 10;

    let client = onec_client::Client::new("http://localhost", "", "");
    let result = guard_future(
        check(
            &client,
            &short_profile,
            &CheckRequest::default(),
            "x = 1;",
            2_000,
            CancellationToken::new(),
        ),
        "deadline-bounded queued check",
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
    let result = guard_future(
        check(
            &client,
            &profile(),
            &request,
            "Процедура Тест()\nКонецПроцедуры",
            2_000,
            CancellationToken::new(),
        ),
        "context-required check",
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
    let result = guard_future(
        check(
            &client,
            &configured,
            &CheckRequest::default(),
            "x = 1;",
            2_000,
            CancellationToken::new(),
        ),
        "dirty-owned-root check",
    )
    .await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.failure.unwrap().code, "cleanup_failed");
    assert_eq!(result.cleanup_status, CleanupStatus::Pending);
}
