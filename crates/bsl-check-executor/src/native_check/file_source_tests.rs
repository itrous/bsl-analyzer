use super::*;
use file_source::FileSourceSnapshot;
use tokio_util::sync::CancellationToken;

struct SnapshotFixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    attempt: PathBuf,
}

impl SnapshotFixture {
    fn new() -> Self {
        let root = super::super::tests::canonical_tempdir();
        let source = root.path().join("original");
        let attempt = root.path().join("attempt");
        for path in [&source, &attempt] {
            #[cfg(windows)]
            super::super::windows_acl::new_private_directory(path).unwrap();
            #[cfg(not(windows))]
            {
                fs::create_dir(path).unwrap();
                set_private_dir(path).unwrap();
            }
        }
        fs::write(source.join("1Cv8.1CD"), b"opaque database bytes").unwrap();
        fs::write(source.join(".native-check-fixture"), b"bsl-analyzer-native-check-fixture-v1\n")
            .unwrap();
        secure_private_file(&source.join(".native-check-fixture")).unwrap();
        Self { _root: root, source, attempt }
    }

    async fn snapshot(&self) -> Result<FileSourceSnapshot, NativeCheckError> {
        FileSourceSnapshot::create(
            &self.source,
            &self.attempt,
            test_deadline(),
            &CancellationToken::new(),
        )
        .await
    }
}

fn test_deadline() -> Instant {
    Instant::now() + std::time::Duration::from_secs(30)
}

