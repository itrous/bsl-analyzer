//! Норма привязки диапазонов ошибок разбора: куда указывает диагностика.
//!
//! Норма объявлена в `docs/architecture/adr/ADR-03-error-range-attribution.md`
//! и держится устройством `Sink`. Здесь она проверяется снаружи, на публичном
//! API, потому что именно эти диапазоны уходят пользователю: `parse().errors()`
//! попадает в LSP-диагностику напрямую, а для запросов — ещё и проекцией в
//! текст литерала.
//!
//! **Что этот файл ловит, а что нет — установлено мутантами, а не предположено.**
//!
//! Ловит соответствие «вид восстановления → форма диапазона» из таблицы ADR.
//! Реализация, выдавшая на `MissingToken` непустой диапазон, даёт 32 нарушения;
//! схлопнувшая диапазон `BumpToken` в точку на начале того же слова — 12.
//!
//! Форму держит только точное условие на вид. Разрешение «полный диапазон ИЛИ
//! пустой на начале слова», написанное здесь ради `Custom`, делало ветвь
//! `BumpToken` слепой: мутант со схлопыванием проходил корпус зелёным.
//!
//! НЕ ловит выбор смещения среди границ значимых токенов. Три мутанта —
//! смещение у лексемы под курсором без нормализации, `BumpToken` на предыдущей
//! лексеме потока вместо последнего слова, нормализация назад вместо вперёд —
//! проходят корпус зелёными. Причина не в слабости утверждения, а в том, что
//! грамматика сама снимает тривию раньше, чем требует токен: на 6 000
//! обрезанных реальных файлов, давших 12 718 ошибок, курсор не стоял на тривии
//! ни разу. Входа, на котором разница видна, в корпусе нет и быть не может.
//!
//! Поэтому направление держат синтетические контроли в модуле тестов
//! `crates/parser/src/sink.rs` — по одному на каждую ветвь расчёта, у которой
//! выбор смещения есть. Им нужен событийный вектор с ошибкой на позиции
//! пробела, а `Event` и `Sink` крейт наружу не отдаёт. Каждый был увиден
//! падающим на подмене нормализации смещением лексемы под курсором.

use std::collections::HashSet;

const MODULE: &str = include_str!("../fixtures/Module.bsl");
const QUERY: &str = include_str!("../fixtures/sdbl_independent_package.sdbl");

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Bsl,
    Sdbl,
}

/// Диапазоны значимых токенов: их начала, концы и полные пары.
///
/// Начала и концы держатся раздельно намеренно. Множество «любая граница»
/// проверять бесполезно: прогон тривии ВСЕГДА начинается там, где кончился
/// предыдущий значимый токен, поэтому смещение, уехавшее в промежуток, такому
/// свойству удовлетворяет и нарушение остаётся невидимым. Норма ADR-03
/// направленная, и проверяться должна направленно: о пропущенном токене
/// сообщают на НАЧАЛЕ следующего слова.
struct Significant {
    starts: HashSet<u32>,
    ranges: HashSet<(u32, u32)>,
    len: u32,
}

impl Significant {
    fn of(text: &str, lang: Lang) -> Self {
        let tokens: Vec<(usize, usize)> = match lang {
            Lang::Bsl => lexer::tokenize(text)
                .into_iter()
                .filter(|t| !t.kind.is_trivia())
                .map(|t| (t.offset, t.text.len()))
                .collect(),
            Lang::Sdbl => lexer::sdbl::tokenize_sdbl(text)
                .into_iter()
                .filter(|t| !is_sdbl_trivia(t.kind))
                .map(|t| (t.offset, t.text.len()))
                .collect(),
        };

        let mut starts = HashSet::new();
        let mut ranges = HashSet::new();
        for (offset, len) in tokens {
            starts.insert(offset as u32);
            ranges.insert((offset as u32, offset.saturating_add(len) as u32));
        }
        Self { starts, ranges, len: text.len() as u32 }
    }

    /// Начало слова либо конец входа — то, на что указывает сообщение о том,
    /// чего в тексте нет.
    fn is_word_start(&self, offset: u32) -> bool {
        offset == self.len || self.starts.contains(&offset)
    }
}

fn is_sdbl_trivia(kind: lexer::sdbl::SdblTokenKind) -> bool {
    matches!(
        kind,
        lexer::sdbl::SdblTokenKind::Whitespace
            | lexer::sdbl::SdblTokenKind::Comment
            | lexer::sdbl::SdblTokenKind::Newline
    )
}

