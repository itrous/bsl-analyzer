use std::path::PathBuf;

use clap::{Subcommand, ValueEnum};

/// Exit codes: 0 ok, 1 execution error, 2 manifest error, 3 invariant violation.
const EXIT_OTHER: i32 = 1;
const EXIT_MANIFEST: i32 = 2;
const EXIT_INVARIANT: i32 = 3;

#[derive(Subcommand)]
pub enum BenchCommands {
    /// Probe the workspace and write a verified target manifest.
    Discover {
        #[arg(short = 's', long = "source-dir", default_value = ".")]
        source_dir: PathBuf,

        /// Manifest output path; stdout if omitted.
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,

        #[arg(long = "boot-budget-ms", default_value = "120000")]
        boot_budget_ms: u64,

        /// Feature names whose probe cannot fit the stand (skipping one is a
        /// measurement verdict — record it in the run report).
        #[arg(long = "skip-features", value_delimiter = ',')]
        skip_features: Vec<String>,
    },
    /// Execute exactly one manifest point in this process and report timings.
    Run {
        #[arg(short = 's', long = "source-dir", default_value = ".")]
        source_dir: PathBuf,

        #[arg(short = 'm', long = "manifest")]
        manifest: PathBuf,

        #[arg(short = 'p', long = "point")]
        point: String,

        #[arg(long = "mode", value_enum, default_value_t = BenchMode::Latency)]
        mode: BenchMode,

        /// At least 1: zero warm samples would degrade p50/p95 to silent zeros.
        #[arg(long = "warm-iterations", default_value = "20",
              value_parser = clap::value_parser!(u32).range(1..))]
        warm_iterations: u32,

        /// Mode `memory` only: settle time after the allocator purge before
        /// reading trimmed RSS (jemalloc returns pages asynchronously).
        #[arg(long = "trim-settle-ms", default_value = "2000")]
        trim_settle_ms: u64,

        /// Report output path; stdout if omitted.
        #[arg(long = "json")]
        json: Option<PathBuf>,

        #[arg(long = "boot-budget-ms", default_value = "120000")]
        boot_budget_ms: u64,
    },
    /// Compare a candidate run against a baseline (regression gate).
    /// Exit codes: 0 pass, 1 regression, 2 incompatible.
    Compare {
        /// Directory of per-run report JSONs (or a single file).
        #[arg(long = "baseline")]
        baseline: PathBuf,

        #[arg(long = "candidate")]
        candidate: PathBuf,

        /// Policy JSON; built-in defaults when omitted.
        #[arg(long = "policy")]
        policy: Option<PathBuf>,
    },
}

pub fn run_bench(command: BenchCommands) -> ! {
    match command {
        BenchCommands::Discover { source_dir, output, boot_budget_ms, skip_features } => {
            bsl_analyzer::help_bootstrap::bootstrap_for_root(&source_dir, None);
            let args = bsl_analyzer::bench::discover::DiscoverArgs {
                source_dir,
                boot_budget_ms,
                skip_features,
            };
            match bsl_analyzer::bench::discover::discover(&args) {
                Ok(manifest) => {
                    let text = match serde_json::to_string_pretty(&manifest) {
                        Ok(text) => text,
                        Err(e) => {
                            eprintln!("bench discover: serialization failed: {e}");
                            std::process::exit(EXIT_OTHER);
                        }
                    };
                    match &output {
                        Some(path) => {
                            if let Err(e) = std::fs::write(path, text) {
                                eprintln!("bench discover: cannot write {}: {e}", path.display());
                                std::process::exit(EXIT_OTHER);
                            }
                            eprintln!(
                                "bench discover: {} targets -> {}",
                                manifest.targets.len(),
                                path.display()
                            );
                        }
                        None => println!("{text}"),
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("bench discover: {e}");
                    std::process::exit(exit_code(&e));
                }
            }
        }
        BenchCommands::Run {
            source_dir,
            manifest,
            point,
            mode,
            warm_iterations,
            trim_settle_ms,
            json,
            boot_budget_ms,
        } => {
            bsl_analyzer::help_bootstrap::bootstrap_for_root(&source_dir, None);
            let mode = match mode {
                BenchMode::Latency => bsl_analyzer::bench::runner::RunMode::Latency,
                BenchMode::Recompute => {
                    // The event callback is installed at database construction,
                    // which happens during boot — the flag must be set first.
                    std::env::set_var("BSL_SALSA_EVENTS", "1");
                    bsl_analyzer::bench::runner::RunMode::Recompute
                }
                BenchMode::Memory => bsl_analyzer::bench::runner::RunMode::Memory,
            };
            let args = bsl_analyzer::bench::runner::RunArgs {
                source_dir,
                manifest_path: manifest,
                point_id: point,
                mode,
                warm_iterations: warm_iterations as usize,
                boot_budget_ms,
                trim_settle_ms,
            };
            match bsl_analyzer::bench::runner::run_point(&args) {
                Ok(report) => {
                    let emitted = match &json {
                        Some(path) => bsl_analyzer::bench::report::write_json(&report, path),
                        None => bsl_analyzer::bench::report::render_json(&report)
                            .map(|text| println!("{text}")),
                    };
                    if let Err(e) = emitted {
                        eprintln!("bench run: {e}");
                        std::process::exit(EXIT_OTHER);
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("bench run: {e}");
                    std::process::exit(exit_code(&e));
                }
            }
        }
        BenchCommands::Compare { baseline, candidate, policy } => {
            use bsl_analyzer::bench::compare;
            let run = || -> Result<(compare::CompareOutcome, String), String> {
                let policy = compare::load_policy(policy.as_deref())?;
                let base = compare::load_reports(&baseline)?;
                let cand = compare::load_reports(&candidate)?;
                let (outcome, report) = compare::compare(&base, &cand, &policy);
                Ok((outcome, compare::render_report(&report)?))
            };
            match run() {
                Ok((outcome, rendered)) => {
                    println!("{rendered}");
                    let code = match outcome {
                        compare::CompareOutcome::Pass => 0,
                        compare::CompareOutcome::Regression => 1,
                        compare::CompareOutcome::Incompatible => 2,
                    };
                    std::process::exit(code);
                }
                Err(e) => {
                    eprintln!("bench compare: {e}");
                    std::process::exit(2);
                }
            }
        }
    }
}

fn exit_code(e: &bsl_analyzer::bench::runner::RunError) -> i32 {
    use bsl_analyzer::bench::runner::RunError;
    match e {
        RunError::Manifest(_) => EXIT_MANIFEST,
        RunError::Invariant(_) => EXIT_INVARIANT,
        RunError::Other(_) => EXIT_OTHER,
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BenchMode {
    Latency,
    Recompute,
    Memory,
}
