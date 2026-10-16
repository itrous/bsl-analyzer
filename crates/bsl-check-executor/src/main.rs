use clap::Parser;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const MAX_REQUEST_FRAME: usize = 6 * 8 * 1024 * 1024 + 64 * 1024;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    profile: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    profiles: BTreeMap<String, bsl_check_executor::NativeProfile>,
}

#[tokio::main]
async fn main() {
    let result = std::panic::catch_unwind(Args::parse);
    let args = match result {
        Ok(args) => args,
        Err(_) => std::process::exit(2),
    };
    let cancel = CancellationToken::new();
    let Some(stop_handler) = install_stop_handler(cancel.clone()) else {
        std::process::exit(2);
    };
    let code = match run(args, cancel).await {
        Ok(()) => 0,
        Err(code) => code,
    };
    drop(stop_handler);
    if code != 0 {
        std::process::exit(code);
    }
}

async fn run(args: Args, cancel: CancellationToken) -> Result<(), i32> {
    let input = read_request_frame(tokio::io::stdin(), MAX_REQUEST_FRAME).await?;
    let request = check_contract::parse_request(&input, MAX_REQUEST_FRAME).map_err(|_| 2)?;
    if request.connection != args.profile {
        write_response(error_response(
            &request,
            "profile_connection_mismatch",
            "The request connection does not match the selected profile",
        ))
        .await?;
        return Ok(());
    }
    let bytes = tokio::fs::read(&args.config).await.map_err(|_| 2)?;
    let registry: Registry = serde_json::from_slice(&bytes).map_err(|_| 2)?;
    let Some(profile) = registry.profiles.get(&args.profile) else {
        write_response(error_response(
            &request,
            "profile_not_found",
            "The selected profile is not registered",
        ))
        .await?;
        return Ok(());
    };
    if !supported_limits(&request.limits) {
        write_response(error_response(
            &request,
            "unsupported_limits",
            "The requested native bounds are unsupported",
        ))
        .await?;
        return Ok(());
    }
    if let Err(error) = bsl_check_executor::native_startup(profile) {
        write_response(error_response(&request, "profile_invalid", &error.to_string())).await?;
        return Ok(());
    }
    let client = match make_client(profile) {
        Ok(client) => client,
        Err(message) => {
            write_response(error_response(&request, "runtime_unavailable", message)).await?;
            return Ok(());
        }
    };
    let response = bsl_check_executor::check_wire(profile, request, &client, cancel).await;
    write_response(response).await
}

async fn read_request_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    maximum_bytes: usize,
) -> Result<Vec<u8>, i32> {
    let mut input = Vec::with_capacity(16 * 1024);
    reader
        .take(maximum_bytes.saturating_add(1) as u64)
        .read_to_end(&mut input)
        .await
        .map_err(|_| 2)?;
    if input.len() > maximum_bytes {
        return Err(2);
    }
    Ok(input)
}

fn supported_limits(limits: &check_contract::WireLimits) -> bool {
    limits.max_code_bytes <= 8 * 1024 * 1024
        && limits.timeout_ms <= 900_000
        && (500..=900_000).contains(&limits.stop_grace_ms)
        && (16 * 1024..=1024 * 1024).contains(&limits.max_response_bytes)
        && (4096..=1024 * 1024).contains(&limits.max_stderr_bytes)
}

fn make_client(
    profile: &bsl_check_executor::NativeProfile,
) -> Result<onec_client::Client, &'static str> {
    if matches!(profile.source_kind, bsl_check_executor::SourceKind::Server) {
        let name = profile
            .runtime_url_env
            .as_deref()
            .ok_or("runtime URL environment is not configured")?;
        let url = std::env::var(name).map_err(|_| "runtime URL is unavailable")?;
        let user = profile
            .user_env
            .as_deref()
            .map(std::env::var)
            .transpose()
            .map_err(|_| "runtime credentials are unavailable")?
            .unwrap_or_default();
        let password = profile
            .password_env
            .as_deref()
            .map(std::env::var)
            .transpose()
            .map_err(|_| "runtime credentials are unavailable")?
            .unwrap_or_default();
        Ok(onec_client::Client::new(&url, &user, &password))
    } else {
        Ok(onec_client::Client::new("http://127.0.0.1/", "", ""))
    }
}

