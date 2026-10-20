use ide::{highlight, HlTag};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use syntax::{TextRange, TextSize};
use vfs::{FileId, FileSet, VfsPath};

fn setup(source: &str) -> (RootDatabaseImpl, FileId) {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    (db, file_id)
}

fn text_range(source: &str, needle: &str) -> TextRange {
    let start = source.find(needle).unwrap_or_else(|| panic!("missing {needle:?}"));
    TextRange::at(TextSize::from(start as u32), TextSize::from(needle.len() as u32))
}

#[test]
fn sdbl_highlighting_maps_inserted_tokens_and_ignores_deleted_tokens() {
    let source = r#"Функция Тест()
    Возврат Новый Запрос("ВЫБРАТЬ 1 КАК База,
    #Удаление
    |2 КАК Удаленное,
    #КонецУдаления
    #Вставка
    |3 КАК Вставленное,
    #КонецВставки
    |4 КАК Последнее").Выполнить();
КонецФункции"#;
    let (db, file_id) = setup(source);
    let result = highlight(&db, file_id);
    let inserted = text_range(source, "Вставленное");
    let deleted = text_range(source, "Удаленное");

    assert!(
        result.highlights.iter().any(|item| {
            item.range == inserted && matches!(item.tag, HlTag::EnumMember | HlTag::Property)
        }),
        "inserted query alias must be highlighted at its source range: {:?}",
        result.highlights
    );
    assert!(
        result.highlights.iter().all(|item| item.range.intersect(deleted).is_none()),
        "deleted query text must have no SDBL highlight: {:?}",
        result.highlights
    );
    assert!(
        result.highlights.iter().all(|item| !source[item.range].contains("#Вставка")
            && !source[item.range].contains("#Удаление")),
        "query highlights must never land on directive markers"
    );
}
