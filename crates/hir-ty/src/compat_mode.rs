//! Platform names a configuration's compatibility mode hides, for the
//! `PlatformMemberHiddenByCompatibilityMode` check.
//!
//! Measured, not taken from the help. Probe of 07.10.2026: one external data
//! processor per name of the 8.3.27 global context (604 functions and
//! properties), each compiled on file bases that differ only in
//! `CompatibilityMode` (8.2.13, 8.3.1 … 8.3.19, 8.3.27) on platforms 8.3.27.2214
//! and 8.3.17.1549. Exactly the 30 names below fail to compile below their
//! threshold mode — «Процедура или функция с указанным именем не определена (X)»
//! for a function, «Переменная не определена (X)» for a property — and compile
//! from it on. Every other global, every type, method and `Асинх` member stays
//! visible even in 8.2.13. English aliases are hidden the same way. The help's
//! «в режиме совместимости с версией …» note is no substitute: `СтрШаблон` is
//! annotated 8.3.5 but hidden until 8.3.6, and 24 of these 30 names carry no
//! such note in the bundled catalog.
//!
//! External data processors compile under the mode of the base they are opened in
//! (same probe), so the configuration's mode applies to them too.

use bsl_platform::PlatformVersion;

const fn v(minor: u32, patch: u32) -> PlatformVersion {
    PlatformVersion { major: 8, minor, patch, build: None }
}

/// (Russian name, English name, first compatibility mode in which it is visible).
const HIDDEN_GLOBALS: &[(&str, &str, PlatformVersion)] = &[
    (
        "ХранилищеПользовательскихНастроекДинамическихСписков",
        "DynamicListsUserSettingsStorage",
        v(3, 3),
    ),
    (
        "ПолучитьУникальныйИдентификаторСПоддержкойСовместимости",
        "GetUUIDWithCompatibilitySupport",
        v(3, 6),
    ),
    ("СтрЗаканчиваетсяНа", "StrEndsWith", v(3, 6)),
    ("СтрНайти", "StrFind", v(3, 6)),
    ("СтрНачинаетсяС", "StrStartsWith", v(3, 6)),
    ("СтрРазделить", "StrSplit", v(3, 6)),
    ("СтрСоединить", "StrConcat", v(3, 6)),
    ("СтрСравнить", "StrCompare", v(3, 6)),
    ("СтрШаблон", "StrTemplate", v(3, 6)),
    ("ПолучитьКоличествоЗаданийПересчетаИтогов", "GetTotalRecalcJobCount", v(3, 9)),
    ("ПолучитьОтключениеБезопасногоРежима", "GetSafeModeDisabled", v(3, 9)),
    ("ПродолжитьВызов", "ProceedWithCall", v(3, 9)),
    ("СтрЗаменитьПоРегулярномуВыражению", "StrReplaceByRegularExpression", v(3, 9)),
    ("СтрНайтиВсеПоРегулярномуВыражению", "StrFindAllByRegularExpression", v(3, 9)),
    ("СтрНайтиПоРегулярномуВыражению", "StrFindByRegularExpression", v(3, 9)),
    ("СтрПодобнаПоРегулярномуВыражению", "StrLikeByRegularExpression", v(3, 9)),
    ("УстановитьКоличествоЗаданийПересчетаИтогов", "SetTotalRecalcJobCount", v(3, 9)),
    ("УстановитьОтключениеБезопасногоРежима", "SetSafeModeDisabled", v(3, 9)),
    ("ПобитовоеИ", "BitwiseAnd", v(3, 11)),
    ("ПобитовоеИНе", "BitwiseAndNot", v(3, 11)),
    ("ПобитовоеИли", "BitwiseOr", v(3, 11)),
    ("ПобитовоеИсключительноеИли", "BitwiseXor", v(3, 11)),
    ("ПобитовоеНе", "BitwiseNot", v(3, 11)),
    ("ПобитовыйСдвигВлево", "BitwiseShiftLeft", v(3, 11)),
    ("ПобитовыйСдвигВправо", "BitwiseShiftRight", v(3, 11)),
    ("ПроверитьБит", "CheckBit", v(3, 11)),
    ("ПроверитьПоБитовойМаске", "CheckByBitMask", v(3, 11)),
    ("УстановитьБит", "SetBit", v(3, 11)),
    ("КопииБазыДанных", "DatabaseCopies", v(3, 14)),
    ("ХранилищеВнешнихДанныхНавигационныхСсылок", "URLExternalDataStorage", v(3, 19)),
];

