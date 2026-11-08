use crate::test_utils::{check_diagnostics_snapshot_for, check_file_diagnostics_with_config};
use crate::{DiagnosticCode, DiagnosticsConfig};
use expect_test::expect;

const CODE: DiagnosticCode = DiagnosticCode::CreateQueryInCycle;
const CALL: DiagnosticCode = DiagnosticCode::LocalCallWithQueryInCycle;

fn count(source: &str) -> usize {
    check_file_diagnostics_with_config(source, DiagnosticsConfig::default())
        .iter()
        .filter(|d| d.code == CODE)
        .count()
}

fn count_call(source: &str) -> usize {
    check_file_diagnostics_with_config(source, DiagnosticsConfig::default())
        .iter()
        .filter(|d| d.code == CALL)
        .count()
}

const DIRECT_EXECUTE: &str = include_str!("fixtures/create_query_in_cycle/direct_execute.bsl");
const DIRECT_EXECUTE_BATCH: &str =
    include_str!("fixtures/create_query_in_cycle/direct_execute_batch.bsl");

/// Wraps `body` as a method with a known query before the loop.
fn in_procedure(body: &str) -> String {
    format!("Процедура Тест(Коллекция, Параметр)\n{body}\nКонецПроцедуры\n")
}

#[test]
fn create_query_in_cycle_i01_direct_execute() {
    check_diagnostics_snapshot_for(
        DIRECT_EXECUTE,
        CODE,
        expect![[r#"
            CreateQueryInCycle @ 6:17..6:35
              message: Выполнение запроса в цикле приводит к деградации производительности. Создайте запрос один раз до цикла и изменяйте только параметры внутри цикла
              severity: Critical"#]],
    );
    let outside = in_procedure("\tЗапрос = Новый Запрос;\n\tЗапрос.Выполнить();\n\tДля Каждого Э Из Коллекция Цикл\n\t\tА = 1;\n\tКонецЦикла;");
    assert_eq!(count(&outside), 0);
}

#[test]
fn create_query_in_cycle_i02_direct_execute_batch() {
    check_diagnostics_snapshot_for(
        DIRECT_EXECUTE_BATCH,
        CODE,
        expect![[r#"
            CreateQueryInCycle @ 6:19..6:42
              message: Выполнение запроса в цикле приводит к деградации производительности. Создайте запрос один раз до цикла и изменяйте только параметры внутри цикла
              severity: Critical"#]],
    );
    let outside = in_procedure("\tЗапрос = Новый Запрос;\n\tЗапрос.ВыполнитьПакет();\n\tДля Каждого Э Из Коллекция Цикл\n\t\tА = 1;\n\tКонецЦикла;");
    assert_eq!(count(&outside), 0);
}

#[test]
fn create_query_in_cycle_i08_unknown_receivers_prove_nothing() {
    let cases = [
        // An unknown parameter.
        "\tДля Каждого Э Из Коллекция Цикл\n\t\tПараметр.Выполнить();\n\tКонецЦикла;",
        // Overwritten by an unknown value.
        "\tЗ = Новый Запрос;\n\tЗ = Параметр;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;",
        // Overwritten by a foreign value.
        "\tЗ = Новый Запрос;\n\tЗ = Новый Массив;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.ВыполнитьПакет();\n\tКонецЦикла;",
        // A mix of a query and an unknown value reaching the read.
        "\tЕсли Параметр Тогда\n\t\tЗ = Новый Запрос;\n\tИначе\n\t\tЗ = Параметр;\n\tКонецЕсли;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;",
        // A mix of a query and a foreign value.
        "\tЕсли Параметр Тогда\n\t\tЗ = Новый Запрос;\n\tИначе\n\t\tЗ = Новый Массив;\n\tКонецЕсли;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;",
        // A query only on the textually last, sibling assignment.
        "\tЕсли Параметр Тогда\n\t\tЗ = Параметр;\n\tИначе\n\t\tЗ = Новый Запрос;\n\tКонецЕсли;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;",
        // An operation with a longer name, in both languages.
        "\tЗ = Новый Запрос;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.ВыполнитьПакетСПромежуточнымиДанными();\n\tКонецЦикла;",
        "\tZ = New Query;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tZ.ExecuteBatchWithIntermediateData();\n\tКонецЦикла;",
        // The argument count rejects the platform method.
        "\tЗ = Новый Запрос;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить(1, 2, 3);\n\tКонецЦикла;",
        // Same-named bare method and builtin.
        "\tДля Каждого Э Из Коллекция Цикл\n\t\tВыполнить();\n\tКонецЦикла;",
        // Method of a foreign platform object.
        "\tД = Новый Массив;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tД.Выполнить();\n\tКонецЦикла;",
    ];
    for case in cases {
        assert_eq!(count(&in_procedure(case)), 0, "{case}");
    }
    let user_method = "Процедура Выполнить()\nКонецПроцедуры\n\nПроцедура Тест(Коллекция)\n\tДля Каждого Э Из Коллекция Цикл\n\t\tВыполнить();\n\tКонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(count(user_method), 0);

    // Positive control: the same places with a known query fire, with no SQL text at all.
    let known = in_procedure("\tЗ = Новый Запрос;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.ВыполнитьПакет();\n\tКонецЦикла;");
    assert_eq!(count(&known), 1);
    let overwritten_with_query = in_procedure("\tЗ = Параметр;\n\tЗ = Новый Запрос;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;");
    assert_eq!(count(&overwritten_with_query), 1);
    let both_branches = in_procedure("\tЕсли Параметр Тогда\n\t\tЗ = Новый Запрос;\n\tИначе\n\t\tЗ = Новый Запрос(\"ВЫБРАТЬ 1\");\n\tКонецЕсли;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;");
    assert_eq!(count(&both_branches), 1);
}

#[test]
fn create_query_in_cycle_i08_recovered_call_keeps_its_neighbour() {
    let source = in_procedure("\tЗ = Новый Запрос;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить(;\n\t\tЗ.ВыполнитьПакет();\n\tКонецЦикла;");
    let reported: Vec<_> =
        check_file_diagnostics_with_config(&source, DiagnosticsConfig::default())
            .into_iter()
            .filter(|d| d.code == CODE)
            .map(|d| source[d.range].to_string())
            .collect();
    // The neighbour is kept and the broken call is no finding of its own.
    assert_eq!(reported, vec!["З.ВыполнитьПакет()".to_string()], "{reported:?}");
}

#[test]
fn create_query_in_cycle_i10_spellings_agree() {
    for (ctor, exec) in [
        ("Новый Запрос", "Выполнить"),
        ("Новый Запрос", "ВыполнитьПакет"),
        ("New Query", "Execute"),
        ("New Query", "ExecuteBatch"),
        ("Новый запрос", "выполнитьпакет"),
        ("NEW QUERY", "EXECUTEBATCH"),
        ("Новый Query", "Выполнить"),
    ] {
        let source = in_procedure(&format!(
            "\tЗ = {ctor};\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.{exec}();\n\tКонецЦикла;"
        ));
        assert_eq!(count(&source), 1, "{ctor} {exec}");
        let without = in_procedure(&format!(
            "\tЗ = {ctor};\n\tДля Каждого Э Из Коллекция Цикл\n\t\tА = 1;\n\tКонецЦикла;"
        ));
        assert_eq!(count(&without), 0, "{ctor} {exec}");
    }
}

#[test]
fn create_query_in_cycle_i11_loop_forms_and_positions() {
    let body = "\tЗ = Новый Запрос;\n";
    let inside = [
        "\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;",
        "\tДля Й = 1 По 3 Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;",
        // Nested loops report once.
        "\tПока Параметр Цикл\n\t\tДля Й = 1 По 3 Цикл\n\t\t\tЗ.Выполнить();\n\t\tКонецЦикла;\n\tКонецЦикла;",
        // An argument of a call, also of a call to a receiver nothing is known about.
        "\tМ = Новый Массив;\n\tПока Параметр Цикл\n\t\tМ.Добавить(З.Выполнить().Выгрузить());\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tПараметр.Добавить(З.Выполнить());\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tНетТакогоМодуля.Добавить(З.Выполнить());\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tМетод(З.Выполнить());\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tСообщить(З.Выполнить());\n\tКонецЦикла;",
        // The header of an inner loop runs in the body of the outer one.
        "\tПока Параметр Цикл\n\t\tПока З.Выполнить() Цикл\n\t\t\tА = 1;\n\t\tКонецЦикла;\n\tКонецЦикла;",
        // A condition inside a loop body.
        "\tПока Параметр Цикл\n\t\tЕсли З.Выполнить().Пустой() Тогда\n\t\t\tА = 1;\n\t\tКонецЕсли;\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tЕсли З.Выполнить().Пустой() Тогда\n\t\tКонецЕсли;\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tЕсли Параметр Тогда\n\t\t\tА = 1;\n\t\tИначеЕсли З.Выполнить().Пустой() Тогда\n\t\t\tА = 1;\n\t\tКонецЕсли;\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tПока З.Выполнить().Выбрать().Следующий() Цикл\n\t\tКонецЦикла;\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tДля Каждого Э Из З.Выполнить().Выгрузить() Цикл\n\t\t\tЕсли Параметр Тогда\n\t\t\t\tА = 1;\n\t\t\tКонецЕсли;\n\t\tКонецЦикла;\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tДля Каждого Э Из З.Выполнить().Выгрузить() Цикл\n\t\tКонецЦикла;\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tДля Й = З.Выполнить() По 3 Цикл\n\t\tКонецЦикла;\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tДля Й = 1 По З.Выполнить() Цикл\n\t\tКонецЦикла;\n\tКонецЦикла;",
        // Other statement kinds hosting the call.
        "\tПока Параметр Цикл\n\t\tРезультат = З.Выполнить();\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tВызватьИсключение З.Выполнить();\n\tКонецЦикла;",
        // The query is written once more in the loop.
        "\tПока Параметр Цикл\n\t\tЗ.Выполнить();\n\t\tЗ = Новый Запрос;\n\tКонецЦикла;",
        // Try.
        "\tПока Параметр Цикл\n\t\tПопытка\n\t\t\tЗ.Выполнить();\n\t\tИсключение\n\t\t\tА = 1;\n\t\tКонецПопытки;\n\tКонецЦикла;",
    ];
    for case in inside {
        assert_eq!(count(&in_procedure(&format!("{body}{case}"))), 1, "{case}");
    }
    let header = [
        "\tПока З.Выполнить() Цикл\n\t\tА = 1;\n\tКонецЦикла;",
        "\tДля Каждого Э Из З.Выполнить().Выгрузить() Цикл\n\t\tА = 1;\n\tКонецЦикла;",
        "\tДля Й = З.Выполнить() По 3 Цикл\n\t\tА = 1;\n\tКонецЦикла;",
    ];
    for case in header {
        assert_eq!(count(&in_procedure(&format!("{body}{case}"))), 0, "{case}");
    }
    let module_code = "З = Новый Запрос;\nПока Истина Цикл\n\tЗ.Выполнить();\nКонецЦикла;\n";
    assert_eq!(count(module_code), 1);
    let module_code_outside = "З = Новый Запрос;\nЗ.Выполнить();\n";
    assert_eq!(count(module_code_outside), 0);
}

#[test]
fn create_query_in_cycle_i12_builders() {
    for ctor in [
        "Новый ПостроительЗапроса",
        "New QueryBuilder",
        "Новый ПостроительОтчета",
        "New ReportBuilder",
    ] {
        let execute = in_procedure(&format!(
            "\tП = {ctor};\n\tДля Каждого Э Из Коллекция Цикл\n\t\tП.Выполнить();\n\tКонецЦикла;"
        ));
        assert_eq!(count(&execute), 1, "{ctor}");
        let batch = in_procedure(&format!(
            "\tП = {ctor};\n\tДля Каждого Э Из Коллекция Цикл\n\t\tП.ВыполнитьПакет();\n\tКонецЦикла;"
        ));
        assert_eq!(count(&batch), 0, "{ctor}");
    }
}

#[test]
fn create_query_in_cycle_i13_one_finding_per_call() {
    let source = in_procedure(
        "\tЗ = Новый Запрос;\n\tПока Параметр Цикл\n\t\tДля Й = 1 По 3 Цикл\n\t\t\tЗ.Выполнить();\n\t\t\tЗ.ВыполнитьПакет();\n\t\tКонецЦикла;\n\tКонецЦикла;",
    );
    assert_eq!(count(&source), 2);
}

#[test]
fn create_query_in_cycle_i08_dead_branch_is_no_execution() {
    let run = |branch: &str| {
        let body = format!(
            "Процедура Тест(Коллекция) Экспорт\n\tЗ = Новый Запрос;\n\tДля Каждого Э Из Коллекция Цикл\n\t\t#Если {branch} Тогда\n\t\tЗ.Выполнить();\n\t\t#КонецЕсли\n\tКонецЦикла;\nКонецПроцедуры\n"
        );
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module("Проба", &body);
        crate::test_utils::check_cfe_at_with_db(
            "CommonModules/Проба/Ext/Module.bsl",
            &body,
            builder.build(),
            &[],
            DiagnosticsConfig::default(),
            |_| {},
            |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
        )
        .into_iter()
        .filter(|d| d.code == CODE)
        .count()
    };
    assert_eq!(run("Сервер И Не Сервер"), 0);
    assert_eq!(run("ТолстыйКлиентОбычноеПриложение"), 0);
    assert_eq!(run("Сервер"), 1);
}

#[test]
fn create_query_in_cycle_i13_receiver_without_a_local() {
    let source = "Функция СоздатьЗапрос()\n\tЗ = Новый Запрос;\n\tВозврат З;\nКонецФункции\n\nПроцедура Тест(Параметр)\n\tПока Параметр Цикл\n\t\tСоздатьЗапрос().Выполнить();\n\tКонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(count(source), 1);
}

/// A query written directly in `#Вставка` of a changed method: the receiver's definitions come
/// from the effective body, whose statements are not the base method's.
#[test]
fn create_query_in_cycle_i01_direct_execute_in_change_and_validate_insert() {
    fn run(insert: &str) -> Vec<String> {
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module(
            "Сервер",
            "Процедура Цель(Коллекция) Экспорт\n\tА = 1;\n\tБ = 2;\nКонецПроцедуры\n",
        );
        let ext = format!(
            "&ИзменениеИКонтроль(\"Цель\")\nПроцедура Расш_Цель(Коллекция)\n\tА = 1;\n#Вставка\n{insert}#КонецВставки\n\tБ = 2;\nКонецПроцедуры\n"
        );
        builder.add_extension("Расш", "");
        crate::test_utils::check_cfe_at_with_db(
            "CommonModules/Сервер/Ext/Module.bsl",
            &ext,
            builder.build(),
            &[],
            DiagnosticsConfig::default(),
            |_| {},
            |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
        )
        .into_iter()
        .filter(|d| d.code == CODE)
        .map(|d| d.message)
        .collect()
    }
    let with_query = run("\tЗ = Новый Запрос;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;\n");
    assert_eq!(with_query.len(), 1, "{with_query:?}");
    let unknown = run(
        "\tЗ = Коллекция;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.Выполнить();\n\tКонецЦикла;\n",
    );
    assert!(unknown.is_empty(), "{unknown:?}");
}

const HELPER_EXECUTE: &str = include_str!("fixtures/create_query_in_cycle/helper_execute.bsl");
const HELPER_CHAIN: &str =
    include_str!("fixtures/create_query_in_cycle/helper_chain_execute_batch.bsl");
const RECURSION_WITH_QUERY: &str =
    include_str!("fixtures/create_query_in_cycle/recursion_with_query.bsl");
const RECURSION_WITHOUT_QUERY: &str =
    include_str!("fixtures/create_query_in_cycle/recursion_without_query.bsl");
const HELPER_OUTSIDE_LOOP: &str =
    include_str!("fixtures/create_query_in_cycle/helper_outside_loop.bsl");

/// The text of each direct finding's range, in file order.
#[allow(dead_code)]
fn reported(source: &str) -> Vec<String> {
    let mut diags: Vec<_> =
        check_file_diagnostics_with_config(source, DiagnosticsConfig::default())
            .into_iter()
            .filter(|d| d.code == CODE)
            .collect();
    diags.sort_by_key(|d| d.range.start());
    diags.into_iter().map(|d| source[d.range].to_string()).collect()
}

/// The text of each local-call finding's range, in file order.
fn reported_call(source: &str) -> Vec<String> {
    let mut diags: Vec<_> =
        check_file_diagnostics_with_config(source, DiagnosticsConfig::default())
            .into_iter()
            .filter(|d| d.code == CALL)
            .collect();
    diags.sort_by_key(|d| d.range.start());
    diags.into_iter().map(|d| source[d.range].to_string()).collect()
}

#[test]
fn create_query_in_cycle_i03_helper_execute() {
    check_diagnostics_snapshot_for(
        HELPER_EXECUTE,
        CALL,
        expect![[r#"
            LocalCallWithQueryInCycle @ 9:3..9:23
              message: Вызов локального метода «ПрочитатьЦену» в теле цикла приводит к выполнению запроса непосредственно или через локальные методы этого модуля. Получите необходимые данные одним запросом до цикла
              severity: Warning"#]],
    );
    let without = HELPER_EXECUTE.replace("\tЗапрос.Выполнить();\n", "");
    assert!(!without.contains("Выполнить"));
    assert_eq!(count_call(&without), 0);
}

fn chain(len: usize, tail: &str, reversed: bool) -> String {
    let mut methods: Vec<String> = (0..len)
        .map(|i| {
            let (kw, end) = if i % 2 == 0 {
                ("Процедура", "КонецПроцедуры")
            } else {
                ("Функция", "КонецФункции")
            };
            let next = i + 1;
            let body = if next == len { tail.to_string() } else { format!("\tП{next}();\n") };
            format!("{kw} П{i}()\n{body}{end}\n")
        })
        .collect();
    methods.push(
        "Процедура Обход(Коллекция)\n\tДля Каждого Э Из Коллекция Цикл\n\t\tП0();\n\tКонецЦикла;\nКонецПроцедуры\n"
            .to_string(),
    );
    if reversed {
        methods.reverse();
    }
    methods.join("\n")
}

#[test]
fn create_query_in_cycle_i04_helper_chain() {
    assert_eq!(reported_call(HELPER_CHAIN), vec!["ПрочитатьТариф(Тариф)"]);
    let without_edge = HELPER_CHAIN.replace("\tЦены = ПрочитатьЦены(Тариф);\n", "");
    assert_eq!(count_call(&without_edge), 0);

    let query = "\tЗ = Новый Запрос;\n\tЗ.ВыполнитьПакет();\n";
    // Longer than any depth a recursive walk or the local type proof would be limited by.
    for len in [2, 3, 40, 400] {
        for reversed in [false, true] {
            assert_eq!(
                reported_call(&chain(len, query, reversed)),
                vec!["П0()"],
                "{len} {reversed}"
            );
            assert_eq!(count_call(&chain(len, "", reversed)), 0, "{len} {reversed}");
        }
    }
}

fn effective_findings(base_helper: &str) -> Vec<crate::Diagnostic> {
    let mut builder = test_fixture::CfeFixtureBuilder::new("");
    builder.add_base_module(
        "Сервер",
        &format!(
            "Процедура Цель(Коллекция) Экспорт\n\tА = 1;\nКонецПроцедуры\n\nПроцедура Помощник()\n{base_helper}КонецПроцедуры\n"
        ),
    );
    let ext = "&ИзменениеИКонтроль(\"Цель\")\nПроцедура Расш_Цель(Коллекция)\n\tА = 1;\n#Вставка\n\tДля Каждого Э Из Коллекция Цикл\n\t\tПомощник();\n\tКонецЦикла;\n#КонецВставки\nКонецПроцедуры\n";
    builder.add_extension("Расш", "");
    crate::test_utils::check_cfe_at_with_db(
        "CommonModules/Сервер/Ext/Module.bsl",
        ext,
        builder.build(),
        &[],
        DiagnosticsConfig::default(),
        |_| {},
        |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
    )
}

#[test]
fn create_query_in_cycle_i04_effective_module_reaches_an_unchanged_helper() {
    let with_query = effective_findings("\tЗ = Новый Запрос;\n\tЗ.Выполнить();\n");
    let without_query = effective_findings("\tА = 1;\n");
    let ours = |diags: &[crate::Diagnostic]| {
        diags.iter().filter(|d| d.code == CALL).map(|d| d.message.clone()).collect::<Vec<_>>()
    };
    let found = ours(&with_query);
    assert_eq!(found.len(), 1, "{with_query:?}");
    assert!(found[0].contains("«Помощник»"), "{found:?}");
    assert!(ours(&without_query).is_empty(), "{without_query:?}");
    // Other diagnostics of the file stay as they are.
    let others = |diags: &[crate::Diagnostic]| {
        diags.iter().filter(|d| d.code != CALL).map(|d| (d.code, d.range)).collect::<Vec<_>>()
    };
    assert_eq!(others(&with_query), others(&without_query));
}

#[test]
fn create_query_in_cycle_i05_recursion_with_query() {
    assert_eq!(
        reported_call(RECURSION_WITH_QUERY),
        vec!["ОбойтиДерево(Узел)", "ОбойтиЧётные(Узел)"]
    );
    let without_seeds = RECURSION_WITH_QUERY
        .replace("\tЗапрос.Выполнить();\n", "")
        .replace("\tЗапрос.ВыполнитьПакет();\n", "");
    assert_eq!(count_call(&without_seeds), 0);
    // The seed reachable only by the later edge of the component is what fires.
    let later_edge = RECURSION_WITH_QUERY.replace("\tЗапрос.Выполнить();\n", "");
    assert_eq!(reported_call(&later_edge), vec!["ОбойтиЧётные(Узел)"]);
}

#[test]
fn create_query_in_cycle_i06_recursion_without_query() {
    assert_eq!(count_call(RECURSION_WITHOUT_QUERY), 0);
    let seeded = RECURSION_WITHOUT_QUERY.replace(
        "Процедура ОбойтиНечётные(Узел)\n",
        "Процедура ОбойтиНечётные(Узел)\n\tЗ = Новый Запрос;\n\tЗ.ВыполнитьПакет();\n",
    );
    assert_eq!(reported_call(&seeded), vec!["ОбойтиЧётные(Узел)"]);
}

#[test]
fn create_query_in_cycle_i07_helper_outside_loop() {
    assert_eq!(count_call(HELPER_OUTSIDE_LOOP), 0);
    let moved = HELPER_OUTSIDE_LOOP.replace("Итог = Удвоить(Тариф);", "Итог = ПрочитатьЦены();");
    assert_eq!(reported_call(&moved), vec!["ПрочитатьЦены()"]);
}

#[test]
fn create_query_in_cycle_i09_only_resolved_local_calls_connect() {
    let helper = "Процедура Помощник()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n\n";
    let looped = |call: &str| {
        format!("{helper}Процедура Обход(Коллекция, Параметр)\n\tДля Каждого Э Из Коллекция Цикл\n\t\t{call};\n\tКонецЦикла;\nКонецПроцедуры\n")
    };
    // Same name, other owner, or no resolution at all.
    for call in [
        "Параметр.Помощник()",
        "НетМодуля.Помощник()",
        "Коллекция[0].Помощник()",
        "ДругойПомощник()",
        "Помощник(1, 2, 3)",
    ] {
        assert_eq!(count_call(&looped(call)), 0, "{call}");
    }
    assert_eq!(reported_call(&looped("Помощник()")), vec!["Помощник()"]);
    assert_eq!(reported_call(&looped("помощник()")), vec!["помощник()"]);
}

#[test]
fn create_query_in_cycle_i09_common_module_boundary() {
    fn run(caller: &str, module_body: &str) -> Vec<String> {
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module("Сервер", module_body);
        builder.add_base_module(
            "Другой",
            "Процедура Помощник() Экспорт\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n",
        );
        crate::test_utils::check_cfe_at_with_db(
            "CommonModules/Сервер/Ext/Module.bsl",
            caller,
            builder.build(),
            &[],
            DiagnosticsConfig::default(),
            |_| {},
            |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
        )
        .into_iter()
        .filter(|d| d.code == CALL)
        .map(|d| caller[d.range].to_string())
        .collect()
    }
    let loop_over = |call: &str, local: &str| {
        format!("{local}Процедура Обход(Коллекция) Экспорт\n\tДля Каждого Э Из Коллекция Цикл\n\t\t{call};\n\tКонецЦикла;\nКонецПроцедуры\n")
    };
    let local_query =
        "Процедура Помощник() Экспорт\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n\n";

    // A method of another module is not followed, whatever it does, and a same-named local
    // method with a query does not lend its finding to it.
    let other = loop_over("Другой.Помощник()", local_query);
    assert_eq!(run(&other, &other), Vec::<String>::new());
}

#[test]
fn create_query_in_cycle_i09_self_form_where_inference_resolves_it() {
    let run = |helper_body: &str, receiver: &str| {
        let source = format!("&НаСервере\nПроцедура Помощник() Экспорт\n{helper_body}КонецПроцедуры\n\n&НаСервере\nПроцедура Обход(Коллекция)\n\tДля Каждого Э Из Коллекция Цикл\n\t\t{receiver}.Помощник();\n\tКонецЦикла;\nКонецПроцедуры\n");
        crate::test_utils::check_form_with_common_modules(&source, &[])
            .into_iter()
            .filter(|d| d.code == CALL)
            .map(|d| source[d.range].to_string())
            .collect::<Vec<_>>()
    };
    let query = "\tЗ = Новый Запрос;\n\tЗ.Выполнить();\n";
    for receiver in ["ЭтотОбъект", "ЭтаФорма"] {
        assert_eq!(run(query, receiver), vec![format!("{receiver}.Помощник()")]);
        assert_eq!(run("\tА = 1;\n", receiver), Vec::<String>::new());
    }
}

#[test]
fn create_query_in_cycle_i10_helpers_in_both_languages() {
    let ru = "Процедура Помощник()\n\tЗапрос = Новый Запрос;\n\tЗапрос.ВыполнитьПакет();\nКонецПроцедуры\n\nПроцедура Обход(Коллекция)\n\tДля Каждого Э Из Коллекция Цикл\n\t\tпомощник();\n\tКонецЦикла;\nКонецПроцедуры\n";
    let en = "Procedure Helper()\n\tRequest = New Query;\n\tRequest.ExecuteBatch();\nEndProcedure\n\nProcedure Walk(Items)\n\tFor Each Item In Items Do\n\t\tHELPER();\n\tEndDo;\nEndProcedure\n";
    assert_eq!(reported_call(ru), vec!["помощник()"]);
    assert_eq!(reported_call(en), vec!["HELPER()"]);
    assert_eq!(count_call(&ru.replace("\tЗапрос.ВыполнитьПакет();\n", "")), 0);
    assert_eq!(count_call(&en.replace("\tRequest.ExecuteBatch();\n", "")), 0);
}

#[test]
fn create_query_in_cycle_i11_helper_positions() {
    let helper =
        "Функция Помощник()\n\tЗ = Новый Запрос;\n\tВозврат З.Выполнить();\nКонецФункции\n\n";
    let body = |code: &str| {
        format!("{helper}Процедура Обход(Параметр, Коллекция)\n{code}\nКонецПроцедуры\n")
    };
    for inside in [
        "\tДля Каждого Э Из Коллекция Цикл\n\t\tПомощник();\n\tКонецЦикла;",
        "\tДля Й = 1 По 3 Цикл\n\t\tПомощник();\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tА = Помощник();\n\tКонецЦикла;",
        "\tПока Параметр Цикл\n\t\tДля Й = 1 По 3 Цикл\n\t\t\tСообщить(Помощник());\n\t\tКонецЦикла;\n\tКонецЦикла;",
    ] {
        assert_eq!(reported_call(&body(inside)), vec!["Помощник()"], "{inside}");
    }
    for header in [
        "\tПока Помощник() Цикл\n\t\tА = 1;\n\tКонецЦикла;",
        "\tДля Каждого Э Из Помощник() Цикл\n\t\tА = 1;\n\tКонецЦикла;",
    ] {
        assert_eq!(count_call(&body(header)), 0, "{header}");
    }
    let module_code = format!("{helper}Пока Истина Цикл\n\tПомощник();\nКонецЦикла;\n");
    assert_eq!(reported_call(&module_code), vec!["Помощник()"]);
    let module_code_outside = format!("{helper}Помощник();\n");
    assert_eq!(count_call(&module_code_outside), 0);
}

#[test]
fn create_query_in_cycle_i12_builders_through_helpers() {
    for (ctor, exec, expected) in [
        ("Новый ПостроительЗапроса", "Выполнить", 1),
        ("Новый ПостроительОтчета", "Выполнить", 1),
        ("Новый ПостроительЗапроса", "ВыполнитьПакет", 0),
        ("Новый ПостроительОтчета", "ВыполнитьПакет", 0),
        ("Новый Запрос", "ВыполнитьПакет", 1),
    ] {
        let source = format!("Процедура Помощник()\n\tП = {ctor};\n\tП.{exec}();\nКонецПроцедуры\n\nПроцедура Обход(Коллекция)\n\tДля Каждого Э Из Коллекция Цикл\n\t\tПомощник();\n\tКонецЦикла;\nКонецПроцедуры\n");
        assert_eq!(count_call(&source), expected, "{ctor} {exec}");
    }
}

#[test]
fn create_query_in_cycle_i13_paths_do_not_multiply() {
    let diamond = "Процедура Низ()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\n\tЗ.ВыполнитьПакет();\nКонецПроцедуры\n\nПроцедура Левый()\n\tНиз();\nКонецПроцедуры\n\nПроцедура Правый()\n\tНиз();\nКонецПроцедуры\n\nПроцедура Верх()\n\tЛевый();\n\tПравый();\nКонецПроцедуры\n\nПроцедура Обход(Коллекция)\n\tДля Каждого Э Из Коллекция Цикл\n\t\tВерх();\n\tКонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(reported_call(diamond), vec!["Верх()"]);
    assert_eq!(count(diamond), 0);
    // An execution nested in a helper call's argument and the helper call are two predicates.
    let nested = "Функция Помощник(Значение)\n\tЗ = Новый Запрос;\n\tВозврат З.Выполнить();\nКонецФункции\n\nПроцедура Обход(Коллекция)\n\tЗ = Новый Запрос;\n\tДля Каждого Э Из Коллекция Цикл\n\t\tПомощник(З.Выполнить());\n\tКонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(reported_call(nested), vec!["Помощник(З.Выполнить())"]);
    assert_eq!(reported(nested), vec!["З.Выполнить()"]);
}

const SUPPRESSED_HELPER: &str =
    include_str!("fixtures/create_query_in_cycle/suppressed_helper.bsl");
const SUPPRESSED_CHAIN: &str = include_str!("fixtures/create_query_in_cycle/suppressed_chain.bsl");
const SUPPRESSED_RECURSION: &str =
    include_str!("fixtures/create_query_in_cycle/suppressed_recursion.bsl");
const MIXED_EXECUTIONS: &str = include_str!("fixtures/create_query_in_cycle/mixed_executions.bsl");
const DISABLED_PROFILE: &str = include_str!("fixtures/create_query_in_cycle/disabled_profile.bsl");

const NEXT_LINE: &str = "// bsl-analyzer:disable-next-line CreateQueryInCycle\n";
const NEXT_LINE_CALL: &str = "// bsl-analyzer:disable-next-line LocalCallWithQueryInCycle\n";

/// `source` without every line that holds `needle`.
fn without_lines(source: &str, needle: &str) -> String {
    source.lines().filter(|l| !l.contains(needle)).map(|l| format!("{l}\n")).collect()
}

fn unsuppressed(source: &str) -> String {
    without_lines(source, "bsl-analyzer:")
}

/// The text of the line each finding starts on.
fn finding_lines(source: &str, diags: &[crate::Diagnostic]) -> Vec<String> {
    let mut lines: Vec<_> = diags
        .iter()
        .filter(|d| d.code == CALL)
        .map(|d| {
            let start = usize::from(d.range.start());
            let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
            let line_end = source[start..].find('\n').map_or(source.len(), |i| start + i);
            source[line_start..line_end].trim().to_string()
        })
        .collect();
    lines.sort();
    lines
}

fn raw_db(text: &str) -> (ide_db::RootDatabaseImpl, vfs::FileId) {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    let file_id = vfs::FileId(0);
    let mut db = ide_db::RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    (db, file_id)
}

fn raw_diagnostics(text: &str, config: &DiagnosticsConfig) -> Vec<crate::Diagnostic> {
    let (db, file_id) = raw_db(text);
    crate::file_diagnostics(&db, file_id, config)
}

/// One database carried through `revisions`; each answer equals the answer of a clean one.
/// Returns the finding count of each revision.
fn counts_on_one_db(revisions: &[&str]) -> Vec<usize> {
    use ide_db::base_db::SourceDatabase;
    let config = DiagnosticsConfig::default();
    let (mut db, file_id) = raw_db(revisions[0]);
    revisions
        .iter()
        .map(|text| {
            db.set_file_text(file_id, text);
            let incremental = crate::file_diagnostics(&db, file_id, &config);
            assert_eq!(incremental, raw_diagnostics(text, &config), "revision:\n{text}");
            incremental.iter().filter(|d| d.code == CALL).count()
        })
        .collect()
}

#[test]
fn create_query_in_cycle_i14_suppressed_helper() {
    assert_eq!(count_call(SUPPRESSED_HELPER), 0);
    assert_eq!(count_call(&unsuppressed(SUPPRESSED_HELPER)), 2);
    let elsewhere = unsuppressed(SUPPRESSED_HELPER)
        .replace("\tЗапрос = Новый Запрос;\n", &format!("\t{NEXT_LINE}\tЗапрос = Новый Запрос;\n"));
    assert_eq!(count_call(&elsewhere), 2);
}

#[test]
fn create_query_in_cycle_i15_suppressed_chain_and_recursion() {
    for source in [SUPPRESSED_CHAIN, SUPPRESSED_RECURSION] {
        assert_eq!(count_call(source), 0, "{source}");
        assert_eq!(count_call(&unsuppressed(source)), 2, "{source}");
    }
}

#[test]
fn create_query_in_cycle_i16_only_the_named_execution_stops_being_a_ground() {
    assert_eq!(count_call(MIXED_EXECUTIONS), 2);
    let both = MIXED_EXECUTIONS.replace(
        "\tЗапрос.ВыполнитьПакет();\n",
        &format!("\t{NEXT_LINE}\tЗапрос.ВыполнитьПакет();\n"),
    );
    assert_eq!(count_call(&both), 0);
    let swapped = unsuppressed(MIXED_EXECUTIONS).replace(
        "\tЗапрос.ВыполнитьПакет();\n",
        &format!("\t{NEXT_LINE}\tЗапрос.ВыполнитьПакет();\n"),
    );
    assert_eq!(count_call(&swapped), 2);
    assert_eq!(count_call(&unsuppressed(MIXED_EXECUTIONS)), 2);
}

#[test]
fn create_query_in_cycle_i17_directive_addresses_the_execution_line() {
    let before_text = unsuppressed(SUPPRESSED_HELPER)
        .replace("\tЗапрос.Текст = ", &format!("\t{NEXT_LINE}\tЗапрос.Текст = "));
    assert_eq!(count_call(&before_text), 2);
    // A multi-line call is addressed by the line it starts on.
    let multiline = "Процедура Помощник()\n\tЗапрос = Новый Запрос;\n\t// bsl-analyzer:disable-next-line CreateQueryInCycle\n\tЗапрос.ВыполнитьПакет(\n\t);\nКонецПроцедуры\n\nПроцедура Обход(К)\n\tДля Каждого Э Из К Цикл\n\t\tПомощник();\n\tКонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(count_call(multiline), 0);
    let on_the_closing_line = "Процедура Помощник()\n\tЗапрос = Новый Запрос;\n\tЗапрос.ВыполнитьПакет(\n\t// bsl-analyzer:disable-next-line CreateQueryInCycle\n\t);\nКонецПроцедуры\n\nПроцедура Обход(К)\n\tДля Каждого Э Из К Цикл\n\t\tПомощник();\n\tКонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(count_call(on_the_closing_line), 1);
}

#[test]
fn create_query_in_cycle_i18_a_suppressed_call_hides_only_itself() {
    let plain = unsuppressed(SUPPRESSED_HELPER);
    let config = DiagnosticsConfig::default();
    let lines = |source: &str| finding_lines(source, &raw_diagnostics(source, &config));

    let off_on = plain
        .replace(
            "\tДля Каждого Тариф Из Тарифы Цикл\n",
            "\t// bsl-analyzer:off LocalCallWithQueryInCycle\n\tДля Каждого Тариф Из Тарифы Цикл\n",
        )
        .replace(
            "\tКонецЦикла;\nКонецПроцедуры\n\nПроцедура ВторойОбход",
            "\tКонецЦикла;\n\t// bsl-analyzer:on LocalCallWithQueryInCycle\nКонецПроцедуры\n\nПроцедура ВторойОбход",
        );
    assert_eq!(lines(&off_on), vec!["ПрочитатьЦены();"]);
    assert_eq!(count_call(&without_lines(&off_on, "bsl-analyzer:")), 2);

    let on_the_call = plain.replacen(
        "\t\tПрочитатьЦены();\n",
        "\t\tПрочитатьЦены(); // bsl-analyzer:disable-line LocalCallWithQueryInCycle\n",
        1,
    );
    // The first caller's finding is hidden, the second caller's stays on its own line.
    let remaining: Vec<usize> = raw_diagnostics(&on_the_call, &config)
        .iter()
        .filter(|d| d.code == CALL)
        .map(|d| usize::from(d.range.start()))
        .collect();
    assert_eq!(remaining, vec![on_the_call.rfind("ПрочитатьЦены()").unwrap()]);

    // A directive on the header leaves the body's line alone.
    let on_the_header = plain.replacen(
        "\tДля Каждого Тариф Из Тарифы Цикл\n",
        &format!("\t{NEXT_LINE_CALL}\tДля Каждого Тариф Из Тарифы Цикл\n"),
        1,
    );
    assert_eq!(lines(&on_the_header).len(), 2);

    // Suppressing a call inside a middle helper does not clear what it calls for other parents.
    let middle = "Процедура Низ()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n\nПроцедура Середина()\n\tНиз(); // bsl-analyzer:disable-line LocalCallWithQueryInCycle\nКонецПроцедуры\n\nПроцедура Первый(К)\n\tДля Каждого Э Из К Цикл\n\t\tСередина();\n\tКонецЦикла;\nКонецПроцедуры\n\nПроцедура Второй(К)\n\tДля Каждого Э Из К Цикл\n\t\tНиз();\n\tКонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(lines(middle), vec!["Низ();", "Середина();"]);
}

fn helper_with_directive(directive: &str) -> String {
    format!("Процедура Помощник()\n\tЗ = Новый Запрос;\n\t{directive}\n\tЗ.Выполнить();\nКонецПроцедуры\n\nПроцедура Обход(К)\n\tДля Каждого Э Из К Цикл\n\t\tПомощник();\n\tКонецЦикла;\nКонецПроцедуры\n")
}

fn with_code(diags: &[crate::Diagnostic], code: DiagnosticCode) -> usize {
    diags.iter().filter(|d| d.code == code).count()
}

#[test]
fn create_query_in_cycle_i19_directive_semantics_stay_line_based() {
    let config = DiagnosticsConfig::default();
    let run = |directive: &str| raw_diagnostics(&helper_with_directive(directive), &config);

    let native = run("// bsl-analyzer:disable-next-line CreateQueryInCycle");
    assert_eq!(with_code(&native, CALL), 0);

    let typo = run("// bsl-analyzer:disable-next-line CreateQueryInCycl");
    assert_eq!(with_code(&typo, CALL), 1);
    assert_eq!(with_code(&typo, DiagnosticCode::UnknownSuppressionCode), 1);

    let code_less = run("// bsl-analyzer:disable-next-line");
    assert_eq!(with_code(&code_less, CALL), 0);
    assert_eq!(with_code(&code_less, DiagnosticCode::SuppressionWithoutCode), 1);

    let skip_check = run("// @skip-check CreateQueryInCycle");
    assert_eq!(with_code(&skip_check, CALL), 1);
    let foreign = run("// other-tool:disable-next-line CreateQueryInCycle");
    assert_eq!(with_code(&foreign, CALL), 1);

    // An explanation written into the code list is a token of that list.
    let reason_in_list = run("// bsl-analyzer:disable-next-line CreateQueryInCycle обоснование");
    assert_eq!(with_code(&reason_in_list, DiagnosticCode::UnknownSuppressionCode), 1);

    // Unmatched forms.
    let unmatched_on = run("// bsl-analyzer:on CreateQueryInCycle");
    assert_eq!(with_code(&unmatched_on, CALL), 1);
    let unmatched_off = run("// bsl-analyzer:off CreateQueryInCycle");
    assert_eq!(with_code(&unmatched_off, CALL), 0);

    // BSLLS aliases under both settings, native under the setting that turns them off.
    let bslls = helper_with_directive("// BSLLS:CreateQueryInCycle-off");
    assert_eq!(with_code(&raw_diagnostics(&bslls, &config), CALL), 0);
    let no_compat = DiagnosticsConfig { bslls_suppression_compat: false, ..Default::default() };
    assert_eq!(with_code(&raw_diagnostics(&bslls, &no_compat), CALL), 1);
    let native_no_compat =
        helper_with_directive("// bsl-analyzer:disable-next-line CreateQueryInCycle");
    assert_eq!(with_code(&raw_diagnostics(&native_no_compat, &no_compat), CALL), 0);
    let foreign_bslls = run("// BSLLS:НетТакогоКода-off");
    assert_eq!(with_code(&foreign_bslls, DiagnosticCode::UnknownSuppressionCode), 0);
    assert_eq!(with_code(&foreign_bslls, CALL), 1);

    // Line endings.
    for source in [SUPPRESSED_HELPER, MIXED_EXECUTIONS] {
        let crlf = source.replace('\n', "\r\n");
        assert_eq!(
            with_code(&raw_diagnostics(&crlf, &config), CALL),
            with_code(&raw_diagnostics(source, &config), CALL),
        );
    }

    // A meta survives a directive covering it, unless its own code is off.
    let covered =
        "// bsl-analyzer:off CreateQueryInCycle\n// bsl-analyzer:off НетТакого\nПроцедура П()\nКонецПроцедуры\n";
    assert_eq!(
        with_code(&raw_diagnostics(covered, &config), DiagnosticCode::UnknownSuppressionCode),
        1
    );
    let meta_off = DiagnosticsConfig {
        disabled: vec![DiagnosticCode::UnknownSuppressionCode],
        ..Default::default()
    };
    assert_eq!(
        with_code(&raw_diagnostics(covered, &meta_off), DiagnosticCode::UnknownSuppressionCode),
        0
    );
}

#[test]
fn create_query_in_cycle_i20_profile_on_one_database() {
    let (db, file_id) = raw_db(DISABLED_PROFILE);
    // (direct findings, local-call findings): the fixture holds two of each.
    let run = |config: &DiagnosticsConfig| {
        let diags = crate::file_diagnostics(&db, file_id, config);
        (with_code(&diags, CODE), with_code(&diags, CALL))
    };
    let enabled = DiagnosticsConfig::default();
    let disabled = DiagnosticsConfig { disabled: vec![CODE, CALL], ..Default::default() };
    let direct_off = DiagnosticsConfig { disabled: vec![CODE], ..Default::default() };
    let call_off = DiagnosticsConfig { disabled: vec![CALL], ..Default::default() };
    let only_other = DiagnosticsConfig {
        only_enabled: Some(vec![DiagnosticCode::SelfAssign]),
        ..Default::default()
    };
    let only_direct = DiagnosticsConfig { only_enabled: Some(vec![CODE]), ..Default::default() };
    let only_call = DiagnosticsConfig { only_enabled: Some(vec![CALL]), ..Default::default() };

    assert_eq!(run(&disabled), (0, 0));
    assert_eq!(run(&only_other), (0, 0));
    assert_eq!(run(&enabled), (2, 2));
    assert_eq!(run(&disabled), (0, 0));
    // Each code is switched on its own: neither one's profile hides the other.
    assert_eq!(run(&direct_off), (0, 2));
    assert_eq!(run(&call_off), (2, 0));
    assert_eq!(run(&only_direct), (2, 0));
    assert_eq!(run(&only_call), (0, 2));

    let meta = crate::handlers::get_metadata(CODE).expect("registered");
    assert!(meta.activated_by_default);
    assert_eq!(meta.scope, crate::metadata::DiagnosticScope::All);
    assert!(meta.modules.is_empty());
    let call_meta = crate::handlers::get_metadata(CALL).expect("registered");
    assert!(call_meta.activated_by_default);
    assert_eq!(call_meta.scope, crate::metadata::DiagnosticScope::All);
    assert!(call_meta.modules.is_empty());
    let diags = crate::file_diagnostics(&db, file_id, &enabled);
    let finding = diags.iter().find(|d| d.code == CODE).expect("a finding");
    assert_eq!(finding.severity, meta.effective_severity());
    assert_eq!(finding.severity, crate::Severity::Critical);
    let call_finding = diags.iter().find(|d| d.code == CALL).expect("a finding");
    assert_eq!(call_finding.severity, call_meta.effective_severity());
    assert_eq!(call_finding.severity, crate::Severity::Warning);

    let lowered = DiagnosticsConfig {
        metadata_overrides: std::collections::HashMap::from([(
            CODE,
            crate::MetadataOverride {
                lsp_severity: Some("Warning".to_string()),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let diags = crate::file_diagnostics(&db, file_id, &lowered);
    assert!(diags
        .iter()
        .filter(|d| d.code == CODE)
        .all(|d| d.severity == crate::Severity::Warning));
    assert_eq!(with_code(&diags, CODE), 2);
    assert_eq!(with_code(&diags, CALL), 2);
}

#[test]
fn create_query_in_cycle_i21_comment_only_edits_on_one_database() {
    let suppressed = SUPPRESSED_HELPER.to_string();
    let plain = unsuppressed(SUPPRESSED_HELPER);
    assert_eq!(counts_on_one_db(&[&plain, &suppressed, &plain, &suppressed]), vec![2, 0, 2, 0]);
}

#[test]
fn create_query_in_cycle_i22_body_and_edge_edits_on_one_database() {
    let base = "Процедура Первый1()\n\tПервый2();\nКонецПроцедуры\n\nПроцедура Первый2()\n\tЗ = Новый Запрос;\n\tЗ.ВыполнитьПакет();\nКонецПроцедуры\n\nПроцедура Второй()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n\nПроцедура Обход(К)\n\tДля Каждого Э Из К Цикл\n\t\tПервый1();\n\t\tВторой();\n\tКонецЦикла;\nКонецПроцедуры\n";
    let no_execution = base.replace("\tЗ.ВыполнитьПакет();\n", "");
    let no_edge = base.replace("\tПервый2();\n", "");
    assert_eq!(counts_on_one_db(&[base, &no_execution, base, &no_edge, base]), vec![2, 1, 2, 1, 2]);
}

#[test]
fn create_query_in_cycle_i23_neighbours_keep_their_findings() {
    let source = "Процедура Тест(Ссылки)\n\tЗапрос = Новый Запрос;\n\tДля Каждого Ссылка Из Ссылки Цикл\n\t\tЗапрос.Выполнить();\n\t\tА = А;\n\tКонецЦикла;\nКонецПроцедуры\n";
    let config = DiagnosticsConfig::default();
    let with = raw_diagnostics(source, &config);
    assert_eq!(with_code(&with, CODE), 1);
    assert_eq!(with_code(&with, DiagnosticCode::SelfAssign), 1);
    let without = raw_diagnostics(&source.replace("\t\tЗапрос.Выполнить();\n", ""), &config);
    assert_eq!(with_code(&without, CODE), 0);
    assert_eq!(with_code(&without, DiagnosticCode::SelfAssign), 1);
}

#[test]
fn create_query_in_cycle_i24_positions_follow_the_text_on_one_database() {
    use ide_db::base_db::SourceDatabase;
    let config = DiagnosticsConfig::default();
    let shifted = format!("// Сдвиг.\n// Ещё сдвиг.\n{SUPPRESSED_HELPER}");
    let shifted_plain = unsuppressed(&shifted);
    let (mut db, file_id) = raw_db(SUPPRESSED_HELPER);

    let first = crate::file_diagnostics(&db, file_id, &config);
    assert_eq!(with_code(&first, CALL), 0);

    db.set_file_text(file_id, &shifted);
    let moved = crate::file_diagnostics(&db, file_id, &config);
    assert_eq!(with_code(&moved, CALL), 0, "the directive still addresses the execution");
    assert_eq!(moved, raw_diagnostics(&shifted, &config));

    db.set_file_text(file_id, &shifted_plain);
    let revealed = crate::file_diagnostics(&db, file_id, &config);
    assert_eq!(revealed, raw_diagnostics(&shifted_plain, &config));
    let expected: Vec<usize> =
        shifted_plain.match_indices("\t\tПрочитатьЦены()").map(|(at, _)| at + 2).collect();
    let mut starts: Vec<usize> =
        revealed.iter().filter(|d| d.code == CALL).map(|d| usize::from(d.range.start())).collect();
    starts.sort();
    assert_eq!(starts, expected);
}

fn extension_findings(code: DiagnosticCode, base: &str, ext: &str) -> Vec<String> {
    let mut builder = test_fixture::CfeFixtureBuilder::new("");
    builder.add_base_module("Сервер", base);
    builder.add_extension("Расш", "");
    crate::test_utils::check_cfe_at_with_db(
        "CommonModules/Сервер/Ext/Module.bsl",
        ext,
        builder.build(),
        &[],
        DiagnosticsConfig::default(),
        |_| {},
        |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
    )
    .into_iter()
    .filter(|d| d.code == code)
    .map(|d| ext[d.range].to_string())
    .collect()
}

/// A directive in the base helper belongs to the effective text the mask reads.
#[test]
fn create_query_in_cycle_i14_effective_mask_reads_the_effective_text() {
    let base = |directive: &str| {
        format!("Процедура Цель(К) Экспорт\n\tА = 1;\nКонецПроцедуры\n\nПроцедура Помощник()\n\tЗ = Новый Запрос;\n\t{directive}\n\tЗ.Выполнить();\nКонецПроцедуры\n")
    };
    let ext = "&ИзменениеИКонтроль(\"Цель\")\nПроцедура Расш_Цель(К)\n\tА = 1;\n#Вставка\n\tДля Каждого Э Из К Цикл\n\t\tПомощник();\n\tКонецЦикла;\n#КонецВставки\nКонецПроцедуры\n";
    assert_eq!(extension_findings(CALL, &base("// Комментарий."), ext), vec!["Помощник()"]);
    assert_eq!(
        extension_findings(
            CALL,
            &base("// bsl-analyzer:disable-next-line CreateQueryInCycle"),
            ext,
        ),
        Vec::<String>::new()
    );

    let direct = |directive: &str| {
        format!("&ИзменениеИКонтроль(\"Цель\")\nПроцедура Расш_Цель(К)\n\tА = 1;\n#Вставка\n\tЗ = Новый Запрос;\n\tДля Каждого Э Из К Цикл\n\t\t{directive}\n\t\tЗ.Выполнить();\n\tКонецЦикла;\n#КонецВставки\nКонецПроцедуры\n")
    };
    let silent_base = "Процедура Цель(К) Экспорт\n\tА = 1;\nКонецПроцедуры\n";
    assert_eq!(
        extension_findings(CODE, silent_base, &direct("// Комментарий.")),
        vec!["З.Выполнить()"]
    );
    assert_eq!(
        extension_findings(
            CODE,
            silent_base,
            &direct("// bsl-analyzer:disable-next-line CreateQueryInCycle")
        ),
        Vec::<String>::new()
    );
}

/// An interceptor keeps its own text: its helper is followed once, a base method is not.
#[test]
fn create_query_in_cycle_i09_weaving_stays_in_the_extension_module() {
    let base = "Процедура Цель(К) Экспорт\nКонецПроцедуры\n\nПроцедура ВБазе()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n";
    let ext = |call: &str| {
        format!("&После(\"Цель\")\nПроцедура Расш_Цель(К)\n\tДля Каждого Э Из К Цикл\n\t\t{call}();\n\tКонецЦикла;\nКонецПроцедуры\n\nПроцедура ВРасширении()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n")
    };
    assert_eq!(extension_findings(CALL, base, &ext("ВРасширении")), vec!["ВРасширении()"]);
    assert_eq!(extension_findings(CALL, base, &ext("ВБазе")), Vec::<String>::new());
}

#[test]
fn create_query_in_cycle_i10_suppression_in_english() {
    let source = "Procedure Helper()\n\tRequest = New Query;\n\t// bsl-analyzer:disable-next-line CreateQueryInCycle\n\tRequest.ExecuteBatch();\nEndProcedure\n\nProcedure First(Items)\n\tFor Each Item In Items Do\n\t\thelper();\n\tEndDo;\nEndProcedure\n\nProcedure Second(Items)\n\tWhile Items.Next() Do\n\t\tHELPER();\n\tEndDo;\nEndProcedure\n";
    assert_eq!(count_call(source), 0);
    assert_eq!(count_call(&unsuppressed(source)), 2);
}

/// `Для` and `Для Каждого` evaluate their header once, on entry, so what their own body writes
/// is not what the header reads, while what comes back round an outer loop is.
#[test]
fn create_query_in_cycle_i11_header_of_a_nested_loop_reads_the_entry_state() {
    let nested = |before: &str, header: &str, body: &str| {
        in_procedure(&format!(
            "{before}\tПока Параметр Цикл\n\t\t{header}\n\t\t\t{body}\n\t\tКонецЦикла;\n\tКонецЦикла;"
        ))
    };
    let query = "З = Новый Запрос;";
    let foreign = "З = Новый ТаблицаЗначений;";
    let for_each = "Для Каждого Э Из З.Выполнить().Выгрузить() Цикл";
    let for_from = "Для Й = З.Выполнить() По 3 Цикл";
    let for_to = "Для Й = 1 По З.Выполнить() Цикл";
    for header in [for_each, for_from, for_to] {
        // The write in the body is overwritten before the next entry into the loop.
        let rewritten = format!("\tПока Параметр Цикл\n\t\t{query}\n\t\t{header}\n\t\t\t{foreign}\n\t\tКонецЦикла;\n\tКонецЦикла;");
        assert_eq!(count(&in_procedure(&rewritten)), 1, "{header}");
        // No outer pass overwrites it: on the next entry the receiver may be the foreign value.
        assert_eq!(count(&nested(&format!("\t{query}\n"), header, foreign)), 0, "{header}");
        // Control: with the body's write gone the same reads are proved.
        assert_eq!(count(&nested(&format!("\t{query}\n"), header, "А = 1;")), 1, "{header}");
    }
    // A `Пока` condition runs anew after its body, so the body's write does reach it.
    let while_header = format!("\tПока Параметр Цикл\n\t\t{query}\n\t\tПока З.Выполнить() Цикл\n\t\t\t{foreign}\n\t\tКонецЦикла;\n\tКонецЦикла;");
    assert_eq!(count(&in_procedure(&while_header)), 0);
}

/// A write the receiver's reader cannot see: it is followed by a jump or sits in dead code.
#[test]
fn create_query_in_cycle_i08_a_write_behind_a_jump_is_not_a_mix() {
    let branch = |first: &str| {
        format!("Процедура Проверить(Условие, Элементы)\n    Если Условие Тогда\n{first}    Иначе\n        Запрос = Новый Запрос;\n    КонецЕсли;\n    Для Каждого Элемент Из Элементы Цикл\n        Запрос.Выполнить();\n    КонецЦикла;\nКонецПроцедуры\n")
    };
    for first in [
        "        Возврат;\n        Запрос = Новый Массив;\n",
        "        Запрос = Новый Массив;\n        Возврат;\n",
        "        Запрос = Новый Массив;\n        ВызватьИсключение \"Нет\";\n",
    ] {
        assert_eq!(count(&branch(first)), 1, "{first}");
    }
    // Control: a write that does fall through still mixes the type.
    assert_eq!(count(&branch("        Запрос = Новый Массив;\n")), 0);

    let in_loop = |after: &str| {
        format!("Процедура Проверить(Условие, Элементы)\n    Запрос = Новый Запрос;\n    Для Каждого Элемент Из Элементы Цикл\n        Если Условие Тогда\n            Запрос = Новый Массив;\n{after}        КонецЕсли;\n        Запрос.Выполнить();\n    КонецЦикла;\nКонецПроцедуры\n")
    };
    assert_eq!(count(&in_loop("            Прервать;\n")), 1);
    assert_eq!(count(&in_loop("            Возврат;\n")), 1);
    // `Продолжить` keeps the write live: the next pass reads it.
    assert_eq!(count(&in_loop("            Продолжить;\n")), 0);
    assert_eq!(count(&in_loop("")), 0);

    // An exception handler is reachable from what the protected statements did.
    let in_try = "Процедура Проверить(Элементы)\n    Запрос = Новый Запрос;\n    Попытка\n        Запрос = Новый Массив;\n        ВызватьИсключение \"Нет\";\n    Исключение\n        А = 1;\n    КонецПопытки;\n    Для Каждого Элемент Из Элементы Цикл\n        Запрос.Выполнить();\n    КонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(count(in_try), 0);

    // An execution in dead code is no execution.
    let dead_execution = "Процедура Проверить(Элементы)\n    Запрос = Новый Запрос;\n    Для Каждого Элемент Из Элементы Цикл\n        Возврат;\n        Запрос.Выполнить();\n    КонецЦикла;\nКонецПроцедуры\n";
    assert_eq!(count(dead_execution), 0);
}

/// A receiver whose one declared type is a union: each arm has to prove the operation.
#[test]
fn create_query_in_cycle_i08_every_arm_of_a_union_proves_the_operation() {
    let source = |arms: &str, operation: &str| {
        format!("// Возвращаемое значение:\n//   {arms}\nФункция Источник() Экспорт\n\tВозврат Неопределено;\nКонецФункции\n\nПроцедура Тест(Коллекция)\n\tЗ = Источник();\n\tДля Каждого Э Из Коллекция Цикл\n\t\tЗ.{operation}();\n\tКонецЦикла;\nКонецПроцедуры\n")
    };
    // A foreign or unknown arm leaves the call unproved.
    assert_eq!(count(&source("Запрос, Массив", "Выполнить")), 0);
    assert_eq!(count(&source("Запрос, Неопределено", "ВыполнитьПакет")), 0);
    // A builder arm proves `Выполнить` only, as the builder has no batch to run.
    assert_eq!(count(&source("Запрос, ПостроительЗапроса", "Выполнить")), 1);
    assert_eq!(count(&source("Запрос, ПостроительЗапроса", "ВыполнитьПакет")), 0);
    // Control: the same declaration with the one known type is proved.
    assert_eq!(count(&source("Запрос", "Выполнить")), 1);
    assert_eq!(count(&source("Запрос", "ВыполнитьПакет")), 1);
}

#[test]
fn create_query_in_cycle_i08_recovered_helper_call_makes_no_edge() {
    let source = |call: &str| {
        format!("Процедура Помощник()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n\nПроцедура Тест(Коллекция)\n\tДля Каждого Э Из Коллекция Цикл\n\t\t{call}\n\tКонецЦикла;\nКонецПроцедуры\n")
    };
    assert_eq!(reported_call(&source("Помощник();")), vec!["Помощник()"]);
    assert_eq!(count_call(&source("Помощник(;")), 0);
    // The call that follows a broken one is still judged on its own.
    assert_eq!(reported_call(&source("Помощник(;\n\t\tПомощник();")), vec!["Помощник()"]);
}

#[test]
fn create_query_in_cycle_i04_removing_an_edge_of_a_long_chain() {
    let query = "\tЗ = Новый Запрос;\n\tЗ.ВыполнитьПакет();\n";
    for len in [2, 40, 400] {
        for reversed in [false, true] {
            let whole = chain(len, query, reversed);
            assert_eq!(reported_call(&whole), vec!["П0()"], "{len} {reversed}");
            // The last edge, into the method that holds the execution, and one in the middle.
            for target in [len - 1, len / 2] {
                let cut = whole.replace(&format!("\tП{target}();\n"), "");
                assert_ne!(cut, whole, "the edge into П{target} exists");
                assert_eq!(count_call(&cut), 0, "edge into П{target} of {len} {reversed}");
            }
        }
    }
}

#[test]
fn create_query_in_cycle_i11_module_code_loop_forms() {
    let helper = "Процедура Помощник()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n\n";
    let heads = [
        "Для Каждого Э Из К Цикл",
        "Для Й = 1 По 3 Цикл",
        "Пока Истина Цикл",
        "Пока Истина Цикл\n\tДля Й = 1 По 3 Цикл",
    ];
    for head in heads {
        let tail = if head.contains('\n') {
            "КонецЦикла;\nКонецЦикла;"
        } else {
            "КонецЦикла;"
        };
        let direct =
            format!("З = Новый Запрос;\nК = Новый Массив;\n{head}\n\tЗ.Выполнить();\n{tail}\n");
        assert_eq!(count(&direct), 1, "{head}");
        let through_helper = format!("{helper}К = Новый Массив;\n{head}\n\tПомощник();\n{tail}\n");
        assert_eq!(reported_call(&through_helper), vec!["Помощник()"], "{head}");
        assert_eq!(count(&through_helper), 0, "{head}");
        // The same loop without the call in its body.
        let empty = format!("{helper}К = Новый Массив;\n{head}\n\tА = 1;\n{tail}\nПомощник();\n");
        assert_eq!(count_call(&empty), 0, "{head}");
    }
    // A header evaluated once, outside of any outer loop, is not a loop body.
    let header = "З = Новый Запрос;\nДля Каждого Э Из З.Выполнить().Выгрузить() Цикл\n\tА = 1;\nКонецЦикла;\n";
    assert_eq!(count(header), 0);
}

#[test]
fn create_query_in_cycle_i20_tags_come_from_the_context() {
    let (db, file_id) = raw_db(DISABLED_PROFILE);
    let tagged = |config: &DiagnosticsConfig| -> Vec<(DiagnosticCode, Vec<crate::DiagnosticTag>)> {
        crate::file_diagnostics(&db, file_id, config)
            .into_iter()
            .filter(|d| d.code == CODE || d.code == CALL)
            .map(|d| (d.code, d.tags))
            .collect()
    };
    let default = DiagnosticsConfig::default();
    let unused_override = || crate::MetadataOverride {
        tags: Some(vec![crate::metadata::MetadataTag::Unused]),
        ..Default::default()
    };
    let unused = DiagnosticsConfig {
        metadata_overrides: std::collections::HashMap::from([
            (CODE, unused_override()),
            (CALL, unused_override()),
        ]),
        ..Default::default()
    };
    // Direct, batch, helper and recursive findings all carry what the context names.
    let by_default = tagged(&default);
    assert_eq!(by_default.len(), 4);
    assert!(by_default.iter().all(|(code, tags)| *tags == default.tags(*code)), "{by_default:?}");
    let overridden = tagged(&unused);
    assert_eq!(overridden.len(), 4);
    assert!(
        overridden.iter().all(|(_, tags)| *tags == vec![crate::DiagnosticTag::Unnecessary]),
        "{overridden:?}"
    );
}

#[test]
fn create_query_in_cycle_i20_profile_in_the_effective_module() {
    let mut builder = test_fixture::CfeFixtureBuilder::new("");
    builder.add_base_module(
        "Сервер",
        "Процедура Цель(Коллекция) Экспорт\n\tА = 1;\nКонецПроцедуры\n\nПроцедура Помощник()\n\tЗ = Новый Запрос;\n\tЗ.Выполнить();\nКонецПроцедуры\n",
    );
    builder.add_extension("Расш", "");
    let ext = "&ИзменениеИКонтроль(\"Цель\")\nПроцедура Расш_Цель(Коллекция)\n\tА = 1;\n#Вставка\n\tДля Каждого Э Из Коллекция Цикл\n\t\tПомощник();\n\tКонецЦикла;\n#КонецВставки\nКонецПроцедуры\n";
    let disabled = DiagnosticsConfig { disabled: vec![CALL], ..Default::default() };
    let direct_off = DiagnosticsConfig { disabled: vec![CODE], ..Default::default() };
    let only_other = DiagnosticsConfig {
        only_enabled: Some(vec![DiagnosticCode::SelfAssign]),
        ..Default::default()
    };
    let enabled = DiagnosticsConfig::default();
    // One database through every profile: the base-aware pass reads the profile of the call.
    let last = crate::test_utils::check_cfe_at_with_db(
        "CommonModules/Сервер/Ext/Module.bsl",
        ext,
        builder.build(),
        &[],
        DiagnosticsConfig::default(),
        |_| {},
        |db, ctx| {
            let found = |config: &DiagnosticsConfig| {
                with_code(&crate::file_diagnostics(db, ctx.file_id, config), CALL)
            };
            assert_eq!(found(&disabled), 0);
            assert_eq!(found(&only_other), 0);
            assert_eq!(found(&enabled), 1);
            assert_eq!(found(&direct_off), 1);
            assert_eq!(found(&disabled), 0);
            crate::file_diagnostics(db, ctx.file_id, &enabled)
        },
    );
    assert_eq!(with_code(&last, CALL), 1);
}

#[test]
fn create_query_in_cycle_i23_a_helper_call_keeps_its_neighbours() {
    let source = |helper_body: &str, neighbour: &str| {
        format!("Процедура Помощник()\n{helper_body}КонецПроцедуры\n\nПроцедура Тест(Коллекция)\n\tДля Каждого Э Из Коллекция Цикл\n\t\tПомощник();\n\t\t{neighbour}\n\tКонецЦикла;\n\t// bsl-analyzer:disable-next-line НетТакогоКода\n\tБ = 1;\nКонецПроцедуры\n")
    };
    let query = "\tЗ = Новый Запрос;\n\tЗ.Выполнить();\n";
    let config = DiagnosticsConfig::default();
    let texts = |text: &str, code: DiagnosticCode| -> Vec<String> {
        raw_diagnostics(text, &config)
            .into_iter()
            .filter(|d| d.code == code)
            .map(|d| text[d.range].to_string())
            .collect()
    };
    let self_assign = DiagnosticCode::SelfAssign;
    let unknown = DiagnosticCode::UnknownSuppressionCode;
    let both = source(query, "А = А;");
    assert_eq!(texts(&both, CALL), vec!["Помощник()"]);
    assert_eq!(texts(&both, self_assign), vec!["А = А"]);
    assert_eq!(texts(&both, unknown).len(), 1);
    // Dropping the execution removes only its finding; the others stay where they were.
    let no_query = source("\tА = 1;\n", "А = А;");
    assert!(texts(&no_query, CALL).is_empty());
    assert_eq!(texts(&no_query, self_assign), vec!["А = А"]);
    assert_eq!(texts(&no_query, unknown).len(), 1);
    // Dropping the neighbour removes only that one.
    let no_neighbour = source(query, "А = 1;");
    assert_eq!(texts(&no_neighbour, CALL), vec!["Помощник()"]);
    assert!(texts(&no_neighbour, self_assign).is_empty());
    assert_eq!(texts(&no_neighbour, unknown).len(), 1);
}

#[test]
fn create_query_in_cycle_i24_moving_only_one_side_of_the_edge() {
    use ide_db::base_db::SourceDatabase;
    let config = DiagnosticsConfig::default();
    let callers_down = |text: &str| {
        text.replace(
            "КонецПроцедуры\n\nПроцедура ПервыйОбход",
            "КонецПроцедуры\n\n// Сдвиг.\n// Сдвиг.\n\nПроцедура ПервыйОбход",
        )
    };
    let helper_down = |text: &str| {
        text.replace("\tЗапрос = Новый Запрос;\n", "\t// Сдвиг.\n\tЗапрос = Новый Запрос;\n")
    };
    let plain = unsuppressed(SUPPRESSED_HELPER);
    for (suppressed, expected) in [
        (callers_down(SUPPRESSED_HELPER), 0),
        (helper_down(SUPPRESSED_HELPER), 0),
        (helper_down(&callers_down(SUPPRESSED_HELPER)), 0),
    ] {
        assert_eq!(count_call(&suppressed), expected, "{suppressed}");
        let revealed = unsuppressed(&suppressed);
        assert_ne!(revealed, plain);
        let starts = |text: &str| -> Vec<usize> {
            let mut starts: Vec<usize> = raw_diagnostics(text, &config)
                .iter()
                .filter(|d| d.code == CALL)
                .map(|d| usize::from(d.range.start()))
                .collect();
            starts.sort();
            starts
        };
        let calls: Vec<usize> =
            revealed.match_indices("\t\tПрочитатьЦены()").map(|(at, _)| at + 2).collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(starts(&revealed), calls, "{revealed}");

        // The same journey on one database: hidden, revealed, hidden again.
        let (mut db, file_id) = raw_db(SUPPRESSED_HELPER);
        for text in [&suppressed, &revealed, &suppressed] {
            db.set_file_text(file_id, text);
            let incremental = crate::file_diagnostics(&db, file_id, &config);
            assert_eq!(incremental, raw_diagnostics(text, &config), "{text}");
            let shown = with_code(&incremental, CALL);
            assert_eq!(shown, if *text == suppressed { 0 } else { 2 });
        }
    }
}
