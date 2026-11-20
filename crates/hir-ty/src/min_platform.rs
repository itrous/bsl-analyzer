//! Since which platform release a resolved platform member exists, for the
//! `PlatformMemberNewerThanMinVersion` check.
//!
//! The answer comes from the catalog's "available since" (`min_version`) strings.
//! A member with no parseable version yields `None`, and the check says nothing
//! about it: absence of data is never evidence that a member is new.

use bsl_platform::{PlatformDataInner, PlatformGlobalCatalog, PlatformVersion};

/// `Асинх` procedures and the `Ждать` operator: platform 8.3.18 and later. Checked
/// live 07.10.2026 — 8.3.17.1549 rejects an `Асинх` procedure with «Неопознанный
/// оператор», 8.3.27.2214 compiles it in any 8.3.x compatibility mode.
pub const ASYNC_INTRODUCED: PlatformVersion =
    PlatformVersion { major: 8, minor: 3, patch: 18, build: None };

/// Catalog dates contradicted by running code, as (owner type in English, member in
/// Russian): treated as undated, so the check stays silent on them. Only members
/// proven live on the older platform belong here.
/// - `ХешированиеДанных.ХешСумма` is dated 8.3.18 (the type 8.3.1). Probe
///   07.10.2026: on 8.3.17.1549
///   `Х = Новый ХешированиеДанных(ХешФункция.MD5); Х.Добавить("a"); Строка(Х.ХешСумма)`
///   returns `0C C1 75 B9 … 26 61` (MD5 of "a"), as 8.3.27.2214 does. Four other
///   members dated 8.3.18 on types of 8.0-8.3.1 fail on 8.3.17 in the same run with
///   «Поле объекта не обнаружено» (`ХешФункция.SHA512`,
///   `СписокПолнотекстовогоПоиска.ОграничиватьСтрокуПоиска`,
///   `ТекстИнтернетПочтовогоСообщения.ПроизвольныйТипТекста`,
///   `ТипТекстаПочтовогоСообщения.ПроизвольныйТекст`): the 8.3.18 dating holds in
///   general and this property is the exception. The BSP (`ОбщегоНазначения`,
///   `ПользователиСлужебный`) reads it on every 8.3.17 base.
const MISDATED_MEMBERS: &[(&str, &str)] = &[("DataHashing", "ХешСумма")];

/// Types whose own catalog date is contradicted by an older help, as the type's
/// English name: treated as undated, so neither the type nor the members raised to
/// its date are reported.
/// - `РежимРедактированияКолонки` is dated 8.5.1 by the platform help of 8.5.1.1343,
///   but 8.0 by the pinned 8.3.27.2214 extract and by the 8.3.17 help
///   (`shcntx_ru.hbk:objects/catalog2/catalog277/ColumnEditMode.html`: «Доступен,
///   начиная с версии 8.0.»). Probe 20.11.2026: on 8.3.17.1549, server context, the
///   enumeration and its three old values exist and print
///   `Непосредственно=Непосредственно; Вход=Вход; ВходПриВводе=Вход при вводе`, while
///   `Авто` fails with «Поле объекта не обнаружено (Авто)». So the date 8.5.1 of the
///   TYPE is contradicted by the running platform, and the value `Авто` (`Auto`) really
///   is new in 8.5.1: this is why only the type is corrected, and `Авто` keeps its own
///   8.5.1 date and is still reported. One enumeration gives the control pair, two of
///   its values differing in exactly this way. The likely cause of the re-dating is
///   that the new value made 1C re-date the whole page; the other three values keep
///   8.0 in the 8.5.1 help.
///
/// Undated rather than "the older date": `None` means no data, and absence of data is
/// never evidence that a member is new, so the check stays silent, which is right
/// because the true date 8.0 is below any minimum platform in use.
const MISDATED_TYPES: &[&str] = &["ColumnEditMode"];

fn is_misdated(owner_en: &str, member_ru: &str) -> bool {
    MISDATED_MEMBERS.iter().any(|(owner, member)| {
        owner.eq_ignore_ascii_case(owner_en) && member.to_lowercase() == member_ru.to_lowercase()
    })
}

fn is_misdated_type(type_en: &str) -> bool {
    MISDATED_TYPES.iter().any(|ty| ty.eq_ignore_ascii_case(type_en))
}

fn parse(value: Option<&str>) -> Option<PlatformVersion> {
    value.and_then(PlatformVersion::parse_catalog)
}

/// A member is no older than the type that carries it: the help lists 593 methods
/// with a version below their owner's, which the owner's own date overrides.
fn with_owner(
    member: Option<PlatformVersion>,
    owner: Option<PlatformVersion>,
) -> Option<PlatformVersion> {
    let member = member?;
    Some(match owner {
        Some(owner) if owner.release_newer_than(member) => owner,
        _ => member,
    })
}

fn type_version(data: &PlatformDataInner, type_name: &str) -> Option<PlatformVersion> {
    if data.is_ambiguous_type_name(type_name) {
        return None;
    }
    let ty = data.get_type(type_name)?;
    // `type_name` may be Russian or English: compare the resolved English name.
    if is_misdated_type(&ty.english_name) {
        return None;
    }
    parse(ty.min_version.as_deref())
}

/// A platform type named in code by itself (a system enumeration such as
/// `СпособКодированияСтроки`).
pub fn platform_type(type_name: &str) -> Option<PlatformVersion> {
    type_version(PlatformDataInner::instance(), type_name)
}

