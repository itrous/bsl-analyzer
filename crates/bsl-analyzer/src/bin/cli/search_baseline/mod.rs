use std::{error::Error, path::PathBuf};

use clap::{Args, Subcommand, ValueEnum};

mod admin;
mod documents;
mod inspect;
mod postgres;
mod publish;
mod publish_policy;
mod retention;
mod snapshot_id;

#[derive(Subcommand)]
pub enum SearchCommand {
    Baseline {
        #[command(subcommand)]
        command: SearchBaselineCommand,
    },
}

#[derive(Subcommand)]
pub enum SearchBaselineCommand {
    Publish(SearchBaselinePublishArgs),

    Inspect {
        #[command(subcommand)]
        command: SearchBaselineInspectCommand,
    },

    Admin {
        #[command(subcommand)]
        command: SearchBaselineAdminCommand,
    },
}

#[derive(Subcommand)]
pub enum SearchBaselineInspectCommand {
    ListSnapshots(SearchBaselineListSnapshotsArgs),

    ShowSnapshot(SearchBaselineShowSnapshotArgs),

    ListFileObjects(SearchBaselineListFileObjectsArgs),

    ShowFileObject(SearchBaselineShowFileObjectArgs),

    ListEmbeddings(SearchBaselineListEmbeddingsArgs),

    ShowEmbeddingCoverage(SearchBaselineShowEmbeddingCoverageArgs),

    Retention(SearchBaselineRetentionArgs),
}

#[derive(Subcommand)]
pub enum SearchBaselineAdminCommand {
    Migrate(SearchBaselineAdminMigrateArgs),

    Gc(SearchBaselineAdminGcArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SearchBaselineCorpusCli {
    WorkspaceCode,
    Reference,
}

#[derive(Args, Clone)]
pub struct SearchBaselinePublishArgs {
    #[arg(long, value_enum, default_value_t = SearchBaselineCorpusCli::WorkspaceCode)]
    pub(super) corpus: SearchBaselineCorpusCli,

    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long = "snapshot-id")]
    pub(super) snapshot_id: Option<String>,

    #[arg(long)]
    pub(super) branch: Option<String>,

    #[arg(long)]
    pub(super) commit: Option<String>,

    #[arg(long = "parent-snapshot-id")]
    pub(super) parent_snapshot_id: Option<String>,

    #[arg(long = "allow-non-policy-branch")]
    pub(super) allow_non_policy_branch: bool,
}

#[derive(Args, Clone)]
pub struct SearchBaselineListSnapshotsArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long, value_enum)]
    pub(super) corpus: Option<SearchBaselineCorpusCli>,

    #[arg(long)]
    pub(super) branch: Option<String>,

    #[arg(long)]
    pub(super) commit: Option<String>,

    #[arg(long, default_value_t = 20)]
    pub(super) limit: usize,
}

#[derive(Args, Clone)]
pub struct SearchBaselineShowSnapshotArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long = "snapshot-id")]
    pub(super) snapshot_id: String,
}

#[derive(Args, Clone)]
pub struct SearchBaselineListFileObjectsArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long)]
    pub(super) collection: Option<String>,

    #[arg(long, default_value_t = 20)]
    pub(super) limit: usize,
}

#[derive(Args, Clone)]
pub struct SearchBaselineShowFileObjectArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long = "file-object-id")]
    pub(super) file_object_id: String,
}

#[derive(Args, Clone)]
pub struct SearchBaselineListEmbeddingsArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long = "model")]
    pub(super) model_id: Option<String>,

    #[arg(long)]
    pub(super) dimension: Option<usize>,
}

#[derive(Args, Clone)]
pub struct SearchBaselineShowEmbeddingCoverageArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long = "model")]
    pub(super) model_id: Option<String>,

    #[arg(long)]
    pub(super) dimension: Option<usize>,
}

#[derive(Args, Clone)]
pub struct SearchBaselineAdminGcArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long)]
    pub(super) execute: bool,
}

#[derive(Args, Clone)]
pub struct SearchBaselineRetentionArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,

    #[arg(long)]
    pub(super) branch: Option<String>,

    #[arg(long, default_value_t = 200)]
    pub(super) limit: usize,
}

#[derive(Args, Clone)]
pub struct SearchBaselineAdminMigrateArgs {
    #[arg(short = 's', long = "source-dir", default_value = ".")]
    pub(super) source_dir: PathBuf,
}

pub fn run(command: SearchCommand) -> Result<(), Box<dyn Error + Send + Sync>> {
    match command {
        SearchCommand::Baseline { command } => match command {
            SearchBaselineCommand::Publish(args) => {
                bsl_analyzer::help_bootstrap::bootstrap_for_root(&args.source_dir, None);
                publish::run(args)
            }
            SearchBaselineCommand::Inspect { command } => match command {
                SearchBaselineInspectCommand::ListSnapshots(args) => inspect::list_snapshots(args),
                SearchBaselineInspectCommand::ShowSnapshot(args) => inspect::show_snapshot(args),
                SearchBaselineInspectCommand::ListFileObjects(args) => {
                    inspect::list_file_objects(args)
                }
                SearchBaselineInspectCommand::ShowFileObject(args) => {
                    inspect::show_file_object(args)
                }
                SearchBaselineInspectCommand::ListEmbeddings(args) => {
                    inspect::list_embeddings(args)
                }
                SearchBaselineInspectCommand::ShowEmbeddingCoverage(args) => {
                    inspect::show_embedding_coverage(args)
                }
                SearchBaselineInspectCommand::Retention(args) => retention::run(args),
            },
            SearchBaselineCommand::Admin { command } => match command {
                SearchBaselineAdminCommand::Migrate(args) => admin::migrate(args),
                SearchBaselineAdminCommand::Gc(args) => admin::gc(args),
            },
        },
    }
}

pub(super) fn corpus_cli_to_domain(corpus: SearchBaselineCorpusCli) -> bsl_search::CorpusId {
    match corpus {
        SearchBaselineCorpusCli::WorkspaceCode => bsl_search::CorpusId::WorkspaceCode,
        SearchBaselineCorpusCli::Reference => bsl_search::CorpusId::Reference,
    }
}

pub(super) fn shorten_fingerprint(fingerprint: &str) -> &str {
    fingerprint.get(..12).unwrap_or(fingerprint)
}

pub(super) fn format_ratio(numerator: usize, denominator: usize) -> String {
    if denominator == 0 {
        return "0/0 (n/a)".to_owned();
    }

    let percent = (numerator as f64 / denominator as f64) * 100.0;
    format!("{numerator}/{denominator} ({percent:.1}%)")
}
