use crate::mcp_install::model::{
    normalize_source_dir_for_scope, InstallPreset, InstallRequest, ServerSpec,
};

pub(super) fn build_server_spec(request: &InstallRequest, binary: &str) -> ServerSpec {
    match request.preset {
        InstallPreset::Workspace => build_workspace_spec(request, binary),
        InstallPreset::Reference => build_reference_spec(request, binary),
    }
}

/// Set by the `bsl-launcher` binary to its own absolute path before it spawns the app.
/// When present, the MCP config must record the launcher (not the app) so the client
/// keeps going through the launcher's auto-update path. Kept in sync by contract with
/// `crates/bsl-launcher/src/main.rs`.
const LAUNCHER_BIN_ENV: &str = "BSL_ANALYZER_LAUNCHER_BIN";

/// The command an AI client should run to launch this MCP server, as an absolute path so
/// the generated config survives a reduced `PATH`. When invoked through the launcher,
/// prefers the launcher's own path (via `LAUNCHER_BIN_ENV`) so the client keeps going
/// through the auto-updating launcher rather than pinning a cached app binary. Otherwise
/// uses the running executable (`current_exe`), falling back to the bare name.
pub(super) fn resolve_self_binary() -> String {
    // Trust the launcher env only if it still points at an existing path, so a stale
    // value left over from a previous install can't poison the recorded command. The
    // absolute-path guard lives in `resolve_binary` (pure, hence testable).
    let launcher_bin =
        std::env::var(LAUNCHER_BIN_ENV).ok().filter(|s| std::path::Path::new(s).exists());
    let current_exe = std::env::current_exe().ok().map(|path| path.to_string_lossy().into_owned());
    resolve_binary(launcher_bin, current_exe)
}

/// Pure core of [`resolve_self_binary`]: the launcher path wins over the running
/// executable, but only when it is a non-empty **absolute** path — a relative or empty
/// value (e.g. an externally mis-set env var) is ignored so the recorded command never
/// becomes relative. Falls through to the running executable, then a bare name. Split
/// out so the precedence is testable without mutating process-global environment.
fn resolve_binary(launcher_bin: Option<String>, current_exe: Option<String>) -> String {
    launcher_bin
        .filter(|s| !s.is_empty() && std::path::Path::new(s).is_absolute())
        .or(current_exe)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "bsl-analyzer".to_owned())
}

fn build_workspace_spec(request: &InstallRequest, binary: &str) -> ServerSpec {
    let mut args = vec![
        "mcp".to_owned(),
        "serve".to_owned(),
        "--profile".to_owned(),
        "workspace".to_owned(),
        "--source-dir".to_owned(),
        normalize_source_dir_for_scope(request.scope, &request.project_dir, &request.source_dir),
    ];

    if let Some(url) = &request.onec_url {
        args.push("--onec-url".to_owned());
        args.push(url.clone());
    }

    if !request.onec_user.is_empty() {
        args.push("--onec-user".to_owned());
        args.push(request.onec_user.clone());
    }

    if !request.onec_password.is_empty() {
        args.push("--onec-password".to_owned());
        args.push(request.onec_password.clone());
    }

    args.extend(enable_tool_args(request));

    ServerSpec {
        name: request.name.clone(),
        command: binary.to_owned(),
        args,
        env: request.env.clone(),
    }
}

fn build_reference_spec(request: &InstallRequest, binary: &str) -> ServerSpec {
    let mut args =
        vec!["mcp".to_owned(), "serve".to_owned(), "--profile".to_owned(), "reference".to_owned()];
    args.extend(enable_tool_args(request));

    ServerSpec {
        name: request.name.clone(),
        command: binary.to_owned(),
        args,
        env: request.env.clone(),
    }
}