fn error_code<T>(result: Result<T, NativeCheckError>) -> &'static str {
    match result {
        Ok(_) => panic!("unsafe source snapshot was accepted"),
        Err(error) => error.code,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn file_source_overlap_is_rejected_before_any_workspace_write() {
    let fixture = SnapshotFixture::new();
    let env_name = "BSL_TEST_FILE_SOURCE_OVERLAP";
    std::env::set_var(env_name, &fixture.source);
    let profile = NativeProfile {
        designer_path: fixture._root.path().join("unused-designer"),
        python_path: PathBuf::from("/usr/bin/python3"),
        xvfb_run_path: None,
        expected_build: "8.3.27.1989".to_owned(),
        work_root: fixture.source.clone(),
        source_kind: SourceKind::File,
        source_connection_env: env_name.to_owned(),
        runtime_url_env: None,
        user_env: None,
        password_env: None,
        max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
        deadline_ms: DEFAULT_DEADLINE_MS,
        max_log_bytes: DEFAULT_MAX_LOG_BYTES,
    };
    let result = profile
        .run_scoped(
            &onec_client::Client::new("http://localhost", "", ""),
            &super::super::types::CheckRequest::default(),
            "x = 1;",
            CancellationToken::new(),
            test_deadline(),
        )
        .await;
    std::env::remove_var(env_name);
    assert!(
        !fixture.source.join(".native-check-root.lock").exists(),
        "even the root lock would modify the original roster"
    );
    assert!(!fixture.source.join("owned-jobs").exists());
    assert_eq!(result.failure.unwrap().code, "source_unavailable");
}

#[test]
fn file_source_drift_is_terminal_but_server_snapshot_retry_is_unchanged() {
    let stale = Err(check_error("source_snapshot_stale", "test drift"));
    assert!(
        !retry_source_snapshot(SourceKind::File, &stale),
        "a second FILE attempt must not turn observed original drift into valid"
    );
    assert!(retry_source_snapshot(SourceKind::Server, &stale));
    let other = Err(check_error("source_unavailable", "test unavailable"));
    assert!(!retry_source_snapshot(SourceKind::Server, &other));
}

#[tokio::test]
async fn snapshot_bytes_are_fresh_and_private_without_reusing_existing_destination() {
    let fixture = SnapshotFixture::new();
    let snapshot = fixture.snapshot().await.unwrap_or_else(|error| panic!("{}", error.code));
    assert_eq!(snapshot.path(), fixture.attempt.join("file-source"));
    assert_eq!(fs::read(snapshot.path().join("1Cv8.1CD")).unwrap(), b"opaque database bytes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(snapshot.path()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(snapshot.path().join("1Cv8.1CD")).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(error_code(fixture.snapshot().await), "source_unavailable");
    assert_eq!(fs::read(snapshot.path().join("1Cv8.1CD")).unwrap(), b"opaque database bytes");
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_rejects_non_private_attempt_before_copying_data() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = SnapshotFixture::new();
    fs::set_permissions(&fixture.attempt, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(error_code(fixture.snapshot().await), "source_unavailable");
    assert!(!fixture.attempt.join("file-source").exists());
}

#[tokio::test]
async fn snapshot_rejects_hardlinked_source_before_creating_destination() {
    let fixture = SnapshotFixture::new();
    fs::hard_link(fixture.source.join("1Cv8.1CD"), fixture._root.path().join("alias.1CD")).unwrap();
    assert_eq!(error_code(fixture.snapshot().await), "source_unavailable");
    assert!(!fixture.attempt.join("file-source").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_rejects_symlink_source_and_symlink_parent() {
    use std::os::unix::fs::symlink;
    let fixture = SnapshotFixture::new();
    let data = fixture.source.join("1Cv8.1CD");
    let moved = fixture.source.join("real.1CD");
    fs::rename(&data, &moved).unwrap();
    symlink(&moved, &data).unwrap();
    assert_eq!(error_code(fixture.snapshot().await), "source_unavailable");
    fs::remove_file(&data).unwrap();
    fs::rename(&moved, &data).unwrap();
    let alias = fixture._root.path().join("source-alias");
    symlink(&fixture.source, &alias).unwrap();
    assert_eq!(
        error_code(
            FileSourceSnapshot::create(
                &alias,
                &fixture.attempt,
                test_deadline(),
                &CancellationToken::new()
            )
            .await
        ),
        "source_unavailable"
    );
    assert!(!fixture.attempt.join("file-source").exists());
}

#[tokio::test]
async fn snapshot_rejects_overlap_in_both_directions() {
    let fixture = SnapshotFixture::new();
    for attempt in [&fixture.source, fixture.source.parent().unwrap()] {
        assert_eq!(
            error_code(
                FileSourceSnapshot::create(
                    &fixture.source,
                    attempt,
                    test_deadline(),
                    &CancellationToken::new()
                )
                .await
            ),
            "source_unavailable"
        );
    }
    assert!(!fixture.source.join("file-source").exists());
}

#[tokio::test]
async fn snapshot_cancel_and_expired_deadline_do_not_create_destination() {
    let fixture = SnapshotFixture::new();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        error_code(
            FileSourceSnapshot::create(&fixture.source, &fixture.attempt, test_deadline(), &cancel)
                .await
        ),
        "cancelled"
    );
    assert_eq!(
        error_code(
            FileSourceSnapshot::create(
                &fixture.source,
                &fixture.attempt,
                Instant::now(),
                &CancellationToken::new()
            )
            .await
        ),
        "deadline_exceeded"
    );
    assert!(!fixture.attempt.join("file-source").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_original_recheck_rejects_same_size_drift_and_path_replacement() {
    for replace in [false, true] {
        let fixture = SnapshotFixture::new();
        let snapshot = fixture.snapshot().await.unwrap_or_else(|error| panic!("{}", error.code));
        if replace {
            fs::rename(fixture.source.join("1Cv8.1CD"), fixture.source.join("old.1CD")).unwrap();
            fs::write(fixture.source.join("1Cv8.1CD"), b"opaque database bytes").unwrap();
        } else {
            fs::write(fixture.source.join("1Cv8.1CD"), b"Opaque database bytes").unwrap();
        }
        assert_eq!(
            error_code(snapshot.verify_original(test_deadline(), &CancellationToken::new()).await),
            "source_snapshot_stale"
        );
    }
}

#[cfg(windows)]
#[tokio::test]
async fn snapshot_original_reader_denies_write_and_delete_opens() {
    let fixture = SnapshotFixture::new();
    let snapshot = fixture.snapshot().await.unwrap_or_else(|error| panic!("{}", error.code));
    assert!(OpenOptions::new().write(true).open(fixture.source.join("1Cv8.1CD")).is_err());
    assert!(fs::rename(fixture.source.join("1Cv8.1CD"), fixture.source.join("moved.1CD")).is_err());
    snapshot
        .verify_original(test_deadline(), &CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{}", error.code));
}

#[tokio::test]
async fn snapshot_copy_cancellation_after_first_chunk_keeps_original_intact() {
    use std::future::Future;
    use std::task::Poll;
    let fixture = SnapshotFixture::new();
    let bytes = vec![0x5a; 4 * 1024 * 1024];
    fs::write(fixture.source.join("1Cv8.1CD"), &bytes).unwrap();
    let cancel = CancellationToken::new();
    let mut copy = std::pin::pin!(FileSourceSnapshot::create(
        &fixture.source,
        &fixture.attempt,
        test_deadline(),
        &cancel
    ));
    std::future::poll_fn(|cx| {
        assert!(
            copy.as_mut().poll(cx).is_pending(),
            "copy must yield before consuming the complete fixture"
        );
        if fs::metadata(fixture.attempt.join("file-source/1Cv8.1CD"))
            .is_ok_and(|metadata| metadata.len() >= 64 * 1024)
        {
            cancel.cancel();
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
    assert_eq!(error_code(copy.await), "cancelled");
    assert_eq!(fs::read(fixture.source.join("1Cv8.1CD")).unwrap(), bytes);
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_rejects_source_drift_while_materializing_before_any_platform_call() {
    use std::future::Future;
    use std::task::Poll;
    for replace in [false, true] {
        let fixture = SnapshotFixture::new();
        fs::write(fixture.source.join("1Cv8.1CD"), vec![0x5a; 4 * 1024 * 1024]).unwrap();
        let cancel = CancellationToken::new();
        let mut copy = std::pin::pin!(FileSourceSnapshot::create(
            &fixture.source,
            &fixture.attempt,
            test_deadline(),
            &cancel
        ));
        super::super::tests::guard_future(
            std::future::poll_fn(|cx| {
                assert!(
                    copy.as_mut().poll(cx).is_pending(),
                    "copy must yield before consuming the complete fixture"
                );
                if fs::metadata(fixture.attempt.join("file-source/1Cv8.1CD"))
                    .is_ok_and(|metadata| metadata.len() >= 64 * 1024)
                {
                    return Poll::Ready(());
                }
                cx.waker().wake_by_ref();
                Poll::Pending
            }),
            "snapshot copy progress",
        )
        .await;
        if replace {
            fs::rename(fixture.source.join("1Cv8.1CD"), fixture.source.join("old.1CD")).unwrap();
            fs::write(fixture.source.join("1Cv8.1CD"), vec![0x5a; 4 * 1024 * 1024]).unwrap();
        } else {
            OpenOptions::new()
                .append(true)
                .open(fixture.source.join("1Cv8.1CD"))
                .unwrap()
                .write_all(b"external growth")
                .unwrap();
        }
        assert_eq!(
            error_code(super::super::tests::guard_future(copy, "drifted snapshot copy").await),
            "source_snapshot_stale"
        );
        assert!(!fixture.attempt.join("platform-calls.jsonl").exists());
    }
}

#[tokio::test]
async fn snapshot_original_recheck_obeys_cancel_and_deadline() {
    let fixture = SnapshotFixture::new();
    let snapshot = fixture.snapshot().await.unwrap_or_else(|error| panic!("{}", error.code));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(error_code(snapshot.verify_original(test_deadline(), &cancel).await), "cancelled");
    assert_eq!(
        error_code(snapshot.verify_original(Instant::now(), &CancellationToken::new()).await),
        "deadline_exceeded"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_cleanup_uses_owned_workspace_without_removing_original() {
    let fixture = SnapshotFixture::new();
    let work_root = fixture._root.path().join("work-root");
    fs::create_dir(&work_root).unwrap();
    set_private_dir(&work_root).unwrap();
    fs::create_dir(work_root.join("owned-jobs")).unwrap();
    let workspace = JobWorkspace::create(&work_root).unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let attempt = workspace.path().join("attempt-0");
    fs::create_dir(&attempt).unwrap();
    set_private_dir(&attempt).unwrap();
    let snapshot = FileSourceSnapshot::create(
        &fixture.source,
        &attempt,
        test_deadline(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_or_else(|error| panic!("{}", error.code));
    assert!(snapshot.path().join("1Cv8.1CD").is_file());
    drop(snapshot);
    let result = finish_with_cleanup(
        super::super::types::CheckResult::new(super::super::types::CheckStatus::Error),
        workspace,
    )
    .await;
    assert_eq!(result.cleanup_status, super::super::types::CleanupStatus::Complete);
    assert!(!workspace_path.exists());
    assert_eq!(fs::read(fixture.source.join("1Cv8.1CD")).unwrap(), b"opaque database bytes");
}

#[cfg(unix)]
#[tokio::test]
async fn file_source_platform_arguments_use_only_private_snapshot_and_preserve_original() {
    run_fake_platform(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn file_source_never_returns_valid_after_original_drifts_during_platform_recheck() {
    run_fake_platform(true).await;
}

#[cfg(unix)]
async fn run_fake_platform(drift: bool) {
    use super::super::types::{CheckContext, CheckRequest, ModuleOrigin, ModuleType};
    use std::future::Future;
    use std::os::unix::fs::PermissionsExt;
    use std::task::Poll;

    let root = super::super::tests::canonical_tempdir();
    let source = root.path().join("original");
    let workspace = root.path().join("workspace");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&workspace).unwrap();
    set_private_dir(&source).unwrap();
    set_private_dir(&workspace).unwrap();
    let original = b"opaque synthetic database bytes, not a real 1CD";
    fs::write(source.join("1Cv8.1CD"), original).unwrap();
    fs::write(source.join("1Cv8.1CL"), b"original lock bytes").unwrap();
    fs::create_dir(source.join("1Cv8Log")).unwrap();
    fs::write(source.join("1Cv8Log/events"), b"original event log").unwrap();
    fs::write(source.join(".native-check-fixture"), b"bsl-analyzer-native-check-fixture-v1\n")
        .unwrap();
    fs::set_permissions(source.join(".native-check-fixture"), fs::Permissions::from_mode(0o600))
        .unwrap();
    let roster = |path: &Path| {
        let mut names =
            fs::read_dir(path).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>();
        names.sort();
        names
    };
    let before_roster = roster(&source);
    let designer = root.path().join("1cv8");
    for executable in [&designer, &root.path().join("ibcmd")] {
        fs::write(executable, include_bytes!("fixtures/file_source_platform.py")).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    // This name belongs exclusively to this test, not to a real configured profile.
    let env_name = if drift {
        "BSL_TEST_FILE_SOURCE_PRIVATE_SNAPSHOT_DRIFT"
    } else {
        "BSL_TEST_FILE_SOURCE_PRIVATE_SNAPSHOT"
    };
    std::env::set_var(env_name, &source);
    let profile = NativeProfile {
        designer_path: designer,
        python_path: PathBuf::from("/usr/bin/python3"),
        xvfb_run_path: None,
        expected_build: "8.3.27.1989".to_owned(),
        work_root: root.path().to_owned(),
        source_kind: SourceKind::File,
        source_connection_env: env_name.to_owned(),
        runtime_url_env: None,
        user_env: None,
        password_env: None,
        max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
        deadline_ms: DEFAULT_DEADLINE_MS,
        max_log_bytes: DEFAULT_MAX_LOG_BYTES,
    };
    let request = CheckRequest {
        input_kind: super::super::types::InputKind::Module,
        module_type: Some(ModuleType::OrdinaryForm),
        context: Some(CheckContext::Metadata {
            owner: "CommonForm.Fixture".to_owned(),
            origin: ModuleOrigin::Configuration,
        }),
    };
    let client = onec_client::Client::new("http://localhost", "", "");
    let cancel = CancellationToken::new();
    let attempt = workspace.join("attempt-0");
    if drift {
        fs::write(root.path().join("inject-source-drift"), b"test external writer").unwrap();
    }
    let mut native = std::pin::pin!(run_native_check(
        &profile,
        &client,
        &request,
        "x = 1;",
        &workspace,
        0,
        test_deadline(),
        &cancel,
        "8.3.27.1989",
        None,
    ));
    let mut expected_original = original.to_vec();
    if drift {
        super::super::tests::guard_future(
            std::future::poll_fn(|cx| {
                assert!(
                    native.as_mut().poll(cx).is_pending(),
                    "platform must pause at the recheck barrier"
                );
                if attempt.join("recheck-ready").exists() {
                    return Poll::Ready(());
                }
                cx.waker().wake_by_ref();
                Poll::Pending
            }),
            "fake recheck barrier",
        )
        .await;
        expected_original[0] = b'O';
        fs::write(source.join("1Cv8.1CD"), &expected_original).unwrap();
        fs::write(attempt.join("recheck-continue"), b"external write complete").unwrap();
    }
    let result =
        super::super::tests::guard_future(native, "fake platform FILE snapshot check").await;
    std::env::remove_var(env_name);
    if drift {
        assert_eq!(error_code(result), "source_snapshot_stale");
    } else {
        let result = result.unwrap_or_else(|error| panic!("{}: {}", error.code, error.message));
        assert_eq!(
            result.valid,
            Some(true),
            "only a fake process result, not native 1C acceptance"
        );
    }
    let snapshot = attempt.join("file-source");
    let calls = fs::read_to_string(attempt.join("platform-calls.jsonl")).unwrap();
    let mut source_calls = 0;
    for line in calls.lines() {
        let args: Vec<String> = serde_json::from_str(line).unwrap();
        assert!(
            !args.iter().any(|arg| arg.contains(source.to_str().unwrap())),
            "original passed to platform: {args:?}"
        );
        if let Some(index) = args.iter().position(|arg| arg == "/F") {
            let path = Path::new(&args[index + 1]);
            assert!(
                path == snapshot || path == attempt.join("ib"),
                "unexpected platform FILE path: {path:?}"
            );
            source_calls += usize::from(path == snapshot);
        }
        for arg in &args {
            if let Some(path) = arg.strip_prefix("--db-path=") {
                assert!(Path::new(path) == snapshot || Path::new(path) == attempt.join("ib"));
            }
        }
    }
    assert_eq!(source_calls, 6, "manifest, CF and active extension export, each including recheck");
    assert_eq!(fs::read(source.join("1Cv8.1CD")).unwrap(), expected_original);
    assert_eq!(roster(&source), before_roster);
    assert_eq!(fs::read(source.join("1Cv8.1CL")).unwrap(), b"original lock bytes");
    assert_eq!(fs::read(source.join("1Cv8Log/events")).unwrap(), b"original event log");
    assert!(!snapshot.join("1Cv8.1CL").exists());
    assert!(!snapshot.join("1Cv8Log").exists());
    assert!(!snapshot.join(".native-check-fixture").exists());
}
