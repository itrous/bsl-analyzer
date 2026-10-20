use ide::{Analysis, CompletionItem, CompletionItemKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::collections::BTreeSet;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn setup(fixture_text: &str) -> (Analysis, FileId, u32) {
    let (fixture_text, test_path, cursor_offset) = extract_cursor(fixture_text);
    let fixture = Fixture::parse(&fixture_text);

    let mut db = RootDatabaseImpl::new();
    let source_root_id = SourceRootId(0);

    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(source_root_id, source_root);

    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, source_root_id);
        db.set_file_text(*file_id, &file.content);
    }

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(&test_path))
        .map(|(id, _)| *id)
        .expect("cursor-bearing file not found");

    (Analysis::from_database(db), test_file, cursor_offset)
}

fn extract_cursor(fixture_text: &str) -> (String, String, u32) {
    let abs_idx = fixture_text.find("$0").expect("fixture must contain $0 cursor marker");
    let prefix = &fixture_text[..abs_idx];
    let last_header_start = prefix.rfind("//- ").expect("cursor must be inside a //- file");
    let header_end =
        prefix[last_header_start..].find('\n').expect("//- header must end with newline")
            + last_header_start;
    let path_line = &prefix[last_header_start + 4..header_end];
    let file_offset_in_prefix = header_end + 1;
    let cursor_in_file = (abs_idx - file_offset_in_prefix) as u32;
    let cleaned = fixture_text.replacen("$0", "", 1);
    (cleaned, path_line.to_string(), cursor_in_file)
}

fn complete(fixture: &str) -> Vec<CompletionItem> {
    let (analysis, file_id, offset) = setup(fixture);
    analysis.completions(file_id, offset, None, ide::Locale::Ru)
}

fn find_mdo_plural<'a>(items: &'a [CompletionItem], label: &str) -> Option<&'a CompletionItem> {
    items.iter().find(|i| i.kind == CompletionItemKind::MdoType && i.label == label)
}

fn collect_mdo_plural_labels(items: &[CompletionItem]) -> BTreeSet<String> {
    items
        .iter()
        .filter(|i| i.kind == CompletionItemKind::MdoType)
        .filter(|i| i.detail.as_deref().is_some_and(|d| d.starts_with("Коллекция метаданных")))
        .map(|i| i.label.clone())
        .collect()
}

fn hbk_globals_available() -> bool {
    !bsl_platform::PlatformDataInner::instance().all_global_properties().is_empty()
}

