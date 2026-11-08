//! SDBL (1C:Enterprise query language) lexer.
//!
//! ## Provenance
//!
//! The token vocabulary has been re-derived from official 1C sources one
//! vocabulary at a time. Each re-derived group carries a `CLEAN-ROOM`
//! banner naming the attestation document that records its sources.
//!
//! - Slice 3b — metadata-object table vocabulary and the `Error`
//!   fallback: `docs/legal/sdbl-clean-room-slice3b.md`.
//! - Slice 4 — query-function vocabulary:
//!   `docs/legal/sdbl-clean-room-slice4.md`.
//! - Slice 5 — virtual tables and external data sources:
//!   `docs/legal/sdbl-clean-room-slice5.md`.
//! - Slice 1-addendum — the query-extension brace pair:
//!   `docs/legal/sdbl-clean-room-slice1-addendum.md`.
//!
//! Nothing awaits re-derivation any more, and no `LEGACY` banner
//! remains: every `SdblTokenKind` variant is covered by an attestation.
//!
//! The lexer core, the structural keyword vocabulary, the clause-keyword
//! leftovers and the primitive-type vocabulary are attested by
//! `docs/legal/sdbl-clean-room-slice{1,2,2-addendum,3a}.md`. Their
//! in-file banners were removed in a repository-wide comment prune and
//! are not restored here, so absence of a banner above a declaration
//! does not imply absence of an attestation — the attestation documents
//! are authoritative on scope.

mod strings_mode;

