//! Since which platform release a resolved platform member exists, for the
//! `PlatformMemberNewerThanMinVersion` check.
//!
//! The answer comes from the catalog's "available since" (`min_version`) strings.
//! A member with no parseable version yields `None`, and the check says nothing
//! about it: absence of data is never evidence that a member is new. Catalog dates
//! contradicted by a running platform are corrected by the curated overlays of
//! `bsl-platform`, so this module consumes already-corrected data.

use bsl_platform::{PlatformDataInner, PlatformGlobalCatalog, PlatformVersion};

/// `Асинх` procedures and the `Ждать` operator: platform 8.3.18 and later. Checked
/// live 07.10.2026 — 8.3.17.1549 rejects an `Асинх` procedure with «Неопознанный
/// оператор», 8.3.27.2214 compiles it in any 8.3.x compatibility mode.
pub const ASYNC_INTRODUCED: PlatformVersion =
    PlatformVersion { major: 8, minor: 3, patch: 18, build: None };

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
    parse(data.get_type(type_name)?.min_version.as_deref())
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
    let version = if is_property {
        data.get_property(owner_type, member)?.min_version.as_deref()
    } else {
        data.get_method(owner_type, member)?.min_version.as_deref()
    };
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
    fn a_misdated_member_reads_with_the_corrected_date() {
        assert_eq!(type_member("ХешированиеДанных", "ХешСумма", true), Some(v("8.3.1")));
        assert_eq!(type_member("DataHashing", "HashSum", true), Some(v("8.3.1")));
        assert_eq!(type_member("ХешированиеДанных", "Добавить", false), Some(v("8.3.1")));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_misdated_type_reads_with_the_corrected_date_in_both_languages() {
        assert_eq!(platform_type("РежимРедактированияКолонки"), Some(v("8.0")));
        assert_eq!(platform_type("ColumnEditMode"), Some(v("8.0")));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_value_of_a_misdated_type_keeps_its_own_date() {
        assert_eq!(
            type_member("РежимРедактированияКолонки", "Непосредственно", true),
            Some(v("8.0"))
        );
        // The control: the value that really is new keeps its own date, so correcting
        // the type does not swallow a genuine finding. A corpus older than 8.5.1 has no
        // such value (the pinned contract corpus carries only the three 8.0 values), so
        // this arm is skipped there rather than failing; a corpus that has it keeps its
        // own date. The control that cannot pass vacuously is
        // `a_type_version_correction_redates_the_type_and_only_the_type` in
        // `crates/bsl-platform/src/overlays_tests.rs`: it runs the correction against a
        // corpus that dates the type 8.5.1 and asserts a value dated 8.5.1 keeps its date.
        if let Some(version) = type_member("РежимРедактированияКолонки", "Авто", true)
        {
            assert_eq!(version, v("8.5.1"));
        }
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
