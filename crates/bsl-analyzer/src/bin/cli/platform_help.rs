use std::error::Error;
use std::path::PathBuf;

use clap::{ArgGroup, Args, Subcommand};

#[derive(Subcommand)]
pub enum PlatformHelpCommand {
    /// Prepare a platform help package (corpus JSON, manifest with SHA-256,
    /// notice) for publication. Archives are read in-process; no external
    /// program is involved.
    Package(PackageArgs),
}

#[derive(Args)]
#[command(group(ArgGroup::new("input").required(true).args(["hbk_dir", "corpus"])))]
pub struct PackageArgs {
    /// Platform directory holding shcntx_ru.hbk and shlang_ru.hbk.
    #[arg(long = "hbk-dir")]
    hbk_dir: Option<PathBuf>,

    /// An existing corpus JSON to package as it is.
    #[arg(long = "corpus")]
    corpus: Option<PathBuf>,

    /// New directory to write the package into; it must not exist.
    #[arg(short = 'o', long = "output")]
    output: PathBuf,

    /// Stable name of this corpus build, e.g. platform-help-8.3.27.2214.
    #[arg(long = "corpus-id")]
    corpus_id: String,

    /// Platform version of the corpus; defaults to the archive directory name
    /// when that is a version.
    #[arg(long = "platform-version")]
    platform_version: Option<String>,
}

pub fn run(command: PlatformHelpCommand) -> Result<(), Box<dyn Error + Send + Sync>> {
    match command {
        PlatformHelpCommand::Package(args) => {
            let input = match (&args.hbk_dir, &args.corpus) {
                (Some(dir), None) => platform_help::PackageInput::Archives(dir),
                (None, Some(corpus)) => platform_help::PackageInput::CorpusJson(corpus),
                _ => unreachable!("clap requires exactly one input"),
            };
            let manifest = platform_help::prepare_package(
                input,
                &args.output,
                &args.corpus_id,
                args.platform_version.as_deref(),
            )?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
            Ok(())
        }
    }
}
