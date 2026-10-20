//! Единый реестр символов препроцессора: написания, допустимые в условии
//! `#Если` / `#If`, и правило сравнения их регистра.
//!
//! Реестр несёт только лексику — тождество символа и два его написания.
//! Семантика остаётся у потребителей: отображение в среды исполнения — за
//! `hir-def`, политика быстрой правки и текст диагностики — за
//! `ide-diagnostics`. Так символ добавляется в одном месте, а обязательные
//! решения по семантике вынуждает исчерпывающий `match` по
//! [`PreprocSymbolId`].
//!
//! ## Provenance
//!
//! Список выведен из раздела 4.8.1.2 «Инструкции препроцессора» руководства
//! разработчика 1С:Предприятие 8.3.27
//! (<https://its.1c.ru/db/v8327doc#bookmark:dev:TI000000164>), где он приведён
//! двуязычной таблицей целиком. Аттестация —
//! `docs/legal/bsl-clean-room-slice-b2.md`.
//!
//! Продукция «Символ препроцессора» того же раздела включает ещё `Область` и
//! `КонецОбласти`. Здесь их нет намеренно: те же слова стоят в таблице
//! инструкций, `#Если Область Тогда` смысла не имеет, а признание их
//! известными погасило бы диагностику на настоящей опечатке.
//!
//! `Linux`, `Windows` и `MacOS` реестру неизвестны, и это установленное
//! решение, а не пропуск: раздел 4.8.1.2 их не определяет, а другого
//! нормативного источника для них не установлено (аттестация B2 фиксирует
//! ноль вхождений в разделе и ни одного вхождения в условиях препроцессора
//! замеренного корпуса конфигураций). Все потребители получают на них один и
//! тот же ответ именно потому, что отвечает один реестр.

use stdx::case::eq_case_folded;

/// Тождество одного двуязычного символа препроцессора.
///
/// Варианты — словарь: написания даёт `match` по варианту, поэтому новый
/// символ не компилируется, пока ему не назначены оба написания, а
/// [`PreprocSymbolId::ALL`] обходят табличные тесты каждого потребителя.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PreprocSymbolId {
    Server,
    AtServer,
    Client,
    AtClient,
    ThinClient,
    MobileClient,
    WebClient,
    ExternalConnection,
    ThickClientManagedApplication,
    ThickClientOrdinaryApplication,
    MobileAppClient,
    MobileAppServer,
    MobileStandaloneServer,
}

impl PreprocSymbolId {
    /// Все символы таблицы 4.8.1.2, в порядке источника.
    pub const ALL: &'static [PreprocSymbolId] = &[
        PreprocSymbolId::Server,
        PreprocSymbolId::AtServer,
        PreprocSymbolId::Client,
        PreprocSymbolId::AtClient,
        PreprocSymbolId::ThinClient,
        PreprocSymbolId::MobileClient,
        PreprocSymbolId::WebClient,
        PreprocSymbolId::ExternalConnection,
        PreprocSymbolId::ThickClientManagedApplication,
        PreprocSymbolId::ThickClientOrdinaryApplication,
        PreprocSymbolId::MobileAppClient,
        PreprocSymbolId::MobileAppServer,
        PreprocSymbolId::MobileStandaloneServer,
    ];

    /// Каноническое русское написание.
    pub const fn ru(self) -> &'static str {
        match self {
            PreprocSymbolId::Server => "Сервер",
            PreprocSymbolId::AtServer => "НаСервере",
            PreprocSymbolId::Client => "Клиент",
            PreprocSymbolId::AtClient => "НаКлиенте",
            PreprocSymbolId::ThinClient => "ТонкийКлиент",
            PreprocSymbolId::MobileClient => "МобильныйКлиент",
            PreprocSymbolId::WebClient => "ВебКлиент",
            PreprocSymbolId::ExternalConnection => "ВнешнееСоединение",
            PreprocSymbolId::ThickClientManagedApplication => "ТолстыйКлиентУправляемоеПриложение",
            PreprocSymbolId::ThickClientOrdinaryApplication => "ТолстыйКлиентОбычноеПриложение",
            PreprocSymbolId::MobileAppClient => "МобильноеПриложениеКлиент",
            PreprocSymbolId::MobileAppServer => "МобильноеПриложениеСервер",
            PreprocSymbolId::MobileStandaloneServer => "МобильныйАвтономныйСервер",
        }
    }

    /// Каноническое английское написание.
    pub const fn en(self) -> &'static str {
        match self {
            PreprocSymbolId::Server => "Server",
            PreprocSymbolId::AtServer => "AtServer",
            PreprocSymbolId::Client => "Client",
            PreprocSymbolId::AtClient => "AtClient",
            PreprocSymbolId::ThinClient => "ThinClient",
            PreprocSymbolId::MobileClient => "MobileClient",
            PreprocSymbolId::WebClient => "WebClient",
            PreprocSymbolId::ExternalConnection => "ExternalConnection",
            PreprocSymbolId::ThickClientManagedApplication => "ThickClientManagedApplication",
            PreprocSymbolId::ThickClientOrdinaryApplication => "ThickClientOrdinaryApplication",
            PreprocSymbolId::MobileAppClient => "MobileAppClient",
            PreprocSymbolId::MobileAppServer => "MobileAppServer",
            PreprocSymbolId::MobileStandaloneServer => "MobileStandaloneServer",
        }
    }

    /// Оба канонических написания, русское первым.
    pub const fn spellings(self) -> [&'static str; 2] {
        [self.ru(), self.en()]
    }
}

