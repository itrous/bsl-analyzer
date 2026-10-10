use std::{
    collections::BTreeMap,
    env,
    error::Error,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, Subcommand, ValueEnum};
use process_record::ProcessRecordGuard;
use serde::Deserialize;

mod process_record;

#[derive(Debug, Deserialize)]
struct OnecConnectionsFile {
    connections: BTreeMap<String, OnecConnectionConfig>,
}

#[derive(Debug, Deserialize)]
struct OnecConnectionConfig {
    url: String,
    #[serde(default)]
    user_env: String,
    #[serde(default)]
    password_env: String,
    #[serde(default)]
    allow_execute: bool,
}

#[derive(Subcommand)]
pub enum McpCommand {
    Serve(McpServeArgs),

    Install(McpInstallArgs),
}

#[derive(Debug, Args, Clone)]
pub struct McpServeArgs {
    #[arg(long = "profile", value_enum)]
    runtime_profile: McpProfileCli,

    #[arg(short = 's', long = "source-dir", required_if_eq("runtime_profile", "workspace"))]
    source_dir: Option<PathBuf>,

    /// Serve an opt-in tool of this profile, repeatable. Names come from the
    /// contract declaration (`opt_in_tools`), not from `tools/list` — an opt-in tool
    /// is absent from the served list until a launch asks for it. A name this profile
    /// does not declare stops the start and lists the ones it does; naming a tool that
    /// is already served changes nothing, so a config keeps working after an opt-in
    /// tool becomes a default.
    #[arg(long = "enable-tool")]
    enable_tools: Vec<String>,

    /// Base directory for workspace-derived files. CLI > BSL_CACHE_DIR > OS cache;
    /// the final leaf is workspaces/v1/<scope digest>. Children receive the absolute base and
    /// frozen scope stamp. Relative paths use the process cwd.
    #[arg(long = "cache-dir")]
    cache_dir: Option<PathBuf>,

    /// Connection mode. `stdio` (default) serves one client directly. `broker`
    /// connects to a shared per-project backend (launching it if absent) and relays
    /// — so many clients/reviews reuse one heavy process. The backend stays warm across
    /// client disconnects and reconnects, then idles out on its own once no client has
    /// used it for the idle TTL (`BSL_MCP_IDLE_TTL_SECS`, default 300s); a backend that
    /// never served any traffic gives up after a short orphan grace
    /// (`BSL_MCP_ORPHAN_GRACE_SECS`, default 30s). `http` serves multiple clients over
    /// Streamable HTTP using individual options or `--config`. `daemon` *is* the broker backend
    /// and is launched internally by a broker proxy. `broker-required` connects only to the
    /// already-running daemon named by `--backend-pid`, whose value the supervisor knows
    /// because it started that daemon: it never launches or falls back to direct stdio, and
    /// it serves the 1C connection the daemon was started with.
    #[arg(long = "mode", value_enum, default_value = "stdio")]
    mode: McpServeMode,

    /// PID of the daemon explicitly launched by a supervisor. Required only for
    /// `--mode broker-required`; the connected socket peer must match it.
    ///
    /// The supervisor must launch `bsl-analyzer-app` itself: the installed `bsl-analyzer` is a
    /// launcher that runs the analyzer as a child, so its pid is not the one that owns the
    /// socket. The value stays outside the machine's own files on purpose — a pin read from
    /// something any same-user process may write would certify whoever wrote it.
    #[arg(long, required_if_eq("mode", "broker-required"))]
    backend_pid: Option<u32>,

    /// IP address for HTTP binding (default: 127.0.0.1).
    #[arg(long)]
    host: Option<IpAddr>,

    /// TCP port for HTTP mode. Required without `--config` and must be in 1..=65535.
    #[arg(long)]
    port: Option<u16>,

    /// Accepted HTTP Host header: the address clients dial, not the name of the client.
    ///
    /// Repeat for aliases; an entry without a port accepts any port. At least one is
    /// required for a non-loopback --host. A list given here replaces the built-in
    /// localhost, 127.0.0.1 and ::1 entirely, so name them again if the server must keep
    /// answering its own machine. A wildcard bind address such as 0.0.0.0 is not a Host
    /// value and allows nothing that a client normally sends.
    #[arg(long = "allowed_hosts", num_args = 1.., value_name = "HOST")]
    allowed_hosts: Vec<String>,

    /// Path to a TOML file with HTTP binding settings.
    ///
    /// Alternative to `--host`, `--port` and `--allowed_hosts`. The file contains top-level
    /// `host`, `port` and `allowed_hosts` fields; `allowed_hosts` is an array of strings. Only
    /// `port` is required: an omitted `host` binds loopback and an omitted `allowed_hosts`
    /// keeps the built-in local names, exactly as the flags do.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["host", "port", "allowed_hosts"])]
    config: Option<PathBuf>,

    #[arg(long)]
    onec_url: Option<String>,

    #[arg(long, default_value = "")]
    onec_user: String,

    #[arg(long, default_value = "")]
    onec_password: String,

    #[command(flatten)]
    source_set: super::source_set::SourceSetArgs,
}

impl McpCommand {
    /// Opt-in on-disk log destination for a daemon backend, enabled by
    /// `BSL_MCP_DAEMON_LOG` (`1`/`true`/`yes`/`on` → `bsl-analyzer-daemon.log`
    /// in the effective workspace cache; any other non-empty value is taken
    /// as an explicit path). A daemon is spawned by a broker proxy with no
    /// terminal, so without a file its diagnostics (build heartbeat, stall
    /// watchdog reports) vanish into a closed stderr — set the variable when
    /// investigating daemon behaviour. Off by default: routine deployments should
    /// not accrete log files. The stall watchdog separately drops a one-shot
    /// report file next to the graph database when a build wedges, so that
    /// post-mortem survives even with logging off.
    ///
    /// The file is APPENDED across runs (each run stamps a session-start line),
    /// never truncated or renamed at startup: the broker's spawn race can start
    /// several daemon candidates within seconds, and a start-time rotation by a
    /// losing candidate would rename the winning daemon's live log out from
    /// under it. Rotation to `.prev` happens only on size (a wedged-build trail
    /// is kilobytes, so the rotation window and a spawn race cannot coincide).
    ///
    /// Returns `None` for non-daemon modes (stdio/broker keep stderr: a direct
    /// client or terminal is attached), when the opt-in is absent, and when the
    /// directory cannot be prepared (logging then falls back to stderr rather
    /// than failing startup).
    pub fn default_daemon_log_file(&self) -> Option<PathBuf> {
        self.daemon_log_file_for(std::env::var("BSL_MCP_DAEMON_LOG").ok().as_deref())
    }

    fn daemon_log_file_for(&self, opt_in: Option<&str>) -> Option<PathBuf> {
        self.daemon_log_file_for_scope(
            opt_in,
            mcp_server::expected_scope_from_env().ok()?.as_deref(),
        )
    }

    fn daemon_log_file_for_scope(
        &self,
        opt_in: Option<&str>,
        expected_scope: Option<&str>,
    ) -> Option<PathBuf> {
        const ROTATE_BYTES: u64 = 50 * 1024 * 1024;

        let McpCommand::Serve(args) = self else { return None };
        if !matches!(args.mode, McpServeMode::Daemon) {
            return None;
        }
        // Logging is prepared before the arguments are validated, and preparing an explicit
        // cache root CREATES it — so a command that is about to be rejected would leave a
        // directory, and a log inside it, at a path the caller named. Ask the validator
        // rather than restating some of its rules here: every reason it rejects for
        // (`--host` outside http mode, a bad port, an empty allowlist) reaches this code
        // just the same.
        if validate_serve_args(args).is_err() {
            return None;
        }
        let opt_in = match opt_in.map(str::trim) {
            None | Some("" | "0" | "false" | "no" | "off") => return None,
            Some("1" | "true" | "yes" | "on") => None,
            Some(path) => Some(PathBuf::from(path)),
        };
        // Both the cache log and a caller-selected log are prepared only after
        // the same frozen Project/scope gate. Install source selection first so
        // Project parsing and cache resolution cannot disagree with the later
        // server launch.
        let source_dir = args.source_dir.as_deref()?.canonicalize().ok()?;
        install_source_set(args, &source_dir).ok()?;
        let project = mcp_server::project::at(&source_dir).ok()?;
        let layout = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            args.cache_dir.as_deref(),
            &std::env::current_dir().ok()?,
            expected_scope,
        )
        .ok()?;
        let log_path = if let Some(path) = opt_in {
            // An unpreparable explicit path degrades to stderr like every other
            // opt-in logging failure, but only after the launch gate above.
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).ok()?;
            }
            path
        } else {
            layout.ensure().ok()?;
            layout.daemon_log_path()
        };
        if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
            // Append `.prev` to the whole file name (`with_extension` would
            // truncate a dotted name like `daemon.custom.log`).
            let mut rotated = log_path.file_name().unwrap_or_default().to_os_string();
            rotated.push(".prev");
            let _ = std::fs::rename(&log_path, log_path.with_file_name(rotated));
        }
        Some(log_path)
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpServeMode {
    Stdio,
    Broker,
    BrokerRequired,
    Daemon,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpServeOptions {
    // The file is an alternative to the flags, so it inherits their defaults: a host left out
    // binds loopback and an omitted allowlist keeps the built-in local names. Without these a
    // working `--mode http --port 8021` would stop working the moment it was moved into a file,
    // which is the one migration the file exists for.
    #[serde(default = "loopback_host")]
    host: IpAddr,
    port: u16,
    #[serde(default)]
    allowed_hosts: Vec<String>,
}

fn loopback_host() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

#[derive(Args)]
pub struct McpInstallArgs {
    #[arg(long, value_enum)]
    target: InstallTargetCli,

    #[arg(long, value_enum)]
    scope: Option<InstallScopeCli>,

    #[arg(long, value_enum, default_value_t = InstallPresetCli::Workspace)]
    preset: InstallPresetCli,

    /// Write `--enable-tool <name>` into the generated client config, repeatable. The
    /// name is validated against the preset's profile, so an installed config cannot
    /// be one that refuses to start.
    #[arg(long = "enable-tool")]
    enable_tools: Vec<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(short = 's', long = "source-dir")]
    source_dir: Option<PathBuf>,

    #[arg(long)]
    onec_url: Option<String>,

    #[arg(long, default_value = "")]
    onec_user: String,

    #[arg(long, default_value = "")]
    onec_password: String,

    #[arg(long = "env", value_parser = parse_env_pair)]
    env: Vec<(String, String)>,

    #[arg(long)]
    force: bool,

    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum InstallTargetCli {
    Codex,
    Gemini,
    Claude,
    Cursor,
    All,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum InstallScopeCli {
    User,
    Project,
    Local,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum InstallPresetCli {
    #[default]
    Workspace,
    Reference,
    Recommended,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpProfileCli {
    Workspace,
    Reference,
}

pub fn run(command: McpCommand) -> Result<(), Box<dyn Error + Send + Sync>> {
    match command {
        McpCommand::Serve(args) => run_mcp_serve(args),
        McpCommand::Install(args) => run_mcp_install(args),
    }
}

fn run_mcp_serve(args: McpServeArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    let http_options = validate_serve_args(&args)?;
    if let Some(ref options) = http_options {
        warn_on_wildcard_allowlist(&options.allowed_hosts);
    }
    let raw_password =
        resolve_onec_password(&args.onec_password, env::var("BSL_ONEC_PASSWORD").ok());
    let password = decode_password(&raw_password);
    let profile = profile_of(args.runtime_profile);
    if matches!(profile, mcp_server::McpProfile::Workspace) {
        let source = args
            .source_dir
            .as_deref()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workspace profile requires --source-dir",
                )
            })?
            .canonicalize()?;
        install_source_set(&args, &source)?;
    }
    let (source_dir, workspace_cache) =
        resolve_workspace_inputs(profile, args.source_dir.clone(), args.cache_dir.as_deref())?;

    match resolve_serve_mode(args.mode, profile)? {
        McpServeMode::Stdio => run_mcp_server(
            profile,
            ServerInputs {
                source_dir,
                workspace_cache,
                onec_url: args.onec_url,
                onec_user: args.onec_user,
                onec_password: password,
                enable_tools: args.enable_tools,
            },
        ),
        McpServeMode::Broker => {
            run_mcp_broker(profile, &args, source_dir, workspace_cache, &password)
        }
        McpServeMode::BrokerRequired => {
            run_mcp_broker_required(profile, &args, source_dir, workspace_cache)
        }
        McpServeMode::Daemon => run_mcp_daemon(
            profile,
            ServerInputs {
                source_dir,
                workspace_cache,
                onec_url: args.onec_url,
                onec_user: args.onec_user,
                onec_password: password,
                enable_tools: args.enable_tools,
            },
        ),
        McpServeMode::Http => {
            let options =
                http_options.expect("validated HTTP mode must contain HTTP serve options");
            run_mcp_http(
                profile,
                ServerInputs {
                    source_dir,
                    workspace_cache,
                    onec_url: args.onec_url,
                    onec_user: args.onec_user,
                    onec_password: password,
                    enable_tools: args.enable_tools,
                },
                options,
            )
        }
    }
}

fn install_source_set(args: &McpServeArgs, root: &Path) -> io::Result<()> {
    let source_set = args
        .source_set
        .resolve(root)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if let Some(installed) = mcp_server::project::source_set_override() {
        return if installed == &source_set {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a different source set override is already installed",
            ))
        };
    }
    if !mcp_server::project::set_source_set_override(source_set.clone()) {
        let installed = mcp_server::project::source_set_override();
        if installed.is_some_and(|installed| installed == &source_set) {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a different source set override was installed concurrently",
        ));
    }
    Ok(())
}

