use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use hir_def::execution_env::EnvFlags;
use hir_def::ty::FunctionSignature;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::sync::OnceLock;

use crate::lower::type_string::{lower_param_type_string_typeid, lower_return_type_string_typeid};
use crate::BuiltinCallableId;

pub(crate) mod candidates;
pub use candidates::builtin_functions;

static BUILTIN_FUNCTIONS: OnceLock<BuiltinFunctions> = OnceLock::new();

#[derive(Debug)]
pub struct BuiltinFunctions {
    signatures: FxHashMap<String, Vec<BuiltinSignature>>,
    callable_ids: FxHashMap<String, BuiltinCallableId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinSignature {
    names: Box<[SmolStr]>,
    params: Box<[ParamTypeSpec]>,
    defaults: Box<[bool]>,
    ret: ReturnTypeSpec,
    max_args: Option<u32>,
    variadic_param: Option<usize>,
    env: EnvFlags,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParamTypeSpec {
    Raw(String),
    Unknown,
    TypeDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReturnTypeSpec {
    Raw(String),
    Undefined,
    Unknown,
}

impl BuiltinSignature {
    pub fn lower(&self, db: &dyn TypeKernelDb) -> FunctionSignature {
        let params = self
            .params
            .iter()
            .map(|p| match p {
                ParamTypeSpec::Raw(s) => lower_param_type_string_typeid(db, s),
                ParamTypeSpec::Unknown => db.unknown(),
                ParamTypeSpec::TypeDescriptor => db.type_descriptor(),
            })
            .collect();
        let ret = match &self.ret {
            ReturnTypeSpec::Raw(s) => lower_return_type_string_typeid(db, s),
            ReturnTypeSpec::Undefined => db.undefined(),
            ReturnTypeSpec::Unknown => db.unknown(),
        };
        FunctionSignature {
            params,
            defaults: self.defaults.clone(),
            ret,
            max_args: self.max_args,
            from_doc_comment: false,
            doc_see: Default::default(),
        }
    }

    pub fn defaults(&self) -> &[bool] {
        &self.defaults
    }

    pub fn max_args(&self) -> Option<u32> {
        self.max_args
    }

    /// Execution environments this function is available in.
    pub fn env(&self) -> EnvFlags {
        self.env
    }

    pub fn param_count(&self) -> usize {
        self.params.len()
    }

    pub fn required_count(&self) -> usize {
        self.defaults.iter().rposition(|has_default| !*has_default).map_or(0, |i| i + 1)
    }
}

/// Whether the platform global `name` returns a value of its first argument's type.
///
/// `Макс` / `Мин` serve the whole comparable family with one HBK signature, so the
/// declared return is `Число, Строка, Дата, Булево`. The platform picks the comparison
/// variant by the first argument and converts (or, on 8.3.27, refuses) the rest, so the
/// result always has the first argument's type. Trusting the declared union instead makes
/// every `Макс(1, Х)` look like a possible `Строка` and trips the next number-only
/// consumer (`Формат(..., "ЧГ=0")`) on valid code.
pub(crate) fn returns_first_argument_type(name: &str) -> bool {
    use stdx::case::CaseExt;
    matches!(name.fold_lower().as_str(), "макс" | "мин" | "max" | "min")
}

fn descriptors_from_global_function(func: &bsl_platform::GlobalFunction) -> Vec<BuiltinSignature> {
    let ret = match &func.return_type {
        None => ReturnTypeSpec::Undefined,
        Some(s) => ReturnTypeSpec::Raw(s.to_string()),
    };

    let mut sigs = if func.variants.is_empty() {
        vec![descriptor_from_params(&func.parameters, ret)]
    } else {
        func.variants.iter().map(|v| descriptor_from_params(&v.parameters, ret.clone())).collect()
    };
    if is_form_data_conversion_helper(&func.name, &func.english_name) {
        complete_form_data_family_params(&mut sigs);
    }
    let env = EnvFlags::from_platform_context(func.context.as_ref());
    for sig in sigs.iter_mut() {
        sig.env = env;
    }
    sigs
}

/// The form-data conversion helpers accept the whole form-data container family
/// interchangeably, yet the HBK dump documents the family inconsistently: it
/// lists all four members on `ДанныеФормыВЗначение` but omits `ДанныеФормыДерево`
/// on `КопироватьДанныеФормы` / `ЗначениеВДанныеФормы`. That omission is a dump
/// artefact, not a runtime restriction — a form-data tree is a valid argument to
/// all of them. Completing the family on exactly these platform helpers keeps
/// the fix scoped: a user procedure documenting a parameter with a single
/// form-data facet stays facet-strict.
fn is_form_data_conversion_helper(name: &str, english_name: &str) -> bool {
    const RU: [&str; 3] = ["КопироватьДанныеФормы", "ЗначениеВДанныеФормы", "ДанныеФормыВЗначение"];
    const EN: [&str; 3] = ["CopyFormData", "ValueToFormData", "FormDataToValue"];
    RU.contains(&name) || EN.contains(&english_name)
}

const FORM_DATA_CONTAINER_FAMILY: [&str; 4] = [
    "ДанныеФормыСтруктураСКоллекцией",
    "ДанныеФормыКоллекция",
    "ДанныеФормыСтруктура",
    "ДанныеФормыДерево",
];

fn complete_form_data_family_params(sigs: &mut [BuiltinSignature]) {
    for sig in sigs.iter_mut() {
        for param in sig.params.iter_mut() {
            if let ParamTypeSpec::Raw(s) = param {
                if let Some(full) = complete_form_data_container_family(s) {
                    *s = full;
                }
            }
        }
    }
}

/// When a documented type is a (proper) subset of the form-data container family,
/// return the full family as a union string; otherwise `None` (leave unrelated
/// or already-complete params untouched). Tokens are matched whole — the dump
/// uses canonical Cyrillic names — so `ДанныеФормыСтруктура` is never confused
/// with `ДанныеФормыСтруктураСКоллекцией`.
fn complete_form_data_container_family(param_type: &str) -> Option<String> {
    let tokens: Vec<&str> =
        param_type.split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return None;
    }
    let all_family = tokens.iter().all(|t| FORM_DATA_CONTAINER_FAMILY.contains(t));
    let already_complete = tokens.contains(&"ДанныеФормыДерево");
    if all_family && !already_complete {
        Some(FORM_DATA_CONTAINER_FAMILY.join(", "))
    } else {
        None
    }
}

pub(crate) fn descriptor_from_params(
    params_in: &[bsl_platform::MethodParam],
    ret: ReturnTypeSpec,
) -> BuiltinSignature {
    let mut params = Vec::with_capacity(params_in.len());
    let mut names = Vec::with_capacity(params_in.len());
    let mut defaults = Vec::with_capacity(params_in.len());
    for param in params_in {
        names.push(param.name.clone());
        params.push(
            param
                .param_type
                .as_deref()
                .map(|s| ParamTypeSpec::Raw(s.to_string()))
                .unwrap_or(ParamTypeSpec::Unknown),
        );
        defaults.push(param.is_optional);
    }

    let last = params_in.last();
    let variadic_param = last
        .filter(|param| {
            param.is_variadic
                || name_implies_unbounded_variadic(param.name.as_str())
                || variadic_param_max(param.name.as_str()).is_some()
        })
        .map(|_| params_in.len() - 1);
    let max_args = if last.is_some_and(|p| p.is_variadic)
        || last.is_some_and(|p| name_implies_unbounded_variadic(p.name.as_str()))
    {
        None
    } else if let Some(m) = last.and_then(|p| variadic_param_max(p.name.as_str())) {
        Some((params.len() as u32).saturating_sub(1).saturating_add(m))
    } else {
        Some(params.len() as u32)
    };

    BuiltinSignature {
        names: names.into_boxed_slice(),
        params: params.into_boxed_slice(),
        defaults: defaults.into_boxed_slice(),
        ret,
        max_args,
        variadic_param,
        env: EnvFlags::ALL,
    }
}

/// Uses the platform's shared interpretation of alphabetic range endpoints.
fn name_implies_unbounded_variadic(name: &str) -> bool {
    bsl_platform::ParameterSeries::parse(name).is_some_and(|series| series.last.is_none())
}

/// Counts the arguments a bounded parameter-name series admits: `Value3-Value5` is three,
/// not five, so the arity matches the names inlay hints give each argument.
fn variadic_param_max(name: &str) -> Option<u32> {
    let series = bsl_platform::ParameterSeries::parse(name)?;
    Some(series.last? - series.first + 1)
}

fn register_fallbacks(sigs: &mut FxHashMap<String, Vec<BuiltinSignature>>) {
    insert_pair(
        sigs,
        ("новый", "new"),
        BuiltinSignature {
            names: Box::new(["Тип".into()]),
            params: Box::new([ParamTypeSpec::TypeDescriptor]),
            defaults: Box::new([false]),
            ret: ReturnTypeSpec::Unknown,
            max_args: Some(1),
            variadic_param: None,
            env: EnvFlags::ALL,
        },
    );

    insert_pair(
        sigs,
        ("описаниетипов", "typedescription"),
        BuiltinSignature {
            names: Box::new(["Тип".into()]),
            params: Box::new([ParamTypeSpec::Unknown]),
            defaults: Box::new([false]),
            ret: ReturnTypeSpec::Unknown,
            max_args: None,
            variadic_param: Some(0),
            env: EnvFlags::ALL,
        },
    );
}

fn insert_pair(
    sigs: &mut FxHashMap<String, Vec<BuiltinSignature>>,
    (ru, en): (&str, &str),
    sig: BuiltinSignature,
) {
    sigs.entry(ru.to_string()).or_insert_with(|| vec![sig.clone()]);
    sigs.entry(en.to_string()).or_insert_with(|| vec![sig]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::facet::DateComponent;
    use bsl_types::kind::TypeKind;
    use bsl_types::testing::InMemoryDb;

    #[test]
    fn legacy_english_aliases_reach_builtin_signatures() {
        let builtins = builtin_functions();
        for (ru, legacy_en) in bsl_platform::LEGACY_GLOBAL_FUNCTION_EN_ALIASES {
            let Some(canonical) = builtins.get(ru) else {
                // Corpus-free build (no platform data) — nothing to alias.
                continue;
            };
            let via_alias = builtins
                .get(legacy_en)
                .unwrap_or_else(|| panic!("legacy alias {legacy_en} must have a signature"));
            assert_eq!(via_alias.len(), canonical.len());
        }
    }

    #[test]
    fn complete_form_data_family_fills_missing_tree() {
        let full = "ДанныеФормыСтруктураСКоллекцией, ДанныеФормыКоллекция, ДанныеФормыСтруктура, ДанныеФормыДерево";
        assert_eq!(
            complete_form_data_container_family(
                "ДанныеФормыСтруктураСКоллекцией, ДанныеФормыКоллекция, ДанныеФормыСтруктура"
            )
            .as_deref(),
            Some(full),
            "a family subset is completed to all four containers"
        );
        assert_eq!(
            complete_form_data_container_family("ДанныеФормыКоллекция").as_deref(),
            Some(full),
            "a single family member completes too"
        );
    }

    #[test]
    fn complete_form_data_family_leaves_complete_and_unrelated_untouched() {
        assert_eq!(
            complete_form_data_container_family(
                "ДанныеФормыСтруктура, ДанныеФормыКоллекция, ДанныеФормыСтруктураСКоллекцией, ДанныеФормыДерево"
            ),
            None,
            "already-complete family is left as-is"
        );
        assert_eq!(
            complete_form_data_container_family("ТаблицаЗначений, ДанныеФормыКоллекция"),
            None,
            "a union mixing a non-family member is not a pure form-data slot"
        );
        assert_eq!(complete_form_data_container_family("Строка"), None);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn copy_form_data_signature_admits_form_data_tree_argument() {
        let db = InMemoryDb::new();
        let sigs = builtin_functions()
            .get("копироватьданныеформы")
            .expect("КопироватьДанныеФормы builtin");
        let sig = sigs.first().expect("at least one signature").lower(&db);
        let owner = bsl_types::facet::MdoRefFacet::new(
            bsl_metadata::MdoType::Report,
            "СтатистикаФорма1".to_string(),
        );
        let tree = db.mk_form_data(bsl_types::facet::FormDataFacet::Tree, Some(owner));
        for (idx, &param) in sig.params.iter().enumerate() {
            assert!(
                crate::subtype::is_coercible_to(&db, tree, param),
                "КопироватьДанныеФормы param #{idx} must admit a form-data tree after family completion"
            );
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn lookup_is_case_insensitive_and_bilingual() {
        let builtins = builtin_functions();
        assert!(builtins.get("СтрДлина").is_some());
        assert!(builtins.get("стрдлина").is_some());
        assert!(builtins.get("СТРДЛИНА").is_some());
        assert!(builtins.get("StrLen").is_some());
        assert!(builtins.get("strlen").is_some());
    }

    fn single_signature<'a>(builtins: &'a BuiltinFunctions, name: &str) -> &'a BuiltinSignature {
        let sigs = builtins.get(name).unwrap_or_else(|| panic!("{name} should exist"));
        assert_eq!(sigs.len(), 1, "{name} should be single-overload, got {} overloads", sigs.len());
        &sigs[0]
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn nstr_has_optional_second_parameter() {
        let db = InMemoryDb::new();
        let builtins = builtin_functions();
        let nstr = single_signature(builtins, "нстр");
        assert_eq!(nstr.param_count(), 2, "НСтр has 2 declared params");
        assert_eq!(nstr.required_count(), 1, "second param is optional, so required=1");
        assert_eq!(nstr.max_args(), Some(2), "fixed arity caps at params.len()");
        assert_eq!(nstr.lower(&db).ret, db.string(None, false));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn strtemplate_is_capped_variadic() {
        let db = InMemoryDb::new();
        let builtins = builtin_functions();
        let sig = single_signature(builtins, "стршаблон");
        assert_eq!(
            sig.max_args(),
            Some(11),
            "Значение1-Значение10 → cap at (params.len()-1) + 10 = 11"
        );
        assert_eq!(sig.lower(&db).ret, db.string(None, false));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn strlen_returns_number() {
        let db = InMemoryDb::new();
        let builtins = builtin_functions();
        let sig = single_signature(builtins, "стрдлина").lower(&db);
        assert_eq!(sig.ret, db.number(None, None));
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0], db.string(None, false));
        assert_eq!(sig.required_count(), 1);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn currentdate_takes_no_args() {
        let db = InMemoryDb::new();
        let builtins = builtin_functions();
        let sig = single_signature(builtins, "текущаядата").lower(&db);
        assert_eq!(sig.ret, db.date(DateComponent::DateTime));
        assert!(sig.params.is_empty());
        assert_eq!(sig.required_count(), 0);
    }

    #[test]
    fn fallback_typedescription_is_unbounded_variadic() {
        let builtins = builtin_functions();
        let sig = single_signature(builtins, "описаниетипов");
        assert_eq!(sig.max_args(), None, "fallback marks truly unbounded variadic");
        assert_eq!(sig.required_count(), 1, "only the type-list is required");
    }

    #[test]
    fn variadic_param_max_detection() {
        assert_eq!(variadic_param_max("Значение1-Значение10"), Some(10));
        assert_eq!(variadic_param_max("Value1-Value5"), Some(5));
        assert_eq!(variadic_param_max("Значение1,...,Значение10"), Some(10));
        assert_eq!(variadic_param_max("Имя"), None);
        assert_eq!(variadic_param_max("Имя-Фамилия"), None);
        assert_eq!(variadic_param_max("X-Y"), None);
        assert_eq!(variadic_param_max("Значение-Значение10"), None);
        assert_eq!(variadic_param_max("Содержимое1,...,СодержимоеN"), None);
        assert_eq!(variadic_param_max("Value3-Value5"), Some(3));
    }

    #[test]
    fn bounded_series_starting_above_one_caps_by_its_length() {
        let params = vec![bsl_platform::MethodParam {
            name: "Value3-Value5".into(),
            param_type: Some("Произвольный".into()),
            is_optional: false,
            is_variadic: false,
        }];
        let sig = descriptor_from_params(&params, ReturnTypeSpec::Unknown);
        assert_eq!(sig.max_args(), Some(3), "Value3, Value4 and Value5 are three arguments");
    }

    #[test]
    fn name_implies_unbounded_variadic_detection() {
        assert!(name_implies_unbounded_variadic("Содержимое1,...,СодержимоеN"));
        assert!(name_implies_unbounded_variadic("Value1,...,ValueK"));
        assert!(!name_implies_unbounded_variadic("Значение1-Значение10"));
        assert!(!name_implies_unbounded_variadic("Значение1,...,Значение10"));
        assert!(!name_implies_unbounded_variadic("Имя"));
        assert!(!name_implies_unbounded_variadic("Имя,...,Фамилия"));
        assert!(!name_implies_unbounded_variadic("X1,...,Y2"));
        assert!(!name_implies_unbounded_variadic("X1,...,X-"));
        assert!(!name_implies_unbounded_variadic("X1,...,X-end-"));
    }

    #[test]
    fn name_implies_unbounded_lifts_signature_max_args() {
        let params = vec![bsl_platform::MethodParam {
            name: "Содержимое1,...,СодержимоеN".into(),
            param_type: Some("Произвольный".into()),
            is_optional: true,
            is_variadic: false,
        }];
        let sig = descriptor_from_params(&params, ReturnTypeSpec::Unknown);
        assert_eq!(
            sig.max_args(),
            None,
            "letter-suffix `,...,` name idiom must lift max_args to None"
        );
    }

    #[test]
    fn explicit_is_variadic_flag_yields_unbounded_max() {
        let params = vec![bsl_platform::MethodParam {
            name: "Значение1".into(),
            param_type: Some("Произвольный".into()),
            is_optional: false,
            is_variadic: true,
        }];
        let sig = descriptor_from_params(&params, ReturnTypeSpec::Unknown);
        assert_eq!(sig.max_args(), None, "is_variadic=true must lift the cap");
        assert_eq!(sig.required_count(), 1, "non-optional param stays required");
    }

    #[test]
    fn is_variadic_flag_overrides_name_idiom() {
        let params = vec![bsl_platform::MethodParam {
            name: "Значение1-Значение10".into(),
            param_type: Some("Произвольный".into()),
            is_optional: false,
            is_variadic: true,
        }];
        let sig = descriptor_from_params(&params, ReturnTypeSpec::Unknown);
        assert_eq!(sig.max_args(), None, "explicit flag must override the cap idiom");
    }

    #[test]
    fn no_flag_no_idiom_preserves_fixed_arity() {
        let params = vec![
            bsl_platform::MethodParam {
                name: "Шаблон".into(),
                param_type: Some("Строка".into()),
                is_optional: false,
                is_variadic: false,
            },
            bsl_platform::MethodParam {
                name: "КодЯзыка".into(),
                param_type: Some("Строка".into()),
                is_optional: true,
                is_variadic: false,
            },
        ];
        let sig = descriptor_from_params(&params, ReturnTypeSpec::Raw("Строка".to_string()));
        assert_eq!(sig.max_args(), Some(2), "fixed-arity cap stays at params.len()");
        assert_eq!(sig.required_count(), 1, "optional second param drops required to 1");
    }

    #[test]
    fn return_type_union_is_parsed() {
        let db = InMemoryDb::new();
        let union = lower_return_type_string_typeid(&db, "Булево, Неопределено");
        match db.lookup_type(union) {
            TypeKind::Union(parts) => {
                assert!(parts.contains(&db.boolean()));
                assert!(parts.contains(&db.undefined()));
            }
            other => panic!("expected TypeKind::Union, got {other:?}"),
        }
    }

    #[test]
    fn none_param_type_maps_to_unknown() {
        let builtins = builtin_functions();
        if let Some(sigs) = builtins.get("открытьформа") {
            assert!(
                sigs.iter().any(|s| s.param_count() > 0),
                "at least one ОткрытьФорму overload must have params"
            );
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn registry_has_many_signatures() {
        let builtins = builtin_functions();
        assert!(builtins.signatures.len() > 500);
    }

    #[test]
    fn fallback_does_not_shadow_json_derived_signature() {
        let mut sigs: FxHashMap<String, Vec<BuiltinSignature>> = FxHashMap::default();
        let json_like = BuiltinSignature {
            names: Box::new(["Первый".into(), "Второй".into()]),
            params: Box::new([
                ParamTypeSpec::Raw("Число".to_string()),
                ParamTypeSpec::Raw("Число".to_string()),
            ]),
            defaults: Box::new([false, false]),
            ret: ReturnTypeSpec::Raw("Число".to_string()),
            max_args: Some(2),
            variadic_param: None,
            env: EnvFlags::ALL,
        };
        sigs.insert("foo".into(), vec![json_like.clone()]);
        sigs.insert("bar".into(), vec![json_like.clone()]);

        let fallback = BuiltinSignature {
            names: Box::new(["Значение".into()]),
            params: Box::new([ParamTypeSpec::Raw("Строка".to_string())]),
            defaults: Box::new([false]),
            ret: ReturnTypeSpec::Raw("Строка".to_string()),
            max_args: Some(1),
            variadic_param: None,
            env: EnvFlags::ALL,
        };
        insert_pair(&mut sigs, ("foo", "bar"), fallback);

        assert_eq!(sigs["foo"], vec![json_like.clone()]);
        assert_eq!(sigs["bar"], vec![json_like]);
    }
}
