#[cfg(not(target_os = "windows"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// jemalloc tuning. Without a background purge thread jemalloc retains freed
// pages as dirty extents and idle RSS stays pinned at the analysis-burst
// high-water mark (measured ~5.7GB resident vs ~0.7GB live on ERP). A background
// thread plus a bounded dirty decay returns those pages to the OS so idle RSS
// tracks live memory. `dirty_decay_ms` is non-zero to keep page reuse cheap on
// hot allocation paths; `muzzy_decay_ms:0` returns muzzy pages promptly.
// `background_thread` is only enabled on linux-gnu, where jemalloc supports it.
#[cfg(not(target_os = "windows"))]
const MALLOC_CONF_BYTES: &[u8] = {
    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    {
        b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0\0"
    }
    #[cfg(not(all(target_os = "linux", not(target_env = "musl"))))]
    {
        b"dirty_decay_ms:1000,muzzy_decay_ms:0\0"
    }
};

// jemalloc reads the `malloc_conf` symbol as a C `const char *`. It must be a
// thin pointer (`Option<&c_char>`), not a `&[u8]` fat slice; the union converts
// `&u8 -> &c_char` in const context. `tikv-jemallocator` links it prefixed.
#[cfg(not(target_os = "windows"))]
union MallocConfPtr {
    bytes: &'static u8,
    cchar: &'static core::ffi::c_char,
}

#[cfg(not(target_os = "windows"))]
#[allow(non_upper_case_globals)]
#[export_name = "_rjem_malloc_conf"]
pub static malloc_conf: Option<&'static core::ffi::c_char> =
    Some(unsafe { MallocConfPtr { bytes: &MALLOC_CONF_BYTES[0] }.cchar });

mod cli;

use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use clap::{CommandFactory, Parser, Subcommand};
use cli::{
    analyze::{analyze, OutputFormat},
    bench::{run_bench, BenchCommands},
    check_config::check_config,
    contract::cli_surface,
    dap::run_dap_server,
    deps::{run_deps, DepsOutputFormat},
    diagnostics_baseline::{self, DiagnosticsCommand},
    extension::{self, ExtensionCommands},
    format::run_format,
    logging::setup_logging,
    lsp::run_lsp_server,
    mcp::{self, McpCommand},
    platform_help::{self as platform_help_cli, PlatformHelpCommand},
    rules::{self, RulesCommands},
    search_baseline::{self, SearchCommand},
    smoke::run_smoke,
};

#[derive(Parser)]
#[command(name = "bsl-analyzer")]
#[command(version)]
#[command(about = "BSL Language Server and Analyzer")]
struct Cli {
    #[arg(long)]
    stdio: bool,

    #[arg(long = "trace-profile", global = true)]
    profile: Option<String>,

    #[arg(long = "trace-profile-json", global = true)]
    profile_json: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant, reason = "CLI command enum should stay unboxed")]
