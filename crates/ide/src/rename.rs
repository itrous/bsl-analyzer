use hir::{ReferenceScope, Semantics};
use ide_db::RootDatabase;
use lexer::{tokenize, TokenKind};
use syntax::{TextRange, TextSize};
use vfs::FileId;

use crate::Location;

/// The identifier under the cursor that a rename would act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameTarget {
    /// Range of the identifier token in the requesting file.
    pub range: TextRange,
    /// Current text of the identifier, offered to clients as the edit placeholder.
    pub current_name: String,
}

/// Why a rename request could not be fulfilled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    /// The cursor is not on a symbol this engine can rename (e.g. a builtin,
    /// a metadata object, or a token that resolves to nothing).
    NotRenameable,
    /// The requested new name is not a single valid BSL identifier.
    InvalidIdentifier(String),
}

/// Validate a rename at `offset` and report the identifier that would be renamed.
///
/// Returns `None` when the position is not on a renameable symbol — the LSP
/// `prepareRename` contract for "rename not possible here".
pub fn prepare_rename<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
) -> Option<RenameTarget> {
    let token = renameable_token(db, file_id, offset)?;
    Some(RenameTarget { range: token.text_range(), current_name: token.text().to_string() })
}

/// Compute the occurrences a rename to `new_name` must edit.
///
/// The edit set is exactly the references of the symbol under the cursor
/// (declaration included), so scoping — file-local vs export-workspace — is
/// inherited from [`crate::references::find_references`].
pub fn rename<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
    new_name: &str,
) -> Result<Vec<Location>, RenameError> {
    if !is_valid_identifier(new_name) {
        return Err(RenameError::InvalidIdentifier(new_name.to_string()));
    }

    if renameable_token(db, file_id, offset).is_none() {
        return Err(RenameError::NotRenameable);
    }

    let locations = crate::references::find_references(db, file_id, offset);
    if locations.is_empty() {
        return Err(RenameError::NotRenameable);
    }

    Ok(locations)
}

fn renameable_token<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
) -> Option<syntax::SyntaxToken> {
    let parse = db.parse(file_id);
    let root = parse.syntax_node();

    let token =
        root.token_at_offset(offset).right_biased().filter(|token| token.kind().is_name_token())?;

    let sema = Semantics::new(db);
    let symbol = sema.symbol_for_token(file_id, &token)?;

    match symbol.reference_scope(db) {
        ReferenceScope::FileLocal | ReferenceScope::ModuleSymbolWorkspace => Some(token),
        ReferenceScope::Unknown | ReferenceScope::Metadata => None,
    }
}