use logos::Logos;
use smol_str::SmolStr;

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SdblTokenKind {
    #[regex(r"[ \t\r]+")]
    Whitespace,

    #[token("\n")]
    Newline,

    #[regex(r"//[^\n]*")]
    Comment,

    #[token("(")]
    LParen,

    #[token(")")]
    RParen,

    // ===================================================================
    // CLEAN-ROOM Slice 1-addendum — query-extension brace pair
    //
    // Braces are not part of the query language. They come from the
    // data-composition extension of it, whose canonical article —
    // 1C:Enterprise 8.3.27 syntax assistant, «Расширение языка запросов
    // для системы компоновки данных» — defines the whole construct as
    // «специальных синтаксических инструкций, заключаемых в фигурные
    // скобки и помещаемых непосредственно в текст запроса». Developer's
    // Reference Глава 8 describes the base language without a single
    // brace, which is the other half of the attestation.
    //
    // The extension's own keywords are deliberately not tokens: they
    // are keywords only inside a brace region, the lexer has no state
    // and so cannot know that a region is open, and their spellings
    // (`Характеристики`, `ПолеЗначения`, …) are ordinary attribute and
    // tabular-section names everywhere else.
    //
    // Attestation: `docs/legal/sdbl-clean-room-slice1-addendum.md`.
    // ===================================================================
    /// `{` — opens a data-composition query-extension instruction.
    ///
    /// The braces mark a section a user may customise at runtime. The
    /// documented elements are `{ВЫБРАТЬ …}` / `{SELECT …}`, the field
    /// aliases available for display; `{ГДЕ …}` / `{WHERE …}`, the
    /// fields available for filtering; `{ХАРАКТЕРИСТИКИ …}` /
    /// `{CHARACTERISTICS …}`, where to find characteristic types and
    /// values for a type; and the parameter form `{&Параметр}` inside a
    /// virtual table's argument list.
    ///
    /// Emitting these as tokens rather than [`SdblTokenKind::Error`] is
    /// what lets the parser skip the region and keep the diagnostics of
    /// the surrounding query.
    #[token("{")]
    LBrace,

    /// `}` — closes a data-composition query-extension instruction; see
    /// [`SdblTokenKind::LBrace`].
    ///
    /// A region may nest, and an unbalanced brace on either side is
    /// tokenised rather than rejected: query texts are routinely
    /// assembled by string concatenation, so a literal that begins or
    /// ends mid-extension is ordinary input.
    #[token("}")]
    RBrace,

    #[token(".")]
    Dot,

    #[token(",")]
    Comma,

    #[token(";")]
    Semicolon,

    #[token("#")]
    Hash,

    #[token("&")]
    Ampersand,

    #[token("|")]
    Bar,

    #[token("=")]
    Eq,

    #[token("<>")]
    Neq,

    #[token("<=")]
    Le,

    #[token("<")]
    Lt,

    #[token(">=")]
    Ge,

    #[token(">")]
    Gt,

    #[token("+")]
    Plus,

    #[token("-")]
    Minus,

    #[token("*")]
    Star,

    #[token("/")]
    Slash,

    #[token("%")]
    Percent,

    /// The fraction may be empty: the platform reads `0.` as a number (8.3.17 and 8.3.27).
    #[regex(r"[0-9]+\.[0-9]*")]
    Float,

    #[regex(r"[0-9]+")]
    Decimal,

    #[token("\"")]
    Quote,

    String,

    #[regex(r"'[0-9]{8,14}'")]
    Date,

    #[regex(r"[_\p{L}][_\p{L}0-9]*", priority = 1)]
    Ident,

    #[regex(r"&[_\p{L}][_\p{L}0-9]*")]
    Parameter,

    #[regex(r"(?i)выбрать|(?i)select")]
    KwSelect,

    #[regex(r"(?i)из|(?i)from")]
    KwFrom,

    #[regex(r"(?i)поместить|(?i)into")]
    KwInto,

    #[regex(r"(?i)где|(?i)where")]
    KwWhere,

    #[regex(r"(?i)сгруппировать|(?i)group")]
    KwGroup,

    #[regex(r"(?i)упорядочить|(?i)order")]
    KwOrder,

    #[regex(r"(?i)имеющие|(?i)having")]
    KwHaving,

    #[regex(r"(?i)итоги|(?i)totals")]
    KwTotals,

    #[regex(r"(?i)объединить|(?i)union")]
    KwUnion,

    #[regex(r"(?i)все|(?i)all")]
    KwAll,

    #[regex(r"(?i)различные|(?i)distinct")]
    KwDistinct,

    #[regex(r"(?i)первые|(?i)top")]
    KwTop,

    #[regex(r"(?i)соединение|(?i)join")]
    KwJoin,

    #[regex(r"(?i)внутреннее|(?i)inner")]
    KwInner,

    #[regex(r"(?i)левое|(?i)left")]
    KwLeft,

    #[regex(r"(?i)правое|(?i)right")]
    KwRight,

    #[regex(r"(?i)полное|(?i)full")]
    KwFull,

    #[regex(r"(?i)внешнее|(?i)outer")]
    KwOuter,

    #[regex(r"(?i)по|(?i)on|(?i)by")]
    KwOnOrBy,

    #[regex(r"(?i)как|(?i)as")]
    KwAs,

    #[regex(r"(?i)в|(?i)in")]
    KwIn,

    #[regex(r"(?i)между|(?i)between")]
    KwBetween,

    #[regex(r"(?i)подобно|(?i)like")]
    KwLike,

    #[regex(r"(?i)есть|(?i)is")]
    KwIs,

    #[regex(r"(?i)выбор|(?i)case")]
    KwCase,

    #[regex(r"(?i)когда|(?i)when")]
    KwWhen,

    #[regex(r"(?i)тогда|(?i)then")]
    KwThen,

    #[regex(r"(?i)иначе|(?i)else")]
    KwElse,

    #[regex(r"(?i)конец|(?i)end")]
    KwEnd,

    #[regex(r"(?i)и|(?i)and")]
    OpAnd,

    #[regex(r"(?i)или|(?i)or")]
    OpOr,

    #[regex(r"(?i)не|(?i)not")]
    OpNot,

    #[regex(r"(?i)истина|(?i)true")]
    LitTrue,

    #[regex(r"(?i)ложь|(?i)false")]
    LitFalse,

    #[regex(r"(?i)null")]
    LitNull,

    #[regex(r"(?i)уничтожить|(?i)drop")]
    KwDrop,

    #[regex(r"(?i)автоупорядочивание|(?i)autoorder")]
    KwAutoOrder,

    #[regex(r"(?i)возр|(?i)asc")]
    KwAsc,

    #[regex(r"(?i)убыв|(?i)desc")]
    KwDesc,

    #[regex(r"(?i)иерархия|(?i)hierarchy")]
    KwHierarchy,

    #[regex(r"(?i)разрешенные|(?i)allowed")]
    KwAllowed,

    #[regex(r"(?i)для|(?i)for")]
    KwFor,

    #[regex(r"(?i)изменения|(?i)update")]
    KwUpdate,

    #[regex(r"(?i)индексировать|(?i)index")]
    KwIndex,

    #[regex(r"(?i)только|(?i)only")]
    KwOnly,

    #[regex(r"(?i)общие|(?i)overall")]
    KwOverall,

    #[regex(r"(?i)периодами|(?i)periods")]
    KwPeriods,

    #[regex(r"(?i)спецсимвол|(?i)escape")]
    KwEscape,

    #[regex(r"(?i)ссылка|(?i)refs")]
    KwRefs,

    #[regex(r"(?i)выразить|(?i)cast")]
    KwCast,

    #[regex(r"(?i)тип|(?i)type")]
    KwType,

    #[regex(r"(?i)значение|(?i)value")]
    KwValue,

    #[regex(r"(?i)булево|(?i)boolean")]
    TypeBoolean,

    #[regex(r"(?i)число|(?i)number")]
    TypeNumber,

    #[regex(r"(?i)строка|(?i)string")]
    TypeString,

    #[regex(r"(?i)дата|(?i)date")]
    TypeDate,

    #[regex(r"(?i)неопределено|(?i)undefined")]
    LitUndefined,

    #[regex(r"(?i)декада|(?i)tendays")]
    PeriodTenDays,

    #[regex(r"(?i)полугодие|(?i)halfyear")]
    PeriodHalfYear,

    // ===================================================================
    // CLEAN-ROOM Slice 4 — query-function vocabulary
    //
    // The functions of the query language: aggregate, date, string,
    // mathematical, and the type/presentation group. Every spelling
    // below comes from the 1C:Enterprise 8.3.27 syntax assistant book
    // «Синтаксис текста запросов» — from its article «Функции языка
    // запросов», which is the complete grouped index of query
    // functions, from its article «Двуязычное представление ключевых
    // слов», which is the Russian/English correspondence table, or
    // from the bilingual headline of the function's own article.
    // Case-insensitivity is documented — «Регистр букв (строчные или
    // заглавные) при написании не имеет значения» (Developer's
    // Reference Глава 8 §8.4.5) — and is what the `(?i)` flag
    // expresses.
    //
    // Three variants named `Fn*` are not functions in the source: the
    // date literal keyword `ДАТАВРЕМЯ`, the selection-list keyword
    // `ПУСТАЯТАБЛИЦА`, and the predefined-data selector
    // `ПустаяСсылка`. Their spellings are canonical; only the family
    // label is wrong, and renaming them would be a downstream-visible
    // change with no provenance value.
    //
    // Attestation: `docs/legal/sdbl-clean-room-slice4.md`.
    //
    // Index (navigational only; the `#[regex]` attributes remain the
    // single source of truth, since logos requires the pattern at the
    // declaration site). The groups are the source's own:
    //
    //   aggregate      FnSum, FnAvg, FnMin, FnMax, FnCount
    //   date           FnYear, FnQuarter, FnMonth, FnDayOfYear, FnDay,
    //                  FnWeek, FnWeekDay, FnHour, FnMinute, FnSecond,
    //                  FnBeginOfPeriod, FnEndOfPeriod, FnDateAdd,
    //                  FnDateDiff, and the FnDateTime literal
    //   string         FnSubstring, FnStringLength, FnStrFind,
    //                  FnStrReplace, FnUpper, FnLower, FnTrimAll,
    //                  FnTrimL, FnTrimR, FnLeft, FnRight
    //   mathematical   FnRound, FnInt, FnExp, FnLog10, FnLog, FnPow,
    //                  FnSqrt, FnACos, FnASin, FnATan, FnCos, FnSin,
    //                  FnTan
    //   other          FnValueType, FnPresentation, FnRefPresentation,
    //                  FnIsNull, FnRecordAutoNumber, FnGroupedBy,
    //                  FnStoredDataSize, FnUUID, and the FnEmptyTable
    //                  and FnEmptyRef keywords
    //
    // The function `СТРОКА (String)` has no variant here: its spelling
    // is the primitive type name, already claimed by
    // [`SdblTokenKind::TypeString`].
    // ===================================================================
    /// `СУММА (SUM)` — arithmetic sum of the field values in the
    /// selection.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.17.3.
    #[regex(r"(?i)сумма|(?i)sum")]
    FnSum,

    /// `СРЕДНЕЕ (AVG)` — mean of the field values in the selection.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.17.3.
    #[regex(r"(?i)среднее|(?i)avg")]
    FnAvg,

    /// `МИНИМУМ (MIN)` — least of the field values in the selection.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.17.3.
    #[regex(r"(?i)минимум|(?i)min")]
    FnMin,

    /// `МАКСИМУМ (MAX)` — greatest of the field values in the
    /// selection.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.17.3.
    #[regex(r"(?i)максимум|(?i)max")]
    FnMax,

    /// `КОЛИЧЕСТВО (COUNT)` — count of the values in the selection.
    ///
    /// The only aggregate that admits `РАЗЛИЧНЫЕ` and `*` as its
    /// argument; that is grammar, not vocabulary, and belongs to the
    /// parser.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.17.3.
    #[regex(r"(?i)количество|(?i)count")]
    FnCount,

    /// `ГОД (YEAR)` — year number of a date.
    ///
    /// Doubles as a period specifier inside `НАЧАЛОПЕРИОДА`,
    /// `КОНЕЦПЕРИОДА`, `ДОБАВИТЬКДАТЕ` and `РАЗНОСТЬДАТ`; the same
    /// token serves both readings, which the parser separates by
    /// position.
    #[regex(r"(?i)год|(?i)year", priority = 2)]
    FnYear,

    /// `КВАРТАЛ (QUARTER)` — quarter number of a date, 1 to 4. Also a
    /// period specifier; see [`FnYear`].
    #[regex(r"(?i)квартал|(?i)quarter", priority = 2)]
    FnQuarter,

    /// `МЕСЯЦ (MONTH)` — month number of a date, 1 to 12. Also a
    /// period specifier; see [`FnYear`].
    #[regex(r"(?i)месяц|(?i)month", priority = 2)]
    FnMonth,

    /// `ДЕНЬГОДА (DAYOFYEAR)` — day of the year, 1 to 366.
    ///
    /// Longest-match wins over [`FnDay`], whose Russian and English
    /// spellings are both proper prefixes of this one.
    #[regex(r"(?i)деньгода|(?i)dayofyear", priority = 2)]
    FnDayOfYear,

    /// `ДЕНЬ (DAY)` — day of the month, 1 to 31. Also a period
    /// specifier; see [`FnYear`].
    #[regex(r"(?i)день|(?i)day", priority = 2)]
    FnDay,

    /// `НЕДЕЛЯ (WEEK)` — week number of the year. Also a period
    /// specifier; see [`FnYear`].
    ///
    /// The result depends on the infobase's first-day-of-week regional
    /// setting, which is runtime behaviour and not the lexer's concern.
    #[regex(r"(?i)неделя|(?i)week", priority = 2)]
    FnWeek,

    /// `ДЕНЬНЕДЕЛИ (WEEKDAY)` — day of the week, 1 (Monday) to
    /// 7 (Sunday).
    #[regex(r"(?i)деньнедели|(?i)weekday", priority = 2)]
    FnWeekDay,

    /// `ЧАС (HOUR)` — hour of the day, 0 to 23. Also a period
    /// specifier; see [`FnYear`].
    #[regex(r"(?i)час|(?i)hour", priority = 2)]
    FnHour,

    /// `МИНУТА (MINUTE)` — minute of the hour, 0 to 59. Also a period
    /// specifier; see [`FnYear`].
    #[regex(r"(?i)минута|(?i)minute", priority = 2)]
    FnMinute,

    /// `СЕКУНДА (SECOND)` — second of the minute, 0 to 59. Also a
    /// difference specifier inside `РАЗНОСТЬДАТ`.
    #[regex(r"(?i)секунда|(?i)second", priority = 2)]
    FnSecond,

    /// `НАЧАЛОПЕРИОДА (BEGINOFPERIOD)` — start of the period that
    /// contains a date.
    #[regex(r"(?i)началопериода|(?i)beginofperiod")]
    FnBeginOfPeriod,

    /// `КОНЕЦПЕРИОДА (ENDOFPERIOD)` — end of the period that contains
    /// a date.
    #[regex(r"(?i)конецпериода|(?i)endofperiod")]
    FnEndOfPeriod,

    /// `ДОБАВИТЬКДАТЕ (DATEADD)` — a date shifted by a whole number of
    /// periods.
    #[regex(r"(?i)добавитькдате|(?i)dateadd")]
    FnDateAdd,

    /// `РАЗНОСТЬДАТ (DATEDIFF)` — difference between two dates,
    /// expressed in a chosen unit.
    ///
    /// The source gives two English spellings: the bilingual keyword
    /// table and the function index say `DATEDIFF`, while the English
    /// article is headlined `DATEDIFFERENCE function` and its examples
    /// use that form. Both are documented, so both are accepted;
    /// `datediff` is a proper prefix of `datedifference`, which
    /// longest-match separates.
    #[regex(r"(?i)разностьдат|(?i)datediff|(?i)datedifference")]
    FnDateDiff,

    /// `ДАТАВРЕМЯ (DATETIME)` — the date literal, written
    /// `ДАТАВРЕМЯ(<год>, <месяц>, <день>[, <час>, <минута>,
    /// <секунда>])`.
    ///
    /// Not a function: the article «Литерал типа ДАТА» calls it the
    /// way a value of type `Дата` is written, and the query-function
    /// index does not list it. The type name `Дата` itself is a
    /// separate token, [`SdblTokenKind::TypeDate`].
    #[regex(r"(?i)датавремя|(?i)datetime")]
    FnDateTime,

    /// `ПОДСТРОКА (SUBSTRING)` — substring by start position and
    /// length.
    #[regex(r"(?i)подстрока|(?i)substring")]
    FnSubstring,

    /// `ДлинаСтроки (StringLength)` — length of a string, as a number.
    #[regex(r"(?i)длинастроки|(?i)stringlength")]
    FnStringLength,

    /// `СтрНайти (StrFind)` — position of a substring, 1-based, or 0
    /// when absent.
    #[regex(r"(?i)стрнайти|(?i)strfind")]
    FnStrFind,

    /// `СтрЗаменить (StrReplace)` — every occurrence of one substring
    /// replaced by another.
    #[regex(r"(?i)стрзаменить|(?i)strreplace")]
    FnStrReplace,

    /// `ВРег (Upper)` — the string in upper case.
    #[regex(r"(?i)врег|(?i)upper")]
    FnUpper,

    /// `НРег (Lower)` — the string in lower case.
    #[regex(r"(?i)нрег|(?i)lower")]
    FnLower,

    /// `СокрЛП (TrimAll)` — the string without leading or trailing
    /// blanks.
    ///
    /// Longest-match wins over [`FnTrimL`], whose Russian spelling is a
    /// proper prefix of this one.
    #[regex(r"(?i)сокрлп|(?i)trimall")]
    FnTrimAll,

    /// `СокрЛ (TrimL)` — the string without leading blanks.
    #[regex(r"(?i)сокрл|(?i)triml")]
    FnTrimL,

    /// `СокрП (TrimR)` — the string without trailing blanks.
    #[regex(r"(?i)сокрп|(?i)trimr")]
    FnTrimR,

    /// `Лев (Left)` — the leading characters of a string.
    ///
    /// Russian spelling only. The English `Left` is byte-identical to
    /// the join keyword `ЛЕВОЕ (LEFT)`, which
    /// [`SdblTokenKind::KwLeft`] already claims; a lexer cannot tell
    /// the two apart, because the difference is grammatical position.
    /// Leaving `Left` on the join keyword keeps `LEFT JOIN` correct
    /// and hands the function reading to the parser, which sees the
    /// following `(`.
    ///
    /// Against the join keyword's Russian spelling there is no
    /// contest: `лев` is a proper prefix of `левое`, so longest match
    /// separates them.
    #[regex(r"(?i)лев")]
    FnLeft,

    /// `Прав (Right)` — the trailing characters of a string.
    ///
    /// Russian spelling only, for the reason given on [`FnLeft`]: the
    /// English `Right` belongs to [`SdblTokenKind::KwRight`].
    #[regex(r"(?i)прав")]
    FnRight,

    /// `Окр (Round)` — a number rounded to a given number of decimal
    /// places.
    #[regex(r"(?i)окр|(?i)round")]
    FnRound,

    /// `Цел (Int)` — the integer part of a number, fraction discarded.
    #[regex(r"(?i)цел|(?i)int")]
    FnInt,

    /// `Exp` — the base of the natural logarithm raised to a power.
    ///
    /// The mathematical functions have no Russian spelling: the source
    /// writes them in Latin letters in both language builds.
    #[regex(r"(?i)exp")]
    FnExp,

    /// `Log10` — decimal logarithm.
    ///
    /// Longest-match wins over [`FnLog`], whose spelling is a proper
    /// prefix of this one.
    #[regex(r"(?i)log10")]
    FnLog10,

    /// `Log` — natural logarithm.
    #[regex(r"(?i)log")]
    FnLog,

    /// `Pow` — a base raised to an exponent.
    #[regex(r"(?i)pow")]
    FnPow,

    /// `Sqrt` — square root.
    #[regex(r"(?i)sqrt")]
    FnSqrt,

    /// `ACos` — arc cosine, in radians.
    #[regex(r"(?i)acos")]
    FnACos,

    /// `ASin` — arc sine, in radians.
    #[regex(r"(?i)asin")]
    FnASin,

    /// `ATan` — arc tangent, in radians.
    #[regex(r"(?i)atan")]
    FnATan,

    /// `Cos` — cosine of an angle in radians.
    #[regex(r"(?i)cos")]
    FnCos,

    /// `Sin` — sine of an angle in radians.
    #[regex(r"(?i)sin")]
    FnSin,

    /// `Tan` — tangent of an angle in radians.
    #[regex(r"(?i)tan")]
    FnTan,

    /// `ТИПЗНАЧЕНИЯ (VALUETYPE)` — the type of a value, as a value of
    /// type `Тип`.
    #[regex(r"(?i)типзначения|(?i)valuetype")]
    FnValueType,

    /// `ПРЕДСТАВЛЕНИЕ (PRESENTATION)` — string presentation of a value
    /// of any type.
    #[regex(r"(?i)представление|(?i)presentation")]
    FnPresentation,

    /// `ПРЕДСТАВЛЕНИЕССЫЛКИ (REFPRESENTATION)` — presentation of a
    /// reference value, the value itself otherwise.
    ///
    /// Longest-match wins over [`FnPresentation`], whose Russian
    /// spelling is a proper prefix of this one.
    ///
    /// Attested only by the syntax assistant: the Developer's
    /// Reference does not mention this function.
    #[regex(r"(?i)представлениессылки|(?i)refpresentation")]
    FnRefPresentation,

    /// `ЕСТЬNULL (ISNULL)` — the first argument, or the second when
    /// the first is `NULL`.
    #[regex(r"(?i)естьnull|(?i)isnull")]
    FnIsNull,

    /// `АВТОНОМЕРЗАПИСИ (RECORDAUTONUMBER)` — a unique ascending
    /// number, usable only in the selection list of a query that
    /// builds a temporary table.
    #[regex(r"(?i)автономерзаписи|(?i)recordautonumber")]
    FnRecordAutoNumber,

    /// `СГРУППИРОВАНОПО (GROUPEDBY)` — whether the current row was
    /// grouped by the given field.
    ///
    /// Longest-match wins over [`SdblTokenKind::KwGroup`] for the
    /// English spelling, whose `group` is a proper prefix of
    /// `groupedby`. The Russian spellings diverge at their tenth
    /// character.
    #[regex(r"(?i)сгруппированопо|(?i)groupedby")]
    FnGroupedBy,

    /// `РАЗМЕРХРАНИМЫХДАННЫХ (StoredDataSize)` — size in bytes that
    /// the given fields occupy in the database.
    ///
    /// The Russian spelling is attested by the Developer's Reference
    /// §8.4.17.4.24; the syntax assistant carries only the English
    /// article.
    #[regex(r"(?i)размерхранимыхданных|(?i)storeddatasize")]
    FnStoredDataSize,

    /// `ПУСТАЯТАБЛИЦА (EMPTYTABLE)` — an empty nested table in the
    /// selection list, written `ПУСТАЯТАБЛИЦА.(<псевдонимы>)`.
    ///
    /// Not a function: the article «Пустые вложенные таблицы в списке
    /// выборки» calls it a keyword, and the query-function index does
    /// not list it. It exists so that the arms of a `ОБЪЕДИНИТЬ` can
    /// agree on their nested-table columns.
    #[regex(r"(?i)пустаятаблица|(?i)emptytable")]
    FnEmptyTable,

    /// `ПустаяСсылка (EmptyRef)` — the empty-reference selector inside
    /// `ЗНАЧЕНИЕ(<тип>.<объект>.ПустаяСсылка)`.
    ///
    /// Not a function: it is the value part of a predefined-data
    /// literal, per the article «Использование предопределенных данных
    /// конфигурации», which gives both spellings and the canonical
    /// example `ГДЕ Город = ЗНАЧЕНИЕ(Справочник.Города.ПустаяСсылка)`.
    #[regex(r"(?i)пустаяссылка|(?i)emptyref")]
    FnEmptyRef,

    /// `УНИКАЛЬНЫЙИДЕНТИФИКАТОР (UUID)` — the unique identifier behind
    /// a reference.
    #[regex(r"(?i)уникальныйидентификатор|(?i)uuid")]
    FnUUID,

    // ===================================================================
    // CLEAN-ROOM Slice 3b — metadata-object table vocabulary
    //
    // Table roots that name a metadata object as a query data source.
    // Every spelling below is the bilingual headline of an article in
    // the 1C:Enterprise 8.3.27 syntax assistant, section «Работа
    // с запросами → Таблицы запросов», which is the canonical inventory
    // of query tables; the ITS query-language textbook names it as such
    // (<https://its.1c.ru/db/pubqlang/content/7/hdoc>). Table names
    // exist in both languages by design — Developer's Reference Глава 8
    // §8.2, «Имя таблицы может быть задано на английском и русском
    // языках» — and are case-insensitive per §8.4.5, which is what the
    // `(?i)` flag expresses.
    //
    // Attestation: `docs/legal/sdbl-clean-room-slice3b.md`.
    //
    // Index (navigational only; the `#[regex]` attributes remain the
    // single source of truth, since logos requires the pattern at the
    // declaration site):
    //
    //   reference-type roots  MdoCatalog, MdoDocument, MdoEnum,
    //                         MdoChartOfCharacteristicTypes,
    //                         MdoChartOfAccounts,
    //                         MdoChartOfCalculationTypes,
    //                         MdoExchangePlan, MdoBusinessProcess,
    //                         MdoTask
    //   register roots        MdoInformationRegister,
    //                         MdoAccumulationRegister,
    //                         MdoAccountingRegister,
    //                         MdoCalculationRegister
    //   other real-table      MdoDocumentJournal, MdoConstant,
    //   roots                 MdoConstants, MdoSequence,
    //                         MdoFilterCriterion
    // ===================================================================
    /// `Справочник.<Имя справочника>` / `Catalog.<Имя справочника>`.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.4 lists
    /// `Справочник (Catalog)` among the predefined-value types usable in
    /// `ЗНАЧЕНИЕ(...)` — <https://its.1c.ru/db/v8327doc#bookmark:dev:TI000000453>.
    #[regex(r"(?i)справочник|(?i)catalog")]
    MdoCatalog,

    /// `Документ.<Имя документа>` / `Document.<Имя документа>`.
    #[regex(r"(?i)документ|(?i)document")]
    MdoDocument,

    /// `ЖурналДокументов.<Имя журнала документов>` /
    /// `DocumentJournal.<Имя журнала документов>`.
    ///
    /// Longest-match wins over [`MdoDocument`], whose Russian and
    /// English spellings are both proper prefixes of this one.
    #[regex(r"(?i)журналдокументов|(?i)documentjournal")]
    MdoDocumentJournal,

    /// `РегистрСведений.<Имя регистра сведений>` /
    /// `InformationRegister.<Имя регистра сведений>`.
    #[regex(r"(?i)регистрсведений|(?i)informationregister")]
    MdoInformationRegister,

    /// `РегистрНакопления.<Имя регистра накопления>` /
    /// `AccumulationRegister.<Имя регистра накопления>`.
    #[regex(r"(?i)регистрнакопления|(?i)accumulationregister")]
    MdoAccumulationRegister,

    /// `РегистрБухгалтерии.<Имя регистра бухгалтерии>` /
    /// `AccountingRegister.<Имя регистра бухгалтерии>`.
    #[regex(r"(?i)регистрбухгалтерии|(?i)accountingregister")]
    MdoAccountingRegister,

    /// `РегистрРасчета.<Имя регистра расчета>` /
    /// `CalculationRegister.<Имя регистра расчета>`.
    #[regex(r"(?i)регистррасчета|(?i)calculationregister")]
    MdoCalculationRegister,

    /// `ПланСчетов.<Имя плана счетов>` /
    /// `ChartOfAccounts.<Имя плана счетов>`.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.4.
    #[regex(r"(?i)плансчетов|(?i)chartofaccounts")]
    MdoChartOfAccounts,

    /// `ПланВидовРасчета.<Имя плана видов расчета>` /
    /// `ChartOfCalculationTypes.<Имя плана видов расчета>`.
    ///
    /// The canonical Russian spelling ends in the singular genitive
    /// `Расчета`, not `Расчетов`.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.4.
    #[regex(r"(?i)планвидоврасчета|(?i)chartofcalculationtypes")]
    MdoChartOfCalculationTypes,

    /// `ПланВидовХарактеристик.<Имя плана видов характеристик>` /
    /// `ChartOfCharacteristicTypes.<Имя плана видов характеристик>`.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.4.
    #[regex(r"(?i)планвидовхарактеристик|(?i)chartofcharacteristictypes")]
    MdoChartOfCharacteristicTypes,

    /// `ПланОбмена.<Имя плана обмена>` /
    /// `ExchangePlan.<Имя плана обмена>`.
    #[regex(r"(?i)планобмена|(?i)exchangeplan")]
    MdoExchangePlan,

    /// `Перечисление.<Имя перечисления>` / `Enum.<Имя перечисления>`.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.4, which
    /// also gives the canonical `ЗНАЧЕНИЕ(Перечисление.ВидыТоваров.Услуга)`
    /// form.
    #[regex(r"(?i)перечисление|(?i)enum")]
    MdoEnum,

    /// `БизнесПроцесс.<Имя бизнес-процесса>` /
    /// `BusinessProcess.<Имя бизнес-процесса>`.
    ///
    /// Second attestation: Developer's Reference Глава 8 §8.4.4, in the
    /// route-point form
    /// `БизнесПроцесс.<Имя>.ТочкаМаршрута.<Имя точки>`.
    #[regex(r"(?i)бизнеспроцесс|(?i)businessprocess")]
    MdoBusinessProcess,

    /// `Задача.<Имя задачи>` / `Task.<Имя задачи>`.
    #[regex(r"(?i)задача|(?i)task")]
    MdoTask,

    /// `Константа.<Имя константы>` / `Constant.<Const name>` — the table
    /// of one named constant.
    ///
    /// Distinct from [`MdoConstants`], the aggregate table of all
    /// constants; the two spellings differ by their final letter only.
    #[regex(r"(?i)константа|(?i)constant")]
    MdoConstant,

    /// `Константы` / `Constants` — the aggregate table holding every
    /// constant.
    ///
    /// Unlike the other roots this one takes no name part: it is a
    /// complete table reference on its own. Distinct from
    /// [`MdoConstant`]; see that variant.
    #[regex(r"(?i)константы|(?i)constants")]
    MdoConstants,

    /// `Последовательность.<Имя последовательности>` /
    /// `Sequence.<Имя последовательности>`.
    #[regex(r"(?i)последовательность|(?i)sequence")]
    MdoSequence,

    /// `КритерийОтбора.<Имя критерия отбора>` /
    /// `FilterCriterion.<Имя критерия отбора>`.
    #[regex(r"(?i)критерийотбора|(?i)filtercriterion")]
    MdoFilterCriterion,

    // ===================================================================
    // CLEAN-ROOM Slice 5 — virtual tables and external data sources
    //
    // A virtual table is computed when the query runs rather than
    // stored, and is named by a suffix appended to a real table's
    // dotted path — `РегистрСведений.<Имя>.СрезПоследних` and the like.
    // Every spelling below is one half of the bilingual headline of an
    // article in the 1C:Enterprise 8.3.27 syntax assistant, section
    // «Работа с запросами → Таблицы запросов», the same canonical
    // inventory the metadata-object roots come from. Table names exist
    // in both languages by design — Developer's Reference Глава 8 §8.2 —
    // and are case-insensitive per §8.4.5, which is what the `(?i)`
    // flag expresses.
    //
    // The suffix alone does not make a virtual table: the lexer has no
    // notion of how many dots preceded the current position, so
    // `Остатки` standing on its own emits the same token as `Остатки`
    // after two dots. Separating the two readings is the parser's work.
    //
    // Attestation: `docs/legal/sdbl-clean-room-slice5.md`.
    //
    // Index (navigational only; the `#[regex]` attributes remain the
    // single source of truth, since logos requires the pattern at the
    // declaration site). Grouped by the real table each suffix hangs
    // off:
    //
    //   external-source root  MdoExternalDataSource
    //   information register  VtSliceFirst, VtSliceLast
    //   accumulation and      VtBalance, VtTurnovers,
    //   accounting registers  VtBalanceAndTurnovers, VtDrCrTurnovers,
    //                         VtExtDimensions,
    //                         VtRecordsWithExtDimensions
    //   calculation register  VtScheduleData,
    //                         VtAdjustedEffectivePeriod
    //   sequence, business    VtBoundaries, VtPoints,
    //   process, task         VtTasksByPerformer
    //   external source       VtTable, VtCube, VtDimensionTable
    //   every root            VtChanges
    //
    // Two documented virtual tables have no variant here.
    // `База<Имя базового регистра расчета>` is a prefix glued to a
    // register name, so it is never a token on its own; recognising it
    // is name resolution over a metadata catalogue. And the Russian
    // half of `Изменения (Changes)` stays with
    // [`SdblTokenKind::KwUpdate`]; see that variant.
    // ===================================================================
    /// `ВнешнийИсточникДанных.<Имя внешнего источника данных>` /
    /// `ExternalDataSource.<Имя внешнего источника данных>` — the root
    /// of every external-source path.
    ///
    /// Deferred here by Slice 3b, which owns the other table roots,
    /// because an external-source path is only complete once it
    /// continues through [`SdblTokenKind::VtTable`] or
    /// [`SdblTokenKind::VtCube`].
    #[regex(r"(?i)внешнийисточникданных|(?i)externaldatasource")]
    MdoExternalDataSource,

    /// `РегистрСведений.<Имя>.СрезПервых` /
    /// `InformationRegister.<Имя>.SliceFirst` — the earliest record per
    /// combination of dimensions.
    #[regex(r"(?i)срезпервых|(?i)slicefirst")]
    VtSliceFirst,

    /// `РегистрСведений.<Имя>.СрезПоследних` /
    /// `InformationRegister.<Имя>.SliceLast` — the latest record per
    /// combination of dimensions.
    ///
    /// Second attestation: the ITS query-language textbook uses
    /// `РегистрСведений.Цены.СрезПоследних` as its worked example of a
    /// virtual table — <https://its.1c.ru/db/pubqlang/content/9/hdoc>.
    #[regex(r"(?i)срезпоследних|(?i)slicelast")]
    VtSliceLast,

    /// `РегистрНакопления.<Имя>.Остатки` /
    /// `AccumulationRegister.<Имя>.Balance`, and the same suffix on an
    /// accounting register.
    ///
    /// The canonical English is the singular `Balance` against the
    /// plural Russian `Остатки`.
    #[regex(r"(?i)остатки|(?i)balance")]
    VtBalance,

    /// `РегистрНакопления.<Имя>.Обороты` /
    /// `AccumulationRegister.<Имя>.Turnovers`, and the same suffix on an
    /// accounting register.
    #[regex(r"(?i)обороты|(?i)turnovers")]
    VtTurnovers,

    /// `РегистрНакопления.<Имя>.ОстаткиИОбороты` /
    /// `AccumulationRegister.<Имя>.BalanceAndTurnovers`.
    ///
    /// Longest-match wins over [`VtBalance`], whose Russian and English
    /// spellings are both proper prefixes of this one. The Russian form
    /// carries the conjunction `И` inside the word, giving the doubled
    /// `ии`.
    #[regex(r"(?i)остаткииобороты|(?i)balanceandturnovers")]
    VtBalanceAndTurnovers,

    /// `РегистрБухгалтерии.<Имя>.ОборотыДтКт` /
    /// `AccountingRegister.<Имя>.DrCrTurnovers` — turnovers broken down
    /// by corresponding debit and credit account.
    ///
    /// Longest-match wins over [`VtTurnovers`], whose Russian spelling
    /// is a proper prefix of this one.
    #[regex(r"(?i)оборотыдткт|(?i)drcrturnovers")]
    VtDrCrTurnovers,

    /// `РегистрБухгалтерии.<Имя>.Субконто` /
    /// `AccountingRegister.<Имя>.ExtDimensions` — the extra-dimension
    /// values of the register's records.
    ///
    /// Second attestation: the ITS query-language textbook chapter on
    /// the `Субконто` parameter —
    /// <https://its.1c.ru/db/pubqlang/content/114/hdoc>.
    #[regex(r"(?i)субконто|(?i)extdimensions")]
    VtExtDimensions,

    /// `РегистрБухгалтерии.<Имя>.ДвиженияССубконто` /
    /// `AccountingRegister.<Имя>.RecordsWithExtDimensions` — the
    /// register's records joined to their extra dimensions.
    #[regex(r"(?i)движенияссубконто|(?i)recordswithextdimensions")]
    VtRecordsWithExtDimensions,

    /// `РегистрРасчета.<Имя>.ДанныеГрафика` /
    /// `CalculationRegister.<Имя>.ScheduleData` — the schedule values
    /// underlying the register's action periods.
    #[regex(r"(?i)данныеграфика|(?i)scheduledata")]
    VtScheduleData,

    /// `РегистрРасчета.<Имя>.ФактическийПериодДействия` /
    /// `CalculationRegister.<Имя>.AdjustedEffectivePeriod` — action
    /// periods after displacement by higher-priority records.
    #[regex(r"(?i)фактическийпериоддействия|(?i)adjustedeffectiveperiod")]
    VtAdjustedEffectivePeriod,

    /// `Последовательность.<Имя>.Границы` /
    /// `Sequence.<Имя>.Boundaries` — the sequence's boundary per set of
    /// dimension values.
    #[regex(r"(?i)границы|(?i)boundaries")]
    VtBoundaries,

    /// `БизнесПроцесс.<Имя>.Точки` / `BusinessProcess.<Имя>.Points` —
    /// the route points of the business process.
    ///
    /// Distinct from `ТочкаМаршрута (RoutePoint)`, which is a field and
    /// an element of a predefined-data path, and therefore stays an
    /// identifier.
    #[regex(r"(?i)точки|(?i)points")]
    VtPoints,

    /// `Задача.<Имя>.ЗадачиПоИсполнителю` /
    /// `Task.<Имя>.TasksByPerformer` — tasks resolved through the
    /// addressing attributes of their performer.
    #[regex(r"(?i)задачипоисполнителю|(?i)tasksbyperformer")]
    VtTasksByPerformer,

    /// `ВнешнийИсточникДанных.<Имя>.Таблица.<Имя таблицы>` /
    /// `ExternalDataSource.<Имя>.Table.<Имя таблицы>`.
    #[regex(r"(?i)таблица|(?i)table")]
    VtTable,

    /// `ВнешнийИсточникДанных.<Имя>.Куб.<Имя куба>` /
    /// `ExternalDataSource.<Имя>.Cube.<Имя куба>`.
    #[regex(r"(?i)куб|(?i)cube")]
    VtCube,

    /// `ВнешнийИсточникДанных.<Имя>.Куб.<Имя куба>.ТаблицаИзмерения.<Имя таблицы>` /
    /// `ExternalDataSource.<Имя>.Cube.<Имя куба>.DimensionTable.<Имя таблицы>`.
    ///
    /// Longest-match wins over [`VtTable`], whose Russian spelling is a
    /// proper prefix of this one; the English pair is the other way
    /// round, and `table` cannot be entered part way through
    /// `dimensiontable` because matching starts at a token boundary.
    #[regex(r"(?i)таблицаизмерения|(?i)dimensiontable")]
    VtDimensionTable,

    /// `<Корень>.<Имя>.Изменения` / `<Root>.<Имя>.Changes` — the
    /// change-registration table, defined for fourteen roots.
    ///
    /// English spelling only. The Russian `Изменения` is byte-identical
    /// to the `ДЛЯ ИЗМЕНЕНИЯ (FOR UPDATE)` clause keyword, which
    /// [`SdblTokenKind::KwUpdate`] already claims; a lexer cannot tell
    /// the two apart, because the difference is grammatical position.
    /// Leaving `Изменения` on the clause keyword keeps `ДЛЯ ИЗМЕНЕНИЯ`
    /// correct and hands the table reading to the parser, which sees the
    /// preceding dot.
    ///
    /// This is the mirror of [`SdblTokenKind::FnLeft`], where the
    /// collision fell on the English spelling and the Russian one was
    /// free. The rule is the same in both directions: the colliding
    /// spelling stays with the incumbent owner.
    #[regex(r"(?i)changes")]
    VtChanges,

    // ===================================================================
    // CLEAN-ROOM Slice 3b — lexer error fallback
    //
    // Attestation: `docs/legal/sdbl-clean-room-slice3b.md`.
    // ===================================================================
    /// Substituted for a failed match, so that input no rule accepts
    /// still reaches the parser as a token covering its own byte range.
    ///
    /// This is not vocabulary and carries no pattern: no 1C source
    /// defines an error token, because the documented query language
    /// describes what is accepted rather than how a tool should
    /// represent what is not. Keeping the offending span in the stream
    /// is what lets an editor point at it instead of at whatever
    /// followed.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdblToken {
    pub kind: SdblTokenKind,
    pub text: SmolStr,
    pub offset: usize,
}