enum Commands {
    Analyze {
        #[arg(
            short = 's',
            long = "source-dir",
            alias = "srcDir",
            alias = "src",
            alias = "project",
            default_value = "."
        )]
        source_dir: PathBuf,

        #[arg(short = 'w', long = "workspace-dir", alias = "workspaceDir")]
        workspace_dir: Option<PathBuf>,

        #[arg(short = 'o', long = "output-dir", alias = "outputDir")]
        output_dir: Option<PathBuf>,

        #[arg(short = 'c', long = "config", alias = "configuration")]
        config: Option<PathBuf>,

        #[arg(short = 'r', long = "reporters", alias = "reporter", value_delimiter = ',')]
        reporters: Vec<String>,

        #[arg(short = 'q', long = "quiet", alias = "silent")]
        quiet: bool,

        #[arg(long)]
        incremental: bool,

        #[arg(long, value_delimiter = ',', requires = "incremental")]
        changed_files: Option<Vec<PathBuf>>,

        #[arg(long, requires = "incremental", conflicts_with = "changed_files")]
        git_diff: Option<String>,

        #[arg(long)]
        workers: Option<usize>,

        #[arg(long, value_enum, default_value_t = OutputFormat::Console)]
        format: OutputFormat,

        #[arg(long)]
        only_diagnostic: Option<String>,

        #[arg(long)]
        diff_filter: Option<PathBuf>,

        #[arg(long = "ignored-author", alias = "ignored-authors", value_delimiter = ',')]
        ignored_authors: Vec<String>,

        #[command(flatten)]
        source_set: cli::source_set::SourceSetArgs,
    },

    CheckConfig {
        #[arg(short, long)]
        config: std::path::PathBuf,

        #[command(flatten)]
        source_set: cli::source_set::SourceSetArgs,
    },

    /// Manage diagnostics baselines. For partitioned baselines, `include` enables
    /// suppression only for selected owners; all other diagnostics remain unsuppressed.
    Diagnostics {
        #[command(subcommand)]
        command: DiagnosticsCommand,
    },

    /// Print the machine-readable contract of this build: CLI commands and flags, MCP
    /// tools with their actions and parameters, and a contract version separate from the
    /// build version. Check compatibility against this instead of grepping `--help`.
    Contract,

    Format {
        file: PathBuf,

        #[arg(short = 'w', long, conflicts_with = "check")]
        write: bool,

        #[arg(long)]
        spaces: bool,

        #[arg(long, default_value = "4")]
        indent_size: u32,

        #[arg(long)]
        check: bool,
    },

    Lsp,

    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },

    Extension {
        #[command(subcommand)]
        command: ExtensionCommands,
    },

    Dap,

    Search {
        #[command(subcommand)]
        command: SearchCommand,
    },

    Rules {
        #[command(subcommand)]
        command: RulesCommands,
    },

    /// Platform help corpus packages.
    PlatformHelp {
        #[command(subcommand)]
        command: PlatformHelpCommand,
    },

    Deps {
        #[arg(short = 's', long = "source-dir", default_value = ".")]
        source_dir: PathBuf,

        #[arg(short = 'd', long = "depth", default_value = "3")]
        depth: u32,

        #[arg(long = "sample", default_value = "200")]
        sample: usize,

        #[arg(long = "format", value_enum, default_value_t = DepsOutputFormat::Csv)]
        format: DepsOutputFormat,

        #[arg(short = 'q', long = "quiet")]
        quiet: bool,

        #[arg(long = "bytes")]
        bytes: bool,

        #[arg(long = "report-mem")]
        report_mem: bool,

        #[arg(long = "bench", conflicts_with_all = ["multi_open", "bench_index"])]
        bench: Option<PathBuf>,

        #[arg(long = "multi-open", value_delimiter = ',')]
        multi_open: Vec<PathBuf>,

        #[arg(long = "bench-index", conflicts_with_all = ["bench", "multi_open"])]
        bench_index: bool,

        #[arg(long = "index-workers")]
        index_workers: Option<usize>,
    },

    Smoke {
        #[arg(short = 's', long = "source-dir", default_value = ".")]
        source_dir: PathBuf,

        #[arg(long = "scenarios", value_delimiter = ',', default_value = "boot")]
        scenarios: Vec<String>,

        #[arg(long = "budgets")]
        budgets: Option<PathBuf>,

        #[arg(long = "json")]
        json: bool,
    },

    Bench {
        #[command(subcommand)]
        command: BenchCommands,
    },
}

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let cli = Cli::parse();

    let (log_file, append_log) = match env::var("BSL_LOG_FILE").ok().map(PathBuf::from) {
        Some(file) => (Some(file), false),
        None => match &cli.command {
            Some(Commands::Mcp { command }) => (command.default_daemon_log_file(), true),
            _ => (None, false),
        },
    };

    let profile_filter = cli.profile.clone().or_else(|| env::var("BSL_PROFILE").ok());
    let json_profile_filter =
        cli.profile_json.clone().or_else(|| env::var("BSL_PROFILE_JSON").ok());

    let _journal_guard =
        match setup_logging(log_file.clone(), append_log, profile_filter, json_profile_filter) {
            Ok(guard) => guard,
            Err(e) => {
                eprintln!("Failed to setup logging: {}", e);
                if let Some(ref path) = log_file {
                    let _ = fs::write(path, format!("ERROR: Failed to setup logging: {}\n", e));
                }
                return Err(e.into());
            }
        };

    // A panic inside a Salsa query leaves no useful trace of *which* query blew
    // up once `catch_unwind` has unwound past the query stack: Salsa attaches the
    // database (and hence the query stack) to the thread only for the duration of
    // a tracked-fn body. Capturing here, in the panic hook, runs at the panic
    // point while that attachment is still live, so the query stacktrace resolves.
    // `capture()` returns `None` — and this hook stays silent — for panics on a
    // thread not currently executing a query; hangs are out of reach entirely
    // (the query stack lives on the stuck thread's thread-local, not ours). The
    // per-request panic log in `handlers::dispatch` is complementary, not replaced.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(backtrace) = salsa::Backtrace::capture() {
            tracing::error!(query_backtrace = %backtrace, "panic inside salsa query");
        }
        prev_hook(info);
    }));

    if append_log && log_file.is_some() {
        // Appended daemon logs concatenate runs; the separator must survive the
        // default filter (`warn` + the `bsl_graph` diagnosis trail), so it rides
        // the `bsl_graph` target rather than plain info.
        tracing::info!(target: "bsl_graph", pid = std::process::id(), "log session started");
    }
    tracing::info!("BSL Analyzer starting (pid: {})", std::process::id());
    tracing::info!("Working directory: {:?}", env::current_dir().ok());
    tracing::info!("Command line args: {:?}", env::args().collect::<Vec<_>>());

    if cli.stdio && cli.command.is_some() {
        tracing::error!("Cannot use --stdio with other commands");
        eprintln!("Error: --stdio flag cannot be used with other subcommands");
        std::process::exit(1);
    }

    // Both the `contract` command and the MCP `bsl-analyzer://contract` resource declare
    // this process's CLI surface, and only the binary can introspect its own clap
    // definition. Hand it over once here rather than per command, so a future command that
    // serves the contract cannot forget to.
    mcp_server::contract::register_cli_surface(cli_surface(&Cli::command()));

    match cli.command {
        Some(Commands::Analyze {
            source_dir,
            workspace_dir,
            output_dir,
            config,
            reporters,
            quiet,
            incremental,
            changed_files,
            git_diff,
            workers,
            format,
            only_diagnostic,
            diff_filter,
            ignored_authors,
            source_set,
        }) => analyze(
            source_dir,
            workspace_dir,
            output_dir,
            config,
            reporters,
            quiet,
            incremental,
            changed_files,
            git_diff,
            workers,
            format,
            only_diagnostic,
            diff_filter,
            ignored_authors,
            source_set,
        ),
        Some(Commands::CheckConfig { config, source_set }) => {
            let root = config.parent().map(Path::to_path_buf).unwrap_or_default();
            bsl_analyzer::help_bootstrap::bootstrap_for_root(&root, Some(&config));
            check_config(config, source_set)
        }
        Some(Commands::Diagnostics { command }) => diagnostics_baseline::run(command),
        Some(Commands::Contract) => {
            println!("{}", serde_json::to_string_pretty(&mcp_server::contract::document())?);
            Ok(())
        }
        Some(Commands::Format { file, write, spaces, indent_size, check }) => {
            run_format(file, write, spaces, indent_size, check)
        }
        Some(Commands::Mcp { command }) => mcp::run(command),
        Some(Commands::Extension { command }) => extension::run(command),
        Some(Commands::Dap) => run_dap_server(),
        Some(Commands::Search { command }) => search_baseline::run(command),
        Some(Commands::PlatformHelp { command }) => platform_help_cli::run(command),
        Some(Commands::Rules { command }) => rules::run(command),
        Some(Commands::Deps {
            source_dir,
            depth,
            sample,
            format,
            quiet,
            bytes,
            report_mem,
            bench,
            multi_open,
            bench_index,
            index_workers,
        }) => {
            bsl_analyzer::help_bootstrap::bootstrap_for_root(&source_dir, None);
            run_deps(
                source_dir,
                depth,
                sample,
                format,
                quiet,
                bytes,
                report_mem,
                bench,
                multi_open,
                bench_index,
                index_workers,
            )
        }
        Some(Commands::Smoke { source_dir, scenarios, budgets, json }) => {
            bsl_analyzer::help_bootstrap::bootstrap_for_root(&source_dir, None);
            run_smoke(source_dir, scenarios, budgets, json)
        }
        Some(Commands::Bench { command }) => run_bench(command),
        Some(Commands::Lsp) | None => run_lsp_server(),
    }
}