/// A rename target must be spelled as one BSL identifier — not a keyword, not
/// an expression, not padded with whitespace. Reuse the real lexer so the check
/// tracks the language exactly (Cyrillic/Latin letters, digits, `_`).
pub(crate) fn is_valid_identifier(name: &str) -> bool {
    let tokens = tokenize(name);
    matches!(tokens.as_slice(), [token] if token.kind == TokenKind::Ident)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use vfs::{file_set::FileSet, VfsPath};

    fn single_file(source: &str) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::default();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, source);
        (db, file_id)
    }

    fn two_files(src_a: &str, src_b: &str) -> (RootDatabaseImpl, FileId, FileId) {
        let mut db = RootDatabaseImpl::default();
        let file_a = FileId(0);
        let file_b = FileId(1);
        let mut file_set = FileSet::new();
        file_set.insert(file_a, VfsPath::new("/a.bsl"));
        file_set.insert(file_b, VfsPath::new("/b.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_a, SourceRootId(0));
        db.set_file_source_root(file_b, SourceRootId(0));
        db.set_file_text(file_a, src_a);
        db.set_file_text(file_b, src_b);
        (db, file_a, file_b)
    }

    fn offset_of(source: &str, needle: &str) -> TextSize {
        TextSize::from(source.find(needle).unwrap() as u32)
    }

    #[test]
    fn valid_identifier_accepts_bilingual_names() {
        assert!(is_valid_identifier("МояПеременная"));
        assert!(is_valid_identifier("MyVariable"));
        assert!(is_valid_identifier("_счётчик1"));
    }

    #[test]
    fn valid_identifier_rejects_keywords_and_garbage() {
        assert!(!is_valid_identifier("Если"));
        assert!(!is_valid_identifier("If"));
        assert!(!is_valid_identifier("1abc"));
        assert!(!is_valid_identifier("Мой Метод"));
        assert!(!is_valid_identifier(" Метод"));
        assert!(!is_valid_identifier(""));
    }

    #[test]
    fn prepare_reports_identifier_range_and_name() {
        let source = "Процедура МояПроцедура()\nКонецПроцедуры\n";
        let (db, file_id) = single_file(source);
        let offset = offset_of(source, "МояПроцедура");

        let target = prepare_rename(&db, file_id, offset).expect("procedure is renameable");
        assert_eq!(target.current_name, "МояПроцедура");
        assert_eq!(&source[target.range], "МояПроцедура");
    }

    #[test]
    fn prepare_declines_builtin_call() {
        let source = "Процедура Тест()\n    Сообщить(1);\nКонецПроцедуры\n";
        let (db, file_id) = single_file(source);
        let offset = offset_of(source, "Сообщить");

        assert!(prepare_rename(&db, file_id, offset).is_none());
    }

    #[test]
    fn rename_rejects_invalid_new_name() {
        let source = "Процедура МояПроцедура()\nКонецПроцедуры\n";
        let (db, file_id) = single_file(source);
        let offset = offset_of(source, "МояПроцедура");

        assert_eq!(
            rename(&db, file_id, offset, "Если").unwrap_err(),
            RenameError::InvalidIdentifier("Если".to_string())
        );
    }

    #[test]
    fn rename_declines_unrenameable_position() {
        let source = "Процедура Тест()\n    Сообщить(1);\nКонецПроцедуры\n";
        let (db, file_id) = single_file(source);
        let offset = offset_of(source, "Сообщить");

        assert_eq!(
            rename(&db, file_id, offset, "НовоеИмя").unwrap_err(),
            RenameError::NotRenameable
        );
    }

    #[test]
    fn rename_local_variable_stays_in_file() {
        let source = r#"
Процедура Тест()
    Перем МояПеременная;
    МояПеременная = 10;
    Результат = МояПеременная * 2;
КонецПроцедуры
"#;
        let (db, file_id) = single_file(source);
        let offset = offset_of(source, "МояПеременная");

        let locations = rename(&db, file_id, offset, "Итог").expect("local var is renameable");
        assert_eq!(locations.len(), 3, "declaration + 2 usages");
        assert!(locations.iter().all(|loc| loc.file_id == file_id));
    }

    #[test]
    fn rename_export_method_does_not_touch_same_named_method_elsewhere() {
        // A bare call resolves within its own module, so an export method in a
        // plain object module and a same-named method in another module are
        // distinct symbols. The workspace-scope path must keep them apart.
        let src_a = r#"
Процедура МояПроцедура() Экспорт
    МояПроцедура();
КонецПроцедуры
"#;
        let src_b = r#"
Процедура МояПроцедура()
    МояПроцедура();
КонецПроцедуры
"#;
        let (db, file_a, _file_b) = two_files(src_a, src_b);
        let offset = offset_of(src_a, "МояПроцедура");

        let locations =
            rename(&db, file_a, offset, "НоваяПроцедура").expect("export method is renameable");
        assert_eq!(locations.len(), 2, "declaration + self-call in file A");
        assert!(
            locations.iter().all(|loc| loc.file_id == file_a),
            "export rename must not leak into a same-named method in file B: {locations:?}"
        );
    }

    #[test]
    fn rename_non_export_method_stays_file_local() {
        let src_a = r#"
Процедура Помощник()
КонецПроцедуры

Процедура Тест()
    Помощник();
КонецПроцедуры
"#;
        let src_b = r#"
Процедура Помощник()
КонецПроцедуры
"#;
        let (db, file_a, file_b) = two_files(src_a, src_b);
        let offset = offset_of(src_a, "Помощник");

        let locations = rename(&db, file_a, offset, "Хелпер").expect("method is renameable");
        assert!(
            locations.iter().all(|loc| loc.file_id == file_a),
            "non-export rename must not cross files: {locations:?}"
        );
        assert_ne!(file_a, file_b);
    }

    /// A rename edits exactly the references, so an identity split into slices does
    /// not refuse — it silently rewrites half the occurrences and leaves a body where
    /// one variable has become two.
    ///
    /// The declared body is the control: the same text with `Перем` already renames in
    /// full, so a run where only the implicit body loses sites names the split alone.
    #[test]
    fn renaming_an_implicit_local_edits_every_occurrence() {
        const IMPLICIT: &str = r#"
Процедура Тест()
    НаборЗаписей = 1;
    Сообщить(НаборЗаписей);
    НаборЗаписей = 2;
    Сообщить(НаборЗаписей);
КонецПроцедуры
"#;
        const DECLARED: &str = r#"
Процедура Тест()
    Перем НаборЗаписей;
    НаборЗаписей = 1;
    Сообщить(НаборЗаписей);
    НаборЗаписей = 2;
    Сообщить(НаборЗаписей);
КонецПроцедуры
"#;

        for (label, source, expected) in
            [("implicit", IMPLICIT, 4usize), ("control: declared", DECLARED, 5)]
        {
            let (db, file_id) = single_file(source);
            let mut from = 0usize;
            while let Some(at) = source[from..].find("НаборЗаписей") {
                let at = from + at;
                from = at + "НаборЗаписей".len();
                let edits = rename(&db, file_id, TextSize::from(at as u32), "Набор")
                    .expect("a local is renameable");
                assert_eq!(
                    edits.len(),
                    expected,
                    "{label}: renaming from the occurrence at {at} would rewrite only part \
                     of the variable: {edits:?}"
                );
            }
        }
    }
}