/// The 1C credential this process will use. The broker passes it to the detached daemon through
/// the environment (not argv, which `ps` would expose for the backend's whole lifetime), so the
/// flag and the variable are two ways in to one value — and anything asking "was a credential
/// given" has to ask about that value, not about the flag.
fn resolve_onec_password(flag: &str, from_env: Option<String>) -> String {
    if flag.is_empty() {
        from_env.unwrap_or_default()
    } else {
        flag.to_owned()
    }
}

/// Gate the 1C connection settings against a mode that cannot apply them.
///
/// Every other mode either builds the state itself or launches the daemon that will, so the
/// settings reach the process that connects to 1C. The supervised proxy connects to a backend
/// that was built before it started: its settings would be silently dropped, and a client
/// naming one infobase would have its `execute` run against the one the daemon was built with.
fn validate_onec_settings(
    mode: McpServeMode,
    url: Option<&str>,
    user: &str,
    password: &str,
) -> Result<(), io::Error> {
    if !matches!(mode, McpServeMode::BrokerRequired) {
        return Ok(());
    }
    if url.is_some() || !user.is_empty() || !password.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--onec-url/--onec-user/--onec-password are not valid with --mode broker-required: \
             the already-running daemon serves the connection it was started with. Pass them to \
             that daemon instead.",
        ));
    }
    Ok(())
}

/// Gate `--backend-pid` and the mode that needs it.
///
/// `peer_pid_available` is a parameter rather than a direct read of the platform list so the
/// refusal can be exercised from a platform that does supply peer PIDs — a check only reachable
/// on macOS would ship untested.
fn validate_backend_pid(
    mode: McpServeMode,
    backend_pid: Option<u32>,
    peer_pid_available: bool,
) -> Result<(), io::Error> {
    if !matches!(mode, McpServeMode::BrokerRequired) {
        if backend_pid.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--backend-pid is only valid with --mode broker-required",
            ));
        }
        return Ok(());
    }
    if !peer_pid_available {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "--mode broker-required is unavailable on this platform: peer credentials carry no \
             process id, so the supervised backend cannot be identified. Use --mode broker.",
        ));
    }
    if backend_pid == Some(0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--backend-pid must be greater than zero",
        ));
    }
    Ok(())
}

fn profile_of(profile: McpProfileCli) -> mcp_server::McpProfile {
    match profile {
        McpProfileCli::Workspace => mcp_server::McpProfile::Workspace,
        McpProfileCli::Reference => mcp_server::McpProfile::Reference,
    }
}

/// Warn about an allowlist entry that names a wildcard bind address.
///
/// A warning rather than a rejection: `0.0.0.0` is a legal thing to dial on Linux, so the
/// entry is not provably dead. It is, however, almost always the bind address copied into
/// the wrong flag, and the resulting server refuses every real client while looking
/// configured — a diagnosis that costs far more later than a line at startup.
fn warn_on_wildcard_allowlist(allowed_hosts: &[String]) {
    let wildcard = mcp_server::wildcard_allowed_hosts(allowed_hosts);
    if !wildcard.is_empty() {
        tracing::warn!(
            entries = %wildcard.join(", "),
            "allowed_hosts names a bind address rather than a Host header; \
             a client sends the address it dialed, which is never a wildcard"
        );
    }
}

fn load_http_options(path: &Path) -> Result<HttpServeOptions, io::Error> {
    let contents = std::fs::read_to_string(path).map_err(|error| {
        io::Error::new(error.kind(), format!("failed to read --config {}: {error}", path.display()))
    })?;
    toml::from_str(&contents).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse --config {} as TOML: {error}", path.display()),
        )
    })
}

fn validate_http_options(options: HttpServeOptions) -> Result<HttpServeOptions, io::Error> {
    if options.port == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "port must be in 1..=65535"));
    }

    // A blank entry satisfies no `Host` at all, so counting it towards the allowlist
    // would start a server that rejects every request it receives.
    if options.allowed_hosts.iter().any(|host| host.trim().is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "allowed_hosts must not contain empty entries",
        ));
    }

    if !options.host.is_loopback() && options.allowed_hosts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a non-loopback host requires at least one allowed_hosts entry",
        ));
    }

    Ok(options)
}

fn validate_serve_args(args: &McpServeArgs) -> Result<Option<HttpServeOptions>, io::Error> {
    // Before anything is built: an unknown name is a typo in a client config, and ignoring
    // it silently would leave the caller believing they had enabled something.
    mcp_server::contract::validate_enabled_tools(
        profile_of(args.runtime_profile),
        &args.enable_tools,
    )
    .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;

    if matches!(args.runtime_profile, McpProfileCli::Reference) && args.cache_dir.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reference profile does not accept --cache-dir",
        ));
    }
    // An unset shell variable expands to an empty argument, which resolves to the current
    // directory — the multi-gigabyte databases would land wherever the client was started.
    if args.cache_dir.as_deref().is_some_and(|dir| dir.as_os_str().is_empty()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--cache-dir must not be empty"));
    }
    if matches!(args.runtime_profile, McpProfileCli::Reference)
        && (args.onec_url.is_some() || !args.onec_user.is_empty() || !args.onec_password.is_empty())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reference profile does not accept --onec-url/--onec-user/--onec-password",
        ));
    }

    // The reference profile has no workspace to describe, and relative flag paths
    // would have no root to resolve against.
    if matches!(args.runtime_profile, McpProfileCli::Reference) && !args.source_set.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reference profile does not accept source-set flags \
             (--configuration-root/--extension/--extension-depends-on/--no-extensions)",
        ));
    }

    // Ahead of the per-mode blocks below: a flag belonging to one mode has to be rejected in
    // every other mode, http included, and a check living inside one mode's block cannot do it.
    validate_backend_pid(args.mode, args.backend_pid, mcp_server::broker::peer_pid_available())?;
    // Validated against the credential the process will actually use — resolved from either of
    // its two ways in and decoded, exactly as the serving path does. A check reading the raw
    // flag alone would both miss a password arriving through the environment and refuse an
    // encoding of no password at all.
    let effective_password = decode_password(&resolve_onec_password(
        &args.onec_password,
        env::var("BSL_ONEC_PASSWORD").ok(),
    ));
    validate_onec_settings(
        args.mode,
        args.onec_url.as_deref(),
        &args.onec_user,
        &effective_password,
    )?;

    if !matches!(args.mode, McpServeMode::Http) {
        if args.host.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--host is only valid with --mode http",
            ));
        }
        if args.port.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--port is only valid with --mode http",
            ));
        }
        if !args.allowed_hosts.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--allowed_hosts is only valid with --mode http",
            ));
        }
        if args.config.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--config is only valid with --mode http",
            ));
        }
        return Ok(None);
    }

    if let Some(config_path) = args.config.as_deref() {
        if args.host.is_some() || args.port.is_some() || !args.allowed_hosts.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--config cannot be combined with --host, --port or --allowed_hosts",
            ));
        }
        return Ok(Some(validate_http_options(load_http_options(config_path)?)?));
    }

    let port = args.port.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "--port or --config is required with --mode http",
        )
    })?;
    // Both ways in are validated by the same function: a rule enforced on one of them alone
    // would let the other start a server the first would have refused.
    Ok(Some(validate_http_options(HttpServeOptions {
        host: args.host.unwrap_or_else(loopback_host),
        port,
        allowed_hosts: args.allowed_hosts.clone(),
    })?))
}

fn resolve_workspace_inputs(
    profile: mcp_server::McpProfile,
    source_dir: Option<PathBuf>,
    cache_dir: Option<&Path>,
) -> io::Result<(Option<PathBuf>, Option<mcp_server::WorkspaceCacheLayout>)> {
    if !matches!(profile, mcp_server::McpProfile::Workspace) {
        return Ok((source_dir, None));
    }
    let source_dir = source_dir.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "workspace profile requires --source-dir")
    })?;
    let canonical_source = source_dir.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to canonicalize --source-dir {}: {error}", source_dir.display()),
        )
    })?;
    let project = mcp_server::project::at(&canonical_source)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let cwd = env::current_dir()?;
    let cache = mcp_server::WorkspaceCacheLayout::for_project(
        &project,
        cache_dir,
        &cwd,
        mcp_server::expected_scope_from_env()?.as_deref(),
    )?;
    Ok((Some(canonical_source), Some(cache)))
}

/// Resolve the effective serve mode.
///
/// An explicit `--mode` always wins (the re-exec'd daemon passes `--mode daemon`, so
/// inheriting the env switch can't turn it back into a proxy). Otherwise the broker
/// applies only to the heavy `workspace` profile, decided by env then platform:
///
/// - `BSL_MCP_BROKER` is an explicit opt-in/out on any OS. It is the only signal that
///   reaches Codex: Codex imports a project's `.mcp.json` but reconstructs the argv
///   (dropping extra flags) and does not propagate its `env` block — so the activation
///   has to live in the binary's own default, not the client config.
/// - With no override, the heavy `workspace` profile defaults to the broker on every
///   platform. Windows is now included: the named-pipe transport carries an explicit
///   current-user security descriptor and verifies the backend's identity (defeating pipe
///   squatting). The backend stays warm across client reconnects and idles out on its own.
///   `BSL_MCP_BROKER=0` forces plain stdio anywhere.
fn resolve_serve_mode(
    flag: McpServeMode,
    profile: mcp_server::McpProfile,
) -> Result<McpServeMode, io::Error> {
    let context =
        ServeModeContext { broker_override: env_broker_override(), platform_default_broker: true };
    Ok(resolve_serve_mode_with_override(flag, profile, context))
}

#[derive(Clone, Copy)]
struct ServeModeContext {
    broker_override: Option<bool>,
    /// Whether this platform defaults the workspace profile to the broker. Injected so
    /// the precedence (`--mode` flag → env override → profile → platform) stays
    /// unit-testable; production passes `true` on every platform.
    platform_default_broker: bool,
}

fn resolve_serve_mode_with_override(
    flag: McpServeMode,
    profile: mcp_server::McpProfile,
    context: ServeModeContext,
) -> McpServeMode {
    if !matches!(flag, McpServeMode::Stdio) {
        return flag;
    }
    if !matches!(profile, mcp_server::McpProfile::Workspace) {
        return McpServeMode::Stdio;
    }
    match context.broker_override {
        Some(true) => return McpServeMode::Broker,
        Some(false) => return McpServeMode::Stdio,
        None => {}
    }
    if context.platform_default_broker {
        McpServeMode::Broker
    } else {
        McpServeMode::Stdio
    }
}

