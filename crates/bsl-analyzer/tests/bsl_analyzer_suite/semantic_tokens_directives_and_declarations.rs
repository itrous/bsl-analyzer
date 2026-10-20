//! What an LSP client decodes for compilation directives, extension
//! annotations and declaration keywords: every emitted token type and
//! modifier must exist in the advertised legend under the expected name.

use bsl_analyzer::lsp::{semantic_tokens, semantic_tokens_legend};
use ide::highlight;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use line_index::LineIndex;
use vfs::{FileId, FileSet, VfsPath};

/// `(text, token type, modifiers)` for every token, decoded through the legend
/// exactly as a client would.
fn decoded_tokens(source: &str) -> Vec<(String, String, Vec<String>)> {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    let highlights = highlight(&db, file_id).highlights;

    let legend = semantic_tokens_legend();
    let line_index = LineIndex::new(source);
    let tokens = semantic_tokens(&line_index, source, &highlights);
    let lines: Vec<&str> = source.split('\n').collect();

    let mut line = 0usize;
    let mut start = 0usize;
    let mut out = Vec::new();
    for token in tokens {
        if token.delta_line > 0 {
            line += token.delta_line as usize;
            start = token.delta_start as usize;
        } else {
            start += token.delta_start as usize;
        }
        let utf16: Vec<u16> = lines[line].encode_utf16().collect();
        let text = String::from_utf16(&utf16[start..start + token.length as usize]).unwrap();
        let ty = legend
            .token_types
            .get(token.token_type as usize)
            .unwrap_or_else(|| {
                panic!("token type {} of {text:?} is not in the legend", token.token_type)
            })
            .as_str()
            .to_string();
        let mods = legend
            .token_modifiers
            .iter()
            .enumerate()
            .filter(|(bit, _)| token.token_modifiers_bitset & (1 << bit) != 0)
            .map(|(_, modifier)| modifier.as_str().to_string())
            .collect();
        out.push((text, ty, mods));
    }
    out
}

fn decoded<'a>(
    tokens: &'a [(String, String, Vec<String>)],
    text: &str,
) -> &'a (String, String, Vec<String>) {
    tokens
        .iter()
        .find(|(t, _, _)| t == text)
        .unwrap_or_else(|| panic!("no token {text:?}: {tokens:?}"))
}

#[test]
fn legend_advertises_the_modifier_token_type() {
    let legend = semantic_tokens_legend();
    assert!(
        legend.token_types.iter().any(|ty| ty.as_str() == "modifier"),
        "legend: {:?}",
        legend.token_types
    );
    assert!(legend.token_modifiers.iter().any(|m| m.as_str() == "declaration"));
}

#[test]
fn client_decodes_directives_annotations_and_declaration_keywords() {
    let source = "&НаСервере\n&После(\"Исходный\")\nПроцедура Расш_Исходный(Знач Параметр) Экспорт\n    Если Параметр Тогда\n        Возврат;\n    КонецЕсли;\nКонецПроцедуры\n";
    let tokens = decoded_tokens(source);

    let (_, ty, mods) = decoded(&tokens, "&НаСервере");
    assert_eq!(ty, "modifier");
    assert!(mods.is_empty(), "{mods:?}");

    let (_, ty, _) = decoded(&tokens, "&После");
    assert_eq!(ty, "decorator");

    for keyword in ["Процедура", "Знач", "Экспорт", "КонецПроцедуры"]
    {
        let (_, ty, mods) = decoded(&tokens, keyword);
        assert_eq!(ty, "keyword", "{keyword}");
        assert!(mods.iter().any(|m| m == "declaration"), "{keyword}: {mods:?}");
    }
    for keyword in ["Если", "Тогда", "Возврат", "КонецЕсли"] {
        let (_, ty, mods) = decoded(&tokens, keyword);
        assert_eq!(ty, "keyword", "{keyword}");
        assert!(!mods.iter().any(|m| m == "declaration"), "{keyword}: {mods:?}");
    }
}
