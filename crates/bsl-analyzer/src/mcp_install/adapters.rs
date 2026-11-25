use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{Map, Value};
use toml_edit::{value, Array, DocumentMut, Item, Table};

use crate::mcp_install::{
    error::InstallError,
    model::{
        resolve_apply_decision, ApplyDecision, InstallAction, InstallEntryResult, InstallPlan,
        InstallResult, InstallScope, InstallStatus, InstallTarget, ServerSpec,
    },
    ports::{CommandOutput, CommandRunner, FileStore},
    program::resolve_program,
};

const LEGACY_SERVER_NAME: &str = "bsl-analyzer";

pub(super) struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> Result<CommandOutput, InstallError> {
        let resolved = resolve_program(program);
        let output =
            Command::new(resolved).args(args).current_dir(cwd).output().map_err(|err| {
                if err.kind() == io::ErrorKind::NotFound {
                    InstallError::TargetBinaryNotFound { program: program.to_owned() }
                } else {
                    InstallError::ExternalCommandFailed {
                        program: program.to_owned(),
                        status: -1,
                        message: err.to_string(),
                    }
                }
            })?;

        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

pub(super) struct RealFileStore;

impl FileStore for RealFileStore {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn write_string(&self, path: &Path, contents: &str) -> io::Result<()> {
        fs::write(path, contents)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

pub(super) fn apply_install_plan(
    plan: &InstallPlan,
    runner: &dyn CommandRunner,
    files: &dyn FileStore,
) -> Result<InstallResult, InstallError> {
    let mut entries = Vec::with_capacity(plan.actions.len());

    for action in &plan.actions {
        entries.push(apply_action(action, runner, files)?);
    }

    Ok(InstallResult { entries })
}

fn apply_action(
    action: &InstallAction,
    runner: &dyn CommandRunner,
    files: &dyn FileStore,
) -> Result<InstallEntryResult, InstallError> {
    match (action.target, action.scope) {
        (InstallTarget::Codex, InstallScope::User) => install_codex_user(action, runner),
        (InstallTarget::Codex, InstallScope::Project) => install_codex_project(action, files),
        (InstallTarget::Gemini, InstallScope::User | InstallScope::Project) => {
            install_gemini(action, runner, files)
        }
        (
            InstallTarget::Claude,
            InstallScope::User | InstallScope::Project | InstallScope::Local,
        ) => install_claude(action, runner, files),
        (InstallTarget::Cursor, InstallScope::User | InstallScope::Project) => {
            install_cursor(action, files)
        }
        _ => Err(InstallError::UnsupportedScope { target: action.target, scope: action.scope }),
    }
}

fn install_codex_user(
    action: &InstallAction,
    runner: &dyn CommandRunner,
) -> Result<InstallEntryResult, InstallError> {
    let location = home_dir()?.join(".codex/config.toml");
    let existing_name = codex_existing_server_name(runner, &action.spec.name, &action.project_dir)?;
    let decision = resolve_apply_decision(
        existing_name.is_some(),
        action.force,
        action.target,
        action.scope,
        &action.spec.name,
        &location.display().to_string(),
    )?;
    let cli_args = build_codex_add_args(&action.spec);

    if action.dry_run {
        let mut detail = render_dry_run_command(
            &location.display().to_string(),
            decision.action_label(),
            "codex",
            &cli_args,
            &action.spec,
        );
        if let Some(existing_name) = existing_name.as_deref() {
            if existing_name != action.spec.name {
                detail.push('\n');
                detail.push_str(&format!(
                    "migration: legacy MCP server '{existing_name}' will be removed before install"
                ));
            }
        }
        return Ok(InstallEntryResult {
            target: action.target,
            scope: action.scope,
            status: InstallStatus::DryRun,
            location: location.display().to_string(),
            detail,
        });
    }

    if matches!(decision, ApplyDecision::Update) {
        if let Some(existing_name) = existing_name.as_deref() {
            if existing_name != action.spec.name {
                run_checked(
                    runner,
                    "codex",
                    &build_codex_remove_args(existing_name),
                    &action.project_dir,
                )?;
            }
        }
    }

    run_checked(runner, "codex", &cli_args, &action.project_dir)?;

    Ok(InstallEntryResult {
        target: action.target,
        scope: action.scope,
        status: decision.status(),
        location: location.display().to_string(),
        detail: if matches!(decision, ApplyDecision::Update) {
            "updated via codex CLI".to_owned()
        } else {
            "installed via codex CLI".to_owned()
        },
    })
}

fn install_codex_project(
    action: &InstallAction,
    files: &dyn FileStore,
) -> Result<InstallEntryResult, InstallError> {
    let path = action.project_dir.join(".codex/config.toml");
    let doc = if files.exists(&path) {
        files
            .read_to_string(&path)
            .map_err(|err| InstallError::ConfigRead {
                path: path.display().to_string(),
                message: err.to_string(),
            })?
            .parse::<DocumentMut>()
            .map_err(|err| InstallError::ConfigParse {
                path: path.display().to_string(),
                format: "TOML",
                message: err.to_string(),
            })?
    } else {
        DocumentMut::new()
    };

    let existing_name = codex_project_existing_server_name(&doc, &action.spec.name);
    let decision = resolve_apply_decision(
        existing_name.is_some(),
        action.force,
        action.target,
        action.scope,
        &action.spec.name,
        &path.display().to_string(),
    )?;

    let updated_doc = upsert_codex_project_server(
        remove_codex_project_server(
            doc,
            existing_name.as_deref().filter(|name| *name != action.spec.name),
        ),
        &action.spec,
    );

    if action.dry_run {
        let mut detail = render_dry_run_config(
            &path.display().to_string(),
            decision.action_label(),
            updated_doc.to_string(),
            &action.spec,
        );
        if let Some(existing_name) = existing_name.as_deref() {
            if existing_name != action.spec.name {
                detail.push('\n');
                detail.push_str(&format!(
                    "migration: legacy MCP server '{existing_name}' will be replaced"
                ));
            }
        }
        return Ok(InstallEntryResult {
            target: action.target,
            scope: action.scope,
            status: InstallStatus::DryRun,
            location: path.display().to_string(),
            detail,
        });
    }

    if let Some(parent) = path.parent() {
        files.create_dir_all(parent).map_err(|err| InstallError::ConfigWrite {
            path: parent.display().to_string(),
            message: err.to_string(),
        })?;
    }
    files.write_string(&path, &updated_doc.to_string()).map_err(|err| {
        InstallError::ConfigWrite { path: path.display().to_string(), message: err.to_string() }
    })?;

    Ok(InstallEntryResult {
        target: action.target,
        scope: action.scope,
        status: decision.status(),
        location: path.display().to_string(),
        detail: if matches!(decision, ApplyDecision::Update) {
            "updated project-scoped codex MCP config".to_owned()
        } else {
            "created project-scoped codex MCP config".to_owned()
        },
    })
}

fn install_gemini(
    action: &InstallAction,
    runner: &dyn CommandRunner,
    files: &dyn FileStore,
) -> Result<InstallEntryResult, InstallError> {
    let location = gemini_config_path(action.scope, &action.project_dir)?;
    let existing_name =
        json_existing_server_name(files, &location, &action.spec.name, "mcpServers")?;
    let decision = resolve_apply_decision(
        existing_name.is_some(),
        action.force,
        action.target,
        action.scope,
        &action.spec.name,
        &location.display().to_string(),
    )?;

    let cli_args = build_gemini_add_args(&action.spec, action.scope);
    if action.dry_run {
        let mut detail = render_dry_run_command(
            &location.display().to_string(),
            decision.action_label(),
            "gemini",
            &cli_args,
            &action.spec,
        );
        if let Some(existing_name) = existing_name.as_deref() {
            if existing_name != action.spec.name {
                detail.push('\n');
                detail.push_str(&format!(
                    "migration: legacy MCP server '{existing_name}' will be removed before install"
                ));
            }
        }
        return Ok(InstallEntryResult {
            target: action.target,
            scope: action.scope,
            status: InstallStatus::DryRun,
            location: location.display().to_string(),
            detail,
        });
    }

    if matches!(decision, ApplyDecision::Update) {
        if let Some(existing_name) = existing_name.as_deref() {
            if existing_name != action.spec.name {
                run_checked(
                    runner,
                    "gemini",
                    &build_gemini_remove_args(existing_name, action.scope),
                    &action.project_dir,
                )?;
            }
        }
    }

    run_checked(runner, "gemini", &cli_args, &action.project_dir)?;

    Ok(InstallEntryResult {
        target: action.target,
        scope: action.scope,
        status: decision.status(),
        location: location.display().to_string(),
        detail: if matches!(decision, ApplyDecision::Update) {
            "updated via gemini CLI".to_owned()
        } else {
            "installed via gemini CLI".to_owned()
        },
    })
}

fn install_claude(
    action: &InstallAction,
    runner: &dyn CommandRunner,
    files: &dyn FileStore,
) -> Result<InstallEntryResult, InstallError> {
    let location = claude_location_hint(action.scope, &action.project_dir)?;
    // `claude mcp get` ignores scope, so project scope is decided by `.mcp.json` itself.
    let existing_name = if matches!(action.scope, InstallScope::Project) {
        json_existing_server_name(
            files,
            &action.project_dir.join(".mcp.json"),
            &action.spec.name,
            "mcpServers",
        )?
    } else {
        claude_existing_server_name(runner, &action.spec.name, &action.project_dir)?
    };
    let decision = resolve_apply_decision(
        existing_name.is_some(),
        action.force,
        action.target,
        action.scope,
        &action.spec.name,
        &location,
    )?;

    let cli_args = build_claude_add_args(&action.spec, action.scope);
    if action.dry_run {
        let mut detail = render_dry_run_command(
            &location,
            decision.action_label(),
            "claude",
            &cli_args,
            &action.spec,
        );
        if let Some(existing_name) = existing_name.as_deref() {
            if existing_name != action.spec.name {
                detail.push('\n');
                detail.push_str(&format!(
                    "migration: legacy MCP server '{existing_name}' will be removed before install"
                ));
            }
        }
        return Ok(InstallEntryResult {
            target: action.target,
            scope: action.scope,
            status: InstallStatus::DryRun,
            location,
            detail,
        });
    }

    if matches!(decision, ApplyDecision::Update) {
        let remove_name = existing_name.as_deref().unwrap_or(&action.spec.name);
        let remove_args = build_claude_remove_args(remove_name, action.scope);
        run_checked(runner, "claude", &remove_args, &action.project_dir)?;
    }

    run_checked(runner, "claude", &cli_args, &action.project_dir)?;

    Ok(InstallEntryResult {
        target: action.target,
        scope: action.scope,
        status: decision.status(),
        location,
        detail: if matches!(decision, ApplyDecision::Update) {
            "updated via claude CLI".to_owned()
        } else {
            "installed via claude CLI".to_owned()
        },
    })
}

fn install_cursor(
    action: &InstallAction,
    files: &dyn FileStore,
) -> Result<InstallEntryResult, InstallError> {
    let path = cursor_config_path(action.scope, &action.project_dir)?;
    let mut root = if files.exists(&path) {
        serde_json::from_str::<Value>(&files.read_to_string(&path).map_err(|err| {
            InstallError::ConfigRead { path: path.display().to_string(), message: err.to_string() }
        })?)
        .map_err(|err| InstallError::ConfigParse {
            path: path.display().to_string(),
            format: "JSON",
            message: err.to_string(),
        })?
    } else {
        Value::Object(Map::new())
    };

    let servers = ensure_json_object(root.as_object_mut(), "root object", &path)?;
    if !matches!(servers.get("mcpServers"), Some(Value::Object(_))) {
        servers.insert("mcpServers".to_owned(), Value::Object(Map::new()));
    }
    let mcp_servers = ensure_json_object(
        servers.get_mut("mcpServers").and_then(Value::as_object_mut),
        "mcpServers",
        &path,
    )?;

    let existing_name = json_object_existing_server_name(mcp_servers, &action.spec.name);
    let decision = resolve_apply_decision(
        existing_name.is_some(),
        action.force,
        action.target,
        action.scope,
        &action.spec.name,
        &path.display().to_string(),
    )?;

    if let Some(existing_name) = existing_name.as_deref() {
        if existing_name != action.spec.name {
            mcp_servers.remove(existing_name);
        }
    }
    mcp_servers.insert(action.spec.name.clone(), cursor_server_value(&action.spec));
    let rendered =
        serde_json::to_string_pretty(&root).map_err(|err| InstallError::ConfigParse {
            path: path.display().to_string(),
            format: "JSON",
            message: err.to_string(),
        })?;

    if action.dry_run {
        let mut detail = render_dry_run_config(
            &path.display().to_string(),
            decision.action_label(),
            rendered,
            &action.spec,
        );
        if let Some(existing_name) = existing_name.as_deref() {
            if existing_name != action.spec.name {
                detail.push('\n');
                detail.push_str(&format!(
                    "migration: legacy MCP server '{existing_name}' will be replaced"
                ));
            }
        }
        return Ok(InstallEntryResult {
            target: action.target,
            scope: action.scope,
            status: InstallStatus::DryRun,
            location: path.display().to_string(),
            detail,
        });
    }

    if let Some(parent) = path.parent() {
        files.create_dir_all(parent).map_err(|err| InstallError::ConfigWrite {
            path: parent.display().to_string(),
            message: err.to_string(),
        })?;
    }
    files.write_string(&path, &(rendered + "\n")).map_err(|err| InstallError::ConfigWrite {
        path: path.display().to_string(),
        message: err.to_string(),
    })?;

    Ok(InstallEntryResult {
        target: action.target,
        scope: action.scope,
        status: decision.status(),
        location: path.display().to_string(),
        detail: if matches!(decision, ApplyDecision::Update) {
            "updated cursor MCP config".to_owned()
        } else {
            "created cursor MCP config".to_owned()
        },
    })
}

fn build_codex_add_args(spec: &ServerSpec) -> Vec<String> {
    let mut args = vec!["mcp".to_owned(), "add".to_owned(), spec.name.clone()];
    for (key, value) in &spec.env {
        args.push("--env".to_owned());
        args.push(format!("{key}={value}"));
    }
    args.push("--".to_owned());
    args.push(spec.command.clone());
    args.extend(spec.args.clone());
    args
}

fn build_codex_remove_args(name: &str) -> Vec<String> {
    vec!["mcp".to_owned(), "remove".to_owned(), name.to_owned()]
}

fn build_gemini_add_args(spec: &ServerSpec, scope: InstallScope) -> Vec<String> {
    let mut args = vec!["mcp".to_owned(), "add".to_owned(), "-s".to_owned(), scope.to_string()];
    for (key, value) in &spec.env {
        args.push("-e".to_owned());
        args.push(format!("{key}={value}"));
    }
    args.push(spec.name.clone());
    args.push(spec.command.clone());
    args.extend(spec.args.clone());
    args
}

fn build_gemini_remove_args(name: &str, scope: InstallScope) -> Vec<String> {
    vec!["mcp".to_owned(), "remove".to_owned(), "-s".to_owned(), scope.to_string(), name.to_owned()]
}

fn build_claude_add_args(spec: &ServerSpec, scope: InstallScope) -> Vec<String> {
    let mut args = vec![
        "mcp".to_owned(),
        "add".to_owned(),
        "-s".to_owned(),
        scope.to_string(),
        spec.name.clone(),
    ];
    for (key, value) in &spec.env {
        args.push("-e".to_owned());
        args.push(format!("{key}={value}"));
    }
    args.push("--".to_owned());
    args.push(spec.command.clone());
    args.extend(spec.args.clone());
    args
}

fn build_claude_remove_args(name: &str, scope: InstallScope) -> Vec<String> {
    vec!["mcp".to_owned(), "remove".to_owned(), "-s".to_owned(), scope.to_string(), name.to_owned()]
}

/// Clients word "no such server" differently across versions; any other failure stays an error.
fn stderr_reports_missing_server(stderr: &str) -> bool {
    stderr.contains("No MCP server named") || stderr.contains("No MCP server found with name")
}

fn codex_server_exists(
    runner: &dyn CommandRunner,
    name: &str,
    cwd: &Path,
) -> Result<bool, InstallError> {
    let args = vec!["mcp".to_owned(), "get".to_owned(), name.to_owned(), "--json".to_owned()];
    let output = runner.run("codex", &args, cwd)?;
    if output.status == 0 {
        return Ok(true);
    }
    if stderr_reports_missing_server(&output.stderr) {
        return Ok(false);
    }
    Err(InstallError::InspectionFailed {
        target: InstallTarget::Codex,
        name: name.to_owned(),
        message: output.stderr.trim().to_owned(),
    })
}

fn codex_existing_server_name(
    runner: &dyn CommandRunner,
    requested_name: &str,
    cwd: &Path,
) -> Result<Option<String>, InstallError> {
    if codex_server_exists(runner, requested_name, cwd)? {
        return Ok(Some(requested_name.to_owned()));
    }

    if let Some(legacy_name) = legacy_server_name(requested_name) {
        if codex_server_exists(runner, legacy_name, cwd)? {
            return Ok(Some(legacy_name.to_owned()));
        }
    }

    Ok(None)
}

fn claude_server_exists(
    runner: &dyn CommandRunner,
    name: &str,
    cwd: &Path,
) -> Result<bool, InstallError> {
    let args = vec!["mcp".to_owned(), "get".to_owned(), name.to_owned()];
    let output = runner.run("claude", &args, cwd)?;
    if output.status == 0 {
        return Ok(true);
    }
    if stderr_reports_missing_server(&output.stderr) {
        return Ok(false);
    }
    Err(InstallError::InspectionFailed {
        target: InstallTarget::Claude,
        name: name.to_owned(),
        message: output.stderr.trim().to_owned(),
    })
}

fn claude_existing_server_name(
    runner: &dyn CommandRunner,
    requested_name: &str,
    cwd: &Path,
) -> Result<Option<String>, InstallError> {
    if claude_server_exists(runner, requested_name, cwd)? {
        return Ok(Some(requested_name.to_owned()));
    }

    if let Some(legacy_name) = legacy_server_name(requested_name) {
        if claude_server_exists(runner, legacy_name, cwd)? {
            return Ok(Some(legacy_name.to_owned()));
        }
    }

    Ok(None)
}

fn run_checked(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<CommandOutput, InstallError> {
    let output = runner.run(program, args, cwd)?;
    if output.status == 0 {
        Ok(output)
    } else {
        Err(InstallError::ExternalCommandFailed {
            program: program.to_owned(),
            status: output.status,
            message: if output.stderr.trim().is_empty() {
                output.stdout.trim()
            } else {
                output.stderr.trim()
            }
            .to_owned(),
        })
    }
}

fn codex_project_contains_server(doc: &DocumentMut, name: &str) -> bool {
    doc.get("mcp_servers").and_then(Item::as_table_like).and_then(|table| table.get(name)).is_some()
}

fn codex_project_existing_server_name(doc: &DocumentMut, requested_name: &str) -> Option<String> {
    if codex_project_contains_server(doc, requested_name) {
        return Some(requested_name.to_owned());
    }

    legacy_server_name(requested_name)
        .filter(|legacy_name| codex_project_contains_server(doc, legacy_name))
        .map(str::to_owned)
}

fn remove_codex_project_server(mut doc: DocumentMut, server_name: Option<&str>) -> DocumentMut {
    if let Some(server_name) = server_name {
        if let Some(servers) = doc.get_mut("mcp_servers").and_then(Item::as_table_like_mut) {
            servers.remove(server_name);
        }
    }
    doc
}

fn upsert_codex_project_server(mut doc: DocumentMut, spec: &ServerSpec) -> DocumentMut {
    let root = doc.as_table_mut();
    if !matches!(root.get("mcp_servers"), Some(Item::Table(_))) {
        root.insert("mcp_servers", Item::Table(Table::new()));
    }
    let servers = root["mcp_servers"].as_table_like_mut().expect("mcp_servers table must exist");
    servers.insert(&spec.name, Item::Table(codex_server_table(spec)));
    doc
}

fn codex_server_table(spec: &ServerSpec) -> Table {
    let mut table = Table::new();
    table["command"] = value(spec.command.clone());

    let mut args = Array::default();
    for arg in &spec.args {
        args.push(arg.as_str());
    }
    table["args"] = value(args);

    if !spec.env.is_empty() {
        let mut env_table = Table::new();
        for (key, env_value) in &spec.env {
            env_table[key] = value(env_value.clone());
        }
        table["env"] = Item::Table(env_table);
    }

    table
}

fn json_existing_server_name(
    files: &dyn FileStore,
    path: &Path,
    requested_name: &str,
    servers_key: &str,
) -> Result<Option<String>, InstallError> {
    if !files.exists(path) {
        return Ok(None);
    }

    let value = serde_json::from_str::<Value>(&files.read_to_string(path).map_err(|err| {
        InstallError::ConfigRead { path: path.display().to_string(), message: err.to_string() }
    })?)
    .map_err(|err| InstallError::ConfigParse {
        path: path.display().to_string(),
        format: "JSON",
        message: err.to_string(),
    })?;

    Ok(value
        .get(servers_key)
        .and_then(Value::as_object)
        .and_then(|servers| json_object_existing_server_name(servers, requested_name)))
}

fn json_object_existing_server_name(
    servers: &Map<String, Value>,
    requested_name: &str,
) -> Option<String> {
    if servers.contains_key(requested_name) {
        return Some(requested_name.to_owned());
    }

    legacy_server_name(requested_name)
        .filter(|legacy_name| servers.contains_key(*legacy_name))
        .map(str::to_owned)
}

fn cursor_server_value(spec: &ServerSpec) -> Value {
    let mut server = Map::new();
    server.insert("type".to_owned(), Value::String("stdio".to_owned()));
    server.insert("command".to_owned(), Value::String(spec.command.clone()));
    server.insert(
        "args".to_owned(),
        Value::Array(spec.args.iter().cloned().map(Value::String).collect()),
    );
    if !spec.env.is_empty() {
        server.insert(
            "env".to_owned(),
            Value::Object(
                spec.env
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                    .collect(),
            ),
        );
    }
    Value::Object(server)
}

fn ensure_json_object<'a>(
    value: Option<&'a mut Map<String, Value>>,
    what: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, InstallError> {
    value.ok_or_else(|| InstallError::InvalidJsonShape {
        path: path.display().to_string(),
        what: what.to_owned(),
    })
}

fn legacy_server_name(requested_name: &str) -> Option<&'static str> {
    (requested_name != LEGACY_SERVER_NAME).then_some(LEGACY_SERVER_NAME)
}

fn gemini_config_path(scope: InstallScope, project_dir: &Path) -> Result<PathBuf, InstallError> {
    match scope {
        InstallScope::User => Ok(home_dir()?.join(".gemini/settings.json")),
        InstallScope::Project => Ok(project_dir.join(".gemini/settings.json")),
        InstallScope::Local => {
            Err(InstallError::UnsupportedScope { target: InstallTarget::Gemini, scope })
        }
    }
}

fn cursor_config_path(scope: InstallScope, project_dir: &Path) -> Result<PathBuf, InstallError> {
    match scope {
        InstallScope::User => Ok(home_dir()?.join(".cursor/mcp.json")),
        InstallScope::Project => Ok(project_dir.join(".cursor/mcp.json")),
        InstallScope::Local => {
            Err(InstallError::UnsupportedScope { target: InstallTarget::Cursor, scope })
        }
    }
}

fn claude_location_hint(scope: InstallScope, project_dir: &Path) -> Result<String, InstallError> {
    Ok(match scope {
        InstallScope::User => home_dir()?.join(".claude.json").display().to_string(),
        InstallScope::Project => project_dir.join(".mcp.json").display().to_string(),
        InstallScope::Local => "~/.claude.json -> projects.<cwd>.mcpServers".to_owned(),
    })
}

fn home_dir() -> Result<PathBuf, InstallError> {
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home));
    }
    if let Some(user_profile) = env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(user_profile));
    }

    match (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
        (Some(drive), Some(path)) => {
            Ok(PathBuf::from(format!("{}{}", drive.to_string_lossy(), path.to_string_lossy())))
        }
        _ => Err(InstallError::HomeDirectoryUnavailable),
    }
}

