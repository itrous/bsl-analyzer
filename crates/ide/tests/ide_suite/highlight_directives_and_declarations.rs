//! Semantic token kinds of compilation directives, extension annotations,
//! preprocessor instructions and declaration keywords.
//!
//! The assertions compare the LSP token type (`HlTag::as_str`) and modifier
//! names rather than enum variants: those strings are what an editor theme
//! sees, so they are the observable contract.

use ide::{highlight, HlRange};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

fn highlights(source: &str) -> Vec<HlRange> {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    highlight(&db, file_id).highlights
}

/// Token type and modifier names of every highlight whose text is `needle`.
fn kinds_of(source: &str, hls: &[HlRange], needle: &str) -> Vec<(&'static str, Vec<&'static str>)> {
    hls.iter()
        .filter(|hl| &source[hl.range] == needle)
        .map(|hl| (hl.tag.as_str(), hl.modifiers.as_strings()))
        .collect()
}

fn assert_kind(source: &str, hls: &[HlRange], needle: &str, ty: &str, declaration: bool) {
    let kinds = kinds_of(source, hls, needle);
    assert!(!kinds.is_empty(), "{needle:?} is not highlighted at all: {hls:?}");
    for (actual_ty, mods) in &kinds {
        assert_eq!(*actual_ty, ty, "token type of {needle:?}");
        assert_eq!(
            mods.contains(&"declaration"),
            declaration,
            "`declaration` modifier of {needle:?}: {mods:?}"
        );
    }
}

#[test]
fn compilation_directives_are_modifiers_not_decorators() {
    let source = "&НаКлиенте\nПроцедура А()\nКонецПроцедуры\n\
                  &НаСервере\nПроцедура Б()\nКонецПроцедуры\n\
                  &НаСервереБезКонтекста\nПроцедура В()\nКонецПроцедуры\n\
                  &НаКлиентеНаСервереБезКонтекста\nПроцедура Г()\nКонецПроцедуры\n\
                  &НаКлиентеНаСервере\nПроцедура Д()\nКонецПроцедуры\n";
    let hls = highlights(source);
    for directive in [
        "&НаКлиенте",
        "&НаСервере",
        "&НаСервереБезКонтекста",
        "&НаКлиентеНаСервереБезКонтекста",
        "&НаКлиентеНаСервере",
    ] {
        assert_kind(source, &hls, directive, "modifier", false);
    }
}

#[test]
fn compilation_directives_english_are_modifiers() {
    let source = "&AtClient\nProcedure A()\nEndProcedure\n\
                  &AtServer\nProcedure B()\nEndProcedure\n\
                  &AtServerNoContext\nProcedure C()\nEndProcedure\n\
                  &AtClientAtServerNoContext\nProcedure D()\nEndProcedure\n\
                  &AtClientAtServer\nProcedure E()\nEndProcedure\n";
    let hls = highlights(source);
    for directive in [
        "&AtClient",
        "&AtServer",
        "&AtServerNoContext",
        "&AtClientAtServerNoContext",
        "&AtClientAtServer",
    ] {
        assert_kind(source, &hls, directive, "modifier", false);
    }
}

#[test]
fn extension_annotations_stay_decorators() {
    let source = "&Перед(\"М1\")\nПроцедура Расш_М1()\nКонецПроцедуры\n\
                  &После(\"М2\")\nПроцедура Расш_М2()\nКонецПроцедуры\n\
                  &Вместо(\"М3\")\nПроцедура Расш_М3()\nКонецПроцедуры\n\
                  &ИзменениеИКонтроль(\"М4\")\nПроцедура Расш_М4()\nКонецПроцедуры\n\
                  &Before(\"M5\")\nProcedure Ext_M5()\nEndProcedure\n\
                  &After(\"M6\")\nProcedure Ext_M6()\nEndProcedure\n\
                  &Around(\"M7\")\nProcedure Ext_M7()\nEndProcedure\n\
                  &ChangeAndValidate(\"M8\")\nProcedure Ext_M8()\nEndProcedure\n";
    let hls = highlights(source);
    for annotation in [
        "&Перед",
        "&После",
        "&Вместо",
        "&ИзменениеИКонтроль",
        "&Before",
        "&After",
        "&Around",
        "&ChangeAndValidate",
    ] {
        assert_kind(source, &hls, annotation, "decorator", false);
    }
}