/// A global function (`СтрНайти(...)`) called by its bare name.
pub fn global_function(name: &str) -> Option<PlatformVersion> {
    let data = PlatformDataInner::instance();
    if let Some(function) = data.get_global_function(name) {
        return parse(function.min_version.as_deref());
    }
    let symbol = PlatformGlobalCatalog::instance().lookup(name)?;
    (symbol.kind == bsl_platform::PlatformGlobalKind::Function)
        .then(|| parse(symbol.min_version.as_deref()))
        .flatten()
}

/// A Global-context property (`ХранилищеДвоичныхДанных`) read by its bare name.
pub fn global_property(name: &str) -> Option<PlatformVersion> {
    let data = PlatformDataInner::instance();
    if let Some(property) = data.get_global_property(name) {
        return parse(property.min_version.as_deref());
    }
    let symbol = PlatformGlobalCatalog::instance().lookup(name)?;
    (symbol.kind == bsl_platform::PlatformGlobalKind::Property)
        .then(|| parse(symbol.min_version.as_deref()))
        .flatten()
}

/// `Новый <Тип>(...)`. The overload actually used is not known here, so the type's
/// own date is raised only when EVERY constructor of it is dated later.
pub fn constructed_type(type_name: &str) -> Option<PlatformVersion> {
    let data = PlatformDataInner::instance();
    let ty = type_version(data, type_name)?;
    let constructors = data.get_constructors(type_name);
    let oldest_constructor = constructors
        .iter()
        .map(|constructor| parse(constructor.min_version.as_deref()))
        .try_fold(None::<PlatformVersion>, |oldest, version| {
            let version = version?;
            Some(Some(match oldest {
                Some(oldest) if version.release_newer_than(oldest) => oldest,
                _ => version,
            }))
        })
        .flatten();
    Some(match oldest_constructor {
        Some(constructor) if constructor.release_newer_than(ty) => constructor,
        _ => ty,
    })
}

/// A method or property of a value the analyzer typed as the platform type
/// `owner_type` — effective version max(member, owner type).
pub fn type_member(owner_type: &str, member: &str, is_property: bool) -> Option<PlatformVersion> {
    let data = PlatformDataInner::instance();
    if data.is_ambiguous_type_name(owner_type) {
        return None;
    }
    let (type_en, name_ru, version) = if is_property {
        let property = data.get_property(owner_type, member)?;
        (&property.type_name, &property.name, property.min_version.as_deref())
    } else {
        let method = data.get_method(owner_type, member)?;
        (&method.type_name, &method.name, method.min_version.as_deref())
    };
    if is_misdated(type_en, name_ru) {
        return None;
    }
    let version = parse(version);
    with_owner(version, type_version(data, owner_type))
}

/// `<ГлобальноеСвойство>.<Метод>(...)` (`ХранилищеДвоичныхДанных.Создать()`): the
/// method, no older than the property that exposes it or its declared type.
pub fn global_member(global_name: &str, member: &str) -> Option<PlatformVersion> {
    let data = PlatformDataInner::instance();
    let method = data.resolve_global_member(global_name, member)?;
    let version = parse(method.min_version.as_deref());
    let version = with_owner(version, type_version(data, method.type_name.as_str()));
    with_owner(version, global_property(global_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> PlatformVersion {
        PlatformVersion::parse_catalog(text).unwrap()
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn catalog_dates_reach_each_member_kind() {
        assert_eq!(global_function("СтрЗаменитьПоРегулярномуВыражению"), Some(v("8.3.23")));
        assert_eq!(global_function("StrReplaceByRegularExpression"), Some(v("8.3.23")));
        assert_eq!(global_function("СтрНайти"), Some(v("8.3.6")));
        assert_eq!(global_function("НетТакойФункции"), None);
        assert_eq!(global_property("ХранилищеДвоичныхДанных"), Some(v("8.3.23")));
        assert_eq!(constructed_type("ГенераторСлучайныхПаролей"), Some(v("8.3.22")));
        assert_eq!(constructed_type("Массив"), Some(v("8.0")));
        assert_eq!(type_member("HTTPЗапрос", "ДобавитьТокенДоступа", false), Some(v("8.3.21")));
        assert_eq!(type_member("HTTPRequest", "ДобавитьТокенДоступа", false), Some(v("8.3.21")));
        assert_eq!(
            type_member("СистемнаяИнформация", "ВариантПриложения", true),
            Some(v("8.3.22"))
        );
        assert_eq!(type_member("Массив", "Добавить", false), Some(v("8.0")));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_misdated_member_reads_as_undated() {
        assert_eq!(type_member("ХешированиеДанных", "ХешСумма", true), None);
        assert_eq!(type_member("DataHashing", "HashSum", true), None);
        assert_eq!(type_member("ХешированиеДанных", "Добавить", false), Some(v("8.3.1")));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_misdated_type_reads_as_undated_in_both_languages() {
        assert_eq!(platform_type("РежимРедактированияКолонки"), None);
        assert_eq!(platform_type("ColumnEditMode"), None);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_value_of_a_misdated_type_keeps_its_own_date() {
        assert_eq!(
            type_member("РежимРедактированияКолонки", "Непосредственно", true),
            Some(v("8.0"))
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn the_corrections_leave_neighbouring_entries_dated() {
        assert_eq!(platform_type("ВидПоляФормы"), Some(v("8.2")));
        assert_eq!(
            type_member("ТаблицаФормы", "ПоведениеПриСжатииПоГоризонтали", true),
            Some(v("8.3.12"))
        );
    }

    #[test]
    fn a_member_is_no_older_than_its_owner() {
        assert_eq!(with_owner(Some(v("8.0")), Some(v("8.3.21"))), Some(v("8.3.21")));
        assert_eq!(with_owner(Some(v("8.3.22")), Some(v("8.2"))), Some(v("8.3.22")));
        assert_eq!(with_owner(None, Some(v("8.3.21"))), None, "an undated member stays silent");
    }
}
