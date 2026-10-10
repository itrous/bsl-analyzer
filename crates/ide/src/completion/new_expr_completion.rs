use bsl_platform::{platform_type_query, PlatformDataInner, TypeNameInput};
use ide_db::RootDatabase;
use rustc_hash::FxHashSet;
use stdx::case::CaseExt;
use syntax::{SyntaxKind, SyntaxToken};

use super::env_filter::EnvFilter;
use super::{CompletionItem, CompletionItemKind, CompletionPosition};

pub(super) fn new_expr_completions<DB: RootDatabase>(
    db: &DB,
    position: CompletionPosition,
) -> Option<Vec<CompletionItem>> {
    let parse = db.parse(position.file_id);
    let root = parse.syntax_node();
    let cursor_token = root.token_at_offset(position.offset).left_biased()?;

    let (prefix, anchor) = if cursor_token.kind() == SyntaxKind::IDENT {
        (cursor_token.text().to_string(), Some(cursor_token.clone()))
    } else {
        (String::new(), Some(cursor_token.clone()))
    };

    if !is_after_new_keyword(anchor.as_ref()?) {
        return None;
    }

    let data = PlatformDataInner::instance();
    let env_filter = EnvFilter::at(db, position.file_id, position.offset);
    let prefix_lower = prefix.fold_lower();
    let mut seen = FxHashSet::default();
    let mut items = Vec::new();

    let mut ctors: Vec<_> = data.all_constructors().iter().collect();
    ctors.sort_by(|a, b| a.type_name.as_str().cmp(b.type_name.as_str()));

    for ctor in ctors {
        if !seen.insert(ctor.type_name.as_str().to_string()) {
            continue;
        }
        let Some(ty) = platform_type_query(db, TypeNameInput::new(db, ctor.type_name.to_string()))
        else {
            continue;
        };
        // Same gates as the `Новый X` diagnostic: homonym names carry an
        // arbitrary entry's availability, so they are not judged.
        if !data.is_ambiguous_type_name(&ty.name) && !env_filter.admits_context(ty.context.as_ref())
        {
            continue;
        }
        let russian = ty.name.to_string();
        let english = ty.english_name.to_string();

        if !prefix_lower.is_empty()
            && !russian.fold_lower().starts_with(&prefix_lower)
            && !english.fold_lower().starts_with(&prefix_lower)
        {
            continue;
        }

        items.push(CompletionItem {
            label: russian.to_string(),
            detail: Some(format!("{russian} / {english}")),
            kind: CompletionItemKind::Constructor,
            insert_text: russian.to_string(),
            documentation: None,
            sort_text: None,
            filter_text: Some(format!("{russian} {english}")),
            source: None,
        });
    }

    Some(items)
}

fn is_after_new_keyword(anchor: &SyntaxToken) -> bool {
    let mut cur = if anchor.kind() == SyntaxKind::IDENT {
        syntax::prev_token_past_empty(anchor)
    } else {
        Some(anchor.clone())
    };
    while let Some(t) = cur.clone() {
        match t.kind() {
            kind if kind.is_trivia() => {
                cur = syntax::prev_token_past_empty(&t);
            }
            SyntaxKind::KW_NEW => return true,
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Analysis;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::vfs::{file_set::FileSet, VfsPath};
    use ide_db::RootDatabaseImpl;

    fn setup(code: &str) -> (Analysis, vfs::FileId, u32) {
        let cursor = code.find("$0").expect("$0 marker");
        let cleaned = code.replace("$0", "");
        let offset = cursor as u32;

        let mut db = RootDatabaseImpl::new();
        let file_id = vfs::FileId(0);
        let mut fs = FileSet::new();
        fs.insert(file_id, VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(fs));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, &cleaned);
        (Analysis::from_database(db), file_id, offset)
    }

    fn all_labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn offers_only_constructors_in_new_position_empty_prefix() {
        let code = "Процедура Тест()
    Х = Новый $0
КонецПроцедуры";
        let (analysis, file_id, offset) = setup(code);
        let items = analysis.completions(file_id, offset, None, crate::Locale::Ru);
        assert!(!items.is_empty(), "must offer at least one platform type");
        assert!(
            items.iter().all(|i| i.kind == CompletionItemKind::Constructor),
            "all items must be Constructor kind, got kinds: {:?}",
            items.iter().map(|i| i.kind).collect::<Vec<_>>()
        );
        let labels = all_labels(&items);
        assert!(labels.contains(&"Массив".to_string()), "labels={labels:?}");
        assert!(labels.contains(&"Структура".to_string()), "labels={labels:?}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn filters_by_prefix_russian() {
        let code = "Процедура Тест()
    Х = Новый Масс$0
КонецПроцедуры";
        let (analysis, file_id, offset) = setup(code);
        let items = analysis.completions(file_id, offset, None, crate::Locale::Ru);
        let labels = all_labels(&items);
        assert!(
            labels.iter().all(|l| l.fold_lower().starts_with("масс")),
            "every label must match prefix Масс, got: {labels:?}"
        );
        assert!(labels.contains(&"Массив".to_string()), "Массив must be offered");
    }

    #[test]
    fn does_not_fire_outside_new_position() {
        let code = "Процедура Тест()
    Х = $0
КонецПроцедуры";
        let (analysis, file_id, offset) = setup(code);
        let items = analysis.completions(file_id, offset, None, crate::Locale::Ru);
        let constructor_only =
            !items.is_empty() && items.iter().all(|i| i.kind == CompletionItemKind::Constructor);
        assert!(!constructor_only, "outside `Новый ` we must not lock into constructor-only mode");
    }
}