/// `BSL_MCP_BROKER` parsed as an explicit tristate: `Some(true|false)` for a recognized
/// truthy/falsy value, `None` when unset or unrecognized (defer to the platform default).
fn env_broker_override() -> Option<bool> {
    let value = env::var("BSL_MCP_BROKER").ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The broker proxy: connect to (or launch) the shared per-project backend and relay
/// stdio to it. The backend is this same binary re-executed with `--mode daemon` and
/// the same launch parameters, so it resolves to the same backend identity.
fn daemon_command(
    executable: &Path,
    source_dir: &Path,
    workspace_cache: &mcp_server::WorkspaceCacheLayout,
    args: &McpServeArgs,
    topology_fp: u64,
    embedding_prefixes: &mcp_server::EmbeddingPrefixes,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(executable);
    cmd.arg("mcp")
        .arg("serve")
        .arg("--profile")
        .arg("workspace")
        .arg("--source-dir")
        .arg(source_dir)
        .arg("--mode")
        .arg("daemon");
    // The scope stamp preserves default laziness even though the resolved base is forwarded.
    cmd.arg("--cache-dir").arg(workspace_cache.declared_base());
    cmd.env(mcp_server::WORKSPACE_CACHE_SCOPE_ENV, workspace_cache.scope_stamp());
    cmd.env_remove(mcp_server::WORKSPACE_CACHE_BASE_ENV);
    // The daemon must resolve the same project this proxy keyed on, and its own
    // config file cannot tell it about a source set that came from argv.
    cmd.args(args.source_set.to_args());
    // Every requested name, not just the effective ones: the daemon projects them through
    // the same declaration and so arrives at the identity the proxy keyed on. Dropping one
    // here would leave the proxy's key promising a surface the daemon does not serve.
    for tool in &args.enable_tools {
        cmd.arg("--enable-tool").arg(tool);
    }
    if let Some(url) = &args.onec_url {
        cmd.arg("--onec-url").arg(url);
    }
    if !args.onec_user.is_empty() {
        cmd.arg("--onec-user").arg(&args.onec_user);
    }
    if !args.onec_password.is_empty() {
        cmd.env("BSL_ONEC_PASSWORD", &args.onec_password);
    }
    cmd.env(mcp_server::broker::TOPOLOGY_FP_ENV, topology_fp.to_string());
    cmd.env(mcp_server::broker::EMBEDDING_QUERY_PREFIX_ENV, &embedding_prefixes.query);
    cmd.env(mcp_server::broker::EMBEDDING_DOCUMENT_PREFIX_ENV, &embedding_prefixes.document);
    cmd.env_remove(mcp_server::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV);
    cmd.env_remove(mcp_server::broker::EMBEDDING_TOKENIZER_FILE_ENV);
    cmd.env_remove(mcp_server::broker::EMBEDDING_TOKENIZER_SHA256_ENV);
    if let Some(profile) = &embedding_prefixes.token_profile {
        cmd.env(
            mcp_server::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV,
            profile.max_input_tokens.to_string(),
        );
        cmd.env(mcp_server::broker::EMBEDDING_TOKENIZER_FILE_ENV, &profile.tokenizer_file);
        cmd.env(mcp_server::broker::EMBEDDING_TOKENIZER_SHA256_ENV, &profile.tokenizer_sha256);
    }
    cmd
}

fn resolve_embedding_prefixes(
    source_dir: Option<&Path>,
) -> Result<mcp_server::EmbeddingPrefixes, Box<dyn Error + Send + Sync>> {
    let frozen_query = env::var(mcp_server::broker::EMBEDDING_QUERY_PREFIX_ENV).ok();
    let frozen_document = env::var(mcp_server::broker::EMBEDDING_DOCUMENT_PREFIX_ENV).ok();
    let frozen = frozen_query.zip(frozen_document);
    if let Some((query, document)) = frozen.as_ref() {
        return frozen_embedding_prefixes(
            query,
            document,
            [
                env::var_os(mcp_server::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV),
                env::var_os(mcp_server::broker::EMBEDDING_TOKENIZER_FILE_ENV),
                env::var_os(mcp_server::broker::EMBEDDING_TOKENIZER_SHA256_ENV),
            ],
        );
    }
    let config = source_dir.map(project_model::Project::new).transpose()?;
    let embedding = config.as_ref().map(|project| &project.config.search.baseline.embedding);
    let query_env = env::var("EMBEDDING_QUERY_PREFIX").ok();
    let document_env = env::var("EMBEDDING_DOCUMENT_PREFIX").ok();
    let mut profile = resolve_embedding_prefix_values(
        embedding,
        query_env.as_deref(),
        document_env.as_deref(),
        frozen.as_ref().map(|(query, document)| (query.as_str(), document.as_str())),
    );
    profile.token_profile = resolve_token_profile(config.as_ref(), source_dir)?;
    Ok(profile)
}

fn frozen_embedding_prefixes(
    query: &str,
    document: &str,
    frozen_profile: [Option<std::ffi::OsString>; 3],
) -> Result<mcp_server::EmbeddingPrefixes, Box<dyn Error + Send + Sync>> {
    let mut prefixes = resolve_embedding_prefix_values(None, None, None, Some((query, document)));
    // The launcher froze the whole input profile, so neither the project file nor the
    // ambient environment may add a token profile the launcher did not key on.
    prefixes.token_profile =
        resolve_token_profile_values(None, None, frozen_profile, [None, None, None])?;
    Ok(prefixes)
}

fn resolve_embedding_prefix_values(
    config: Option<&project_model::SearchEmbeddingConfig>,
    query_environment: Option<&str>,
    document_environment: Option<&str>,
    frozen: Option<(&str, &str)>,
) -> mcp_server::EmbeddingPrefixes {
    if let Some((query, document)) = frozen {
        return mcp_server::EmbeddingPrefixes {
            query: query.to_owned(),
            document: document.to_owned(),
            token_profile: None,
        };
    }
    let query = match config {
        Some(config) => config.resolve_query_prefix(query_environment),
        None => query_environment.unwrap_or_default().to_owned(),
    };
    let document = match config {
        Some(config) => config.resolve_document_prefix(document_environment),
        None => document_environment.unwrap_or_default().to_owned(),
    };
    mcp_server::EmbeddingPrefixes { query, document, token_profile: None }
}

fn resolve_token_profile(
    project: Option<&project_model::Project>,
    workspace: Option<&Path>,
) -> Result<Option<mcp_server::EmbeddingTokenProfile>, Box<dyn Error + Send + Sync>> {
    resolve_token_profile_values(
        project,
        workspace,
        [
            env::var_os(mcp_server::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV),
            env::var_os(mcp_server::broker::EMBEDDING_TOKENIZER_FILE_ENV),
            env::var_os(mcp_server::broker::EMBEDDING_TOKENIZER_SHA256_ENV),
        ],
        [
            env::var_os("EMBEDDING_MAX_INPUT_TOKENS"),
            env::var_os("EMBEDDING_TOKENIZER_FILE"),
            env::var_os("EMBEDDING_TOKENIZER_SHA256"),
        ],
    )
}

fn resolve_token_profile_values(
    project: Option<&project_model::Project>,
    workspace: Option<&Path>,
    frozen: [Option<std::ffi::OsString>; 3],
    environment: [Option<std::ffi::OsString>; 3],
) -> Result<Option<mcp_server::EmbeddingTokenProfile>, Box<dyn Error + Send + Sync>> {
    mcp_server::resolve_embedding_token_profile_values(
        project.map(|project| &project.config),
        workspace,
        frozen,
        environment,
    )
    .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)
}

fn run_mcp_broker(
    profile: mcp_server::McpProfile,
    args: &McpServeArgs,
    source_dir: Option<PathBuf>,
    workspace_cache: Option<mcp_server::WorkspaceCacheLayout>,
    onec_password: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let source_dir = require_workspace_broker(profile, source_dir)?;
    let workspace_cache = workspace_cache.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "workspace broker requires a cache layout")
    })?;

    let topology_fp = mcp_server::broker::workspace_topology_fingerprint(&source_dir);
    let embedding_prefixes = resolve_embedding_prefixes(Some(&source_dir))?;
    let key = mcp_server::broker::BackendKey::new(
        &source_dir,
        workspace_cache.root(),
        profile,
        backend_config_fingerprint(&embedding_prefixes, &source_dir),
        topology_fp,
        mcp_server::contract::effective_opt_in(profile, &args.enable_tools),
    );

    // Credentials travel via env rather than argv, while source/cache and the frozen
    // topology are propagated exactly so proxy and daemon derive one backend key.
    let cmd = daemon_command(
        &env::current_exe()?,
        &source_dir,
        &workspace_cache,
        args,
        topology_fp,
        &embedding_prefixes,
    );

    tracing::info!(?source_dir, "Starting MCP broker proxy");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = rt.block_on(mcp_server::broker::proxy::connect_or_launch(key, cmd));
    // A completed relay can leave Tokio's uncancellable stdin read blocking runtime drop.
    rt.shutdown_timeout(Duration::from_secs(2));

    use mcp_server::broker::proxy::ProxyOutcome;
    match result? {
        ProxyOutcome::Served => Ok(()),
        // Connect-phase failure only: no client bytes were relayed yet, so fall back to
        // serving directly over stdio (the stdio server answers the pending `initialize`
        // cleanly). A relay-phase failure surfaced as `Err` above and is fatal — we must
        // not re-serve on a half-consumed stdin.
        ProxyOutcome::Unavailable(e) => {
            tracing::warn!(error = %e, "broker backend unavailable; serving directly over stdio");
            run_mcp_server(
                profile,
                ServerInputs {
                    source_dir: Some(source_dir),
                    workspace_cache: Some(workspace_cache),
                    onec_url: args.onec_url.clone(),
                    onec_user: args.onec_user.clone(),
                    onec_password: onec_password.to_owned(),
                    // The same set the proxy validated: a client that failed to reach the
                    // daemon must not be served a different surface than one that did.
                    enable_tools: args.enable_tools.clone(),
                },
            )
        }
    }
}

/// Required broker proxy for supervisor-owned lifecycle. It neither constructs a
/// daemon command nor has a direct-stdio fallback.
fn run_mcp_broker_required(
    profile: mcp_server::McpProfile,
    args: &McpServeArgs,
    source_dir: Option<PathBuf>,
    workspace_cache: Option<mcp_server::WorkspaceCacheLayout>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let source_dir = require_workspace_broker(profile, source_dir)?;
    let workspace_cache = workspace_cache.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "required workspace broker requires a cache layout",
        )
    })?;
    let expected_pid = args.backend_pid.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "--mode broker-required requires --backend-pid")
    })?;
    let embedding_prefixes = resolve_embedding_prefixes(Some(&source_dir))?;
    let key = mcp_server::broker::BackendKey::new(
        &source_dir,
        workspace_cache.root(),
        profile,
        backend_config_fingerprint(&embedding_prefixes, &source_dir),
        mcp_server::broker::workspace_topology_fingerprint(&source_dir),
        mcp_server::contract::effective_opt_in(profile, &args.enable_tools),
    );

    tracing::info!(?source_dir, expected_pid, "Starting required MCP broker proxy");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = rt.block_on(mcp_server::broker::proxy::connect_required(key, expected_pid));
    rt.shutdown_timeout(Duration::from_secs(2));
    result?;
    Ok(())
}

/// The shared backend: build the resident state once and serve every connecting
/// proxy from it until idle.
fn run_mcp_daemon(
    profile: mcp_server::McpProfile,
    inputs: ServerInputs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let source_dir = require_workspace_broker(profile, inputs.source_dir.clone())?;
    let workspace_cache = inputs.workspace_cache.clone().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "workspace daemon requires a cache layout")
    })?;

    // Prefer the frozen identity the spawning proxy passed (see the env write in
    // `run_mcp_broker`); a directly-launched daemon derives its own.
    let topology_fp = env::var(mcp_server::broker::TOPOLOGY_FP_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| mcp_server::broker::workspace_topology_fingerprint(&source_dir));
    let embedding_prefixes = resolve_embedding_prefixes(Some(&source_dir))?;
    let key = mcp_server::broker::BackendKey::new(
        &source_dir,
        workspace_cache.root(),
        profile,
        backend_config_fingerprint(&embedding_prefixes, &source_dir),
        topology_fp,
        mcp_server::contract::effective_opt_in(profile, &inputs.enable_tools),
    );

    // Build only after the daemon wins the bind, so a race loser never starts a
    // competing workspace build against the same per-project databases.
    let build = move || {
        build_server(
            profile,
            ServerInputs {
                source_dir: Some(source_dir),
                workspace_cache: Some(workspace_cache),
                ..inputs
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))
    };

    tracing::info!("Starting MCP broker backend (daemon)");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = rt.block_on(mcp_server::broker::daemon::run(
        build,
        key,
        broker_orphan_grace(),
        broker_idle_ttl(),
    ));
    drop(rt);
    result?;
    Ok(())
}

