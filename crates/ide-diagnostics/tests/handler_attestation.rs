//! У каждого из семнадцати SDBL-хендлеров есть вердикт о происхождении его решения.
//!
//! Предмет — диагностики запросов, которые держали крейт в Tier B до аттестации
//! нижнего слоя. Вопрос к каждой один: решение диагностики — что сообщается, где
//! и что остаётся молчанием — выведено из публичной нормы 1С, выбрано осознанно
//! с названной ценой, или не разрешено. Строка без ответа здесь не компилируется,
//! а открытая находка краснит гейт.
//!
//! Тип запрещает ровно отсутствие вердикта — и не запрещает заглушку в его
//! обосновании. Проверка заглушек ниже — backstop, а не доказательство: верную
//! причину от правдоподобной машина не отличает, это предмет ревью.
//!
//! Provenance: `docs/legal/ide-diagnostics-handler-attestation.md`.

use std::path::Path;
use std::str::FromStr;

use ide_diagnostics::{get_metadata, DiagnosticCode};

/// Что стоит за решением диагностики.
enum Verdict {
    /// Выводится из публичной нормы: раздел и утверждение, которое он делает.
    R { source: &'static str, claim: &'static str },
    /// Осознанный выбор сверх нормы или при её молчании. Нормативная часть
    /// называется отдельно; причина и цена — что теряется без решения или что
    /// оно пропускает.
    A { norm: &'static str, reason: &'static str, cost: &'static str },
    /// Никто не выбирал. Находка, а не исход: пока стоит, перенос не выполняется.
    // Не конструируется, и это условие закрытия: находка разрешается в R или A.
    // Вариант остаётся тем, во что пишется следующая находка.
    #[allow(dead_code)]
    D { what: &'static str },
}

impl Verdict {
    fn letter(&self) -> char {
        match self {
            Verdict::R { .. } => 'R',
            Verdict::A { .. } => 'A',
            Verdict::D { .. } => 'D',
        }
    }

    /// Все поля обоснования, каждое обязано быть содержательным само по себе.
    fn justifications(&self) -> Vec<(&'static str, &'static str)> {
        match self {
            Verdict::R { source, claim } => vec![("источник", source), ("утверждение", claim)],
            Verdict::A { norm, reason, cost } => {
                vec![("норма", norm), ("причина", reason), ("цена", cost)]
            }
            Verdict::D { what } => vec![("находка", what)],
        }
    }
}

struct Entry {
    code: &'static str,
    file: &'static str,
    verdict: Verdict,
    /// Где живёт анализ, на который опирается хендлер, и чем этот участок закрыт.
    lower: &'static str,
    /// Находки журнала, относящиеся к строке.
    findings: &'static [&'static str],
}

/// Исход находки журнала.
enum Closure {
    /// Не разрешена. Не конструируется, пока гейт зелёный.
    #[allow(dead_code)]
    Open,
    Closed {
        basis: &'static str,
    },
    /// Решение владельца принято, но закрытие ждёт внешнего подтверждения.
    /// Допустимо, только пока перенос не выполнен: крейт числится в Tier B.
    Awaiting {
        basis: &'static str,
        until: &'static str,
    },
}

struct Finding {
    id: &'static str,
    what: &'static str,
    closure: Closure,
}

/// Предмет на базе программы: поимённый бакет 1 сводки, код и файл хендлера.
/// Хранится здесь, а не читается из истории: гейт не зависит от глубины клона.
const SUBJECT: &[(&str, &str)] = &[
    ("AssignAliasFieldsInQuery", "assign_alias_fields_in_query"),
    ("FieldsFromJoinsWithoutIsNull", "fields_from_joins_without_is_null"),
    ("FullOuterJoinQuery", "full_outer_join_query"),
    ("IncorrectUseLikeInQuery", "incorrect_use_like_in_query"),
    ("JoinWithSubQuery", "join_with_sub_query"),
    ("JoinWithVirtualTable", "join_with_virtual_table"),
    ("LogicalOrInJoinQuerySection", "logical_or_in_join_query_section"),
    ("LogicalOrInTheWhereSectionOfQuery", "logical_or_in_the_where_section_of_query"),
    ("MultilineStringInQuery", "multiline_string_in_query"),
    ("QueryNestedFieldsByDot", "query_nested_fields_by_dot"),
    ("QueryParseError", "query_parse_error"),
    ("QueryToMissingMetadata", "query_to_missing_metadata"),
    ("RefOveruse", "ref_overuse"),
    ("SelectTopWithoutOrderBy", "select_top_without_order_by"),
    ("UnionAll", "union_all"),
    ("UsingLikeInQuery", "using_like_in_query"),
    ("VirtualTableCallWithoutParameters", "virtual_table_call_without_parameters"),
];

const CHECKLIST: &[Entry] = &[
    Entry {
        code: "AssignAliasFieldsInQuery",
        file: "assign_alias_fields_in_query",
        verdict: Verdict::A {
            norm: "v8std #437 §2, §2б: выбранному полю дать имя явно и писать его через КАК; обе ветки сообщения — поле без имени и имя без КАК — прямое следствие",
            reason: "исправление вставляет КАК перед написанным именем либо « КАК <последний идентификатор выражения>»; норма fixes не предписывает, имя для безымянного поля выбрано локально",
            cost: "для выражения без идентификатора исправления нет, а выбранное имя может не совпасть с тем, что ждёт код обработки результата — исправление помечено безопасным только как вставка, смысл имени проверяет разработчик",
        },
        lower: "F05 (только первая часть объединения задаёт имена колонок; * и Т.* не проверяются); проекция диапазона — SdblPositionMapper крейта",
        findings: &["T2", "P3"],
    },
    Entry {
        code: "FieldsFromJoinsWithoutIsNull",
        file: "fields_from_joins_without_is_null",
        verdict: Verdict::A {
            norm: "metod8dev #2653, #2614, #2516: поле необязательной стороны внешнего соединения получает NULL, защита — ЕСТЬNULL либо проверка ЕСТЬ NULL; сообщение называет оба средства и замену на внутреннее соединение",
            reason: "сохранённые неточности F02: сопоставление псевдонима свёрткой ASCII, проверка присутствия в ГДЕ по точному написанию, аргументы ЕСТЬNULL все считаются защищёнными, ГДЕ/СГРУППИРОВАТЬ/ИМЕЮЩИЕ/УПОРЯДОЧИТЬ/ПО не осматриваются; по одному сообщению на каждое незащищённое использование",
            cost: "часть небезопасных использований (в группировке, сортировке, условиях соединения) не сообщается, а кириллический псевдоним в другом регистре даёт лишнее сообщение; правило выключено по умолчанию, потому что типовой код массово соединяет таблицу с собой, где NULL невозможен",
        },
        lower: "F02 замены sdbl-hir, slice 13",
        findings: &["P3"],
    },
    Entry {
        code: "FullOuterJoinQuery",
        file: "full_outer_join_query",
        verdict: Verdict::A {
            norm: "v8std #435 §1.1: ПОЛНОЕ ВНЕШНЕЕ СОЕДИНЕНИЕ не использовать, особенно несколько; сообщение предлагает переписать через объединение и левые соединения, как советует стандарт",
            reason: "сообщается каждое полное соединение; исключение стандарта для запросов, которые без него переписать нельзя, не моделируется — признак такой невозможности синтаксису не виден",
            cost: "оправданное полное соединение тоже получает сообщение, и подавлять его приходится вручную",
        },
        lower: "F07 (принят решением владельца как заново выведенный)",
        findings: &["P3"],
    },
    Entry {
        code: "IncorrectUseLikeInQuery",
        file: "incorrect_use_like_in_query",
        verdict: Verdict::A {
            norm: "v8std #726: шаблоном ПОДОБНО может быть только строковый литерал или параметр запроса; поле таблицы шаблоном быть не может",
            reason: "отмечается только шаблон, который является ссылкой на поле; вычисляемый шаблон и конкатенация, которые стандарт тоже запрещает, не отмечаются — известное расхождение github#305, оставленное без изменения",
            cost: "запрещённые стандартом вычисляемые шаблоны проходят молча; сообщение общее и не объясняет, что именно запрещено",
        },
        lower: "sdbl-hir lower_like_expr / is_column_ref_pattern; sdbl-hir, Tier A",
        findings: &["P3"],
    },
    Entry {
        code: "JoinWithSubQuery",
        file: "join_with_sub_query",
        verdict: Verdict::R {
            source: "v8std #655 §1.1",
            claim: "в запросах не следует соединять с вложенными запросами, «не важно с какой стороны соединения»: сообщается вложенный запрос в присоединяемой части и первый источник, если за ним следуют соединения; единственный источник и источники через запятую не соединены и не сообщаются",
        },
        lower: "F04; диапазон — сам вложенный запрос внутри скобок",
        findings: &["P3"],
    },
    Entry {
        code: "JoinWithVirtualTable",
        file: "join_with_virtual_table",
        verdict: Verdict::A {
            norm: "v8std #655 §2; pubqlang «Соединения с виртуальными таблицами»: виртуальную таблицу по любую сторону соединения не соединять, а выносить во временную таблицу",
            reason: "виртуальная таблица распознаётся по последней части имени через общий словарь видов, тот же, что строит поля; сохранённая неточность F03 — однокомпонентное имя, совпадающее с видом виртуальной таблицы, тоже считается ею",
            cost: "временная таблица с именем вроде «Остатки» в соединении даёт ложное сообщение",
        },
        lower: "F03 замены sdbl-hir, slice 13",
        findings: &["T4", "P3"],
    },
    Entry {
        code: "LogicalOrInJoinQuerySection",
        file: "logical_or_in_join_query_section",
        verdict: Verdict::A {
            norm: "v8std #658 §2.1: ИЛИ в условии соединения мешает индексу, кроме ИЛИ над одним полем, которое СУБД сводит к В; новое сообщение называет это и оговаривает, что перестройка через ОБЪЕДИНИТЬ ВСЕ допустима, только если сохраняет результат, как и сам стандарт",
            reason: "сохранённые неточности F01: «одно поле» сравнивается точным текстом узла, операции сравнения кроме = исключение сохраняют, вложенные запросы условия судятся вместе с ним",
            cost: "Х.А и х.а считаются разными полями — лишнее сообщение, пропуска нет; ИЛИ над одним полем с неравенствами остаётся без сообщения",
        },
        lower: "F01; текст сообщения заменён решением владельца (P1)",
        findings: &["P1", "P3"],
    },
    Entry {
        code: "LogicalOrInTheWhereSectionOfQuery",
        file: "logical_or_in_the_where_section_of_query",
        verdict: Verdict::A {
            norm: "v8std #658 §1–§2: ИЛИ в условии отбора может лишить СУБД индексного поиска; рекомендуемая перестройка — части, объединённые через ОБЪЕДИНИТЬ ВСЕ",
            reason: "сообщается каждое ИЛИ собственного условия ГДЕ запроса; исключение §2.1 для ИЛИ над одним полем здесь не применяется (сохранённое решение F06), скалярные вложенные запросы не осматриваются",
            cost: "допустимое ИЛИ над одним полем и основные/дополнительные условия §1 не различаются — каждая дизъюнкция становится точкой ревью; документация правила говорит об этом прямо",
        },
        lower: "F06 замены sdbl-hir, slice 13",
        findings: &["P3"],
    },
    Entry {
        code: "MultilineStringInQuery",
        file: "multiline_string_in_query",
        verdict: Verdict::A {
            norm: "прямой нормы нет; синтаксис строкового литерала языка запросов (mini-spec выражений, §«String literal — single vs multi»): литерал продолжается до парной кавычки, удвоенная кавычка — экранирование",
            reason: "HIR предлагает каждый литерал с содержимым (F08), а хендлер оставляет сообщение, только если в тексте запроса действительно есть литерал, продолжающийся на следующей строке: собственный обход байтов с учётом удвоенной кавычки",
            cost: "намеренный многострочный литерал тоже сообщается; правило — эвристика ошибки экранирования, а не требование языка",
        },
        lower: "F08 и разделение слоёв: огибающая BSL снимается извлечением запроса, решение о переносе строки принимает хендлер",
        findings: &["T6", "P3"],
    },
    Entry {
        code: "QueryNestedFieldsByDot",
        file: "query_nested_fields_by_dot",
        verdict: Verdict::A {
            norm: "v8std #654 §1.1–1.2; pubqlang 152, 159: разыменование ссылочного поля через точку порождает неявное соединение",
            reason: "путь от источника запроса сообщается, если его длина не меньше настройки minPathDepth (по умолчанию 3, локальный порог, норма числа не даёт); путь внутри параметров виртуальной таблицы и цепочка после ВЫРАЗИТЬ сообщаются без длины и порогу не подчиняются; ключ настройки принят как совместимое поведение (P4)",
            cost: "двухчастный путь T.Реквизит.Х не сообщается по умолчанию; путь во вложенном запросе сообщается дважды, как и до замены F10",
        },
        lower: "F10 замены sdbl-hir, slice 13",
        findings: &["P3", "P4"],
    },
    Entry {
        code: "QueryParseError",
        file: "query_parse_error",
        verdict: Verdict::A {
            norm: "синтаксис языка запросов (pubqlang; mini-spec SELECT и выражений): текст, который не разбирается, ошибочен",
            reason: "хендлер проецирует диапазоны и тексты ошибок разбора (error_ranges_in_bsl, format_ru) в координаты BSL; что считать текстом запроса, решает извлечение запросов крейта, где ставить ошибку — восстановление парсера",
            cost: "строка, похожая на запрос, но им не являющаяся, может получить сообщение; запрос, собранный конкатенацией по частям, не проверяется",
        },
        lower: "parser (SDBL-грамматика, Tier A, программы SDBL-слайсов) и извлечение запросов ide-diagnostics",
        findings: &["T8", "P3"],
    },
    Entry {
        code: "QueryToMissingMetadata",
        file: "query_to_missing_metadata",
        verdict: Verdict::A {
            norm: "прямой нормы нет; таблица запроса называет объект метаданных конфигурации, и путь, который ни во что не разрешается, ошибочен",
            reason: "проверяются пути стандартных объектов и внешних источников данных против метаданных проекта; без метаданных правило молчит; новое сообщение говорит, что источник не разрешается в таблицу метаданных, — это верно и для отсутствующего объекта, и для существующего, который таблицей запроса не является, например общего модуля (P2)",
            cost: "без выгрузки конфигурации ошибки не видны; в отличие от соседей правило — проверка корректности, а не стандарта",
        },
        lower: "sdbl-hir, Tier A (разрешение путей таблиц) и bsl-metadata",
        findings: &["T7", "P2", "P3"],
    },
    Entry {
        code: "RefOveruse",
        file: "ref_overuse",
        verdict: Verdict::A {
            norm: "v8std #654; pubqlang 158 «Исключить получение поля Ссылка через точку»: обращение к .Ссылка у поля, которое уже ссылка, порождает лишнее соединение",
            reason: "сообщается путь, где перед .Ссылка стоит хотя бы одно поле, а тип этой цепочки по метаданным — ссылка; Псевдоним.Ссылка и путь от имени объекта метаданных без поля не сообщаются",
            cost: "без метаданных тип цепочки неизвестен и правило молчит; другие дорогие разыменования стандарта сюда не входят",
        },
        lower: "sdbl-hir, Tier A (check_column_ref_for_ref_overuse, разрешение типов полей)",
        findings: &["T9", "P3"],
    },
    Entry {
        code: "SelectTopWithoutOrderBy",
        file: "select_top_without_order_by",
        verdict: Verdict::A {
            norm: "v8std #412: ПЕРВЫЕ без УПОРЯДОЧИТЬ ПО даёт недетерминированный набор строк, кроме случая, когда нужна одна строка и порядок не важен",
            reason: "ПЕРВЫЕ в части объединения сообщается всегда; ПЕРВЫЕ N > 1 без сортировки — всегда; ПЕРВЫЕ 1 и 0 — только при выключенной настройке skipSelectTopOne (по умолчанию включена) и без ГДЕ: наличие условия принято признаком «нужна одна подходящая строка»",
            cost: "ПЕРВЫЕ 1 без условия и сортировки по умолчанию молчит; условие ГДЕ может и не делать строку единственной",
        },
        lower: "sdbl-hir, Tier A (сбор признаков ПЕРВЫЕ, объединения и ГДЕ при понижении запроса)",
        findings: &["T9", "P3", "P4"],
    },
    Entry {
        code: "UnionAll",
        file: "union_all",
        verdict: Verdict::A {
            norm: "v8std #434: ОБЪЕДИНИТЬ без ВСЕ удаляет дубликаты ценой лишней обработки и допустим, только когда удаление дубликатов нужно",
            reason: "сообщается каждое ОБЪЕДИНИТЬ / UNION без ВСЕ / ALL: нужно ли удаление дубликатов, из текста не выводится",
            cost: "законное ОБЪЕДИНИТЬ тоже получает сообщение; документация называет правило консервативным",
        },
        lower: "sdbl-hir, Tier A (обход частей объединения при понижении)",
        findings: &["P3"],
    },
    Entry {
        code: "UsingLikeInQuery",
        file: "using_like_in_query",
        verdict: Verdict::A {
            norm: "v8std #726: результат ПОДОБНО зависит от СУБД, допустимы только литерал и параметр шаблоном; руководство разработчика — операция ПОДОБНО",
            reason: "сообщается каждое использование ПОДОБНО, включая НЕ ПОДОБНО, без разбора формы шаблона; правило выключено по умолчанию",
            cost: "допустимое по стандарту использование тоже сообщается; для разбора формы есть IncorrectUseLikeInQuery",
        },
        lower: "sdbl-hir lower_like_expr; sdbl-hir, Tier A",
        findings: &["T3", "P3"],
    },
    Entry {
        code: "VirtualTableCallWithoutParameters",
        file: "virtual_table_call_without_parameters",
        verdict: Verdict::A {
            norm: "v8std #657, #733; metod8dev #5457: условия отбора виртуальной таблицы передавать её параметрами",
            reason: "сообщается вызов виртуальной таблицы без скобок или со скобками, где не передан ни один аргумент; заполненный хотя бы один параметр, в том числе только период, считается использованием механизма",
            cost: "отбор, вынесенный в ГДЕ при заполненном периоде, не сообщается",
        },
        lower: "sdbl-hir, Tier A (check_virtual_table_params)",
        findings: &["T9", "P3"],
    },
];

const FINDINGS: &[Finding] = &[
    Finding {
        id: "C1",
        what: "после c5e811e7 (2026-04-18) CONTRIBUTING.md принимал вклады только на условиях LGPL-3.0-or-later, а в ide-diagnostics, bsl-analyzer, parser, lexer и sdbl-hir 44 коммита восьми сторонних авторов",
        closure: Closure::Awaiting {
            basis: "решение владельца: основание — публичное объявление о приёме вкладов на условиях MIT; CONTRIBUTING исправлен; перенос ждёт подтверждений авторов по чек-листу CONTRIBUTIONS, неподтверждённое заменяется точечно",
            until: "каждая строка CONTRIBUTIONS подтверждена или заменена",
        },
    },
    Finding {
        id: "T1",
        what: "у всех двенадцати носителей вне #52 были фикстуры <Код>Diagnostic.bsl с именованием чужого проекта, перенесённые в инлайн 07d2b977",
        closure: Closure::Closed {
            basis: "отдельный аудитор вне чистой комнаты (pio run 1357) сверил sha256 строк 27 исторических и upstream-фикстур с тестами 12 носителей; тесты с материальными совпадениями заменены по решению владельца, у оставленных совпадает только идиома «Запрос = Новый Запрос;»",
        },
    },
    Finding {
        id: "T2",
        what: "AssignAliasFieldsInQuery: be23d93e и 7e58e174 называют фикстуру носителя Java test fixture",
        closure: Closure::Closed {
            basis: "тестовый материал носителя заменён целиком по процедуре #52, изъятые литералы внесены в RETIRED",
        },
    },
    Finding {
        id: "T3",
        what: "UsingLikeInQuery: вход 2 изъятого материала IncorrectUseLikeInQuery (названный источник 9e5b05e6) жил тем же байтовым блоком в носителе",
        closure: Closure::Closed {
            basis: "тестовый материал носителя заменён целиком; изъятые литералы, включая общий блок, внесены в RETIRED",
        },
    },
    Finding {
        id: "T4",
        what: "JoinWithVirtualTable: d30e0a8d — тесты с позициями, совпадающими с Java",
        closure: Closure::Closed {
            basis: "хэш-сверка: 15 совпавших строк в 4 тестах; тестовый материал носителя заменён целиком, изъятые литералы внесены в RETIRED, кроме собственного типового входа, общего с join_with_sub_query.rs",
        },
    },
    Finding {
        id: "T5",
        what: "FullOuterJoinQuery, JoinWithSubQuery, AssignAliasFieldsInQuery: 8b36bb5c — позиции диагностик совпадают с Java",
        closure: Closure::Closed {
            basis: "хэш-сверка: в full_outer_join_query.rs 29 совпавших строк — носитель заменён целиком; в join_with_sub_query.rs одна строка в одном тесте — тест заменён; AssignAliasFieldsInQuery заменён по T2",
        },
    },
    Finding {
        id: "T6",
        what: "MultilineStringInQuery: 0b367063 называет Java-реализацию правила источником",
        closure: Closure::Closed {
            basis: "детекция заменена в F08; хэш-сверка: 16 совпавших строк — тестовый материал носителя заменён целиком",
        },
    },
    Finding {
        id: "T7",
        what: "QueryToMissingMetadata: 5736244a — severity и формат сообщения сверены с Java",
        closure: Closure::Closed {
            basis: "сообщение заменено (P2), metadata приняты (P3); хэш-сверка: совпадает только идиома «Запрос = Новый Запрос;», тесты оставлены с этой записью",
        },
    },
    Finding {
        id: "T8",
        what: "QueryParseError: большая фикстура заменена 6f9d5103, у прочих входов свидетеля не было",
        closure: Closure::Closed {
            basis: "фикстура пересмотрена с красным и зелёным контролем и хэш-сверкой без совпадений; два теста с совпадениями заменены, у остальных совпадений нет",
        },
    },
    Finding {
        id: "T9",
        what: "QueryNestedFieldsByDot, RefOveruse, SelectTopWithoutOrderBy, VirtualTableCallWithoutParameters: источник не назван, есть только T1",
        closure: Closure::Closed {
            basis: "хэш-сверка: select_top_without_order_by.rs и virtual_table_call_without_parameters.rs заменены целиком, в query_nested_fields_by_dot.rs заменён совпавший тест, в ref_overuse.rs совпадает только идиома и тесты оставлены",
        },
    },
    Finding {
        id: "P1",
        what: "LogicalOrInJoinQuerySection: сообщение введено 3cb89eac «Translate messages to match Java bsl-language-server»",
        closure: Closure::Closed {
            basis: "сообщение заменено новой формулировкой из v8std #658 §2.1 узкой продуктовой правкой, санкционированной владельцем",
        },
    },
    Finding {
        id: "P2",
        what: "QueryToMissingMetadata: сообщение введено 5736244a «Update diagnostic message to match Java format»",
        closure: Closure::Closed {
            basis: "сообщение заменено новой формулировкой узкой продуктовой правкой, санкционированной владельцем",
        },
    },
    Finding {
        id: "P3",
        what: "значения metadata всех кодов крейта сверены с чужими @DiagnosticMetadata (0629d987 «match Java exactly»)",
        closure: Closure::Closed {
            basis: "решение владельца: принято как совместимое поведение, вердикт A, по всем кодам крейта",
        },
    },
    Finding {
        id: "P4",
        what: "ключ настройки minPathDepth введён со ссылкой на чужую реализацию (329802ad); skipSelectTopOne чист",
        closure: Closure::Closed {
            basis: "решение владельца: ключ minPathDepth принят как совместимое поведение, вердикт A; skipSelectTopOne зафиксирован без находки",
        },
    },
];

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Объявления `handlers.rs` без комментариев, строковых литералов, пробелов и
/// фигурных скобок. Комментарий и строка вычитаются первыми: закомментированная
/// ветка `match` иначе читалась бы как живая регистрация. Пробелы и скобки
/// снимаются потому, что rustfmt переносит длинную ветку в блок, и сравнение по
/// сырому тексту зависело бы от длины имени, а не от того, что ветка есть.
fn compact_registry() -> String {
    let text = std::fs::read_to_string(manifest_dir().join("src/handlers.rs"))
        .expect("src/handlers.rs не прочитан");
    compact_code(&text)
}

fn compact_code(text: &str) -> String {
    code_only(text).chars().filter(|c| !c.is_whitespace() && *c != '{' && *c != '}').collect()
}

/// Текст Rust без комментариев и строковых литералов; на их месте — пробел.
///
/// Это лексический обход объявлений инфраструктуры, а не разбор BSL: нужно лишь
/// не принять текст комментария или строки за код.
fn code_only(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match (chars[i], chars.get(i + 1).copied()) {
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push(' ');
            }
            ('/', Some('*')) => {
                let mut depth = 0usize;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1).copied() == Some('*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1).copied() == Some('/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                out.push(' ');
            }
            ('r', Some('"' | '#')) if !out.ends_with(|c: char| c.is_alphanumeric() || c == '_') => {
                let mut j = i + 1;
                let mut hashes = 0;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if chars.get(j) != Some(&'"') {
                    out.push(chars[i]);
                    i += 1;
                    continue;
                }
                j += 1;
                loop {
                    match chars.get(j) {
                        None => break,
                        Some('"') if (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) => {
                            j += 1 + hashes;
                            break;
                        }
                        Some(_) => j += 1,
                    }
                }
                i = j;
                out.push(' ');
            }
            ('"', _) => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += if chars[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
                out.push(' ');
            }
            ('\'', Some('\\')) => {
                i += 2;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
                out.push(' ');
            }
            ('\'', Some(_)) if chars.get(i + 2).copied() == Some('\'') => {
                i += 3;
                out.push(' ');
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Чтение реестра не принимает за регистрацию текст в комментарии или строке.
///
/// Продуктовый `handlers.rs` для этой проверки не портится: вход синтетический.
#[test]
fn registry_reading_ignores_comments_and_strings() {
    let live = "DiagnosticCode::UnionAll=>Some(&union_all::METADATA)";
    let sample = "match code {\n    DiagnosticCode::UnionAll => {\n        Some(&union_all::METADATA)\n    }\n}\n";
    assert!(compact_code(sample).contains(live), "живая ветка в блоке не найдена");
    for disabled in [
        "match code {\n    // DiagnosticCode::UnionAll => Some(&union_all::METADATA),\n}\n",
        "match code {\n    /* DiagnosticCode::UnionAll => Some(&union_all::METADATA), */\n}\n",
        "const S: &str = \"DiagnosticCode::UnionAll => Some(&union_all::METADATA)\";\n",
        "const S: &str = r#\"DiagnosticCode::UnionAll => Some(&union_all::METADATA)\"#;\n",
        "// pub mod union_all;\n",
    ] {
        let code = compact_code(disabled);
        assert!(
            !code.contains(live) && !code.contains("pubmodunion_all;"),
            "текст вне кода принят за регистрацию: {disabled:?}"
        );
    }
    assert!(
        compact_code("let c = '\"'; pub mod union_all;").contains("pubmodunion_all;"),
        "символьный литерал с кавычкой сбил чтение"
    );
}

/// Состав чек-листа равен предмету, и каждая строка — живой подключённый хендлер.
#[test]
fn the_checklist_is_exactly_the_subject() {
    let mut breaches = Vec::new();

    let mut keys: Vec<(&str, &str)> = CHECKLIST.iter().map(|e| (e.code, e.file)).collect();
    let total = keys.len();
    keys.sort_unstable();
    keys.dedup();
    if keys.len() != total {
        breaches.push(format!("в чек-листе повторы: {} строк, {} разных", total, keys.len()));
    }

    for &(code, file) in SUBJECT {
        if !keys.contains(&(code, file)) {
            breaches.push(format!("нет строки предмета: {code} / {file}"));
        }
    }
    for &(code, file) in &keys {
        if !SUBJECT.contains(&(code, file)) {
            breaches.push(format!("строка вне предмета: {code} / {file}"));
        }
    }

    let registry = compact_registry();
    for entry in CHECKLIST {
        let site = format!("{} / {}", entry.code, entry.file);
        match DiagnosticCode::from_str(entry.code) {
            Err(_) => breaches.push(format!("{site}: такого кода нет")),
            Ok(code) => {
                if get_metadata(code).is_none() {
                    breaches.push(format!("{site}: у кода нет metadata"));
                }
            }
        }
        let path = manifest_dir().join(format!("src/handlers/{}.rs", entry.file));
        if !path.is_file() {
            breaches.push(format!("{site}: нет файла {}", path.display()));
        }
        if !registry.contains(&format!("pubmod{};", entry.file)) {
            breaches.push(format!("{site}: модуль не подключён в handlers.rs"));
        }
        if !registry
            .contains(&format!("DiagnosticCode::{}=>Some(&{}::METADATA)", entry.code, entry.file))
        {
            breaches.push(format!("{site}: код не связан с metadata этого модуля"));
        }
    }

    assert_eq!(SUBJECT.len(), 17, "предмет программы — ровно 17 хендлеров");
    assert!(breaches.is_empty(), "состав чек-листа:\n  {}", breaches.join("\n  "));
}

/// Отметки невыполненной работы. Перечень заведомо неполон: он ловит расхожие.
const PLACEHOLDER_MARKS: &[&str] =
    &["tbd", "todo", "fixme", "xxx", "wip", "заглушка", "потом", "позже"];

/// Нижняя граница длины поля обоснования: самое короткое настоящее поле —
/// ссылка на раздел стандарта, «v8std #655 §1.1», 15 знаков; заглушки короче.
const SHORTEST_FIELD: usize = 12;

fn placeholder_breaches(site: &str, label: &str, text: &str) -> Vec<String> {
    let mut breaches = Vec::new();
    if text.trim().chars().count() < SHORTEST_FIELD {
        breaches.push(format!("{site}: {label} пусто или короче {SHORTEST_FIELD} знаков"));
    }
    // По словам, а не по подстроке: «потом» живёт внутри «потому».
    let folded = text.to_lowercase();
    for word in folded.split(|c: char| !c.is_alphanumeric()) {
        if PLACEHOLDER_MARKS.contains(&word) {
            breaches.push(format!("{site}: {label} содержит отметку незаконченного — {word}"));
        }
    }
    breaches
}

/// Называет ли текст публичный источник 1С: номер стандарта или раздел ИТС.
fn names_a_public_source(text: &str) -> bool {
    ["v8std #", "metod8dev #", "pubqlang"].iter().any(|mark| text.contains(mark))
}

/// У каждого вердикта есть все поля основания, и они не заглушки; R называет источник.
#[test]
fn every_verdict_carries_its_justification() {
    let mut breaches = Vec::new();

    for entry in CHECKLIST {
        let site = entry.code;
        for (label, text) in entry.verdict.justifications() {
            breaches.extend(placeholder_breaches(site, label, text));
        }
        breaches.extend(placeholder_breaches(site, "нижняя граница", entry.lower));
        if let Verdict::R { source, .. } = entry.verdict {
            if !names_a_public_source(source) {
                breaches.push(format!("{site}: вердикт R не называет публичного источника"));
            }
        }
    }

    assert!(!CHECKLIST.is_empty(), "чек-лист пуст — проверка была бы зелена вхолостую");
    assert!(breaches.is_empty(), "обоснования не по форме:\n  {}", breaches.join("\n  "));
}

/// Ни одного D и ни одной открытой находки; каждая ссылка на находку разрешается.
#[test]
fn no_unresolved_finding_remains() {
    let mut breaches = Vec::new();

    for entry in CHECKLIST {
        if let Verdict::D { what } = entry.verdict {
            breaches.push(format!("{}: открытый D — {what}", entry.code));
        }
        for id in entry.findings {
            if !FINDINGS.iter().any(|finding| finding.id == *id) {
                breaches.push(format!("{}: ссылка на неизвестную находку {id}", entry.code));
            }
        }
    }

    let mut ids: Vec<&str> = FINDINGS.iter().map(|finding| finding.id).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != FINDINGS.len() {
        breaches.push("в журнале повторяется ID находки".to_owned());
    }

    for finding in FINDINGS {
        breaches.extend(placeholder_breaches(finding.id, "находка", finding.what));
        match finding.closure {
            Closure::Open => breaches.push(format!("{}: находка открыта", finding.id)),
            Closure::Closed { .. } if finding.id == "C1" && contributions_pending() => {
                breaches.push("C1: закрыта, а в чек-листе вкладов есть строки «ожидает»".to_owned())
            }
            Closure::Awaiting { basis, until } => {
                if finding.id != "C1" {
                    breaches.push(format!(
                        "{}: ожидать внешнего подтверждения может только C1",
                        finding.id
                    ));
                }
                breaches.extend(placeholder_breaches(finding.id, "основание", basis));
                breaches.extend(placeholder_breaches(finding.id, "условие закрытия", until));
                if !crate_still_in_tier_b() {
                    breaches.push(format!(
                        "{}: перенос выполнен, а находка ещё ждёт — {until}",
                        finding.id
                    ));
                }
                if finding.id == "C1" && !contributions_pending() {
                    breaches.push(
                        "C1: строк «ожидает» не осталось — условие выполнено, находку пора закрыть"
                            .to_owned(),
                    );
                }
            }
            Closure::Closed { basis } => {
                breaches.extend(placeholder_breaches(finding.id, "основание закрытия", basis))
            }
        }
    }

    assert!(breaches.is_empty(), "неразрешённое:\n  {}", breaches.join("\n  "));
}

/// Подтверждение автора, что его вклад распространяется на условиях MIT OR Apache-2.0.
enum Confirmation {
    /// Подтверждения нет. Пока такая строка есть, перенос крейта не выполняется.
    Pending,
    /// Подтверждено; `link` — адрес сообщения автора с согласием, переданный
    /// оркестратором.
    #[allow(dead_code)]
    Confirmed { link: &'static str },
    /// Вклад заменён точечно; `commit` — полный хэш коммита замены.
    #[allow(dead_code)]
    Replaced { commit: &'static str },
}

impl Confirmation {
    fn label(&self) -> &'static str {
        match self {
            Confirmation::Pending => "ожидает",
            Confirmation::Confirmed { .. } => "подтверждено",
            Confirmation::Replaced { .. } => "заменено",
        }
    }

    /// Свидетельство исхода в том виде, в каком его пишет документ.
    fn evidence(&self) -> String {
        match self {
            Confirmation::Pending => "—".to_owned(),
            Confirmation::Confirmed { link } => (*link).to_owned(),
            Confirmation::Replaced { commit } => format!("`{commit}`"),
        }
    }
}

/// Ссылка на сообщение: схема https, имя хоста из меток DNS, необязательный порт
/// и непустой путь.
///
/// Машина проверяет форму свидетельства, а не то, что по ссылке действительно
/// лежит согласие: это проверяет тот, кто записывает строку, и ревью.
fn is_link(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("https://") else {
        return false;
    };
    if rest.contains(char::is_whitespace) {
        return false;
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = authority.split_once(':').unwrap_or((authority, "443"));
    let port_ok = !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u32>().is_ok_and(|p| (1..=65535).contains(&p));
    let labels: Vec<&str> = host.split('.').collect();
    let labels_ok = labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.chars().count())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        });
    port_ok && labels_ok && !path.is_empty()
}

/// Полный хэш коммита: 40 шестнадцатеричных знаков нижнего регистра.
fn is_full_commit(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn git() -> std::process::Command {
    let mut command = std::process::Command::new("git");
    // Унаследованные GIT_* под pre-commit хуком указывают на корень репозитория и
    // перебивают `current_dir`; ответ git тогда не о том дереве.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(&key);
        }
    }
    command.current_dir(manifest_dir());
    command
}

/// Коммит, на котором снят чек-лист вкладов.
const CONTRIBUTION_SNAPSHOT: &str = "93844e860b0839314b2701f6dbb36c5c658c8477";

/// Коммит, которым `CONTRIBUTING.md` стал требовать для вкладов только LGPL.
const CONTRIBUTION_CUTOFF: &str = "c5e811e7b264ff95c25a4897491bd29ef71c211e";

/// Крейты, чей вклад после `CONTRIBUTION_CUTOFF` требует подтверждения: два
/// переносимых и три, перенесённые раньше под той же политикой вкладов.
const CONTRIBUTION_PATHS: &[&str] = &[
    ":(top)crates/ide-diagnostics",
    ":(top)crates/bsl-analyzer",
    ":(top)crates/parser",
    ":(top)crates/lexer",
    ":(top)crates/sdbl-hir",
];

/// Авторы, чьи коммиты в чек-лист не входят: владелец проекта под двумя написаниями.
const OWNER_NAMES: &[&str] = &["Елишев Кирилл", "Кирилл Елишев"];

/// Тестовая идентичность владельца для доработок сайта bsl-analyzer (решение
/// владельца, 2026-10-07): её коммиты — коммиты владельца.
const OWNER_EMAILS: &[&str] = &["site-content@example.test"];

fn git_ok(args: &[&str]) -> bool {
    git().args(args).output().is_ok_and(|output| output.status.success())
}

fn git_lines(args: &[&str]) -> Vec<String> {
    git()
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Полна ли история: репозиторий есть, клон не shallow и снимок в нём есть.
///
/// Публичное зеркало публикуется без истории, а CI по умолчанию клонирует
/// неглубоко; там сверка с историей пропускается с сообщением — отсутствие
/// объекта в неполном клоне не доказывает отсутствие коммита.
fn history_complete() -> bool {
    let shallow = git_lines(&["rev-parse", "--is-shallow-repository"]);
    let complete = shallow.first().is_some_and(|line| line == "false")
        && git_ok(&["cat-file", "-e", &format!("{CONTRIBUTION_SNAPSHOT}^{{commit}}")]);
    if !complete {
        eprintln!("пропуск: история git неполна, сверка вкладов с историей не выполнена");
    }
    complete
}

/// Отказ, когда сверить с историей нечем, а перенос уже выполнен.
///
/// Пока крейт в Tier B, пропуск безвреден: перенос всё равно запрещён строками
/// «ожидает». После переноса неполная история (shallow-клон CI) не удостоверяет
/// ни состав чек-листа, ни замены, и гейт обязан упасть, а не пропустить. Публичное
/// зеркало без `docs/legal` аттестации не несёт и сверяться в нём не с чем.
fn transfer_needs_history(what: &str) -> Option<String> {
    let mirror = !manifest_dir().join("../../docs/legal").is_dir();
    if crate_still_in_tier_b() || mirror {
        return None;
    }
    Some(format!(
        "{what}: история git неполна, а перенос выполнен — сверка не удостоверена; нужна полная история"
    ))
}

/// Коммит замены правдоподобен: он есть, сделан после снимка, входит в проверяемый
/// HEAD и меняет хотя бы один путь исходного коммита.
///
/// Что замена действительно убирает текст вклада, машина не устанавливает — это
/// проверяет ревью коммита замены; здесь отсекается заведомо негодное свидетельство,
/// например сам снимок или коммит, не трогающий файлов вклада.
fn replacement_breach(original: &str, replacement: &str) -> Option<String> {
    if !history_complete() {
        return transfer_needs_history(&format!("{original}: замена {replacement}"));
    }
    if !git_ok(&["cat-file", "-e", &format!("{replacement}^{{commit}}")]) {
        return Some(format!("{original}: коммита замены {replacement} нет в истории"));
    }
    if replacement == CONTRIBUTION_SNAPSHOT
        || !git_ok(&["merge-base", "--is-ancestor", CONTRIBUTION_SNAPSHOT, replacement])
    {
        return Some(format!("{original}: замена {replacement} сделана не после снимка"));
    }
    if !git_ok(&["merge-base", "--is-ancestor", replacement, "HEAD"]) {
        return Some(format!("{original}: замена {replacement} не входит в проверяемый HEAD"));
    }
    let touched =
        |commit: &str| git_lines(&["show", "--no-renames", "--name-only", "--format=", commit]);
    let original_paths = touched(original);
    if !touched(replacement).iter().any(|path| original_paths.contains(path)) {
        return Some(format!("{original}: замена {replacement} не трогает файлов вклада"));
    }
    None
}

/// Исход ведётся по коммиту, а не по автору: автор может подтвердить часть вклада,
/// и разные коммиты могут заменяться разными коммитами замены.
struct Contribution {
    commit: &'static str,
    author: &'static str,
    status: Confirmation,
}

/// Коммиты сторонних авторов в крейтах `CONTRIBUTION_PATHS` между введением
/// условия «только LGPL» (`CONTRIBUTION_CUTOFF`) и снимком, без слияний и без коммитов
/// владельца; полные хэши. Состав сверяется с историей тестом ниже.
const CONTRIBUTIONS: &[Contribution] = &[
    Contribution {
        commit: "e40873dc8bacaed90fce7a7b7c6a95585704c0d4",
        author: "Aleksandr Ponkratov",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "0e73edd090566d6d2407f70ed485ac4fd02f5874",
        author: "Aleksey Kalsin",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "897c83731f89cc471ae01cd89dd1a08fe5ac7695",
        author: "Aleksey Kalsin",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "16a10103d1fad5b2fd11d2c6a593015eb0f6f24a",
        author: "Aleksey Kalsin",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "815b70db71c57307a4c5ab0a3c26b6d6d7cc63cb",
        author: "Aleksey Kalsin",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "d8895b732fb175ed053fc8bc12c2d5661bb17c1a",
        author: "Boris Sinitsyn",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "c0b2fe77a1a389584e042effd7d9b88a14d0ed6b",
        author: "Boris Sinitsyn",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "22ef9bdf93e039ee66fd719111a056fc5cff5991",
        author: "Boris Sinitsyn",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "732966c02834874cd358ed4233929bb9b12d64a7",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "01261f37198abaaf4a928718336900881f1b0c22",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "35b16473acdd9591418da45ee397e871f9981a46",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "31edcd75b099bf2959a7bec3b6358e14856596ac",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "df6b65393cb1e315df4259c55a94eb278095a217",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "9980afe5d6748aa84eb052530fa9416e7e2e5822",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "c6db2116b1a039237944dbba435cd52efeb7b191",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "be52ecc64d8b404f907922a5865382aa64d75d25",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "feb5364e881c453f2825b8a3b9c426ace4593673",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "f92c7e42ec57d2d07ecf9ae4caa1d724f221dd5f",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "949029e0daa9fc5f9e2600d5148f501bc4a10c7b",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "bf5a3c8a7eff857861eeea6a18a9e6cc45374943",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "bfef26c61035de5ac3734b9ba1e6d3ac0d79e941",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "c7458e137bc6240bd54c0b2063c5f3d291730df3",
        author: "defin85",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "b935cb89c7502f50bc9d4000df5360437e8ee3f3",
        author: "flamber-dev",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "a252b8abf8970f0e8d70abc0902c2514802e6894",
        author: "flamber-dev",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "e14e2caa20db7494ed8bf19483118300c500c1e6",
        author: "flamber-dev",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "824f755157a8a00ed86dfe2d129090674e4cf08d",
        author: "flamber-dev",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "cbcd5c6ef9c765c307a12414e9364ecd9959b402",
        author: "flamber-dev",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "9575b32cca3a7404bcdb2c107b23acae0ba5f282",
        author: "Igor Apresov",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "53a2af74c8abd478910de9fa204e78ef24c1baac",
        author: "Igor Apresov",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "a44315d70a0ad227c87ea329fa8f8e3149e60763",
        author: "Igor Apresov",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "16c5fdfd6fa29229b28b9fda0fe92221c9d44b8e",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "eac793fab4180d0b3d03a794db31fb4a1d03e63a",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "9b071ee651aad22bfa209b21f6a8e2ab27ebe1a5",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "edf263024a53d936d85939cd100520ef20772d68",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "694932056d95d646a13d90d72cbac38dda7abc07",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "e8a1d42d45cac96f65aef349cd9220574d30669a",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "8d5ebf668eeb8b8d4f6fb33342bd2a50b76692d8",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "5ba81dcec0989a99df074dc1ede10c4f3c89886d",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "63852d0eb129b300a35b891ec42094d3a1e20ce9",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "89c4381a863dbe851b398d9fb540ece459c10cf6",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "25b54e668f58f6358c1c0c705debf7d437b369eb",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "9c98d65981a8a7f0042334c342953efd7a968134",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "cc006e9a8833fd874d9603ed1430dce8ff230e2d",
        author: "Igor Beliaev (OniVe)",
        status: Confirmation::Pending,
    },
    Contribution {
        commit: "3b31ec206c49f4a5c237f449fbaec4edd5897978",
        author: "kdmaster",
        status: Confirmation::Pending,
    },
];

const CONTRIBUTION_COMMITS: usize = 44;

fn contributions_pending() -> bool {
    CONTRIBUTIONS.iter().any(|c| matches!(c.status, Confirmation::Pending))
}

/// Числится ли `ide-diagnostics` в Tier B `LICENSING.md`, то есть не выполнен ли перенос.
fn crate_still_in_tier_b() -> bool {
    let path = manifest_dir().join("../../LICENSING.md");
    // Вне workspace — распакованный пакет крейта — карты тиров нет, и переносить там
    // нечего; запрет переноса держит дерево репозитория, где файл есть.
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("пропуск: {} нет, тир крейта сверять не с чем", path.display());
        return true;
    };
    let tier_b = text
        .split("## Tier B")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap_or_else(|| panic!("в LICENSING.md нет раздела Tier B"));
    tier_b.lines().any(|line| line.trim_start().starts_with("| `ide-diagnostics` |"))
}

/// Пока хоть одна строка чек-листа ждёт подтверждения, перенос крейта не выполнен.
///
/// Решение владельца: одного публичного объявления о приёме вкладов для переноса
/// мало. Тест держит это машинно: перенос, сделанный раньше подтверждений, краснит
/// гейт; свидетельство подтверждения и замены проверяется по форме.
#[test]
fn contributions_block_the_transfer_until_confirmed() {
    let mut breaches = Vec::new();
    if CONTRIBUTIONS.len() != CONTRIBUTION_COMMITS {
        breaches.push(format!(
            "в чек-листе {} коммитов, записано {CONTRIBUTION_COMMITS}",
            CONTRIBUTIONS.len()
        ));
    }
    let mut all: Vec<&str> = CONTRIBUTIONS.iter().map(|c| c.commit).collect();
    all.sort_unstable();
    all.dedup();
    if all.len() != CONTRIBUTIONS.len() {
        breaches.push("коммит повторён в чек-листе".to_owned());
    }
    for c in CONTRIBUTIONS {
        match c.status {
            Confirmation::Confirmed { link } if !is_link(link) => breaches
                .push(format!("{}: подтверждение не ссылка на сообщение — {link}", c.commit)),
            Confirmation::Replaced { commit } if !is_full_commit(commit) => {
                breaches.push(format!("{}: замена не полный хэш коммита — {commit}", c.commit))
            }
            Confirmation::Replaced { commit } => {
                breaches.extend(replacement_breach(c.commit, commit))
            }
            _ => {}
        }
    }
    if contributions_pending() && !crate_still_in_tier_b() {
        breaches.push("перенос выполнен, а подтверждения авторов не все".to_owned());
    }
    assert!(breaches.is_empty(), "вклады сторонних авторов:\n  {}", breaches.join("\n  "));
}

/// Чек-лист — это ровно история на снимке, а не её копия.
///
/// Без этой сверки пропуск коммита при согласованной правке числа и документа
/// незаметно выводил бы вклад из-под блокировки переноса.
#[test]
fn the_checklist_is_the_history_at_the_snapshot() {
    if !history_complete() {
        if let Some(breach) = transfer_needs_history("состав чек-листа") {
            panic!("{breach}");
        }
        return;
    }
    let range = format!("{CONTRIBUTION_CUTOFF}..{CONTRIBUTION_SNAPSHOT}");
    let mut args = vec!["log", "--no-merges", "--format=%H|%an|%ae", range.as_str(), "--"];
    args.extend_from_slice(CONTRIBUTION_PATHS);
    let mut history: Vec<(String, String)> = git_lines(&args)
        .into_iter()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '|');
            Some((fields.next()?.to_owned(), fields.next()?.to_owned(), fields.next()?.to_owned()))
        })
        .filter(|(_, author, email)| {
            !OWNER_NAMES.contains(&author.as_str()) && !OWNER_EMAILS.contains(&email.as_str())
        })
        .map(|(commit, author, _)| (commit, author))
        .collect();
    let mut checklist: Vec<(String, String)> =
        CONTRIBUTIONS.iter().map(|c| (c.commit.to_owned(), c.author.to_owned())).collect();
    history.sort();
    checklist.sort();
    let missing: Vec<_> = history.iter().filter(|row| !checklist.contains(row)).collect();
    let extra: Vec<_> = checklist.iter().filter(|row| !history.contains(row)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "чек-лист разошёлся с историей на снимке:\n  нет в чек-листе: {missing:?}\n  лишние: {extra:?}"
    );
}

