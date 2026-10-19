mod common;

/// A process the server starts can inherit its stderr and outlive it, as the shared daemon
/// does on Windows; closing the server must not wait for that process to close the pipe.
#[cfg(target_os = "linux")]
#[test]
fn shutdown_returns_while_another_process_still_holds_the_server_stderr() -> std::io::Result<()> {
    use std::io::Write;
    use std::process::Command;

    // Built-ins only, so the holder starts no process of its own and is the only other holder
    // of the server's stderr. It blocks until the test writes to the FIFO.
    const HOLDER: &str = r#"read _ < "$1"; echo holder-closing >&2"#;

    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("bsl-analyzer.toml"), "")?;
    let pidfile = root.path().join("holder.pid");
    let fifo = root.path().join("holder.fifo");
    let mkfifo = Command::new("mkfifo").arg(&fifo).status()?;
    assert!(mkfifo.success(), "mkfifo exited with {mkfifo}");

    // A test that fails before releasing the holder would leave it blocked opening the FIFO
    // for good, so it is stopped on the way out unless it was released.
    struct Holder<'a> {
        pidfile: &'a std::path::Path,
        released: bool,
    }
    impl Drop for Holder<'_> {
        fn drop(&mut self) {
            if self.released {
                return;
            }
            if let Ok(pid) = std::fs::read_to_string(self.pidfile) {
                let _ = Command::new("kill").arg(pid.trim()).status();
            }
        }
    }
    let mut holder = Holder { pidfile: &pidfile, released: false };

    let mut command = Command::new("sh");
    command
        .args(["-c", r#"sh -c "$1" sh "$4" & echo $! > "$2"; exec "$3" lsp --mode stdio"#])
        .args(["sh", HOLDER])
        .arg(&pidfile)
        .arg(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg(&fifo);
    let mut lsp =
        common::Lsp::start_command(command, root.path(), serde_json::json!({}), common::SILENCE);

    let status = lsp.shutdown(2);
    assert!(status.success(), "LSP subprocess exited with {status}");
    // Nothing has been written to the FIFO yet, so the holder still has the pipe open.
    let pid = std::fs::read_to_string(&pidfile)?;
    let alive = Command::new("kill").args(["-0", pid.trim()]).status()?;
    assert!(alive.success(), "stderr holder {} is gone before the test released it", pid.trim());

    // Opened for both reading and writing, the FIFO neither waits for the holder to open it
    // nor raises SIGPIPE, and the line stays buffered until the holder reads it.
    let mut go = std::fs::OpenOptions::new().read(true).write(true).open(&fifo)?;
    go.write_all(b"\n")?;
    let stderr = lsp.finish_stderr();
    holder.released = true;
    assert!(stderr.ends_with("holder-closing\n"), "{stderr}");
    drop(go);
    Ok(())
}
