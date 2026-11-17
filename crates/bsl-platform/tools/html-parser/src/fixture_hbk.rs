//! A small, self-written help archive pair for tests: the page markup follows
//! the syntax helper's structure, every text is the project's own.

use std::path::Path;

/// `(path, content)` of the hand-written context archive pages.
const ARRAY_PAGES: &[(&str, &str)] = &[
    (
        "objects/catalog1/Array.html",
        r#"<html><body><h1 class="V8SH_pagetitle">Массив (Array)</h1><p class="V8SH_chapter">Описание:</p><p>Fixture text: an ordered collection.</p><p class="V8SH_chapter">Использование в версии:</p><p class="V8SH_versionInfo">Доступен, начиная с версии 8.0.</p></body></html>"#,
    ),
    (
        "objects/catalog1/Array/methods/Add1.html",
        r#"<html><body><h1 class="V8SH_pagetitle">Массив.Добавить (Array.Add)</h1><p class="V8SH_chapter">Синтаксис:</p>Добавить(&lt;Значение&gt;)<p class="V8SH_chapter">Параметры:</p><div class="V8SH_rubric"> <p>&lt;Значение&gt; (необязательный)</div>Тип: Произвольный. <br>Fixture text: the element to append.<p class="V8SH_chapter">Описание:</p><p>Fixture text: appends one element to the end of the fixture array.</p><p class="V8SH_chapter">Использование в версии:</p><p class="V8SH_versionInfo">Доступен, начиная с версии 8.0.</p></body></html>"#,
    ),
    (
        "objects/catalog1/Array/methods/Count2.html",
        r#"<html><body><h1 class="V8SH_pagetitle">Массив.Количество (Array.Count)</h1><p class="V8SH_chapter">Синтаксис:</p>Количество()<p class="V8SH_chapter">Возвращаемое значение:</p><p>Тип: Число.</p><p class="V8SH_chapter">Описание:</p><p>Fixture text: number of elements.</p></body></html>"#,
    ),
    (
        "objects/Global context/methods/catalog9/StrLen3.html",
        r#"<html><body><h1 class="V8SH_pagetitle">Глобальный контекст.СтрДлина (Global context.StrLen)</h1><p class="V8SH_chapter">Синтаксис:</p>СтрДлина(&lt;Строка&gt;)<p class="V8SH_chapter">Параметры:</p><div class="V8SH_rubric"> <p>&lt;Строка&gt; (обязательный)</div>Тип: Строка. <br>Fixture text: the measured string.<p class="V8SH_chapter">Описание:</p><p>Fixture text: length of a string.</p></body></html>"#,
    ),
];

const SHLANG_PAGES: &[(&str, &str)] = &[("struct_If.st", "fixture")];

/// The analyzer applies its curated overlay to every corpus, and an overlay
/// whose target is absent rejects the corpus. The archive therefore carries a
/// page for every overlay target, generated from the overlay itself.
const OVERLAYS: &str = include_str!("../../../data/platform_overlays.json");