/// Текст аттестации, если каталог `docs/legal` есть в этом дереве.
fn attestation_text() -> Option<String> {
    let legal = manifest_dir().join("../../docs/legal");
    // The public mirror is published without `docs/legal/`, so the attestation is absent
    // there by construction. A missing FILE inside an existing directory still fails below.
    if !legal.is_dir() {
        eprintln!("пропуск: каталога docs/legal нет, аттестацию сверять не с чем");
        return None;
    }
    let attestation = legal.join("ide-diagnostics-handler-attestation.md");
    Some(
        std::fs::read_to_string(&attestation)
            .unwrap_or_else(|err| panic!("{} не прочитан: {err}", attestation.display())),
    )
}

/// Ячейки строки Markdown-таблицы без пробелов по краям; `None` — в строке нет `|`.
fn table_cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if !line.contains('|') {
        return None;
    }
    let inner = line.strip_prefix('|').unwrap_or(line);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    Some(inner.split('|').map(|cell| cell.trim().to_owned()).collect())
}

/// Строки таблицы, отобранные `belongs`, равны ожидаемым: каждая ровно один раз,
/// лишних нет.
fn table_breaches(
    text: &str,
    expected: &[Vec<String>],
    belongs: impl Fn(&[String]) -> bool,
    what: &str,
) -> Vec<String> {
    let rows: Vec<Vec<String>> =
        text.lines().filter_map(table_cells).filter(|cells| belongs(cells)).collect();
    let show = |cells: &[String]| format!("| {} |", cells.join(" | "));

    let mut breaches = Vec::new();
    for row in expected {
        match rows.iter().filter(|candidate| *candidate == row).count() {
            1 => {}
            0 => breaches.push(format!("в аттестации не найдена {what}: {}", show(row))),
            n => breaches.push(format!("в аттестации повторена {n} раз {what}: {}", show(row))),
        }
    }
    for row in &rows {
        if !expected.contains(row) {
            breaches.push(format!("в аттестации лишняя {what}: {}", show(row)));
        }
    }
    breaches
}

