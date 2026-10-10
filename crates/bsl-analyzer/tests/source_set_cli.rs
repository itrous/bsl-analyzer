//! End-to-end checks that the source-set flags actually change the analysis,
//! driven through the real `analyze` binary and its `--format jsonl` output.
//!
//! Unit tests over `SourceSetArgs` only prove that argv turns into the right
//! project model. They cannot show that the model reaches the analyzer, which
//! is the whole point of the flags: an extension analyzed without its main
//! configuration reports valid calls into that configuration as unresolved.
//!
//! Every check here is an A/B differing by exactly one flag, and the run that
//! is expected to be clean is only trusted next to a run that is not — a
//! "diagnostic absent" result on its own is equally consistent with the file
//! never having been analyzed at all.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const MAIN_MODULE: &str = "БазовыйМодуль";
const DEP_MODULE: &str = "МодульЗависимости";
const EXT_MODULE: &str = "МодульРасширения";

/// A call nothing in any source set can resolve. Kept beside the call under
/// test so that "the diagnostic disappeared" can be told apart from "the
/// diagnostic stopped being produced at all in the flagged branch".
const MISSING_MODULE: &str = "ЗаведомоНетТакогоМодуля";
const MISSING_CALL: &str = "ЗаведомоНетТакогоМодуля.НетТакогоМетода();";

/// A configuration root deep enough that `Project`'s own two-level search for
/// `Configuration.xml` cannot find it, and not under `src/cf` or `Configuration`
/// either. Without this the "no main configuration" run would silently acquire
/// one by discovery, and the control it provides would be worthless.
const MAIN: &str = "a/b/main";
const EXT: &str = "a/b/ext";
const DEP: &str = "a/b/dep";

fn configuration_xml(name: &str, module: &str, extension: bool) -> String {
    let purpose = if extension {
        "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<Configuration uuid="11111111-0000-0000-0000-000000000001">
		<Properties><Name>{name}</Name><Synonym/><Comment/><NamePrefix/>{purpose}<DefaultRunMode>ManagedApplication</DefaultRunMode></Properties>
		<ChildObjects><CommonModule>{module}</CommonModule></ChildObjects>
	</Configuration>
</MetaDataObject>"#
    )
}

fn common_module_xml(module: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
	<CommonModule uuid="22222222-0000-0000-0000-000000000002">
		<Properties><Name>{module}</Name><Synonym/><Comment/><Global>false</Global><ClientManagedApplication>false</ClientManagedApplication><Server>true</Server><ExternalConnection>false</ExternalConnection><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ServerCall>false</ServerCall><Privileged>false</Privileged><ReturnValuesReuse>DontUse</ReturnValuesReuse></Properties>
	</CommonModule>
</MetaDataObject>"#
    )
}

fn write_configuration(root: &Path, rel: &str, name: &str, module: &str, body: &str, ext: bool) {
    let dir = root.join(rel);
    let module_dir = dir.join("CommonModules").join(module).join("Ext");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(dir.join("Configuration.xml"), configuration_xml(name, module, ext)).unwrap();
    std::fs::write(
        dir.join("CommonModules").join(format!("{module}.xml")),
        common_module_xml(module),
    )
    .unwrap();
    std::fs::write(module_dir.join("Module.bsl"), body).unwrap();
}