/// Символ по любому его написанию; регистр сравнивается по правилу лексера.
pub fn lookup(text: &str) -> Option<PreprocSymbolId> {
    PreprocSymbolId::ALL
        .iter()
        .copied()
        .find(|id| eq_case_folded(text, id.ru()) || eq_case_folded(text, id.en()))
}

/// Известно ли написание как символ препроцессора.
pub fn is_known(text: &str) -> bool {
    lookup(text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Тринадцать двуязычных пар раздела 4.8.1.2 руководства разработчика.
    ///
    /// Живут в тесте, а не в проверяемом коде: список, сверяемый сам с собой,
    /// сверкой не является.
    const SECTION_4_8_1_2: &[(&str, &str)] = &[
        ("Сервер", "Server"),
        ("НаСервере", "AtServer"),
        ("Клиент", "Client"),
        ("НаКлиенте", "AtClient"),
        ("ТонкийКлиент", "ThinClient"),
        ("МобильныйКлиент", "MobileClient"),
        ("ВебКлиент", "WebClient"),
        ("ВнешнееСоединение", "ExternalConnection"),
        ("ТолстыйКлиентУправляемоеПриложение", "ThickClientManagedApplication"),
        ("ТолстыйКлиентОбычноеПриложение", "ThickClientOrdinaryApplication"),
        ("МобильноеПриложениеКлиент", "MobileAppClient"),
        ("МобильноеПриложениеСервер", "MobileAppServer"),
        ("МобильныйАвтономныйСервер", "MobileStandaloneServer"),
    ];

    /// Написания реестра равны таблице источника — в обе стороны.
    ///
    /// Проверка «каждое написание источника известно» одна пропустила бы
    /// лишнее: ровно так три написания без источника и прожили в списке.
    /// Поэтому рядом стоит счёт.
    #[test]
    fn the_registry_equals_the_source_table() {
        for (ru, en) in SECTION_4_8_1_2 {
            let by_ru =
                lookup(ru).unwrap_or_else(|| panic!("{ru}: написание источника не признано"));
            let by_en =
                lookup(en).unwrap_or_else(|| panic!("{en}: написание источника не признано"));
            assert_eq!(by_ru, by_en, "{ru}/{en}: написания источника дали разные символы");
            assert_eq!(
                by_ru.spellings(),
                [*ru, *en],
                "{ru}/{en}: канонические написания разошлись"
            );
        }
        assert_eq!(
            PreprocSymbolId::ALL.len(),
            SECTION_4_8_1_2.len(),
            "в реестре есть символы сверх таблицы 4.8.1.2"
        );
    }

    #[test]
    fn every_variant_is_listed_once_and_spelled_uniquely() {
        let mut seen: Vec<PreprocSymbolId> = PreprocSymbolId::ALL.to_vec();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), PreprocSymbolId::ALL.len(), "вариант повторён в ALL");

        let mut spellings: Vec<&str> =
            PreprocSymbolId::ALL.iter().flat_map(|id| id.spellings()).collect();
        assert_eq!(spellings.len(), PreprocSymbolId::ALL.len() * 2);
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), PreprocSymbolId::ALL.len() * 2, "написание встречается дважды");
    }

    /// Регистр не значим ни в одном написании: каждое проверяется в четырёх
    /// вариантах, а не только там, где различие уже заметили.
    #[test]
    fn every_spelling_is_recognized_in_any_case() {
        for &id in PreprocSymbolId::ALL {
            for spelling in id.spellings() {
                for variant in [
                    spelling.to_string(),
                    spelling.to_uppercase(),
                    spelling.to_lowercase(),
                    alternating_case(spelling),
                ] {
                    assert_eq!(
                        lookup(&variant),
                        Some(id),
                        "{variant:?}: написание {spelling:?} не опознано"
                    );
                }
            }
        }
    }

    fn alternating_case(text: &str) -> String {
        text.chars()
            .enumerate()
            .flat_map(|(i, c)| {
                if i % 2 == 0 {
                    c.to_uppercase().collect::<Vec<_>>()
                } else {
                    c.to_lowercase().collect()
                }
            })
            .collect()
    }

    /// Край сложения регистра: лексер принимает `ſ` за `s`, а `ᲃ` за `с`;
    /// реестр обязан отвечать так же, иначе он расходится с токенизатором.
    #[test]
    fn the_unicode_fold_edge_is_recognized() {
        assert_eq!(lookup("\u{17F}erver"), Some(PreprocSymbolId::Server));
        assert_eq!(
            lookup("MobileStandalone\u{17F}erver"),
            Some(PreprocSymbolId::MobileStandaloneServer)
        );
        assert_eq!(lookup("Thi\u{17F}Client"), None, "`ſ` за `n` не сходит");
        assert_eq!(lookup("\u{1C83}ервер"), Some(PreprocSymbolId::Server));
        assert_eq!(
            lookup("Thic\u{212A}ClientManagedApplication"),
            Some(PreprocSymbolId::ThickClientManagedApplication)
        );
        // Турецкая точечная пара своего класса не покидает.
        assert_eq!(lookup("Cl\u{131}ent"), None);
        assert_eq!(lookup("Cl\u{130}ent"), None);
    }

    #[test]
    fn spellings_outside_the_table_are_unknown() {
        for text in [
            "Нечто",
            "_",
            "",
            "Unknown",
            "Test",
            "Линукс",
            // ОС-символы: раздел 4.8.1.2 их не определяет, и ответ реестра
            // на них один для всех потребителей.
            "Linux",
            "Windows",
            "MacOS",
            // Продукция раздела знает эти слова, но как инструкции.
            "Область",
            "КонецОбласти",
            "Region",
            // Похожие, но не те написания.
            "Сервера",
            "Server1",
            "НаСервер",
        ] {
            assert!(!is_known(text), "{text:?} опознан как символ препроцессора");
        }
    }

    /// Алфавит реестра целиком лежит в латинице и русском блоке — том самом,
    /// на котором `crates/lexer/tests/lexer_suite/case_fold_matches_lexer.rs` сверяет
    /// правило сравнения с `(?i)` лексера. Буква вне этого алфавита оставила
    /// бы сверку без покрытия молча.
    #[test]
    fn the_registry_alphabet_is_the_one_the_lexer_check_covers() {
        for &id in PreprocSymbolId::ALL {
            for spelling in id.spellings() {
                for c in spelling.chars() {
                    let covered = c.is_ascii_alphabetic()
                        || ('а'..='я').contains(&c)
                        || ('А'..='Я').contains(&c)
                        || c == 'ё'
                        || c == 'Ё';
                    assert!(covered, "{spelling:?}: буква {c:?} вне сверенного алфавита");
                }
            }
        }
    }
}
