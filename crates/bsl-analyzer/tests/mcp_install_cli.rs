use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::TempDir;

/// `mcp install` writes the absolute path of the running executable as the launch
/// command. In these tests the harness runs the `bsl-analyzer-app` test binary, so its
/// `current_exe()` is exactly `CARGO_BIN_EXE_bsl-analyzer-app`.
const SELF_BIN: &str = env!("CARGO_BIN_EXE_bsl-analyzer-app");

#[derive(Debug)]
struct Invocation {
    program: String,
    cwd: String,
    args: Vec<String>,
}

struct TestHarness {
    _temp: TempDir,
    bin_dir: PathBuf,
    home_dir: PathBuf,
    project_dir: PathBuf,
    log_path: PathBuf,
}

impl TestHarness {
    fn new() -> Self {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let bin_dir = root.join("bin");
        let home_dir = root.join("home");
        let project_dir = root.join("project");
        let log_path = root.join("invocations.log");

        fs::create_dir_all(&bin_dir).expect("bin dir");
        fs::create_dir_all(&home_dir).expect("home dir");
        fs::create_dir_all(project_dir.join("src")).expect("project src dir");

        Self { _temp: temp, bin_dir, home_dir, project_dir, log_path }
    }

    fn install_stub(&self, name: &str, script: &str) {
        // Windows resolves a bare program name through PATHEXT, so the stub needs an extension.
        let file_name = if cfg!(windows) { format!("{name}.cmd") } else { name.to_owned() };
        let path = self.bin_dir.join(file_name);
        fs::write(&path, script).expect("write stub");
        make_executable(&path);
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"));
        let mut dirs = vec![self.bin_dir.clone()];
        dirs.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        let path_env = env::join_paths(dirs).expect("join PATH");
        cmd.current_dir(&self.project_dir)
            .env("HOME", &self.home_dir)
            .env("USERPROFILE", &self.home_dir)
            .env("PATH", path_env)
            .env("TEST_LOG", &self.log_path);
        cmd
    }

    fn invocations(&self) -> Vec<Invocation> {
        if !self.log_path.exists() {
            return Vec::new();
        }

        parse_invocations(&fs::read_to_string(&self.log_path).expect("log contents"))
    }

    fn source_dir(&self) -> PathBuf {
        self.project_dir.join("src")
    }
}

