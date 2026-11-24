use std::path::Path;
use std::process::Command;

use serde_json::Value;

const DUPLICATES: &str = include_str!("../../ide-diagnostics/src/handlers/fixtures/method_declaration_conflicts/duplicate_methods.bsl");

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn full_epf_and_erf_source_analysis_contains_separate_blockers_without_parse_errors() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    write(
        root,
        "src/cf/Configuration.xml",
        r#"<MetaDataObject><Configuration uuid="00000000-0000-0000-0000-000000000001"><Properties><Name>Host</Name><DefaultRunMode>ManagedApplication</DefaultRunMode></Properties><ChildObjects><CommonModule>Good</CommonModule></ChildObjects></Configuration></MetaDataObject>"#,
    );
    write(
        root,
        "src/cf/CommonModules/Good.xml",
        r#"<MetaDataObject><CommonModule uuid="00000000-0000-0000-0000-000000000002"><Properties><Name>Good</Name><Server>true</Server><ClientManagedApplication>false</ClientManagedApplication><ClientOrdinaryApplication>false</ClientOrdinaryApplication><ExternalConnection>false</ExternalConnection></Properties></CommonModule></MetaDataObject>"#,
    );
    write(
        root,
        "src/cf/CommonModules/Good/Ext/Module.bsl",
        "Procedure Shared(Value) Export\nEndProcedure\n",
    );
    for (directory, kind, object, uuid) in [
        ("epf", "ExternalDataProcessor", "Processor", "00000000-0000-0000-0000-000000000003"),
        ("erf", "ExternalReport", "Report", "00000000-0000-0000-0000-000000000004"),
    ] {
        write(
            root,
            &format!("src/{directory}/{object}.xml"),
            &format!(
                r#"<MetaDataObject><{kind} uuid="{uuid}"><Properties><Name>{object}</Name></Properties></{kind}></MetaDataObject>"#
            ),
        );
        write(root, &format!("src/{directory}/{object}/Ext/ObjectModule.bsl"), DUPLICATES);
    }
    write(root, "bsl-analyzer.toml", "[source]\nroot = \"src/cf\"\nextensions = []\nexternals = [{ name = \"Processor\", path = \"src/epf\" }, { name = \"Report\", path = \"src/erf\" }]\n");
    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s"])
        .arg(root)
        .args(["--format", "jsonl"])
        .env_remove("ONEC_CONFIGURATIONS_ROOT")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (tail, expected) in [
        ("Processor/Ext/ObjectModule.bsl", 4),
        ("Report/Ext/ObjectModule.bsl", 4),
        ("Good/Ext/Module.bsl", 0),
    ] {
        let event = events
            .iter()
            .find(|event| {
                event["type"] == "file"
                    && event["path"].as_str().is_some_and(|path| Path::new(path).ends_with(tail))
            })
            .unwrap_or_else(|| panic!("{tail} was not analyzed: {events:?}"));
        assert!(event["error"].is_null(), "{event}");
        let diagnostics = event["diagnostics"].as_array().unwrap();
        assert!(
            !diagnostics.iter().any(|diagnostic| diagnostic["code"] == "ParseError"),
            "{event}"
        );
        let duplicates: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic["code"] == "DuplicateMethodDeclaration")
            .collect();
        assert_eq!(duplicates.len(), expected, "{event}");
        for diagnostic in &duplicates {
            assert_eq!(diagnostic["severity"], "Blocker", "{diagnostic}");
        }
        if expected > 0 {
            let names: Vec<String> = duplicates
                .iter()
                .map(|diagnostic| {
                    let line = DUPLICATES
                        .lines()
                        .nth(diagnostic["start_line"].as_u64().unwrap() as usize)
                        .unwrap();
                    let start = diagnostic["start_column"].as_u64().unwrap() as usize;
                    let end = diagnostic["end_column"].as_u64().unwrap() as usize;
                    line.chars().skip(start).take(end - start).collect()
                })
                .collect();
            assert_eq!(names, ["Применить", "ПРИМЕНИТЬ", "Shared", "shared"]);
        }
    }
}