/// (owner type in English, Russian member, English member, first visible mode).
/// `Запрос.ТребуемаяАктуальностьДанных` below 8.3.14 compiles (members are
/// late-bound) and fails when run: «Поле объекта не обнаружено».
const HIDDEN_TYPE_MEMBERS: &[(&str, &str, &str, PlatformVersion)] =
    &[("Query", "ТребуемаяАктуальностьДанных", "RequiredDataRelevance", v(3, 14))];

fn same_name(left: &str, right: &str) -> bool {
    left.chars().count() == right.chars().count()
        && left.chars().flat_map(char::to_lowercase).eq(right.chars().flat_map(char::to_lowercase))
}

/// The first compatibility mode in which the global function or property `name`
/// is visible, when some mode hides it; `None` for every name no mode hides.
pub fn hidden_global(name: &str) -> Option<PlatformVersion> {
    HIDDEN_GLOBALS
        .iter()
        .find(|(ru, en, _)| same_name(ru, name) || same_name(en, name))
        .map(|(_, _, threshold)| *threshold)
}

/// The first compatibility mode in which `member` of the platform type
/// `owner_type` (Russian or English name) is visible, when some mode hides it.
pub fn hidden_type_member(owner_type: &str, member: &str) -> Option<PlatformVersion> {
    let data = bsl_platform::PlatformDataInner::instance();
    let owner_en = data.get_type(owner_type).map(|ty| ty.english_name.as_str())?;
    HIDDEN_TYPE_MEMBERS
        .iter()
        .find(|(owner, ru, en, _)| {
            owner.eq_ignore_ascii_case(owner_en) && (same_name(ru, member) || same_name(en, member))
        })
        .map(|(_, _, _, threshold)| *threshold)
}

/// The mode the code compiles under, or `None` when it hides nothing: no mode
/// known, `DontUse`, or a value that is not a mode.
pub fn effective_mode(value: Option<&str>) -> Option<PlatformVersion> {
    PlatformVersion::parse_compatibility_mode(value?).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_reach_russian_and_english_names() {
        assert_eq!(hidden_global("СтрНайти"), Some(v(3, 6)));
        assert_eq!(hidden_global("strfind"), Some(v(3, 6)));
        assert_eq!(hidden_global("ПобитовоеИ"), Some(v(3, 11)));
        assert_eq!(hidden_global("ХранилищеВнешнихДанныхНавигационныхСсылок"), Some(v(3, 19)));
        assert_eq!(hidden_global("Найти"), None);
        assert_eq!(hidden_global("ГенераторСлучайныхПаролей"), None);
        assert_eq!(HIDDEN_GLOBALS.len(), 30, "the probe found exactly 30 hidden globals");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn query_relevance_is_the_one_hidden_member() {
        assert_eq!(hidden_type_member("Запрос", "ТребуемаяАктуальностьДанных"), Some(v(3, 14)));
        assert_eq!(hidden_type_member("Query", "RequiredDataRelevance"), Some(v(3, 14)));
        assert_eq!(hidden_type_member("Запрос", "Текст"), None);
    }

    #[test]
    fn dont_use_and_garbage_hide_nothing() {
        assert_eq!(
            effective_mode(Some("Version8_2_13")),
            Some(PlatformVersion { major: 8, minor: 2, patch: 13, build: None })
        );
        assert_eq!(effective_mode(Some("DontUse")), None);
        assert_eq!(effective_mode(Some("8.3.x")), None);
        assert_eq!(effective_mode(Some("8.2.13 (опечатка)")), None);
        assert_eq!(effective_mode(Some("Version8_2_13 (опечатка)")), None);
        assert_eq!(effective_mode(Some("1.0")), None);
        assert_eq!(effective_mode(Some("8.3.17.1549")), None);
        assert_eq!(
            effective_mode(Some("Version8_1")),
            Some(PlatformVersion { major: 8, minor: 1, patch: 0, build: None })
        );
        assert_eq!(effective_mode(None), None);
    }
}