/// Broker/daemon modes serve the heavy workspace backend and key on its source dir.
fn require_workspace_broker(
    profile: mcp_server::McpProfile,
    source_dir: Option<PathBuf>,
) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    if !matches!(profile, mcp_server::McpProfile::Workspace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "broker/daemon modes require --profile workspace",
        )
        .into());
    }
    source_dir.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "broker/daemon modes require --source-dir")
            .into()
    })
}

/// Grace window for a backend that has never served real MCP traffic and has no live
/// connections — the launching proxy died before its first request, or only liveness
/// probes connected. It bounds how long such a never-used backend lingers before giving
/// up; a backend that *has* served traffic uses the longer [`broker_idle_ttl`] instead.
/// Default 30s; override via `BSL_MCP_ORPHAN_GRACE_SECS`.
fn broker_orphan_grace() -> Duration {
    let secs =
        env::var("BSL_MCP_ORPHAN_GRACE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
    Duration::from_secs(secs)
}

/// How long a backend that has served real MCP traffic stays warm after its last session
/// disconnects, so an editor that restarts or cycles its MCP link (opencode, Zed, …)
/// reconnects to the resident state instead of paying the multi-second cold rebuild.
/// Default 300s (5min), kept modest because a warm workspace backend is memory-heavy and
/// each project keeps its own; override via `BSL_MCP_IDLE_TTL_SECS` (legacy
/// `BSL_MCP_IDLE_SECS` still honored).
fn broker_idle_ttl() -> Duration {
    let secs = env::var("BSL_MCP_IDLE_TTL_SECS")
        .or_else(|_| env::var("BSL_MCP_IDLE_SECS"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    Duration::from_secs(secs)
}

fn run_mcp_install(args: McpInstallArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    use bsl_analyzer::mcp_install::{
        self, default_server_name, InstallPreset, InstallRequest, InstallScope, InstallTarget,
        InstallTargetSelector,
    };

    let project_dir = env::current_dir()?;
    let env = args.env.into_iter().collect::<BTreeMap<_, _>>();
    let password = decode_password(&args.onec_password);
    let source_dir = args.source_dir.unwrap_or_else(|| project_dir.clone());
    let name = args.name;

    let target = match args.target {
        InstallTargetCli::Codex => InstallTargetSelector::One(InstallTarget::Codex),
        InstallTargetCli::Gemini => InstallTargetSelector::One(InstallTarget::Gemini),
        InstallTargetCli::Claude => InstallTargetSelector::One(InstallTarget::Claude),
        InstallTargetCli::Cursor => InstallTargetSelector::One(InstallTarget::Cursor),
        InstallTargetCli::All => InstallTargetSelector::All,
    };

    if !args.enable_tools.is_empty() {
        if matches!(args.preset, InstallPresetCli::Recommended) {
            // The recommended set is what a build vouches for unattended; an opt-in tool
            // is a decision of whoever installs, and it has to be made against a named
            // profile rather than spread over both servers this preset writes.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--enable-tool is not used with '--preset recommended'; install \
                 '--preset workspace' or '--preset reference' to enable a tool",
            )
            .into());
        }
        let profile = match args.preset {
            InstallPresetCli::Workspace | InstallPresetCli::Recommended => {
                mcp_server::McpProfile::Workspace
            }
            InstallPresetCli::Reference => mcp_server::McpProfile::Reference,
        };
        mcp_server::contract::validate_enabled_tools(profile, &args.enable_tools)
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    }
    let enable_tools = args.enable_tools;

    let requests = match args.preset {
        InstallPresetCli::Workspace => vec![InstallRequest {
            target,
            scope: resolve_scope(args.scope, InstallScope::Project)?,
            preset: InstallPreset::Workspace,
            name: name.unwrap_or_else(|| default_server_name(InstallPreset::Workspace).to_owned()),
            project_dir: project_dir.clone(),
            source_dir,
            onec_url: args.onec_url,
            onec_user: args.onec_user,
            onec_password: password,
            env,
            enable_tools,
            force: args.force,
            dry_run: args.dry_run,
        }],
        InstallPresetCli::Reference => vec![InstallRequest {
            target,
            scope: resolve_scope(args.scope, InstallScope::User)?,
            preset: InstallPreset::Reference,
            name: name.unwrap_or_else(|| default_server_name(InstallPreset::Reference).to_owned()),
            project_dir: project_dir.clone(),
            source_dir,
            onec_url: args.onec_url,
            onec_user: args.onec_user,
            onec_password: password,
            env,
            enable_tools,
            force: args.force,
            dry_run: args.dry_run,
        }],
        InstallPresetCli::Recommended => {
            if args.scope.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--scope is not used with '--preset recommended'; it installs 'reference:user' and 'workspace:project'",
                )
                .into());
            }

            let (reference_name, workspace_name) = resolve_recommended_names(name.as_deref())?;

            vec![
                InstallRequest {
                    target,
                    scope: InstallScope::User,
                    preset: InstallPreset::Reference,
                    name: reference_name,
                    project_dir: project_dir.clone(),
                    source_dir: source_dir.clone(),
                    onec_url: args.onec_url.clone(),
                    onec_user: args.onec_user.clone(),
                    onec_password: password.clone(),
                    env: env.clone(),
                    enable_tools: Vec::new(),
                    force: args.force,
                    dry_run: args.dry_run,
                },
                InstallRequest {
                    target,
                    scope: InstallScope::Project,
                    preset: InstallPreset::Workspace,
                    name: workspace_name,
                    project_dir,
                    source_dir,
                    onec_url: args.onec_url,
                    onec_user: args.onec_user,
                    onec_password: password,
                    env,
                    enable_tools: Vec::new(),
                    force: args.force,
                    dry_run: args.dry_run,
                },
            ]
        }
    };

    let result = match mcp_install::install_many(requests) {
        Ok(result) => result,
        Err(err) => {
            eprintln!("{}", format_install_error(&err));
            std::process::exit(2);
        }
    };

    for entry in result.entries {
        println!(
            "[{}:{}] {} -> {}",
            entry.target,
            entry.scope,
            status_label(entry.status),
            entry.location
        );
        if !entry.detail.is_empty() {
            println!("{}", entry.detail);
        }
    }

    Ok(())
}

fn run_mcp_server(
    profile: mcp_server::McpProfile,
    inputs: ServerInputs,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing::info!(
        ?profile,
        source_dir = ?inputs.source_dir,
        onec_url = ?inputs.onec_url,
        "Starting MCP server (stdio)"
    );

    let server = build_server(profile, inputs)?;
    let shutdown_guard = server.clone();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let serve_result = rt.block_on(mcp_server::serve_stdio(server));

    shutdown_guard.shutdown();
    drop(shutdown_guard);
    // Tokio stdin uses an uncancellable blocking read, so an open MCP client's stdin
    // must not hold the process in Runtime::drop after the transport has closed.
    rt.shutdown_timeout(Duration::from_secs(2));

    serve_result?;
    Ok(())
}

fn run_mcp_http(
    profile: mcp_server::McpProfile,
    inputs: ServerInputs,
    options: HttpServeOptions,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut inputs = inputs;
    inputs.source_dir = inputs
        .source_dir
        .map(|path| {
            path.canonicalize().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to canonicalize --source-dir {}: {error}", path.display()),
                )
            })
        })
        .transpose()?;
    let source_dir = inputs.source_dir.clone();
    let requested_address = SocketAddr::new(options.host, options.port);
    let mut process_record =
        ProcessRecordGuard::acquire(profile, source_dir.clone(), requested_address)?;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let listener =
        rt.block_on(tokio::net::TcpListener::bind(requested_address)).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to bind HTTP MCP listener on {requested_address}: {error}"),
            )
        })?;
    let actual_address = listener.local_addr()?;
    process_record.write_bound_process(actual_address)?;

    let server = build_server(profile, inputs)?;
    let shutdown_guard = server.clone();
    let serve_result = match process_record.mark_running() {
        Ok(()) => {
            if !actual_address.ip().is_loopback() {
                tracing::warn!(
                    %actual_address,
                    "MCP HTTP is exposed beyond loopback without authentication or TLS"
                );
            }
            tracing::info!(
                %actual_address,
                ?profile,
                allowed_hosts = ?options.allowed_hosts,
                "Starting MCP server (HTTP)"
            );

            let cancellation = tokio_util::sync::CancellationToken::new();
            rt.block_on(serve_http_until_signal(
                listener,
                server,
                profile,
                actual_address,
                options.allowed_hosts,
                cancellation,
                &mut process_record,
            ))
        }
        Err(error) => Err(error.into()),
    };

    drop(rt);
    shutdown_guard.shutdown();
    drop(shutdown_guard);

    let record_result = process_record.mark_stopped();
    drop(process_record);

    record_result?;
    serve_result?;
    Ok(())
}

async fn serve_http_until_signal(
    listener: tokio::net::TcpListener,
    server: mcp_server::McpServer,
    profile: mcp_server::McpProfile,
    address: SocketAddr,
    allowed_hosts: Vec<String>,
    cancellation: tokio_util::sync::CancellationToken,
    process_record: &mut ProcessRecordGuard,
) -> anyhow::Result<()> {
    let serve = mcp_server::serve_http(
        listener,
        server,
        profile,
        address,
        allowed_hosts,
        cancellation.clone(),
    );
    tokio::pin!(serve);

    tokio::select! {
        result = &mut serve => result,
        signal_result = shutdown_signal() => {
            let record_result = process_record.mark_stopping();
            cancellation.cancel();
            let serve_result = serve.await;
            record_result?;
            signal_result?;
            serve_result
        }
    }
}

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Build the MCP server (resident state + tool router) for a profile. Shared by the
/// stdio path and the broker backend so both construct identical state.
/// Everything a server is built from. These travel together through every serve mode, and
/// a mode that dropped one of them on the way would serve a surface or a connection the
/// launch did not ask for.
struct ServerInputs {
    source_dir: Option<PathBuf>,
    workspace_cache: Option<mcp_server::WorkspaceCacheLayout>,
    onec_url: Option<String>,
    onec_user: String,
    onec_password: String,
    enable_tools: Vec<String>,
}

/// What a backend must agree on beyond its paths: the embedding setup and the
/// platform help source. A backend fixes its help snapshot for life, so a client
/// configured for another source must reach a fresh backend, not the old one.
fn backend_config_fingerprint(
    embedding_prefixes: &mcp_server::EmbeddingPrefixes,
    source_dir: &Path,
) -> u64 {
    let embedding =
        mcp_server::broker::embedding_config_fingerprint_with_prefixes(embedding_prefixes);
    let config = project_model::ProjectConfig::load(source_dir).ok().flatten();
    let help = platform_help::requested_source(config.as_ref(), source_dir)
        // `Debug` names the variant: a path and a URL with the same text are
        // different sources.
        .map(|request| format!("{request:?}"))
        .unwrap_or_else(|reason| format!("invalid: {reason}"));
    let mut hasher = blake3::Hasher::new();
    hasher.update(&embedding.to_le_bytes());
    hasher.update(help.as_bytes());
    let digest = hasher.finalize();
    u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("8 bytes"))
}

fn build_server(
    profile: mcp_server::McpProfile,
    inputs: ServerInputs,
) -> Result<mcp_server::McpServer, Box<dyn Error + Send + Sync>> {
    let ServerInputs {
        source_dir,
        workspace_cache,
        onec_url,
        onec_user,
        onec_password,
        enable_tools,
    } = inputs;
    super::logging::activate_vector_journal(source_dir.as_deref());
    // Every process that serves MCP builds its server here — stdio, daemon, HTTP and
    // the broker's direct fallback — so the help source is fixed here, before the
    // server indexes or answers anything.
    match source_dir.as_deref() {
        Some(root) => bsl_analyzer::help_bootstrap::bootstrap_for_root(root, None),
        None => {
            platform_help::bootstrap(None, &env::current_dir().unwrap_or_default());
        }
    }
    let state = match profile {
        mcp_server::McpProfile::Workspace => {
            let source_dir = source_dir.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workspace profile requires --source-dir",
                )
            })?;
            let source_dir = source_dir.canonicalize().unwrap_or(source_dir);
            let workspace_cache = match workspace_cache {
                Some(cache) => cache,
                None => {
                    let project = mcp_server::project::at(&source_dir)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    mcp_server::WorkspaceCacheLayout::for_project(
                        &project,
                        None,
                        &env::current_dir()?,
                        env::var(mcp_server::WORKSPACE_CACHE_SCOPE_ENV).ok().as_deref(),
                    )?
                }
            };
            let embedding_prefixes = resolve_embedding_prefixes(Some(&source_dir))?;
            let mut state = mcp_server::SharedState::workspace_with_cache_and_prefixes(
                source_dir,
                workspace_cache,
                Some(embedding_prefixes),
            )
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            if let Some(ref url) = onec_url {
                tracing::info!(%url, "Configuring 1C HTTP client");
                state.set_onec_client(onec_client::Client::new(url, &onec_user, &onec_password));
            }
            configure_named_onec_connections(&mut state)?;
            state.warm_start();
            state
        }
        mcp_server::McpProfile::Reference => mcp_server::SharedState::reference(source_dir),
    };

    Ok(mcp_server::McpServer::with_gate(
        profile,
        state,
        &mcp_server::ToolGate::for_launch(profile, &enable_tools),
    ))
}

