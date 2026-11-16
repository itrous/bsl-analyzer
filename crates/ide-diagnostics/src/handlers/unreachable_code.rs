use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::cfg::CfgVertex;
use hir::BodySourceMap;
use hir::LocalRange;
use ide_db::TextRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Minor,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Design, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::UnreachableCode;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }

    let cfg = ctx.cfg();
    let Some(entry) = cfg.start() else {
        return;
    };
    let exit = cfg.exit();

    let dead_tail_vertices = compute_dead_tail_vertices(&cfg, entry);

    let unreachable_ranges =
        collect_unreachable_ranges(&cfg, ctx.source_map(), entry, exit, |idx| {
            dead_tail_vertices.contains(&idx)
        });

    create_diagnostics(acc, unreachable_ranges, &ctx.root().text().to_string(), code, ctx);
}

fn collect_unreachable_ranges<F>(
    cfg: &hir::cfg::ControlFlowGraph,
    source_map: &BodySourceMap,
    entry: hir::cfg::NodeIndex,
    exit: hir::cfg::NodeIndex,
    is_unreachable: F,
) -> Vec<LocalRange>
where
    F: Fn(hir::cfg::NodeIndex) -> bool,
{
    let mut ranges = Vec::new();

    for (vertex_idx, vertex) in cfg.vertices() {
        if vertex_idx == entry || vertex_idx == exit {
            continue;
        }

        if !is_unreachable(vertex_idx) {
            continue;
        }

        if let CfgVertex::BasicBlock(block) = vertex {
            for &stmt_id in block.statements() {
                if let Some(range) = source_map.stmt_range(stmt_id) {
                    ranges.push(range);
                }
            }
        } else if let Some(range) = get_vertex_range(cfg, vertex_idx, vertex, source_map) {
            ranges.push(range);
        }
    }

    ranges
}