fn version_page(title: &str, version: Option<&str>, params: usize) -> String {
    let mut page = format!(r#"<html><body><h1 class="V8SH_pagetitle">{title}</h1>"#);
    if params > 0 {
        page.push_str(r#"<p class="V8SH_chapter">Параметры:</p>"#);
        for index in 0..params {
            page.push_str(&format!(
                r#"<div class="V8SH_rubric"> <p>&lt;П{index}&gt; (обязательный)</div>Тип: Произвольный. <br>Fixture text."#
            ));
        }
    }
    page.push_str(r#"<p class="V8SH_chapter">Описание:</p><p>Fixture text.</p>"#);
    if let Some(version) = version {
        page.push_str(&format!(
            r#"<p class="V8SH_chapter">Использование в версии:</p><p class="V8SH_versionInfo">Доступен, начиная с версии {version}.</p>"#
        ));
    }
    page.push_str("</body></html>");
    page
}

/// An overlay target and what its records together require of its page.
struct Target {
    owner: String,
    russian: String,
    english: String,
    params: usize,
    version: Option<String>,
}

/// The earliest availability a set of records agrees on. A record without a
/// `min_version` states no lower bound at all, so it wins over any version.
fn earliest(left: Option<String>, right: Option<String>) -> Option<String> {
    let (left, right) = (left?, right?);
    let parts = |version: &str| {
        version.split('.').map(|part| part.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>()
    };
    Some(if parts(&left) <= parts(&right) { left } else { right })
}

/// Collects a record into its TARGET rather than into a page of its own:
/// several records name one method, a different parameter each, and a second
/// page of the same name would make the target ambiguous and reject the whole
/// corpus. The page declares as many parameters as the highest index needs and
/// the earliest version any of its records names.
fn merge_target(targets: &mut Vec<Target>, target: Target) {
    match targets
        .iter_mut()
        .find(|known| known.owner == target.owner && known.english == target.english)
    {
        Some(known) => {
            known.params = known.params.max(target.params);
            known.version = earliest(known.version.take(), target.version);
        }
        None => targets.push(target),
    }
}

fn overlay_target_pages() -> Vec<(String, String)> {
    let overlays: serde_json::Value = serde_json::from_str(OVERLAYS).expect("curated overlay");
    let entries = |section: &str| overlays[section].as_array().cloned().unwrap_or_default();
    let text = |entry: &serde_json::Value, field: &str| entry[field].as_str().map(str::to_owned);
    let mut pages = Vec::new();
    let mut types = std::collections::BTreeSet::new();
    let mut methods: Vec<Target> = Vec::new();
    for entry in entries("method_parameter_overrides") {
        let ty = text(&entry, "canonical_type").unwrap();
        types.insert(ty.clone());
        merge_target(
            &mut methods,
            Target {
                owner: ty,
                russian: text(&entry, "russian_name").unwrap(),
                english: text(&entry, "english_name").unwrap(),
                params: entry["parameter_index"].as_u64().unwrap() as usize + 1,
                version: text(&entry, "min_version"),
            },
        );
    }
    for (index, target) in methods.iter().enumerate() {
        let (ty, ru, en) = (&target.owner, &target.russian, &target.english);
        pages.push((
            format!("objects/catalog2/{ty}/methods/{en}{index}.html"),
            version_page(
                &format!("{ty}.{ru} ({ty}.{en})"),
                target.version.as_deref(),
                target.params,
            ),
        ));
    }
    let mut functions: Vec<Target> = Vec::new();
    for entry in entries("global_function_parameter_overrides") {
        merge_target(
            &mut functions,
            Target {
                owner: String::new(),
                russian: text(&entry, "russian_name").unwrap(),
                english: text(&entry, "english_name").unwrap(),
                params: entry["parameter_index"].as_u64().unwrap() as usize + 1,
                version: text(&entry, "min_version"),
            },
        );
    }
    for (index, target) in functions.iter().enumerate() {
        let (ru, en) = (&target.russian, &target.english);
        pages.push((
            format!("objects/Global context/methods/catalog8/{en}{index}.html"),
            version_page(
                &format!("Глобальный контекст.{ru} (Global context.{en})"),
                target.version.as_deref(),
                target.params,
            ),
        ));
    }
    for entry in entries("type_property_additions") {
        types.insert(text(&entry, "canonical_type").unwrap());
    }
    // A member of the metadata-object family that wildcard overrides expand to.
    let family = ("MetadataObjectCatalog".to_owned(), "ОбъектМетаданных: Справочник".to_owned());
    for (en, ru) in types.into_iter().map(|ty| (ty.clone(), ty)).chain([family]) {
        // The parser finds a type through its directory, next to which the type
        // page lies.
        pages.push((format!("objects/catalog2/{en}/__categories__"), String::new()));
        pages.push((
            format!("objects/catalog2/{en}.html"),
            version_page(&format!("{ru} ({en})"), Some("8.0"), 0),
        ));
    }
    pages
}

/// Every context page of the fixture archive: the hand-written `Массив` pages
/// plus the generated overlay targets.
pub fn shcntx_pages() -> Vec<(String, String)> {
    let mut pages: Vec<(String, String)> =
        ARRAY_PAGES.iter().map(|(path, page)| (path.to_string(), page.to_string())).collect();
    pages.extend(overlay_target_pages());
    pages
}

/// Writes `shcntx_ru.hbk` and `shlang_ru.hbk` into `dir`, with the storage split
/// across `chunk`-byte blocks.
pub fn write_pair(dir: &Path, shcntx_pages: &[(String, String)], chunk: usize) {
    std::fs::create_dir_all(dir).unwrap();
    let shcntx: Vec<(&str, &str)> =
        shcntx_pages.iter().map(|(path, page)| (path.as_str(), page.as_str())).collect();
    for (name, pages) in [("shcntx_ru.hbk", shcntx.as_slice()), ("shlang_ru.hbk", SHLANG_PAGES)] {
        let zip = crate::hbk_writer::build_zip(pages);
        let container = crate::hbk_writer::build_container(
            &[("Book", Some(b"{fixture}")), (crate::hbk::FILE_STORAGE, Some(&zip))],
            chunk,
        );
        std::fs::write(dir.join(name), container).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    /// Two pages under one path or one title make an overlay target ambiguous,
    /// and the help loader then rejects the entire corpus. The failure surfaces
    /// far from here — in every crate that loads help from this archive — so the
    /// invariant is named where the pages are built.
    #[test]
    fn no_two_fixture_pages_share_a_path_or_a_title() {
        let mut paths = BTreeSet::new();
        let mut titles = BTreeSet::new();
        for (path, page) in super::shcntx_pages() {
            assert!(paths.insert(path.clone()), "two fixture pages share the path {path}");
            let Some(rest) = page.split_once("V8SH_pagetitle\">") else { continue };
            let title = rest.1.split('<').next().unwrap_or_default().to_owned();
            assert!(titles.insert(title.clone()), "two fixture pages share the title {title}");
        }
        assert!(titles.len() > 4, "the fixture lost its pages: only {} titles", titles.len());
    }
}