#[test]
fn completion_mdo_plural_label_from_hbk() {
    if !hbk_globals_available() {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Док$0
КонецПроцедуры
"#,
    );
    let docs = find_mdo_plural(&items, "Документы").unwrap_or_else(|| {
        panic!(
            "Документы (MdoType) must appear; labels: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        )
    });
    let hbk = bsl_platform::PlatformDataInner::instance()
        .get_global_property("Документы")
        .expect("HBK must list Документы as a global property");
    assert_eq!(docs.label, hbk.name.as_str(), "label must come from HBK prop.name");
}

#[test]
fn completion_mdo_plural_bilingual_english_from_hbk() {
    if !hbk_globals_available() {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Doc$0
КонецПроцедуры
"#,
    );
    let docs = find_mdo_plural(&items, "Документы").unwrap_or_else(|| {
        panic!(
            "English prefix `Doc` must surface Документы via HBK english_name; labels: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        )
    });
    let filter = docs.filter_text.as_deref().unwrap_or("");
    assert!(filter.contains("Документы"), "filter_text must contain RU name; filter = {filter:?}");
    assert!(filter.contains("Documents"), "filter_text must contain EN name; filter = {filter:?}");
}

#[test]
fn completion_mdo_plural_readonly_marker_in_detail() {
    if !hbk_globals_available() {
        return;
    }
    let hbk = bsl_platform::PlatformDataInner::instance()
        .get_global_property("Документы")
        .expect("HBK must list Документы");
    if !hbk.is_readonly {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Док$0
КонецПроцедуры
"#,
    );
    let docs = find_mdo_plural(&items, "Документы").expect("Документы (MdoType) must appear");
    let detail = docs.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("[Только чтение]"),
        "HBK readonly flag must surface in detail; detail = {detail:?}",
    );
}

#[test]
fn completion_mdo_plural_documentation_from_hbk() {
    if !hbk_globals_available() {
        return;
    }
    let data = bsl_platform::PlatformDataInner::instance();
    let hbk = data.get_global_property("Документы").expect("HBK must list Документы");
    let Some(hbk_docs) = data.get_property_docs(hbk.id) else {
        return;
    };
    if hbk_docs.description.trim().is_empty() {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Док$0
КонецПроцедуры
"#,
    );
    let docs = find_mdo_plural(&items, "Документы").expect("Документы must appear");
    let doc_str = docs.documentation.as_deref().unwrap_or("");
    let probe = hbk_docs.description.trim();
    let head: String = probe.chars().take(20).collect();
    assert!(
        doc_str.contains(&head),
        "documentation panel must include HBK description prefix {head:?}; got {doc_str:?}",
    );
}

#[test]
fn completion_mdo_plural_kind_remains_mdo_type() {
    if !hbk_globals_available() {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Док$0
КонецПроцедуры
"#,
    );
    let docs = find_mdo_plural(&items, "Документы").expect("Документы must appear");
    assert_eq!(
        docs.kind,
        CompletionItemKind::MdoType,
        "HBK migration must not downgrade kind to Property"
    );
}

#[test]
fn completion_mdo_plural_detail_keeps_legacy_prefix() {
    if !hbk_globals_available() {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Док$0
КонецПроцедуры
"#,
    );
    let docs = find_mdo_plural(&items, "Документы").expect("Документы must appear");
    let detail = docs.detail.as_deref().unwrap_or("");
    assert!(detail.starts_with("Коллекция метаданных"), "detail = {detail:?}");
}

#[test]
fn completion_mdo_plural_common_module_not_emitted_top_level() {
    if !hbk_globals_available() {
        return;
    }
    let items = complete(
        r#"//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ПримернаяФункция() Экспорт
    Возврат 1;
КонецФункции

//- /test.bsl
Процедура Тест()
    Общ$0
КонецПроцедуры
"#,
    );
    assert!(
        find_mdo_plural(&items, "ОбщиеМодули").is_none(),
        "`ОбщиеМодули` is an HBK type descriptor, not a Global-context property; \
         it must not surface at top level. labels: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    let workspace_cm = items
        .iter()
        .find(|i| i.label == "ОбщегоНазначения" && i.kind == CompletionItemKind::MdoObject);
    assert!(
        workspace_cm.is_some(),
        "workspace CommonModule (band 3) must still surface; labels: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn completion_mdo_plural_cube_not_emitted_top_level() {
    if !hbk_globals_available() {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    Куб$0
КонецПроцедуры
"#,
    );
    assert!(
        find_mdo_plural(&items, "Кубы").is_none(),
        "`Кубы` is nested under ExternalDataSourceManager in HBK, not a Global-context \
         property; it must not surface at top level. labels: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn completion_mdo_plural_dimension_table_not_emitted_top_level() {
    if !hbk_globals_available() {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    ТаблицыИ$0
КонецПроцедуры
"#,
    );
    assert!(
        find_mdo_plural(&items, "ТаблицыИзмерения").is_none(),
        "`ТаблицыИзмерения` is nested under ExternalDataSourceCubeManager in HBK, not a \
         Global-context property; it must not surface at top level. labels: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn mdo_plural_completion_set_matches_frozen_baseline() {
    if !hbk_globals_available() {
        return;
    }
    let items = complete(
        r#"//- /test.bsl
Процедура Тест()
    А = 1;
    $0
КонецПроцедуры
"#,
    );
    let emitted = collect_mdo_plural_labels(&items);
    let baseline_txt = include_str!("../fixtures/expected_mdo_plurals.txt");
    let expected: BTreeSet<String> = baseline_txt
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect();
    assert_eq!(
        emitted, expected,
        "MDO plural baseline drift — refresh fixtures/expected_mdo_plurals.txt and update \
         MdoType::all() if a new platform-shipped MDO collection arrived per \
         crates/bsl-platform/data/PROVENANCE.md.\nemitted: {emitted:?}\nexpected: {expected:?}"
    );

    assert!(!emitted.contains("Кубы"), "Кубы must not be emitted at top level");
    assert!(!emitted.contains("ТаблицыИзмерения"), "ТаблицыИзмерения must not be emitted");
    assert!(!emitted.contains("ОбщиеМодули"), "ОбщиеМодули must not be emitted");
}