fn create_diagnostics(
    diagnostics: &mut Vec<Diagnostic<LocalRange>>,
    ranges: Vec<LocalRange>,
    source_text: &str,
    code: DiagnosticCode,
    ctx: &BodyContext,
) {
    let merged = merge_ranges(ranges, source_text);
    for range in merged {
        diagnostics.push(Diagnostic {
            code,
            message: message_ru(),
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

fn merge_ranges(ranges: Vec<LocalRange>, source_text: &str) -> Vec<LocalRange> {
    let mut ranges: Vec<TextRange> = ranges.into_iter().map(LocalRange::in_root).collect();
    if ranges.is_empty() {
        return Vec::new();
    }

    ranges.sort_by_key(|r| r.start());

    let mut merged: Vec<TextRange> = Vec::new();
    let mut current = ranges[0];

    for range in ranges.into_iter().skip(1) {
        let gap_start = usize::from(current.end());
        let gap_end = usize::from(range.start());

        let should_merge = if gap_end > gap_start && gap_end <= source_text.len() {
            let gap_text = &source_text[gap_start..gap_end];
            let newline_count = gap_text.chars().filter(|&c| c == '\n').count();
            newline_count <= 1
        } else {
            true
        };

        if should_merge {
            current = TextRange::new(current.start(), current.end().max(range.end()));
        } else {
            merged.push(current);
            current = range;
        }
    }
    merged.push(current);

    merged.into_iter().map(LocalRange::of_detached_node).collect()
}

fn compute_dead_tail_vertices(
    cfg: &hir::cfg::ControlFlowGraph,
    entry: hir::cfg::NodeIndex,
) -> std::collections::HashSet<hir::cfg::NodeIndex> {
    let reachable_with_all_edges = compute_reachable_vertices(cfg, entry, |_| true);
    let reachable_without_dead_edges =
        compute_reachable_vertices(cfg, entry, |edge_type| edge_type.is_executable());
    let exit = cfg.exit();

    reachable_with_all_edges
        .difference(&reachable_without_dead_edges)
        .copied()
        .filter(|&vertex| vertex != entry && vertex != exit)
        .collect()
}

fn compute_reachable_vertices<F>(
    cfg: &hir::cfg::ControlFlowGraph,
    entry: hir::cfg::NodeIndex,
    can_traverse: F,
) -> std::collections::HashSet<hir::cfg::NodeIndex>
where
    F: Fn(&hir::cfg::CfgEdgeType) -> bool,
{
    let mut reachable = std::collections::HashSet::new();
    let mut worklist = vec![entry];

    while let Some(node) = worklist.pop() {
        if !reachable.insert(node) {
            continue;
        }

        for (target, edge_type) in cfg.outgoing_edges(node) {
            if can_traverse(edge_type) && !reachable.contains(&target) {
                worklist.push(target);
            }
        }
    }

    reachable
}

fn get_vertex_range(
    cfg: &hir::cfg::ControlFlowGraph,
    vertex_idx: hir::cfg::NodeIndex,
    vertex: &CfgVertex,
    source_map: &BodySourceMap,
) -> Option<LocalRange> {
    if let Some(range) =
        cfg.source_stmt_id(vertex_idx).and_then(|stmt_id| source_map.stmt_range(stmt_id))
    {
        return Some(range);
    }

    match vertex {
        CfgVertex::BasicBlock(block) => {
            let statements = block.statements();
            if statements.is_empty() {
                return None;
            }

            let first = statements.first()?;
            let last = statements.last()?;

            let first_range = source_map.stmt_range(*first)?;
            let last_range = source_map.stmt_range(*last)?;

            Some(LocalRange::of_detached_node(TextRange::new(
                first_range.in_root().start(),
                last_range.in_root().end(),
            )))
        }
        CfgVertex::Conditional(_) | CfgVertex::LoopEntry(_) => None,
        CfgVertex::WhileHeader(loop_vertex) => source_map.expr_range(loop_vertex.condition),
        CfgVertex::ForHeader(loop_vertex) => source_map.binding_range(loop_vertex.loop_var),
        CfgVertex::ForEachHeader(loop_vertex) => source_map.binding_range(loop_vertex.loop_var),
        CfgVertex::Try => None,
        CfgVertex::PreprocCondition(preproc) => {
            Some(preproc.full_range.or(preproc.directive_range).unwrap_or(preproc.condition_range))
        }
        CfgVertex::Exit => None,
    }
}

fn message_ru() -> String {
    "Недостижимый код".to_string()
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_unreachable_after_return() {
        let code = r#"
Процедура Тест()
    Возврат;
    Сообщить("Недостижимо");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..4:28
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_after_raise() {
        let code = r#"
Процедура Тест()
    ВызватьИсключение "Ошибка";
    Сообщить("Недостижимо");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..4:28
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_after_break() {
        let code = r#"
Процедура Тест()
    Для Каждого Элемент Из Коллекция Цикл
        Прервать;
        Сообщить("Недостижимо");
    КонецЦикла;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 5:9..5:32
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_after_continue() {
        let code = r#"
Процедура Тест()
    Для Каждого Элемент Из Коллекция Цикл
        Продолжить;
        Сообщить("Недостижимо");
    КонецЦикла;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 5:9..5:32
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_multiline_block() {
        let code = r#"
Процедура Тест()
    Возврат;
    А = 1;
    Б = 2;
    Сообщить(А + Б);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..6:20
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_no_unreachable_in_different_branches() {
        let code = r#"
Процедура Тест()
    Если Условие Тогда
        Возврат;
    КонецЕсли;
    Сообщить("Достижимо");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::UnreachableCode, expect![[r#""#]]);
    }

    #[test]
    fn test_no_unreachable_after_conditional_return() {
        let code = r#"
Функция Тест()
    Если А Тогда
        Возврат 1;
    Иначе
        Возврат 2;
    КонецЕсли;
КонецФункции
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::UnreachableCode, expect![[r#""#]]);
    }

    #[test]
    fn test_unreachable_after_region_with_return() {
        let code = r#"
Функция Тест()
    #Область Тест
    Возврат;
    #КонецОбласти
    Сообщить("Недостижимо");
КонецФункции
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 6:5..6:28
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_after_region_with_return_and_if() {
        let code = r#"
Функция Пример11()
    #Область ВложеннаяОбласть
    Если Истина Тогда
        Возврат;
    КонецЕсли;
    Возврат;
    #КонецОбласти
    Сообщить(5);
КонецФункции
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 9:5..9:16
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_in_outer_region() {
        let code = r#"
#Область ВнешняяОбласть
Функция Пример11()
    #Область ВложеннаяОбласть
    Если Истина Тогда
        Возврат;
    КонецЕсли;
    Возврат;
    #КонецОбласти
    Сообщить(5);
КонецФункции
#КонецОбласти
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 10:5..10:16
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_java_fixture() {
        let code = r#"Процедура Пример1()
    Для каждого Строка Из Строки Цикл
        Если Условие Тогда
            Продолжить;;; // <-- Ошибка нет
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры

Процедура Пример1()
    Для каждого Строка Из Строки Цикл
        Если Условие Тогда
            Продолжить;
            Метод();    // <-- Ошибка: после продолжить
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры

Процедура Пример2()
    Для каждого Строка Из Строки Цикл
        Если Условие Тогда
            Возврат;
            Метод();    // <-- Ошибка: после Возврат
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры

Процедура Пример3()
    Для каждого Строка Из Строки Цикл
        Если Условие2 Тогда
            Прервать;
            Метод();    // <-- Ошибка: после Прервать
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры

Процедура Пример4()
    Возврат;
    Для каждого Строка Из Строки Цикл   // <-- Ошибка: после Возврат
        Если Условие2 Тогда
            Метод();
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры

Функция Пример5()
    Возврат 1;
    Возврат 2; // <-- Ошибка: после Возврат
    Для каждого Строка Из Строки Цикл  // <-- Ошибка нет: второй Возврат не ловим
        Если Условие2 Тогда
            Метод();
        КонецЕсли;
    КонецЦикла;
КонецФункции

Функция Пример6()
    Для каждого Строка Из Строки Цикл
        Если Условие2 Тогда
            ВызватьИсключение "Ошибка";
            Метод();    // <-- Ошибка: После ВызватьИсключение
        КонецЕсли;
    КонецЦикла;
КонецФункции

Функция Пример7()
    Для каждого Строка Из Строки Цикл
        Если Условие2 Тогда
            ВызватьИсключение "Ошибка";
            Метод();    // <-- Ошибка: После ВызватьИсключение
            Прервать;   // не анализируем
            Метод2();   // ошибки нет, относится к блоку выше
        КонецЕсли;
    КонецЦикла;
КонецФункции

Функция Пример8()
    #Если Сервер Тогда
        Возврат;
    #Иначе
        // ошибки здесь нет
        Для каждого Строка Из Строки Цикл
            Если Условие2 Тогда
                ВызватьИсключение "Ошибка";
                Метод();    // <-- Ошибка: После ВызватьИсключение
                Прервать;   // не анализируем
                Метод2();   // ошибки нет, относится к блоку выше
            КонецЕсли;
        КонецЦикла;
     #КонецЕсли
КонецФункции

Функция Пример9()
    #Если Сервер Тогда
        Возврат;
        Метод(); // <-- Ошибка: После Возврат
    #ИначеЕсли Не Сервер Тогда
        Метод();
        #Если Сервер Тогда
            Метод3();
            Возврат;
        #КонецЕсли
        Метод4(); // ошибки нет
        Возврат;
        Метод5(); // <-- Ошибка: После возврат
    #Иначе
        // ошибки здесь нет
        Для каждого Строка Из Строки Цикл
            Если Условие2 Тогда
                ВызватьИсключение "Ошибка";
                #Если Клиент Тогда // <-- Ошибка: После ВызватьИсключение
                    Метод();    // не анализируем
                    Прервать;   // не анализируем
                    Метод2();   // ошибки нет, относится к блоку выше
                #КонецЕсли
            КонецЕсли;
        КонецЦикла;
     #КонецЕсли
КонецФункции

Функция Пример10()
    #Если Сервер Тогда
        Возврат;
    #Иначе
        ВызватьИсключение "";
    #КонецЕсли

    Метод2();   // <-- Ошибка: ренее были Возврат и Вызватьисключение, ка не ловим

КонецФункции

#Область ВнешняяОбласть
Функция Пример11()
    #Область ВложеннаяОбласть
    Если Истина Тогда
        Возврат;
    КонецЕсли;
    Возврат;
    // ошибки быть не должно
    #КонецОбласти
    Сообщить(5); // <-- Ошибка: ранее был Возврат
КонецФункции
#КонецОбласти

#Область ВнешняяОбласть
Функция Пример12()
    #Область ВложеннаяОбласть
        #Область ЕщеОднаВложеннаяОбласть
            Если Истина Тогда
                ВызватьИсключение "";
            КонецЕсли;
            Возврат;
        // ошибки ниже быть не должно
        #КонецОбласти
    #КонецОбласти
КонецФункции
#КонецОбласти

Функция ДосрочныйВыход()
    Если А Тогда
        Возврат 1;
    Иначе
        Возврат 2;
    КонецЕсли;

    ТутОшибка = Истина; // <- недостижимый код
КонецФункции

#Если Сервер Тогда
   Возврат;
#Иначе
    Метод();
    Возврат;
    Метод2();   // <-- Ошибка: После Возврат
#КонецЕсли

Если Условие2 Тогда
    ВызватьИсключение "Ошибка";
    Метод();    // <-- Ошибка: После ВызватьИсключение
    Возврат;   // не анализируем
    Метод2();   // ошибки нет, относится к блоку выше
КонецЕсли;

Возврат;
Метод2();   // Ошибка: После Возврат
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 13:13..13:20
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 22:13..22:20
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 31:13..31:20
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 38:5..42:16
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 47:5..52:16
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 59:13..59:20
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 68:13..70:21
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 83:17..85:25
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 94:9..94:16
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 103:9..103:17
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 109:17..113:27
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 126:5..126:13
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 139:5..139:16
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 164:5..164:23
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 172:5..172:13
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 175:1..180:11
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 182:1..183:9
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_if_elsif_with_raise_in_else_only() {
        let code = "Процедура Тест(Важность, ВариантВажности)\n\tЕсли Важность = \"Обычная\" Тогда\n\t\tВариантВажности = 1;\n\tИначеЕсли Важность = \"Высокая\" Тогда\n\t\tВариантВажности = 2;\n\tИначеЕсли Важность = \"Низкая\" Тогда\n\t\tВариантВажности = 3;\n\tИначе\n\t\tВызватьИсключение(\"Ошибка\");\n\tКонецЕсли;\nКонецПроцедуры\n";
        check_diagnostics_snapshot_for(code, DiagnosticCode::UnreachableCode, expect![[r#""#]]);
    }

    #[test]
    fn test_raise_with_two_arguments_in_if() {
        let code = "Функция Тест()\n\tДля Каждого Элемент Из Коллекция Цикл\n\t\tЕсли Условие Тогда\n\t\t\tТекст = СтрШаблон(\"Ошибка: %1\", Элемент);\n\t\t\tВызватьИсключение(Текст, КатегорияОшибки.ОшибкаХранимыхДанных);\n\t\tКонецЕсли;\n\t\tРезультат = Элемент + 1;\n\tКонецЦикла;\n\tВозврат Результат;\nКонецФункции\n";

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnreachableCode, expect![[r#""#]]);
    }

    #[test]
    fn test_unreachable_after_all_branches_return() {
        let code = r#"
Функция Тест()
    Если А Тогда
        Возврат 1;
    Иначе
        Возврат 2;
    КонецЕсли;

    ТутОшибка = Истина;
КонецФункции
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 9:5..9:23
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_foreach_after_return() {
        let code = r#"
Процедура Пример4()
    Возврат;
    Для каждого Строка Из Строки Цикл
        Если Условие2 Тогда
            Метод();
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..8:16
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_after_goto() {
        let code = r#"
Процедура Тест()
    Перейти ~Конец;
    Сообщить("Недостижимо");
    ~Конец:
    Сообщить("Достижимо");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..4:28
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn test_unreachable_in_preproc_else_module_level() {
        let code = r#"
#Если Сервер Тогда
   Возврат;
#Иначе
    Метод();
    Возврат;
    Метод2();   // unreachable
#КонецЕсли
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 7:5..7:13
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn unreachable_if_after_return_starts_at_if_header() {
        let code = r#"
Процедура Тест()
    Возврат;
    Если Условие Тогда
        Метод();
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..6:15
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn unreachable_try_after_return_starts_at_try_header() {
        let code = r#"
Процедура Тест()
    Возврат;
    Попытка
        Метод();
    Исключение
        Метод2();
    КонецПопытки;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..8:18
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn unreachable_while_after_return_covers_whole_loop() {
        let code = r#"
Процедура Тест()
    Возврат;
    Пока Условие Цикл
        Метод();
    КонецЦикла;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..6:16
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn unreachable_label_after_return_is_in_diagnostic_range() {
        let code = r#"
Процедура Тест()
    Возврат;
    ~Метка:
    Сообщить("Недостижимо");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 4:5..5:28
              message: Недостижимый код
              severity: Error"#]],
        );
    }

    #[test]
    fn reachable_goto_target_after_return_has_no_diagnostic() {
        let code = r#"
Процедура Тест()
    Если Условие Тогда
        Перейти ~Продолжение;
    КонецЕсли;
    Возврат;
    ~Продолжение:
    Сообщить("Достижимо");
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(code, DiagnosticCode::UnreachableCode, expect![[r#""#]]);
    }

    #[test]
    fn unreachable_if_ranges_match_in_module_and_method_code() {
        let code = r#"
Возврат;
Если Условие Тогда
    Метод();
КонецЕсли;

Процедура Тест()
    Возврат;
    Если Условие Тогда
        Метод();
    КонецЕсли;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnreachableCode,
            expect![[r#"
            UnreachableCode @ 3:1..5:11
              message: Недостижимый код
              severity: Error
            UnreachableCode @ 9:5..11:15
              message: Недостижимый код
              severity: Error"#]],
        );
    }
}