/// Перечень вердиктов документа совпадает с чек-листом в обе стороны.
#[test]
fn the_document_lists_every_verdict() {
    let Some(text) = attestation_text() else {
        return;
    };

    let expected: Vec<Vec<String>> = CHECKLIST
        .iter()
        .map(|entry| {
            vec![
                format!("`{}`", entry.code),
                format!("`{}.rs`", entry.file),
                entry.verdict.letter().to_string(),
            ]
        })
        .collect();
    let breaches = table_breaches(
        &text,
        &expected,
        |row| row.len() == 3 && row[1].starts_with('`') && row[1].ends_with(".rs`"),
        "строка перечня",
    );

    assert!(breaches.is_empty(), "перечень разошёлся с чек-листом:\n  {}", breaches.join("\n  "));
}

/// Журнал находок документа совпадает с журналом гейта: те же ID, тот же исход.
#[test]
fn the_document_lists_every_finding() {
    let Some(text) = attestation_text() else {
        return;
    };

    let expected: Vec<Vec<String>> = FINDINGS
        .iter()
        .map(|finding| {
            let status = match finding.closure {
                Closure::Open => "открыта",
                Closure::Closed { .. } => "закрыта",
                Closure::Awaiting { .. } => "ожидает",
            };
            vec![format!("**{}**", finding.id), status.to_owned()]
        })
        .collect();
    let breaches = table_breaches(
        &text,
        &expected,
        |row| row.len() == 2 && row[0].starts_with("**") && row[0].ends_with("**"),
        "строка журнала",
    );

    assert!(breaches.is_empty(), "журнал разошёлся с гейтом:\n  {}", breaches.join("\n  "));
}

