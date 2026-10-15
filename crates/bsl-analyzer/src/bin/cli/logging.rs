use std::{env, fs, io, path::PathBuf, sync::Arc};

mod journal;
pub use journal::JournalGuard;

pub fn activate_vector_journal(source: Option<&std::path::Path>) {
    journal::activate(source);
}

/// Placeholder in `BSL_LOG_FILE` that is replaced with the current process id.
pub const PID_PLACEHOLDER: &str = "{pid}";

/// Expands every `{pid}` in an explicit `BSL_LOG_FILE` value to `pid`.
///
/// An explicit log file is truncated on start, so processes started with the same
/// value (several editor windows, an LSP server next to an MCP server) overwrite one
/// another's log and a crashed worker's trail is lost. The placeholder is an opt-in
/// to one file per process; a value without it is returned unchanged and keeps the
/// single truncate-on-start file.
pub fn expand_log_file_path(path: PathBuf, pid: u32) -> PathBuf {
    match path.to_str() {
        Some(text) if text.contains(PID_PLACEHOLDER) => {
            PathBuf::from(text.replace(PID_PLACEHOLDER, &pid.to_string()))
        }
        _ => path,
    }
}

pub fn setup_logging(
    log_file: Option<PathBuf>,
    append_file: bool,
    profile_filter: Option<String>,
    json_profile_filter: Option<String>,
) -> anyhow::Result<JournalGuard> {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    // `append_file` is set for the daemon's default log: concurrent short-lived
    // daemon candidates (the broker's spawn race) must not truncate the winner's
    // live file. An explicit `BSL_LOG_FILE` keeps the historical truncate-on-start
    // behaviour.
    let writer: BoxMakeWriter = match log_file {
        Some(file) if append_file => BoxMakeWriter::new(Arc::new(
            fs::OpenOptions::new().create(true).append(true).open(&file)?,
        )),
        Some(file) => BoxMakeWriter::new(Arc::new(fs::File::create(&file)?)),
        None => BoxMakeWriter::new(io::stderr),
    };

    let user_filter = env::var("BSL_LOG").ok().unwrap_or_else(|| "warn".to_owned());
    // The graph-build heartbeat (target `bsl_graph`) stays visible at the default
    // `warn` level: it is the only trail that localises a wedged cold build. The
    // directive precedes the user filter, so an explicit `bsl_graph=off` wins.
    let filter = format!("bsl_graph=info,{},salsa=warn", user_filter);

    use tracing_subscriber::Layer;
    bsl_search::lifecycle::set_process_id(uuid::Uuid::new_v4().to_string());
    let (layer, guard) = journal::layer();
    bsl_analyzer::tracing::Config { writer, filter, profile_filter, json_profile_filter }
        .init_with_layer(layer.with_filter(journal::filter(&user_filter)))?;

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_placeholder_is_replaced_with_the_process_id() {
        assert_eq!(
            expand_log_file_path(PathBuf::from("logs/bsl-{pid}.log"), 4242),
            PathBuf::from("logs/bsl-4242.log")
        );
    }

    #[test]
    fn every_pid_placeholder_occurrence_is_replaced() {
        assert_eq!(
            expand_log_file_path(PathBuf::from("logs/{pid}/bsl-{pid}.log"), 7),
            PathBuf::from("logs/7/bsl-7.log")
        );
    }

    #[test]
    fn path_without_placeholder_is_unchanged() {
        for raw in ["logs/bsl.log", "logs/bsl-pid.log", "logs/{PID}.log", "logs/{pid.log"] {
            assert_eq!(expand_log_file_path(PathBuf::from(raw), 4242), PathBuf::from(raw));
        }
    }
}
