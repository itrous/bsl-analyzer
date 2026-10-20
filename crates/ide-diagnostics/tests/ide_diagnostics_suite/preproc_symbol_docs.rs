//! Пользовательские таблицы символов препроцессора обязаны совпадать с
//! реестром.
//!
//! Расхождение здесь тихое: диагностика начинает предлагать канон для
//! символа, которого в опубликованной таблице нет, и заметить это можно
//! только чтением документа. До сведения списков ровно так и было с
//! `МобильныйАвтономныйСервер`.

use syntax::preproc_symbols::PreprocSymbolId;

fn doc(lang: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join(lang)
        .join("CanonicalSpellingKeywords.md");
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

#[test]
fn both_docs_list_every_registry_spelling() {
    for lang in ["ru", "en"] {
        let text = doc(lang);
        for &id in PreprocSymbolId::ALL {
            let [ru, en] = id.spellings();
            let row = format!("| {ru} ");
            assert!(
                text.contains(&row),
                "docs/{lang}/CanonicalSpellingKeywords.md: нет строки символа {ru:?}"
            );
            assert!(
                text.contains(en),
                "docs/{lang}/CanonicalSpellingKeywords.md: нет написания {en:?}"
            );
        }
        // Контроль чувствительности: написание вне реестра в таблице символов
        // не стоит, иначе проверка зелена и у документа, перечисляющего всё
        // подряд.
        assert!(
            !text.contains("| Linux "),
            "docs/{lang}/CanonicalSpellingKeywords.md: ОС-символ попал в таблицу"
        );
    }
}
