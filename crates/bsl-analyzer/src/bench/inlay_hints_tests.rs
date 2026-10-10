use super::manifest::{hash_text, BenchManifest, Expect, FeatureSpec, Target, SCHEMA_VERSION};
use super::runner::{run_point, RunArgs, RunMode};

const REPEATED_RECEIVER_FIXTURE: &str = "\
Функция Преобразовать(Первый, Второй)
    Возврат Первый + Второй;
КонецФункции

Процедура Тест()
    Массив = Новый Массив;
    Список = Новый СписокЗначений;
    Массив.Добавить(1);
    Список.Добавить(2);
    Массив.Добавить(Массив.Добавить(3));
    Список.Добавить(Список.Добавить(4));
    Массив.Добавить(5);
    Список.Добавить(6);
    Массив.Добавить(7);
    Список.Добавить(8);
    Массив.Добавить(9);
    Список.Добавить(10);
    Преобразовать(11, 12);
    Массив.Добавить(Список.Добавить(13));
КонецПроцедуры
";

const REPEATED_RECEIVER_FIXTURE_HASH: &str =
    "4941d3be7120c2e7c313288855d7515396f0a89d9158d31d1a58a012b00e18ec";

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn inlay_hints_point_exercises_repeated_receiver_owner_lookup() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Module.bsl"), REPEATED_RECEIVER_FIXTURE).unwrap();
    assert_eq!(hash_text(REPEATED_RECEIVER_FIXTURE), REPEATED_RECEIVER_FIXTURE_HASH);

    let manifest = BenchManifest {
        schema_version: SCHEMA_VERSION,
        workspace_commit: None,
        config_hash: None,
        targets: vec![Target {
            id: "inlay_hints/repeated_receivers/01".to_string(),
            relative_path: "Module.bsl".to_string(),
            file_hash: REPEATED_RECEIVER_FIXTURE_HASH.to_string(),
            spec: FeatureSpec::InlayHints { range: None },
            expect: Expect::Cardinality { min: 16, max: 16 },
            note: None,
        }],
    };
    let manifest_path = workspace.path().join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest).unwrap()).unwrap();

    let report = run_point(&RunArgs {
        source_dir: workspace.path().to_path_buf(),
        manifest_path,
        point_id: "inlay_hints/repeated_receivers/01".to_string(),
        mode: RunMode::Latency,
        warm_iterations: 2,
        boot_budget_ms: 60_000,
        trim_settle_ms: 10,
    })
    .unwrap();

    assert!(report.invariant_ok, "{:?}", report.invariant_error);
    assert_eq!(report.observed_count, 16);
    assert_eq!(report.warm_ns.len(), 2);
    assert!(report.warm_ns.iter().all(|&sample| sample > 0));
    assert_eq!(report.digest.len(), 64);
}
