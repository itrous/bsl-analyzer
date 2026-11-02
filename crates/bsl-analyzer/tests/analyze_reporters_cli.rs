use std::{env, fs, path::PathBuf, process::Command};

use tempfile::TempDir;

#[test]
fn analyze_writes_sarif_report() {
    let temp = TempDir::new().expect("tempdir");
    let source_dir = temp.path().join("src");
    let output_dir = temp.path().join("reports");
    fs::create_dir_all(&source_dir).expect("source dir");
    fs::create_dir_all(&output_dir).expect("output dir");
    fs::write(source_dir.join("Module.bsl"), "Процедура Тест(\n").expect("fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s"])
        .arg(&source_dir)
        .args(["-r", "sarif", "-o"])
        .arg(&output_dir)
        .arg("-q")
        .output()
        .expect("run bsl-analyzer");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report_path = output_dir.join("bsl-analyzer.sarif");
    assert!(report_path.exists(), "SARIF report should be written");

    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report_path).expect("report contents"))
            .expect("valid sarif json");
    assert_eq!(report["version"], "2.1.0");
    assert_eq!(report["runs"][0]["tool"]["driver"]["name"], "bsl-analyzer");
}

/// The calculation-register recorder rule reaches the report as a warning, and only for the
/// history of an employee: a table without parameters stays with the neighbouring rule, an
/// explicit `<>` exclusion and a filter by the document's own recorder are not reported.
#[test]
fn analyze_sarif_reports_the_calculation_register_recorder_rule_as_a_warning() {
    let temp = TempDir::new().expect("tempdir");
    let source_dir = temp.path().join("src");
    let output_dir = temp.path().join("reports");
    fs::create_dir_all(&source_dir).expect("source dir");
    fs::create_dir_all(&output_dir).expect("output dir");

    let table = "РегистрРасчета.Начисления.ФактическийПериодДействия";
    let query = |name: &str, table_and_filter: &str| {
        format!(
            "Функция {name}(Сотрудник, Ссылка) Экспорт\n    Запрос = Новый Запрос;\n    Запрос.Текст = \"ВЫБРАТЬ Т.Результат КАК Результат\n    |ИЗ {table_and_filter}\";\n    Возврат Запрос;\nКонецФункции\n\n"
        )
    };
    let module = [
        "Процедура Запись()\n    Движения.Начисления.Записать();\nКонецПроцедуры\n\n".to_owned(),
        query("История", &format!("{table}(Сотрудник = &Сотрудник) КАК Т")),
        query("БезПараметров", &format!("{table} КАК Т\n    |ГДЕ Т.Регистратор <> &Ссылка")),
        query(
            "Исключение",
            &format!("{table}(Сотрудник = &Сотрудник И Регистратор <> &Ссылка) КАК Т"),
        ),
        query("СвойНабор", &format!("{table}(Регистратор = &Ссылка) КАК Т")),
    ]
    .concat();
    fs::write(source_dir.join("Module.bsl"), &module).expect("fixture");
    let line_of = |needle: &str| {
        module.lines().position(|line| line.contains(needle)).expect("needle in fixture") + 1
    };

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s"])
        .arg(&source_dir)
        .args(["-r", "sarif", "-o"])
        .arg(&output_dir)
        .arg("-q")
        .output()
        .expect("run bsl-analyzer");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("bsl-analyzer.sarif")).expect("report contents"),
    )
    .expect("valid sarif json");
    let results = report["runs"][0]["results"].as_array().expect("results array");
    let of_rule = |rule: &str| -> Vec<(String, u64)> {
        results
            .iter()
            .filter(|result| result["ruleId"] == rule)
            .map(|result| {
                (
                    result["level"].as_str().expect("level").to_owned(),
                    result["locations"][0]["physicalLocation"]["region"]["startLine"]
                        .as_u64()
                        .expect("start line"),
                )
            })
            .collect()
    };

    assert_eq!(
        of_rule("CalculationRegisterVirtualTableWithoutRecorder"),
        [("warning".to_owned(), line_of("Сотрудник = &Сотрудник) КАК Т") as u64)],
    );
    assert_eq!(
        of_rule("VirtualTableCallWithoutParameters"),
        [(
            "error".to_owned(),
            line_of("ИЗ РегистрРасчета.Начисления.ФактическийПериодДействия КАК Т") as u64
        )],
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn issue80_dom_sarif_accepts_valid_and_rejects_invalid_controls() {
    let temp = TempDir::new().expect("tempdir");
    let source_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/issue80/dom-cli");
    let output_dir = temp.path().join("reports");
    fs::create_dir_all(&output_dir).expect("output dir");

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s"])
        .arg(&source_dir)
        .args(["-r", "sarif", "-o"])
        .arg(&output_dir)
        .arg("-q")
        .output()
        .expect("run bsl-analyzer");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("bsl-analyzer.sarif")).expect("report contents"),
    )
    .expect("valid sarif json");
    let results = report["runs"][0]["results"].as_array().expect("results array");
    let type_mismatches: Vec<_> =
        results.iter().filter(|result| result["ruleId"] == "TypeMismatch").collect();
    let mismatches_at = |line| {
        type_mismatches
            .iter()
            .filter(|result| {
                result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
                    == "Module.bsl"
                    && result["locations"][0]["physicalLocation"]["region"]["startLine"] == line
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(type_mismatches.len(), 1, "fixture must produce only the scalar control");
    let valid = mismatches_at(12);
    assert!(valid.is_empty(), "valid DOM/HTML child must not produce TypeMismatch");
    let invalid = mismatches_at(17);
    assert_eq!(invalid.len(), 1, "scalar control must retain one TypeMismatch");
    assert!(
        invalid[0]["message"]["text"].as_str().expect("invalid mismatch message").contains("Число"),
        "scalar control must report its Number argument, got {invalid:?}"
    );
}

/// Two analyze runs over the same tree must produce byte-identical SARIF: A/B
/// regression gates compare reports with plain `cmp`. The fixture provokes the
/// historical divergence source — several `DuplicateStringLiteral` groups per
/// file, whose emit order followed hash-map iteration and differed between
/// processes.
#[test]
fn analyze_sarif_is_byte_identical_across_runs() {
    let temp = TempDir::new().expect("tempdir");
    let source_dir = temp.path().join("src");
    fs::create_dir_all(&source_dir).expect("source dir");

    for file in ["ModuleA.bsl", "ModuleB.bsl"] {
        let mut text = String::from("Процедура Тест()\n");
        for group in 0..6 {
            for _ in 0..3 {
                text.push_str(&format!("    Значение = \"Литерал номер {group}\";\n"));
            }
        }
        text.push_str("КонецПроцедуры\n");
        fs::write(source_dir.join(file), &text).expect("fixture");
    }

    let run = |output_dir: &std::path::Path| {
        fs::create_dir_all(output_dir).expect("output dir");
        let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["analyze", "-s"])
            .arg(&source_dir)
            .args(["-r", "sarif", "-o"])
            .arg(output_dir)
            .arg("-q")
            .output()
            .expect("run bsl-analyzer");
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read(output_dir.join("bsl-analyzer.sarif")).expect("sarif bytes")
    };

    let first = run(&temp.path().join("reports-a"));
    let second = run(&temp.path().join("reports-b"));

    let findings: serde_json::Value = serde_json::from_slice(&first).expect("valid sarif json");
    let results = findings["runs"][0]["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "fixture must actually produce diagnostics");

    assert_eq!(first, second, "SARIF must be byte-identical across runs");
}

/// In-code suppression directives must reach the CLI reporters: a `// bsl-analyzer:off` for a
/// code removes its findings from the SARIF report, proving the shared choke point in
/// `apply_extension_merge` is honoured by the `analyze` path.
#[test]
fn analyze_sarif_honours_in_code_suppression() {
    let module =
        |directive: &str| format!("Процедура Тест()\n{directive}    А = А;\nКонецПроцедуры\n");

    let self_assign_count = |source: &str| -> usize {
        let temp = TempDir::new().expect("tempdir");
        let source_dir = temp.path().join("src");
        let output_dir = temp.path().join("reports");
        fs::create_dir_all(&source_dir).expect("source dir");
        fs::create_dir_all(&output_dir).expect("output dir");
        fs::write(source_dir.join("Module.bsl"), source).expect("fixture");

        let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
            .args(["analyze", "-s"])
            .arg(&source_dir)
            .args(["-r", "sarif", "-o"])
            .arg(&output_dir)
            .arg("-q")
            .output()
            .expect("run bsl-analyzer");
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let report: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(output_dir.join("bsl-analyzer.sarif")).expect("report contents"),
        )
        .expect("valid sarif json");
        report["runs"][0]["results"]
            .as_array()
            .map(|results| results.iter().filter(|r| r["ruleId"] == "SelfAssign").count())
            .unwrap_or(0)
    };

    assert!(self_assign_count(&module("")) >= 1, "fixture must produce a SelfAssign finding");
    assert_eq!(
        self_assign_count(&module("    // bsl-analyzer:off SelfAssign\n")),
        0,
        "the suppression directive must remove SelfAssign from the SARIF report"
    );
}

#[test]
fn analyze_writes_codequality_report() {
    let temp = TempDir::new().expect("tempdir");
    let source_dir = temp.path().join("src");
    let output_dir = temp.path().join("reports");
    fs::create_dir_all(&source_dir).expect("source dir");
    fs::create_dir_all(&output_dir).expect("output dir");
    fs::write(source_dir.join("Module.bsl"), "Процедура Тест(\n").expect("fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s"])
        .arg(&source_dir)
        .args(["-r", "codequality", "-o"])
        .arg(&output_dir)
        .arg("-q")
        .output()
        .expect("run bsl-analyzer");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report_path = output_dir.join("gl-code-quality-report.json");
    assert!(report_path.exists(), "code quality report should be written");

    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(report_path).expect("report contents"))
            .expect("valid codequality json");
    let entries = report.as_array().expect("codequality is an array");
    assert!(!entries.is_empty(), "fixture must produce diagnostics");
    let entry = &entries[0];
    assert!(entry["fingerprint"].is_string());
    assert!(entry["check_name"].is_string());
    assert!(entry["location"]["path"].is_string());
    assert!(entry["location"]["lines"]["begin"].is_u64());
}

#[test]
fn analyze_writes_junit_report() {
    let temp = TempDir::new().expect("tempdir");
    let source_dir = temp.path().join("src");
    let output_dir = temp.path().join("reports");
    fs::create_dir_all(&source_dir).expect("source dir");
    fs::create_dir_all(&output_dir).expect("output dir");
    fs::write(source_dir.join("Module.bsl"), "Процедура Тест(\n").expect("fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_bsl-analyzer-app"))
        .args(["analyze", "-s"])
        .arg(&source_dir)
        .args(["-r", "junit", "-o"])
        .arg(&output_dir)
        .arg("-q")
        .output()
        .expect("run bsl-analyzer");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report_path = output_dir.join("bsl-analyzer.junit.xml");
    assert!(report_path.exists(), "junit report should be written");

    let xml = fs::read_to_string(report_path).expect("report contents");
    assert!(xml.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
    assert!(xml.contains("<testsuites"));
    assert!(xml.contains("</testsuites>"));
    assert!(xml.contains("<testcase"));
    // Every opened suite is closed → the document is well-formed enough for CI.
    assert_eq!(xml.matches("<testsuite ").count(), xml.matches("</testsuite>").count());
}