/// End of the chain: what the installer writes is what the client will run, so the flag
/// has to survive all the way into the argv handed to the target's own CLI.
#[test]
fn enable_tool_reaches_the_argv_written_into_the_client_config() {
    let harness = TestHarness::new();
    harness.install_stub("codex", &codex_stub());

    let output = harness
        .command()
        .args([
            "mcp",
            "install",
            "--target",
            "codex",
            "--scope",
            "user",
            "--preset",
            "reference",
            "--enable-tool",
            "syntax_help",
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let argv = harness
        .invocations()
        .into_iter()
        .find(|call| call.args.iter().any(|arg| arg == "serve"))
        .expect("an install writes a serve command")
        .args;
    assert!(
        argv.windows(2).any(|pair| pair[0] == "--enable-tool" && pair[1] == "syntax_help"),
        "argv: {argv:?}"
    );
}

/// The recommended preset must never enable an opt-in tool, and it says so rather than
/// dropping the flag: silently installing something other than what was asked for is how
/// an operator ends up believing a tool is on.
#[test]
fn recommended_preset_refuses_to_enable_a_tool() {
    let harness = TestHarness::new();
    harness.install_stub("codex", &codex_stub());

    let output = harness
        .command()
        .args([
            "mcp",
            "install",
            "--target",
            "codex",
            "--preset",
            "recommended",
            "--enable-tool",
            "syntax_help",
        ])
        .output()
        .expect("run binary");

    assert!(!output.status.success(), "recommended must refuse --enable-tool");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--enable-tool"), "stderr: {stderr}");
    assert!(harness.invocations().is_empty(), "nothing must be installed on refusal");
}

/// A name the preset's profile does not declare is refused before anything is written:
/// an installed config that cannot start is worse than a failed install.
#[test]
fn install_refuses_a_tool_the_preset_profile_does_not_declare() {
    let harness = TestHarness::new();
    harness.install_stub("codex", &codex_stub());

    let output = harness
        .command()
        .args([
            "mcp",
            "install",
            "--target",
            "codex",
            "--scope",
            "user",
            "--preset",
            "reference",
            "--enable-tool",
            "graph",
        ])
        .output()
        .expect("run binary");

    assert!(!output.status.success(), "graph is not a reference tool");
    assert!(harness.invocations().is_empty(), "nothing must be installed on refusal");
}

#[test]
fn codex_user_reference_force_uses_cli_and_passes_env() {
    let harness = TestHarness::new();
    harness.install_stub("codex", &codex_stub());

    let output = harness
        .command()
        .env("CODEX_GET_EXISTS", "1")
        .args([
            "mcp",
            "install",
            "--target",
            "codex",
            "--scope",
            "user",
            "--preset",
            "reference",
            "--force",
            "--env",
            "NAPARNIK_TOKEN=test",
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[codex:user] updated"));

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 2);
    assert_eq!(invocations[0].program, "codex");
    assert_eq!(invocations[0].args, vec!["mcp", "get", "bsl-analyzer-reference", "--json"]);
    assert_eq!(invocations[1].args[0..4], ["mcp", "add", "bsl-analyzer-reference", "--env"]);
    assert!(invocations[1].args.contains(&"NAPARNIK_TOKEN=test".to_owned()));
    assert!(invocations[1].args.contains(&"--".to_owned()));
    assert!(invocations[1].args.contains(&SELF_BIN.to_owned()));
    assert_eq!(
        invocations[1].args,
        vec![
            "mcp",
            "add",
            "bsl-analyzer-reference",
            "--env",
            "NAPARNIK_TOKEN=test",
            "--",
            SELF_BIN,
            "mcp",
            "serve",
            "--profile",
            "reference",
        ]
    );
    assert_same_path(&invocations[1].cwd, &harness.project_dir);
}

#[test]
fn codex_user_reference_force_migrates_legacy_name() {
    let harness = TestHarness::new();
    harness.install_stub("codex", &codex_stub());

    let output = harness
        .command()
        .env("CODEX_GET_EXISTS_NAMES", "bsl-analyzer")
        .args([
            "mcp",
            "install",
            "--target",
            "codex",
            "--scope",
            "user",
            "--preset",
            "reference",
            "--force",
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 4);
    assert_eq!(invocations[0].args, vec!["mcp", "get", "bsl-analyzer-reference", "--json"]);
    assert_eq!(invocations[1].args, vec!["mcp", "get", "bsl-analyzer", "--json"]);
    assert_eq!(invocations[2].args, vec!["mcp", "remove", "bsl-analyzer"]);
    assert_eq!(
        invocations[3].args,
        vec![
            "mcp",
            "add",
            "bsl-analyzer-reference",
            "--",
            SELF_BIN,
            "mcp",
            "serve",
            "--profile",
            "reference",
        ]
    );
}

#[test]
fn codex_project_writes_toml_without_invoking_cli() {
    let harness = TestHarness::new();
    harness.install_stub("codex", &failing_stub());

    let output = harness
        .command()
        .args([
            "mcp",
            "install",
            "--target",
            "codex",
            "--scope",
            "project",
            "--source-dir",
            harness.source_dir().to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(harness.invocations().is_empty());

    let config_path = harness.project_dir.join(".codex/config.toml");
    let config = fs::read_to_string(&config_path).expect("project config");
    assert!(config.contains("[mcp_servers.bsl-analyzer-workspace]"));
    assert!(config.contains(
        "args = [\"mcp\", \"serve\", \"--profile\", \"workspace\", \"--source-dir\", \"src\"]"
    ));
}

#[test]
fn gemini_project_force_updates_via_add() {
    let harness = TestHarness::new();
    harness.install_stub("gemini", &gemini_stub());
    let settings_dir = harness.project_dir.join(".gemini");
    fs::create_dir_all(&settings_dir).expect("settings dir");
    fs::write(
        settings_dir.join("settings.json"),
        r#"{"mcpServers":{"bsl-analyzer-workspace":{"command":"old","args":["serve"]}}}"#,
    )
    .expect("settings");

    let output = harness
        .command()
        .args([
            "mcp",
            "install",
            "--target",
            "gemini",
            "--scope",
            "project",
            "--force",
            "--env",
            "NAPARNIK_TOKEN=test",
            "--source-dir",
            harness.source_dir().to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[gemini:project] updated"));

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].program, "gemini");
    assert_eq!(invocations[0].args[0..4], ["mcp", "add", "-s", "project"]);
    assert!(invocations[0].args.contains(&"-e".to_owned()));
    assert!(invocations[0].args.contains(&"NAPARNIK_TOKEN=test".to_owned()));
    assert!(invocations[0].args.contains(&"bsl-analyzer-workspace".to_owned()));
    assert_same_path(&invocations[0].cwd, &harness.project_dir);
}

#[test]
fn gemini_project_force_migrates_legacy_name() {
    let harness = TestHarness::new();
    harness.install_stub("gemini", &gemini_stub());
    let settings_dir = harness.project_dir.join(".gemini");
    fs::create_dir_all(&settings_dir).expect("settings dir");
    fs::write(
        settings_dir.join("settings.json"),
        r#"{"mcpServers":{"bsl-analyzer":{"command":"old","args":["serve"]}}}"#,
    )
    .expect("settings");

    let output = harness
        .command()
        .args([
            "mcp",
            "install",
            "--target",
            "gemini",
            "--scope",
            "project",
            "--preset",
            "workspace",
            "--force",
            "--source-dir",
            harness.source_dir().to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 2);
    assert_eq!(invocations[0].args, vec!["mcp", "remove", "-s", "project", "bsl-analyzer"]);
    assert_eq!(
        invocations[1].args[0..5],
        ["mcp", "add", "-s", "project", "bsl-analyzer-workspace"]
    );
}

#[test]
fn claude_project_force_runs_get_remove_add() {
    let harness = TestHarness::new();
    harness.install_stub("claude", &claude_stub());

    let output = harness
        .command()
        .env("CLAUDE_GET_EXISTS", "1")
        .args([
            "mcp",
            "install",
            "--target",
            "claude",
            "--scope",
            "project",
            "--force",
            "--env",
            "NAPARNIK_TOKEN=test",
            "--source-dir",
            harness.source_dir().to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[claude:project] updated"));

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 3);
    assert_eq!(invocations[0].args, vec!["mcp", "get", "bsl-analyzer-workspace"]);
    assert_eq!(
        invocations[1].args,
        vec!["mcp", "remove", "-s", "project", "bsl-analyzer-workspace"]
    );
    assert_eq!(
        invocations[2].args[0..5],
        ["mcp", "add", "-s", "project", "bsl-analyzer-workspace"]
    );
    assert!(invocations[2].args.contains(&"-e".to_owned()));
    assert!(invocations[2].args.contains(&"NAPARNIK_TOKEN=test".to_owned()));
}

#[test]
fn claude_project_force_migrates_legacy_name() {
    let harness = TestHarness::new();
    harness.install_stub("claude", &claude_stub());

    let output = harness
        .command()
        .env("CLAUDE_GET_EXISTS_NAMES", "bsl-analyzer")
        .args([
            "mcp",
            "install",
            "--target",
            "claude",
            "--scope",
            "project",
            "--preset",
            "workspace",
            "--force",
            "--source-dir",
            harness.source_dir().to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 4);
    assert_eq!(invocations[0].args, vec!["mcp", "get", "bsl-analyzer-workspace"]);
    assert_eq!(invocations[1].args, vec!["mcp", "get", "bsl-analyzer"]);
    assert_eq!(invocations[2].args, vec!["mcp", "remove", "-s", "project", "bsl-analyzer"]);
    assert_eq!(
        invocations[3].args[0..5],
        ["mcp", "add", "-s", "project", "bsl-analyzer-workspace"]
    );
}

#[test]
fn codex_recommended_installs_reference_and_workspace() {
    let harness = TestHarness::new();
    harness.install_stub("codex", &codex_stub());

    let output = harness
        .command()
        .args([
            "mcp",
            "install",
            "--target",
            "codex",
            "--preset",
            "recommended",
            "--source-dir",
            harness.source_dir().to_str().expect("utf-8 path"),
            "--env",
            "NAPARNIK_TOKEN=test",
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[codex:user] installed"));
    assert!(stdout.contains("[codex:project] installed"));

    let invocations = harness.invocations();
    assert_eq!(invocations.len(), 3);
    assert_eq!(invocations[0].args, vec!["mcp", "get", "bsl-analyzer-reference", "--json"]);
    assert_eq!(invocations[1].args, vec!["mcp", "get", "bsl-analyzer", "--json"]);
    assert_eq!(
        invocations[2].args,
        vec![
            "mcp",
            "add",
            "bsl-analyzer-reference",
            "--env",
            "NAPARNIK_TOKEN=test",
            "--",
            SELF_BIN,
            "mcp",
            "serve",
            "--profile",
            "reference",
        ]
    );

    let config_path = harness.project_dir.join(".codex/config.toml");
    let config = fs::read_to_string(&config_path).expect("project config");
    assert!(config.contains("[mcp_servers.bsl-analyzer-workspace]"));
    assert!(config.contains(
        "args = [\"mcp\", \"serve\", \"--profile\", \"workspace\", \"--source-dir\", \"src\"]"
    ));
}

#[test]
fn codex_recommended_uses_name_prefix_for_both_servers() {
    let harness = TestHarness::new();
    harness.install_stub("codex", &codex_stub());

    let output = harness
        .command()
        .args([
            "mcp",
            "install",
            "--target",
            "codex",
            "--preset",
            "recommended",
            "--name",
            "custom-bsl",
            "--source-dir",
            harness.source_dir().to_str().expect("utf-8 path"),
        ])
        .output()
        .expect("run binary");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let invocations = harness.invocations();
    assert_eq!(invocations[0].args, vec!["mcp", "get", "custom-bsl-reference", "--json"]);
    assert_eq!(invocations[1].args, vec!["mcp", "get", "bsl-analyzer", "--json"]);
    assert_eq!(
        invocations[2].args,
        vec![
            "mcp",
            "add",
            "custom-bsl-reference",
            "--",
            SELF_BIN,
            "mcp",
            "serve",
            "--profile",
            "reference",
        ]
    );

    let config_path = harness.project_dir.join(".codex/config.toml");
    let config = fs::read_to_string(&config_path).expect("project config");
    assert!(config.contains("[mcp_servers.custom-bsl-workspace]"));
}

fn parse_invocations(contents: &str) -> Vec<Invocation> {
    let mut invocations = Vec::new();
    let mut program = None;
    let mut cwd = None;
    let mut args = Vec::new();

    for line in contents.lines() {
        if line == "---" {
            if let (Some(program), Some(cwd)) = (program.take(), cwd.take()) {
                invocations.push(Invocation { program, cwd, args: std::mem::take(&mut args) });
            }
            continue;
        }

        if let Some(value) = line.strip_prefix("PROGRAM=") {
            program = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("CWD=") {
            cwd = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("ARG=") {
            args.push(value.to_owned());
        }
    }

    if let (Some(program), Some(cwd)) = (program, cwd) {
        invocations.push(Invocation { program, cwd, args });
    }

    invocations
}

fn assert_same_path(actual: &str, expected: &Path) {
    let actual = fs::canonicalize(actual).expect("actual cwd canonical path");
    let expected = fs::canonicalize(expected).expect("expected cwd canonical path");
    assert_eq!(actual, expected);
}

fn codex_stub() -> String {
    #[cfg(unix)]
    return codex_stub_sh();
    #[cfg(windows)]
    return codex_stub_cmd();
}

fn gemini_stub() -> String {
    #[cfg(unix)]
    return gemini_stub_sh();
    #[cfg(windows)]
    return gemini_stub_cmd();
}

fn claude_stub() -> String {
    #[cfg(unix)]
    return claude_stub_sh();
    #[cfg(windows)]
    return claude_stub_cmd();
}

fn failing_stub() -> String {
    #[cfg(unix)]
    return failing_stub_sh();
    #[cfg(windows)]
    return failing_stub_cmd();
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod");
}

#[cfg(windows)]
fn make_executable(_path: &Path) {}

#[cfg(unix)]
fn codex_stub_sh() -> String {
    common_stub_prelude_sh(
        r#"
if [ "$1" = "mcp" ] && [ "$2" = "get" ]; then
  if [ "$CODEX_GET_EXISTS" = "1" ]; then
    echo '{}'
    exit 0
  fi
  case ",$CODEX_GET_EXISTS_NAMES," in
    *,"$3",*)
      echo '{}'
      exit 0
      ;;
  esac
  echo "Error: No MCP server named '$3' found." >&2
  exit 1
fi
exit 0
"#,
    )
}

#[cfg(unix)]
fn gemini_stub_sh() -> String {
    common_stub_prelude_sh("exit 0\n")
}

#[cfg(unix)]
fn claude_stub_sh() -> String {
    common_stub_prelude_sh(
        r#"
if [ "$1" = "mcp" ] && [ "$2" = "get" ]; then
  if [ "$CLAUDE_GET_EXISTS" = "1" ]; then
    echo "present"
    exit 0
  fi
  case ",$CLAUDE_GET_EXISTS_NAMES," in
    *,"$3",*)
      echo "present"
      exit 0
      ;;
  esac
  echo "No MCP server found with name: $3" >&2
  exit 1
fi
exit 0
"#,
    )
}

#[cfg(unix)]
fn failing_stub_sh() -> String {
    common_stub_prelude_sh("echo 'unexpected invocation' >&2\nexit 99\n")
}

#[cfg(unix)]
fn common_stub_prelude_sh(body: &str) -> String {
    format!(
        "#!/bin/sh\n\
echo \"PROGRAM=$(basename \"$0\")\" >> \"$TEST_LOG\"\n\
echo \"CWD=$PWD\" >> \"$TEST_LOG\"\n\
for arg in \"$@\"; do\n\
  echo \"ARG=$arg\" >> \"$TEST_LOG\"\n\
done\n\
echo \"---\" >> \"$TEST_LOG\"\n\
{body}"
    )
}

// Batch stubs. Every echo sits outside a parenthesised block (the client wordings contain
// parentheses and quotes), the log writes put the redirect first so values get no trailing
// space, and the arguments needed for branching are saved before the `shift` loop renumbers
// them.
#[cfg(windows)]
fn cmd_script(body: &str) -> String {
    let text = format!(
        "@echo off\n\
setlocal\n\
set \"A1=%~1\"\n\
set \"A2=%~2\"\n\
set \"A3=%~3\"\n\
>>\"%TEST_LOG%\" echo PROGRAM=%~n0\n\
>>\"%TEST_LOG%\" echo CWD=%CD%\n\
:logloop\n\
if \"%~1\"==\"\" goto logdone\n\
>>\"%TEST_LOG%\" echo ARG=%~1\n\
shift\n\
goto logloop\n\
:logdone\n\
>>\"%TEST_LOG%\" echo ---\n\
{body}"
    );
    text.replace('\n', "\r\n")
}

#[cfg(windows)]
fn codex_stub_cmd() -> String {
    cmd_script(
        r#"if not "%A1%"=="mcp" goto done
if not "%A2%"=="get" goto done
if "%CODEX_GET_EXISTS%"=="1" goto present
if not defined CODEX_GET_EXISTS_NAMES goto missing
for %%n in (%CODEX_GET_EXISTS_NAMES%) do if "%%n"=="%A3%" goto present
:missing
>&2 echo Error: No MCP server named '%A3%' found.
exit /b 1
:present
echo {}
exit /b 0
:done
exit /b 0
"#,
    )
}

#[cfg(windows)]
fn gemini_stub_cmd() -> String {
    cmd_script("exit /b 0\n")
}

#[cfg(windows)]
fn claude_stub_cmd() -> String {
    cmd_script(
        r#"if not "%A1%"=="mcp" goto done
if not "%A2%"=="get" goto done
if "%CLAUDE_GET_EXISTS%"=="1" goto present
if not defined CLAUDE_GET_EXISTS_NAMES goto missing
for %%n in (%CLAUDE_GET_EXISTS_NAMES%) do if "%%n"=="%A3%" goto present
:missing
>&2 echo No MCP server found with name: %A3%
exit /b 1
:present
echo present
exit /b 0
:done
exit /b 0
"#,
    )
}

#[cfg(windows)]
fn failing_stub_cmd() -> String {
    cmd_script(">&2 echo unexpected invocation\nexit /b 99\n")
}