#[cfg(test)]
mod contract_surface {
    use super::*;
    use expect_test::expect;
    use serde_json::Value;
    use std::fmt::Write;

    fn surface() -> Value {
        cli_surface(&Cli::command())
    }

    fn command<'a>(parent: &'a Value, name: &str) -> &'a Value {
        parent["commands"]
            .as_array()
            .unwrap_or_else(|| panic!("'{}' has no subcommands", parent["name"]))
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no command '{name}' under '{}'", parent["name"]))
    }

    fn arg_names(cmd: &Value) -> Vec<&str> {
        cmd["args"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect()
    }

    /// The flags downstream CI checks for, asserted against the declaration rather than
    /// against the wording of `--help`. Renaming one of these is a contract change and
    /// must be a deliberate one — see `mcp_server::contract::CONTRACT_VERSION`.
    #[test]
    fn declares_the_flags_consumers_depend_on() {
        let surface = surface();

        let analyze = command(&surface, "analyze");
        let names = arg_names(analyze);
        assert!(names.contains(&"source-dir"), "{names:?}");
        assert!(names.contains(&"format"), "{names:?}");
        let format =
            analyze["args"].as_array().unwrap().iter().find(|a| a["name"] == "format").unwrap();
        assert_eq!(format["values"], serde_json::json!(["console", "jsonl"]));

        let serve = command(command(&surface, "mcp"), "serve");
        let names = arg_names(serve);
        assert!(names.contains(&"profile"), "{names:?}");
        assert!(names.contains(&"source-dir"), "{names:?}");
        let mode = serve["args"].as_array().unwrap().iter().find(|a| a["name"] == "mode").unwrap();
        assert!(
            mode["values"].as_array().unwrap().contains(&serde_json::json!("stdio")),
            "{mode:#}"
        );
    }

    #[test]
    fn allowed_hosts_help_states_what_it_matches_and_what_it_replaces() {
        let mut serve = Cli::command();
        let serve = serve
            .find_subcommand_mut("mcp")
            .expect("mcp command")
            .find_subcommand_mut("serve")
            .expect("mcp serve command");
        let allowed_hosts = serve
            .get_arguments()
            .find(|arg| arg.get_long() == Some("allowed_hosts"))
            .expect("mcp serve declares --allowed_hosts");
        let help =
            allowed_hosts.get_long_help().expect("--allowed_hosts carries long help").to_string();

        assert!(help.contains("Host header"), "{help}");
        assert!(help.contains("replaces"), "{help}");
        for default in ["localhost", "127.0.0.1", "::1"] {
            assert!(help.contains(default), "the replaced default {default} is unnamed: {help}");
        }
        assert!(
            serve.get_arguments().any(|arg| arg.get_long() == Some("config")),
            "mcp serve declares --config as an alternative"
        );
    }

    #[test]
    fn cli_contract_partitioned_baseline() {
        let surface = surface();
        let baseline = command(command(&surface, "diagnostics"), "baseline");
        for name in ["create", "check", "update"] {
            let operation = command(baseline, name);
            let names = arg_names(operation);
            assert!(names.contains(&"source-dir"), "{name}: {names:?}");
            assert!(names.contains(&"config"), "{name}: {names:?}");
            assert!(names.contains(&"format"), "{name}: {names:?}");
            assert!(names.contains(&"partition"), "{name}: {names:?}");
            assert_eq!(names.contains(&"from-v1"), name == "create", "{name}: {names:?}");
        }
    }

    fn render(cmd: &Value, depth: usize) -> String {
        let indent = "  ".repeat(depth);
        let mut out = String::new();
        let _ = writeln!(out, "{indent}{}", cmd["name"].as_str().unwrap());
        for arg in cmd["args"].as_array().unwrap() {
            let mut line = format!("{indent}  {}", arg["name"].as_str().unwrap());
            if let Some(short) = arg["short"].as_str() {
                let _ = write!(line, " (-{short})");
            }
            if let Some(aliases) = arg["aliases"].as_array() {
                let aliases: Vec<&str> = aliases.iter().map(|a| a.as_str().unwrap()).collect();
                let _ = write!(line, " [{}]", aliases.join(", "));
            }
            if let Some(conflicts) = arg["conflicts_with"].as_array() {
                let conflicts: Vec<&str> = conflicts.iter().map(|c| c.as_str().unwrap()).collect();
                let _ = write!(line, " !{}", conflicts.join(" !"));
            }
            if let Some(values) = arg["values"].as_array() {
                let values: Vec<&str> = values.iter().map(|v| v.as_str().unwrap()).collect();
                let _ = write!(line, " = {}", values.join(" | "));
            }
            let _ = writeln!(out, "{line}");
        }
        for sub in cmd["commands"].as_array().into_iter().flatten() {
            out.push_str(&render(sub, depth + 1));
        }
        out
    }

    /// Every command, flag and accepted enum value in one place, so a rename or removal
    /// shows up in the diff of the change that causes it instead of in a consumer's CI.
    /// Rebase with `UPDATE_EXPECT=1 cargo test -p bsl-analyzer contract_surface`.
    #[test]
    fn cli_surface_snapshot() {
        expect![[r#"
            bsl-analyzer
              stdio
              trace-profile
              trace-profile-json
              analyze
                changed-files !git-diff
                config (-c) [configuration]
                configuration-root
                diff-filter
                extension !no-extensions
                extension-depends-on
                external !no-externals
                external-depends-on
                format = console | jsonl
                git-diff !changed-files
                ignored-author [ignored-authors]
                incremental
                no-extensions !extension
                no-externals !external
                only-diagnostic
                output-dir (-o) [outputDir]
                quiet (-q) [silent]
                reporters (-r) [reporter]
                source-dir (-s) [srcDir, src, project]
                workers
                workspace-dir (-w) [workspaceDir]
              check-config
                config (-c)
                configuration-root
                extension !no-extensions
                extension-depends-on
                external !no-externals
                external-depends-on
                no-extensions !extension
                no-externals !external
              diagnostics
                baseline
                  create
                    config (-c)
                    format = text | json
                    from-v1 !partition
                    partition !from-v1
                    source-dir (-s)
                  check
                    config (-c)
                    format = text | json
                    partition
                    source-dir (-s)
                  update
                    config (-c)
                    format = text | json
                    partition
                    source-dir (-s)
              contract
              format
                check !write
                file
                indent-size
                spaces
                write (-w) !check
              lsp
              mcp
                serve
                  allowed_hosts !config
                  backend-pid
                  cache-dir
                  config !allowed_hosts !host !port
                  configuration-root
                  enable-tool
                  extension !no-extensions
                  extension-depends-on
                  external !no-externals
                  external-depends-on
                  host !config
                  mode = stdio | broker | broker-required | daemon | http
                  no-extensions !extension
                  no-externals !external
                  onec-password
                  onec-url
                  onec-user
                  port !config
                  profile = workspace | reference
                  source-dir (-s)
                install
                  dry-run
                  enable-tool
                  env
                  force
                  name
                  onec-password
                  onec-url
                  onec-user
                  preset = workspace | reference | recommended
                  scope = user | project | local
                  source-dir (-s)
                  target = codex | gemini | claude | cursor | all
              extension
                export
                  output (-o)
              dap
              search
                baseline
                  publish
                    allow-non-policy-branch
                    branch
                    commit
                    corpus = workspace-code | reference
                    parent-snapshot-id
                    snapshot-id
                    source-dir (-s)
                  inspect
                    list-snapshots
                      branch
                      commit
                      corpus = workspace-code | reference
                      limit
                      source-dir (-s)
                    show-snapshot
                      snapshot-id
                      source-dir (-s)
                    list-file-objects
                      collection
                      limit
                      source-dir (-s)
                    show-file-object
                      file-object-id
                      source-dir (-s)
                    list-embeddings
                      dimension
                      model
                      source-dir (-s)
                    show-embedding-coverage
                      dimension
                      model
                      source-dir (-s)
                    retention
                      branch
                      limit
                      source-dir (-s)
                  admin
                    migrate
                      source-dir (-s)
                    gc
                      execute
                      source-dir (-s)
              rules
                export
                  format = sonarqube | json
                  lang
                  output (-o)
                list
              platform-help
                package
                  corpus
                  corpus-id
                  hbk-dir
                  output (-o)
                  platform-version
              deps
                bench !bench-index !multi-open
                bench-index !bench !multi-open
                bytes
                depth (-d)
                format = csv | json
                index-workers
                multi-open !bench !bench-index
                quiet (-q)
                report-mem
                sample
                source-dir (-s)
              smoke
                budgets
                json
                scenarios
                source-dir (-s)
              bench
                discover
                  boot-budget-ms
                  output (-o)
                  skip-features
                  source-dir (-s)
                run
                  boot-budget-ms
                  json
                  manifest (-m)
                  mode = latency | recompute | memory
                  point (-p)
                  source-dir (-s)
                  trim-settle-ms
                  warm-iterations
                compare
                  baseline
                  candidate
                  policy
        "#]]
        .assert_eq(&render(&surface(), 0));
    }
}