fn errors_of(text: &str, lang: Lang) -> Vec<(parser_error::RecoveryKind, u32, u32)> {
    let parse = match lang {
        Lang::Bsl => parser::parse(text),
        Lang::Sdbl => parser::parse_sdbl(text),
    };
    parse
        .errors()
        .iter()
        .map(|e| {
            (e.structured().recovery(), u32::from(e.range().start()), u32::from(e.range().end()))
        })
        .collect()
}

/// Обрезка входа по границе символа: даёт много ошибок разбора из фикстуры,
/// не заводя в репозиторий отдельного корпуса битых файлов.
fn cut(text: &str, num: usize, den: usize) -> &str {
    let mut at = text.len() * num / den;
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    &text[..at]
}

const BROKEN_BSL: &[&str] = &[
    "Процедура П()\n\tА = Б.\nКонецПроцедуры\n",
    "Процедура П(\nКонецПроцедуры\n",
    "Процедура П()\n\tФ( , );\nКонецПроцедуры\n",
    "Процедура П()\n\tА = Б + ;\nКонецПроцедуры\n",
    "Процедура П()\n\tЕсли А \n\t\tБ = 1;\n\tКонецЕсли;\nКонецПроцедуры\n",
    "Процедура П()\n\tА = Новый ;\nКонецПроцедуры\n",
    "#Вставка\n\tА = 1;\n",
    "Процедура Тест(   ",
    "Перем   ;\n",
    "&НаКлиенте   \n",
    // Ошибка, потраченная на слово: ключевое слово там, где ждали выражение.
    // Такие входы нужны отдельно — обрезки фикстур дают почти одни сообщения о
    // пропущенном токене, и ветвь `BumpToken` остаётся без веса.
    "Процедура П()\n\tА = Возврат;\nКонецПроцедуры\n",
    "Процедура П()\n\tЕсли Тогда Б = 1; КонецЕсли;\nКонецПроцедуры\n",
    "Процедура П()\n\tДля Каждого Из КонецЦикла;\nКонецПроцедуры\n",
    "Процедура П()\n\tА = 1 КонецПроцедуры\n",
    "Процедура П()\n\tФ(1 2);\nКонецПроцедуры\n",
    "Процедура П()\n\tА = Б[1 2];\nКонецПроцедуры\n",
    "Процедура П()\n\tЕсли А Тогда Б = 1; Иначе Тогда В = 2; КонецЕсли;\nКонецПроцедуры\n",
    "Процедура П(А Б)\nКонецПроцедуры\n",
    "Функция Ф() Экспорт Экспорт\nКонецФункции\n",
    "Процедура П()\n\tПопытка А = 1; Исключение Тогда Б = 2; КонецПопытки;\nКонецПроцедуры\n",
];

const BROKEN_SDBL: &[&str] = &[
    "ВЫБРАТЬ Т.\nИЗ ИЗ Т",
    "ВЫБРАТЬ   ИЗ Справочник.Товары",
    "ВЫБРАТЬ ЕСТЬNULL( Т.А ,  \nИЗ Т",
    "ВЫБРАТЬ * ИЗ Т {ВЫБРАТЬ \n",
    "ВЫБРАТЬ * ИЗ ( ВЫБРАТЬ \n) КАК Т",
    "ВЫБРАТЬ 1 УПОРЯДОЧИТЬ ПО   \n",
    "ВЫБРАТЬ * ИЗ Т ГДЕ Т.А В ( 1 , \n",
    "ВЫБРАТЬ ВЫБОР КОГДА 1 ТОГДА \n КОНЕЦ ИЗ Т",
    "УНИЧТОЖИТЬ   \n",
    "SELECT A; ) T.\nFROM Products",
    "ВЫБРАТЬ 1 2 ИЗ Т",
    "ВЫБРАТЬ Т.А КАК КАК ИЗ Т",
    "ВЫБРАТЬ * ИЗ Т СОЕДИНЕНИЕ Т2 ПО ПО Т.А = Т2.А",
];