pub fn tokenize_sdbl(input: &str) -> Vec<SdblToken> {
    let mut result = Vec::new();
    let mut pos = 0;

    while pos < input.len() {
        let remaining = &input[pos..];

        if remaining.starts_with('"') {
            let strings = strings_mode::scan(input, pos);
            result.extend(strings.tokens);
            pos = strings.end_pos;
        } else {
            let mut lexer = SdblTokenKind::lexer(remaining);
            if let Some(token_result) = lexer.next() {
                let kind = token_result.unwrap_or(SdblTokenKind::Error);
                if kind == SdblTokenKind::Quote {
                    unreachable!("Quote should be handled above");
                }
                let text = SmolStr::new(lexer.slice());
                let offset = pos;
                result.push(SdblToken { kind, text: text.clone(), offset });
                pos += text.len();
            } else {
                break;
            }
        }
    }

    result
}

/// Whether the query language itself knows `word` (a function, virtual table, keyword or type).
///
/// The parser collapses every `Fn*`, `Vt*`, `Kw*` and type token to a plain identifier, so this
/// lexer is the only place where "the language knows this word" survives. Metadata roots and
/// period names collapse too, but neither names anything that can be called, so a call spelled
/// with one of them is as wrong as a typo.
pub fn is_sdbl_vocabulary_word(word: &str) -> bool {
    use SdblTokenKind as K;

    let mut significant = tokenize_sdbl(word)
        .into_iter()
        .filter(|token| !matches!(token.kind, K::Whitespace | K::Newline));
    match (significant.next(), significant.next()) {
        (Some(token), None) => !matches!(
            token.kind,
            K::Ident
                | K::PeriodTenDays
                | K::PeriodHalfYear
                | K::MdoCatalog
                | K::MdoDocument
                | K::MdoDocumentJournal
                | K::MdoInformationRegister
                | K::MdoAccumulationRegister
                | K::MdoAccountingRegister
                | K::MdoCalculationRegister
                | K::MdoChartOfAccounts
                | K::MdoChartOfCalculationTypes
                | K::MdoChartOfCharacteristicTypes
                | K::MdoExchangePlan
                | K::MdoEnum
                | K::MdoBusinessProcess
                | K::MdoTask
                | K::MdoConstant
                | K::MdoConstants
                | K::MdoSequence
                | K::MdoFilterCriterion
                | K::MdoExternalDataSource
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_words_are_recognised() {
        for word in ["ВРЕГ", "СокрЛП", "Цел", "Sqrt", "Остатки", "ТИП", "ВЫРАЗИТЬ"]
        {
            assert!(is_sdbl_vocabulary_word(word), "{word} must be a vocabulary word");
        }
    }

    #[test]
    fn invented_words_are_not_vocabulary() {
        for word in ["ПОДСТРКА", "ВЫДУМАННАЯФУНКЦИЯ", "КОНКАТ", "ДОБАВИТЬМЕСЯЦ", "", "Т.Имя"]
        {
            assert!(!is_sdbl_vocabulary_word(word), "{word:?} must not be a vocabulary word");
        }
    }

    #[test]
    fn test_select_statement() {
        let tokens = tokenize_sdbl("SELECT Name FROM Catalog.Products");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwSelect);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::KwFrom);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::MdoCatalog);
        assert_eq!(tokens[7].kind, SdblTokenKind::Dot);
        assert_eq!(tokens[8].kind, SdblTokenKind::Ident);
    }

    #[test]
    fn test_russian_keywords() {
        let tokens = tokenize_sdbl("ВЫБРАТЬ Наименование ИЗ Справочник.Товары");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwSelect);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::KwFrom);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::MdoCatalog);
        assert_eq!(tokens[7].kind, SdblTokenKind::Dot);
        assert_eq!(tokens[8].kind, SdblTokenKind::Ident);
    }

    #[test]
    fn test_where_clause() {
        let tokens = tokenize_sdbl("WHERE Price > 100");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwWhere);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::Gt);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::Decimal);
    }

    #[test]
    fn test_join() {
        let tokens =
            tokenize_sdbl("LEFT JOIN Catalog.Categories ON Products.Category = Categories.Ref");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwLeft);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::KwJoin);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::MdoCatalog);
    }

    #[test]
    fn test_aggregate_functions() {
        let tokens = tokenize_sdbl("SUM(Amount) AS Total");
        assert_eq!(tokens[0].kind, SdblTokenKind::FnSum);
        assert_eq!(tokens[1].kind, SdblTokenKind::LParen);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::RParen);
        assert_eq!(tokens[4].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[5].kind, SdblTokenKind::KwAs);
        assert_eq!(tokens[6].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[7].kind, SdblTokenKind::Ident);
    }

    #[test]
    fn test_parameters() {
        let tokens = tokenize_sdbl("WHERE Date > &StartDate");
        assert_eq!(tokens[6].kind, SdblTokenKind::Parameter);
        assert_eq!(tokens[6].text.as_str(), "&StartDate");
    }

    #[test]
    fn test_temporary_table() {
        let tokens = tokenize_sdbl("INTO #TempTable");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwInto);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::Hash);
        assert_eq!(tokens[3].kind, SdblTokenKind::Ident);
    }

    #[test]
    fn test_virtual_table() {
        let tokens = tokenize_sdbl("FROM AccumulationRegister.Stock.Balance");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwFrom);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::MdoAccumulationRegister);
        assert_eq!(tokens[3].kind, SdblTokenKind::Dot);
        assert_eq!(tokens[4].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[5].kind, SdblTokenKind::Dot);
        assert_eq!(tokens[6].kind, SdblTokenKind::VtBalance);
    }

    #[test]
    fn test_case_expression() {
        let tokens = tokenize_sdbl("CASE WHEN Amount > 0 THEN 1 ELSE 0 END");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwCase);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::KwWhen);
        assert_eq!(tokens[10].kind, SdblTokenKind::KwThen);
        assert_eq!(tokens[14].kind, SdblTokenKind::KwElse);
        assert_eq!(tokens[18].kind, SdblTokenKind::KwEnd);
    }

    #[test]
    fn test_string_literal() {
        let tokens = tokenize_sdbl(r#""Hello World""#);
        assert_eq!(tokens[0].kind, SdblTokenKind::String);
    }

    #[test]
    fn test_date_literal() {
        let tokens = tokenize_sdbl("'20240101'");
        assert_eq!(tokens[0].kind, SdblTokenKind::Date);
    }

    #[test]
    fn test_boolean_literals() {
        let tokens = tokenize_sdbl("TRUE FALSE Истина Ложь");
        assert_eq!(tokens[0].kind, SdblTokenKind::LitTrue);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::LitFalse);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::LitTrue);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::LitFalse);
    }

    #[test]
    fn test_comment_separator_line() {
        let input = "SELECT 1;\n////////////////////////////////////////////////////////////////////////////////\nSELECT 2";
        let tokens = tokenize_sdbl(input);

        let semicolon_pos = tokens.iter().position(|t| t.kind == SdblTokenKind::Semicolon).unwrap();
        let comment_pos = tokens.iter().position(|t| t.kind == SdblTokenKind::Comment);

        println!("Tokens around semicolon:");
        for (i, token) in tokens.iter().enumerate() {
            if i >= semicolon_pos.saturating_sub(2) && i <= semicolon_pos + 5 {
                println!(
                    "  [{} {}]: {:?} = {:?}",
                    i,
                    if i == semicolon_pos { "←" } else { " " },
                    token.kind,
                    token.text
                );
            }
        }

        assert!(comment_pos.is_some(), "Comment token should be found for ////////////////");
        println!("Comment found at position {}", comment_pos.unwrap());
    }

    #[test]
    fn test_date_functions() {
        let tokens = tokenize_sdbl("YEAR(Date) MONTH(Date) DAY(Date)");
        assert_eq!(tokens[0].kind, SdblTokenKind::FnYear);
        assert_eq!(tokens[4].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[5].kind, SdblTokenKind::FnMonth);
        assert_eq!(tokens[9].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[10].kind, SdblTokenKind::FnDay);
    }

    #[test]
    fn test_string_functions() {
        let tokens = tokenize_sdbl("SUBSTRING(Name, 1, 10) UPPER(Name)");
        assert_eq!(tokens[0].kind, SdblTokenKind::FnSubstring);
        assert_eq!(tokens[10].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[11].kind, SdblTokenKind::FnUpper);
    }

    #[test]
    fn test_group_by() {
        let tokens = tokenize_sdbl("GROUP BY Category HAVING COUNT(*) > 5");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwGroup);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::KwOnOrBy);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::KwHaving);
        assert_eq!(tokens[7].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[8].kind, SdblTokenKind::FnCount);
    }

    #[test]
    fn test_order_by() {
        let tokens = tokenize_sdbl("ORDER BY Name ASC, Price DESC");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwOrder);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::KwOnOrBy);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::KwAsc);
        assert_eq!(tokens[9].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[10].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[11].kind, SdblTokenKind::KwDesc);
    }

    #[test]
    fn test_union() {
        let tokens = tokenize_sdbl("SELECT * FROM Table1 UNION ALL SELECT * FROM Table2");
        assert_eq!(tokens[8].kind, SdblTokenKind::KwUnion);
        assert_eq!(tokens[9].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[10].kind, SdblTokenKind::KwAll);
    }

    #[test]
    fn test_distinct_top() {
        let tokens = tokenize_sdbl("SELECT DISTINCT TOP 100 Name");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwSelect);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::KwDistinct);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::KwTop);
    }

    #[test]
    fn test_in_predicate() {
        let tokens = tokenize_sdbl("WHERE Category IN (&CategoryList)");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwWhere);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::KwIn);
    }

    #[test]
    fn test_between_predicate() {
        let tokens = tokenize_sdbl("WHERE Price BETWEEN 100 AND 500");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwWhere);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::KwBetween);
        assert_eq!(tokens[6].kind, SdblTokenKind::Decimal);
        assert_eq!(tokens[7].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[8].kind, SdblTokenKind::OpAnd);
    }

    #[test]
    fn test_like_predicate() {
        let tokens = tokenize_sdbl("WHERE Name LIKE \"%Products%\"");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwWhere);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::KwLike);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::String);
    }

    #[test]
    fn test_is_null() {
        let tokens = tokenize_sdbl("WHERE Description IS NULL");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwWhere);
        assert_eq!(tokens[1].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::KwIs);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::LitNull);
    }

    #[test]
    fn test_cast() {
        let tokens = tokenize_sdbl("CAST(Price AS NUMBER(15, 2))");
        assert_eq!(tokens[0].kind, SdblTokenKind::KwCast);
        assert_eq!(tokens[1].kind, SdblTokenKind::LParen);
        assert_eq!(tokens[2].kind, SdblTokenKind::Ident);
        assert_eq!(tokens[3].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[4].kind, SdblTokenKind::KwAs);
        assert_eq!(tokens[5].kind, SdblTokenKind::Whitespace);
        assert_eq!(tokens[6].kind, SdblTokenKind::TypeNumber);
    }

    #[test]
    fn test_multiline_string_behavior() {
        let input = "SELECT\n   \"\" AS Field";
        let tokens = tokenize_sdbl(input);

        println!("\nMultiline string test tokens:");
        for (i, token) in tokens.iter().enumerate() {
            println!("  {}: {:?} = {:?}", i, token.kind, token.text);
        }

        assert!(!tokens.is_empty(), "Should produce some tokens");
    }

    #[test]
    fn test_string_without_newline() {
        let tokens = tokenize_sdbl(r#""simple""#);
        let string_tokens: Vec<_> =
            tokens.iter().filter(|t| t.kind == SdblTokenKind::String).collect();
        assert_eq!(string_tokens.len(), 3);
        assert_eq!(string_tokens[0].text, r#"""#);
        assert_eq!(string_tokens[1].text, r#"simple"#);
        assert_eq!(string_tokens[2].text, r#"""#);
    }

    #[test]
    fn test_string_with_embedded_newline() {
        let input = "\"text\nmore\"";
        let tokens = tokenize_sdbl(input);

        println!("\nString with embedded newline tokens:");
        for (i, token) in tokens.iter().enumerate() {
            println!("  {}: {:?} = {:?}", i, token.kind, token.text);
        }

        assert!(!tokens.is_empty(), "Should produce tokens");
    }

    #[test]
    fn test_multiline_string_splitting() {
        let input = r#"   " КАК,
   " КАК,
   " КАК,"#;
        let tokens = tokenize_sdbl(input);

        let string_tokens: Vec<_> =
            tokens.iter().filter(|t| t.kind == SdblTokenKind::String).collect();

        assert!(
            string_tokens.len() >= 2,
            "Expected at least 2 String tokens after splitting, got {}",
            string_tokens.len()
        );
    }
}