/// Сводка документа считает то же, что держит чек-лист.
#[test]
fn the_document_counts_what_the_checklist_holds() {
    let Some(text) = attestation_text() else {
        return;
    };

    let titles = ["R — выведено из нормы", "A — осознанное решение", "D — не разрешено", "Всего"];
    let count =
        |letter: char| CHECKLIST.iter().filter(|entry| entry.verdict.letter() == letter).count();
    let expected = vec![
        vec![titles[0].to_owned(), format!("**{}**", count('R'))],
        vec![titles[1].to_owned(), format!("**{}**", count('A'))],
        vec![titles[2].to_owned(), format!("**{}**", count('D'))],
        vec![titles[3].to_owned(), format!("**{}**", CHECKLIST.len())],
    ];
    let breaches = table_breaches(
        &text,
        &expected,
        |row| row.first().is_some_and(|title| titles.contains(&title.as_str())),
        "строка сводки",
    );

    assert!(breaches.is_empty(), "сводка разошлась со счётом:\n  {}", breaches.join("\n  "));
}

/// Чек-лист вкладов в документе совпадает с гейтом: коммит, автор, исход, свидетельство.
#[test]
fn the_document_lists_every_contribution() {
    let Some(text) = attestation_text() else {
        return;
    };
    let expected: Vec<Vec<String>> = CONTRIBUTIONS
        .iter()
        .map(|c| {
            vec![
                format!("`{}`", c.commit),
                c.author.to_owned(),
                c.status.label().to_owned(),
                c.status.evidence(),
            ]
        })
        .collect();
    // Строка отбирается по форме — первая ячейка хэш коммита в кавычках, — а не по
    // известному исходу: строка с опечаткой в исходе иначе ускользала бы от сверки.
    let breaches = table_breaches(
        &text,
        &expected,
        |row| {
            row.first().is_some_and(|cell| {
                cell.len() > 2
                    && cell.starts_with('`')
                    && cell.ends_with('`')
                    && cell[1..cell.len() - 1].bytes().all(|b| b.is_ascii_hexdigit())
            })
        },
        "строка чек-листа вкладов",
    );
    assert!(
        breaches.is_empty(),
        "чек-лист вкладов разошёлся с гейтом:\n  {}",
        breaches.join("\n  ")
    );
}