#[test]
fn an_error_points_where_the_norm_says() {
    let mut corpus: Vec<(String, String, Lang)> = Vec::new();

    for (num, den) in [(1, 3), (1, 2), (2, 3), (9, 10), (1, 1)] {
        corpus.push((format!("module {num}/{den}"), cut(MODULE, num, den).to_string(), Lang::Bsl));
        corpus.push((format!("query {num}/{den}"), cut(QUERY, num, den).to_string(), Lang::Sdbl));
    }
    for (i, text) in BROKEN_BSL.iter().enumerate() {
        corpus.push((format!("bsl #{i}"), (*text).to_string(), Lang::Bsl));
    }
    for (i, text) in BROKEN_SDBL.iter().enumerate() {
        corpus.push((format!("sdbl #{i}"), (*text).to_string(), Lang::Sdbl));
    }

    let mut breaches = Vec::new();
    let mut seen = Vec::new();

    for (name, text, lang) in &corpus {
        let significant = Significant::of(text, *lang);

        for (recovery, start, end) in errors_of(text, *lang) {
            seen.push(recovery);
            let ok = match recovery {
                // Пропущенного токена в тексте нет, поэтому диапазон пуст, а
                // указывает он на начало слова за промежутком.
                parser_error::RecoveryKind::MissingToken => {
                    start == end && significant.is_word_start(start)
                }
                // Спан идёт от слова до слова: обе границы — начала.
                //
                // Непустоты здесь требовать нельзя, хотя это и оставляет
                // условие слепым к схлопыванию спана в точку: восстановление,
                // не съевшее ни одного токена, даёт пустой спан законно — на
                // этом корпусе таких четыре.
                parser_error::RecoveryKind::RecoverySpan => {
                    significant.is_word_start(start) && significant.is_word_start(end)
                }
                // Ошибка потрачена на слово и показывается на нём ЦЕЛИКОМ.
                // Пустой диапазон на начале того же слова здесь не годится:
                // разрешив его, условие приняло бы регрессию, схлопывающую
                // подчёркивание в точку.
                parser_error::RecoveryKind::BumpToken => significant.ranges.contains(&(start, end)),
                // Единственный вид с двумя законными формами: обычно как
                // `BumpToken`, но при пустом дереве слова, на котором показать,
                // ещё нет, и диапазон пуст.
                parser_error::RecoveryKind::Custom => {
                    significant.ranges.contains(&(start, end))
                        || (start == end && significant.is_word_start(start))
                }
            };

            if !ok {
                breaches.push(format!("{name}: {recovery:?} {start}..{end} не по норме"));
            }
        }
    }

    // Покрытие меряется видами восстановления, а не числом ошибок: у каждого
    // своя ветвь расчёта диапазона, и корпус с одним видом оставляет две
    // другие непроверенными.
    //
    // Видов четыре, требуются три: `Custom` грамматика не производит ни на
    // каком входе — его ставят потребители, строящие `ParseError` сами
    // (`hir-def` при проекции ошибок запроса, `syntax` в запасных ветках).
    // Ту ветвь держит синтетический тест в модуле тестов стока, и другого
    // способа до неё добраться нет.
    for required in [
        parser_error::RecoveryKind::MissingToken,
        parser_error::RecoveryKind::BumpToken,
        parser_error::RecoveryKind::RecoverySpan,
    ] {
        assert!(
            seen.contains(&required),
            "корпус не дал ни одной ошибки вида {required:?}: её ветвь расчёта диапазона не проверена"
        );
    }

    assert!(
        breaches.is_empty(),
        "нарушений нормы: {}\n{}",
        breaches.len(),
        breaches.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn constructor_member_access_and_closer_typos_point_at_the_offending_token() {
    let constructor_access =
        "Функция Тест()\n    Возврат Новый Файл(\"a\").ПолноеИмя;\nКонецФункции";
    let parse = parser::parse(constructor_access);
    let dot = constructor_access.find(".ПолноеИмя").unwrap();
    let error = parse.errors().first().expect("constructor member access is invalid");
    assert_eq!(parse.errors().len(), 1, "unexpected recovery cascade: {:?}", parse.errors());
    assert_eq!(usize::from(error.range().start()), dot);
    assert_eq!(usize::from(error.range().end()), dot + 1);

    let method_access = "Процедура Тест()\n    Если Новый Файл(\"a\").Существует() Тогда\n    КонецЕсли;\nКонецПроцедуры";
    let parse = parser::parse(method_access);
    let dot = method_access.find(".Существует").unwrap();
    let error = parse.errors().first().expect("constructor method access is invalid");
    assert_eq!(parse.errors().len(), 1, "unexpected recovery cascade: {:?}", parse.errors());
    assert_eq!(usize::from(error.range().start()), dot);
    assert_eq!(usize::from(error.range().end()), dot + 1);

    let typo = "Процедура Тест()\n    Если Истина Тогда\n    КонецЕслли;\nКонецПроцедуры";
    let parse = parser::parse(typo);
    let typo_start = typo.find("КонецЕслли").unwrap();
    let error = parse.errors().first().expect("misspelled block closer is invalid");
    assert_eq!(parse.errors().len(), 1, "typo must not trigger a missing-closer cascade");
    assert_eq!(
        &typo[usize::from(error.range().start())..usize::from(error.range().end())],
        "КонецЕслли"
    );
    assert_eq!(usize::from(error.range().start()), typo_start);
}