/// Main configuration plus an extension whose module calls the main
/// configuration's exported common-module method.
fn workspace_calling_main_configuration(root: &Path) {
    write_configuration(
        root,
        MAIN,
        "ОсновнаяКонфигурация",
        MAIN_MODULE,
        "Функция Экспортируемая() Экспорт\n\tВозврат 1;\nКонецФункции\n",
        false,
    );
    write_configuration(
        root,
        EXT,
        "Расширение",
        EXT_MODULE,
        &format!(
            "Процедура Вызвать() Экспорт\n\t{MAIN_MODULE}.Экспортируемая();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
        true,
    );
}

struct Run {
    files: Vec<Value>,
    done: Value,
    stderr: String,
}

impl Run {
    /// Matched on the module's own path tail rather than on the name appearing
    /// anywhere in the absolute path: a temp directory that happens to carry the
    /// module's name in an ancestor would otherwise pick the wrong file.
    fn file_event(&self, module: &str) -> Option<&Value> {
        self.file_event_at(&format!("CommonModules/{module}/Ext/Module.bsl"))
    }

    fn file_event_at(&self, tail: &str) -> Option<&Value> {
        let tail = Path::new(tail);
        let tail = if tail.is_absolute() { tail.canonicalize().ok()? } else { tail.to_path_buf() };
        self.files
            .iter()
            .find(|e| e["path"].as_str().is_some_and(|p| Path::new(p).ends_with(&tail)))
    }

    /// The module's file event, having established that it was actually
    /// analyzed. A `file` event is emitted for files whose analysis panicked or
    /// whose text could not be read, with the failure recorded in `error` and
    /// `done.failed_files` and the process still exiting zero — so the event's
    /// mere presence proves nothing.
    fn analyzed(&self, module: &str) -> &Value {
        self.analyzed_at(&format!("CommonModules/{module}/Ext/Module.bsl"))
    }

    fn analyzed_at(&self, tail: &str) -> &Value {
        let event = self
            .file_event_at(tail)
            .unwrap_or_else(|| panic!("{tail} was not analyzed at all; jsonl: {:?}", self.files));
        assert_eq!(event["error"], Value::Null, "{tail} failed to analyze: {event}");
        assert_eq!(self.done["failed_files"], 0, "some file failed: {}", self.done);
        event
    }

    /// Messages of the given code reported for the module, in order.
    ///
    /// Compared as text rather than counted: the fixture keeps a deliberately
    /// unresolvable call beside the one under test, and a count alone cannot
    /// tell "the real call resolved" from "the two swapped places" or from the
    /// diagnostic being suppressed wholesale.
    fn messages_at(&self, tail: &str, code: &str) -> Vec<String> {
        self.analyzed_at(tail)["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["code"].as_str() == Some(code))
            .map(|d| d["message"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// Which module names the given code complains about, sorted.
    fn unresolved_modules(&self, module: &str) -> Vec<String> {
        self.unresolved_modules_at(&format!("CommonModules/{module}/Ext/Module.bsl"))
    }

    fn unresolved_modules_at(&self, tail: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .messages_at(tail, "UnresolvedMethodCall")
            .iter()
            .filter_map(|m| m.split('\'').nth(1).map(str::to_owned))
            .collect();
        names.sort();
        names
    }
}

fn analyze(source_dir: &Path, flags: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(source_dir)
        .args(flags)
        .args(["--format", "jsonl"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("failed to run the analyzer");
    // Checked for every run, including the ones inspected only through stderr:
    // the notice is printed before the walk, so a process that dies afterwards
    // still satisfies a bare `contains` and turns its paired run into noise.
    assert!(
        output.status.success(),
        "analyze {flags:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let events: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    Run {
        files: events.iter().filter(|e| e["type"] == "file").cloned().collect(),
        done: events
            .iter()
            .find(|e| e["type"] == "done")
            .cloned()
            .unwrap_or_else(|| panic!("no done event; stdout: {stdout}")),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn path(root: &Path, rel: &str) -> PathBuf {
    root.join(rel)
}

fn extension_metadata_fixture() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/extension_metadata"
    ))
}

fn assert_extension_metadata_fields(run: &Run, tail: &str, extension_visible: bool) {
    let unresolved = run.messages_at(tail, "UnresolvedField");
    let query = run.messages_at(tail, "UnknownFieldInQuery");
    for field in ["Номенклатура", "Количество"] {
        assert!(
            !unresolved.iter().any(|message| message.contains(field)),
            "{field} is inherited for {tail}; UnresolvedField: {unresolved:?}"
        );
        assert!(
            !query.iter().any(|message| message.contains(field)),
            "{field} is inherited for {tail}; UnknownFieldInQuery: {query:?}"
        );
    }
    for (code, messages) in [("UnresolvedField", &unresolved), ("UnknownFieldInQuery", &query)] {
        assert_eq!(
            messages.iter().any(|message| message.contains("РасшПоле")),
            !extension_visible,
            "extension visibility for {tail}; {code}: {messages:?}"
        );
    }
    assert!(
        unresolved.iter().any(|message| message.contains("НетТакогоПослеДобавить")),
        "the Добавить() control must fire for {tail}: {unresolved:?}"
    );
    assert!(
        query.iter().any(|message| message.contains("НетТакогоВЗапросе")),
        "the query control must fire for {tail}: {query:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_cli_covers_document_fields_common_module_and_external() {
    let root = extension_metadata_fixture();
    let run = analyze(
        &root,
        &[
            "--configuration-root",
            "base",
            "--extension",
            "Расширение=extension",
            "--external",
            "АРМ=external",
        ],
    );

    assert_extension_metadata_fields(&run, "base/Documents/Заказ/Ext/ObjectModule.bsl", false);
    assert_extension_metadata_fields(&run, "extension/Documents/Заказ/Ext/ObjectModule.bsl", true);
    assert_extension_metadata_fields(&run, "external/АРМ/Ext/ObjectModule.bsl", true);

    assert!(
        run.messages_at("extension/CommonModules/Сервер/Ext/Module.bsl", "CommonModuleNameCached")
            .is_empty(),
        "the borrowed Сервер inherits DontUse"
    );
    assert_eq!(
        run.messages_at(
            "base/CommonModules/СерверЗапросов/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "a cached module without the naming marker remains a positive control"
    );
    assert!(
        run.messages_at(
            "base/CommonModules/СерверПовтИсп/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .is_empty(),
        "a cached module with the marker remains clean"
    );
    assert_eq!(
        run.messages_at(
            "extension/CommonModules/СерверЗапросов/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "the borrowed module inherits DuringRequest"
    );
    assert_eq!(
        run.messages_at(
            "extension/CommonModules/СерверСеанса/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "the borrowed module inherits DuringSession"
    );
    assert_eq!(
        run.messages_at(
            "extension/CommonModules/СерверНеизвестный/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "explicit Unknown remains cached because only DontUse disables caching"
    );
    assert!(
        run.messages_at(
            "extension/CommonModules/СерверОтключаемый/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .is_empty(),
        "explicit DontUse in the extension disables caching"
    );
    assert_eq!(
        run.messages_at(
            "extension/CommonModules/СерверВключаемый/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1,
        "explicit DuringRequest in the extension enables caching"
    );
    let extension_unresolved =
        run.messages_at("extension/Documents/Заказ/Ext/ObjectModule.bsl", "UnresolvedField");
    assert!(
        !extension_unresolved.iter().any(|message| message.contains("Добавлено")),
        "the extension-only section field resolves: {extension_unresolved:?}"
    );
    assert!(
        extension_unresolved.iter().any(|message| message.contains("НетТакогоВРасшТаблице")),
        "the extension-only section row stays typed: {extension_unresolved:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_cli_no_extensions_keeps_base_and_rejects_extension_fields() {
    let root = extension_metadata_fixture();
    let run = analyze(
        &root,
        &["--configuration-root", "base", "--no-extensions", "--external", "АРМ=external"],
    );

    assert_extension_metadata_fields(&run, "base/Documents/Заказ/Ext/ObjectModule.bsl", false);
    assert_extension_metadata_fields(&run, "external/АРМ/Ext/ObjectModule.bsl", false);
    assert!(
        run.file_event_at("extension/Documents/Заказ/Ext/ObjectModule.bsl").is_none(),
        "the extension module must not be analyzed under --no-extensions"
    );
    for tail in [
        "base/CommonModules/Сервер/Ext/Module.bsl",
        "base/CommonModules/СерверПовтИсп/Ext/Module.bsl",
    ] {
        assert!(run.messages_at(tail, "CommonModuleNameCached").is_empty());
    }
    assert_eq!(
        run.messages_at(
            "base/CommonModules/СерверЗапросов/Ext/Module.bsl",
            "CommonModuleNameCached"
        )
        .len(),
        1
    );
    assert!(
        run.files.iter().all(|event| {
            event["path"].as_str().is_none_or(|path| !path.contains("/extension/"))
        }),
        "no extension file may enter the source set: {:?}",
        run.files
    );

    let base_only = analyze(&root, &["--configuration-root", "base"]);
    assert_extension_metadata_fields(
        &base_only,
        "base/Documents/Заказ/Ext/ObjectModule.bsl",
        false,
    );
    assert!(base_only
        .messages_at("base/CommonModules/Сервер/Ext/Module.bsl", "CommonModuleNameCached")
        .is_empty());
    assert!(
        base_only.file_event_at("extension/Documents/Заказ/Ext/ObjectModule.bsl").is_none(),
        "an ordinary base-only run must not analyze extension modules"
    );
    assert!(
        base_only.files.iter().all(|event| {
            event["path"].as_str().is_none_or(|path| !path.contains("/extension/"))
        }),
        "an ordinary base-only run must exclude every extension file: {:?}",
        base_only.files
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_cli_external_depends_on_empty_excludes_only_extension_metadata() {
    let root = extension_metadata_fixture();
    let run = analyze(
        &root,
        &[
            "--configuration-root",
            "base",
            "--extension",
            "Расширение=extension",
            "--external",
            "АРМ=external",
            "--external-depends-on",
            "АРМ=",
        ],
    );

    assert_extension_metadata_fields(&run, "external/АРМ/Ext/ObjectModule.bsl", false);
    assert_extension_metadata_fields(&run, "extension/Documents/Заказ/Ext/ObjectModule.bsl", true);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn extension_metadata_cli_external_dependency_excludes_other_selected_extension() {
    let root = extension_metadata_fixture();
    let run = analyze(
        &root,
        &[
            "--configuration-root",
            "base",
            "--extension",
            "Расширение=extension",
            "--extension",
            "Зависимое=dependent",
            "--extension-depends-on",
            "Зависимое=Расширение",
            "--external",
            "АРМ=external",
            "--external-depends-on",
            "АРМ=Расширение",
        ],
    );

    assert_extension_metadata_fields(&run, "external/АРМ/Ext/ObjectModule.bsl", true);
    for code in ["UnresolvedField", "UnknownFieldInQuery"] {
        let messages = run.messages_at("external/АРМ/Ext/ObjectModule.bsl", code);
        assert!(
            messages.iter().any(|message| message.contains("ЗависимоеПоле")),
            "metadata from the excluded dependent extension must stay invisible; {code}: {messages:?}"
        );
    }
}

#[test]
fn shared_configuration_dependency_resolves_from_project_dotenv() {
    let dir = workspace();
    let project = dir.path().join("project");
    let shared = dir.path().join("configurations");
    std::fs::create_dir_all(&project).unwrap();

    write_configuration(
        &shared,
        "UT11/11.5.22.129",
        "ОсновнаяКонфигурация",
        MAIN_MODULE,
        "Функция Экспортируемая() Экспорт\n\tВозврат 1;\nКонецФункции\n",
        false,
    );
    write_configuration(
        &project,
        "Расширения/EXT",
        "Расширение",
        EXT_MODULE,
        &format!(
            "Процедура Вызвать() Экспорт\n\t{MAIN_MODULE}.Экспортируемая();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
        true,
    );
    std::fs::write(
        project.join("bsl-analyzer.toml"),
        r#"[source]
extensions = ["Расширения/EXT"]

[source.configuration]
id = "UT11"
version = "11.5.22.129"
"#,
    )
    .unwrap();
    std::fs::write(
        project.join(".env"),
        format!("ONEC_CONFIGURATIONS_ROOT={}\n", shared.display()),
    )
    .unwrap();

    let run = analyze(&project, &[]);
    assert_eq!(
        run.unresolved_modules(EXT_MODULE),
        vec![MISSING_MODULE.to_string()],
        "the shared configuration must provide the base module context"
    );
}

#[test]
fn binding_the_main_configuration_resolves_calls_into_it() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());

    // Positive control. Without a main configuration the call cannot resolve,
    // and this assertion is what makes the paired one below mean anything.
    let standalone = analyze(dir.path(), &["--extension", &format!("EXT={EXT}")]);
    assert_eq!(
        standalone.unresolved_modules(EXT_MODULE),
        vec![MAIN_MODULE.to_string(), MISSING_MODULE.to_string()],
        "alone, neither the main configuration's module nor the missing one resolves"
    );

    let bound =
        analyze(dir.path(), &["--configuration-root", MAIN, "--extension", &format!("EXT={EXT}")]);
    assert_eq!(
        bound.unresolved_modules(EXT_MODULE),
        vec![MISSING_MODULE.to_string()],
        "binding must resolve the main configuration's module and leave only the missing one"
    );
}

#[test]
fn a_declared_dependency_resolves_calls_between_extensions() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_configuration(
        dir.path(),
        DEP,
        "Зависимость",
        DEP_MODULE,
        "Функция ИзЗависимости() Экспорт\n\tВозврат 2;\nКонецФункции\n",
        true,
    );
    std::fs::write(
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext/Module.bsl"),
        format!(
            "Процедура Вызвать() Экспорт\n\t{DEP_MODULE}.ИзЗависимости();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    )
    .unwrap();

    let declared: Vec<String> = vec![
        "--configuration-root".into(),
        MAIN.into(),
        "--extension".into(),
        format!("DEP={DEP}"),
        "--extension".into(),
        format!("EXT={EXT}"),
    ];
    let refs: Vec<&str> = declared.iter().map(String::as_str).collect();

    // Independent extensions do not see each other, so this is the control.
    let unrelated = analyze(dir.path(), &refs);
    assert_eq!(
        unrelated.unresolved_modules(EXT_MODULE),
        vec![MISSING_MODULE.to_string(), DEP_MODULE.to_string()],
        "without a declared edge the other extension's module must stay invisible"
    );

    let mut with_edge = refs.clone();
    with_edge.extend(["--extension-depends-on", "EXT=DEP"]);
    let dependent = analyze(dir.path(), &with_edge);
    assert_eq!(
        dependent.unresolved_modules(EXT_MODULE),
        vec![MISSING_MODULE.to_string()],
        "the edge must resolve the dependency's module and leave only the missing one"
    );
}

#[test]
fn no_extensions_drops_the_configured_list() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        format!("[source]\nroot = \"{MAIN}\"\nextensions = [\"{EXT}\"]\n"),
    )
    .unwrap();

    // Paired control: the flag is only shown to remove the extension if the same
    // workspace analyzes it without the flag. A wiring bug that always dropped
    // configured extensions would satisfy the one-sided check.
    let configured = analyze(dir.path(), &[]);
    configured.analyzed(EXT_MODULE);

    let opted_out = analyze(dir.path(), &["--no-extensions"]);
    assert!(
        opted_out.file_event(EXT_MODULE).is_none(),
        "--no-extensions must drop the list the config declared"
    );
    // The flag drops extensions, not the analysis. Without this, a wiring bug
    // that cleared every source root would satisfy the assertion above by
    // analyzing nothing at all.
    opted_out.analyzed(MAIN_MODULE);
}

/// How many times the notice appears — the invariant is exactly one message per
/// run, and a substring check would pass just as happily on a duplicate.
fn notices(run: &Run) -> usize {
    // The run's own health is asserted here as well: the notice is printed
    // before the walk, so a per-file failure afterwards still leaves the phrase
    // in stderr while the process exits zero — and an analysis regression for
    // exactly the extension under test would slip through.
    assert_eq!(run.done["failed_files"], 0, "some file failed: {}", run.done);
    // Counted per line, because the message embeds the source path: a workspace
    // path containing the phrase would otherwise inflate the count.
    run.stderr
        .lines()
        .filter(|line| {
            line.contains("is a configuration extension analyzed without its main configuration")
        })
        .count()
}

#[test]
fn an_extension_taken_as_the_main_root_is_reported() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_configuration(
        dir.path(),
        DEP,
        "Зависимость",
        DEP_MODULE,
        "Функция ИзЗависимости() Экспорт\n\tВозврат 2;\nКонецФункции\n",
        true,
    );

    // The case that needs no flag at all, and the one an integrator actually
    // hits: point `-s` straight at an extension. The notice is tied to the
    // resolved root, not to any flag, so narrowing it to the override path
    // would leave exactly this run silent in front of its false findings.
    assert_eq!(
        notices(&analyze(&path(dir.path(), EXT), &[])),
        1,
        "an extension given directly as the source dir must be called out once"
    );
    assert_eq!(
        notices(&analyze(&path(dir.path(), MAIN), &[])),
        0,
        "a main configuration given directly must stay silent"
    );

    // Only `--configuration-root` moves between the runs of each pair. Varying
    // the extension flags at the same time would let an implementation keyed on
    // `--no-extensions`, or on the list being empty, pass without ever asking
    // what the resolved root actually is.
    for extensions in [vec!["--no-extensions"], vec!["--extension", "DEP=a/b/dep"]] {
        let with_root = |root: &str| {
            let mut flags = vec!["--configuration-root", root];
            flags.extend(extensions.iter().copied());
            notices(&analyze(dir.path(), &flags))
        };

        assert_eq!(
            with_root(EXT),
            1,
            "an extension used as the main root must be called out once (extensions: {extensions:?})"
        );
        assert_eq!(
            with_root(MAIN),
            0,
            "a real main configuration must stay silent (extensions: {extensions:?})"
        );
    }
}

/// Drives `mcp serve` over stdio: handshake, wait for the resident database to
/// be ready, then one `diagnostics file` call. Returns the modules that stayed
/// unresolved, plus the `status` body that reported readiness.
///
/// The MCP path re-derives the project from a bare workspace path in a dozen
/// places, none of which can see argv. Nothing in the `analyze` checks above
/// would notice one of them left on the old source, so this asks the question
/// again on the channel an embedding host actually uses.
///
/// Readiness is polled rather than assumed: a freshly started server answers a
/// data action with a "still building" envelope that carries no diagnostics at
/// all, which would read exactly like "everything resolved".
/// One MCP server over stdio, driven by JSON-RPC a line at a time.
struct McpSession {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl McpSession {
    fn start(workspace: &Path, flags: &[&str]) -> Self {
        use std::process::Stdio;

        let mut child = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["mcp", "serve", "--profile", "workspace", "--mode", "stdio", "-s"])
            .arg(workspace)
            .args(flags)
            // `--mode stdio` is also the *unset* value, and the workspace profile
            // resolves that to the broker — which detaches a daemon that outlives
            // the test and writes a cache into the temp workspace.
            .env("BSL_MCP_BROKER", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start the MCP server");
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut session = Self { child, stdin: Some(stdin), stdout, next_id: 1 };
        session.request(
            "initialize",
            serde_json::json!({"protocolVersion": "2024-11-05", "capabilities": {},
                               "clientInfo": {"name": "t", "version": "1"}}),
        );
        session.send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        session
    }

    fn send(&mut self, value: Value) {
        use std::io::Write as _;
        writeln!(self.stdin.as_mut().expect("the session is open"), "{value}").unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        use std::io::BufRead as _;
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        );
        loop {
            let mut line = String::new();
            assert_ne!(self.stdout.read_line(&mut line).unwrap(), 0, "the server closed stdout");
            if let Ok(message) = serde_json::from_str::<Value>(&line) {
                if message["id"] == id {
                    return message;
                }
            }
        }
    }

    /// A tool call's reply, whole.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", serde_json::json!({"name": tool, "arguments": arguments}))
    }

    /// Poll `tool`'s `status` action until its state settles; the settled status.
    fn wait_ready(&mut self, tool: &str) -> Value {
        let mut status = Value::Null;
        for _ in 0..300 {
            status = self.call(tool, serde_json::json!({"action": "status"}))["result"]
                ["structuredContent"]
                .clone();
            if status["state"] == "ready" || status["state"] == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(status["state"], "ready", "{tool} never became ready: {status}");
        status
    }

    /// One `diagnostics` call that must succeed; its reply, whole.
    fn diagnostics(&mut self, arguments: Value) -> Value {
        let reply = self.call("diagnostics", arguments);
        let body = reply.to_string();
        assert!(!body.contains("\"isError\":true"), "the diagnostics call failed: {body}");
        reply
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

fn mcp_probe(workspace: &Path, module_file: &Path, flags: &[&str]) -> (Vec<String>, Value) {
    let mut session = McpSession::start(workspace, flags);
    let status = session.wait_ready("diagnostics");
    let body = session
        .diagnostics(
            serde_json::json!({"action": "file", "path": module_file.display().to_string()}),
        )
        .to_string();

    let mut names: Vec<String> = body
        .match_indices("разрешить получателя вызова '")
        .map(|(at, needle)| {
            let rest = &body[at + needle.len()..];
            rest[..rest.find('\'').unwrap_or(0)].to_owned()
        })
        .collect();
    names.sort();
    names.dedup();
    (names, status)
}

/// The graph's own view of the source set: node and edge counts once its build
/// settles.
///
/// Graph passes re-derive the project from a bare workspace path, separately
/// from the resident diagnostics host. A regression that left that path on the
/// on-disk config would keep every diagnostics check green while the graph was
/// built over a different set of roots.
fn mcp_graph_overview(workspace: &Path, flags: &[&str]) -> Value {
    let mut session = McpSession::start(workspace, flags);
    // `failed` is a real outcome here, not a flake: the builder reports it when
    // it panicked, and an overview read past it would compare empty to empty.
    session.wait_ready("graph");
    session.call("graph", serde_json::json!({"action": "overview"}))["result"]["structuredContent"]
        ["result"]
        .clone()
}

#[test]
fn the_source_set_reaches_the_graph() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());

    // Both runs bind the main configuration, so the only thing moving is whether
    // the extension is part of the set — which is exactly what the graph passes
    // re-derive for themselves.
    let base_only =
        mcp_graph_overview(dir.path(), &["--configuration-root", MAIN, "--no-extensions"]);
    assert_eq!(base_only["nodes"], 1, "only the main configuration's method: {base_only}");
    assert_eq!(base_only["edges"], 0, "nothing calls it: {base_only}");

    let with_extension = mcp_graph_overview(
        dir.path(),
        &["--configuration-root", MAIN, "--extension", "EXT=a/b/ext"],
    );
    assert_eq!(with_extension["nodes"], 2, "both methods: {with_extension}");
    assert_eq!(
        with_extension["edge_provenance"]["resolved"], 1,
        "the extension's call into the main configuration must resolve into an edge: \
         {with_extension}"
    );
}

/// Two configuration directories deep enough that discovery finds neither, and
/// no flags binding them: the workspace itself becomes the only declared root,
/// while each module still attributes to the nested directory that holds its
/// metadata. The build's pre-pool warm-up covers declared roots, so a second
/// attributed root reaches the whole-config loader lazily — from inside the
/// worker pool, where its fan-out may deadlock the build.
///
/// One such configuration is not enough to show this: the single module doubles
/// as the batch representative the warm-up already touches.
#[test]
fn nested_configurations_under_a_bare_workspace_still_build_a_graph() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());

    let overview = mcp_graph_overview(dir.path(), &[]);
    assert_eq!(overview["nodes"], 2, "both nested configurations' methods: {overview}");
}

#[test]
fn the_mcp_status_reports_a_standalone_extension() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    let module =
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext").join("Module.bsl");

    // Pointed straight at the extension, with no main configuration behind it —
    // the state in which this backend's findings are wrong and nothing else in
    // the protocol says why.
    let (_, standalone) = mcp_probe(&path(dir.path(), EXT), &module, &[]);
    assert!(
        standalone["standalone_extension"]
            .as_str()
            .is_some_and(|s| s.contains("configuration extension analyzed without")),
        "status must carry the notice: {standalone}"
    );

    let (_, bound) = mcp_probe(
        dir.path(),
        &module,
        &["--configuration-root", MAIN, "--extension", "EXT=a/b/ext"],
    );
    assert!(
        bound.get("standalone_extension").is_none(),
        "a bound main configuration must leave the field out: {bound}"
    );
}

#[test]
fn the_source_set_reaches_the_mcp_server() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    let module =
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext").join("Module.bsl");

    assert_eq!(
        mcp_probe(dir.path(), &module, &[]).0,
        vec![MAIN_MODULE.to_string(), MISSING_MODULE.to_string()],
        "without a source set the main configuration's module cannot resolve"
    );
    assert_eq!(
        mcp_probe(
            dir.path(),
            &module,
            &["--configuration-root", MAIN, "--extension", "EXT=a/b/ext"]
        )
        .0,
        vec![MISSING_MODULE.to_string()],
        "the source set must resolve the main configuration's module here too"
    );
}

/// An external data processor export: `<Name>.xml` beside `<Name>/`, with one
/// managed form whose module is `body`. The same tree the designer writes,
/// minus everything the analysis does not read.
const EPF: &str = "a/b/epf";
const EPF_NAME: &str = "АРМ";
const EPF_FORM_MODULE: &str = "АРМ/Forms/Форма/Ext/Form/Module.bsl";

fn write_external(root: &Path, rel: &str, name: &str, body: &str) {
    write_external_with_attribute(root, rel, name, body, None);
}

const VALID_EXTERNAL_BODY: &str =
    "Процедура Проверить() Экспорт\n    Значение = 1;\nКонецПроцедуры\n";
const BROKEN_EXTERNAL_BODY: &str =
    "Процедура Проверить() Экспорт\n    Значение = ;\nКонецПроцедуры\n";

/// Both kinds, including an ordinary form with an available textual module.
/// `managed` is a container, not an export root that discovery may recurse into.
fn nested_external_modules(root: &Path) -> Vec<PathBuf> {
    workspace_calling_main_configuration(root);
    let mut modules = Vec::new();
    let mut declarations = Vec::new();
    for (folder, name, kind) in
        [("epf", "Обработка", "ExternalDataProcessor"), ("erf", "Отчёт", "ExternalReport")]
    {
        let relative = format!("src/{folder}/managed/{name}");
        let dir = root.join(&relative);
        write_external(root, &relative, name, BROKEN_EXTERNAL_BODY);
        std::fs::write(dir.join(format!("{name}.xml")), processor_xml(kind, name, None)).unwrap();
        let object = dir.join(name).join("Ext/ObjectModule.bsl");
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, BROKEN_EXTERNAL_BODY).unwrap();
        let ordinary = dir.join(name).join("Forms/Обычная/Ext/Form/Module.bsl");
        std::fs::create_dir_all(ordinary.parent().unwrap()).unwrap();
        std::fs::write(&ordinary, BROKEN_EXTERNAL_BODY).unwrap();
        let form_xml = std::fs::read_to_string(dir.join(name).join("Forms/Форма.xml")).unwrap();
        std::fs::write(
            dir.join(name).join("Forms/Обычная.xml"),
            form_xml
                .replace("<Name>Форма</Name>", "<Name>Обычная</Name>")
                .replace("<FormType>Managed</FormType>", "<FormType>Ordinary</FormType>"),
        )
        .unwrap();
        let xml = processor_xml(kind, name, None)
            .replace("<Form>Форма</Form>", "<Form>Форма</Form><Form>Обычная</Form>");
        std::fs::write(dir.join(format!("{name}.xml")), xml).unwrap();
        modules.extend([object, dir.join(name).join("Forms/Форма/Ext/Form/Module.bsl"), ordinary]);
        declarations.push(format!("{{ name = \"{name}\", path = \"{relative}\" }}"));
    }
    std::fs::write(
        root.join("bsl-analyzer.toml"),
        format!(
            "[source]\nroot = \"{MAIN}\"\nextensions = []\nexternals = [{}]\n",
            declarations.join(",")
        ),
    )
    .unwrap();
    modules.into_iter().map(|module| module.canonicalize().unwrap()).collect()
}

fn external_diagnostics(session: &mut McpSession, path: &Path) -> Value {
    session.diagnostics(serde_json::json!({"action": "file", "path": path,
        "codes": ["ParseError"]}))["result"]["structuredContent"]
        .clone()
}

fn wait_external_diagnostics(
    session: &mut McpSession,
    path: &Path,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let body = external_diagnostics(session, path);
        if body["stale"] == false && accept(&body) {
            return body;
        }
        assert!(std::time::Instant::now() < deadline, "{} never settled: {body}", path.display());
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn wait_file_diagnostics(
    session: &mut McpSession,
    path: &Path,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let body = session.diagnostics(serde_json::json!({
            "action": "file",
            "path": path.display().to_string(),
            "codes": ["UnknownFieldInQuery"]
        }))["result"]["structuredContent"]
            .clone();
        if body["stale"] == false && accept(&body) {
            return body;
        }
        assert!(std::time::Instant::now() < deadline, "{} never settled: {body}", path.display());
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn wait_query_semantics(session: &mut McpSession, query: &str, root_id: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let reply = session.call(
            "query",
            serde_json::json!({"action": "validate", "query": query, "root_id": root_id}),
        );
        assert!(reply["error"].is_null(), "query validate failed: {reply}");
        let answer = reply["result"]["structuredContent"].clone();
        if answer["results"][0]["backend"] == "workspace_semantics" {
            return answer;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "query semantics never became ready: {answer}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn document_configuration_xml(name: &str, extension: bool) -> String {
    let purpose = if extension {
        "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses"><Configuration uuid="11111111-0000-0000-0000-000000000001"><Properties><Name>{name}</Name>{purpose}</Properties><ChildObjects><Document>Документ1</Document></ChildObjects></Configuration></MetaDataObject>"#
    )
}

fn effective_document_xml(extension: bool, base_attribute: &str) -> String {
    let identity = if extension {
        "<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>11111111-1111-1111-1111-111111111111</ExtendedConfigurationObject>"
    } else {
        ""
    };
    let uuid = if extension {
        "22222222-2222-2222-2222-222222222222"
    } else {
        "11111111-1111-1111-1111-111111111111"
    };
    let attribute = if extension { "ДобавленноеПоле" } else { base_attribute };
    let attribute_uuid = if extension {
        "66666666-6666-6666-6666-666666666666"
    } else {
        "44444444-4444-4444-4444-444444444444"
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
<Document uuid="{uuid}"><Properties><Name>Документ1</Name>{identity}</Properties><ChildObjects>
<TabularSection uuid="33333333-3333-3333-3333-333333333333"><Properties><Name>Товары</Name></Properties><ChildObjects>
<Attribute uuid="{attribute_uuid}"><Properties><Name>{attribute}</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>
</ChildObjects></TabularSection></ChildObjects></Document></MetaDataObject>"#
    )
}

/// Editing a base attribute while the resident is open must refresh both its file
/// diagnostics and query validation. The adopted extension keeps its own added field
/// through the drift and after the base is restored.
#[test]
fn adopted_document_base_attribute_drift_reaches_resident_diagnostics_and_query_validate() {
    let dir = workspace();
    let base = path(dir.path(), MAIN);
    let extension = path(dir.path(), EXT);
    for (root, config_name, is_extension) in
        [(&base, "ОсновнаяКонфигурация", false), (&extension, "Расширение", true)]
    {
        std::fs::create_dir_all(root.join("Documents")).unwrap();
        std::fs::write(
            root.join("Configuration.xml"),
            document_configuration_xml(config_name, is_extension),
        )
        .unwrap();
        std::fs::write(
            root.join("Documents/Документ1.xml"),
            effective_document_xml(is_extension, "БазовоеПоле"),
        )
        .unwrap();
    }
    let module = extension.join("Documents/Документ1/Ext/ManagerModule.bsl");
    std::fs::create_dir_all(module.parent().unwrap()).unwrap();
    let query = "ВЫБРАТЬ Т.БазовоеПоле, Т.ДобавленноеПоле ИЗ Документ.Документ1.Товары КАК Т";
    let source = format!(
        "Процедура Проверить() Экспорт\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"{query}\";\n    Запрос.Выполнить();\nКонецПроцедуры\n"
    );
    std::fs::write(&module, source).unwrap();
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        format!(
            "[source]\nroot = \"{MAIN}\"\nextensions = [{{ name = \"EXT\", path = \"{EXT}\" }}]\n"
        ),
    )
    .unwrap();

    let flags = ["--configuration-root", MAIN, "--extension", "EXT=a/b/ext"];
    let mut session = McpSession::start(dir.path(), &flags);
    session.wait_ready("diagnostics");

    let initial = wait_file_diagnostics(&mut session, &module, |answer| {
        answer["result"]["kind"] == "full"
            && answer["result"]["findings"].as_array().is_some_and(Vec::is_empty)
    });
    session.wait_ready("search");
    let search_reply = session.call(
        "search",
        serde_json::json!({"action": "search_code", "query": "БазовоеПоле", "limit": 50}),
    );
    assert!(search_reply["error"].is_null(), "search_code failed: {search_reply}");
    let hits = search_reply["result"]["structuredContent"]["hits"]
        .as_array()
        .expect("search_code returns structured hits");
    let module_root_ids: Vec<_> = hits
        .iter()
        .filter(|hit| {
            hit["path"].as_str().is_some_and(|path| {
                Path::new(path).ends_with("Documents/Документ1/Ext/ManagerModule.bsl")
            })
        })
        .map(|hit| hit["root_id"].as_str().expect("code hit names its source root"))
        .collect();
    assert!(!module_root_ids.is_empty(), "search finds the module's indexed query text: {hits:?}");
    assert!(
        module_root_ids.iter().all(|root_id| *root_id == module_root_ids[0]),
        "all module hits name the same source root: {module_root_ids:?}"
    );
    let extension_root_id = module_root_ids[0].to_owned();
    let initial_query = wait_query_semantics(&mut session, query, &extension_root_id);
    assert!(
        initial_query["results"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["code"] != "UnknownFieldInQuery"),
        "base and adopted fields should both resolve: {initial_query}"
    );
    assert_eq!(initial_query["context"]["asserted_root_id"], extension_root_id);
    let missing_query = "ВЫБРАТЬ Т.НетТакогоПоля ИЗ Документ.Документ1.Товары КАК Т";
    let negative = wait_query_semantics(&mut session, missing_query, &extension_root_id);
    assert!(
        negative["results"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "UnknownFieldInQuery"),
        "negative control proves workspace metadata semantics are active: {negative}"
    );

    let base_xml = base.join("Documents/Документ1.xml");
    let base_before = std::fs::read_to_string(&base_xml).unwrap();
    let drifted = base_before.replace("БазовоеПоле", "ПереименованноеПоле");
    std::fs::write(&base_xml, drifted).unwrap();
    let drift_diagnostics = wait_file_diagnostics(&mut session, &module, |answer| {
        answer["revision"].as_u64() > initial["revision"].as_u64()
            && answer["result"]["findings"].as_array().is_some_and(|findings| {
                findings.iter().any(|finding| finding["code"] == "UnknownFieldInQuery")
            })
    });
    let drift_query = wait_query_semantics(&mut session, query, &extension_root_id);
    assert!(
        drift_query["results"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "UnknownFieldInQuery"),
        "query validation must agree with fresh diagnostics after base drift: {drift_query}"
    );
    assert!(drift_diagnostics["revision"].as_u64() > initial["revision"].as_u64());

    std::fs::write(&base_xml, base_before).unwrap();
    let restored = wait_file_diagnostics(&mut session, &module, |answer| {
        answer["revision"].as_u64() > drift_diagnostics["revision"].as_u64()
            && answer["result"]["findings"].as_array().is_some_and(Vec::is_empty)
    });
    let restored_query = wait_query_semantics(&mut session, query, &extension_root_id);
    assert!(
        restored_query["results"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["code"] != "UnknownFieldInQuery"),
        "restoring the base field must restore the effective query schema: {restored_query}"
    );
    assert!(restored["revision"].as_u64() > drift_diagnostics["revision"].as_u64());
}

#[test]
fn nested_external_text_modules_have_cli_and_mcp_parse_errors_and_recover() {
    let dir = workspace();
    let modules = nested_external_modules(dir.path());
    let config_path = dir.path().join("bsl-analyzer.toml");
    let explicit_config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, format!("[source]\nroot = \"{MAIN}\"\nextensions = []\n"))
        .unwrap();
    let undiscovered = analyze(dir.path(), &[]);
    for module in &modules {
        assert!(undiscovered.file_event_at(module.to_str().unwrap()).is_none());
    }
    assert!(undiscovered.stderr.contains("managed"), "{}", undiscovered.stderr);
    std::fs::write(config_path, explicit_config).unwrap();
    let disabled = analyze(dir.path(), &["--no-externals"]);
    for module in &modules {
        assert!(disabled.file_event_at(module.to_str().unwrap()).is_none());
    }

    let broken = analyze(dir.path(), &[]);
    let mut session = McpSession::start(dir.path(), &[]);
    session.wait_ready("diagnostics");
    let mut initial = Vec::new();
    for module in &modules {
        assert!(!broken.messages_at(module.to_str().unwrap(), "ParseError").is_empty());
        let answer = external_diagnostics(&mut session, module);
        assert_eq!(answer["stale"], false, "{answer}");
        assert_eq!(answer["result"]["kind"], "full", "{answer}");
        assert_eq!(answer["result"]["truncated"], false, "{answer}");
        let findings =
            answer["result"]["findings"].as_array().expect("findings, not not_in_workspace");
        assert!(!findings.is_empty(), "{answer}");
        let cli_findings: Vec<_> = broken.analyzed_at(module.to_str().unwrap())["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| finding["code"] == "ParseError")
            .collect();
        assert_eq!(findings.len(), cli_findings.len());
        for (finding, cli) in findings.iter().zip(cli_findings) {
            assert_eq!(finding["code"], "ParseError", "{finding}");
            assert!(finding["location"]["root_id"].is_string(), "{finding}");
            for coordinate in ["start_line", "start_column", "end_line", "end_column"] {
                assert_eq!(finding["range"][coordinate], cli[coordinate]);
            }
        }
        let rooted = session.diagnostics(serde_json::json!({
            "action": "file", "root_id": findings[0]["location"]["root_id"],
            "path": findings[0]["location"]["path"], "codes": ["ParseError"]
        }));
        assert_eq!(
            rooted["result"]["structuredContent"]["result"]["result_id"],
            answer["result"]["result_id"],
            "the published root/path must name this module"
        );
        initial.push(answer);
    }
    for module in &modules {
        std::fs::write(module, VALID_EXTERNAL_BODY).unwrap();
    }
    let corrected = analyze(dir.path(), &[]);
    for (module, before) in modules.iter().zip(initial) {
        assert!(corrected.messages_at(module.to_str().unwrap(), "ParseError").is_empty());
        wait_external_diagnostics(&mut session, module, |answer| {
            answer["revision"].as_u64() > before["revision"].as_u64()
                && answer["result"]["kind"] == "full"
                && answer["result"]["result_id"] != before["result"]["result_id"]
                && answer["result"]["findings"].as_array().is_some_and(Vec::is_empty)
        });
    }
}

#[test]
fn nested_external_module_addition_and_removal_reach_the_resident() {
    let dir = workspace();
    let modules = nested_external_modules(dir.path());
    let module = modules.last().unwrap();
    std::fs::remove_file(module).unwrap();
    let mut session = McpSession::start(dir.path(), &[]);
    session.wait_ready("diagnostics");
    let missing = external_diagnostics(&mut session, module);
    assert_eq!(missing["result"]["error"], "not_in_workspace", "{missing}");
    std::fs::write(module, BROKEN_EXTERNAL_BODY).unwrap();
    let added = wait_external_diagnostics(&mut session, module, |answer| {
        answer["result"]["kind"] == "full"
            && answer["result"]["findings"].as_array().is_some_and(|v| !v.is_empty())
    });
    std::fs::remove_file(module).unwrap();
    wait_external_diagnostics(&mut session, module, |answer| {
        answer["revision"].as_u64() > added["revision"].as_u64()
            && answer["result"]["error"] == "not_in_workspace"
    });
}

#[test]
fn a_binary_external_is_refused_instead_of_reported_clean() {
    let dir = workspace();
    std::fs::write(dir.path().join("Binary.epf"), [0u8, 1, 2, 3]).unwrap();
    let error = analyze_refuses(dir.path(), &["--external", "Binary=Binary.epf"]);
    assert!(error.contains("Binary"), "the refused input must be named: {error}");
}

#[test]
fn native_syntax_regressions_have_matching_cli_and_mcp_ranges() {
    let dir = workspace();
    let modules = nested_external_modules(dir.path());
    for module in &modules {
        std::fs::write(module, VALID_EXTERNAL_BODY).unwrap();
    }
    let module = &modules[0];
    let mut session = McpSession::start(dir.path(), &[]);
    session.wait_ready("diagnostics");
    let mut before = external_diagnostics(&mut session, module);
    // These fragment pairs were checked on platform 8.3.27.1989 in demo.
    for (bad, good) in [
        ("Если Истина Тогда\nКонецЕслли;", "Если Истина Тогда\nКонецЕсли;"),
        (
            "ПолныйПуть = Новый Файл(\"a\").ПолноеИмя;",
            "Файл = Новый Файл(\"a\");\nПолныйПуть = Файл.ПолноеИмя;",
        ),
        (
            "Если Новый Файл(\"a\").Существует() Тогда\nКонецЕсли;",
            "Файл = Новый Файл(\"a\");\nЕсли Файл.Существует() Тогда\nКонецЕсли;",
        ),
    ] {
        for (body, broken) in [(bad, true), (good, false)] {
            std::fs::write(
                module,
                format!("Процедура Проверить() Экспорт\n{body}\nКонецПроцедуры\n"),
            )
            .unwrap();
            let answer = wait_external_diagnostics(&mut session, module, |answer| {
                answer["revision"].as_u64() > before["revision"].as_u64()
                    && answer["result"]["kind"] == "full"
                    && answer["result"]["findings"]
                        .as_array()
                        .is_some_and(|findings| findings.is_empty() != broken)
            });
            let cli = analyze(dir.path(), &[]);
            let cli_findings: Vec<_> = cli.analyzed_at(module.to_str().unwrap())["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|finding| finding["code"] == "ParseError")
                .collect();
            let findings = answer["result"]["findings"].as_array().unwrap();
            assert_eq!(findings.len(), cli_findings.len(), "{body}");
            assert_eq!(answer["result"]["truncated"], false);
            for (finding, cli) in findings.iter().zip(cli_findings) {
                for coordinate in ["start_line", "start_column", "end_line", "end_column"] {
                    assert_eq!(finding["range"][coordinate], cli[coordinate], "{body}");
                }
            }
            before = answer;
        }
    }
}

/// The processor's XML, internal or external: `element` is the object element and
/// `attribute`, when given, one string attribute of the object.
fn processor_xml(element: &str, name: &str, attribute: Option<&str>) -> String {
    let attribute = attribute.map_or(String::new(), |attribute| {
        format!(
            r#"<Attribute uuid="d010948a-27f1-4b21-80a2-361efec05def"><Properties><Name>{attribute}</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>"#
        )
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<{element} uuid="3696c164-ad14-4a0d-b659-10e3bf6d6ad2">
		<Properties><Name>{name}</Name><Synonym/><Comment/><DefaultForm>{element}.{name}.Form.Форма</DefaultForm></Properties>
		<ChildObjects>{attribute}<Form>Форма</Form></ChildObjects>
	</{element}>
</MetaDataObject>"#
    )
}

fn write_external_with_attribute(
    root: &Path,
    rel: &str,
    name: &str,
    body: &str,
    attribute: Option<&str>,
) {
    let dir = root.join(rel);
    let form_dir = dir.join(name).join("Forms").join("Форма").join("Ext").join("Form");
    std::fs::create_dir_all(&form_dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.xml")),
        processor_xml("ExternalDataProcessor", name, attribute),
    )
    .unwrap();
    std::fs::write(
        dir.join(name).join("Forms").join("Форма.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<Form uuid="8919791a-5b27-410f-9404-010ce96c6db6">
		<Properties><Name>Форма</Name><Synonym/><Comment/><FormType>Managed</FormType></Properties>
	</Form>
</MetaDataObject>"#,
    )
    .unwrap();
    std::fs::write(
        form_dir.parent().unwrap().join("Form.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.20">
	<AutoCommandBar name="ФормаКоманднаяПанель" id="-1"/>
	<Attributes/>
</Form>"#,
    )
    .unwrap();
    std::fs::write(form_dir.join("Module.bsl"), body).unwrap();
}

/// Runs `analyze` expecting it to refuse, and returns its stderr.
fn analyze_refuses(source_dir: &Path, flags: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .arg("analyze")
        .arg("-s")
        .arg(source_dir)
        .args(flags)
        .args(["--format", "jsonl"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .expect("failed to run the analyzer");
    assert!(!output.status.success(), "analyze {flags:?} must refuse to start");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn external_notices(run: &Run) -> usize {
    assert_eq!(run.done["failed_files"], 0, "some file failed: {}", run.done);
    run.stderr
        .lines()
        .filter(|line| line.contains("are analyzed without an owning configuration"))
        .count()
}

#[test]
fn binding_the_main_configuration_resolves_calls_from_an_external_object() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    );
    let external = format!("{EPF_NAME}={EPF}");

    // Positive control: alone, the owning configuration's module is unresolved
    // and the run says why. Without this the bound run's clean result could
    // equally mean the form module was never analyzed.
    let standalone = analyze(dir.path(), &["--external", &external]);
    assert_eq!(
        standalone.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MAIN_MODULE.to_string(), MISSING_MODULE.to_string()],
    );
    assert_eq!(external_notices(&standalone), 1, "the missing owner is called out once");

    let bound = analyze(dir.path(), &["--configuration-root", MAIN, "--external", &external]);
    assert_eq!(
        bound.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string()],
        "the owning configuration's module resolves and only the missing one remains"
    );
    assert_eq!(external_notices(&bound), 0, "bound, there is nothing to announce");
}

#[test]
fn an_external_object_sees_every_extension_while_the_base_sees_none() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    // The extension exports a method; the base calls it too, as the control:
    // an extension's API is invisible from the base, so the same call that
    // resolves from the external object must stay unresolved there.
    std::fs::write(
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext/Module.bsl"),
        "Функция ИзРасширения() Экспорт\n\tВозврат 3;\nКонецФункции\n",
    )
    .unwrap();
    std::fs::write(
        path(dir.path(), MAIN).join("CommonModules").join(MAIN_MODULE).join("Ext/Module.bsl"),
        format!(
            "Функция Экспортируемая() Экспорт\n\tВозврат 1;\nКонецФункции\n\
             Процедура Контроль()\n\t{EXT_MODULE}.ИзРасширения();\nКонецПроцедуры\n"
        ),
    )
    .unwrap();
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{EXT_MODULE}.ИзРасширения();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    );
    let external = format!("{EPF_NAME}={EPF}");
    // No dependency is declared anywhere: the external object sees the
    // extension by construction, not by an edge.
    let run = analyze(
        dir.path(),
        &["--configuration-root", MAIN, "--extension", "EXT=a/b/ext", "--external", &external],
    );

    assert_eq!(
        run.unresolved_modules(MAIN_MODULE),
        vec![EXT_MODULE.to_string()],
        "control: the base does not see the extension"
    );
    assert_eq!(
        run.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string()],
        "the external object sees the extension without any declared edge"
    );
}

#[test]
fn an_external_depends_on_narrows_what_it_sees_to_the_named_extensions() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    std::fs::write(
        path(dir.path(), EXT).join("CommonModules").join(EXT_MODULE).join("Ext/Module.bsl"),
        "Функция ИзРасширения() Экспорт\n\tВозврат 3;\nКонецФункции\n",
    )
    .unwrap();
    write_configuration(
        dir.path(),
        DEP,
        "Зависимость",
        DEP_MODULE,
        "Функция ИзЗависимости() Экспорт\n\tВозврат 4;\nКонецФункции\n",
        true,
    );
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\n\t{EXT_MODULE}.ИзРасширения();\n\t{DEP_MODULE}.ИзЗависимости();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    );
    let external = format!("{EPF_NAME}={EPF}");
    let flags = |extra: &[&str]| -> Vec<String> {
        [
            "--configuration-root",
            MAIN,
            "--extension",
            "EXT=a/b/ext",
            "--extension",
            "DEP=a/b/dep",
            "--external",
            &external,
        ]
        .into_iter()
        .chain(extra.iter().copied())
        .map(str::to_owned)
        .collect()
    };
    fn as_str(flags: &[String]) -> Vec<&str> {
        flags.iter().map(String::as_str).collect()
    }

    let every = analyze(dir.path(), &as_str(&flags(&[])));
    assert_eq!(
        every.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string()],
        "control: without dependsOn both extensions are visible"
    );

    let narrowed = analyze(dir.path(), &as_str(&flags(&["--external-depends-on", "АРМ=EXT"])));
    assert_eq!(
        narrowed.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string(), DEP_MODULE.to_string()],
        "narrowed to EXT, the other extension's module is gone and the base stays"
    );

    // The base alone, declared in the file: `dependsOn = []` is not "no key".
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        format!(
            "[source]\nroot = \"{MAIN}\"\nextensions = [\n  {{ name = \"EXT\", path = \"{EXT}\" }},\n  {{ name = \"DEP\", path = \"{DEP}\" }},\n]\nexternals = [{{ name = \"{EPF_NAME}\", path = \"{EPF}\", dependsOn = [] }}]\n"
        ),
    )
    .unwrap();
    let base_only = analyze(dir.path(), &[]);
    assert_eq!(
        base_only.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string(), DEP_MODULE.to_string(), EXT_MODULE.to_string()],
        "an empty list leaves the base visible and nothing else"
    );
}

#[test]
fn an_external_under_src_epf_is_discovered_unless_opted_out() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(
        dir.path(),
        "src/epf/АРМ",
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\n\t{MISSING_CALL}\nКонецПроцедуры\n"
        ),
    );
    let discovered = analyze(dir.path(), &["--configuration-root", MAIN]);
    assert_eq!(
        discovered.unresolved_modules_at(EPF_FORM_MODULE),
        vec![MISSING_MODULE.to_string()],
        "found without a declaration, and bound to the owner"
    );
    assert_eq!(external_notices(&discovered), 0);

    let opted_out = analyze(dir.path(), &["--configuration-root", MAIN, "--no-externals"]);
    assert!(
        opted_out.file_event_at(EPF_FORM_MODULE).is_none(),
        "control: opted out, the export is not a root and its module is not analyzed"
    );
}

#[test]
fn a_root_declared_under_the_wrong_key_is_refused_by_name() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(dir.path(), EPF, EPF_NAME, "Процедура П()\nКонецПроцедуры\n");

    let as_extension = analyze_refuses(
        dir.path(),
        &["--configuration-root", MAIN, "--extension", &format!("{EPF_NAME}={EPF}")],
    );
    // The CLI renders a project error in its Debug form, so the variant name is
    // what reaches the operator; matching on it keeps the check honest either way.
    assert!(
        as_extension.contains("StructuredNotAnExtension"),
        "an export named as an extension: {as_extension}"
    );

    let as_external = analyze_refuses(
        dir.path(),
        &["--configuration-root", MAIN, "--external", &format!("EXT={EXT}")],
    );
    assert!(
        as_external.contains("ExternalIsAConfiguration"),
        "an extension named as an external: {as_external}"
    );

    let inside = analyze_refuses(
        dir.path(),
        &["--configuration-root", MAIN, "--external", &format!("CM={MAIN}/CommonModules")],
    );
    // `CommonModules/` holds exactly one object XML in this fixture, so it is
    // refused by what that XML describes, naming the element it found.
    assert!(
        inside.contains("ExternalNotAnExternalObject") && inside.contains("CommonModule"),
        "a directory that is not one export: {inside}"
    );
}

#[test]
fn the_mcp_status_reports_an_external_object_without_its_owner() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\nКонецПроцедуры\n"
        ),
    );
    let module = path(dir.path(), EPF).join(EPF_FORM_MODULE);
    let external = format!("{EPF_NAME}={EPF}");

    let (unresolved, standalone) = mcp_probe(dir.path(), &module, &["--external", &external]);
    assert_eq!(unresolved, vec![MAIN_MODULE.to_string()], "control: alone, the owner is missing");
    assert!(
        standalone["standalone_extension"]
            .as_str()
            .is_some_and(|s| s.contains("analyzed without an owning configuration")),
        "status must carry the notice: {standalone}"
    );

    let (unresolved, bound) =
        mcp_probe(dir.path(), &module, &["--configuration-root", MAIN, "--external", &external]);
    assert!(unresolved.is_empty(), "bound, the owner's module resolves: {unresolved:?}");
    assert!(
        bound.get("standalone_extension").is_none(),
        "a bound owning configuration must leave the field out: {bound}"
    );
}

