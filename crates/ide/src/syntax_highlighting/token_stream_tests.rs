//! Golden snapshots of the full highlight token stream — byte range, tag, and
//! modifiers for every emitted token. They pin the exact observable output of
//! `highlight()` so traversal or resolution rewrites can prove they preserve
//! behavior token-for-token, which per-tag assertions in the sibling tests
//! cannot.

use expect_test::{expect, Expect};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

use super::{highlight, HlMod, HlRange};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn create_db_with_file(source: &str) -> (RootDatabaseImpl, FileId) {
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

fn render_modifiers(modifiers: HlMod) -> String {
    let mut parts = Vec::new();
    if modifiers.contains(HlMod::EXPORT) {
        parts.push("EXPORT");
    }
    if modifiers.contains(HlMod::DEPRECATED) {
        parts.push("DEPRECATED");
    }
    if modifiers.contains(HlMod::ASYNC) {
        parts.push("ASYNC");
    }
    if modifiers.contains(HlMod::DECLARATION) {
        parts.push("DECLARATION");
    }
    if modifiers.contains(HlMod::DEFINITION) {
        parts.push("DEFINITION");
    }
    if modifiers.contains(HlMod::DOCUMENTATION) {
        parts.push("DOCUMENTATION");
    }
    parts.join("+")
}

fn render_token_stream(code: &str, highlights: &[HlRange]) -> String {
    let mut out = String::new();
    for hl in highlights {
        let start: usize = hl.range.start().into();
        let end: usize = hl.range.end().into();
        let text = &code[start..end];
        // Comments and literals can span lines; keep every entry one line.
        let text = text.replace('\n', "\\n");
        let mods = render_modifiers(hl.modifiers);
        if mods.is_empty() {
            out.push_str(&format!("{start}..{end} {:?} {text:?}\n", hl.tag));
        } else {
            out.push_str(&format!("{start}..{end} {:?} [{mods}] {text:?}\n", hl.tag));
        }
    }
    out
}

fn check_token_stream(code: &str, expect: Expect) {
    let (db, file_id) = create_db_with_file(code);
    let result = highlight(&db, file_id);
    expect.assert_eq(&render_token_stream(code, &result.highlights));
}

fn check_token_stream_with_config(code: &str, expect: Expect) {
    let (mut db, file_id) = create_db_with_file(code);
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);
    let result = highlight(&db, file_id);
    expect.assert_eq(&render_token_stream(code, &result.highlights));
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn module_shape_token_stream() {
    // Module-level variables and code, preprocessor regions and branches,
    // annotations, comments, explicit and implicit locals, parameters with
    // defaults, builtin vs user calls, case-insensitive reuse — every token
    // class the tree walker classifies outside of SDBL literals.
    let code = r#"#Область Интерфейс
Перем МодульнаяПеременная Экспорт; // модульная
Перем ЛокальнаяМодульная;

&НаСервере
Процедура Обработать(Параметр1, Параметр2 = 10) Экспорт
    Перем Явная;
    Явная = Параметр1 + параметр2;
    Неявная = СокрЛП(Явная);
    МодульнаяПеременная = Неявная;
    Обработать(Явная, 2);
    Сообщить("готово");
КонецПроцедуры

Функция Вычислить()
    #Если Сервер Тогда
    Возврат Истина;
    #Иначе
    Возврат Ложь;
    #КонецЕсли
КонецФункции
#КонецОбласти

ЛокальнаяМодульная = Вычислить();
"#;
    check_token_stream(
        code,
        expect![[r##"
            0..15 Preprocessor "#Область"
            35..45 Keyword "Перем"
            46..84 Variable [EXPORT+DECLARATION] "МодульнаяПеременная"
            85..99 Keyword "Экспорт"
            101..122 Comment "// модульная"
            123..133 Keyword "Перем"
            134..170 Variable [DECLARATION] "ЛокальнаяМодульная"
            173..192 Annotation "&НаСервере"
            193..211 Keyword "Процедура"
            212..232 Procedure [EXPORT+DEFINITION] "Обработать"
            233..250 Parameter [DECLARATION] "Параметр1"
            252..269 Parameter [DECLARATION] "Параметр2"
            270..271 Operator "="
            272..274 NumberLiteral "10"
            276..290 Keyword "Экспорт"
            295..305 Keyword "Перем"
            306..316 Variable [DECLARATION] "Явная"
            322..332 Variable "Явная"
            333..334 Operator "="
            335..352 Parameter "Параметр1"
            353..354 Operator "+"
            355..372 Parameter "параметр2"
            378..392 Variable [DECLARATION] "Неявная"
            393..394 Operator "="
            395..407 BuiltinFunction [EXPORT] "СокрЛП"
            408..418 Variable "Явная"
            425..463 Variable [EXPORT] "МодульнаяПеременная"
            464..465 Operator "="
            466..480 Variable "Неявная"
            486..506 Function [EXPORT] "Обработать"
            507..517 Variable "Явная"
            519..520 NumberLiteral "2"
            527..543 BuiltinFunction [EXPORT] "Сообщить"
            544..558 StringLiteral "\"готово\""
            561..589 Keyword "КонецПроцедуры"
            591..605 Keyword "Функция"
            606..624 Function [DEFINITION] "Вычислить"
            631..640 Preprocessor "#Если"
            654..664 Keyword "Тогда"
            669..683 Keyword "Возврат"
            684..696 BooleanLiteral "Истина"
            702..713 Preprocessor "#Иначе"
            718..732 Keyword "Возврат"
            733..741 BooleanLiteral "Ложь"
            747..766 Preprocessor "#КонецЕсли"
            767..791 Keyword "КонецФункции"
            792..817 Preprocessor "#КонецОбласти"
            819..855 Variable "ЛокальнаяМодульная"
            856..857 Operator "="
            858..876 Function "Вычислить"
        "##]],
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn mdo_and_typed_member_token_stream() {
    // Configuration-backed manager chains: MDO plural, metadata object name,
    // typed properties and methods resolved through inferred receiver types.
    let code = r#"Процедура Тест(Значение)
    НаборЗаписей = РегистрыСведений.РегистрСведений1.СоздатьНаборЗаписей();
    НаборЗаписей.Отбор.Справочник1.Установить(Значение);
    НаборЗаписей.Загрузить(Новый ТаблицаЗначений);
    НаборЗаписей.Записать();
КонецПроцедуры
"#;
    check_token_stream_with_config(
        code,
        expect![[r#"
            0..18 Keyword "Процедура"
            19..27 Procedure [DEFINITION] "Тест"
            28..44 Parameter [DECLARATION] "Значение"
            50..74 Variable [DECLARATION] "НаборЗаписей"
            75..76 Operator "="
            77..109 Class "РегистрыСведений"
            142..180 Function [EXPORT] "СоздатьНаборЗаписей"
            188..212 Variable "НаборЗаписей"
            213..223 Property "Отбор"
            224..245 Property "Справочник1"
            246..266 Function [EXPORT] "Установить"
            267..283 Parameter "Значение"
            290..314 Variable "НаборЗаписей"
            315..333 Function [EXPORT] "Загрузить"
            334..344 Keyword "Новый"
            382..406 Variable "НаборЗаписей"
            407..423 Function [EXPORT] "Записать"
            427..455 Keyword "КонецПроцедуры"
        "#]],
    );
}

#[test]
fn sdbl_query_token_stream() {
    // A multiline query literal: SDBL keywords, aggregate and builtin
    // functions, table names, aliases, fields, parameters, operators — the
    // sub-language stream spliced into the BSL one.
    let code = r#"Функция Выбрать(Партнер)
    Запрос = "ВЫБРАТЬ
    |    Продажи.Регистратор КАК Документ,
    |    СУММА(ЕСТЬNULL(Продажи.Сумма, 0)) КАК Итог
    |ИЗ
    |    РегистрНакопления.Продажи КАК Продажи
    |ГДЕ
    |    Продажи.Партнер = &Партнер
    |СГРУППИРОВАТЬ ПО
    |    Продажи.Регистратор";
    Возврат Запрос;
КонецФункции
"#;
    check_token_stream(
        code,
        expect![[r#"
            0..14 Keyword "Функция"
            15..29 Function [DEFINITION] "Выбрать"
            30..44 Parameter [DECLARATION] "Партнер"
            50..62 Variable [DECLARATION] "Запрос"
            63..64 Operator "="
            66..80 Keyword "ВЫБРАТЬ"
            90..104 Namespace "Продажи"
            105..127 Property "Регистратор"
            128..134 Keyword "КАК"
            135..151 EnumMember "Документ"
            162..172 Function "СУММА"
            173..185 Function "ЕСТЬNULL"
            186..200 Namespace "Продажи"
            201..211 Property "Сумма"
            217..223 Keyword "КАК"
            224..232 EnumMember "Итог"
            238..242 Keyword "ИЗ"
            252..286 UnresolvedReference "РегистрНакопления"
            287..301 UnresolvedReference "Продажи"
            302..308 Keyword "КАК"
            309..323 Namespace "Продажи"
            329..335 Keyword "ГДЕ"
            345..359 Namespace "Продажи"
            360..374 Property "Партнер"
            375..376 Operator "="
            398..424 Keyword "СГРУППИРОВАТЬ"
            425..429 Keyword "ПО"
            439..453 Namespace "Продажи"
            454..476 Property "Регистратор"
            483..497 Keyword "Возврат"
            498..510 Variable "Запрос"
            512..536 Keyword "КонецФункции"
        "#]],
    );
}

/// Every comment byte remains covered exactly once after structural token splitting.
#[test]
fn documentation_token_stream() {
    check_token_stream(
        "// Parameters:\n// Value - String - text\nProcedure Test(Value)\nEndProcedure",
        expect![[r#"
            0..3 Comment "// "
            3..14 Keyword [DOCUMENTATION] "Parameters:"
            15..18 Comment "// "
            18..23 Parameter [DOCUMENTATION] "Value"
            23..26 Comment " - "
            26..32 Type [DOCUMENTATION] "String"
            32..39 Comment " - text"
            40..49 Keyword "Procedure"
            50..54 Procedure [DEFINITION] "Test"
            55..60 Parameter [DECLARATION] "Value"
            62..74 Keyword "EndProcedure"
        "#]],
    );
}
