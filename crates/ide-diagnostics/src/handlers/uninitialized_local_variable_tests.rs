use crate::test_utils::{
    check_diagnostics_for_with_config, check_diagnostics_snapshot_for, check_form_with_form_xml,
    check_with_cfe, format_diags,
};
use crate::{DiagnosticCode, DiagnosticsConfig};
use expect_test::{expect, Expect};

/// One branch assigns `Х`, the read after the `Если` finds it unassigned on the other.
const BASE: &str = "Процедура Обработать(А)
    Если А Тогда
        Х = 1;
    КонецЕсли;
    Б = Х;
КонецПроцедуры
";

const ISSUE_EXAMPLE: &str = "Процедура Обработать()
    Для Номер = 1 По 2 Цикл
        Копия = ПозднееЗначение;
        ПозднееЗначение = 7;
        Сообщить(Копия);
    КонецЦикла;
КонецПроцедуры
";

const LOOP_NOT_RUN: &str = "Процедура Обработать(Условие)
    Пока Условие Цикл
        Значение = 1;
        Условие = Ложь;
    КонецЦикла;
    Копия = Значение;
КонецПроцедуры
";

const TRY_HANDLER: &str = "Процедура Обработать()
    Попытка
        Х = 1;
        Ф();
    Исключение
        Б = Х;
    КонецПопытки;
КонецПроцедуры
";

/// A managed form whose main attribute `Объект` is a constants set.
const FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <Attributes>
        <Attribute name="Объект" id="1">
            <Type>
                <v8:Type>cfg:ConstantsSet</v8:Type>
            </Type>
            <MainAttribute>true</MainAttribute>
        </Attribute>
    </Attributes>
</Form>"#;

fn check(source: &str, expected: Expect) {
    check_diagnostics_snapshot_for(source, DiagnosticCode::UninitializedLocalVariable, expected);
}

/// `UnresolvedName` and this rule's findings in a configured workspace: without a
/// configuration the cascade cannot prove a name absent.
fn name_diagnostics(source: &str) -> String {
    let diagnostics = check_with_cfe(source, test_fixture::CfeFixtureBuilder::new("").build())
        .into_iter()
        .filter(|diag| {
            matches!(
                diag.code,
                DiagnosticCode::UnresolvedName | DiagnosticCode::UninitializedLocalVariable
            )
        })
        .collect::<Vec<_>>();
    format_diags(source, &diagnostics)
}

fn base_with(after_read: &str) -> String {
    BASE.replace("    Б = Х;\n", &format!("    Б = Х;\n{after_read}"))
}