/// The `workspace` sweep aggregates the same findings the `file` action flags, so
/// it must carry the same advisory when the project has no owner for its external
/// objects: a consumer reading only that envelope must not take the unresolved
/// calls for real.
#[test]
fn the_diagnostics_workspace_action_reports_a_missing_owner_like_the_file_action() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    write_external(
        dir.path(),
        EPF,
        EPF_NAME,
        &format!(
            "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n\t{MAIN_MODULE}.Экспортируемая();\nКонецПроцедуры\n"
        ),
    );
    let module = path(dir.path(), EPF).join(EPF_FORM_MODULE);
    let external = format!("{EPF_NAME}={EPF}");
    const REASON: &str = "owning_configuration_missing";

    let names_reason = |flags: &[&str], arguments: Value| -> bool {
        let mut session = McpSession::start(dir.path(), flags);
        session.wait_ready("diagnostics");
        session.diagnostics(arguments).to_string().contains(REASON)
    };
    let file = serde_json::json!({"action": "file", "path": module.display().to_string()});
    let sweep = serde_json::json!({"action": "workspace"});

    let alone: &[&str] = &["--external", &external];
    assert!(names_reason(alone, file), "control: the file action names the missing owner");
    assert!(names_reason(alone, sweep.clone()), "the workspace sweep names it too");

    let bound: &[&str] = &["--configuration-root", MAIN, "--external", &external];
    assert!(!names_reason(bound, sweep), "bound, the sweep carries no such reason");
}

