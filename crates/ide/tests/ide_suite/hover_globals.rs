use ide::Analysis;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
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

fn hover_markup(fixture: &str) -> Option<String> {
    let (analysis, file_id, offset) = setup(fixture);
    analysis.hover(file_id, offset, ide::Locale::Ru).map(|h| h.markup)
}

fn hbk_globals_available() -> bool {
    !bsl_platform::PlatformDataInner::instance().all_global_properties().is_empty()
}

#[test]
fn hover_bare_metadata_shows_hbk_property_markup() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    А = Метад$0анные;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve");
    assert!(markup.contains("Метаданные"), "title missing; markup: {markup}");
    assert!(markup.contains("ОбъектМетаданныхКонфигурация"), "type info missing; markup: {markup}");
    assert!(markup.contains("Только чтение"), "readonly marker missing; markup: {markup}");
}

#[test]
fn hover_bare_mdo_plural_combines_workspace_shape_and_hbk() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    А = Доку$0менты;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve for MDO plural");
    assert!(
        markup.contains("ДокументМенеджер"),
        "workspace ManagerCollection shape must surface; markup: {markup}"
    );
    assert!(
        !markup.contains("ДокументыМенеджер"),
        "HBK declared plural manager type must NOT replace workspace shape; markup: {markup}"
    );
    assert!(
        markup.contains("Только чтение"),
        "HBK readonly marker must surface for MDO plural; markup: {markup}"
    );
}

#[test]
fn hover_mdo_plural_shows_hbk_description_when_present() {
    if !hbk_globals_available() {
        return;
    }
    let data = bsl_platform::PlatformDataInner::instance();
    let prop = data.get_global_property("Документы").expect("HBK must list Документы");
    let Some(docs) = data.get_property_docs(prop.id) else {
        return;
    };
    if docs.description.trim().is_empty() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    А = Доку$0менты;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve");
    let head: String = docs.description.trim().chars().take(20).collect();
    assert!(
        markup.contains(&head),
        "HBK description prefix {head:?} must appear in hover markup; got: {markup}"
    );
}

#[test]
fn hover_mdo_plural_assigned_from_another_collection_keeps_its_own_shape() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Документы = Справочники;
    А = Доку$0менты;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve");
    // The assignment writes to a Global-context property and rebinds nothing, so
    // `Документы` is still the document collection — not the catalog one.
    assert!(
        !markup.contains("СправочникМенеджер"),
        "the assignment cannot turn Документы into the catalog collection; markup: {markup}"
    );
    assert!(
        markup.contains("ДокументМенеджер") || markup.contains("Документы (Documents)"),
        "the document collection is what the name denotes; markup: {markup}"
    );
}

#[test]
fn hover_mdo_plural_assigned_a_primitive_keeps_the_collection_card() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Документы = "просто строка";
    А = Доку$0менты;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve");
    // Assigning a string declares no local either: the platform refuses the write,
    // so the name is still the collection and the HBK card applies.
    assert!(
        markup.contains("Документы (Documents)"),
        "the collection card is the honest answer; markup: {markup}"
    );
}

#[test]
fn hover_local_shadowing_metadata_uses_local_hover() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Перем Метад$0анные;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve at Перем declaration");
    assert!(
        !markup.contains("ОбъектМетаданныхКонфигурация"),
        "HBK markup leaked when local Перем shadows the global; markup: {markup}"
    );
}

#[test]
fn hover_local_var_shadows_mdo_plural() {
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Перем Доку$0менты;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve at Перем declaration");
    assert!(
        !markup.contains("ДокументыМенеджер"),
        "HBK manager type leaked when local shadows MDO plural; markup: {markup}"
    );
}

#[test]
fn hover_workspace_cm_shadows_hbk_global_property() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /CommonModules/Метаданные/Ext/Module.bsl
Функция Foo() Экспорт
    Возврат 1;
КонецФункции

//- /test.bsl
Процедура Тест()
    А = Метад$0анные;
КонецПроцедуры
"#,
    );
    if let Some(m) = markup {
        assert!(
            !m.contains("ОбъектМетаданныхКонфигурация"),
            "HBK markup leaked when workspace CommonModule shadows the global; markup: {m}"
        );
    }
}

#[test]
fn hover_primitive_typed_global_property_renders_hbk() {
    if !hbk_globals_available() {
        return;
    }
    let prop = bsl_platform::PlatformDataInner::instance().get_global_property("ПараметрЗапуска");
    let Some(prop) = prop else { return };
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    А = Парам$0етрЗапуска;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve for primitive-typed global property");

    let exclusive_marker = format!("**{} ({})**", prop.name, prop.english_name);
    let has_exclusive = markup.contains(&exclusive_marker)
        || (prop.is_readonly && markup.contains("*Только чтение*"))
        || prop.min_version.as_ref().is_some_and(|v| markup.contains(v.as_str()));
    assert!(
        has_exclusive,
        "no HBK-exclusive marker in markup — fallback rendered instead of \
         `render_property_hover`. Expected one of: bilingual title \
         {exclusive_marker:?}, `*Только чтение*` (is_readonly={}), or \
         min_version marker ({:?}). markup: {markup}",
        prop.is_readonly,
        prop.min_version.as_ref().map(|v| v.as_str()),
    );
}