fn configure_named_onec_connections(
    state: &mut mcp_server::SharedState,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let Some(path) = env::var_os("BSL_ONEC_CONNECTIONS_FILE") else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let text = std::fs::read_to_string(&path)?;
    let config: OnecConnectionsFile = serde_json::from_str(&text)?;
    for (name, connection) in config.connections {
        if name.trim().is_empty() || connection.url.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "1C connection name and url must not be empty",
            )
            .into());
        }
        let user = if connection.user_env.is_empty() {
            String::new()
        } else {
            env::var(&connection.user_env).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing environment variable {}", connection.user_env),
                )
            })?
        };
        let password = if connection.password_env.is_empty() {
            String::new()
        } else {
            env::var(&connection.password_env).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing environment variable {}", connection.password_env),
                )
            })?
        };
        state.add_onec_connection(
            name,
            mcp_server::OnecConnection::new(
                onec_client::Client::new(&connection.url, &user, &password),
                connection.allow_execute,
            ),
        );
    }
    Ok(())
}

fn resolve_scope(
    scope: Option<InstallScopeCli>,
    default: bsl_analyzer::mcp_install::InstallScope,
) -> Result<bsl_analyzer::mcp_install::InstallScope, io::Error> {
    Ok(match scope {
        Some(InstallScopeCli::User) => bsl_analyzer::mcp_install::InstallScope::User,
        Some(InstallScopeCli::Project) => bsl_analyzer::mcp_install::InstallScope::Project,
        Some(InstallScopeCli::Local) => bsl_analyzer::mcp_install::InstallScope::Local,
        None => default,
    })
}

fn resolve_recommended_names(name: Option<&str>) -> Result<(String, String), io::Error> {
    match name {
        None => Ok((
            bsl_analyzer::mcp_install::default_server_name(
                bsl_analyzer::mcp_install::InstallPreset::Reference,
            )
            .to_owned(),
            bsl_analyzer::mcp_install::default_server_name(
                bsl_analyzer::mcp_install::InstallPreset::Workspace,
            )
            .to_owned(),
        )),
        Some(base) => {
            let base = base.trim();
            if base.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--name must not be empty",
                ));
            }
            Ok((format!("{base}-reference"), format!("{base}-workspace")))
        }
    }
}

fn format_install_error(err: &bsl_analyzer::mcp_install::InstallError) -> String {
    if let Some(hint) = err.hint() {
        format!("{err}\nHint: {hint}")
    } else {
        err.to_string()
    }
}

fn parse_env_pair(input: &str) -> Result<(String, String), String> {
    let Some((key, value)) = input.split_once('=') else {
        return Err("expected KEY=value".to_owned());
    };
    if key.is_empty() {
        return Err("environment variable name must not be empty".to_owned());
    }
    Ok((key.to_owned(), value.to_owned()))
}

fn status_label(status: bsl_analyzer::mcp_install::InstallStatus) -> &'static str {
    match status {
        bsl_analyzer::mcp_install::InstallStatus::Installed => "installed",
        bsl_analyzer::mcp_install::InstallStatus::Updated => "updated",
        bsl_analyzer::mcp_install::InstallStatus::DryRun => "dry-run",
    }
}

