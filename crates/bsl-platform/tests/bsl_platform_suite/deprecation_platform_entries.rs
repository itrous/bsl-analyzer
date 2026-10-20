use bsl_platform::deprecation::{
    registry, CompatibilityBucket, DeprecationEntry, DisplayKind, ElementKind, LifecycleGroup,
    Lookup, OwnerType, Replacement,
};
use std::collections::HashSet;

fn lookup(query: Lookup<'_>) -> &'static DeprecationEntry {
    registry().lookup(query).expect("platform deprecation fact must exist")
}

fn replacement(ru: &'static str, en: &'static str) -> Replacement {
    Replacement { ru, en }
}

#[test]
fn deprecation_registry_has_no_duplicate_lookup_keys_for_platform_entries() {
    let mut seen: HashSet<(ElementKind, Option<String>, String)> = HashSet::new();

    for entry in registry().entries() {
        let names = [entry.ru.to_lowercase(), entry.en.to_lowercase()];
        let owners = match entry.owner {
            Some(owner) => vec![Some(owner.ru.to_lowercase()), Some(owner.en.to_lowercase())],
            None => vec![None],
        };

        for name in names.into_iter().filter(|name| !name.is_empty()) {
            for owner in owners.iter().filter(|owner| owner.as_deref() != Some("")) {
                assert!(
                    seen.insert((entry.element_kind, owner.clone(), name.clone())),
                    "duplicate platform deprecation lookup key for {entry:?}",
                );
            }
        }
    }
}

#[test]
fn deprecation_registry_covers_current_global_platform_diagnostics() {
    let current_date = lookup(Lookup::global_method("ТекущаяДата"));
    let current_date_en = lookup(Lookup::global_method("CurrentDate"));
    assert!(std::ptr::eq(current_date, current_date_en));
    assert_eq!(current_date.group, LifecycleGroup::DateTime);
    assert_eq!(
        current_date.replacement,
        Some(replacement("ТекущаяДатаСеанса", "CurrentSessionDate"))
    );
    assert_eq!(current_date.compatibility, CompatibilityBucket::Any);
    assert_eq!(current_date.display, DisplayKind::Function);

    let find = lookup(Lookup::global_method("Find"));
    assert_eq!(find.group, LifecycleGroup::StringSearch);
    assert_eq!(find.replacement, Some(replacement("СтрНайти", "StrFind")));
    assert_eq!(find.compatibility, CompatibilityBucket::CompatibilityMode8_3_6);

    let message = lookup(Lookup::global_method("Сообщить"));
    assert_eq!(message.group, LifecycleGroup::UserNotification);
    assert_eq!(
        message.replacement,
        Some(replacement("ОбщегоНазначения.СообщитьПользователю", "CommonUse.MessageToUser",))
    );
    assert_eq!(message.display, DisplayKind::Function);
}

#[test]
fn deprecation_registry_covers_managed_form_type_fact() {
    let managed_form = lookup(Lookup::type_("ManagedForm"));
    let managed_form_ru = lookup(Lookup::type_("УправляемаяФорма"));

    assert!(std::ptr::eq(managed_form, managed_form_ru));
    assert_eq!(managed_form.group, LifecycleGroup::ManagedForm);
    assert_eq!(
        managed_form.replacement,
        Some(replacement("ФормаКлиентскогоПриложения", "ClientApplicationForm"))
    );
    assert_eq!(managed_form.compatibility, CompatibilityBucket::CompatibilityMode8_3_14);
    assert_eq!(managed_form.display, DisplayKind::Type);
}

#[test]
fn deprecation_registry_covers_8310_application_interface_methods() {
    let short_caption = lookup(Lookup::global_method("GetShortApplicationCaption"));
    let interface_variant =
        lookup(Lookup::global_method("ТекущийВариантИнтерфейсаКлиентскогоПриложения"));

    assert_eq!(short_caption.group, LifecycleGroup::ApplicationInterface);
    assert_eq!(
        short_caption.replacement,
        Some(replacement(
            "КлиентскоеПриложение.ПолучитьКраткийЗаголовок",
            "ClientApplication.GetShortCaption",
        ))
    );
    assert_eq!(short_caption.compatibility, CompatibilityBucket::CompatibilityMode8_3_10);
    assert_eq!(short_caption.display, DisplayKind::Method);
    assert_eq!(
        interface_variant.replacement,
        Some(replacement(
            "КлиентскоеПриложение.ТекущийВариантИнтерфейса",
            "ClientApplication.CurrentInterfaceVariant",
        ))
    );

    let entries = registry()
        .entries()
        .iter()
        .filter(|entry| entry.compatibility == CompatibilityBucket::CompatibilityMode8_3_10)
        .count();
    assert_eq!(entries, 6);
}

#[test]
fn deprecation_registry_covers_8317_error_processing_and_get_form_methods() {
    let brief_error = lookup(Lookup::global_method("BriefErrorDescription"));
    let get_form = lookup(Lookup::global_method("ПолучитьФорму"));

    assert_eq!(brief_error.group, LifecycleGroup::ErrorProcessing);
    assert_eq!(
        brief_error.replacement,
        Some(replacement(
            "ОбработкаОшибок.КраткоеПредставлениеОшибки",
            "ErrorProcessing.BriefErrorDescription",
        ))
    );
    assert_eq!(brief_error.compatibility, CompatibilityBucket::CompatibilityMode8_3_17);
    assert_eq!(brief_error.display, DisplayKind::Method);
    assert_eq!(get_form.group, LifecycleGroup::ManagedForm);
    assert_eq!(get_form.replacement, Some(replacement("ОткрытьФорму", "OpenForm")));

    let entries = registry()
        .entries()
        .iter()
        .filter(|entry| entry.compatibility == CompatibilityBucket::CompatibilityMode8_3_17)
        .count();
    assert_eq!(entries, 7);
}

#[test]
fn deprecation_registry_covers_8312_attributes_with_owners() {
    let chart_plot_area =
        lookup(Lookup::new(ElementKind::Attribute, Some("ChartPlotArea"), "ShowSeriesScaleLabels"));
    assert_eq!(
        chart_plot_area.owner,
        Some(OwnerType {
            ru: "ОбластьПостроенияДиаграммы", en: "ChartPlotArea"
        })
    );
    assert_eq!(chart_plot_area.group, LifecycleGroup::ChartPresentation);
    assert_eq!(
        chart_plot_area.replacement,
        Some(replacement("ШкалаСерий.ПоложениеПодписейШкалы", "SeriesScale.ScaleLabelLocation"))
    );
    assert_eq!(chart_plot_area.compatibility, CompatibilityBucket::CompatibilityMode8_3_12);
    assert_eq!(chart_plot_area.display, DisplayKind::Attribute);

    let gantt_legend =
        lookup(Lookup::new(ElementKind::Attribute, Some("GanttChart"), "ShowLegend"));
    assert_eq!(
        gantt_legend.replacement,
        Some(replacement(
            "одно из свойств ОбластьЛегендыДиаграммы, ОбластьЛегендыДиаграммыГанта или ОбластьЛегендыСводнойДиаграммы",
            "one of the properties of ChartLegendArea, GanttChartLegendArea or PivotChartLegendArea",
        ))
    );
    assert!(registry().lookup(Lookup::new(ElementKind::Attribute, None, "ShowLegend")).is_none());
}

#[test]
fn deprecation_registry_covers_8312_methods_global_methods_and_enums() {
    let palette = lookup(Lookup::method("Chart", "GetPalette"));
    assert_eq!(palette.group, LifecycleGroup::ChartPresentation);
    assert_eq!(
        palette.replacement,
        Some(replacement(
            "ОписаниеПалитрыЦветов.ПолучитьПалитру",
            "ColorPaletteDescription.GetPalette"
        ))
    );
    assert_eq!(palette.display, DisplayKind::Method);

    let clear_event_log = lookup(Lookup::global_method("ClearEventLog"));
    assert_eq!(clear_event_log.group, LifecycleGroup::EventLog);
    assert_eq!(clear_event_log.replacement, None);
    assert_eq!(clear_event_log.display, DisplayKind::GlobalMethod);

    let enum_name = lookup(Lookup::new(ElementKind::EnumName, None, "ОриентацияМетокДиаграммы"));
    assert_eq!(enum_name.group, LifecycleGroup::ChartPresentation);
    assert_eq!(enum_name.replacement, Some(replacement("ОриентацияПодписейДиаграммы", "")));
    assert_eq!(enum_name.display, DisplayKind::EnumName);

    let enum_member =
        lookup(Lookup::new(ElementKind::EnumName, Some("ОриентацияМетокДиаграммы"), "Авто"));
    assert_eq!(enum_member.replacement, None);
    assert_eq!(enum_member.display, DisplayKind::EnumName);

    let enum_value =
        lookup(Lookup::new(ElementKind::EnumValue, Some("ChildFormItemsGroup"), "Horizontal"));
    assert_eq!(enum_value.group, LifecycleGroup::ManagedForm);
    assert_eq!(
        enum_value.replacement,
        Some(replacement("ГоризонтальнаяВсегда", "AlwaysHorizontal"))
    );
    assert_eq!(enum_value.display, DisplayKind::EnumValue);
}

#[test]
fn deprecation_registry_groups_cover_current_platform_deprecated_families() {
    let groups = registry().groups();

    assert!(groups.contains(&LifecycleGroup::DateTime));
    assert!(groups.contains(&LifecycleGroup::StringSearch));
    assert!(groups.contains(&LifecycleGroup::UserNotification));
    assert!(groups.contains(&LifecycleGroup::ManagedForm));
    assert!(groups.contains(&LifecycleGroup::ApplicationInterface));
    assert!(groups.contains(&LifecycleGroup::ErrorProcessing));
    assert!(groups.contains(&LifecycleGroup::ChartPresentation));
    assert!(groups.contains(&LifecycleGroup::EventLog));

    assert_eq!(registry().entries().len(), 54);
}

#[test]
fn deprecation_registry_excludes_source_doc_deprecated_method_call_facts() {
    assert!(registry().lookup(Lookup::global_method("SourceDeprecated")).is_none());
    assert!(registry().lookup(Lookup::global_method("DeprecatedMethodCall")).is_none());
    assert!(registry()
        .entries()
        .iter()
        .all(|entry| entry.ru != "DeprecatedMethodCall" && entry.en != "DeprecatedMethodCall"));
}