#[test]
fn hover_implicit_local_shadows_hbk_property() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные = "просто строка";
    А = Метад$0анные;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve");
    assert!(
        !markup.contains("ОбъектМетаданныхКонфигурация"),
        "HBK markup leaked when implicit local shadows the global; markup: {markup}"
    );
    assert!(
        !markup.contains("Только чтение"),
        "HBK readonly marker leaked when implicit local shadows the global; markup: {markup}"
    );
}

#[test]
fn hover_bare_global_function_unaffected() {
    if bsl_platform::PlatformDataInner::instance().all_global_functions().is_empty() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Начать$0Транзакцию();
КонецПроцедуры
"#,
    )
    .expect("global function hover should resolve");
    assert!(markup.contains("НачатьТранзакцию"), "function title missing; markup: {markup}");
}

#[test]
fn hover_receiver_property_now_shows_version_and_availability() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Метад$0анные;
КонецПроцедуры
"#,
    )
    .expect("hover should resolve");
    let prop = bsl_platform::PlatformDataInner::instance()
        .get_global_property("Метаданные")
        .expect("Метаданные must be in HBK");

    if let Some(ver) = prop.min_version.as_ref() {
        assert!(
            markup.contains(ver.as_str()),
            "min_version {ver:?} expected in markup; got: {markup}"
        );
    }
    if prop.context.is_some() {
        assert!(
            markup.contains("**Доступность:**"),
            "availability section expected in markup; got: {markup}"
        );
    }
}

/// The type-comparison heuristic in `hover_for_global_property` cannot catch a
/// local of UNKNOWN type: nothing distinguishes it from the global unless the
/// bare-name cascade refuses to re-type a held name in the first place.
#[test]
fn hover_implicit_local_of_unknown_type_shadows_hbk_property() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные = НеизвестнаяФункция();
    А = Метад$0анные;
КонецПроцедуры
"#,
    );
    if let Some(markup) = markup {
        assert!(
            !markup.contains("ОбъектМетаданныхКонфигурация"),
            "HBK markup leaked when a local of unknown type holds the name; markup: {markup}"
        );
        assert!(
            !markup.contains("Только чтение"),
            "HBK readonly marker leaked when a local of unknown type holds the name; markup: {markup}"
        );
    }
}

/// A user symbol holding the name rules out EVERY text-keyed reading in
/// `hover_free_name`, not just the global-property one. The four below are the
/// complete set of such readings; each was verified to leak before the guard
/// moved to the caller.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn assigned_collection_name_still_shows_the_collection_card() {
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Справочники = НеизвестнаяФункция();
    А = Справоч$0ники;
КонецПроцедуры
"#,
    );
    // An assignment to a Global-context property declares no local, so the read is
    // still the collection and its card is the honest answer.
    assert!(
        markup.as_deref().is_some_and(|m| m.contains("СправочникМенеджер")),
        "the name still denotes the collection; markup: {markup:?}"
    );
}

#[test]
fn held_name_suppresses_global_property_card() {
    if !hbk_globals_available() {
        return;
    }
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    Метаданные = НеизвестнаяФункция();
    А = Метад$0анные;
КонецПроцедуры
"#,
    );
    assert!(
        markup.as_deref().is_none_or(|m| !m.contains("ОбъектМетаданныхКонфигурация")),
        "global-property card leaked for a held name; markup: {markup:?}"
    );
}

#[test]
fn held_name_suppresses_platform_type_card() {
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    ВидДвиженияБухгалтерии = НеизвестнаяФункция();
    А = ВидДвиженияБухгалт$0ерии;
КонецПроцедуры
"#,
    );
    assert!(
        markup.as_deref().is_none_or(|m| !m.contains("**Тип:** ВидДвиженияБухгалтерии")),
        "platform-type card leaked for a held name; markup: {markup:?}"
    );
}

#[test]
fn held_name_suppresses_global_function_card() {
    let markup = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    НачатьТранзакцию = НеизвестнаяФункция();
    А = Начать$0Транзакцию;
КонецПроцедуры
"#,
    );
    assert!(
        markup.as_deref().is_none_or(|m| !m.contains("Глобальная функция")),
        "global-function card leaked for a held name; markup: {markup:?}"
    );
}

/// Positive control for the whole class: with nobody holding the names, all four
/// readings still render.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn unheld_names_still_render_all_four_readings() {
    let plural = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    А = Справоч$0ники;
КонецПроцедуры
"#,
    )
    .expect("metadata collection must render");
    assert!(plural.contains("Справочники"), "plural markup: {plural}");

    if hbk_globals_available() {
        let prop = hover_markup(
            r#"//- /test.bsl
Процедура Тест()
    А = Метад$0анные;
КонецПроцедуры
"#,
        )
        .expect("global property must render");
        assert!(prop.contains("ОбъектМетаданныхКонфигурация"), "property markup: {prop}");
    }

    let platform_type = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    А = ВидДвиженияБухгалт$0ерии;
КонецПроцедуры
"#,
    )
    .expect("platform type must render");
    assert!(
        platform_type.contains("ВидДвиженияБухгалтерии"),
        "platform type markup: {platform_type}"
    );

    let func = hover_markup(
        r#"//- /test.bsl
Процедура Тест()
    А = Начать$0Транзакцию;
КонецПроцедуры
"#,
    )
    .expect("global function must render");
    assert!(func.contains("НачатьТранзакцию"), "function markup: {func}");
}