fn shell_preview(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_escape(program));
    parts.extend(args.iter().map(|arg| shell_escape(arg)));
    parts.join(" ")
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn render_dry_run_command(
    location: &str,
    action: &str,
    program: &str,
    args: &[String],
    spec: &ServerSpec,
) -> String {
    let mut lines = vec![
        format!("planned action: {action}"),
        format!("target config: {location}"),
        format!("command: {}", shell_preview(program, &redacted_args(args))),
    ];
    if let Some(warning) = secret_warning(spec) {
        lines.push(warning);
    }
    lines.join("\n")
}

fn render_dry_run_config(
    location: &str,
    action: &str,
    config: String,
    spec: &ServerSpec,
) -> String {
    let config = redact_config_preview(config, spec);
    let mut lines =
        vec![format!("planned action: {action}"), format!("target config: {location}"), config];
    if let Some(warning) = secret_warning(spec) {
        lines.push(warning);
    }
    lines.join("\n")
}

fn redacted_args(args: &[String]) -> Vec<String> {
    let mut redacted = args.to_vec();
    for index in secret_arg_value_indices(args) {
        redacted[index] = "<redacted>".to_owned();
    }
    redacted
}

fn redact_config_preview(mut config: String, spec: &ServerSpec) -> String {
    for index in secret_arg_value_indices(&spec.args) {
        config = config.replace(&spec.args[index], "<redacted>");
    }
    config
}

