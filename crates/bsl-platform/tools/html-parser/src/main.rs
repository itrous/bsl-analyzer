//! Command-line wrapper over the `html_parser` library: regenerates the help
//! corpus JSON either from the two help archives of a platform (read
//! in-process, no external tools) or from already unpacked page trees.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};

const USAGE: &str = "\
usage:
  html-parser hbk <platform_dir> <output_json>
      read shcntx_ru.hbk and shlang_ru.hbk from <platform_dir>
  html-parser <shlang_dir> <shcntx_dir> <output_json>
      parse already unpacked help page trees";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [mode, platform_dir, output] if mode == "hbk" => {
            from_hbk(Path::new(platform_dir), Path::new(output))
        }
        [shlang, shcntx, output] => {
            from_dirs(Path::new(shlang), Path::new(shcntx), Path::new(output))
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn from_hbk(platform_dir: &Path, output: &Path) -> Result<()> {
    // Pages are unpacked into a fresh directory of the library's own next to the
    // output, so the extraction stays on the output's disk.
    let scratch: PathBuf = match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let data = html_parser::extract_corpus_from_hbk(
        &platform_dir.join("shcntx_ru.hbk"),
        &platform_dir.join("shlang_ru.hbk"),
        &scratch,
    )?;
    write(&data, output)
}

fn from_dirs(shlang: &Path, shcntx: &Path, output: &Path) -> Result<()> {
    write(&html_parser::parse_help_dirs(shlang, shcntx)?, output)
}

fn write(data: &html_parser::PlatformData, output: &Path) -> Result<()> {
    std::fs::write(output, data.to_json()?)
        .with_context(|| format!("failed to write {}", output.display()))?;
    eprintln!("{}: {} types, {} methods", output.display(), data.type_count(), data.method_count());
    Ok(())
}
