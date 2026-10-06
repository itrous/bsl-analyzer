use serde::Deserialize;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_DEADLINE_MS: u64 = 120_000;
const MAX_DEADLINE_MS: u64 = 15 * 60 * 1000;
const DEFAULT_MAX_LOG_BYTES: usize = 256 * 1024;
const MIN_MAX_LOG_BYTES: usize = 4096;
const MAX_LOG_BYTES: usize = 1024 * 1024;
const OWNER_MARKER: &str = ".native-check-owner";
const OWNER_MARKER_PREFIX: &str = "bsl-analyzer-native-check-v1:";
static NEXT_JOB: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Server,
    File,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeProfile {
    pub designer_path: PathBuf,
    pub python_path: PathBuf,
    #[serde(default)]
    pub xvfb_run_path: Option<PathBuf>,
    pub expected_build: String,
    pub work_root: PathBuf,
    #[serde(default = "default_source_kind")]
    pub source_kind: SourceKind,
    pub source_connection_env: String,
    #[serde(default)]
    pub user_env: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: usize,
    #[serde(default = "default_deadline_ms")]
    pub deadline_ms: u64,
    #[serde(default = "default_max_log_bytes")]
    pub max_log_bytes: usize,
}

impl fmt::Debug for NativeProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeProfile")
            .field("designer_path", &"<redacted>")
            .field("python_path", &"<redacted>")
            .field("xvfb_run_path", &self.xvfb_run_path.as_ref().map(|_| "<redacted>"))
            .field("expected_build", &"<redacted>")
            .field("work_root", &"<redacted>")
            .field("source_kind", &self.source_kind)
            .field("source_connection_env", &"<redacted>")
            .field("user_env", &self.user_env.as_ref().map(|_| "<redacted>"))
            .field("password_env", &self.password_env.as_ref().map(|_| "<redacted>"))
            .field("max_input_bytes", &self.max_input_bytes)
            .field("deadline_ms", &self.deadline_ms)
            .field("max_log_bytes", &self.max_log_bytes)
            .finish()
    }
}