/// The flags that make the installed config self-describing: reading it shows which
/// opt-in tools this server was set up to serve, without knowing the build's defaults.
fn enable_tool_args(request: &InstallRequest) -> Vec<String> {
    request
        .enable_tools
        .iter()
        .flat_map(|tool| ["--enable-tool".to_owned(), tool.clone()])
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use crate::mcp_install::model::{
        InstallPreset, InstallRequest, InstallScope, InstallTarget, InstallTargetSelector,
    };

    use super::build_server_spec;

    fn request() -> InstallRequest {
        InstallRequest {
            target: InstallTargetSelector::One(InstallTarget::Cursor),
            scope: InstallScope::Project,
            preset: InstallPreset::Workspace,
            name: "bsl-analyzer".to_owned(),
            project_dir: PathBuf::from("/tmp/workspace"),
            source_dir: PathBuf::from("/tmp/workspace/src"),
            onec_url: Some("http://localhost/base/hs/bsl-analyzer".to_owned()),
            onec_user: "admin".to_owned(),
            onec_password: "secret".to_owned(),
            env: BTreeMap::from([("NAPARNIK_TOKEN".to_owned(), "test".to_owned())]),
            enable_tools: Vec::new(),
            force: false,
            dry_run: false,
        }
    }

    /// The installed config carries the flag, so reading it shows which opt-in tools this
    /// server was set up to serve without knowing the build's defaults — and the server
    /// the client launches serves them, since it is that argv the client runs.
    #[test]
    fn enabled_tools_reach_the_installed_argv_in_both_presets() {
        for preset in [InstallPreset::Workspace, InstallPreset::Reference] {
            let mut request = request();
            request.preset = preset;
            request.enable_tools = vec!["references".to_owned(), "call_hierarchy".to_owned()];

            let spec = build_server_spec(&request, "bsl-analyzer");

            for tool in &request.enable_tools {
                assert!(
                    spec.args.windows(2).any(|pair| pair[0] == "--enable-tool" && pair[1] == *tool),
                    "{preset:?} argv does not carry --enable-tool {tool}: {:?}",
                    spec.args
                );
            }
        }
    }

    /// Nothing asked for, nothing written: an install that did not name a tool must produce
    /// the same command line it produced before the flag existed.
    #[test]
    fn no_enable_tool_flag_appears_when_none_was_asked_for() {
        let spec = build_server_spec(&request(), "bsl-analyzer");

        assert!(spec.args.iter().all(|arg| arg != "--enable-tool"), "argv: {:?}", spec.args);
    }

    #[test]
    fn workspace_preset_builds_expected_stdio_command() {
        let spec = build_server_spec(&request(), "bsl-analyzer");

        assert_eq!(spec.name, "bsl-analyzer");
        assert_eq!(spec.command, "bsl-analyzer");
        assert_eq!(
            spec.args,
            vec![
                "mcp",
                "serve",
                "--profile",
                "workspace",
                "--source-dir",
                "src",
                "--onec-url",
                "http://localhost/base/hs/bsl-analyzer",
                "--onec-user",
                "admin",
                "--onec-password",
                "secret",
            ]
        );
        assert_eq!(spec.env.get("NAPARNIK_TOKEN"), Some(&"test".to_owned()));
    }

    #[test]
    fn reference_preset_builds_global_reference_command() {
        let mut req = request();
        req.preset = InstallPreset::Reference;

        let spec = build_server_spec(&req, "bsl-analyzer");

        assert_eq!(spec.name, "bsl-analyzer");
        assert_eq!(spec.command, "bsl-analyzer");
        assert_eq!(spec.args, vec!["mcp", "serve", "--profile", "reference"]);
        assert_eq!(spec.env.get("NAPARNIK_TOKEN"), Some(&"test".to_owned()));
    }

    /// The resolved binary path is written verbatim as the launch command, so a config
    /// generated with an absolute path keeps it (immune to the client's reduced PATH).
    #[test]
    fn server_spec_uses_provided_binary_path() {
        let spec = build_server_spec(&request(), "/usr/local/bin/bsl-analyzer");
        assert_eq!(spec.command, "/usr/local/bin/bsl-analyzer");

        let mut req = request();
        req.preset = InstallPreset::Reference;
        let spec = build_server_spec(&req, "/opt/bsl/bsl-analyzer");
        assert_eq!(spec.command, "/opt/bsl/bsl-analyzer");
    }

    /// `resolve_self_binary` yields an absolute path (the running executable), which is
    /// what makes the generated client config robust to a reduced PATH.
    #[test]
    fn resolve_self_binary_is_absolute() {
        let bin = super::resolve_self_binary();
        assert!(!bin.is_empty());
        assert!(std::path::Path::new(&bin).is_absolute(), "expected an absolute path, got {bin}");
    }

    /// When started via the launcher (`BSL_ANALYZER_LAUNCHER_BIN` set), the launcher path
    /// wins over the running app binary, so the generated config keeps going through the
    /// auto-updating launcher.
    #[test]
    fn resolve_binary_prefers_launcher_over_app() {
        let launcher = std::env::temp_dir().join("bsl-analyzer").to_string_lossy().into_owned();
        let bin = super::resolve_binary(
            Some(launcher.clone()),
            Some("/home/u/.bsl-analyzer/bin/bsl-analyzer-app".to_owned()),
        );
        assert_eq!(bin, launcher);
    }

    /// Without the launcher env (or with it empty), it falls back to the running app
    /// binary — preserving the absolute-path-for-reduced-PATH behaviour for a bare app.
    #[test]
    fn resolve_binary_falls_back_to_current_exe() {
        let app = "/opt/bsl/bsl-analyzer-app".to_owned();
        assert_eq!(super::resolve_binary(None, Some(app.clone())), app);
        assert_eq!(super::resolve_binary(Some(String::new()), Some(app.clone())), app);
    }

    /// Last resort when neither source resolves: the bare name (relies on PATH).
    #[test]
    fn resolve_binary_falls_back_to_bare_name() {
        assert_eq!(super::resolve_binary(None, None), "bsl-analyzer");
        assert_eq!(super::resolve_binary(Some(String::new()), Some(String::new())), "bsl-analyzer");
    }

    /// A relative launcher value (e.g. an externally mis-set env var) is rejected so the
    /// recorded command never becomes relative; it falls through to the running exe.
    #[test]
    fn resolve_binary_ignores_relative_launcher_value() {
        let app = "/opt/bsl/bsl-analyzer-app".to_owned();
        assert_eq!(
            super::resolve_binary(Some("bin/bsl-analyzer".to_owned()), Some(app.clone())),
            app
        );
        assert_eq!(super::resolve_binary(Some("bsl-analyzer".to_owned()), Some(app.clone())), app);
    }
}