#[test]
fn first_iteration_read_is_reported() {
    check(
        ISSUE_EXAMPLE,
        expect![[r#"
        UninitializedLocalVariable @ 3:17..3:32
          message: Переменная 'ПозднееЗначение' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
          severity: Warning"#]],
    );
    check(
        "Процедура Обработать()
    Для г = 1 По 2 Цикл
        б = а;
        б = б;
        а = Новый Структура;
    КонецЦикла;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 3:13..3:14
              message: Переменная 'а' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
              severity: Warning"#]],
    );
    check(
        &ISSUE_EXAMPLE.replace("    Для Номер", "    ПозднееЗначение = 0;\n    Для Номер"),
        expect![[""]],
    );
}

#[test]
fn read_after_loop_that_may_not_run() {
    check(
        LOOP_NOT_RUN,
        expect![[r#"
        UninitializedLocalVariable @ 6:13..6:21
          message: Переменная 'Значение' может быть не инициализирована: на пути, где цикл не выполняется ни разу, значение ей не присваивается
          severity: Warning"#]],
    );
    let input = LOOP_NOT_RUN.replace("    Пока", "    Значение = 0;\n    Пока");
    check(&input, expect![[""]]);
}

#[test]
fn branches_must_all_assign() {
    let input = BASE.replace("    КонецЕсли;", "    Иначе\n        Х = 2;\n    КонецЕсли;");
    check(&input, expect![[""]]);
    check(
        BASE,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn exiting_branch_does_not_reach_read() {
    let input = BASE.replace("    КонецЕсли;", "    Иначе\n        Возврат;\n    КонецЕсли;");
    check(&input, expect![[""]]);
    check(
        &BASE.replace(
            "    КонецЕсли;",
            "    Иначе\n        ВызватьИсключение \"сбой\";\n    КонецЕсли;",
        ),
        expect![[""]],
    );
    let input = BASE.replace("    КонецЕсли;", "    Иначе\n        В = 2;\n    КонецЕсли;");
    check(
        &input,
        expect![[r#"
        UninitializedLocalVariable @ 7:9..7:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn explicit_var_is_not_judged() {
    let input = BASE.replace("    Если А", "    Перем Х;\n    Если А");
    check(&input, expect![[""]]);
    check(
        BASE,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn parameter_is_not_judged() {
    let input = BASE.replace("Обработать(А)", "Обработать(А, Х)");
    check(&input, expect![[""]]);
    check(
        BASE,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn name_owned_outside_body_is_not_judged() {
    let judged = |source: &str| {
        let diagnostics = check_form_with_form_xml(source, FORM_XML)
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UninitializedLocalVariable)
            .collect::<Vec<_>>();
        format_diags(source, &diagnostics)
    };

    let input = format!("Перем Х;\n\n{BASE}");
    check(&input, expect![[""]]);
    expect![[""]].assert_eq(&judged(&BASE.replace('Х', "Объект")));
    expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]].assert_eq(&judged(BASE));
}

#[test]
fn unknown_value_assignment_initializes() {
    let source = "Процедура Обработать(Параметр)
    Если Параметр Тогда
        Х = Параметр.Что;
    Иначе
        Х = Параметр;
    КонецЕсли;
    Б = Х;
КонецПроцедуры
";
    check(source, expect![[""]]);
    let input = source.replace("    Иначе\n        Х = Параметр;\n", "");
    check(
        &input,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn by_ref_argument_initializes_by_val_does_not() {
    let with_fill = |params: &str| {
        format!(
            "{}\nПроцедура Заполнить({params})\nКонецПроцедуры\n",
            ISSUE_EXAMPLE.replace(
                "        Копия = ПозднееЗначение;",
                "        Заполнить(ПозднееЗначение);\n        Копия = ПозднееЗначение;",
            )
        )
    };
    let input = with_fill("Результат");
    check(&input, expect![[""]]);
    let input = with_fill("Знач Результат");
    check(
        &input,
        expect![[r#"
        UninitializedLocalVariable @ 3:19..3:34
          message: Переменная 'ПозднееЗначение' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
          severity: Warning
        UninitializedLocalVariable @ 4:17..4:32
          message: Переменная 'ПозднееЗначение' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
          severity: Warning"#]],
    );
}

#[test]
fn unproven_call_or_execute_suppresses() {
    let with_call = |call: &str| {
        ISSUE_EXAMPLE.replace(
            "        Копия = ПозднееЗначение;",
            &format!("        {call}\n        Копия = ПозднееЗначение;"),
        )
    };
    let input = with_call("НеизвестныйМетод(ПозднееЗначение);");
    check(&input, expect![[""]]);
    let input = with_call("СтрДлина(ПозднееЗначение);");
    check(&input, expect![[""]]);
    let input = with_call("Выполнить(\"ПозднееЗначение = 1\");");
    check(&input, expect![[""]]);
    check(
        ISSUE_EXAMPLE,
        expect![[r#"
        UninitializedLocalVariable @ 3:17..3:32
          message: Переменная 'ПозднееЗначение' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
          severity: Warning"#]],
    );
}

#[test]
fn nested_loops() {
    check(
        "Процедура Обработать()
    Для Н = 1 По 2 Цикл
        Для М = 1 По 2 Цикл
            Б = Х;
            Х = 1;
        КонецЦикла;
    КонецЦикла;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 4:17..4:18
              message: Переменная 'Х' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
              severity: Warning"#]],
    );
    check(
        "Процедура Обработать()
    Для Н = 1 По 2 Цикл
        Х = 0;
        Для М = 1 По 2 Цикл
            Б = Х;
            Х = 1;
        КонецЦикла;
    КонецЦикла;
КонецПроцедуры
",
        expect![[""]],
    );
    check(
        "Процедура Обработать()
    Для Н = 1 По 2 Цикл
        Для М = 1 По 2 Цикл
            Б = Х;
        КонецЦикла;
        Х = 1;
    КонецЦикла;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 4:17..4:18
              message: Переменная 'Х' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
              severity: Warning"#]],
    );
}

#[test]
fn recovered_statement_suppresses_body() {
    let input = base_with("    А.Свойство\n");
    check(&input, expect![[""]]);
    check(
        BASE,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn goto_without_label_suppresses_body() {
    let input = base_with("    Перейти ~Нет;\n");
    check(&input, expect![[""]]);
    let input = base_with("    Перейти ~Нет;\n~Нет:\n    В = 1;\n");
    check(
        &input,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn solver_budget_exhaustion_suppresses_body() {
    // `PairingBrokenTransaction` walks every path of the method recursively and
    // overflows the test thread's stack on a body this long; it is not under test.
    let judged = |count: usize| {
        let source = base_with(&"    Если Истина Тогда КонецЕсли;\n".repeat(count));
        let mut config = DiagnosticsConfig::all_enabled();
        config.disabled.push(DiagnosticCode::PairingBrokenTransaction);
        let diagnostics = check_diagnostics_for_with_config(
            &source,
            config,
            DiagnosticCode::UninitializedLocalVariable,
        );
        format_diags(&source, &diagnostics)
    };
    expect![[""]].assert_eq(&judged(hir::dataflow::DEFAULT_MAX_ITERATIONS + 1));
    expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]].assert_eq(&judged(1));
}

#[test]
fn absent_name_reported_only_as_unresolved() {
    let source = "Процедура Обработать()
    Б = Х;
    Х = 1;
КонецПроцедуры
";
    expect![[r#"
        UnresolvedName @ 2:9..2:10
          message: Имя 'Х' не определено
          severity: Major"#]]
    .assert_eq(&name_diagnostics(source));
}

#[test]
fn loop_variable_is_initialized_by_header() {
    check(
        "Процедура Обработать(Массив)
    Для Сч = 1 По 2 Цикл
        Б = Сч;
    КонецЦикла;
    В = Сч;
    Для Каждого Эл Из Массив Цикл
        Г = Эл;
    КонецЦикла;
    Д = Эл;
КонецПроцедуры
",
        expect![[""]],
    );
}

#[test]
fn index_write_initializes_field_write_does_not() {
    check(
        "Процедура Обработать(А)
    Если А Тогда
        Т = Новый Массив;
    Иначе
        Т[0] = 1;
    КонецЕсли;
    Б = Т;
КонецПроцедуры
",
        expect![[""]],
    );
    check(
        "Процедура Обработать(А)
    Если А Тогда
        С = Новый Структура;
    Иначе
        С.Поле = 1;
    КонецЕсли;
    В = С;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 5:9..5:10
              message: Переменная 'С' может быть не инициализирована: на одном из путей к этому чтению значение ей не присваивается
              severity: Warning
            UninitializedLocalVariable @ 7:9..7:10
              message: Переменная 'С' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn unreachable_read_is_not_reported() {
    let input = BASE.replace("    Б = Х;", "    Возврат;\n    Б = Х;");
    check(&input, expect![[""]]);
    check(
        BASE,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn argument_and_read_in_one_statement() {
    let input = BASE.replace("    Б = Х;", "    Б = СтрДлина(Х) + Х;");
    check(&input, expect![[""]]);
    check(
        &format!(
            "{}\nФункция ЗнчФ(Знач П)\n    Возврат П;\nКонецФункции\n",
            BASE.replace("    Б = Х;", "    Б = ЗнчФ(Х) + Х;")
        ),
        expect![[r#"
            UninitializedLocalVariable @ 5:14..5:15
              message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
              severity: Warning
            UninitializedLocalVariable @ 5:19..5:20
              message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn platform_read_before_assignment_is_not_local() {
    let source = "Процедура Обработать()
    Б = Метаданные;
    Метаданные = 1;
КонецПроцедуры
";
    expect![[""]].assert_eq(&name_diagnostics(source));
}

#[test]
fn try_handler_and_sibling_branch_are_other_path() {
    check(
        TRY_HANDLER,
        expect![[r#"
        UninitializedLocalVariable @ 6:13..6:14
          message: Переменная 'Х' может быть не инициализирована: на одном из путей к этому чтению значение ей не присваивается
          severity: Warning"#]],
    );
    check(
        "Процедура Обработать(А)
    Если А Тогда
        Х = 1;
    Иначе
        Б = Х;
    КонецЕсли;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 5:13..5:14
              message: Переменная 'Х' может быть не инициализирована: на одном из путей к этому чтению значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn metadata_is_enabled_warning() {
    let metadata = crate::handlers::get_metadata(DiagnosticCode::UninitializedLocalVariable)
        .expect("registered");
    assert!(metadata.activated_by_default);
    assert_eq!(metadata.calculate_severity(), crate::Severity::Warning);
}

#[test]
fn english_messages() {
    let english = |source: &str| {
        let mut config = DiagnosticsConfig::all_enabled();
        config.locale = base_db::Locale::En;
        let diagnostics = check_diagnostics_for_with_config(
            source,
            config,
            DiagnosticCode::UninitializedLocalVariable,
        );
        format_diags(source, &diagnostics)
    };
    expect![[r#"
        UninitializedLocalVariable @ 3:17..3:32
          message: Variable 'ПозднееЗначение' may be uninitialized: on the first iteration of the loop it is not assigned yet; the assignment in the loop takes effect only on later iterations
          severity: Warning"#]].assert_eq(&english(ISSUE_EXAMPLE));
    expect![[r#"
        UninitializedLocalVariable @ 6:13..6:21
          message: Variable 'Значение' may be uninitialized: it is not assigned on a path where the loop runs zero times
          severity: Warning"#]].assert_eq(&english(LOOP_NOT_RUN));
    expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Variable 'Х' may be uninitialized: it is not assigned on a path through a branch of a condition
          severity: Warning"#]].assert_eq(&english(BASE));
    expect![[r#"
        UninitializedLocalVariable @ 6:13..6:14
          message: Variable 'Х' may be uninitialized: it is not assigned on some path to this read
          severity: Warning"#]]
    .assert_eq(&english(TRY_HANDLER));
}

#[test]
fn literal_loop_headers() {
    check(
        "Процедура Обработать(А, Н)
    Пока Истина Цикл
        Значение = 1;
        Если А Тогда
            Прервать;
        КонецЕсли;
    КонецЦикла;
    Б = Значение;
КонецПроцедуры
",
        expect![[""]],
    );
    let for_loop = "Процедура Обработать(А, Н)
    Для Ном = 1 По 3 Цикл
        Значение = Ном;
    КонецЦикла;
    Б = Значение;
КонецПроцедуры
";
    check(for_loop, expect![[""]]);
    let input = for_loop.replace("= 1 По 3", "= -5 По -3");
    check(&input, expect![[""]]);
    let input = for_loop.replace("По 3", "По -1");
    check(
        &input,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:17
          message: Переменная 'Значение' может быть не инициализирована: на пути, где цикл не выполняется ни разу, значение ей не присваивается
          severity: Warning"#]],
    );
    let input = for_loop.replace("По 3", "По Н");
    check(
        &input,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:17
          message: Переменная 'Значение' может быть не инициализирована: на пути, где цикл не выполняется ни разу, значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn loop_variable_read_before_its_loop() {
    check(
        "Процедура Обработать()
    Б = Сч;
    Для Сч = 1 По 2 Цикл
    КонецЦикла;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 2:9..2:11
              message: Переменная 'Сч' может быть не инициализирована: на одном из путей к этому чтению значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn preprocessor_branches_are_ordinary_edges() {
    let source = "Процедура Обработать()
#Если Сервер Тогда
    Х = 1;
#Иначе
    Х = 2;
#КонецЕсли
    Б = Х;
КонецПроцедуры
";
    check(source, expect![[""]]);
    let input = source.replace("#Иначе\n    Х = 2;\n", "");
    check(
        &input,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на одном из путей к этому чтению значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn dropped_statement_suppresses_body() {
    let input = base_with("    Для Каждого Из А Цикл КонецЦикла;\n");
    check(&input, expect![[""]]);
    let input = base_with("    Если А Х = 1; КонецЕсли;\n");
    check(&input, expect![[""]]);
    check(
        BASE,
        expect![[r#"
        UninitializedLocalVariable @ 5:9..5:10
          message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]],
    );
}

#[test]
fn header_roots_are_judged() {
    // A read in the root of a header is evaluated at the header's own point, not at a
    // statement of a block.
    let after_one_branch = |header: &str| {
        format!(
            "Процедура Обработать(А, Б)
    Если А Тогда
        Х = 1;
    КонецЕсли;
{header}
КонецПроцедуры
"
        )
    };
    let branch = |position: &str| {
        format!(
            "UninitializedLocalVariable @ {position}
  message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
  severity: Warning"
        )
    };
    let judged = |header: &str, position: &str| {
        let source = after_one_branch(header);
        let diagnostics = check_diagnostics_for_with_config(
            &source,
            DiagnosticsConfig::all_enabled(),
            DiagnosticCode::UninitializedLocalVariable,
        );
        assert_eq!(format_diags(&source, &diagnostics), branch(position), "{header}");
    };
    judged("    Если Х Тогда\n    КонецЕсли;", "5:10..5:11");
    judged("    Если Б Тогда\n    ИначеЕсли Х Тогда\n    КонецЕсли;", "6:15..6:16");
    judged("    Пока Х Цикл\n    КонецЦикла;", "5:10..5:11");
    judged("    Для Сч = Х По 3 Цикл\n    КонецЦикла;", "5:14..5:15");
    judged("    Для Сч = 1 По Х Цикл\n    КонецЦикла;", "5:19..5:20");
    judged("    Для Каждого Эл Из Х Цикл\n    КонецЦикла;", "5:23..5:24");
}

#[test]
fn for_bound_sees_what_the_other_bound_initializes() {
    let source = "Процедура Обработать(А)
    Если А Тогда
        Х = 1;
    КонецЕсли;
    Для Сч = НеизвестныйМетод(Х) По Х Цикл
    КонецЦикла;
КонецПроцедуры
";
    check(source, expect![[""]]);
}

#[test]
fn for_each_loop_first_iteration_and_not_run() {
    check(
        "Процедура Обработать(А)
    Для Каждого Эл Из А Цикл
        Б = Х;
        Х = 1;
    КонецЦикла;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 3:13..3:14
              message: Переменная 'Х' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
              severity: Warning"#]],
    );
    check(
        "Процедура Обработать(А)
    Для Каждого Эл Из А Цикл
        Х = 1;
    КонецЦикла;
    Б = Х;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 5:9..5:10
              message: Переменная 'Х' может быть не инициализирована: на пути, где цикл не выполняется ни разу, значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn skipped_assignment_on_a_later_iteration_is_not_loop_not_run() {
    // `Продолжить` returns to the header without the assignment, so a zero-iteration
    // path is not the only one without a value.
    check(
        "Процедура Обработать(А, Б)
    Пока А Цикл
        Если Б Тогда
            Продолжить;
        КонецЕсли;
        Х = 1;
    КонецЦикла;
    В = Х;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 8:9..8:10
              message: Переменная 'Х' может быть не инициализирована: на одном из путей к этому чтению значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn names_are_compared_case_insensitively_and_reported_as_read() {
    check(
        "Процедура Обработать(А)
    Если А Тогда
        ПозднееЗначение = 1;
    Иначе
        позднеезначение = 2;
    КонецЕсли;
    Б = ПОЗДНЕЕЗНАЧЕНИЕ;
КонецПроцедуры
",
        expect![[""]],
    );
    check(
        "Процедура Обработать(А)
    Если А Тогда
        позднеезначение = 2;
    КонецЕсли;
    Б = ПОЗДНЕЕЗНАЧЕНИЕ;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 5:9..5:24
              message: Переменная 'ПОЗДНЕЕЗНАЧЕНИЕ' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn argument_inside_an_expression_is_a_read() {
    // Only a bare name passed as the argument is taken for an initialization.
    check(
        &BASE.replace("    Б = Х;", "    Б = НеизвестныйМетод(Х + 1);"),
        expect![[r#"
            UninitializedLocalVariable @ 5:26..5:27
              message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
              severity: Warning"#]],
    );
    check(&BASE.replace("    Б = Х;", "    Б = НеизвестныйМетод(Х);"), expect![[""]]);
}

#[test]
fn method_call_and_constructor_arguments_initialize() {
    let input = BASE.replace("    Б = Х;", "    А.Метод(Х);\n    В = Х;");
    check(&input, expect![[""]]);
    let input = BASE.replace("    Б = Х;", "    Б = Новый Структура(\"К\", Х);\n    В = Х;");
    check(&input, expect![[""]]);
}

#[test]
fn try_without_assignment_in_handler_is_other_path() {
    check(
        "Процедура Обработать()
    Попытка
        Х = Ф();
    Исключение
    КонецПопытки;
    Б = Х;
КонецПроцедуры
",
        expect![[r#"
            UninitializedLocalVariable @ 6:9..6:10
              message: Переменная 'Х' может быть не инициализирована: на одном из путей к этому чтению значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn module_code_is_judged_like_a_method() {
    check(
        "Если Условие Тогда
    Х = 1;
КонецЕсли;
Б = Х;
",
        expect![[r#"
            UninitializedLocalVariable @ 4:5..4:6
              message: Переменная 'Х' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
              severity: Warning"#]],
    );
}

#[test]
fn for_without_variable_suppresses_body() {
    let input = base_with("    Для = 1 По 2 Цикл Х = 1; КонецЦикла;\n");
    check(&input, expect![[""]]);
}

#[test]
fn condition_read_before_late_assignment_stays_with_unresolved_name() {
    let diagnostics = |source: &str| name_diagnostics(source);
    expect![[r#"
        UnresolvedName @ 2:10..2:11
          message: Имя 'Х' не определено
          severity: Major
        UninitializedLocalVariable @ 3:13..3:14
          message: Переменная 'Х' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях
          severity: Warning"#]]
    .assert_eq(&diagnostics(
        "Процедура Обработать()
    Пока Х < 3 Цикл
        Х = Х + 1;
    КонецЦикла;
КонецПроцедуры
",
    ));
    expect![[r#"
        UnresolvedName @ 3:14..3:15
          message: Имя 'Х' не определено
          severity: Major"#]]
    .assert_eq(&diagnostics(
        "Процедура Обработать(А)
    Пока А Цикл
        Если Х = 1 Тогда
            А = Ложь;
        КонецЕсли;
        Х = 1;
    КонецЦикла;
КонецПроцедуры
",
    ));
}

#[test]
fn own_name_and_metadata_collection_are_not_judged() {
    let one_branch = |name: &str| {
        format!(
            "Процедура Обработать(А)
    Если А Тогда
        {name} = 1;
    КонецЕсли;
    Б = {name};
КонецПроцедуры
"
        )
    };
    check(&one_branch("ЭтотОбъект"), expect![[""]]);
    check(&one_branch("Справочники"), expect![[""]]);
}

#[test]
fn header_call_argument_initializes() {
    check(
        "Процедура Обработать(А)
    Если А Тогда
        Х = 1;
    КонецЕсли;
    Если НеизвестныйМетод(Х) Тогда
    КонецЕсли;
    Б = Х;
КонецПроцедуры
",
        expect![[""]],
    );
}

#[test]
fn exported_application_variable_is_not_judged() {
    let judged = |name: &str| {
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module_global(
            "Глобальный",
            "Процедура ЭкспортнаяПроцедура() Экспорт КонецПроцедуры",
        );
        let fixture = builder.build();
        let app_path = fixture.root().join("Ext/ManagedApplicationModule.bsl");
        std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
        std::fs::write(&app_path, "Перем ПеременнаяПриложения Экспорт;\n").unwrap();
        let source = BASE.replace('Х', name);
        let diagnostics = check_with_cfe(&source, fixture)
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UninitializedLocalVariable)
            .collect::<Vec<_>>();
        format_diags(&source, &diagnostics)
    };
    expect![[""]].assert_eq(&judged("ПеременнаяПриложения"));
    expect![[r#"
        UninitializedLocalVariable @ 5:9..5:21
          message: Переменная 'НеЭкспортная' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается
          severity: Warning"#]]
    .assert_eq(&judged("НеЭкспортная"));
}

#[test]
fn literal_loop_header_edges() {
    // `от = до` still enters the body; `True` is the same literal as `Истина`; any
    // other constant condition is not evaluated.
    let for_loop = "Процедура Обработать()
    Для Ном = 3 По 3 Цикл
        Значение = Ном;
    КонецЦикла;
    Б = Значение;
КонецПроцедуры
";
    check(for_loop, expect![[""]]);
    let while_loop = "Процедура Обработать(А)
    Пока Истина Цикл
        Значение = 1;
        Если А Тогда
            Прервать;
        КонецЕсли;
    КонецЦикла;
    Б = Значение;
КонецПроцедуры
";
    check(&while_loop.replace("Истина", "True"), expect![[""]]);
    check(
        &while_loop.replace("Истина", "Ложь"),
        expect![[r#"
            UninitializedLocalVariable @ 8:9..8:17
              message: Переменная 'Значение' может быть не инициализирована: на пути, где цикл не выполняется ни разу, значение ей не присваивается
              severity: Warning"#]],
    );
}