async fn write_response(response: check_contract::CheckResponse) -> Result<(), i32> {
    let bytes = serde_json::to_vec(&response).map_err(|_| 2)?;
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&bytes).await.map_err(|_| 2)?;
    stdout.write_all(b"\n").await.map_err(|_| 2)?;
    stdout.flush().await.map_err(|_| 2)
}

fn error_response(
    request: &check_contract::CheckRequest,
    code: &str,
    message: &str,
) -> check_contract::CheckResponse {
    check_contract::CheckResponse {
        schema_version: check_contract::SCHEMA_VERSION.to_owned(),
        request_id: request.request_id.clone(),
        connection: request.connection.clone(),
        backend: "bsl-native-check/0.2.85".to_owned(),
        status: check_contract::Status::Error,
        valid: None,
        issues: Vec::new(),
        contexts_required: Vec::new(),
        contexts_checked: Vec::new(),
        truncated: false,
        failure: Some(check_contract::Failure {
            code: code.to_owned(),
            message: message.to_owned(),
        }),
        compilation_status: None,
        compiler: None,
        cleanup_status: None,
        module_type: request.module_type,
        context: request.context.clone(),
        backend_info: None,
    }
}

struct StopHandler(tokio::task::JoinHandle<()>);

impl Drop for StopHandler {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn install_stop_handler(cancel: CancellationToken) -> Option<StopHandler> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut term) = signal(SignalKind::terminate()) else { return None };
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
            cancel.cancel();
        });
        Some(StopHandler(task))
    }
    #[cfg(windows)]
    {
        let event = match std::env::var("BSL_CHECK_STOP_EVENT") {
            Ok(event) => event,
            Err(std::env::VarError::NotPresent) => {
                return Some(StopHandler(tokio::spawn(std::future::pending())));
            }
            Err(std::env::VarError::NotUnicode(_)) => return None,
        };
        if !validate_stop_event(&event) {
            return None;
        }
        let task = tokio::spawn(async move {
            if wait_stop_event(&event, cancel.clone()).await {
                cancel.cancel();
            }
        });
        Some(StopHandler(task))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cancel;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_frame_is_read_through_eof_and_rejects_one_byte_over_limit() {
        let exact = read_request_frame(&b"{}"[..], 2).await.unwrap();
        assert_eq!(exact, b"{}");
        assert_eq!(read_request_frame(&b"{} "[..], 2).await.unwrap_err(), 2);
    }

    #[test]
    fn native_limits_reject_unsupported_stop_grace_before_profile_startup() {
        let mut limits = check_contract::WireLimits {
            timeout_ms: 120_000,
            max_code_bytes: 2 * 1024 * 1024,
            max_response_bytes: 256 * 1024,
            max_stderr_bytes: 64 * 1024,
            stop_grace_ms: 500,
        };
        assert!(supported_limits(&limits));
        limits.stop_grace_ms = 499;
        assert!(!supported_limits(&limits));
        limits.stop_grace_ms = 900_001;
        assert!(!supported_limits(&limits));
    }
}

#[cfg(windows)]
async fn wait_stop_event(name: &str, cancel: CancellationToken) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    let name: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
    use windows_sys::Win32::System::Threading::{
        OpenEventW, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
    };
    let handle = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if handle.is_null() {
        return false;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        match unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 0) } {
            windows_sys::Win32::Foundation::WAIT_OBJECT_0 => return true,
            windows_sys::Win32::Foundation::WAIT_TIMEOUT => {}
            _ => return false,
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return false,
            _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn validate_stop_event(_name: &str) -> bool {
    false
}

#[cfg(windows)]
fn validate_stop_event(name: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let name: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
    if name.len() <= 1 || name.len() > 260 {
        return false;
    }
    unsafe {
        use windows_sys::Win32::System::Threading::{OpenEventW, SYNCHRONIZATION_SYNCHRONIZE};
        let handle = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        windows_sys::Win32::Foundation::CloseHandle(handle);
        true
    }
}