fn decode_password(password: &str) -> String {
    if let Some(encoded) = password.strip_prefix("base64:") {
        if let Some(decoded) = base64_decode(encoded) {
            if let Ok(s) = String::from_utf8(decoded) {
                return s;
            }
        }
    }
    password.to_owned()
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in input.as_bytes() {
        if b == b'=' || b == b'\n' || b == b'\r' {
            continue;
        }
        let val = TABLE.iter().position(|&c| c == b)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{
        backend_config_fingerprint, daemon_command, decode_password, frozen_embedding_prefixes,
        resolve_embedding_prefix_values, resolve_onec_password, resolve_serve_mode_with_override,
        resolve_token_profile_values, validate_backend_pid, validate_onec_settings,
        validate_serve_args, warn_on_wildcard_allowlist, HttpServeOptions, McpCommand,
        McpProfileCli, McpServeArgs, McpServeMode, ServeModeContext,
    };
    use clap::Parser;
    use std::io;
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;

    fn test_cache_layout(
        workspace: &std::path::Path,
        base: Option<&std::path::Path>,
    ) -> mcp_server::WorkspaceCacheLayout {
        let project = mcp_server::project::at(workspace).unwrap();
        mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            base,
            &std::env::current_dir().unwrap(),
            None,
        )
        .unwrap()
    }

    #[derive(Debug, Parser)]
    struct ServeCli {
        #[command(flatten)]
        args: McpServeArgs,
    }

    #[test]
    fn backend_key_follows_the_configured_platform_help_source() {
        let dir = tempfile::tempdir().unwrap();
        let prefixes = mcp_server::EmbeddingPrefixes {
            query: String::new(),
            document: String::new(),
            token_profile: None,
        };
        let config =
            |body: &str| std::fs::write(dir.path().join("bsl-analyzer.toml"), body).unwrap();

        config("");
        let default = backend_config_fingerprint(&prefixes, dir.path());
        config("[platform_help]\nsource = \"external\"\npath = \"a.json\"\n");
        let corpus_a = backend_config_fingerprint(&prefixes, dir.path());
        config("[platform_help]\nsource = \"external\"\npath = \"b.json\"\n");
        let corpus_b = backend_config_fingerprint(&prefixes, dir.path());
        assert_ne!(default, corpus_a);
        assert_ne!(corpus_a, corpus_b, "another help source must reach another backend");
        config("[platform_help]\nsource = \"external\"\npath = \"a.json\"\n");
        assert_eq!(backend_config_fingerprint(&prefixes, dir.path()), corpus_a);

        // A path and a URL spelled with the same text are different sources.
        config("[platform_help]\nsource = \"external\"\npath = \"/srv/help\"\n");
        let as_path = backend_config_fingerprint(&prefixes, dir.path());
        config("[platform_help]\nsource = \"external\"\nurl = \"/srv/help\"\n");
        assert_ne!(backend_config_fingerprint(&prefixes, dir.path()), as_path, "path is not url");
    }
    #[test]
    fn embedding_prefix_resolution_honors_project_empty_and_frozen_pair() {
        let config = project_model::SearchEmbeddingConfig {
            query_prefix: Some(String::new()),
            document_prefix: None,
            ..Default::default()
        };
        let resolved = resolve_embedding_prefix_values(
            Some(&config),
            Some("environment query"),
            Some("environment document"),
            None,
        );
        assert_eq!(resolved.query, "");
        assert_eq!(resolved.document, "environment document");

        let frozen = resolve_embedding_prefix_values(
            Some(&config),
            Some("environment query"),
            Some("environment document"),
            Some(("frozen query", "frozen document")),
        );
        assert_eq!(frozen.query, "frozen query");
        assert_eq!(frozen.document, "frozen document");
    }

    #[test]
    fn token_profile_resolution_honors_origins_and_rejects_partial_or_zero_profiles() {
        use std::ffi::OsString;

        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("tokenizer.json");
        std::fs::write(
            &artifact,
            r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"[UNK]":0,"document":1,"query":2,"extra":3,"hello":4},"unk_token":"[UNK]"}}"#,
        )
        .unwrap();
        let project_config = dir.path().join("bsl-analyzer.toml");
        std::fs::write(
            &project_config,
            "[search.baseline.embedding]\nmaxInputTokens = 8192\ntokenizerFile = \"tokenizer.json\"\ntokenizerSha256 = \"a58e529fb53e63963f904605eee152d54676591a555ca3072b51bdb680e993c9\"\n",
        )
        .unwrap();
        let project = project_model::Project::new(dir.path()).unwrap();
        let env = [
            Some(OsString::from("4096")),
            Some(OsString::from("elsewhere/tokenizer.json")),
            Some(OsString::from(
                "a58e529fb53e63963f904605eee152d54676591a555ca3072b51bdb680e993c9",
            )),
        ];
        let resolved =
            resolve_token_profile_values(Some(&project), Some(dir.path()), [None, None, None], env)
                .unwrap()
                .unwrap();
        assert_eq!(resolved.max_input_tokens, 8192);
        assert_eq!(resolved.tokenizer_file, artifact.canonicalize().unwrap());
        assert_eq!(
            resolved.tokenizer_sha256,
            "a58e529fb53e63963f904605eee152d54676591a555ca3072b51bdb680e993c9"
        );

        let environment_profile = resolve_token_profile_values(
            None,
            Some(dir.path()),
            [None, None, None],
            [
                Some(OsString::from("4096")),
                Some(OsString::from("tokenizer.json")),
                Some(OsString::from(
                    "a58e529fb53e63963f904605eee152d54676591a555ca3072b51bdb680e993c9",
                )),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(environment_profile.max_input_tokens, 4096);
        assert_eq!(environment_profile.tokenizer_file, artifact.canonicalize().unwrap());
        assert_eq!(
            environment_profile.tokenizer_sha256,
            "a58e529fb53e63963f904605eee152d54676591a555ca3072b51bdb680e993c9"
        );

        let partial = resolve_token_profile_values(
            None,
            Some(dir.path()),
            [None, None, None],
            [Some(OsString::from("8192")), None, None],
        )
        .unwrap_err();
        assert_eq!(partial.to_string(), "embedding_invalid_config");

        let zero = resolve_token_profile_values(
            None,
            Some(dir.path()),
            [None, None, None],
            [Some(OsString::from("0")), None, None],
        )
        .unwrap_err();
        assert_eq!(zero.to_string(), "embedding_invalid_config");

        let missing = resolve_token_profile_values(
            None,
            Some(dir.path()),
            [None, None, None],
            [
                Some(OsString::from("8192")),
                Some(OsString::from("missing-tokenizer.json")),
                Some(OsString::from("hash")),
            ],
        )
        .unwrap_err();
        assert_eq!(missing.to_string(), "embedding_invalid_config");
        assert!(!missing.to_string().contains("missing-tokenizer.json"));

        assert!(resolve_token_profile_values(
            None,
            Some(dir.path()),
            [None, None, None],
            [None, None, None]
        )
        .unwrap()
        .is_none());
    }

    /// The broker keys the backend on the profile it resolved and hands the daemon only the
    /// frozen environment. A daemon that rebuilt a different profile from that environment
    /// would bind under another key and serve with another token policy.
    #[test]
    fn daemon_rebuilds_the_broker_token_profile_from_its_frozen_environment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tokenizer.json"),
            r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"[UNK]":0,"document":1,"query":2,"extra":3,"hello":4},"unk_token":"[UNK]"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("bsl-analyzer.toml"),
            "[search.baseline.embedding]\nmaxInputTokens = 8192\ntokenizerFile = \"tokenizer.json\"\ntokenizerSha256 = \"a58e529fb53e63963f904605eee152d54676591a555ca3072b51bdb680e993c9\"\n",
        )
        .unwrap();
        let project = project_model::Project::new(dir.path()).unwrap();
        let broker = mcp_server::EmbeddingPrefixes {
            query: "query: ".to_owned(),
            document: "document: ".to_owned(),
            token_profile: resolve_token_profile_values(
                Some(&project),
                Some(dir.path()),
                [None, None, None],
                [None, None, None],
            )
            .unwrap(),
        };
        assert!(broker.token_profile.is_some(), "the fixture must configure a token profile");
        let source = dir.path().canonicalize().unwrap();
        let cache = test_cache_layout(&source, None);
        let command = daemon_command(
            PathBuf::from("bsl-analyzer").as_path(),
            &source,
            &cache,
            &serve_args(McpServeMode::Broker, None),
            42,
            &broker,
        );
        let frozen = |key: &str| {
            command
                .get_envs()
                .find(|(name, _)| *name == std::ffi::OsStr::new(key))
                .and_then(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        };

        let daemon = frozen_embedding_prefixes(
            &frozen(mcp_server::broker::EMBEDDING_QUERY_PREFIX_ENV).unwrap().to_string_lossy(),
            &frozen(mcp_server::broker::EMBEDDING_DOCUMENT_PREFIX_ENV).unwrap().to_string_lossy(),
            [
                frozen(mcp_server::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV),
                frozen(mcp_server::broker::EMBEDDING_TOKENIZER_FILE_ENV),
                frozen(mcp_server::broker::EMBEDDING_TOKENIZER_SHA256_ENV),
            ],
        )
        .unwrap();

        let profile =
            daemon.token_profile.as_ref().expect("the daemon must keep the token profile");
        assert!(profile.token_policy.is_some(), "the daemon must load the frozen tokenizer");
        let expected = broker.token_profile.as_ref().unwrap();
        assert_eq!(profile.max_input_tokens, expected.max_input_tokens);
        assert_eq!(profile.tokenizer_file, expected.tokenizer_file);
        assert_eq!(profile.tokenizer_sha256, expected.tokenizer_sha256);
        assert_eq!(
            mcp_server::broker::embedding_config_fingerprint_with_prefixes(&daemon),
            mcp_server::broker::embedding_config_fingerprint_with_prefixes(&broker),
        );

        let without_profile =
            mcp_server::EmbeddingPrefixes { token_profile: None, ..broker.clone() };
        let command = daemon_command(
            PathBuf::from("bsl-analyzer").as_path(),
            &source,
            &cache,
            &serve_args(McpServeMode::Broker, None),
            42,
            &without_profile,
        );
        assert!(command.get_envs().any(|(name, value)| name
            == std::ffi::OsStr::new(mcp_server::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV)
            && value.is_none()));
        assert!(frozen_embedding_prefixes("query: ", "document: ", [None, None, None])
            .unwrap()
            .token_profile
            .is_none());
    }

    #[test]
    fn workspace_profile_defaults_to_broker() {
        // Production passes `platform_default_broker: true` on every platform, so the
        // workspace profile with no override resolves to the broker everywhere.
        let mode = resolve_serve_mode_with_override(
            McpServeMode::Stdio,
            mcp_server::McpProfile::Workspace,
            ServeModeContext { broker_override: None, platform_default_broker: true },
        );

        assert!(matches!(mode, McpServeMode::Broker));
    }

    #[test]
    fn workspace_profile_defaults_to_stdio_when_platform_default_is_off() {
        // The platform default is an injected seam; with it off, the workspace profile
        // stays on stdio. No production platform sets this today, but the precedence
        // must remain correct if one ever does.
        let mode = resolve_serve_mode_with_override(
            McpServeMode::Stdio,
            mcp_server::McpProfile::Workspace,
            ServeModeContext { broker_override: None, platform_default_broker: false },
        );

        assert!(matches!(mode, McpServeMode::Stdio));
    }

    #[test]
    fn broker_env_override_off_forces_stdio_even_when_platform_defaults_to_broker() {
        let mode = resolve_serve_mode_with_override(
            McpServeMode::Stdio,
            mcp_server::McpProfile::Workspace,
            ServeModeContext { broker_override: Some(false), platform_default_broker: true },
        );

        assert!(matches!(mode, McpServeMode::Stdio));
    }

    #[test]
    fn broker_env_override_on_forces_broker_even_when_platform_default_is_off() {
        let mode = resolve_serve_mode_with_override(
            McpServeMode::Stdio,
            mcp_server::McpProfile::Workspace,
            ServeModeContext { broker_override: Some(true), platform_default_broker: false },
        );

        assert!(matches!(mode, McpServeMode::Broker));
    }

    #[test]
    fn reference_profile_never_defaults_to_broker() {
        let mode = resolve_serve_mode_with_override(
            McpServeMode::Stdio,
            mcp_server::McpProfile::Reference,
            ServeModeContext { broker_override: Some(true), platform_default_broker: true },
        );

        assert!(matches!(mode, McpServeMode::Stdio));
    }

    #[test]
    fn explicit_mode_flag_wins_over_defaults() {
        // The re-exec'd daemon passes `--mode daemon`; an explicit flag must survive
        // regardless of profile or platform default.
        let mode = resolve_serve_mode_with_override(
            McpServeMode::Daemon,
            mcp_server::McpProfile::Workspace,
            ServeModeContext { broker_override: Some(false), platform_default_broker: false },
        );

        assert!(matches!(mode, McpServeMode::Daemon));
    }

    #[test]
    fn explicit_http_mode_wins_over_broker_defaults() {
        let mode = resolve_serve_mode_with_override(
            McpServeMode::Http,
            mcp_server::McpProfile::Workspace,
            ServeModeContext { broker_override: Some(true), platform_default_broker: true },
        );

        assert!(matches!(mode, McpServeMode::Http));
    }

    #[test]
    fn http_mode_requires_a_port() {
        let args = serve_args(McpServeMode::Http, None);

        let err = validate_serve_args(&args).expect_err("HTTP without --port must be rejected");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("--port"));
    }

    #[test]
    fn user_supplied_port_zero_is_rejected() {
        let args = serve_args(McpServeMode::Http, Some(0));

        let err = validate_serve_args(&args).expect_err("port zero is reserved for internal tests");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("1..=65535"));
    }

    #[test]
    fn http_without_explicit_host_uses_ipv4_loopback() {
        let args = serve_args(McpServeMode::Http, Some(8021));

        let options = validate_serve_args(&args)
            .expect("valid HTTP options")
            .expect("HTTP mode returns HTTP options");

        assert_eq!(options.host, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(options.port, 8021);
        assert!(options.allowed_hosts.is_empty());
    }

    #[test]
    fn http_cli_parses_config_path() {
        let cli = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--mode",
            "http",
            "--config",
            "mcp-http.toml",
        ])
        .expect("valid HTTP command line");

        assert_eq!(cli.args.config, Some(PathBuf::from("mcp-http.toml")));
    }

    #[test]
    fn http_cli_parses_allowed_hosts_array() {
        let cli = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--mode",
            "http",
            "--host",
            "0.0.0.0",
            "--port",
            "8021",
            "--allowed_hosts",
            "first.example.test",
            "second.example.test",
        ])
        .expect("valid HTTP command line");

        assert_eq!(cli.args.host, Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert_eq!(cli.args.port, Some(8021));
        assert_eq!(cli.args.allowed_hosts, ["first.example.test", "second.example.test"]);
    }

    #[test]
    fn http_cli_rejects_invalid_ip_and_out_of_range_port() {
        let invalid_ip = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--mode",
            "http",
            "--host",
            "not-an-ip",
            "--port",
            "8021",
        ])
        .expect_err("--host must be parsed as IpAddr");
        assert_eq!(invalid_ip.kind(), clap::error::ErrorKind::ValueValidation);

        let oversized_port = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--mode",
            "http",
            "--port",
            "65536",
        ])
        .expect_err("ports above u16::MAX must be rejected");
        assert_eq!(oversized_port.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn http_config_loads_allowed_hosts_array() {
        let mut args = serve_args(McpServeMode::Http, None);
        args.config =
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-http.toml"));
        let options = validate_serve_args(&args)
            .expect("valid HTTP TOML")
            .expect("HTTP mode returns options");

        assert_eq!(options.host, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(options.port, 8021);
        assert_eq!(
            options.allowed_hosts,
            ["localhost", "127.0.0.1", "::1", "10.9.9.10", "mcp-server", "mcp-server.local",]
        );
    }

    #[test]
    fn http_config_conflicts_with_individual_options() {
        let conflict = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--mode",
            "http",
            "--config",
            "mcp-http.toml",
            "--port",
            "8021",
        ])
        .expect_err("--config and individual HTTP options are alternatives");

        assert_eq!(conflict.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    fn http_options_from_toml(contents: &str) -> (tempfile::TempDir, McpServeArgs) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp-http.toml");
        std::fs::write(&path, contents).expect("write config");
        let mut args = serve_args(McpServeMode::Http, None);
        args.config = Some(path);
        (dir, args)
    }

    /// `--config` is advertised as an alternative to the three flags, so a file has to accept
    /// everything the flags accept. A required `host` would break the one migration the file
    /// exists for: moving a working loopback run into a scheduler task.
    #[test]
    fn http_config_omitting_host_and_allowlist_matches_the_flags() {
        let (_dir, args) = http_options_from_toml("port = 8021\n");
        let from_file = validate_serve_args(&args).expect("port alone is a valid file");

        let mut flags = serve_args(McpServeMode::Http, Some(8021));
        flags.allowed_hosts.clear();
        let from_flags = validate_serve_args(&flags).expect("port alone is a valid command line");

        assert_eq!(from_file, from_flags);
        assert_eq!(
            from_file,
            Some(HttpServeOptions {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 8021,
                allowed_hosts: Vec::new(),
            })
        );
    }

    #[test]
    fn http_config_omitting_host_keeps_the_named_allowlist() {
        let (_dir, args) =
            http_options_from_toml("port = 8021\nallowed_hosts = [\"mcp.example.test\"]\n");
        let options = validate_serve_args(&args)
            .expect("a named allowlist without a host is valid")
            .expect("HTTP mode returns options");

        assert_eq!(options.host, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(options.allowed_hosts, ["mcp.example.test"]);
    }

    #[test]
    fn http_config_rejects_port_zero() {
        let (_dir, args) = http_options_from_toml("port = 0\n");
        let err = validate_serve_args(&args).expect_err("port 0 binds nothing");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("1..=65535"), "{err}");
    }

    #[test]
    fn http_config_rejects_a_blank_allowlist_entry() {
        let (_dir, args) =
            http_options_from_toml("port = 8021\nallowed_hosts = [\"mcp.example.test\", \" \"]\n");
        let err = validate_serve_args(&args)
            .expect_err("a blank entry matches no Host and must be rejected");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("allowed_hosts"), "{err}");
    }

    #[test]
    fn http_config_rejects_a_non_loopback_host_without_an_allowlist() {
        let (_dir, args) = http_options_from_toml("host = \"0.0.0.0\"\nport = 8021\n");
        let err = validate_serve_args(&args)
            .expect_err("a non-loopback bind without an allowlist is unsafe");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("allowed_hosts"), "{err}");
    }

    /// A misspelled key is a typo in an operator's file, and accepting it silently would start
    /// a server bound somewhere other than the one place the file names.
    #[test]
    fn http_config_rejects_an_unknown_field() {
        let (_dir, args) = http_options_from_toml("port = 8021\nhosts = [\"mcp.example.test\"]\n");
        let err = validate_serve_args(&args).expect_err("an unknown key is a typo");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("hosts"), "{err}");
    }

    #[test]
    fn http_config_names_the_path_it_could_not_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("absent.toml");
        let mut args = serve_args(McpServeMode::Http, None);
        args.config = Some(missing.clone());

        let err = validate_serve_args(&args).expect_err("an unreadable file cannot be served");

        assert!(err.to_string().contains(&missing.display().to_string()), "{err}");
    }

    #[test]
    fn existing_stdio_cli_keeps_parsing_without_http_options() {
        let cli = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--mode",
            "stdio",
        ])
        .expect("the existing stdio command line must remain valid");

        assert!(matches!(cli.args.mode, McpServeMode::Stdio));
        assert!(cli.args.host.is_none());
        assert!(cli.args.port.is_none());
        assert!(cli.args.allowed_hosts.is_empty());
        assert!(cli.args.config.is_none());
    }

    #[test]
    fn required_broker_cli_requires_the_supervised_backend_pid() {
        let missing = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--mode",
            "broker-required",
        ])
        .expect_err("required broker without a supervised PID must fail");
        assert_eq!(missing.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(missing.to_string().contains("--backend-pid"));

        let cli = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--mode",
            "broker-required",
            "--backend-pid",
            "42",
        ])
        .expect("required broker with a supervised PID");
        assert!(matches!(cli.args.mode, McpServeMode::BrokerRequired));
        assert_eq!(cli.args.backend_pid, Some(42));
    }

    #[test]
    fn backend_pid_is_rejected_outside_required_broker_mode() {
        // Every mode but the one that needs it, http included: http takes its own validation
        // path, and a check written for the others alone silently skips it.
        for mode in
            [McpServeMode::Stdio, McpServeMode::Broker, McpServeMode::Daemon, McpServeMode::Http]
        {
            let mut args = serve_args(mode, None);
            args.backend_pid = Some(42);

            let error = validate_serve_args(&args)
                .err()
                .unwrap_or_else(|| panic!("{mode:?} must reject a peer pin"));
            assert!(error.to_string().contains("--backend-pid"), "{mode:?}: {error}");
        }
    }

    /// A setting the proxy cannot apply must be refused, not dropped: the daemon it connects to
    /// was built against one infobase, and a client naming another would have its `execute` run
    /// somewhere it never named.
    #[test]
    fn one_c_settings_are_refused_by_the_supervised_proxy() {
        for (url, user, password) in
            [(Some("http://base-b"), "", ""), (None, "user", ""), (None, "", "secret")]
        {
            let error = validate_onec_settings(McpServeMode::BrokerRequired, url, user, password)
                .expect_err("the supervised proxy cannot apply 1C settings");
            assert!(error.to_string().contains("--onec-url"), "{error}");
        }

        validate_onec_settings(McpServeMode::BrokerRequired, None, "", "")
            .expect("no settings, nothing to drop");

        // The credential has a second way in, and the mode drops it just as silently. The seam is
        // what matters: resolution first, then the gate on what resolution produced.
        let from_env = resolve_onec_password("", Some("secret-from-the-environment".to_owned()));
        assert_eq!(from_env, "secret-from-the-environment");
        assert!(
            validate_onec_settings(McpServeMode::BrokerRequired, None, "", &from_env).is_err(),
            "a credential from the environment is dropped just the same"
        );
        assert_eq!(
            resolve_onec_password("from-the-flag", Some("ignored".to_owned())),
            "from-the-flag"
        );

        // An encoding of no password is no password: the gate refuses a credential that would be
        // dropped, and there is nothing here to drop. Read through the whole argument path, which
        // a platform whose peer credentials carry no PID refuses before the password is resolved.
        if mcp_server::broker::peer_pid_available() {
            let mut args = serve_args(McpServeMode::BrokerRequired, None);
            args.backend_pid = Some(42);
            args.onec_password = "base64:".to_owned();
            validate_serve_args(&args).expect("an encoded empty password is still no password");
        }
        assert!(
            validate_onec_settings(
                McpServeMode::BrokerRequired,
                None,
                "",
                &decode_password("base64:")
            )
            .is_ok(),
            "an encoded empty password reaches the gate as no password"
        );
        for mode in [McpServeMode::Stdio, McpServeMode::Broker, McpServeMode::Daemon] {
            validate_onec_settings(mode, Some("http://base-a"), "user", "secret")
                .unwrap_or_else(|e| panic!("{mode:?} applies 1C settings itself: {e}"));
        }
    }

    #[test]
    fn required_broker_is_refused_where_peer_credentials_carry_no_pid() {
        let refused = validate_backend_pid(McpServeMode::BrokerRequired, Some(42), false)
            .expect_err("a platform without peer PIDs cannot identify the supervised backend");
        assert_eq!(refused.kind(), io::ErrorKind::Unsupported);
        assert!(refused.to_string().contains("--mode broker-required"), "{refused}");

        validate_backend_pid(McpServeMode::BrokerRequired, Some(42), true)
            .expect("a platform with peer PIDs serves the supervised mode");
        assert!(validate_backend_pid(McpServeMode::BrokerRequired, Some(0), true).is_err());
    }

    #[test]
    fn http_only_options_are_rejected_in_existing_modes() {
        for mode in [McpServeMode::Stdio, McpServeMode::Broker, McpServeMode::Daemon] {
            let mut args = serve_args(mode, None);
            args.host = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
            assert!(
                validate_serve_args(&args).unwrap_err().to_string().contains("--host"),
                "{mode:?} must reject --host"
            );

            args.host = None;
            args.port = Some(8021);
            assert!(
                validate_serve_args(&args).unwrap_err().to_string().contains("--port"),
                "{mode:?} must reject --port"
            );

            args.port = None;
            args.allowed_hosts = vec!["mcp.example.test".to_owned()];
            assert!(
                validate_serve_args(&args).unwrap_err().to_string().contains("--allowed_hosts"),
                "{mode:?} must reject --allowed_hosts"
            );

            args.allowed_hosts.clear();
            args.config = Some(PathBuf::from("mcp-http.toml"));
            assert!(
                validate_serve_args(&args).unwrap_err().to_string().contains("--config"),
                "{mode:?} must reject --config"
            );
        }
    }

    /// Captures the warnings a closure emits. The scoped default owns only this thread,
    /// which is exactly the reach needed: the emission under test runs inline.
    fn warnings_during<T>(f: impl FnOnce() -> T) -> (T, String) {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl tracing_subscriber::fmt::MakeWriter<'_> for Buf {
            type Writer = Buf;
            fn make_writer(&self) -> Buf {
                self.clone()
            }
        }
        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(buf.clone())
            .without_time()
            .finish();
        let result = tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.0.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A wildcard entry passes validation — it is non-empty and satisfies the non-loopback
    /// rule — while authorizing nothing a client sends. Silence there is what turns a
    /// typo into a server that starts cleanly and refuses everyone.
    #[test]
    fn a_wildcard_allowlist_entry_is_warned_about_and_a_real_one_is_not() {
        let wildcard = vec!["0.0.0.0".to_owned(), "mcp.example.test".to_owned()];
        let (_, warned) = warnings_during(|| warn_on_wildcard_allowlist(&wildcard));
        assert!(warned.contains("0.0.0.0"), "{warned}");
        assert!(warned.contains("allowed_hosts"), "{warned}");
        assert!(!warned.contains("mcp.example.test"), "only the wildcard is named: {warned}");

        let real = vec!["mcp.example.test".to_owned(), "127.0.0.1".to_owned()];
        let (_, quiet) = warnings_during(|| warn_on_wildcard_allowlist(&real));
        assert!(quiet.is_empty(), "a usable allowlist warns about nothing: {quiet}");
    }

    #[test]
    fn non_loopback_http_host_requires_an_allowed_host() {
        let mut args = serve_args(McpServeMode::Http, Some(8021));
        args.host = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));

        let err =
            validate_serve_args(&args).expect_err("non-loopback bind without allowlist is unsafe");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("allowed_hosts"));
    }

    #[test]
    fn a_blank_allowed_host_does_not_satisfy_the_allowlist() {
        let mut args = serve_args(McpServeMode::Http, Some(8021));
        args.host = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        args.allowed_hosts = vec!["   ".to_owned()];

        let err = validate_serve_args(&args)
            .expect_err("a blank allowlist entry matches no Host and must be rejected");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("allowed_hosts"));
    }

    #[test]
    fn non_loopback_http_host_accepts_an_allowed_host() {
        let mut args = serve_args(McpServeMode::Http, Some(8021));
        args.host = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        args.allowed_hosts = vec!["mcp.example.test".to_owned()];

        let options = validate_serve_args(&args)
            .expect("an explicit allowlist permits a non-loopback bind")
            .expect("HTTP mode returns HTTP options");

        assert_eq!(options.host, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(options.allowed_hosts, ["mcp.example.test"]);
    }

    #[test]
    fn http_accepts_the_maximum_user_port() {
        let args = serve_args(McpServeMode::Http, Some(u16::MAX));

        let options = validate_serve_args(&args)
            .expect("65535 is a valid user port")
            .expect("HTTP mode returns HTTP options");

        assert_eq!(options.port, u16::MAX);
    }

    #[test]
    fn workspace_profile_still_requires_source_dir() {
        let err = ServeCli::try_parse_from(["serve", "--profile", "workspace"])
            .expect_err("workspace must keep requiring --source-dir");

        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(err.to_string().contains("--source-dir"));
    }

    #[test]
    fn workspace_cli_accepts_cache_dir() {
        let cli = ServeCli::try_parse_from([
            "serve",
            "--profile",
            "workspace",
            "--source-dir",
            ".",
            "--cache-dir",
            "../кеш с пробелом",
        ])
        .expect("workspace cache override must parse");

        assert_eq!(cli.args.cache_dir, Some(PathBuf::from("../кеш с пробелом")));
    }

    /// A name no profile declares stops the start rather than being dropped: a typo in a
    /// client config would otherwise look like a working launch that silently serves less.
    #[test]
    fn enable_tool_rejects_unknown_name_with_nonempty_list() {
        let mut args = serve_args(McpServeMode::Stdio, None);
        args.enable_tools = vec!["symbol_inf0".to_owned()];

        let err = validate_serve_args(&args).expect_err("an undeclared tool name must not start");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let message = err.to_string();
        assert!(message.contains("symbol_inf0"), "refusal does not name the input: {message}");
        // The list is generated from the declaration, so it can never be empty — an error
        // that named nothing would leave the caller no way to correct the config.
        for name in mcp_server::contract::declared_tools(mcp_server::McpProfile::Workspace) {
            assert!(message.contains(name), "refusal omits '{name}': {message}");
        }
    }

    /// A tool this profile declares is accepted whether or not it is opt-in today. That is
    /// what keeps an installed config starting after an opt-in tool becomes a default: the
    /// flag stays valid and simply stops changing anything.
    #[test]
    fn enable_tool_accepts_every_declared_name() {
        for (cli_profile, profile) in [
            (McpProfileCli::Workspace, mcp_server::McpProfile::Workspace),
            (McpProfileCli::Reference, mcp_server::McpProfile::Reference),
        ] {
            let declared: Vec<String> =
                mcp_server::contract::declared_tools(profile).map(str::to_owned).collect();
            let mut args = serve_args(McpServeMode::Stdio, None);
            args.runtime_profile = cli_profile;
            if matches!(cli_profile, McpProfileCli::Reference) {
                args.source_dir = None;
            }
            args.enable_tools = declared;

            validate_serve_args(&args)
                .unwrap_or_else(|e| panic!("{} rejected its own tools: {e}", profile.as_str()));
        }
    }

    /// A name belonging to the other profile is refused rather than ignored: accepting it
    /// would report success for a tool this launch will never serve.
    #[test]
    fn enable_tool_rejects_a_name_from_the_other_profile() {
        let mut args = serve_args(McpServeMode::Stdio, None);
        args.enable_tools = vec!["its_help".to_owned()];

        let err = validate_serve_args(&args).expect_err("its_help is not a workspace tool");

        assert!(err.to_string().contains("its_help"), "{err}");
    }

    #[test]
    fn reference_profile_rejects_cache_dir() {
        let mut args = serve_args(McpServeMode::Stdio, None);
        args.runtime_profile = McpProfileCli::Reference;
        args.source_dir = None;
        args.cache_dir = Some(PathBuf::from("cache"));

        let err = validate_serve_args(&args).expect_err("reference cache override is ambiguous");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("--cache-dir"));
    }

    /// `--cache-dir "$UNSET_VAR"` reaches the process as an empty argument, which resolves to
    /// the working directory the client happened to start in.
    #[test]
    fn empty_cache_dir_is_refused_rather_than_resolved_to_the_working_directory() {
        let mut args = serve_args(McpServeMode::Stdio, None);
        args.cache_dir = Some(PathBuf::new());

        let err = validate_serve_args(&args).expect_err("an empty cache root names nothing");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("--cache-dir"));
    }

    #[test]
    fn workspace_cache_scope_explicit_default_base_keeps_leaf_but_stamps_origin() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("исходники");
        std::fs::create_dir(&source).unwrap();
        let canonical_source = source.canonicalize().unwrap();
        let implicit = test_cache_layout(&canonical_source, None);
        let explicit = test_cache_layout(&canonical_source, Some(implicit.declared_base()));
        assert_eq!(implicit.root(), explicit.root());
        assert!(implicit.scope_stamp().starts_with("default:"));
        assert!(explicit.scope_stamp().starts_with("explicit:"));
    }

    #[test]
    fn workspace_cache_scope_broker_child_receives_absolute_base_and_stamp() {
        let source = tempfile::tempdir().unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        let cache_base = cache_parent.path().join("кеш с пробелом");
        let cache = test_cache_layout(source.path(), Some(&cache_base));
        let mut args = serve_args(McpServeMode::Broker, None);
        args.cache_dir = Some(cache_base.clone());
        let embedding_prefixes = mcp_server::EmbeddingPrefixes {
            query: "query: ".to_owned(),
            document: "document: ".to_owned(),
            token_profile: Some(mcp_server::EmbeddingTokenProfile {
                max_input_tokens: 8192,
                tokenizer_file: cache.root().join("tokenizer.json"),
                tokenizer_sha256: "abc123".to_owned(),
                token_policy: None,
            }),
        };

        let command = daemon_command(
            PathBuf::from("bsl-analyzer").as_path(),
            &source.path().canonicalize().unwrap(),
            &cache,
            &args,
            42,
            &embedding_prefixes,
        );
        let argv = command.get_args().map(PathBuf::from).collect::<Vec<_>>();
        let flag = argv.iter().position(|arg| arg == "--cache-dir").unwrap();

        assert!(argv[flag + 1].is_absolute());
        assert_eq!(argv[flag + 1], cache.declared_base());
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(mcp_server::WORKSPACE_CACHE_SCOPE_ENV)
                && value == Some(cache.scope_stamp().as_ref())
        }));
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(mcp_server::broker::EMBEDDING_QUERY_PREFIX_ENV)
                && value == Some(std::ffi::OsStr::new("query: "))
        }));
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(mcp_server::broker::EMBEDDING_DOCUMENT_PREFIX_ENV)
                && value == Some(std::ffi::OsStr::new("document: "))
        }));
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(mcp_server::broker::EMBEDDING_MAX_INPUT_TOKENS_ENV)
                && value == Some(std::ffi::OsStr::new("8192"))
        }));
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(mcp_server::broker::EMBEDDING_TOKENIZER_FILE_ENV)
                && value == Some(cache.root().join("tokenizer.json").as_os_str())
        }));
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(mcp_server::broker::EMBEDDING_TOKENIZER_SHA256_ENV)
                && value == Some(std::ffi::OsStr::new("abc123"))
        }));
    }

    /// Without the flag the child receives the resolved OS base with a default-origin stamp,
    /// preserving the lazy creation policy.
    #[test]
    fn workspace_cache_scope_broker_child_receives_default_base_and_stamp() {
        let source = tempfile::tempdir().unwrap();
        let canonical_source = source.path().canonicalize().unwrap();
        let cache = test_cache_layout(&canonical_source, None);
        let args = serve_args(McpServeMode::Broker, None);

        let command = daemon_command(
            PathBuf::from("bsl-analyzer").as_path(),
            &canonical_source,
            &cache,
            &args,
            42,
            &mcp_server::EmbeddingPrefixes::default(),
        );
        let argv = command.get_args().map(PathBuf::from).collect::<Vec<_>>();

        let flag = argv.iter().position(|arg| arg == "--cache-dir").unwrap();
        assert_eq!(argv[flag + 1], cache.declared_base());
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(mcp_server::WORKSPACE_CACHE_SCOPE_ENV)
                && value == Some(cache.scope_stamp().as_ref())
        }));
        assert!(!canonical_source.join(".build").exists(), "the default stays lazy");
    }

    /// The daemon derives the backend identity from its own argv, so every name the proxy
    /// was given has to reach it. A dropped name would leave proxy and daemon keying on
    /// different surfaces: the proxy would hand clients a socket whose server serves
    /// something else, and the default-off promise would depend on who started first.
    #[test]
    fn broker_child_receives_every_enable_tool() {
        let source = tempfile::tempdir().unwrap();
        let canonical_source = source.path().canonicalize().unwrap();
        let cache = test_cache_layout(&canonical_source, None);
        let mut args = serve_args(McpServeMode::Broker, None);
        args.enable_tools = vec!["graph".to_owned(), "outline".to_owned()];

        let command = daemon_command(
            PathBuf::from("bsl-analyzer").as_path(),
            &canonical_source,
            &cache,
            &args,
            42,
            &mcp_server::EmbeddingPrefixes::default(),
        );
        let argv: Vec<String> =
            command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        for tool in &args.enable_tools {
            assert!(
                argv.windows(2).any(|pair| pair[0] == "--enable-tool" && pair[1] == *tool),
                "argv does not carry --enable-tool {tool}: {argv:?}"
            );
        }
    }

    #[test]
    fn reference_profile_still_rejects_onec_options() {
        let mut args = serve_args(McpServeMode::Stdio, None);
        args.runtime_profile = McpProfileCli::Reference;
        args.source_dir = None;
        args.onec_url = Some("http://onec.example.test".to_owned());

        let err = validate_serve_args(&args).expect_err("reference must keep rejecting 1C options");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("--onec-url/--onec-user/--onec-password"));
    }

    #[test]
    fn reference_profile_without_onec_options_remains_valid() {
        let mut args = serve_args(McpServeMode::Stdio, None);
        args.runtime_profile = McpProfileCli::Reference;
        args.source_dir = None;

        assert!(validate_serve_args(&args)
            .expect("reference without 1C options remains valid")
            .is_none());
    }

    fn serve_args(mode: McpServeMode, port: Option<u16>) -> McpServeArgs {
        McpServeArgs {
            runtime_profile: McpProfileCli::Workspace,
            source_dir: Some(std::path::PathBuf::from(".")),
            enable_tools: Vec::new(),
            cache_dir: None,
            mode,
            backend_pid: None,
            host: None,
            port,
            allowed_hosts: Vec::new(),
            config: None,
            onec_url: None,
            onec_user: String::new(),
            onec_password: String::new(),
            source_set: Default::default(),
        }
    }

    fn serve_command(mode: McpServeMode, source_dir: &std::path::Path) -> McpCommand {
        let mut args = serve_args(mode, None);
        args.source_dir = Some(source_dir.to_path_buf());
        McpCommand::Serve(args)
    }

    #[test]
    fn daemon_log_requires_explicit_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = serve_command(McpServeMode::Daemon, dir.path());
        for off in [None, Some(""), Some("0"), Some("false"), Some("off")] {
            assert!(cmd.daemon_log_file_for(off).is_none(), "{off:?} must keep stderr");
        }
    }

    #[test]
    fn daemon_log_opt_in_lands_in_build_dir_and_rotates_only_by_size() {
        let dir = tempfile::tempdir().unwrap();

        let path = serve_command(McpServeMode::Daemon, dir.path())
            .daemon_log_file_for(Some("1"))
            .expect("opt-in enables the default log file");
        let project = mcp_server::project::at(&dir.path().canonicalize().unwrap()).unwrap();
        let layout = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            None,
            &std::env::current_dir().unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(path, layout.daemon_log_path());
        assert!(path.parent().unwrap().is_dir(), "the selected cache namespace is created eagerly");

        // A small live log is left in place: a concurrent daemon candidate must
        // not rename the winner's file out from under it.
        std::fs::write(&path, "live winner log").unwrap();
        let again = serve_command(McpServeMode::Daemon, dir.path())
            .daemon_log_file_for(Some("true"))
            .unwrap();
        assert_eq!(again, path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "live winner log");
        let prev = path.with_file_name("bsl-analyzer-daemon.log.prev");
        assert!(!prev.exists());

        // An oversized log rotates to `.prev` (sparse file keeps the test cheap).
        std::fs::File::create(&path).unwrap().set_len(51 * 1024 * 1024).unwrap();
        serve_command(McpServeMode::Daemon, dir.path()).daemon_log_file_for(Some("1")).unwrap();
        assert!(prev.exists());
        assert!(!path.exists());
    }

    #[test]
    fn daemon_log_opt_in_uses_external_cache_without_touching_source() {
        let source = tempfile::tempdir().unwrap();
        let cache_parent = tempfile::tempdir().unwrap();
        let cache = cache_parent.path().join("кеш с пробелом");
        let mut command = serve_command(McpServeMode::Daemon, source.path());
        let McpCommand::Serve(args) = &mut command else { unreachable!() };
        args.cache_dir = Some(cache.clone());

        let path = command
            .daemon_log_file_for(Some("1"))
            .expect("external daemon log path must be prepared");

        let project = mcp_server::project::at(&source.path().canonicalize().unwrap()).unwrap();
        let layout = mcp_server::WorkspaceCacheLayout::for_project(
            &project,
            Some(&cache),
            &std::env::current_dir().unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(path, layout.daemon_log_path());
        assert!(!source.path().join(".build").exists());
    }

    /// The log destination is resolved before the arguments are validated, so a combination
    /// that validation is about to reject must not create the directory on its way out.
    #[test]
    fn daemon_log_setup_creates_no_cache_for_a_command_validation_will_reject() {
        let parent = tempfile::tempdir().unwrap();
        let cache = parent.path().join("должен-остаться-несозданным");
        let mut command = serve_command(McpServeMode::Daemon, parent.path());
        {
            let McpCommand::Serve(args) = &mut command else { unreachable!() };
            args.runtime_profile = McpProfileCli::Reference;
            args.source_dir = None;
            args.cache_dir = Some(cache.clone());
        }

        assert!(command.daemon_log_file_for(Some("1")).is_none());
        assert!(!cache.exists(), "a rejected command created {}", cache.display());
        let McpCommand::Serve(args) = &command else { unreachable!() };
        assert!(validate_serve_args(args).is_err(), "the combination is indeed rejected");
    }

    /// A second member of the same class, rejected for a reason that has nothing to do with
    /// the cache: the guard must ask the validator, not restate the rules it happens to know.
    #[test]
    fn daemon_log_setup_creates_no_cache_when_an_unrelated_flag_is_rejected() {
        let source = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let cache = parent.path().join("несозданный-кеш");
        let mut command = serve_command(McpServeMode::Daemon, source.path());
        {
            let McpCommand::Serve(args) = &mut command else { unreachable!() };
            args.cache_dir = Some(cache.clone());
            args.config = Some(PathBuf::from("mcp-http.toml"));
        }

        assert!(command.daemon_log_file_for(Some("1")).is_none());
        assert!(!cache.exists(), "a rejected command created {}", cache.display());
        let McpCommand::Serve(args) = &command else { unreachable!() };
        assert!(validate_serve_args(args).is_err(), "--config outside http mode is rejected");
    }

    #[test]
    fn daemon_log_opt_in_accepts_an_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("custom.log");
        let path = serve_command(McpServeMode::Daemon, dir.path())
            .daemon_log_file_for(Some(custom.to_str().unwrap()))
            .unwrap();
        assert_eq!(path, custom);
    }

    #[test]
    fn daemon_log_explicit_path_prepares_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("nested/dirs/daemon.log");
        let path = serve_command(McpServeMode::Daemon, dir.path())
            .daemon_log_file_for(Some(custom.to_str().unwrap()))
            .expect("missing parent directories are created, not fatal");
        assert_eq!(path, custom);
        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn daemon_log_scope_failure_does_not_create_or_rotate_custom_log() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("bsl-analyzer.toml"), "[source\n").unwrap();
        let log_parent = source.path().join("must-not-exist");
        let custom = log_parent.join("daemon.log");
        let path = custom.to_string_lossy().into_owned();

        assert!(serve_command(McpServeMode::Daemon, source.path())
            .daemon_log_file_for(Some(&path))
            .is_none());
        assert!(!log_parent.exists(), "invalid Project created the custom log parent");
    }

    #[test]
    fn daemon_log_scope_stamp_failure_does_not_rotate_existing_custom_log() {
        let source = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let custom = logs.path().join("daemon.log");
        std::fs::File::create(&custom).unwrap().set_len(51 * 1024 * 1024).unwrap();
        let wrong_scope = format!("explicit:{}", "a".repeat(64));

        assert!(serve_command(McpServeMode::Daemon, source.path())
            .daemon_log_file_for_scope(Some(custom.to_str().unwrap()), Some(&wrong_scope))
            .is_none());
        assert_eq!(std::fs::metadata(&custom).unwrap().len(), 51 * 1024 * 1024);
        assert!(!logs.path().join("daemon.log.prev").exists());
    }

    #[test]
    fn daemon_log_rotation_appends_prev_to_dotted_names() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("daemon.custom.log");
        std::fs::File::create(&custom).unwrap().set_len(51 * 1024 * 1024).unwrap();
        serve_command(McpServeMode::Daemon, dir.path())
            .daemon_log_file_for(Some(custom.to_str().unwrap()))
            .unwrap();
        assert!(dir.path().join("daemon.custom.log.prev").exists());
        assert!(!custom.exists());
    }

    #[test]
    fn stdio_and_broker_modes_keep_stderr_logging() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [McpServeMode::Stdio, McpServeMode::Broker] {
            assert!(serve_command(mode, dir.path()).daemon_log_file_for(Some("1")).is_none());
        }
    }
}