/// Свидетельство подтверждения проверяется по форме, а не по наличию текста.
#[test]
fn confirmation_evidence_must_have_its_form() {
    assert!(is_link("https://t.me/project_group/1234"));
    assert!(is_link("https://chat.example.org:8443/messages/123"));
    for bad in [
        "author did not reply; confirmation absent",
        "https://",
        "https:///",
        "https:///consent",
        "https://?x/y",
        "https://t.me",
        "https://localhost/msg",
        "https://t.me/a b",
        "https://.t.me/x",
        "https://-.t.me/consent",
        "https://-bad-.example/consent",
        "https://a-.t.me/x",
        "https://t.me:0/x",
        "https://t.me:99999/x",
        "https://t.me:/x",
    ] {
        assert!(!is_link(bad), "принято как ссылка: {bad}");
    }
    assert!(is_full_commit("93844e860b0839314b2701f6dbb36c5c658c8477"));
    assert!(!is_full_commit("93844e86"));
    assert!(!is_full_commit("93844E860B0839314B2701F6DBB36C5C658C8477"));
    if history_complete() {
        let earlier = CONTRIBUTIONS[0].commit;
        assert!(
            replacement_breach(earlier, CONTRIBUTION_SNAPSHOT).is_some(),
            "снимок принят заменой"
        );
        assert!(replacement_breach(earlier, earlier).is_some(), "сам вклад принят своей заменой");
        assert!(replacement_breach(
            CONTRIBUTION_SNAPSHOT,
            "0000000000000000000000000000000000000000"
        )
        .is_some());
    }
}