impl NativeProfile {
    pub fn validate(&self) -> Result<(), NativeProfileError> {
        if !self.designer_path.is_absolute() {
            return Err(NativeProfileError::Invalid("designer_path must be absolute"));
        }
        let designer = self
            .designer_path
            .canonicalize()
            .map_err(|_| NativeProfileError::Invalid("designer_path is unavailable"))?;
        if designer.as_path() != self.designer_path.as_path() || !designer.is_file() {
            return Err(NativeProfileError::Invalid(
                "designer_path must name the canonical executable file",
            ));
        }
        validate_executable(&self.python_path)?;
        if !self
            .python_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("python3"))
        {
            return Err(NativeProfileError::Invalid("python_path must name a Python 3 executable"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&designer)
                .map_err(|_| NativeProfileError::Invalid("designer_path is unavailable"))?
                .permissions()
                .mode();
            if mode & 0o111 == 0 {
                return Err(NativeProfileError::Invalid("designer_path is not executable"));
            }
        }
        if let Some(wrapper) = &self.xvfb_run_path {
            if !wrapper.is_absolute() {
                return Err(NativeProfileError::Invalid("xvfb_run_path must be absolute"));
            }
            let canonical = wrapper
                .canonicalize()
                .map_err(|_| NativeProfileError::Invalid("xvfb_run_path is unavailable"))?;
            if canonical.as_path() != wrapper.as_path() || !canonical.is_file() {
                return Err(NativeProfileError::Invalid(
                    "xvfb_run_path must name the canonical executable file",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&canonical)
                    .map_err(|_| NativeProfileError::Invalid("xvfb_run_path is unavailable"))?
                    .permissions()
                    .mode();
                if mode & 0o111 == 0 {
                    return Err(NativeProfileError::Invalid("xvfb_run_path is not executable"));
                }
            }
        }
        if self.expected_build.trim().is_empty() || self.expected_build.len() > 64 {
            return Err(NativeProfileError::Invalid("expected_build must contain 1 to 64 bytes"));
        }
        validate_env_name(&self.source_connection_env)?;
        if let Some(name) = &self.user_env {
            validate_env_name(name)?;
        }
        if let Some(name) = &self.password_env {
            validate_env_name(name)?;
        }
        match self.source_kind {
            SourceKind::Server => {
                if self.user_env.is_none() {
                    return Err(NativeProfileError::Invalid(
                        "user_env is required for server source profiles",
                    ));
                }
            }
            SourceKind::File => {}
        }
        if self.max_input_bytes == 0 || self.max_input_bytes > MAX_INPUT_BYTES {
            return Err(NativeProfileError::Invalid(
                "max_input_bytes is outside the allowed limit",
            ));
        }
        if self.deadline_ms == 0 || self.deadline_ms > MAX_DEADLINE_MS {
            return Err(NativeProfileError::Invalid("deadline_ms is outside the allowed limit"));
        }
        if self.max_log_bytes < MIN_MAX_LOG_BYTES || self.max_log_bytes > MAX_LOG_BYTES {
            return Err(NativeProfileError::Invalid("max_log_bytes is outside the allowed limit"));
        }
        validate_owned_work_root(&self.work_root)?;
        Ok(())
    }

    pub(super) fn validate_runtime_environment(&self) -> Result<(), NativeProfileError> {
        let source = runtime_env(&self.source_connection_env)?;
        match self.source_kind {
            SourceKind::Server => {
                if runtime_env(self.user_env.as_deref().ok_or(NativeProfileError::Invalid(
                    "user_env is required for server source profiles",
                ))?)?
                .is_empty()
                {
                    return Err(NativeProfileError::Invalid("configured source user is empty"));
                }
                if let Some(password) = &self.password_env {
                    let _ = runtime_env(password)?;
                }
                let value = source
                    .to_str()
                    .ok_or(NativeProfileError::Invalid("source connection must be valid UTF-8"))?;
                if super::source::parse_server_location(value).is_none() {
                    return Err(NativeProfileError::Invalid(
                        "source connection must name exactly one host and infobase",
                    ));
                }
            }
            SourceKind::File => {
                let path = std::path::Path::new(&source);
                if super::source::canonical_file_source(path).is_none() {
                    return Err(NativeProfileError::Invalid(
                        "file source must name an existing canonical infobase directory",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn prepare_job_root(&self) -> Result<(), NativeProfileError> {
        let jobs = self.work_root.join("owned-jobs");
        match std::fs::symlink_metadata(&jobs) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(NativeProfileError::Invalid(
                        "worker job root is not a private directory",
                    ));
                }
                let canonical = jobs
                    .canonicalize()
                    .map_err(|_| NativeProfileError::Invalid("worker job root is unavailable"))?;
                if canonical != jobs {
                    return Err(NativeProfileError::Invalid("worker job root must be canonical"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&jobs).map_err(|_| {
                    NativeProfileError::Invalid("worker job root cannot be created")
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&jobs, std::fs::Permissions::from_mode(0o700))
                        .map_err(|_| {
                            NativeProfileError::Invalid("worker job root cannot be secured")
                        })?;
                }
            }
            Err(_) => {
                return Err(NativeProfileError::Invalid("worker job root is unavailable"));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let metadata = std::fs::metadata(&jobs)
                .map_err(|_| NativeProfileError::Invalid("worker job root is unavailable"))?;
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o777 != 0o700
            {
                return Err(NativeProfileError::Invalid(
                    "worker job root must be owned by this user and have mode 0700",
                ));
            }
        }
        cleanup_owned_jobs(&jobs)
    }

    pub(crate) async fn run(
        &self,
        client: &onec_client::Client,
        request: &super::types::CheckRequest,
        code: &str,
        cancel: tokio_util::sync::CancellationToken,
        deadline: std::time::Instant,
    ) -> super::types::CheckResult {
        let mut result = super::echoed_result(request, super::types::CheckStatus::Unsupported);
        let workspace = match JobWorkspace::create(&self.work_root) {
            Ok(workspace) => workspace,
            Err(_) => {
                result.failure = Some(super::types::CheckFailure {
                    code: "workspace_unavailable".to_owned(),
                    message: "The private native-check workspace is unavailable".to_owned(),
                });
                return result;
            }
        };
        let source = match runtime_env(&self.source_connection_env) {
            Ok(source) => source,
            Err(_) => {
                result.failure = Some(super::types::CheckFailure {
                    code: "source_unavailable".to_owned(),
                    message: "The configured source connection is unavailable".to_owned(),
                });
                return finish_with_cleanup(result, workspace);
            }
        };
        let mut runtime_passport = None;
        let actual_build = match self.source_kind {
            SourceKind::Server => {
                let configured_source = match source.to_str() {
                    Some(value) => value,
                    None => {
                        result.failure = Some(super::types::CheckFailure {
                            code: "source_unavailable".to_owned(),
                            message: "The configured source connection is unavailable".to_owned(),
                        });
                        return finish_with_cleanup(result, workspace);
                    }
                };
                let passport = match super::runtime::server_runtime_passport(
                    client,
                    configured_source,
                    &self.expected_build,
                    deadline,
                    &cancel,
                )
                .await
                {
                    Ok(passport) => passport,
                    Err(error) => {
                        apply_native_check_error(
                            &mut result,
                            check_error(error.code(), error.message()),
                        );
                        return finish_with_cleanup(result, workspace);
                    }
                };
                runtime_passport = Some(passport.clone());
                passport.build
            }
            SourceKind::File => {
                let path = std::path::Path::new(&source);
                if super::source::canonical_file_source(path).is_none() {
                    result.failure = Some(super::types::CheckFailure {
                        code: "source_unavailable".to_owned(),
                        message: "The configured file source is unavailable".to_owned(),
                    });
                    return finish_with_cleanup(result, workspace);
                }
                self.expected_build.clone()
            }
        };
        let Some(core_library) = self.designer_path.parent().map(|path| path.join("core83.so"))
        else {
            result.failure = Some(super::types::CheckFailure {
                code: "source_proof_unavailable".to_owned(),
                message: "The configured Designer binary version cannot be verified".to_owned(),
            });
            return finish_with_cleanup(result, workspace);
        };
        let designer_build = match super::source::probe_platform_build(
            &self.python_path,
            &core_library,
            workspace.path(),
            deadline,
            &cancel,
            self.max_log_bytes,
        )
        .await
        {
            Ok(build) => build,
            Err(error) => {
                result.status = super::types::CheckStatus::Error;
                result.failure = Some(super::types::CheckFailure {
                    code: error.code().to_owned(),
                    message: error.message().to_owned(),
                });
                return finish_with_cleanup(result, workspace);
            }
        };
        if designer_build != self.expected_build || designer_build != actual_build {
            apply_native_check_error(
                &mut result,
                check_error(
                    "compiler_build_mismatch",
                    "The configured Designer does not match the source runtime build",
                ),
            );
            return finish_with_cleanup(result, workspace);
        }
        let actual_build = designer_build;
        result.compiler = Some(super::types::CompilerInfo {
            build: Some(actual_build.clone()),
            compatibility: None,
            fingerprint: None,
        });
        let mut checked = run_native_check(
            self,
            client,
            request,
            code,
            workspace.path(),
            0,
            deadline,
            &cancel,
            &actual_build,
            runtime_passport.as_ref(),
        )
        .await;
        if checked.as_ref().is_err_and(|error| error.code == "source_snapshot_stale") {
            if self.source_kind == SourceKind::Server {
                let Some(configured_source) = source.to_str() else {
                    result.status = super::types::CheckStatus::Error;
                    result.failure = Some(super::types::CheckFailure {
                        code: "source_unavailable".to_owned(),
                        message: "The configured source connection is unavailable".to_owned(),
                    });
                    return finish_with_cleanup(result, workspace);
                };
                runtime_passport = match super::runtime::server_runtime_passport(
                    client,
                    configured_source,
                    &self.expected_build,
                    deadline,
                    &cancel,
                )
                .await
                {
                    Ok(passport) => Some(passport),
                    Err(error) => {
                        apply_native_check_error(
                            &mut result,
                            check_error(error.code(), error.message()),
                        );
                        return finish_with_cleanup(result, workspace);
                    }
                };
            }
            checked = run_native_check(
                self,
                client,
                request,
                code,
                workspace.path(),
                1,
                deadline,
                &cancel,
                &actual_build,
                runtime_passport.as_ref(),
            )
            .await;
        }
        match checked {
            Ok(mut checked) => {
                if checked.compiler.is_none() {
                    checked.compiler = Some(super::types::CompilerInfo {
                        build: Some(actual_build),
                        compatibility: None,
                        fingerprint: None,
                    });
                }
                finish_with_cleanup(checked, workspace)
            }
            Err(error) => {
                apply_native_check_error(&mut result, error);
                finish_with_cleanup(result, workspace)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct NativeCheckError {
    status: super::types::CheckStatus,
    code: &'static str,
    message: &'static str,
    compilation_status: Option<super::types::CompilationStatus>,
}

fn apply_native_check_error(result: &mut super::types::CheckResult, error: NativeCheckError) {
    result.status = error.status;
    result.valid = None;
    result.compilation_status = error.compilation_status.or(result.compilation_status);
    result.failure = Some(super::types::CheckFailure {
        code: error.code.to_owned(),
        message: error.message.to_owned(),
    });
}

#[allow(
    clippy::too_many_arguments,
    reason = "One native compile attempt needs its independent source, request, deadline, and passport inputs"
)]
async fn run_native_check(
    profile: &NativeProfile,
    client: &onec_client::Client,
    request: &super::types::CheckRequest,
    code: &str,
    workspace: &Path,
    attempt_index: u8,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
    actual_build: &str,
    initial_passport: Option<&super::runtime::ServerRuntimePassport>,
) -> Result<super::types::CheckResult, NativeCheckError> {
    use super::commands::{
        create_file_infobase_args, local_action_args, source_dump_args, LocalAction,
    };
    use super::source::{
        canonical_file_source, flat_module_export_name, native_diagnostic_owner,
        parse_script_variant, resolve_flat_module_export, FlatModuleResolveError,
    };
    use super::types::{
        CheckContext, CheckIssue, CheckStatus, CompilationStatus, InputKind, ModuleOrigin,
        ModuleType,
    };

    let configured_source = runtime_env(&profile.source_connection_env).map_err(|_| {
        check_error("source_unavailable", "The configured file source is unavailable")
    })?;
    let source_path = Path::new(&configured_source);
    let source = match profile.source_kind {
        SourceKind::File => canonical_file_source(source_path).ok_or_else(|| {
            check_error("source_unavailable", "The configured file source is unavailable")
        })?,
        SourceKind::Server => source_path.to_path_buf(),
    };
    let user =
        profile.user_env.as_deref().map(runtime_env).transpose().map_err(|_| {
            check_error("source_unavailable", "The configured source is unavailable")
        })?;
    let password =
        profile.password_env.as_deref().map(runtime_env).transpose().map_err(|_| {
            check_error("source_unavailable", "The configured source is unavailable")
        })?;

    let synthetic_name =
        format!("BslCheck{}", bsl_metadata::Uuid::new_v4().to_string().replace('-', ""));
    let (module_type, owner, input_kind, synthetic, selected_extension) =
        match (&request.context, request.input_kind) {
            (
                Some(CheckContext::Metadata { owner, origin: ModuleOrigin::Configuration }),
                InputKind::Module,
            ) => {
                let module_type = request.module_type.ok_or_else(|| {
                    check_error(
                        "module_type_required",
                        "The module type is required for this owner",
                    )
                })?;
                (module_type, owner.clone(), InputKind::Module, false, None)
            }
            (
                Some(CheckContext::Metadata { owner, origin: ModuleOrigin::Extension { name } }),
                InputKind::Module,
            ) => {
                let module_type = request.module_type.ok_or_else(|| {
                    check_error(
                        "module_type_required",
                        "The module type is required for this owner",
                    )
                })?;
                (module_type, owner.clone(), InputKind::Module, false, Some(name.clone()))
            }
            (Some(CheckContext::Synthetic { module_type }), InputKind::Module) => (
                *module_type,
                synthetic_owner(*module_type, &synthetic_name),
                InputKind::Module,
                true,
                None,
            ),
            (None, InputKind::Snippet) => (
                ModuleType::Common,
                synthetic_owner(ModuleType::Common, &synthetic_name),
                InputKind::Snippet,
                true,
                None,
            ),
            _ => {
                return Err(check_error(
                    "module_context_required",
                    "A supported module context is required",
                ));
            }
        };
    let attempt = workspace.join(format!("attempt-{attempt_index}"));
    fs::create_dir(&attempt).map_err(|_| {
        check_error("workspace_unavailable", "The private native-check workspace is unavailable")
    })?;
    set_private_dir(&attempt)?;
    let source_extensions_data = attempt.join("ibcmd-source-data");
    let copy_extensions_data = attempt.join("ibcmd-copy-data");
    let file_extension_state = if profile.source_kind == SourceKind::File {
        Some(
            file_extension_manifest(
                profile,
                &source,
                &attempt,
                &source_extensions_data,
                actual_build,
                "extensions",
                deadline,
                cancel,
            )
            .await?,
        )
    } else {
        None
    };
    let applied_cf = attempt.join("source-applied.cf");
    let source_args = source_dump_args(
        profile.source_kind,
        source.as_os_str(),
        user.as_deref(),
        password.as_deref(),
        &applied_cf,
    )
    .ok_or_else(|| check_error("source_unavailable", "The configured source is unavailable"))?;
    run_designer_stage(profile, &source_args, &attempt, "source-dump.log", deadline, cancel)
        .await?;
    secure_private_file(&applied_cf)?;
    let before_hash = hash_file(&applied_cf)?;

    let file_properties =
        file_extension_state.as_ref().map(|(_, properties)| properties.clone()).unwrap_or_default();
    let mut effective_extensions = match profile.source_kind {
        SourceKind::File => effective_extensions_from_file(&file_properties),
        SourceKind::Server => {
            effective_extensions_from_runtime(initial_passport.ok_or_else(|| {
                check_error(
                    "runtime_passport_unavailable",
                    "The source runtime state is unavailable",
                )
            })?)?
        }
    };
    if let Some(extension_name) = selected_extension.as_deref() {
        if !effective_extensions.iter().any(|extension| extension.name == extension_name) {
            return Err(unsupported_error(
                "extension_not_active",
                "The requested extension is not active in the source configuration",
            ));
        }
    }
    if !effective_extensions.is_empty() && profile.source_kind == SourceKind::Server {
        verify_ibcmd_build(profile, &attempt, actual_build, deadline, cancel).await?;
    }
    export_effective_extensions(
        profile,
        profile.source_kind,
        &source,
        user.as_deref(),
        password.as_deref(),
        &mut effective_extensions,
        &attempt,
        "source-extension",
        deadline,
        cancel,
    )
    .await?;

    let ib = attempt.join("ib");
    let create_args = create_file_infobase_args(&ib).ok_or_else(|| {
        check_error("workspace_unavailable", "The private infobase path is invalid")
    })?;
    run_designer_stage(profile, &create_args, &attempt, "create.log", deadline, cancel).await?;
    let load =
        local_action_args(&ib, LocalAction::LoadConfig(&applied_cf)).expect("absolute job paths");
    run_designer_stage(profile, &load, &attempt, "load.log", deadline, cancel).await?;
    let update = local_action_args(&ib, LocalAction::UpdateDatabase).expect("absolute job paths");
    run_designer_stage(profile, &update, &attempt, "update.log", deadline, cancel).await?;

    if !effective_extensions.is_empty() {
        for (index, extension) in effective_extensions.iter().enumerate() {
            let cfe_path = extension.cfe_path.as_deref().ok_or_else(|| {
                check_error(
                    "extension_snapshot_unavailable",
                    "The extension snapshot is unavailable",
                )
            })?;
            let load =
                local_action_args(&ib, LocalAction::LoadExtensionConfig(cfe_path, &extension.name))
                    .ok_or_else(|| {
                        check_error(
                            "extension_snapshot_unavailable",
                            "The extension snapshot is unavailable",
                        )
                    })?;
            run_designer_stage(
                profile,
                &load,
                &attempt,
                &format!("extension-load-{index}.log"),
                deadline,
                cancel,
            )
            .await?;
            let update =
                local_action_args(&ib, LocalAction::UpdateExtensionDatabase(&extension.name))
                    .ok_or_else(|| {
                        check_error(
                            "extension_snapshot_unavailable",
                            "The extension snapshot is unavailable",
                        )
                    })?;
            run_designer_stage(
                profile,
                &update,
                &attempt,
                &format!("extension-apply-{index}.log"),
                deadline,
                cancel,
            )
            .await?;
            update_extension_properties(
                profile,
                &ib,
                &copy_extensions_data,
                &attempt,
                extension,
                deadline,
                cancel,
            )
            .await?;
        }
        verify_copy_extensions(
            profile,
            &ib,
            &copy_extensions_data,
            &attempt,
            &effective_extensions,
            deadline,
            cancel,
        )
        .await?;
        let applicability = local_action_args(&ib, LocalAction::CheckExtensionApplicability)
            .expect("absolute job paths");
        run_designer_stage(
            profile,
            &applicability,
            &attempt,
            "extensions-applicability.log",
            deadline,
            cancel,
        )
        .await?;
    }

    let xml = attempt.join("configuration-files");
    let modules = attempt.join("modules");
    fs::create_dir(&xml).map_err(|_| {
        check_error("workspace_unavailable", "The private module workspace is unavailable")
    })?;
    fs::create_dir(&modules).map_err(|_| {
        check_error("workspace_unavailable", "The private module workspace is unavailable")
    })?;
    set_private_dir(&xml)?;
    set_private_dir(&modules)?;
    let dump_xml =
        local_action_args(&ib, LocalAction::DumpConfigToFiles(&xml)).expect("absolute job paths");
    run_designer_stage(profile, &dump_xml, &attempt, "dump-config.log", deadline, cancel).await?;
    let compatibility = super::source::parse_compatibility_mode(&xml).map_err(|_| {
        check_error(
            "source_metadata_unavailable",
            "The source compatibility mode cannot be verified",
        )
    })?;
    if file_extension_state
        .as_ref()
        .is_some_and(|(probe, _)| matches!(probe, FileExtensionProbe::LegacyNoExtensionStorage))
        && compatibility != "Version8_2_16"
    {
        return Err(check_error(
            "extension_snapshot_unavailable",
            "The file source compatibility does not support the verified empty-extension rule",
        ));
    }
    if synthetic {
        add_synthetic_owner(&xml, module_type, &synthetic_name, input_kind == InputKind::Snippet)?;
    }
    let load_xml =
        local_action_args(&ib, LocalAction::LoadConfigFromFiles(&xml)).expect("absolute job paths");
    run_designer_stage(profile, &load_xml, &attempt, "load-config-files.log", deadline, cancel)
        .await?;
    let dump_modules =
        local_action_args(&ib, LocalAction::DumpModuleFiles(&modules)).expect("absolute job paths");
    run_designer_stage(profile, &dump_modules, &attempt, "dump-modules.log", deadline, cancel)
        .await?;
    secure_module_files(&modules)?;

    let target_xml = if let Some(extension_name) = selected_extension.as_deref() {
        let extension_xml = attempt.join("extension-xml");
        let extension_modules = attempt.join("extension-modules");
        fs::create_dir(&extension_xml).map_err(|_| {
            check_error("workspace_unavailable", "The private extension workspace is unavailable")
        })?;
        fs::create_dir(&extension_modules).map_err(|_| {
            check_error("workspace_unavailable", "The private extension workspace is unavailable")
        })?;
        set_private_dir(&extension_xml)?;
        set_private_dir(&extension_modules)?;
        let dump_extension_xml = local_action_args(
            &ib,
            LocalAction::DumpExtensionConfigToFiles(&extension_xml, extension_name),
        )
        .ok_or_else(|| {
            check_error("extension_snapshot_unavailable", "The extension metadata is unavailable")
        })?;
        run_designer_stage(
            profile,
            &dump_extension_xml,
            &attempt,
            "dump-extension-config.log",
            deadline,
            cancel,
        )
        .await?;
        let dump_extension_modules = local_action_args(
            &ib,
            LocalAction::DumpExtensionModuleFiles(&extension_modules, extension_name),
        )
        .ok_or_else(|| {
            check_error("extension_snapshot_unavailable", "The extension module is unavailable")
        })?;
        run_designer_stage(
            profile,
            &dump_extension_modules,
            &attempt,
            "dump-extension-modules.log",
            deadline,
            cancel,
        )
        .await?;
        secure_module_files(&extension_modules)?;
        Some((extension_xml, extension_modules))
    } else {
        None
    };

    let variant = parse_script_variant(&xml).map_err(|_| {
        check_error("source_metadata_unavailable", "The source module metadata cannot be resolved")
    })?;
    let owner_xml = target_xml.as_ref().map_or(xml.as_path(), |(xml, _)| xml.as_path());
    let target_modules =
        target_xml.as_ref().map_or_else(|| modules.clone(), |(_, modules)| modules.clone());
    validate_module_owner(owner_xml, &owner, module_type)?;
    let export = flat_module_export_name(&owner, module_type, variant).ok_or_else(|| {
        check_error("module_context_unsupported", "The requested module owner is not supported")
    })?;
    let module_path =
        match resolve_flat_module_export(&target_modules, &owner, module_type, variant) {
            Ok(path) => path,
            Err(FlatModuleResolveError::NotFound) => {
                let path = target_modules.join(export);
                create_private_file(&path)?;
                path
            }
            Err(_) => {
                return Err(check_error(
                    "module_source_unavailable",
                    "The native module source cannot be resolved",
                ));
            }
        };
    let mut diagnostic_owner =
        native_diagnostic_owner(&owner, module_type, variant).ok_or_else(|| {
            check_error("module_context_unsupported", "The requested module owner is not supported")
        })?;
    if let Some(extension_name) = selected_extension.as_deref() {
        diagnostic_owner = format!("{extension_name} {diagnostic_owner}");
    }
    let passes = applicable_modes(&xml, owner_xml, &owner, module_type)?;

    // A clean baseline prevents unrelated configuration errors from being attributed to input.
    for (index, mode) in passes.iter().enumerate() {
        let baseline_action = check_modules_action(*mode, selected_extension.as_deref());
        let args = local_action_args(&ib, baseline_action).expect("absolute job paths");
        let log_name = format!("baseline-{index}.log");
        let (log, exit_code) =
            run_check_stage(profile, &args, &attempt, &log_name, deadline, cancel, true).await?;
        if !log.is_empty() {
            let parsed = super::compiler::parse_diagnostics(&log).map_err(|_| {
                check_error(
                    "diagnostics_unavailable",
                    "The native compiler output cannot be interpreted safely",
                )
            })?;
            if !native_success_verdict(&log, exit_code, &parsed) {
                return Err(check_error(
                    "baseline_compilation_failed",
                    "The source configuration has unrelated compilation diagnostics",
                ));
            }
        } else {
            return Err(check_error(
                "compiler_unavailable",
                "The native compiler did not provide a completion record",
            ));
        }
    }

    let calibration = "Процедура BA023Calibration()\n@\nКонецПроцедуры";
    write_native_module(&module_path, calibration)?;
    for (index, mode) in passes.iter().enumerate() {
        let load_action = if let Some(extension_name) = selected_extension.as_deref() {
            LocalAction::LoadExtensionModuleFiles(&target_modules, extension_name)
        } else {
            LocalAction::LoadModuleFiles(&target_modules)
        };
        let args = local_action_args(&ib, load_action).expect("absolute job paths");
        run_designer_stage(
            profile,
            &args,
            &attempt,
            &format!("calibration-load-{index}.log"),
            deadline,
            cancel,
        )
        .await?;
        let check_action = check_modules_action(*mode, selected_extension.as_deref());
        let args = local_action_args(&ib, check_action).expect("absolute job paths");
        let (log, exit_code) = run_check_stage(
            profile,
            &args,
            &attempt,
            &format!("calibration-{index}.log"),
            deadline,
            cancel,
            true,
        )
        .await?;
        let parsed = super::compiler::parse_diagnostics(&log).map_err(|_| {
            check_error(
                "compiler_calibration_failed",
                "The native compiler context could not be calibrated",
            )
        })?;
        if exit_code != 101
            || !parsed.issues.iter().any(|issue| issue.owner == diagnostic_owner)
            || parsed.truncated
        {
            return Err(check_error(
                "compiler_calibration_failed",
                "The native compiler context could not be calibrated",
            ));
        }
    }

    let transformed = prepare_native_module(code, input_kind);
    write_native_module(&module_path, &transformed)?;
    let mut result = super::echoed_result(request, CheckStatus::Valid);
    result.module_type = Some(module_type);
    result.owner = Some(owner);
    result.contexts_required = passes.iter().map(|mode| mode_label(*mode).to_owned()).collect();
    for (index, mode) in passes.iter().enumerate() {
        let load_action = if let Some(extension_name) = selected_extension.as_deref() {
            LocalAction::LoadExtensionModuleFiles(&target_modules, extension_name)
        } else {
            LocalAction::LoadModuleFiles(&target_modules)
        };
        let args = local_action_args(&ib, load_action).expect("absolute job paths");
        run_designer_stage(
            profile,
            &args,
            &attempt,
            &format!("input-load-{index}.log"),
            deadline,
            cancel,
        )
        .await?;
        let check_action = check_modules_action(*mode, selected_extension.as_deref());
        let args = local_action_args(&ib, check_action).expect("absolute job paths");
        let (log, exit_code) = run_check_stage(
            profile,
            &args,
            &attempt,
            &format!("input-check-{index}.log"),
            deadline,
            cancel,
            true,
        )
        .await?;
        result.contexts_checked.push(mode_label(*mode).to_owned());
        let parsed = parse_input_diagnostics(&log, exit_code)?;
        result.truncated |= parsed.truncated;
        let successful = native_success_verdict(&log, exit_code, &parsed);
        if !successful {
            if log.is_empty() {
                return Err(check_error(
                    "compiler_unavailable",
                    "The native compiler did not provide a completion record",
                ));
            }
            if parsed.truncated || parsed.issues.is_empty() {
                return Err(check_error(
                    "compiler_unavailable",
                    "The native compiler did not confirm a usable result",
                ));
            }
            if exit_code != 101 {
                return Err(check_error(
                    "diagnostics_unavailable",
                    "The native compiler reported diagnostics without an invalid status",
                ));
            }
            let mut user_issues = Vec::new();
            for issue in parsed.issues {
                if issue.owner != diagnostic_owner {
                    return Err(check_error(
                        "unrelated_compilation_error",
                        "The native check encountered an unrelated compilation diagnostic",
                    ));
                }
                let (line, column) = map_input_position(code, issue.line, issue.column, input_kind);
                let (Some(line), Some(column)) = (line, column) else {
                    return Err(position_unavailable_error());
                };
                user_issues.push(CheckIssue {
                    message: issue.message,
                    line: Some(line),
                    column: Some(column),
                });
            }
            result.issues.extend(user_issues);
        }
    }

    // Re-read the source after compilation; an outdated snapshot is never reported as valid.
    let verify = attempt.join("source-verify.cf");
    let source_args = source_dump_args(
        profile.source_kind,
        source.as_os_str(),
        user.as_deref(),
        password.as_deref(),
        &verify,
    )
    .ok_or_else(|| check_error("source_unavailable", "The configured source is unavailable"))?;
    run_designer_stage(profile, &source_args, &attempt, "source-verify.log", deadline, cancel)
        .await?;
    secure_private_file(&verify)?;
    if hash_file(&verify)? != before_hash {
        return Err(check_error(
            "source_snapshot_stale",
            "The source configuration changed during native compilation",
        ));
    }
    if let Some((initial_probe, initial_properties)) = &file_extension_state {
        let current = file_extension_manifest(
            profile,
            &source,
            &attempt,
            &source_extensions_data,
            actual_build,
            "extensions-verify",
            deadline,
            cancel,
        )
        .await?;
        if &current.0 != initial_probe || &current.1 != initial_properties {
            return Err(check_error(
                "source_snapshot_stale",
                "The source extension state changed during native compilation",
            ));
        }
    }
    if let Some(initial) = initial_passport {
        let configured_source = source.to_str().ok_or_else(|| {
            check_error("source_unavailable", "The configured source is unavailable")
        })?;
        let current = super::runtime::server_runtime_passport(
            client,
            configured_source,
            actual_build,
            deadline,
            cancel,
        )
        .await
        .map_err(|error| check_error(error.code(), error.message()))?;
        if &current != initial {
            return Err(check_error(
                "source_snapshot_stale",
                "The source runtime configuration changed during native compilation",
            ));
        }
    }
    verify_effective_extension_hashes(
        profile,
        profile.source_kind,
        &source,
        user.as_deref(),
        password.as_deref(),
        &effective_extensions,
        &attempt,
        "source-extension-verify",
        deadline,
        cancel,
    )
    .await?;
    if result.issues.is_empty() {
        result.status = CheckStatus::Valid;
        result.valid = Some(true);
        result.compilation_status = Some(CompilationStatus::Valid);
    } else {
        result.status = CheckStatus::Invalid;
        result.valid = Some(false);
        result.compilation_status = Some(CompilationStatus::Invalid);
    }
    let mut fingerprint = blake3::Hasher::new();
    fingerprint.update(before_hash.as_bytes());
    fingerprint.update(&(compatibility.len() as u64).to_le_bytes());
    fingerprint.update(compatibility.as_bytes());
    match &file_extension_state {
        Some((FileExtensionProbe::LegacyNoExtensionStorage, _)) => {
            fingerprint.update(b"\0legacy-no-extension-storage\0");
        }
        Some((FileExtensionProbe::Names(names), properties)) => {
            fingerprint.update(b"\0file-extension-list\0");
            for name in names {
                fingerprint.update(&(name.len() as u64).to_le_bytes());
                fingerprint.update(name.as_bytes());
            }
            for extension in properties {
                fingerprint.update(file_extension_properties_hash(extension).as_bytes());
            }
        }
        None => {
            fingerprint.update(b"\0server-extension-list\0");
            if let Some(passport) = initial_passport {
                for extension in &passport.applied {
                    fingerprint.update(
                        runtime_extension_properties_hash(
                            &extension.name,
                            extension.active,
                            extension.safe_mode,
                            &extension.scope,
                        )
                        .as_bytes(),
                    );
                }
                for disabled in &passport.disabled {
                    fingerprint.update(&(disabled.len() as u64).to_le_bytes());
                    fingerprint.update(disabled.as_bytes());
                }
            }
        }
    }
    for extension in &effective_extensions {
        fingerprint.update(&(extension.name.len() as u64).to_le_bytes());
        fingerprint.update(extension.name.as_bytes());
        fingerprint.update(extension.source_properties_hash.as_bytes());
        fingerprint.update(
            extension
                .cfe_hash
                .as_ref()
                .ok_or_else(|| {
                    check_error(
                        "extension_snapshot_unavailable",
                        "The extension snapshot is unavailable",
                    )
                })?
                .as_bytes(),
        );
    }
    result.compiler = Some(super::types::CompilerInfo {
        build: Some(actual_build.to_owned()),
        compatibility: Some(compatibility),
        fingerprint: Some(fingerprint.finalize().to_hex().to_string()),
    });
    Ok(result)
}

fn check_error(code: &'static str, message: &'static str) -> NativeCheckError {
    let status = match code {
        "compiler_build_mismatch" | "runtime_build_mismatch" | "unsupported_extension_scope" => {
            super::types::CheckStatus::Unsupported
        }
        _ => super::types::CheckStatus::Error,
    };
    NativeCheckError { status, code, message, compilation_status: None }
}

fn unsupported_error(code: &'static str, message: &'static str) -> NativeCheckError {
    NativeCheckError {
        status: super::types::CheckStatus::Unsupported,
        code,
        message,
        compilation_status: None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FileExtensionProbe {
    Names(Vec<String>),
    LegacyNoExtensionStorage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileExtensionProperties {
    name: String,
    version: String,
    active: bool,
    purpose: String,
    safe_mode: bool,
    security_profile: String,
    unsafe_action_protection: bool,
    used_in_distributed_infobase: bool,
    scope: String,
    hash_sum: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EffectiveExtension {
    name: String,
    safe_mode: bool,
    scope: String,
    cfe_hash: Option<blake3::Hash>,
    source_properties_hash: blake3::Hash,
    file_properties: Option<FileExtensionProperties>,
    cfe_path: Option<PathBuf>,
}

fn fingerprint_fields(fields: &[&str]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    for field in fields {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hasher.finalize()
}

fn file_extension_properties_hash(extension: &FileExtensionProperties) -> blake3::Hash {
    fingerprint_fields(&[
        &extension.name,
        &extension.version,
        if extension.active { "yes" } else { "no" },
        &extension.purpose,
        if extension.safe_mode { "yes" } else { "no" },
        &extension.security_profile,
        if extension.unsafe_action_protection { "yes" } else { "no" },
        if extension.used_in_distributed_infobase { "yes" } else { "no" },
        &extension.scope,
        &extension.hash_sum,
    ])
}

fn runtime_extension_properties_hash(
    name: &str,
    active: bool,
    safe_mode: bool,
    scope: &str,
) -> blake3::Hash {
    fingerprint_fields(&[
        name,
        if active { "yes" } else { "no" },
        if safe_mode { "yes" } else { "no" },
        scope,
    ])
}

fn reconcile_file_extension_names(
    names: &[String],
    properties: Vec<FileExtensionProperties>,
) -> Result<Vec<FileExtensionProperties>, NativeCheckError> {
    let mut by_name = properties
        .into_iter()
        .map(|extension| (extension.name.clone(), extension))
        .collect::<std::collections::HashMap<_, _>>();
    if names.len() != by_name.len() {
        return Err(check_error(
            "extension_snapshot_unavailable",
            "The extension name and property snapshots do not match",
        ));
    }
    names
        .iter()
        .map(|name| {
            by_name.remove(name).ok_or_else(|| {
                check_error(
                    "extension_snapshot_unavailable",
                    "The extension name and property snapshots do not match",
                )
            })
        })
        .collect()
}

fn effective_extensions_from_file(
    properties: &[FileExtensionProperties],
) -> Vec<EffectiveExtension> {
    properties
        .iter()
        .filter(|extension| extension.active)
        .map(|extension| EffectiveExtension {
            name: extension.name.clone(),
            safe_mode: extension.safe_mode,
            scope: if extension.scope.eq_ignore_ascii_case("infobase") {
                "infobase".to_owned()
            } else {
                extension.scope.clone()
            },
            cfe_hash: None,
            source_properties_hash: file_extension_properties_hash(extension),
            file_properties: Some(extension.clone()),
            cfe_path: None,
        })
        .collect()
}

fn effective_extensions_from_runtime(
    passport: &super::runtime::ServerRuntimePassport,
) -> Result<Vec<EffectiveExtension>, NativeCheckError> {
    if passport.applied.iter().any(|extension| extension.active && extension.scope != "infobase") {
        return Err(unsupported_error(
            "effective_extension_snapshot_unavailable",
            "The runtime extension state includes an unsupported active extension scope",
        ));
    }
    Ok(passport
        .applied
        .iter()
        .filter(|extension| extension.active)
        .map(|extension| EffectiveExtension {
            name: extension.name.clone(),
            safe_mode: extension.safe_mode,
            scope: extension.scope.clone(),
            cfe_hash: None,
            source_properties_hash: runtime_extension_properties_hash(
                &extension.name,
                extension.active,
                extension.safe_mode,
                &extension.scope,
            ),
            file_properties: None,
            cfe_path: None,
        })
        .collect())
}

async fn inspect_file_extensions(
    profile: &NativeProfile,
    source: &Path,
    workspace: &Path,
    log_name: &str,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<FileExtensionProbe, NativeCheckError> {
    use super::compiler::{read_bounded_log, run_designer, Termination};

    if source.to_str().is_none_or(|value| value.chars().any(char::is_control)) {
        return Err(check_error("source_unavailable", "The configured file source is unavailable"));
    }
    let args = [
        std::ffi::OsString::from("DESIGNER"),
        std::ffi::OsString::from("/F"),
        source.as_os_str().to_owned(),
        std::ffi::OsString::from("/DisableStartupMessages"),
        std::ffi::OsString::from("/DisableStartupDialogs"),
        std::ffi::OsString::from("/DumpDBCfgList"),
        std::ffi::OsString::from("-AllExtensions"),
    ];
    let log_path = workspace.join(log_name);
    let output =
        run_designer(profile, &args, workspace, &log_path, deadline, cancel, profile.max_log_bytes)
            .await
            .map_err(|_| {
                check_error(
                    "extension_snapshot_unavailable",
                    "The extension list cannot be verified",
                )
            })?;
    let exit_code = match output.termination {
        Termination::Completed(Some(code @ (0 | 1))) => code,
        Termination::Cancelled => {
            return Err(check_error("cancelled", "Native module check was cancelled"));
        }
        Termination::Deadline => {
            return Err(check_error(
                "deadline_exceeded",
                "Native module check exceeded its deadline",
            ));
        }
        _ => {
            return Err(check_error(
                "extension_snapshot_unavailable",
                "The extension list cannot be verified",
            ));
        }
    };
    if output.truncated || !output.stdout.is_empty() || !output.stderr.is_empty() {
        return Err(check_error(
            "extension_snapshot_unavailable",
            "The extension list cannot be verified",
        ));
    }
    let (log, truncated) = read_bounded_log(&log_path, profile.max_log_bytes).map_err(|_| {
        check_error("extension_snapshot_unavailable", "The extension list cannot be verified")
    })?;
    if truncated {
        return Err(check_error(
            "extension_snapshot_unavailable",
            "The extension list cannot be verified",
        ));
    }
    classify_file_extension_list(exit_code, &log)
}

fn classify_file_extension_list(
    exit_code: i32,
    log: &[u8],
) -> Result<FileExtensionProbe, NativeCheckError> {
    match exit_code {
        0 => parse_file_extension_names(log).map(FileExtensionProbe::Names),
        1 if is_legacy_no_extension_storage_message(log) => {
            Ok(FileExtensionProbe::LegacyNoExtensionStorage)
        }
        _ => Err(check_error(
            "extension_snapshot_unavailable",
            "The extension list cannot be verified",
        )),
    }
}

fn is_legacy_no_extension_storage_message(log: &[u8]) -> bool {
    const MESSAGE: &str =
        "The database structure does not support extensions. Turn the compatibility mode off.";
    let Ok(text) = std::str::from_utf8(log) else { return false };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .is_some_and(|message| message == MESSAGE)
}

fn parse_file_extension_names(log: &[u8]) -> Result<Vec<String>, NativeCheckError> {
    let text = std::str::from_utf8(log).map_err(|_| {
        check_error("extension_snapshot_unavailable", "The extension list cannot be verified")
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut names = Vec::new();
    for line in text.lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        if name.len() > 128
            || name.chars().any(char::is_control)
            || names.iter().any(|old| old == name)
        {
            return Err(check_error(
                "extension_snapshot_unavailable",
                "The extension list cannot be verified",
            ));
        }
        names.push(name.to_owned());
        if names.len() > 128 {
            return Err(check_error(
                "extension_snapshot_unavailable",
                "The extension list cannot be verified",
            ));
        }
    }
    Ok(names)
}

fn parse_ibcmd_extension_list(
    output: &[u8],
) -> Result<Vec<FileExtensionProperties>, NativeCheckError> {
    use std::collections::BTreeMap;
    const FIELDS: [&str; 10] = [
        "name",
        "version",
        "active",
        "purpose",
        "safe-mode",
        "security-profile-name",
        "unsafe-action-protection",
        "used-in-distributed-infobase",
        "scope",
        "hash-sum",
    ];
    let text = std::str::from_utf8(output).map_err(|_| {
        check_error("extension_snapshot_unavailable", "The extension properties cannot be verified")
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.len() > 256 * 1024 {
        return Err(check_error(
            "extension_snapshot_unavailable",
            "The extension properties cannot be verified",
        ));
    }
    let mut records = Vec::new();
    let mut record = BTreeMap::<String, String>::new();
    let mut names = std::collections::HashSet::new();
    for line in text.lines().chain(std::iter::once("")) {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            if !record.is_empty() {
                records.push(parse_ibcmd_extension_record(&record, &FIELDS, &mut names)?);
                record.clear();
                if records.len() > 128 {
                    return Err(check_error(
                        "extension_snapshot_unavailable",
                        "The extension properties cannot be verified",
                    ));
                }
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(check_error(
                "extension_snapshot_unavailable",
                "The extension properties cannot be verified",
            ));
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || !FIELDS.contains(&key) || record.contains_key(key) || value.len() > 512
        {
            return Err(check_error(
                "extension_snapshot_unavailable",
                "The extension properties cannot be verified",
            ));
        }
        record.insert(key.to_owned(), value.to_owned());
    }
    Ok(records)
}

fn parse_ibcmd_extension_record(
    record: &std::collections::BTreeMap<String, String>,
    fields: &[&str],
    names: &mut std::collections::HashSet<String>,
) -> Result<FileExtensionProperties, NativeCheckError> {
    if record.len() != fields.len() || fields.iter().any(|field| !record.contains_key(*field)) {
        return Err(check_error(
            "extension_snapshot_unavailable",
            "The extension properties cannot be verified",
        ));
    }
    let unquote = |value: &str| -> Result<String, NativeCheckError> {
        let value = if value.starts_with('"') {
            value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).ok_or_else(|| {
                check_error(
                    "extension_snapshot_unavailable",
                    "The extension properties cannot be verified",
                )
            })?
        } else {
            if value.contains('"') {
                return Err(check_error(
                    "extension_snapshot_unavailable",
                    "The extension properties cannot be verified",
                ));
            }
            value
        };
        if value.len() > 512 || value.chars().any(char::is_control) {
            return Err(check_error(
                "extension_snapshot_unavailable",
                "The extension properties cannot be verified",
            ));
        }
        Ok(value.to_owned())
    };
    let value = |key: &str| -> Result<String, NativeCheckError> {
        unquote(record.get(key).expect("record keys validated"))
    };
    let bool_value = |key: &str| -> Result<bool, NativeCheckError> {
        match value(key)?.as_str() {
            "yes" => Ok(true),
            "no" => Ok(false),
            _ => Err(check_error(
                "extension_snapshot_unavailable",
                "The extension properties cannot be verified",
            )),
        }
    };
    let name = value("name")?;
    if name.trim().is_empty() || name.len() > 128 || !names.insert(name.clone()) {
        return Err(check_error(
            "extension_snapshot_unavailable",
            "The extension properties cannot be verified",
        ));
    }
    let scope = value("scope")?;
    Ok(FileExtensionProperties {
        name,
        version: value("version")?,
        active: bool_value("active")?,
        purpose: value("purpose")?,
        safe_mode: bool_value("safe-mode")?,
        security_profile: value("security-profile-name")?,
        unsafe_action_protection: bool_value("unsafe-action-protection")?,
        used_in_distributed_infobase: bool_value("used-in-distributed-infobase")?,
        scope,
        hash_sum: value("hash-sum")?,
    })
}

fn ibcmd_path(profile: &NativeProfile) -> Result<PathBuf, NativeCheckError> {
    let path =
        profile.designer_path.parent().map(|parent| parent.join("ibcmd")).ok_or_else(|| {
            check_error("extension_snapshot_unavailable", "The extension tool is unavailable")
        })?;
    let canonical = path.canonicalize().map_err(|_| {
        check_error("extension_snapshot_unavailable", "The extension tool is unavailable")
    })?;
    if canonical != path || !canonical.is_file() {
        return Err(check_error(
            "extension_snapshot_unavailable",
            "The extension tool is unavailable",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&canonical)
            .map_err(|_| {
                check_error("extension_snapshot_unavailable", "The extension tool is unavailable")
            })?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(check_error(
                "extension_snapshot_unavailable",
                "The extension tool is unavailable",
            ));
        }
    }
    Ok(canonical)
}

async fn run_ibcmd(
    profile: &NativeProfile,
    args: &[std::ffi::OsString],
    cwd: &Path,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<u8>, NativeCheckError> {
    use super::compiler::{run_bounded, Termination};

    let executable = ibcmd_path(profile)?;
    let output = run_bounded(&executable, args, cwd, deadline, cancel, profile.max_log_bytes)
        .await
        .map_err(|_| {
            check_error("extension_tool_failed", "The local extension tool could not be run")
        })?;
    match output.termination {
        Termination::Completed(Some(0)) if !output.truncated && output.stderr.is_empty() => {
            Ok(output.stdout)
        }
        Termination::Cancelled => {
            Err(check_error("cancelled", "Native module check was cancelled"))
        }
        Termination::Deadline => {
            Err(check_error("deadline_exceeded", "Native module check exceeded its deadline"))
        }
        Termination::OutputLimit => Err(check_error(
            "native_output_truncated",
            "The local extension tool output exceeded its limit",
        )),
        _ => Err(check_error(
            "extension_tool_failed",
            "The local extension tool could not verify the extension state",
        )),
    }
}

async fn verify_ibcmd_build(
    profile: &NativeProfile,
    workspace: &Path,
    expected_build: &str,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), NativeCheckError> {
    use std::ffi::OsString;

    let output =
        run_ibcmd(profile, &[OsString::from("--version")], workspace, deadline, cancel).await?;
    let output = std::str::from_utf8(&output).map_err(|_| {
        check_error("compiler_build_mismatch", "The extension tool build cannot be verified")
    })?;
    if output.trim() != expected_build {
        return Err(check_error(
            "compiler_build_mismatch",
            "The extension tool does not match the selected platform build",
        ));
    }
    Ok(())
}

async fn list_file_extensions(
    profile: &NativeProfile,
    infobase: &Path,
    data_dir: &Path,
    workspace: &Path,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<FileExtensionProperties>, NativeCheckError> {
    use std::ffi::OsString;

    ensure_private_directory(data_dir)?;
    let args = [
        OsString::from("extension"),
        OsString::from("list"),
        path_option_argument("--db-path=", infobase),
        path_option_argument("--data=", data_dir),
    ];
    let output = run_ibcmd(profile, &args, workspace, deadline, cancel).await?;
    parse_ibcmd_extension_list(&output)
}

async fn update_extension_properties(
    profile: &NativeProfile,
    infobase: &Path,
    data_dir: &Path,
    workspace: &Path,
    extension: &EffectiveExtension,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), NativeCheckError> {
    use std::ffi::OsString;

    ensure_private_directory(data_dir)?;
    let args = [
        OsString::from("extension"),
        OsString::from("update"),
        path_option_argument("--db-path=", infobase),
        path_option_argument("--data=", data_dir),
        OsString::from(format!("--name={}", extension.name)),
        OsString::from("--active=yes"),
        OsString::from(format!("--safe-mode={}", if extension.safe_mode { "yes" } else { "no" })),
        OsString::from(format!("--scope={}", extension.scope)),
    ];
    // ibcmd may emit informational stdout on success. The authoritative state check follows
    // immediately in verify_copy_extensions; run_ibcmd already requires exit 0 and empty stderr.
    run_ibcmd(profile, &args, workspace, deadline, cancel).await?;
    Ok(())
}

async fn verify_copy_extensions(
    profile: &NativeProfile,
    infobase: &Path,
    data_dir: &Path,
    workspace: &Path,
    expected: &[EffectiveExtension],
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), NativeCheckError> {
    let records =
        list_file_extensions(profile, infobase, data_dir, workspace, deadline, cancel).await?;
    if records.len() != expected.len()
        || records
            .iter()
            .map(|record| record.name.as_str())
            .ne(expected.iter().map(|extension| extension.name.as_str()))
    {
        return Err(check_error(
            "extension_properties_unavailable",
            "The copied extension set or order does not match the source",
        ));
    }
    for extension in expected {
        let Some(record) = records.iter().find(|record| record.name == extension.name) else {
            return Err(check_error(
                "extension_properties_unavailable",
                "The copied extension set does not match the source",
            ));
        };
        if !record.active
            || record.safe_mode != extension.safe_mode
            || record.scope != extension.scope
            || extension.file_properties.as_ref().is_some_and(|source| source != record)
        {
            return Err(check_error(
                "extension_properties_unavailable",
                "The copied extension properties do not match the source",
            ));
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "One extension export uses the source credentials and the current bounded job context"
)]
async fn export_effective_extensions(
    profile: &NativeProfile,
    source_kind: SourceKind,
    source: &Path,
    user: Option<&std::ffi::OsStr>,
    password: Option<&std::ffi::OsStr>,
    extensions: &mut [EffectiveExtension],
    workspace: &Path,
    log_prefix: &str,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), NativeCheckError> {
    use super::commands::source_dump_extension_args;

    for (index, extension) in extensions.iter_mut().enumerate() {
        let path = workspace.join(format!("{log_prefix}-{index}.cfe"));
        let args = source_dump_extension_args(
            source_kind,
            source.as_os_str(),
            user,
            password,
            &path,
            Some(&extension.name),
        )
        .ok_or_else(|| {
            check_error("extension_snapshot_unavailable", "The extension snapshot is unavailable")
        })?;
        run_designer_stage(
            profile,
            &args,
            workspace,
            &format!("{log_prefix}-{index}.log"),
            deadline,
            cancel,
        )
        .await?;
        secure_private_file(&path)?;
        extension.cfe_hash = Some(hash_file(&path)?);
        extension.cfe_path = Some(path);
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "Each source hash recheck uses the same independent bounded snapshot inputs"
)]
async fn verify_effective_extension_hashes(
    profile: &NativeProfile,
    source_kind: SourceKind,
    source: &Path,
    user: Option<&std::ffi::OsStr>,
    password: Option<&std::ffi::OsStr>,
    extensions: &[EffectiveExtension],
    workspace: &Path,
    log_prefix: &str,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), NativeCheckError> {
    use super::commands::source_dump_extension_args;

    for (index, extension) in extensions.iter().enumerate() {
        let path = workspace.join(format!("{log_prefix}-{index}.cfe"));
        let args = source_dump_extension_args(
            source_kind,
            source.as_os_str(),
            user,
            password,
            &path,
            Some(&extension.name),
        )
        .ok_or_else(|| {
            check_error("extension_snapshot_unavailable", "The extension snapshot is unavailable")
        })?;
        run_designer_stage(
            profile,
            &args,
            workspace,
            &format!("{log_prefix}-{index}.log"),
            deadline,
            cancel,
        )
        .await?;
        secure_private_file(&path)?;
        if Some(hash_file(&path)?) != extension.cfe_hash {
            return Err(check_error(
                "source_snapshot_stale",
                "An applied source extension changed during native compilation",
            ));
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "The file extension manifest combines source identity with this attempt's tool limits"
)]
async fn file_extension_manifest(
    profile: &NativeProfile,
    source: &Path,
    attempt: &Path,
    data_dir: &Path,
    expected_build: &str,
    log_prefix: &str,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(FileExtensionProbe, Vec<FileExtensionProperties>), NativeCheckError> {
    let probe = inspect_file_extensions(
        profile,
        source,
        attempt,
        &format!("{log_prefix}.log"),
        deadline,
        cancel,
    )
    .await?;
    match probe.clone() {
        FileExtensionProbe::LegacyNoExtensionStorage => Ok((probe, Vec::new())),
        FileExtensionProbe::Names(names) if names.is_empty() => Ok((probe, Vec::new())),
        FileExtensionProbe::Names(names) => {
            verify_ibcmd_build(profile, attempt, expected_build, deadline, cancel).await?;
            let properties =
                list_file_extensions(profile, source, data_dir, attempt, deadline, cancel).await?;
            let properties = reconcile_file_extension_names(&names, properties)?;
            if properties.iter().any(|extension| {
                extension.active && !extension.scope.eq_ignore_ascii_case("infobase")
            }) {
                return Err(unsupported_error(
                    "unsupported_extension_scope",
                    "The source uses an unsupported extension scope",
                ));
            }
            Ok((probe, properties))
        }
    }
}

fn path_option_argument(prefix: &str, path: &Path) -> std::ffi::OsString {
    let mut argument = std::ffi::OsString::from(prefix);
    argument.push(path.as_os_str());
    argument
}

async fn run_designer_stage(
    profile: &NativeProfile,
    args: &[std::ffi::OsString],
    cwd: &Path,
    log_name: &str,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<u8>, NativeCheckError> {
    run_designer_stage_with_exit(profile, args, cwd, log_name, deadline, cancel, false)
        .await
        .map(|(log, _)| log)
}

async fn run_check_stage(
    profile: &NativeProfile,
    args: &[std::ffi::OsString],
    cwd: &Path,
    log_name: &str,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
    permit_invalid: bool,
) -> Result<(Vec<u8>, i32), NativeCheckError> {
    run_designer_stage_with_exit(profile, args, cwd, log_name, deadline, cancel, permit_invalid)
        .await
}

async fn run_designer_stage_with_exit(
    profile: &NativeProfile,
    args: &[std::ffi::OsString],
    cwd: &Path,
    log_name: &str,
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
    permit_invalid: bool,
) -> Result<(Vec<u8>, i32), NativeCheckError> {
    use super::compiler::{read_bounded_log, run_designer, Termination};
    let log_path = cwd.join(log_name);
    let output =
        run_designer(profile, args, cwd, &log_path, deadline, cancel, profile.max_log_bytes)
            .await
            .map_err(|_| {
                check_error(
                    "native_process_failed",
                    "The local native compiler could not be started",
                )
            })?;
    if output.truncated {
        return Err(check_error(
            "native_output_truncated",
            "The local native compiler output exceeded its limit",
        ));
    }
    let exit_code = match output.termination {
        Termination::Completed(Some(0)) => 0,
        Termination::Completed(Some(101)) if permit_invalid => 101,
        Termination::Cancelled => {
            return Err(check_error("cancelled", "Native module check was cancelled"));
        }
        Termination::Deadline => {
            return Err(check_error(
                "deadline_exceeded",
                "Native module check exceeded its deadline",
            ));
        }
        Termination::OutputLimit => {
            return Err(check_error(
                "native_output_truncated",
                "The local native compiler output exceeded its limit",
            ));
        }
        _ => {
            return Err(check_error(
                "native_process_failed",
                "The local native compiler reported a failure",
            ));
        }
    };
    let (log, truncated) = read_bounded_log(&log_path, profile.max_log_bytes).map_err(|_| {
        check_error("native_log_unavailable", "The local native compiler log is unavailable")
    })?;
    if truncated {
        return Err(check_error(
            "native_output_truncated",
            "The local native compiler output exceeded its limit",
        ));
    }
    Ok((log, exit_code))
}

fn set_private_dir(_path: &Path) -> Result<(), NativeCheckError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700)).map_err(|_| {
            check_error(
                "workspace_unavailable",
                "The private native-check workspace cannot be secured",
            )
        })?;
    }
    Ok(())
}

fn secure_private_file(path: &Path) -> Result<(), NativeCheckError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        check_error("workspace_unavailable", "A private native-check file is unavailable")
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(check_error(
            "workspace_unavailable",
            "A private native-check file is unavailable",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|_| {
            check_error("workspace_unavailable", "A private native-check file cannot be secured")
        })?;
    }
    Ok(())
}

fn secure_module_files(directory: &Path) -> Result<(), NativeCheckError> {
    let entries = fs::read_dir(directory).map_err(|_| {
        check_error("module_source_unavailable", "The private module files are unavailable")
    })?;
    for entry in entries {
        let entry = entry.map_err(|_| {
            check_error("module_source_unavailable", "The private module files are unavailable")
        })?;
        secure_private_file(&entry.path())?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> Result<(), NativeCheckError> {
    use std::fs::OpenOptions;
    let _file = OpenOptions::new().write(true).create_new(true).open(path).map_err(|_| {
        check_error("module_source_unavailable", "The private module source cannot be created")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _file.set_permissions(fs::Permissions::from_mode(0o600)).map_err(|_| {
            check_error("module_source_unavailable", "The private module source cannot be secured")
        })?;
    }
    Ok(())
}

fn add_synthetic_owner(
    xml_root: &Path,
    module_type: super::types::ModuleType,
    name: &str,
    snippet: bool,
) -> Result<(), NativeCheckError> {
    use super::types::ModuleType;

    if !name.starts_with("BslCheck")
        || name.len() > 48
        || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(check_error(
            "source_metadata_unavailable",
            "The synthetic module name is invalid",
        ));
    }
    let (folder, object_kind, template) = match module_type {
        ModuleType::Object | ModuleType::Manager => {
            ("Catalogs", "Catalog", include_str!("fixtures/catalog.xml"))
        }
        ModuleType::ManagedForm => {
            ("CommonForms", "CommonForm", include_str!("fixtures/managed_form.xml"))
        }
        ModuleType::OrdinaryForm => {
            ("CommonForms", "CommonForm", include_str!("fixtures/ordinary_form.xml"))
        }
        ModuleType::Common => (
            "CommonModules",
            bsl_metadata::MdoType::CommonModule.english_name(),
            include_str!("fixtures/common_module.xml"),
        ),
    };
    validate_fixture_template(template)?;
    let folder_path = xml_root.join(folder);
    ensure_private_directory(&folder_path)?;
    let uuid = bsl_metadata::Uuid::new_v4().to_string();
    let mut rendered = template.replace("{{MODULE_NAME}}", name);
    rendered = rendered.replace("{{MODULE_UUID}}", &uuid);
    rendered = rendered.replace("{{CATALOG_NAME}}", name);
    rendered = rendered.replace("{{CATALOG_UUID}}", &uuid);
    rendered = rendered.replace("{{FORM_NAME}}", name);
    rendered = rendered.replace("{{FORM_UUID}}", &uuid);
    for property in ["OBJECT", "REF", "SELECTION", "LIST", "MANAGER"] {
        rendered = rendered.replace(
            &format!("{{{{TYPE_ID_{property}}}}}"),
            &bsl_metadata::Uuid::new_v4().to_string(),
        );
        rendered = rendered.replace(
            &format!("{{{{VALUE_ID_{property}}}}}"),
            &bsl_metadata::Uuid::new_v4().to_string(),
        );
    }
    if rendered.contains("{{") || rendered.contains("}}") {
        return Err(check_error(
            "source_metadata_unavailable",
            "The synthetic module template is incomplete",
        ));
    }
    if module_type == ModuleType::Common && !snippet {
        for property in
            ["ClientManagedApplication", "ExternalConnection", "ClientOrdinaryApplication"]
        {
            replace_xml_once(
                &mut rendered,
                &format!("<{property}>false</{property}>"),
                &format!("<{property}>true</{property}>"),
            )?;
        }
    }
    let object_path = folder_path.join(format!("{name}.{}", bsl_conventions::XML_EXTENSION));
    create_private_file(&object_path)?;
    write_private_existing(&object_path, rendered.as_bytes())?;

    if matches!(module_type, ModuleType::ManagedForm) {
        let form_root = folder_path.join(name);
        ensure_private_directory(&form_root)?;
        let form_dir = form_root.join(bsl_conventions::ConventionalName::Ext.canonical());
        ensure_private_directory(&form_dir)?;
        let body_path = form_dir.join(bsl_conventions::ConventionalName::FormXml.canonical());
        create_private_file(&body_path)?;
        let body = include_str!("fixtures/managed_form_body.xml");
        validate_fixture_template(body)?;
        write_private_existing(&body_path, body.as_bytes())?;
    }
    if matches!(module_type, ModuleType::Common) {
        // The metadata loader discovers common modules from their object directory plus
        // sibling XML; the empty module file makes this a real owner without source reuse.
        let module_root = folder_path.join(name);
        ensure_private_directory(&module_root)?;
        let ext_root = module_root.join(bsl_conventions::ConventionalName::Ext.canonical());
        ensure_private_directory(&ext_root)?;
        let module_path = ext_root.join(bsl_conventions::ConventionalName::Module.canonical());
        create_private_file(&module_path)?;
        write_private_existing(&module_path, b"")?;
    }

    let configuration_path =
        xml_root.join(bsl_conventions::ConventionalName::ConfigurationXml.canonical());
    let metadata = fs::symlink_metadata(&configuration_path).map_err(|_| {
        check_error("source_metadata_unavailable", "The configuration template is unavailable")
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 2 * 1024 * 1024
    {
        return Err(check_error(
            "source_metadata_unavailable",
            "The configuration template is unavailable",
        ));
    }
    let mut configuration = fs::read_to_string(&configuration_path).map_err(|_| {
        check_error("source_metadata_unavailable", "The configuration template is unavailable")
    })?;
    let entry = format!("<{object_kind}>{name}</{object_kind}>");
    if configuration.contains(&entry) {
        return Err(check_error(
            "source_metadata_unavailable",
            "The synthetic module owner already exists",
        ));
    }
    let closings: Vec<usize> =
        configuration.match_indices("</ChildObjects>").map(|(index, _)| index).collect();
    if closings.len() != 1 {
        return Err(check_error(
            "source_metadata_unavailable",
            "The configuration template is ambiguous",
        ));
    }
    configuration.insert_str(closings[0], &entry);
    write_private_existing(&configuration_path, configuration.as_bytes())?;
    Ok(())
}

fn validate_fixture_template(template: &str) -> Result<(), NativeCheckError> {
    if template.len() > 128 * 1024
        || !template.starts_with("<?xml")
        || !template.contains("version=\"2.20\"")
        || template.contains("<!DOCTYPE")
        || template.contains("<!ENTITY")
    {
        return Err(check_error(
            "source_metadata_unavailable",
            "The synthetic module template is unsupported",
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), NativeCheckError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(check_error(
            "workspace_unavailable",
            "The private metadata directory is unavailable",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| {
                check_error(
                    "workspace_unavailable",
                    "The private metadata directory is unavailable",
                )
            })?;
            set_private_dir(path)
        }
        Err(_) => Err(check_error(
            "workspace_unavailable",
            "The private metadata directory is unavailable",
        )),
    }
}

fn replace_xml_once(xml: &mut String, from: &str, to: &str) -> Result<(), NativeCheckError> {
    let mut matches = xml.match_indices(from);
    let (index, _) = matches.next().ok_or_else(|| {
        check_error("source_metadata_unavailable", "The synthetic module template is unsupported")
    })?;
    if matches.next().is_some() {
        return Err(check_error(
            "source_metadata_unavailable",
            "The synthetic module template is ambiguous",
        ));
    }
    xml.replace_range(index..index + from.len(), to);
    Ok(())
}

fn write_private_existing(path: &Path, bytes: &[u8]) -> Result<(), NativeCheckError> {
    use std::fs::OpenOptions;
    use std::io::Write;
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        check_error("workspace_unavailable", "The private configuration file is unavailable")
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(check_error(
            "workspace_unavailable",
            "The private configuration file is unavailable",
        ));
    }
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|_| {
        check_error("workspace_unavailable", "The private configuration file cannot be written")
    })?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|_| {
        check_error("workspace_unavailable", "The private configuration file cannot be written")
    })
}

fn write_native_module(path: &Path, code: &str) -> Result<(), NativeCheckError> {
    use std::fs::OpenOptions;
    use std::io::Write;
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        check_error("module_source_unavailable", "The private module source is unavailable")
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(check_error(
            "module_source_unavailable",
            "The private module source is unavailable",
        ));
    }
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|_| {
        check_error("module_source_unavailable", "The private module source cannot be written")
    })?;
    file.write_all(b"\xef\xbb\xbf")
        .and_then(|_| file.write_all(code.as_bytes()))
        .and_then(|_| file.sync_all())
        .map_err(|_| {
            check_error("module_source_unavailable", "The private module source cannot be written")
        })
}

fn has_success_marker(log: &[u8]) -> bool {
    const SUCCESS: &str = "No syntax errors found!";
    let Ok(text) = std::str::from_utf8(log) else { return false };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.lines().rfind(|line| !line.trim().is_empty()) == Some(SUCCESS)
}

fn parse_input_diagnostics(
    log: &[u8],
    exit_code: i32,
) -> Result<super::compiler::ParsedDiagnostics, NativeCheckError> {
    match super::compiler::parse_diagnostics(log) {
        Ok(parsed) => Ok(parsed),
        Err(_) if exit_code == 101 => Err(position_unavailable_error()),
        Err(_) => Err(check_error(
            "diagnostics_unavailable",
            "The native compiler output cannot be interpreted safely",
        )),
    }
}

fn position_unavailable_error() -> NativeCheckError {
    let mut error = check_error(
        "position_unavailable",
        "A native diagnostic position could not be mapped to the input",
    );
    error.compilation_status = Some(super::types::CompilationStatus::Invalid);
    error
}

fn native_success_verdict(
    log: &[u8],
    exit_code: i32,
    parsed: &super::compiler::ParsedDiagnostics,
) -> bool {
    exit_code == 0 && has_success_marker(log) && parsed.issues.is_empty() && !parsed.truncated
}

fn hash_file(path: &Path) -> Result<blake3::Hash, NativeCheckError> {
    use std::io::Read;
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        check_error("source_unavailable", "The source configuration snapshot is unavailable")
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > 256 * 1024 * 1024
    {
        return Err(check_error(
            "source_unavailable",
            "The source configuration snapshot is unavailable",
        ));
    }
    let mut file = fs::File::open(path).map_err(|_| {
        check_error("source_unavailable", "The source configuration snapshot is unavailable")
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|_| {
            check_error("source_unavailable", "The source configuration snapshot is unavailable")
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize())
}

fn synthetic_owner(module_type: super::types::ModuleType, name: &str) -> String {
    use super::types::ModuleType;
    let kind = match module_type {
        ModuleType::Object | ModuleType::Manager => "Справочник",
        ModuleType::ManagedForm | ModuleType::OrdinaryForm => "ОбщаяФорма",
        ModuleType::Common => "ОбщийМодуль",
    };
    format!("{kind}.{name}")
}

fn validate_module_owner(
    xml: &Path,
    owner: &str,
    module_type: super::types::ModuleType,
) -> Result<(), NativeCheckError> {
    use super::source::resolve_form_kind;
    use super::types::ModuleType;
    match module_type {
        ModuleType::ManagedForm | ModuleType::OrdinaryForm => {
            let kind = resolve_form_kind(xml, owner).map_err(|_| {
                check_error(
                    "source_metadata_unavailable",
                    "The source module metadata cannot be resolved",
                )
            })?;
            let expected = match module_type {
                ModuleType::ManagedForm => bsl_metadata::FormType::Managed,
                ModuleType::OrdinaryForm => bsl_metadata::FormType::Ordinary,
                _ => unreachable!(),
            };
            if kind != Some(expected) {
                return Err(check_error(
                    "module_context_unsupported",
                    "The module owner does not match the requested form type",
                ));
            }
        }
        ModuleType::Common => {
            let name = owner.rsplit('.').next().ok_or_else(|| {
                check_error(
                    "module_context_unsupported",
                    "The requested module owner is not supported",
                )
            })?;
            let config = bsl_metadata::load_from_directory(xml).map_err(|_| {
                check_error(
                    "source_metadata_unavailable",
                    "The source module metadata cannot be resolved",
                )
            })?;
            use bsl_metadata::traits::MdObject;
            if !config.common_modules().iter().any(|module| module.name() == name) {
                return Err(check_error(
                    "module_context_unsupported",
                    "The requested module owner does not exist",
                ));
            }
        }
        ModuleType::Object | ModuleType::Manager => {
            let (kind_name, name) = owner.split_once('.').ok_or_else(|| {
                check_error(
                    "module_context_unsupported",
                    "The requested module owner is not supported",
                )
            })?;
            let kind = kind_name.parse::<bsl_metadata::MdoType>().map_err(|_| {
                check_error(
                    "module_context_unsupported",
                    "The requested module owner is not supported",
                )
            })?;
            let config = bsl_metadata::load_from_directory(xml).map_err(|_| {
                check_error(
                    "source_metadata_unavailable",
                    "The source module metadata cannot be resolved",
                )
            })?;
            if !config.has_metadata_object(kind, name)
                || (module_type == ModuleType::Manager && kind.russian_plural().is_none())
            {
                return Err(check_error(
                    "module_context_unsupported",
                    "The requested module owner does not exist",
                ));
            }
        }
    }
    Ok(())
}

fn applicable_modes(
    configuration_xml_root: &Path,
    owner_xml_root: &Path,
    owner: &str,
    module_type: super::types::ModuleType,
) -> Result<Vec<super::commands::CheckModulesMode>, NativeCheckError> {
    use super::commands::CheckModulesMode as Mode;
    use super::types::ModuleType;
    let modes = match module_type {
        ModuleType::OrdinaryForm => vec![Mode::ThickClientOrdinaryApplication],
        ModuleType::ManagedForm => {
            let mut modes = vec![Mode::ThinClient, Mode::Server, Mode::WebClient];
            let managed_form_in_ordinary = super::source::parse_configuration_boolean(
                configuration_xml_root,
                "UseManagedFormInOrdinaryApplication",
            )
            .map_err(|_| {
                check_error(
                    "source_metadata_unavailable",
                    "The source form applicability cannot be verified",
                )
            })?;
            if managed_form_in_ordinary {
                modes.push(Mode::ThickClientOrdinaryApplication);
            }
            modes
        }
        ModuleType::Object | ModuleType::Manager => {
            vec![Mode::Server, Mode::ExternalConnection, Mode::ThickClientOrdinaryApplication]
        }
        ModuleType::Common => {
            let name = owner.rsplit('.').next().ok_or_else(|| {
                check_error(
                    "module_context_unsupported",
                    "The requested module owner is not supported",
                )
            })?;
            let configuration =
                bsl_metadata::load_from_directory(owner_xml_root).map_err(|_| {
                    check_error(
                        "source_metadata_unavailable",
                        "The source module metadata cannot be resolved",
                    )
                })?;
            use bsl_metadata::traits::MdObject;
            let module = configuration
                .common_modules()
                .iter()
                .find(|module| module.name() == name)
                .ok_or_else(|| {
                    check_error(
                        "module_context_unsupported",
                        "The requested common module does not exist",
                    )
                })?;
            let mut modes = Vec::new();
            if module.is_client_managed_application() {
                modes.push(Mode::ThinClient);
            }
            if module.is_server() {
                modes.push(Mode::Server);
            }
            if module.is_client_managed_application() {
                modes.push(Mode::WebClient);
            }
            if module.is_external_connection() {
                modes.push(Mode::ExternalConnection);
            }
            if module.is_client_ordinary_application() {
                modes.push(Mode::ThickClientOrdinaryApplication);
            }
            modes
        }
    };
    if modes.is_empty() {
        return Err(check_error(
            "module_context_unsupported",
            "The requested module is disabled in every native compilation context",
        ));
    }
    Ok(modes)
}

fn mode_label(mode: super::commands::CheckModulesMode) -> &'static str {
    use super::commands::CheckModulesMode as Mode;
    match mode {
        Mode::ThinClient => "thin_client",
        Mode::Server => "server",
        Mode::WebClient => "web_client",
        Mode::ExternalConnection => "external_connection",
        Mode::ThickClientOrdinaryApplication => "thick_client",
    }
}

fn check_modules_action<'a>(
    mode: super::commands::CheckModulesMode,
    extension: Option<&'a str>,
) -> super::commands::LocalAction<'a> {
    match extension {
        Some(name) => super::commands::LocalAction::CheckExtensionModules(mode, name),
        None => super::commands::LocalAction::CheckModules(mode),
    }
}

fn map_input_position(
    original: &str,
    line: u32,
    column: u32,
    kind: super::types::InputKind,
) -> (Option<u32>, Option<u32>) {
    let original = original.strip_prefix('\u{feff}').unwrap_or(original);
    let generated_prefix_lines = if kind == super::types::InputKind::Snippet { 1 } else { 0 };
    let Some(target_line) = line.checked_sub(generated_prefix_lines) else {
        return (None, None);
    };
    if target_line == 0 {
        return (None, None);
    }
    let lines: Vec<&str> = original.split('\n').collect();
    let target_index = target_line as usize - 1;
    if target_index == lines.len() {
        let Some(eof_line) = u32::try_from(lines.len()).ok() else { return (None, None) };
        let last_line = lines.last().copied().unwrap_or_default();
        let last_line = last_line.strip_suffix('\r').unwrap_or(last_line);
        let Some(eof_column) = last_line
            .encode_utf16()
            .count()
            .checked_add(1)
            .and_then(|column| u32::try_from(column).ok())
        else {
            return (None, None);
        };
        return (Some(eof_line), Some(eof_column));
    }
    if target_index > lines.len() {
        return (None, None);
    }
    let Some(raw_line) = lines.get(target_index).copied() else {
        return (None, None);
    };
    let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
    let native_chars = raw_line.encode_utf16().count();
    if column as usize > native_chars.saturating_add(1) {
        return (None, None);
    }
    (Some(target_line), Some(column))
}

fn finish_with_cleanup(
    mut result: super::types::CheckResult,
    workspace: JobWorkspace,
) -> super::types::CheckResult {
    match workspace.cleanup() {
        Ok(()) => result.cleanup_status = super::types::CleanupStatus::Complete,
        Err(_) => {
            result.cleanup_status = super::types::CleanupStatus::Pending;
            result.status = super::types::CheckStatus::Error;
            result.valid = None;
            result.failure = Some(super::types::CheckFailure {
                code: "cleanup_failed".to_owned(),
                message: "The private native-check workspace could not be removed".to_owned(),
            });
        }
    }
    result
}

fn validate_executable(path: &Path) -> Result<(), NativeProfileError> {
    if !path.is_absolute() {
        return Err(NativeProfileError::Invalid("executable path must be absolute"));
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| NativeProfileError::Invalid("configured executable is unavailable"))?;
    if canonical.as_os_str() != path.as_os_str() || !canonical.is_file() {
        return Err(NativeProfileError::Invalid(
            "configured executable must name the canonical file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&canonical)
            .map_err(|_| NativeProfileError::Invalid("configured executable is unavailable"))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(NativeProfileError::Invalid("configured executable is not executable"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeProfileError {
    Invalid(&'static str),
    CleanupFailed,
}

impl fmt::Display for NativeProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::CleanupFailed => f.write_str("stale native-check jobs could not be cleaned"),
        }
    }
}

impl std::error::Error for NativeProfileError {}

fn validate_env_name(name: &str) -> Result<(), NativeProfileError> {
    let mut chars = name.chars();
    let first =
        chars.next().ok_or(NativeProfileError::Invalid("environment variable name is empty"))?;
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return Err(NativeProfileError::Invalid("environment variable name is invalid"));
    }
    Ok(())
}

fn runtime_env(name: &str) -> Result<std::ffi::OsString, NativeProfileError> {
    let value = std::env::var_os(name)
        .ok_or(NativeProfileError::Invalid("a configured environment variable is unavailable"))?;
    if value.is_empty() || value.len() > 8192 {
        return Err(NativeProfileError::Invalid(
            "a configured environment variable has an invalid size",
        ));
    }
    Ok(value)
}

fn prepare_native_module(code: &str, input_kind: super::types::InputKind) -> String {
    let code = code.strip_prefix('\u{feff}').unwrap_or(code);
    match input_kind {
        super::types::InputKind::Module => code.to_owned(),
        super::types::InputKind::Snippet => {
            format!("Процедура BA023Snippet()\n{code}\nКонецПроцедуры")
        }
    }
}

fn validate_owned_work_root(path: &Path) -> Result<(), NativeProfileError> {
    if !path.is_absolute() {
        return Err(NativeProfileError::Invalid("work_root must be absolute"));
    }
    let canonical =
        path.canonicalize().map_err(|_| NativeProfileError::Invalid("work_root is unavailable"))?;
    if canonical != path || !canonical.is_dir() {
        return Err(NativeProfileError::Invalid("work_root must be the canonical directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::metadata(&canonical)
            .map_err(|_| NativeProfileError::Invalid("work_root is unavailable"))?;
        let mode = metadata.permissions().mode() & 0o777;
        if metadata.uid() != unsafe { libc::geteuid() } || mode != 0o700 {
            return Err(NativeProfileError::Invalid(
                "work_root must be owned by this user and have mode 0700",
            ));
        }
    }
    Ok(())
}

pub(crate) struct JobWorkspace {
    path: PathBuf,
    armed: bool,
}

impl JobWorkspace {
    pub(crate) fn create(work_root: &Path) -> Result<Self, NativeProfileError> {
        let jobs = work_root.join("owned-jobs");
        let canonical_jobs = jobs
            .canonicalize()
            .map_err(|_| NativeProfileError::Invalid("worker job root is unavailable"))?;
        if canonical_jobs != jobs {
            return Err(NativeProfileError::Invalid("worker job root must be canonical"));
        }
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| NativeProfileError::Invalid("system clock is invalid"))?
            .as_nanos();
        let id =
            format!("{}-{time}-{}", std::process::id(), NEXT_JOB.fetch_add(1, Ordering::Relaxed));
        let path = jobs.join(&id);
        fs::create_dir(&path)
            .map_err(|_| NativeProfileError::Invalid("worker job directory cannot be created"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|_| {
                NativeProfileError::Invalid("worker job directory cannot be secured")
            })?;
        }
        let marker = path.join(OWNER_MARKER);
        let mut file =
            OpenOptions::new().write(true).create_new(true).open(marker).map_err(|_| {
                NativeProfileError::Invalid("worker ownership manifest cannot be written")
            })?;
        file.write_all(format!("{OWNER_MARKER_PREFIX}{id}\n").as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| {
                NativeProfileError::Invalid("worker ownership manifest cannot be written")
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.join(OWNER_MARKER), fs::Permissions::from_mode(0o600))
                .map_err(|_| {
                    NativeProfileError::Invalid("worker ownership manifest cannot be secured")
                })?;
        }
        #[cfg(unix)]
        OpenOptions::new()
            .read(true)
            .open(&jobs)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                NativeProfileError::Invalid("worker ownership manifest cannot be synced")
            })?;
        Ok(Self { path, armed: true })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn cleanup(mut self) -> Result<(), NativeProfileError> {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {
                self.armed = false;
                Ok(())
            }
            Err(_) => {
                self.armed = false;
                Err(NativeProfileError::CleanupFailed)
            }
        }
    }
}

impl Drop for JobWorkspace {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn cleanup_owned_jobs(jobs: &Path) -> Result<(), NativeProfileError> {
    let entries = fs::read_dir(jobs).map_err(|_| NativeProfileError::CleanupFailed)?;
    for entry in entries {
        let entry = entry.map_err(|_| NativeProfileError::CleanupFailed)?;
        let file_type = entry.file_type().map_err(|_| NativeProfileError::CleanupFailed)?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.parent() != Some(jobs)
            || path.canonicalize().ok().as_deref() != Some(path.as_path())
        {
            continue;
        }
        let marker = path.join(OWNER_MARKER);
        let _marker_meta = match fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
            _ => continue,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if _marker_meta.uid() != unsafe { libc::geteuid() }
                || _marker_meta.permissions().mode() & 0o777 != 0o600
            {
                continue;
            }
            let dir_meta = fs::metadata(&path).map_err(|_| NativeProfileError::CleanupFailed)?;
            if dir_meta.uid() != unsafe { libc::geteuid() }
                || dir_meta.permissions().mode() & 0o777 != 0o700
            {
                continue;
            }
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !valid_job_id(id)
            || !fs::read_to_string(&marker)
                .is_ok_and(|content| content == format!("{OWNER_MARKER_PREFIX}{id}\n"))
        {
            continue;
        }
        if job_creator_may_be_live(id) {
            continue;
        }
        cleanup_owned_processes(&path)?;
        fs::remove_dir_all(path).map_err(|_| NativeProfileError::CleanupFailed)?;
    }
    Ok(())
}

fn job_creator_may_be_live(id: &str) -> bool {
    let Some(pid) =
        id.split('-').next().and_then(|value| value.parse::<u32>().ok()).filter(|pid| *pid > 1)
    else {
        return true;
    };
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return true };
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn cleanup_owned_processes(job: &Path) -> Result<(), NativeProfileError> {
    let mut directories = vec![job.to_owned()];
    for entry in fs::read_dir(job).map_err(|_| NativeProfileError::CleanupFailed)? {
        let entry = entry.map_err(|_| NativeProfileError::CleanupFailed)?;
        let metadata = entry.file_type().map_err(|_| NativeProfileError::CleanupFailed)?;
        if metadata.is_dir() && !metadata.is_symlink() {
            directories.push(entry.path());
        }
    }
    for directory in directories {
        let active = directory.join(".native-check-active-pgid");
        let metadata = match fs::symlink_metadata(&active) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
            Ok(_) => return Err(NativeProfileError::CleanupFailed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(NativeProfileError::CleanupFailed),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o777 != 0o600
                || metadata.len() > 64
            {
                return Err(NativeProfileError::CleanupFailed);
            }
            let data =
                fs::read_to_string(&active).map_err(|_| NativeProfileError::CleanupFailed)?;
            let record = data.strip_suffix('\n').ok_or(NativeProfileError::CleanupFailed)?;
            let (pid, start) = record.split_once(':').ok_or(NativeProfileError::CleanupFailed)?;
            let pid = pid
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid > 1)
                .ok_or(NativeProfileError::CleanupFailed)?;
            let start = start
                .parse::<u64>()
                .ok()
                .filter(|start| *start > 0)
                .ok_or(NativeProfileError::CleanupFailed)?;
            recover_process_group(pid, start, &directory)?;
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            return Err(NativeProfileError::CleanupFailed);
        }
        #[cfg(unix)]
        fs::remove_file(active).map_err(|_| NativeProfileError::CleanupFailed)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn recover_process_group(
    pid: u32,
    start_time: u64,
    owned_directory: &Path,
) -> Result<(), NativeProfileError> {
    let pgid = libc::pid_t::try_from(pid).map_err(|_| NativeProfileError::CleanupFailed)?;
    let stat_path = format!("/proc/{pid}/stat");
    match fs::read_to_string(&stat_path) {
        Ok(stat) => {
            let (group, actual_start) =
                parse_proc_identity(&stat).ok_or(NativeProfileError::CleanupFailed)?;
            if group != pid || actual_start != start_time {
                return Err(NativeProfileError::CleanupFailed);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if unsafe { libc::kill(-pgid, 0) } != 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                return Err(NativeProfileError::CleanupFailed);
            }
            return recover_leaderless_owned_group(pid, start_time, owned_directory);
        }
        Err(_) => return Err(NativeProfileError::CleanupFailed),
    }
    if unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 {
        return Err(NativeProfileError::CleanupFailed);
    }
    for _ in 0..100 {
        if unsafe { libc::kill(-pgid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(NativeProfileError::CleanupFailed)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn recover_process_group(
    _pid: u32,
    _start_time: u64,
    _owned_directory: &Path,
) -> Result<(), NativeProfileError> {
    Err(NativeProfileError::CleanupFailed)
}

#[cfg(target_os = "linux")]
fn recover_leaderless_owned_group(
    pgid: u32,
    leader_start_time: u64,
    owned_directory: &Path,
) -> Result<(), NativeProfileError> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let owned_directory =
        owned_directory.canonicalize().map_err(|_| NativeProfileError::CleanupFailed)?;
    for _ in 0..100 {
        if !process_group_exists(pgid) {
            return Ok(());
        }
        let (members, group_has_process) =
            leaderless_group_members(pgid, leader_start_time, &owned_directory)?;
        if members.is_empty() {
            if group_has_process {
                // Remaining entries are zombies: they cannot run or access the job directory.
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        let mut pinned = Vec::with_capacity(members.len());
        let mut retry = false;
        for (member_pid, member_start) in members {
            let member_pid =
                libc::pid_t::try_from(member_pid).map_err(|_| NativeProfileError::CleanupFailed)?;
            let raw_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, member_pid, 0) };
            if raw_fd < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    retry = true;
                    break;
                }
                return Err(NativeProfileError::CleanupFailed);
            }
            let pidfd = unsafe { OwnedFd::from_raw_fd(raw_fd as i32) };
            if !leaderless_member_is_owned(member_pid as u32, pgid, member_start, &owned_directory)?
            {
                retry = true;
                break;
            }
            pinned.push((member_pid, pidfd));
        }
        if retry {
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        for (_, pidfd) in pinned {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(NativeProfileError::CleanupFailed);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(NativeProfileError::CleanupFailed)
}

#[cfg(target_os = "linux")]
fn leaderless_group_members(
    pgid: u32,
    leader_start_time: u64,
    owned_directory: &Path,
) -> Result<(Vec<(u32, u64)>, bool), NativeProfileError> {
    use std::os::unix::fs::MetadataExt;

    let entries = fs::read_dir("/proc").map_err(|_| NativeProfileError::CleanupFailed)?;
    let mut members = Vec::new();
    let mut group_has_process = false;
    for entry in entries {
        let entry = entry.map_err(|_| NativeProfileError::CleanupFailed)?;
        let Some(member_pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat_path = entry.path().join("stat");
        let stat = match fs::read_to_string(&stat_path) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(NativeProfileError::CleanupFailed),
        };
        let Some((state, member_group, member_start)) = parse_proc_state_identity(&stat) else {
            return Err(NativeProfileError::CleanupFailed);
        };
        if member_group != pgid {
            continue;
        }
        group_has_process = true;
        if member_start < leader_start_time {
            return Err(NativeProfileError::CleanupFailed);
        }
        if matches!(state, b'Z' | b'X') {
            continue;
        }
        let metadata = fs::metadata(entry.path()).map_err(|_| NativeProfileError::CleanupFailed)?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(NativeProfileError::CleanupFailed);
        }
        let cwd = fs::canonicalize(entry.path().join("cwd"))
            .map_err(|_| NativeProfileError::CleanupFailed)?;
        if !cwd.starts_with(owned_directory) {
            return Err(NativeProfileError::CleanupFailed);
        }
        members.push((member_pid, member_start));
    }
    Ok((members, group_has_process))
}

#[cfg(target_os = "linux")]
fn leaderless_member_is_owned(
    pid: u32,
    pgid: u32,
    start_time: u64,
    owned_directory: &Path,
) -> Result<bool, NativeProfileError> {
    use std::os::unix::fs::MetadataExt;

    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(NativeProfileError::CleanupFailed),
    };
    if parse_proc_identity(&stat) != Some((pgid, start_time)) {
        return Ok(false);
    }
    let metadata = match fs::metadata(format!("/proc/{pid}")) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(NativeProfileError::CleanupFailed),
    };
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(NativeProfileError::CleanupFailed);
    }
    let cwd = match fs::canonicalize(format!("/proc/{pid}/cwd")) {
        Ok(cwd) => cwd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(NativeProfileError::CleanupFailed),
    };
    if !cwd.starts_with(owned_directory) {
        return Err(NativeProfileError::CleanupFailed);
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
fn process_group_exists(pgid: u32) -> bool {
    if unsafe { libc::kill(-(pgid as i32), 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(target_os = "linux")]
fn parse_proc_identity(stat: &str) -> Option<(u32, u64)> {
    parse_proc_state_identity(stat).map(|(_, group, start)| (group, start))
}

#[cfg(target_os = "linux")]
fn parse_proc_state_identity(stat: &str) -> Option<(u8, u32, u64)> {
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    Some((
        fields.first()?.as_bytes().first().copied()?,
        fields.get(2)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
    ))
}

fn default_max_input_bytes() -> usize {
    DEFAULT_MAX_INPUT_BYTES
}

fn default_source_kind() -> SourceKind {
    SourceKind::Server
}

fn default_deadline_ms() -> u64 {
    DEFAULT_DEADLINE_MS
}

fn default_max_log_bytes() -> usize {
    DEFAULT_MAX_LOG_BYTES
}

fn valid_job_id(id: &str) -> bool {
    let mut parts = id.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(pid), Some(time), Some(sequence), None)
            if pid.bytes().all(|byte| byte.is_ascii_digit())
                && time.bytes().all(|byte| byte.is_ascii_digit())
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_profile_conditions_are_separate_from_operational_errors() {
        for (code, message) in [
            ("compiler_build_mismatch", "The compiler build does not match"),
            ("runtime_build_mismatch", "The runtime build does not match"),
            ("unsupported_extension_scope", "The extension scope is unsupported"),
        ] {
            let error = check_error(code, message);
            let mut result = crate::native_check::types::CheckResult::new(error.status);
            apply_native_check_error(&mut result, error);
            assert_eq!(result.status, crate::native_check::types::CheckStatus::Unsupported);
            assert_eq!(result.valid, None);
            assert_eq!(result.compilation_status, None);
            assert_eq!(result.failure.as_ref().unwrap().code, code);
        }

        for code in ["cancelled", "deadline_exceeded"] {
            let error = check_error(code, "Native module check did not complete");
            let mut result = crate::native_check::types::CheckResult::new(error.status);
            apply_native_check_error(&mut result, error);
            assert_eq!(result.status, crate::native_check::types::CheckStatus::Error);
            assert_eq!(result.failure.as_ref().unwrap().code, code);
        }
    }

    #[test]
    fn position_unavailable_keeps_native_result_when_cleanup_fails() {
        let mut error = check_error(
            "position_unavailable",
            "A native diagnostic position could not be mapped to the input",
        );
        error.compilation_status = Some(crate::native_check::types::CompilationStatus::Invalid);
        let mut result = crate::native_check::types::CheckResult::new(error.status);
        apply_native_check_error(&mut result, error);
        assert_eq!(result.status, crate::native_check::types::CheckStatus::Error);
        assert_eq!(result.valid, None);
        assert_eq!(
            result.compilation_status,
            Some(crate::native_check::types::CompilationStatus::Invalid)
        );
        assert_eq!(result.failure.as_ref().unwrap().code, "position_unavailable");

        let root = tempfile::tempdir().unwrap();
        let workspace =
            JobWorkspace { path: root.path().join("already-removed-owned-job"), armed: true };
        let result = finish_with_cleanup(result, workspace);
        assert_eq!(result.status, crate::native_check::types::CheckStatus::Error);
        assert_eq!(result.valid, None);
        assert_eq!(result.cleanup_status, crate::native_check::types::CleanupStatus::Pending);
        assert_eq!(result.failure.as_ref().unwrap().code, "cleanup_failed");
        assert_eq!(
            result.compilation_status,
            Some(crate::native_check::types::CompilationStatus::Invalid)
        );
    }

    #[test]
    fn malformed_input_diagnostic_on_native_invalid_has_no_fabricated_issue() {
        let log = b"{CommonModule.Test.Module(2)}: invalid position";
        let error = parse_input_diagnostics(log, 101).unwrap_err();
        assert_eq!(error.code, "position_unavailable");
        assert_eq!(error.status, crate::native_check::types::CheckStatus::Error);
        assert_eq!(
            error.compilation_status,
            Some(crate::native_check::types::CompilationStatus::Invalid)
        );

        let mut result = crate::native_check::types::CheckResult::new(error.status);
        apply_native_check_error(&mut result, error);
        assert!(result.issues.is_empty());
        assert_eq!(result.valid, None);
        assert_eq!(
            result.compilation_status,
            Some(crate::native_check::types::CompilationStatus::Invalid)
        );
        assert_eq!(parse_input_diagnostics(log, 0).unwrap_err().code, "diagnostics_unavailable");
        assert!(parse_input_diagnostics(b"", 101).unwrap().issues.is_empty());
    }

    #[test]
    fn leading_bom_is_removed_before_wrapping_modules_and_snippets() {
        let code = "\u{feff}А = 1;";
        assert_eq!(
            prepare_native_module(code, crate::native_check::types::InputKind::Module),
            "А = 1;"
        );
        assert_eq!(
            prepare_native_module(code, crate::native_check::types::InputKind::Snippet),
            "Процедура BA023Snippet()\nА = 1;\nКонецПроцедуры"
        );
    }

    #[test]
    fn snippet_suffix_positions_map_to_source_eof_with_or_without_trailing_newline() {
        use crate::native_check::types::InputKind;

        let code = "Если Истина Тогда\n    А = 1;";
        assert_eq!(map_input_position(code, 4, 1, InputKind::Snippet), (Some(2), Some(11)));
        assert_eq!(map_input_position("А = 1;\n", 4, 1, InputKind::Snippet), (Some(2), Some(1)));
        assert_eq!(map_input_position("А = 1;\n", 3, 1, InputKind::Module), (Some(2), Some(1)));
        assert_eq!(
            map_input_position("А = 1;\n\u{feff}Б = 2;", 4, 1, InputKind::Snippet),
            (Some(2), Some(8))
        );
        assert_eq!(map_input_position("А = 1;", 9, 1, InputKind::Module), (None, None));
    }

    #[test]
    fn native_module_writer_preserves_second_and_later_line_boms() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("module.bsl");
        create_private_file(&path).unwrap();
        let double_bom = prepare_native_module(
            "\u{feff}\u{feff}А = 1;",
            crate::native_check::types::InputKind::Module,
        );
        write_native_module(&path, &double_bom).unwrap();
        assert_eq!(fs::read(&path).unwrap(), "\u{feff}\u{feff}А = 1;".as_bytes());

        let later_line_bom = prepare_native_module(
            "А = 1;\n\u{feff}Б = 2;",
            crate::native_check::types::InputKind::Snippet,
        );
        write_native_module(&path, &later_line_bom).unwrap();
        assert!(fs::read(&path).unwrap().windows(4).any(|window| window == b"\n\xef\xbb\xbf"));
    }

    #[test]
    fn debug_does_not_expose_local_paths_or_environment_names() {
        let profile = NativeProfile {
            designer_path: "/private/platform/1cv8".into(),
            python_path: "/private/platform/python3".into(),
            xvfb_run_path: Some("/private/platform/xvfb-run".into()),
            expected_build: "private-build".into(),
            work_root: "/private/work".into(),
            source_kind: SourceKind::File,
            source_connection_env: "PRIVATE_CONNECTION_VAR".into(),
            user_env: Some("PRIVATE_USER_VAR".into()),
            password_env: Some("PRIVATE_PASSWORD_VAR".into()),
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            deadline_ms: DEFAULT_DEADLINE_MS,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
        };
        let debug = format!("{profile:?}");
        for secret in [
            "/private/platform/1cv8",
            "/private/platform/xvfb-run",
            "private-build",
            "/private/work",
            "PRIVATE_CONNECTION_VAR",
            "PRIVATE_USER_VAR",
            "PRIVATE_PASSWORD_VAR",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn environment_references_are_names_not_arbitrary_strings() {
        assert!(validate_env_name("BSL_NATIVE_USER_1").is_ok());
        assert!(validate_env_name("").is_err());
        assert!(validate_env_name("HOME=value").is_err());
        assert!(validate_env_name("1BSL_USER").is_err());
    }

    #[test]
    fn profile_limits_are_finite() {
        assert_eq!(MAX_INPUT_BYTES, 8 * 1024 * 1024);
        assert_eq!(MAX_DEADLINE_MS, 15 * 60 * 1000);
        assert_eq!(MAX_LOG_BYTES, 1024 * 1024);
    }

    #[test]
    fn file_extension_probe_normalizes_known_legacy_bom_crlf_only() {
        assert_eq!(
            classify_file_extension_list(0, b"").unwrap(),
            FileExtensionProbe::Names(vec![])
        );
        assert_eq!(
            classify_file_extension_list(0, b"\xef\xbb\xbfBA023Extension\n").unwrap(),
            FileExtensionProbe::Names(vec!["BA023Extension".to_owned()])
        );
        assert!(matches!(
            classify_file_extension_list(
                1,
                b"The database structure does not support extensions. Turn the compatibility mode off.\n"
            ),
            Ok(FileExtensionProbe::LegacyNoExtensionStorage)
        ));
        assert_eq!(
            classify_file_extension_list(
                1,
                b"\xef\xbb\xbfThe database structure does not support extensions. Turn the compatibility mode off.\r\n"
            )
            .unwrap(),
            FileExtensionProbe::LegacyNoExtensionStorage
        );
        assert!(classify_file_extension_list(1, b"different failure\n").is_err());
        assert!(classify_file_extension_list(
            1,
            b"\xef\xbb\xbfThe database structure does not support extensions. Turn the compatibility mode off.\r\nextra\r\n"
        )
        .is_err());
        assert!(classify_file_extension_list(255, b"").is_err());
    }

    #[test]
    fn ibcmd_extension_parser_preserves_empty_values_and_rejects_incomplete_records() {
        let output = concat!(
            "name : \"BA023Extension\"\n",
            "version : \n",
            "active : yes\n",
            "purpose : customization\n",
            "safe-mode : no\n",
            "security-profile-name : \n",
            "unsafe-action-protection : yes\n",
            "used-in-distributed-infobase : no\n",
            "scope : infobase\n",
            "hash-sum : \"z6QvL0nf9r3MB1FqrgT9XPjuurw=\"\n\n\n",
        );
        let parsed = parse_ibcmd_extension_list(output.as_bytes()).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "BA023Extension");
        assert_eq!(parsed[0].version, "");
        assert_eq!(parsed[0].security_profile, "");
        assert!(parsed[0].active);
        assert!(!parsed[0].safe_mode);
        assert!(parse_ibcmd_extension_list(b"name : \"Incomplete\"\nactive : yes\n").is_err());
        assert!(parse_ibcmd_extension_list(b"name : \"A\"\nname : \"B\"\n").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ibcmd_update_info_stdout_is_followed_by_strict_copy_state_verification() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        use tokio_util::sync::CancellationToken;

        let root = tempfile::tempdir().unwrap();
        let designer = root.path().join("1cv8");
        let ibcmd = root.path().join("ibcmd");
        fs::write(&designer, b"stub").unwrap();
        fs::write(
            &ibcmd,
            concat!(
                "#!/bin/sh\n",
                "case \"$1:$2\" in\n",
                "  extension:update) printf '%s\\n' \"[INFO] Set the 'BA023Extension' extension properties\" \"[INFO] The 'BA023Extension' extension properties are set\" ;;\n",
                "  extension:list) cat <<'EOF'\n",
                "name : \"BA023Extension\"\n",
                "version : \n",
                "active : yes\n",
                "purpose : customization\n",
                "safe-mode : yes\n",
                "security-profile-name : \n",
                "unsafe-action-protection : yes\n",
                "used-in-distributed-infobase : no\n",
                "scope : infobase\n",
                "hash-sum : \"z6QvL0nf9r3MB1FqrgT9XPjuurw=\"\n\n",
                "EOF\n",
                "  ;;\n",
                "  *) exit 2 ;;\n",
                "esac\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&designer, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&ibcmd, fs::Permissions::from_mode(0o755)).unwrap();

        let profile = NativeProfile {
            designer_path: designer,
            python_path: root.path().join("python3"),
            xvfb_run_path: None,
            expected_build: "8.3.27.1989".to_owned(),
            work_root: root.path().to_owned(),
            source_kind: SourceKind::File,
            source_connection_env: "UNUSED_TEST_SOURCE".to_owned(),
            user_env: None,
            password_env: None,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            deadline_ms: DEFAULT_DEADLINE_MS,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
        };
        let workspace = root.path().join("job");
        let data_dir = workspace.join("ibcmd-data");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&data_dir).unwrap();
        set_private_dir(&workspace).unwrap();
        set_private_dir(&data_dir).unwrap();
        let source_properties = FileExtensionProperties {
            name: "BA023Extension".to_owned(),
            version: String::new(),
            active: true,
            purpose: "customization".to_owned(),
            safe_mode: true,
            security_profile: String::new(),
            unsafe_action_protection: true,
            used_in_distributed_infobase: false,
            scope: "infobase".to_owned(),
            hash_sum: "z6QvL0nf9r3MB1FqrgT9XPjuurw=".to_owned(),
        };
        let expected = [EffectiveExtension {
            name: source_properties.name.clone(),
            safe_mode: source_properties.safe_mode,
            scope: source_properties.scope.clone(),
            cfe_hash: None,
            source_properties_hash: file_extension_properties_hash(&source_properties),
            file_properties: Some(source_properties),
            cfe_path: None,
        }];
        let deadline = Instant::now() + Duration::from_secs(5);
        let cancel = CancellationToken::new();

        update_extension_properties(
            &profile,
            &workspace.join("ib"),
            &data_dir,
            &workspace,
            &expected[0],
            deadline,
            &cancel,
        )
        .await
        .unwrap();
        let mut mismatched_expected = expected.clone();
        mismatched_expected[0].file_properties.as_mut().unwrap().purpose =
            "different-purpose".to_owned();
        let mismatch = verify_copy_extensions(
            &profile,
            &workspace.join("ib"),
            &data_dir,
            &workspace,
            &mismatched_expected,
            deadline,
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(mismatch.code, "extension_properties_unavailable");
        verify_copy_extensions(
            &profile,
            &workspace.join("ib"),
            &data_dir,
            &workspace,
            &expected,
            deadline,
            &cancel,
        )
        .await
        .unwrap();
    }

    #[test]
    fn native_success_verdict_rejects_diagnostics_even_with_completion_marker() {
        let log = b"\xef\xbb\xbf{CommonModule.BA023_Extra.Module(2,9)}: variable is not defined\r\nNo syntax errors found!\r\n";
        let parsed = crate::native_check::compiler::parse_diagnostics(log).unwrap();
        assert_eq!(parsed.issues.len(), 1);
        assert!(!native_success_verdict(log, 0, &parsed));
        assert!(native_success_verdict(
            b"\xef\xbb\xbfNo syntax errors found!\r\n",
            0,
            &crate::native_check::compiler::ParsedDiagnostics { issues: vec![], truncated: false }
        ));
        assert!(!native_success_verdict(
            b"\xef\xbb\xbf{CommonModule.BA023_Extra.Module(2,9)}: warning\r\nNo syntax errors found!\r\ntrailing text\r\n",
            0,
            &parsed
        ));
        assert!(!native_success_verdict(log, 101, &parsed));
        assert!(!native_success_verdict(
            log,
            0,
            &crate::native_check::compiler::ParsedDiagnostics { issues: vec![], truncated: true }
        ));
    }

    #[test]
    fn check_action_uses_base_scope_or_the_named_extension_scope() {
        use crate::native_check::commands::CheckModulesMode::Server;

        fn arguments(action: super::super::commands::LocalAction<'_>) -> Vec<String> {
            super::super::commands::local_action_args(Path::new("/private/job/ib"), action)
                .unwrap()
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        }
        assert_eq!(
            arguments(check_modules_action(Server, None)),
            [
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/CheckModules",
                "-Server",
            ]
        );
        assert_eq!(
            arguments(check_modules_action(Server, Some("BA023Extension"))),
            [
                "DESIGNER",
                "/F",
                "/private/job/ib",
                "/DisableStartupMessages",
                "/DisableStartupDialogs",
                "/CheckModules",
                "-Server",
                "-Extension",
                "BA023Extension",
            ]
        );
    }

    #[test]
    fn synthetic_common_module_is_discoverable_from_its_owned_source_tree() {
        let root = tempfile::tempdir().unwrap();
        let common_modules = root.path().join("CommonModules");
        fs::create_dir(&common_modules).unwrap();
        fs::write(
            root.path().join("Configuration.xml"),
            include_str!("../../../bsl-metadata/fixtures/designer/Configuration.xml"),
        )
        .unwrap();
        let name = "BslCheckSynthetic";

        add_synthetic_owner(
            root.path(),
            crate::native_check::types::ModuleType::Common,
            name,
            true,
        )
        .unwrap();

        let module_path = common_modules.join(name).join("Ext/Module.bsl");
        assert!(module_path.is_file());
        let config = bsl_metadata::load_from_directory(root.path()).unwrap();
        use bsl_metadata::Module;
        let module = config.find_common_module(name).unwrap();
        assert_eq!(module.uri(), Some("CommonModules/BslCheckSynthetic/Ext/Module.bsl"));
        assert!(module.is_server());
    }

    #[test]
    fn extension_managed_form_applicability_uses_base_configuration_property() {
        use crate::native_check::commands::CheckModulesMode as Mode;
        use crate::native_check::types::ModuleType;

        let base = tempfile::tempdir().unwrap();
        let extension = tempfile::tempdir().unwrap();
        fs::write(
            base.path().join("Configuration.xml"),
            "<Configuration><UseManagedFormInOrdinaryApplication>true</UseManagedFormInOrdinaryApplication></Configuration>",
        )
        .unwrap();
        fs::write(extension.path().join("Configuration.xml"), "<Configuration />").unwrap();

        let modes = applicable_modes(
            base.path(),
            extension.path(),
            "CommonForm.ExtensionManaged",
            ModuleType::ManagedForm,
        )
        .unwrap();
        assert_eq!(
            modes,
            [Mode::ThinClient, Mode::Server, Mode::WebClient, Mode::ThickClientOrdinaryApplication]
        );
    }

    #[test]
    fn extension_name_reconciliation_uses_source_order_and_exact_membership() {
        let a = FileExtensionProperties {
            name: "A".to_owned(),
            version: String::new(),
            active: true,
            purpose: "customization".to_owned(),
            safe_mode: true,
            security_profile: String::new(),
            unsafe_action_protection: true,
            used_in_distributed_infobase: false,
            scope: "infobase".to_owned(),
            hash_sum: "hash-a".to_owned(),
        };
        let b = FileExtensionProperties { name: "B".to_owned(), ..a.clone() };
        let reconciled = reconcile_file_extension_names(
            &["B".to_owned(), "A".to_owned()],
            vec![a.clone(), b.clone()],
        )
        .unwrap();
        assert_eq!(
            reconciled.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            ["B", "A"]
        );
        assert!(reconcile_file_extension_names(&["A".to_owned()], vec![a, b]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn startup_cleanup_removes_only_owned_directories_and_never_follows_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("owned-jobs");
        fs::create_dir(&jobs).unwrap();
        fs::set_permissions(&jobs, fs::Permissions::from_mode(0o700)).unwrap();

        let mut dead_creator = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let creator_pid = dead_creator.id();
        dead_creator.kill().unwrap();
        dead_creator.wait().unwrap();
        let owned_id = format!("{creator_pid}-456-0");
        let owned = jobs.join(&owned_id);
        fs::create_dir(&owned).unwrap();
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(owned.join(OWNER_MARKER), format!("{OWNER_MARKER_PREFIX}{owned_id}\n")).unwrap();
        fs::set_permissions(owned.join(OWNER_MARKER), fs::Permissions::from_mode(0o600)).unwrap();

        let foreign = jobs.join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("keep"), b"foreign").unwrap();
        let link = jobs.join("foreign-link");
        symlink(&foreign, &link).unwrap();

        cleanup_owned_jobs(&jobs).unwrap();

        assert!(!owned.exists());
        assert!(foreign.join("keep").exists());
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn startup_cleanup_skips_job_whose_creator_process_is_still_live() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("owned-jobs");
        fs::create_dir(&jobs).unwrap();
        fs::set_permissions(&jobs, fs::Permissions::from_mode(0o700)).unwrap();
        let mut creator = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let id = format!("{}-123-0", creator.id());
        let owned = jobs.join(&id);
        fs::create_dir(&owned).unwrap();
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = owned.join(OWNER_MARKER);
        fs::write(&marker, format!("{OWNER_MARKER_PREFIX}{id}\n")).unwrap();
        fs::set_permissions(marker, fs::Permissions::from_mode(0o600)).unwrap();

        cleanup_owned_jobs(&jobs).unwrap();
        let preserved = owned.is_dir();
        let _ = creator.kill();
        let _ = creator.wait();
        assert!(preserved, "startup cleanup removed a live foreign worker's job");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_refuses_to_kill_a_live_group_without_its_leader_identity() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("group-ready");
        let child_pid_path = directory.path().join("descendant-pid");
        let body = format!(
            "/bin/sleep 30 & echo $! > '{}'; echo ready > '{}'; wait",
            child_pid_path.display(),
            ready.display()
        );
        let mut command = Command::new("/bin/sh");
        command.process_group(0).arg("-c").arg(body);
        let mut leader = command.spawn().unwrap();
        let pid = leader.id();
        for _ in 0..200 {
            if ready.is_file() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let ready_seen = ready.is_file();
        let start_time = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| parse_proc_identity(&stat).map(|(_, start)| start));
        let _ = leader.kill();
        let _ = leader.wait();
        let descendant_pid = fs::read_to_string(&child_pid_path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok());
        let recovery = start_time
            .map(|start| recover_process_group(pid, start, directory.path()))
            .unwrap_or(Err(NativeProfileError::CleanupFailed));
        let descendant_survived =
            descendant_pid.is_some_and(|child| Path::new(&format!("/proc/{child}")).exists());

        let owned_group_still_exists = descendant_pid.is_some_and(|child| {
            fs::read_to_string(format!("/proc/{child}/stat"))
                .ok()
                .and_then(|stat| parse_proc_identity(&stat))
                .is_some_and(|(group, _)| group == pid)
        });
        if owned_group_still_exists {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        for _ in 0..100 {
            if unsafe { libc::kill(-(pid as i32), 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready_seen);
        assert_eq!(recovery, Err(NativeProfileError::CleanupFailed));
        assert!(descendant_survived, "recovery killed an unverified surviving process group");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_cleanup_recovers_a_leaderless_group_owned_by_the_private_attempt() {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("owned-jobs");
        fs::create_dir(&jobs).unwrap();
        fs::set_permissions(&jobs, fs::Permissions::from_mode(0o700)).unwrap();
        let mut dead_creator = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let job_id = format!("{}-456-0", dead_creator.id());
        dead_creator.kill().unwrap();
        dead_creator.wait().unwrap();

        let job = jobs.join(&job_id);
        fs::create_dir(&job).unwrap();
        fs::set_permissions(&job, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = job.join(OWNER_MARKER);
        fs::write(&marker, format!("{OWNER_MARKER_PREFIX}{job_id}\n")).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        let attempt = job.join("attempt-0");
        fs::create_dir(&attempt).unwrap();
        fs::set_permissions(&attempt, fs::Permissions::from_mode(0o700)).unwrap();

        let ready = attempt.join("group-ready");
        let child_pid_path = attempt.join("descendant-pid");
        let body = format!(
            "/bin/sleep 30 & echo $! > '{}'; echo ready > '{}'; wait",
            child_pid_path.display(),
            ready.display()
        );
        let mut command = Command::new("/bin/sh");
        command.current_dir(&attempt).process_group(0).arg("-c").arg(body);
        let mut leader = command.spawn().unwrap();
        let pgid = leader.id();
        for _ in 0..200 {
            if ready.is_file() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready.is_file());
        let start_time = fs::read_to_string(format!("/proc/{pgid}/stat"))
            .ok()
            .and_then(|stat| parse_proc_identity(&stat).map(|(_, start)| start))
            .unwrap();
        let descendant_pid =
            fs::read_to_string(&child_pid_path).unwrap().trim().parse::<u32>().unwrap();
        let descendant_start = fs::read_to_string(format!("/proc/{descendant_pid}/stat"))
            .ok()
            .and_then(|stat| parse_proc_identity(&stat).map(|(_, start)| start))
            .unwrap();
        assert!(descendant_start >= start_time);

        let active = attempt.join(".native-check-active-pgid");
        let mut manifest =
            fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(active).unwrap();
        writeln!(manifest, "{pgid}:{start_time}").unwrap();
        drop(manifest);
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert_eq!(
            leaderless_group_members(pgid, descendant_start, &attempt).unwrap().0,
            [(descendant_pid, descendant_start)]
        );

        cleanup_owned_jobs(&jobs).unwrap();

        assert!(!job.exists(), "startup cleanup retained a proven owned orphan job");
        let still_running = fs::read_to_string(format!("/proc/{descendant_pid}/stat"))
            .ok()
            .and_then(|stat| parse_proc_state_identity(&stat))
            .is_some_and(|(state, group, start)| {
                group == pgid && start == descendant_start && !matches!(state, b'Z' | b'X')
            });
        assert!(!still_running, "startup cleanup left the owned orphan running");
    }
}
