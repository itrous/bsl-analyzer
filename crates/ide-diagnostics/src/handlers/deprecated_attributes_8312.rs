use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::DeprecatedKind8312;
use hir::LocalRange;
use stdx::case::CaseExt;

use super::deprecated_platform_api;
use super::deprecated_platform_facts::deprecated_8312_replacement;

pub fn from_hir(
    name: &str,
    kind: DeprecatedKind8312,
    owner: Option<&str>,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::DeprecatedPlatformApi;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }
    // An enum-name finding quotes the member, which is often not in the registry
    // (`ОриентацияМетокДиаграммы.Вертикальная`), so the enum before the dot is the
    // name that silences it. For other kinds the object is a variable, not a feature.
    let owner_excluded = kind == DeprecatedKind8312::EnumName
        && owner.is_some_and(|owner| deprecated_platform_api::is_excluded(ctx.config, owner));
    if owner_excluded || deprecated_platform_api::is_excluded(ctx.config, name) {
        return None;
    }

    let (message, replacement) = get_message_and_replacement(name, &kind);

    Some(Diagnostic {
        code,
        message: format!("{} Используйте: {}", message, replacement),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

fn get_message_and_replacement(name: &str, kind: &DeprecatedKind8312) -> (String, String) {
    let lower = name.fold_lower();
    let is_russian = lower.chars().any(|c| c as u32 > 127);

    let replacement = deprecated_8312_replacement(name, *kind);

    let message = match kind {
        DeprecatedKind8312::Attribute => {
            if is_russian {
                format!("Атрибут \"{}\" устарел.", name)
            } else {
                format!("Attribute \"{}\" is deprecated.", name)
            }
        }
        DeprecatedKind8312::Method => {
            if is_russian {
                format!("Метод \"{}\" устарел.", name)
            } else {
                format!("Method \"{}\" is deprecated.", name)
            }
        }
        DeprecatedKind8312::GlobalMethod => {
            if is_russian {
                format!("Глобальный метод \"{}\" устарел.", name)
            } else {
                format!("Global method \"{}\" is deprecated.", name)
            }
        }
        DeprecatedKind8312::EnumName => {
            if is_russian {
                format!("Имя перечисления \"{}\" устарело.", name)
            } else {
                format!("Enum name \"{}\" is deprecated.", name)
            }
        }
        DeprecatedKind8312::EnumValue => {
            if is_russian {
                format!("Значение перечисления \"{}\" устарело.", name)
            } else {
                format!("Enum value \"{}\" is deprecated.", name)
            }
        }
    };

    (message, replacement.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        check_ast_diagnostic_with_config, check_diagnostics_for_with_config, check_hir_diagnostic,
        format_diags, range_to_line_col,
    };
    use crate::DiagnosticsConfig;
    use base_db::Locale;
    use expect_test::expect;
    use serde_json::json;

    fn format_diags_trim_trailing(source: &str, diags: &[crate::Diagnostic]) -> String {
        format_diags(source, diags).lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn test_chart_plot_area_russian() {
        let code = r#"
Процедура Тест()
    ОбластьПостроенияДиаграммы.ОтображатьШкалу = Ложь;
    ОбластьПостроенияДиаграммы.ОриентацияМеток = Ложь;
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:32..3:47
              message: Атрибут "ОтображатьШкалу" устарел. Используйте: ОтображатьШкалы
              severity: Warning
            DeprecatedPlatformApi @ 4:32..4:47
              message: Атрибут "ОриентацияМеток" устарел. Используйте: ШкалаТочек.ОриентацияПодписей
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_plot_area_english() {
        let code = r#"
Procedure Test()
    ChartPlotArea.ShowScale = True;
    ChartPlotArea.ShowSeriesScaleLabels = True;
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:19..3:28
              message: Attribute "ShowScale" is deprecated. Используйте: ShowScales
              severity: Warning
            DeprecatedPlatformApi @ 4:19..4:40
              message: Attribute "ShowSeriesScaleLabels" is deprecated. Используйте: SeriesScale.ScaleLabelLocation
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_attributes() {
        let code = r#"
Процедура Тест()
    Диаграмма.ОтображатьЛегенду = Истина;
    ДиаграммаГанта.ОтображатьЗаголовок = Истина;
    Диаграмма.ПалитраЦветов = Истина;
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:15..3:32
              message: Атрибут "ОтображатьЛегенду" устарел. Используйте: одно из свойств ОбластьЛегендыДиаграммы, ОбластьЛегендыДиаграммыГанта или ОбластьЛегендыСводнойДиаграммы
              severity: Warning
            DeprecatedPlatformApi @ 4:20..4:39
              message: Атрибут "ОтображатьЗаголовок" устарел. Используйте: одно из свойств ОбластьЗаголовкаДиаграммы, ОбластьЗаголовкаДиаграммыГанта или ОбластьЗаголовкаСводнойДиаграммы
              severity: Warning
            DeprecatedPlatformApi @ 5:15..5:28
              message: Атрибут "ПалитраЦветов" устарел. Используйте: ОписаниеПалитрыЦветов.ПалитраЦветов
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_methods() {
        let code = r#"
Процедура Тест()
    Тест = Диаграмма.ПолучитьПалитру();
    Диаграмма.УстановитьПалитру(Неопределено);
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:22..3:37
              message: Метод "ПолучитьПалитру" устарел. Используйте: ОписаниеПалитрыЦветов.ПолучитьПалитру
              severity: Warning
            DeprecatedPlatformApi @ 4:15..4:32
              message: Метод "УстановитьПалитру" устарел. Используйте: ОписаниеПалитрыЦветов.УстановитьПалитру
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_global_method_russian() {
        let code = r#"
Процедура Тест()
    ОчиститьЖурналРегистрации(Отбор);
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:5..3:30
              message: Глобальный метод "ОчиститьЖурналРегистрации" устарел. Используйте:
              severity: Warning"#]]
        .assert_eq(&format_diags_trim_trailing(code, &diags));
    }

    #[test]
    fn test_global_method_english() {
        let code = r#"
Procedure Test()
    ClearEventLog(Filter);
EndProcedure
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:5..3:18
              message: Global method "ClearEventLog" is deprecated. Используйте:
              severity: Warning"#]]
        .assert_eq(&format_diags_trim_trailing(code, &diags));
    }

    #[test]
    fn test_enum_name() {
        let code = r#"
Процедура Тест()
    Ориентация = ОриентацияМетокДиаграммы.Авто;
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:43..3:47
              message: Имя перечисления "Авто" устарело. Используйте:
              severity: Warning"#]]
        .assert_eq(&format_diags_trim_trailing(code, &diags));
    }

    #[test]
    fn test_child_form_items_group() {
        let code = r#"
Процедура Тест()
    Группировка = ГруппировкаПодчиненныхЭлементовФормы.Горизонтальная;
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 3:56..3:70
              message: Значение перечисления "Горизонтальная" устарело. Используйте: ГоризонтальнаяВсегда
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_plot_area_all_russian_attributes() {
        let code = r#"Процедура Тест()
    тест = ОбластьПостроенияДиаграммы.ОтображатьШкалу;
    ОбластьПостроенияДиаграммы.ЛинииШкалы = Ложь;
    ОбластьПостроенияДиаграммы.ЦветШкалы = Ложь;
    ОбластьПостроенияДиаграммы.ОтображатьПодписиШкалыСерий = Ложь;
    ОбластьПостроенияДиаграммы.ОтображатьПодписиШкалыТочек = Ложь;
    ОбластьПостроенияДиаграммы.ОтображатьПодписиШкалыЗначений = Ложь;
    ОбластьПостроенияДиаграммы.ОтображатьЛинииЗначенийШкалы = Ложь;
    ОбластьПостроенияДиаграммы.ФорматШкалыЗначений = Ложь;
    ОбластьПостроенияДиаграммы.ОриентацияМеток = Ложь;
КонецПроцедуры"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:39..2:54
              message: Атрибут "ОтображатьШкалу" устарел. Используйте: ОтображатьШкалы
              severity: Warning
            DeprecatedPlatformApi @ 3:32..3:42
              message: Атрибут "ЛинииШкалы" устарел. Используйте: ЛинииШкал
              severity: Warning
            DeprecatedPlatformApi @ 4:32..4:41
              message: Атрибут "ЦветШкалы" устарел. Используйте: ЦветШкал
              severity: Warning
            DeprecatedPlatformApi @ 5:32..5:59
              message: Атрибут "ОтображатьПодписиШкалыСерий" устарел. Используйте: ШкалаСерий.ПоложениеПодписейШкалы
              severity: Warning
            DeprecatedPlatformApi @ 6:32..6:59
              message: Атрибут "ОтображатьПодписиШкалыТочек" устарел. Используйте: ШкалаТочек.ПоложениеПодписейШкалы
              severity: Warning
            DeprecatedPlatformApi @ 7:32..7:62
              message: Атрибут "ОтображатьПодписиШкалыЗначений" устарел. Используйте: ШкалаЗначений.ПоложениеПодписейШкалы
              severity: Warning
            DeprecatedPlatformApi @ 8:32..8:60
              message: Атрибут "ОтображатьЛинииЗначенийШкалы" устарел. Используйте: ШкалаЗначений.ОтображениеЛинийСетки
              severity: Warning
            DeprecatedPlatformApi @ 9:32..9:51
              message: Атрибут "ФорматШкалыЗначений" устарел. Используйте: ШкалаЗначений.ФорматПодписей
              severity: Warning
            DeprecatedPlatformApi @ 10:32..10:47
              message: Атрибут "ОриентацияМеток" устарел. Используйте: ШкалаТочек.ОриентацияПодписей
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_plot_area_all_english_attributes() {
        let code = r#"Procedure Test()
    ChartPlotArea.ShowScale = True;
    ChartPlotArea.ShowSeriesScaleLabels = True;
    ChartPlotArea.ShowPointsScaleLabels = True;
    ChartPlotArea.ShowValuesScaleLabels = True;
    ChartPlotArea.ShowScaleValueLines = True;
    ChartPlotArea.ValueScaleFormat = True;
    ChartPlotArea.LabelsOrientation = True;
EndProcedure"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:19..2:28
              message: Attribute "ShowScale" is deprecated. Используйте: ShowScales
              severity: Warning
            DeprecatedPlatformApi @ 3:19..3:40
              message: Attribute "ShowSeriesScaleLabels" is deprecated. Используйте: SeriesScale.ScaleLabelLocation
              severity: Warning
            DeprecatedPlatformApi @ 4:19..4:40
              message: Attribute "ShowPointsScaleLabels" is deprecated. Используйте: PointsScale.ScaleLabelLocation
              severity: Warning
            DeprecatedPlatformApi @ 5:19..5:40
              message: Attribute "ShowValuesScaleLabels" is deprecated. Используйте: ValuesScale.ScaleLabelLocation
              severity: Warning
            DeprecatedPlatformApi @ 6:19..6:38
              message: Attribute "ShowScaleValueLines" is deprecated. Используйте: ValuesScale.GridLinesShowMode
              severity: Warning
            DeprecatedPlatformApi @ 7:19..7:35
              message: Attribute "ValueScaleFormat" is deprecated. Используйте: ValuesScale.LabelFormat
              severity: Warning
            DeprecatedPlatformApi @ 8:19..8:36
              message: Attribute "LabelsOrientation" is deprecated. Используйте: PointsScale.LabelOrientation
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_legend_title_russian() {
        let code = r#"Процедура Тест2()
    Диаграмма.ОтображатьЛегенду = Истина;
    Диаграмма.ОтображатьЗаголовок = Истина;
    ДиаграммаГанта.ОтображатьЛегенду = Истина;
    ДиаграммаГанта.ОтображатьЗаголовок = Истина;
    СводнаяДиаграмма.ОтображатьЛегенду = Истина;
    СводнаяДиаграмма.ОтображатьЗаголовок = Истина;
КонецПроцедуры"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:15..2:32
              message: Атрибут "ОтображатьЛегенду" устарел. Используйте: одно из свойств ОбластьЛегендыДиаграммы, ОбластьЛегендыДиаграммыГанта или ОбластьЛегендыСводнойДиаграммы
              severity: Warning
            DeprecatedPlatformApi @ 3:15..3:34
              message: Атрибут "ОтображатьЗаголовок" устарел. Используйте: одно из свойств ОбластьЗаголовкаДиаграммы, ОбластьЗаголовкаДиаграммыГанта или ОбластьЗаголовкаСводнойДиаграммы
              severity: Warning
            DeprecatedPlatformApi @ 4:20..4:37
              message: Атрибут "ОтображатьЛегенду" устарел. Используйте: одно из свойств ОбластьЛегендыДиаграммы, ОбластьЛегендыДиаграммыГанта или ОбластьЛегендыСводнойДиаграммы
              severity: Warning
            DeprecatedPlatformApi @ 5:20..5:39
              message: Атрибут "ОтображатьЗаголовок" устарел. Используйте: одно из свойств ОбластьЗаголовкаДиаграммы, ОбластьЗаголовкаДиаграммыГанта или ОбластьЗаголовкаСводнойДиаграммы
              severity: Warning
            DeprecatedPlatformApi @ 6:22..6:39
              message: Атрибут "ОтображатьЛегенду" устарел. Используйте: одно из свойств ОбластьЛегендыДиаграммы, ОбластьЛегендыДиаграммыГанта или ОбластьЛегендыСводнойДиаграммы
              severity: Warning
            DeprecatedPlatformApi @ 7:22..7:41
              message: Атрибут "ОтображатьЗаголовок" устарел. Используйте: одно из свойств ОбластьЗаголовкаДиаграммы, ОбластьЗаголовкаДиаграммыГанта или ОбластьЗаголовкаСводнойДиаграммы
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_palette_russian() {
        let code = r#"Процедура Тест2()
    Диаграмма.ПалитраЦветов = Истина;
    Диаграмма.ЦветНачалаГрадиентнойПалитры = Истина;
    Диаграмма.ЦветКонцаГрадиентнойПалитры = Истина;
    Диаграмма.МаксимальноеКоличествоЦветовГрадиентнойПалитры = Истина;
КонецПроцедуры"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:15..2:28
              message: Атрибут "ПалитраЦветов" устарел. Используйте: ОписаниеПалитрыЦветов.ПалитраЦветов
              severity: Warning
            DeprecatedPlatformApi @ 3:15..3:43
              message: Атрибут "ЦветНачалаГрадиентнойПалитры" устарел. Используйте: ОписаниеПалитрыЦветов.ЦветНачалаГрадиентнойПалитры
              severity: Warning
            DeprecatedPlatformApi @ 4:15..4:42
              message: Атрибут "ЦветКонцаГрадиентнойПалитры" устарел. Используйте: ОписаниеПалитрыЦветов.ЦветКонцаГрадиентнойПалитры
              severity: Warning
            DeprecatedPlatformApi @ 5:15..5:61
              message: Атрибут "МаксимальноеКоличествоЦветовГрадиентнойПалитры" устарел. Используйте: ОписаниеПалитрыЦветов.МаксимальноеКоличествоЦветовГрадиентнойПалитры
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_legend_title_english() {
        let code = r#"Procedure Test2()
    Chart.ShowLegend = True;
    GanttChart.ShowLegend = True;
    PivotChart.ShowLegend = True;
    Chart.ShowTitle = True;
    GanttChart.ShowTitle = True;
    PivotChart.ShowTitle = True;
EndProcedure"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:11..2:21
              message: Attribute "ShowLegend" is deprecated. Используйте: one of the properties of ChartLegendArea, GanttChartLegendArea or PivotChartLegendArea
              severity: Warning
            DeprecatedPlatformApi @ 3:16..3:26
              message: Attribute "ShowLegend" is deprecated. Используйте: one of the properties of ChartLegendArea, GanttChartLegendArea or PivotChartLegendArea
              severity: Warning
            DeprecatedPlatformApi @ 4:16..4:26
              message: Attribute "ShowLegend" is deprecated. Используйте: one of the properties of ChartLegendArea, GanttChartLegendArea or PivotChartLegendArea
              severity: Warning
            DeprecatedPlatformApi @ 5:11..5:20
              message: Attribute "ShowTitle" is deprecated. Используйте: one of the properties of ChartTitleArea, GanttChartTitleArea or PivotChartTitleArea
              severity: Warning
            DeprecatedPlatformApi @ 6:16..6:25
              message: Attribute "ShowTitle" is deprecated. Используйте: one of the properties of ChartTitleArea, GanttChartTitleArea or PivotChartTitleArea
              severity: Warning
            DeprecatedPlatformApi @ 7:16..7:25
              message: Attribute "ShowTitle" is deprecated. Используйте: one of the properties of ChartTitleArea, GanttChartTitleArea or PivotChartTitleArea
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_palette_english() {
        let code = r#"Procedure Test2()
    Chart.ColorPalette = True;
    Chart.GradientPaletteStartColor = True;
    Chart.GradientPaletteEndColor = True;
    Chart.GradientPaletteMaxColors = True;
EndProcedure"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:11..2:23
              message: Attribute "ColorPalette" is deprecated. Используйте: ColorPaletteDescription.ColorPalette
              severity: Warning
            DeprecatedPlatformApi @ 3:11..3:36
              message: Attribute "GradientPaletteStartColor" is deprecated. Используйте: ColorPaletteDescription.GradientPaletteStartColor
              severity: Warning
            DeprecatedPlatformApi @ 4:11..4:34
              message: Attribute "GradientPaletteEndColor" is deprecated. Используйте: ColorPaletteDescription.GradientPaletteEndColor
              severity: Warning
            DeprecatedPlatformApi @ 5:11..5:35
              message: Attribute "GradientPaletteMaxColors" is deprecated. Используйте: ColorPaletteDescription.GradientPaletteMaxColors
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_chart_palette_methods_english() {
        let code = r#"Procedure Test2()
    Chart.GetPalette();
    Chart.SetPalette(True);
EndProcedure"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:11..2:21
              message: Method "GetPalette" is deprecated. Используйте: ColorPaletteDescription.GetPalette
              severity: Warning
            DeprecatedPlatformApi @ 3:11..3:21
              message: Method "SetPalette" is deprecated. Используйте: ColorPaletteDescription.SetPalette
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_enum_name_russian() {
        let code = r#"Процедура Тест3()
    Ориентация = ОриентацияМетокДиаграммы.Авто;
КонецПроцедуры"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:43..2:47
              message: Имя перечисления "Авто" устарело. Используйте:
              severity: Warning"#]]
        .assert_eq(&format_diags_trim_trailing(code, &diags));
    }

    #[test]
    fn test_enum_value_russian() {
        let code = r#"Процедура Тест5()
    Группировка = ГруппировкаПодчиненныхЭлементовФормы.Горизонтальная;
КонецПроцедуры"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:56..2:70
              message: Значение перечисления "Горизонтальная" устарело. Используйте: ГоризонтальнаяВсегда
              severity: Warning"#]].assert_eq(&format_diags(code, &diags));
    }

    #[test]
    fn test_enum_value_english() {
        let code = r#"Procedure Test5()
    test = ChildFormItemsGroup.Horizontal;
EndProcedure"#;
        let diagnostics = check_hir_diagnostic(code);
        let diags: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::DeprecatedPlatformApi)
            .collect();
        expect![[r#"
            DeprecatedPlatformApi @ 2:32..2:42
              message: Enum value "Horizontal" is deprecated. Используйте: AlwaysHorizontal
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diags));
    }

    /// `Вертикальная` is not a feature of the registry, so only the enum in front
    /// of the dot can silence it; `Авто` is one, and `Горизонтальная` is a value,
    /// not an enum name.
    const ENUM_FIXTURE: &str = r#"Процедура Тест()
    О1 = ОриентацияМетокДиаграммы.Вертикальная;
    О2 = ОриентацияМетокДиаграммы.Авто;
    Г1 = ГруппировкаПодчиненныхЭлементовФормы.Горизонтальная;
    ОриентацияМетокДиаграммы.Вертикальная();
КонецПроцедуры"#;

    fn enum_fixture_findings(names: &[&str]) -> Vec<crate::Diagnostic> {
        let config = DiagnosticsConfig::from_project_json(
            &json!({ "parameters": { "DeprecatedPlatformApi": { "exclude": names } } }),
            Locale::Ru,
        );
        check_diagnostics_for_with_config(
            ENUM_FIXTURE,
            config,
            DiagnosticCode::DeprecatedPlatformApi,
        )
    }

    fn enum_fixture_lines(names: &[&str]) -> Vec<u32> {
        enum_fixture_findings(names)
            .iter()
            .map(|d| range_to_line_col(ENUM_FIXTURE, d.range).0 + 1)
            .collect()
    }

    #[test]
    fn exclude_matches_enum_name_findings_by_enum() {
        // The control: five findings, one on each of the three reads and two on
        // the call (the member of a call is reported by two lowering paths).
        let baseline = enum_fixture_findings(&[]);
        assert_eq!(enum_fixture_lines(&[]), [2, 3, 4, 5, 5]);
        let texts: Vec<_> = baseline.iter().map(|d| d.message.trim_end().to_owned()).collect();
        assert_eq!(
            texts,
            [
                r#"Имя перечисления "Вертикальная" устарело. Используйте:"#,
                r#"Имя перечисления "Авто" устарело. Используйте:"#,
                r#"Значение перечисления "Горизонтальная" устарело. Используйте: ГоризонтальнаяВсегда"#,
                r#"Имя перечисления "Вертикальная" устарело. Используйте:"#,
                r#"Имя перечисления "Вертикальная" устарело. Используйте:"#,
            ]
        );

        // The enum silences its name findings, the call included; the value
        // finding of the other enum stays untouched.
        assert_eq!(enum_fixture_lines(&["ОриентацияМетокДиаграммы"]), [4]);
        assert_eq!(enum_fixture_lines(&["ориентацияметокдиаграммы"]), [4], "case is ignored");

        // A member that is a feature of the registry silences itself and no
        // neighbour.
        assert_eq!(enum_fixture_lines(&["Авто"]), [2, 4, 5, 5]);

        // A member that is no feature silences nothing: the name of the enum is
        // the only way to silence it.
        assert_eq!(enum_fixture_lines(&["Вертикальная"]), [2, 3, 4, 5, 5]);

        // The object in front of the dot is not compared for a value finding.
        assert_eq!(enum_fixture_lines(&["ГруппировкаПодчиненныхЭлементовФормы"]), [2, 3, 4, 5, 5]);
    }

    #[test]
    fn exclude_compares_the_object_only_for_enum_name_findings() {
        // The only object that is both an owner and a feature name is the enum of
        // the registry, and it is only ever reported as an enum name; so the other
        // kinds cannot be built from source and the handler is called directly.
        let flagged = |kind: DeprecatedKind8312, names: &[&str]| {
            let config = DiagnosticsConfig::from_project_json(
                &json!({ "parameters": { "DeprecatedPlatformApi": { "exclude": names } } }),
                Locale::Ru,
            );
            let emitted = std::cell::Cell::new(false);
            check_ast_diagnostic_with_config("", config, |ctx| {
                let range = LocalRange::of_detached_node(ide_db::TextRange::empty(0.into()));
                let owner = Some("ОриентацияМетокДиаграммы");
                emitted.set(from_hir("ОтображатьШкалу", kind, owner, range, ctx).is_some());
                Vec::new()
            });
            emitted.get()
        };

        assert!(flagged(DeprecatedKind8312::EnumName, &[]), "the control");
        assert!(flagged(DeprecatedKind8312::Attribute, &[]), "the control");
        assert!(!flagged(DeprecatedKind8312::EnumName, &["ОриентацияМетокДиаграммы"]));
        assert!(flagged(DeprecatedKind8312::Attribute, &["ОриентацияМетокДиаграммы"]));
        assert!(flagged(DeprecatedKind8312::EnumValue, &["ОриентацияМетокДиаграммы"]));
        assert!(flagged(DeprecatedKind8312::Method, &["ОриентацияМетокДиаграммы"]));
    }
}