#[test]
fn directive_and_annotation_on_one_method_get_different_types() {
    let source = "&НаСервере\n&После(\"Исходный\")\nПроцедура Расш_Исходный()\nКонецПроцедуры\n";
    let hls = highlights(source);
    assert_kind(source, &hls, "&НаСервере", "modifier", false);
    assert_kind(source, &hls, "&После", "decorator", false);
}

#[test]
fn preprocessor_instructions_stay_macros_and_region_names_stay_untagged() {
    let source = "#Область Служебные\n\
                  #Если Сервер Тогда\n\
                  Процедура Служебные()\nКонецПроцедуры\n\
                  #КонецЕсли\n\
                  #КонецОбласти\n\
                  #Region Internal\n#If Server Then\n#EndIf\n#EndRegion\n";
    let hls = highlights(source);
    for instruction in [
        "#Область",
        "#Если",
        "#КонецЕсли",
        "#КонецОбласти",
        "#Region",
        "#If",
        "#EndIf",
        "#EndRegion",
    ] {
        assert_kind(source, &hls, instruction, "macro", false);
    }
    // The region label matches a procedure name; it must stay a plain label.
    let region_label = source.find("Служебные").unwrap();
    assert!(
        hls.iter().all(|hl| usize::from(hl.range.start()) != region_label),
        "region label must not be highlighted: {hls:?}"
    );
}

#[test]
fn declaration_keywords_carry_the_declaration_modifier() {
    let source = "Перем Модульная Экспорт;\n\
                  Асинх Процедура Обработать(Знач Параметр) Экспорт\n\
                  \x20   Перем Локальная;\n\
                  \x20   Если Параметр = Неопределено Тогда\n\
                  \x20       Возврат;\n\
                  \x20   КонецЕсли;\n\
                  \x20   Пока Ложь Цикл\n\
                  \x20   КонецЦикла;\n\
                  \x20   Локальная = Новый Массив;\n\
                  КонецПроцедуры\n\
                  Функция Вычислить()\n\
                  \x20   Возврат Ждать Обработать(1);\n\
                  КонецФункции\n";
    let hls = highlights(source);
    for keyword in [
        "Перем",
        "Экспорт",
        "Асинх",
        "Процедура",
        "Знач",
        "КонецПроцедуры",
        "Функция",
        "КонецФункции",
    ] {
        assert_kind(source, &hls, keyword, "keyword", true);
    }
    for keyword in
        ["Если", "Тогда", "Возврат", "КонецЕсли", "Пока", "Цикл", "КонецЦикла", "Новый", "Ждать"]
    {
        assert_kind(source, &hls, keyword, "keyword", false);
    }
}

#[test]
fn declaration_keywords_english_carry_the_declaration_modifier() {
    let source = "Var ModuleVar Export;\n\
                  Async Procedure Handle(Val Param) Export\n\
                  \x20   Var Local;\n\
                  \x20   If Param = Undefined Then\n\
                  \x20       Return;\n\
                  \x20   EndIf;\n\
                  EndProcedure\n\
                  Function Compute()\n\
                  \x20   Return 1;\n\
                  EndFunction\n";
    let hls = highlights(source);
    for keyword in
        ["Var", "Export", "Async", "Procedure", "Val", "EndProcedure", "Function", "EndFunction"]
    {
        assert_kind(source, &hls, keyword, "keyword", true);
    }
    for keyword in ["If", "Then", "Return", "EndIf"] {
        assert_kind(source, &hls, keyword, "keyword", false);
    }
}

#[test]
fn declaration_keyword_used_as_member_name_is_not_a_declaration() {
    let source = "А = Б.Функция;\n\
                  А = Б.Экспорт;\n\
                  А = Б.Перем;\n\
                  А = Б.Знач;\n\
                  А = Б.Асинх;\n\
                  Б.КонецФункции();\n\
                  А = Б.Procedure;\n";
    let hls = highlights(source);
    for member in ["Функция", "Экспорт", "Перем", "Знач", "Асинх", "КонецФункции", "Procedure"]
    {
        let kinds = kinds_of(source, &hls, member);
        assert!(!kinds.is_empty(), "{member:?} is not highlighted at all: {hls:?}");
        for (_, mods) in kinds {
            assert!(!mods.contains(&"declaration"), "{member:?} after `.`: {mods:?}");
        }
    }
}