fn secret_arg_value_indices(args: &[String]) -> impl Iterator<Item = usize> + '_ {
    args.windows(2)
        .enumerate()
        .filter_map(|(index, pair)| (pair[0] == "--onec-password").then_some(index + 1))
}

fn secret_warning(spec: &ServerSpec) -> Option<String> {
    spec.args.windows(2).find(|pair| pair[0] == "--onec-password").map(|_| {
        "warning: --onec-password will be stored in the target MCP config as a process argument"
            .to_owned()
    })
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{BTreeMap, HashMap},
        io,
        path::{Path, PathBuf},
    };

    use crate::mcp_install::{
        model::{InstallAction, InstallScope, InstallStatus, InstallTarget, ServerSpec},
        ports::{CommandOutput, CommandRunner, FileStore},
        InstallError,
    };

    use super::{
        apply_action, build_codex_add_args, stderr_reports_missing_server,
        upsert_codex_project_server,
    };

    struct FakeRunner {
        outputs: RefCell<Vec<CommandOutput>>,
        invocations: RefCell<Vec<(String, Vec<String>, PathBuf)>>,
    }

    impl FakeRunner {
        fn new(outputs: Vec<CommandOutput>) -> Self {
            Self { outputs: RefCell::new(outputs), invocations: RefCell::new(Vec::new()) }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            cwd: &Path,
        ) -> Result<CommandOutput, InstallError> {
            self.invocations.borrow_mut().push((
                program.to_owned(),
                args.to_vec(),
                cwd.to_path_buf(),
            ));
            Ok(self.outputs.borrow_mut().remove(0))
        }
    }

    #[derive(Default)]
    struct MemoryFiles {
        files: RefCell<HashMap<PathBuf, String>>,
    }

    impl MemoryFiles {
        fn with(path: PathBuf, contents: &str) -> Self {
            let mut files = HashMap::new();
            files.insert(path, contents.to_owned());
            Self { files: RefCell::new(files) }
        }
    }

    impl FileStore for MemoryFiles {
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.files
                .borrow()
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing"))
        }

        fn write_string(&self, path: &Path, contents: &str) -> io::Result<()> {
            self.files.borrow_mut().insert(path.to_path_buf(), contents.to_owned());
            Ok(())
        }

        fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
            Ok(())
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.borrow().contains_key(path)
        }
    }

    fn spec() -> ServerSpec {
        ServerSpec {
            name: "bsl-analyzer".to_owned(),
            command: "bsl-analyzer".to_owned(),
            args: vec!["mcp".to_owned(), "--source-dir".to_owned(), ".".to_owned()],
            env: BTreeMap::new(),
        }
    }

    fn spec_named(name: &str) -> ServerSpec {
        ServerSpec {
            name: name.to_owned(),
            command: "bsl-analyzer".to_owned(),
            args: vec![
                "mcp".to_owned(),
                "serve".to_owned(),
                "--profile".to_owned(),
                "reference".to_owned(),
            ],
            env: BTreeMap::new(),
        }
    }

    fn spec_with_password() -> ServerSpec {
        ServerSpec {
            name: "bsl-analyzer".to_owned(),
            command: "bsl-analyzer".to_owned(),
            args: vec![
                "mcp".to_owned(),
                "serve".to_owned(),
                "--profile".to_owned(),
                "workspace".to_owned(),
                "--onec-password".to_owned(),
                "super-secret".to_owned(),
            ],
            env: BTreeMap::new(),
        }
    }

    fn action(target: InstallTarget, scope: InstallScope) -> InstallAction {
        InstallAction {
            target,
            scope,
            spec: spec(),
            project_dir: PathBuf::from("/workspace"),
            force: true,
            dry_run: false,
        }
    }

    fn action_with_spec(
        target: InstallTarget,
        scope: InstallScope,
        spec: ServerSpec,
    ) -> InstallAction {
        InstallAction {
            target,
            scope,
            spec,
            project_dir: PathBuf::from("/workspace"),
            force: true,
            dry_run: false,
        }
    }

    #[test]
    fn dry_run_command_preview_redacts_onec_password() {
        // Project scope never consults the client registry: an empty runner would panic on any call.
        let runner = FakeRunner::new(Vec::new());
        let files = MemoryFiles::default();
        let mut install_action =
            action_with_spec(InstallTarget::Claude, InstallScope::Project, spec_with_password());
        install_action.dry_run = true;

        let result = apply_action(&install_action, &runner, &files).expect("dry-run succeeds");

        assert_eq!(result.status, InstallStatus::DryRun);
        assert!(!result.detail.contains("super-secret"));
        assert!(result.detail.contains("<redacted>"));
        assert!(result.detail.contains("--onec-password will be stored"));
    }

    #[test]
    fn dry_run_config_preview_redacts_onec_password() {
        let runner = FakeRunner::new(Vec::new());
        let files = MemoryFiles::default();
        let mut install_action =
            action_with_spec(InstallTarget::Cursor, InstallScope::Project, spec_with_password());
        install_action.dry_run = true;

        let result = apply_action(&install_action, &runner, &files).expect("dry-run succeeds");

        assert_eq!(result.status, InstallStatus::DryRun);
        assert!(!result.detail.contains("super-secret"));
        assert!(result.detail.contains("<redacted>"));
        assert!(result.detail.contains("--onec-password will be stored"));
    }

    #[test]
    fn codex_project_merge_preserves_other_servers() {
        let initial = r#"[mcp_servers.other]
command = "other"
args = ["serve"]
"#
        .parse()
        .expect("valid toml");

        let updated = upsert_codex_project_server(initial, &spec());
        let rendered = updated.to_string();

        assert!(rendered.contains("[mcp_servers.other]"));
        assert!(rendered.contains("[mcp_servers.bsl-analyzer]"));
    }

    /// An absolute binary path with spaces / non-ASCII (what `current_exe` may yield)
    /// survives both Codex sinks intact: the `.codex/config.toml` string round-trips, and
    /// `codex mcp add -- <command>` keeps it as a single argv element (no shell-split).
    #[test]
    fn codex_preserves_command_path_with_spaces_and_unicode() {
        let mut s = spec();
        s.command = "/home/пользователь/My Apps/bsl-analyzer".to_owned();

        let doc = upsert_codex_project_server(String::new().parse().expect("valid toml"), &s);
        let reparsed: toml_edit::DocumentMut =
            doc.to_string().parse().expect("rendered toml re-parses");
        assert_eq!(
            reparsed["mcp_servers"]["bsl-analyzer"]["command"].as_str(),
            Some("/home/пользователь/My Apps/bsl-analyzer"),
            "TOML sink preserves the path verbatim"
        );

        let args = build_codex_add_args(&s);
        let sep = args.iter().position(|a| a == "--").expect("-- separator present");
        assert_eq!(
            args[sep + 1],
            "/home/пользователь/My Apps/bsl-analyzer",
            "CLI argv keeps the path as one element"
        );
    }

    #[test]
    fn codex_project_force_migrates_legacy_server_name() {
        let path = PathBuf::from("/workspace/.codex/config.toml");
        let files = MemoryFiles::with(
            path.clone(),
            r#"[mcp_servers.bsl-analyzer]
command = "old"
args = ["mcp", "--source-dir", "."]
"#,
        );
        let runner = FakeRunner::new(Vec::new());

        let result = apply_action(
            &action_with_spec(
                InstallTarget::Codex,
                InstallScope::Project,
                spec_named("bsl-analyzer-reference"),
            ),
            &runner,
            &files,
        )
        .expect("codex migration succeeds");

        assert_eq!(result.status, InstallStatus::Updated);
        let written = files.read_to_string(&path).expect("file written");
        assert!(!written.contains("[mcp_servers.bsl-analyzer]\n"));
        assert!(written.contains("[mcp_servers.bsl-analyzer-reference]"));
    }

    #[test]
    fn cursor_merge_preserves_existing_servers() {
        let path = PathBuf::from("/workspace/.cursor/mcp.json");
        let files = MemoryFiles::with(
            path.clone(),
            r#"{"mcpServers":{"other":{"type":"stdio","command":"other","args":["serve"]}}}"#,
        );
        let runner = FakeRunner::new(Vec::new());

        let result =
            apply_action(&action(InstallTarget::Cursor, InstallScope::Project), &runner, &files)
                .expect("cursor install succeeds");

        assert_eq!(result.status, InstallStatus::Installed);
        let written = files.read_to_string(&path).expect("file written");
        assert!(written.contains("\"other\""));
        assert!(written.contains("\"bsl-analyzer\""));
    }

    #[test]
    fn cursor_force_migrates_legacy_server_name() {
        let path = PathBuf::from("/workspace/.cursor/mcp.json");
        let files = MemoryFiles::with(
            path.clone(),
            r#"{"mcpServers":{"bsl-analyzer":{"type":"stdio","command":"old","args":["serve"]}}}"#,
        );
        let runner = FakeRunner::new(Vec::new());

        let result = apply_action(
            &action_with_spec(
                InstallTarget::Cursor,
                InstallScope::Project,
                spec_named("bsl-analyzer-reference"),
            ),
            &runner,
            &files,
        )
        .expect("cursor migration succeeds");

        assert_eq!(result.status, InstallStatus::Updated);
        let written = files.read_to_string(&path).expect("file written");
        assert!(!written.contains("\"bsl-analyzer\":"));
        assert!(written.contains("\"bsl-analyzer-reference\":"));
    }

    #[test]
    fn claude_user_force_reinstalls_existing_server() {
        let runner = FakeRunner::new(vec![
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        ]);
        let files = MemoryFiles::default();

        let result =
            apply_action(&action(InstallTarget::Claude, InstallScope::User), &runner, &files)
                .expect("claude install succeeds");

        assert_eq!(result.status, InstallStatus::Updated);
        let invocations = runner.invocations.borrow();
        assert_eq!(invocations.len(), 3);
        assert_eq!(invocations[0].1[1], "get");
        assert_eq!(invocations[1].1[1], "remove");
        assert_eq!(invocations[2].1[1], "add");
    }

    fn ok() -> CommandOutput {
        CommandOutput { status: 0, stdout: String::new(), stderr: String::new() }
    }

    fn failed(stderr: &str) -> CommandOutput {
        CommandOutput { status: 1, stdout: String::new(), stderr: stderr.to_owned() }
    }

    const REAL_MISSING: &str = "No MCP server named \"bsl-analyzer\". Configured servers: codex, gemini (and 3 more - run `claude mcp list` to see all)";
    const LEGACY_MISSING: &str = "No MCP server found with name: bsl-analyzer";

    fn project_mcp_json() -> PathBuf {
        PathBuf::from("/workspace").join(".mcp.json")
    }

    fn subcommands(runner: &FakeRunner) -> Vec<String> {
        runner.invocations.borrow().iter().map(|call| call.1[1].clone()).collect()
    }

    #[test]
    fn claude_project_without_mcp_json_installs_without_consulting_client() {
        let runner = FakeRunner::new(vec![ok()]);
        let files = MemoryFiles::default();

        let result =
            apply_action(&action(InstallTarget::Claude, InstallScope::Project), &runner, &files)
                .expect("project install succeeds");

        assert_eq!(result.status, InstallStatus::Installed);
        assert_eq!(subcommands(&runner), vec!["add"]);
    }

    #[test]
    fn claude_project_entry_in_mcp_json_with_force_removes_then_adds() {
        let runner = FakeRunner::new(vec![ok(), ok()]);
        let files = MemoryFiles::with(
            project_mcp_json(),
            r#"{"mcpServers":{"bsl-analyzer":{"command":"x"}}}"#,
        );

        let result =
            apply_action(&action(InstallTarget::Claude, InstallScope::Project), &runner, &files)
                .expect("project update succeeds");

        assert_eq!(result.status, InstallStatus::Updated);
        assert_eq!(subcommands(&runner), vec!["remove", "add"]);
    }

    #[test]
    fn claude_project_entry_in_mcp_json_without_force_is_already_exists() {
        let runner = FakeRunner::new(Vec::new());
        let files = MemoryFiles::with(
            project_mcp_json(),
            r#"{"mcpServers":{"bsl-analyzer":{"command":"x"}}}"#,
        );
        let mut install_action = action(InstallTarget::Claude, InstallScope::Project);
        install_action.force = false;

        let err = apply_action(&install_action, &runner, &files).expect_err("must refuse");

        assert!(matches!(err, InstallError::AlreadyExists { .. }), "got {err:?}");
    }

    #[test]
    fn claude_project_legacy_name_in_mcp_json_is_removed() {
        let runner = FakeRunner::new(vec![ok(), ok()]);
        let files = MemoryFiles::with(
            project_mcp_json(),
            r#"{"mcpServers":{"bsl-analyzer":{"command":"x"}}}"#,
        );
        let install_action = action_with_spec(
            InstallTarget::Claude,
            InstallScope::Project,
            spec_named("bsl-analyzer-workspace"),
        );

        apply_action(&install_action, &runner, &files).expect("migration succeeds");

        let invocations = runner.invocations.borrow();
        assert_eq!(invocations[0].1[1], "remove");
        assert_eq!(invocations[0].1.last().map(String::as_str), Some("bsl-analyzer"));
        assert_eq!(invocations[1].1[1], "add");
    }

    #[test]
    fn claude_user_scope_tolerates_real_missing_server_wording() {
        let runner = FakeRunner::new(vec![failed(REAL_MISSING), ok()]);
        let files = MemoryFiles::default();

        let result =
            apply_action(&action(InstallTarget::Claude, InstallScope::User), &runner, &files)
                .expect("install proceeds");

        assert_eq!(result.status, InstallStatus::Installed);
    }

    #[test]
    fn claude_user_scope_tolerates_legacy_missing_server_wording() {
        let runner = FakeRunner::new(vec![failed(LEGACY_MISSING), ok()]);
        let files = MemoryFiles::default();

        let result =
            apply_action(&action(InstallTarget::Claude, InstallScope::User), &runner, &files)
                .expect("install proceeds");

        assert_eq!(result.status, InstallStatus::Installed);
    }

    #[test]
    fn claude_user_scope_unrelated_failure_is_inspection_failed() {
        let message = "claude: EACCES: permission denied, open '/root/.claude.json'";
        let runner = FakeRunner::new(vec![failed(message)]);
        let files = MemoryFiles::default();

        let err = apply_action(&action(InstallTarget::Claude, InstallScope::User), &runner, &files)
            .expect_err("must not be treated as absent");

        match err {
            InstallError::InspectionFailed { message: got, .. } => assert_eq!(got, message),
            other => panic!("expected InspectionFailed, got {other:?}"),
        }
        assert_eq!(subcommands(&runner), vec!["get"]);
    }

    #[test]
    fn missing_server_predicate_accepts_both_wordings_only() {
        assert!(stderr_reports_missing_server(REAL_MISSING));
        assert!(stderr_reports_missing_server(LEGACY_MISSING));
        assert!(!stderr_reports_missing_server("claude: EACCES: permission denied"));
        assert!(!stderr_reports_missing_server(""));
    }
}