// Prefixed by the root directory: the base carries a namesake at
// `DataProcessors/АРМ/Ext/ObjectModule.bsl`, and a bare tail would find it first.
const EPF_OBJECT_MODULE: &str = "epf/АРМ/Ext/ObjectModule.bsl";

/// A method that exists on no object: the negative control for `UnresolvedMethodCall`.
const BOGUS_CALL: &str = "\tЭтотОбъект.ЗаведомоНетТакогоМетода();\n";

fn object_module_body(attribute: &str) -> String {
    format!(
        "Процедура ОбработкаПроведения() Экспорт\n\tЗначение = ЭтотОбъект.{attribute};\n\tОпечатка = ЭтотОбъект.{attribute}ЛишняяБуква;\nКонецПроцедуры\n"
    )
}

/// Which attributes `UnresolvedField` complains about, sorted.
fn unresolved_fields(run: &Run, tail: &str) -> Vec<String> {
    let mut names: Vec<String> = run
        .messages_at(tail, "UnresolvedField")
        .iter()
        .filter_map(|m| m.split('\'').nth(1).map(str::to_owned))
        .collect();
    names.sort();
    names
}

#[test]
fn an_external_object_module_knows_its_own_attributes_and_not_a_namesakes() {
    let dir = workspace();
    workspace_calling_main_configuration(dir.path());
    // The base carries an INTERNAL processor of the same name with a different
    // attribute — the ERP shape, where the export is a copy of a built-in one.
    let internal = path(dir.path(), MAIN).join("DataProcessors");
    std::fs::create_dir_all(internal.join("АРМ/Ext")).unwrap();
    std::fs::write(
        internal.join("АРМ.xml"),
        processor_xml("DataProcessor", "АРМ", Some("Внутренний")),
    )
    .unwrap();
    std::fs::write(
        internal.join("АРМ/Ext/ObjectModule.bsl"),
        format!(
            "{}{BOGUS_CALL}КонецПроцедуры\n",
            object_module_body("Внутренний").trim_end_matches("КонецПроцедуры\n")
        ),
    )
    .unwrap();

    write_external_with_attribute(
        dir.path(),
        EPF,
        "АРМ",
        "Процедура П()\nКонецПроцедуры\n",
        Some("Внешний"),
    );
    let epf_object = path(dir.path(), EPF).join("АРМ/Ext");
    std::fs::create_dir_all(&epf_object).unwrap();
    std::fs::write(
        epf_object.join("ObjectModule.bsl"),
        format!(
            "{}\tЧужой = ЭтотОбъект.Внутренний;\n{BOGUS_CALL}КонецПроцедуры\n",
            object_module_body("Внешний").trim_end_matches("КонецПроцедуры\n")
        ),
    )
    .unwrap();

    let run = analyze(dir.path(), &["--configuration-root", MAIN, "--external", "АРМ=a/b/epf"]);

    // The internal module is the equivalence control: same shape, same verdicts.
    assert_eq!(
        unresolved_fields(&run, "DataProcessors/АРМ/Ext/ObjectModule.bsl"),
        vec!["ВнутреннийЛишняяБуква".to_string()],
        "control: the internal processor resolves its attribute and flags the typo"
    );
    assert_eq!(
        unresolved_fields(&run, EPF_OBJECT_MODULE),
        vec!["ВнешнийЛишняяБуква".to_string(), "Внутренний".to_string()],
        "the external object owns Внешний, flags its typo, and does not borrow the \
         namesake's Внутренний"
    );

    // A call that exists nowhere must be flagged on both: the external kind has
    // no manager collection to name the receiver by, and that must not turn
    // into silence.
    let bogus_calls = |tail: &str| run.messages_at(tail, "UnresolvedMethodCall");
    assert!(
        bogus_calls("DataProcessors/АРМ/Ext/ObjectModule.bsl")
            .iter()
            .any(|m| m.contains("ЗаведомоНетТакогоМетода")),
        "control: the internal processor flags the bogus call"
    );
    let external_calls = bogus_calls(EPF_OBJECT_MODULE);
    assert!(
        external_calls
            .iter()
            .any(|m| m.contains("ЗаведомоНетТакогоМетода") && m.contains("ВнешняяОбработка.АРМ")),
        "the external object flags the bogus call and names itself: {external_calls:?}"
    );
}
