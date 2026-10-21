//! Pure recognition of configuration-extension method *weaving* annotations
//! (`&Вместо` / `&Перед` / `&После`, i.e. Around / Before / After) — Phase 3,
//! increment 1.
//!
//! Unlike `&ИзменениеИКонтроль` (text-splice merge, see [`crate::extension_merge`]),
//! weaving does not rewrite the base module text: an interceptor is a separate
//! method that wraps or replaces a base method `M`. This module only recognizes the
//! annotation and extracts the base method name it targets; the cross-module name
//! resolution and `ПродолжитьВызов` return typing that act on that target live in
//! later increments (`hir-ty`). Like the rest of the pure engine it is free of any
//! database / Salsa dependency so the recognition can be unit-tested in isolation.

use syntax::{SyntaxKind, SyntaxNode};

use crate::extension_merge::annotation_first_string_arg;
use crate::symbol_tree::MethodSymbol;

/// Interned identity of a *weaving* extension module: the (ext, base) file pair whose
/// own bodies are inferred with the base module as a same-module sibling fallback.
/// Unlike [`crate::effective_module::EffectiveModuleId`] there is no text splice — the
/// extension module keeps its own native text; only cross-module name resolution gains
/// the base fallback.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct WeavingModuleId<'db> {
    #[returns(copy)]
    pub ext_file: vfs::FileId,
    #[returns(copy)]
    pub base_file: vfs::FileId,
}

/// Which weaving annotation an interceptor method carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterceptionKind {
    /// `&Вместо("M")` — replaces base `M`; its body may call the original via
    /// `ПродолжитьВызов(...)`.
    Around,
    /// `&Перед("M")` — runs before base `M`.
    Before,
    /// `&После("M")` — runs after base `M`.
    After,
}

/// A recognized weaving interceptor: the kind of interception and the base method
/// name it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interception {
    pub kind: InterceptionKind,
    /// Name of the base method this interceptor wraps or replaces.
    pub target: String,
}

/// Recognize a `&Вместо` / `&Перед` / `&После` interceptor method and extract the
/// base method name it targets. Returns `None` when the method carries none of the
/// three weaving annotations, or the annotation's first string argument is absent
/// (degrade to no-weave — never invent a target).
///
/// A method carrying more than one of these annotations is malformed BSL; the first
/// recognized kind (Around, then Before, then After) wins, which is sufficient for
/// the well-formed single-annotation shape this targets.
pub fn interceptor_target(method: &SyntaxNode) -> Option<Interception> {
    const KINDS: [(SyntaxKind, InterceptionKind); 3] = [
        (SyntaxKind::ANN_AROUND, InterceptionKind::Around),
        (SyntaxKind::ANN_BEFORE, InterceptionKind::Before),
        (SyntaxKind::ANN_AFTER, InterceptionKind::After),
    ];

    KINDS.into_iter().find_map(|(ann_kind, kind)| {
        annotation_first_string_arg(method, ann_kind).map(|target| Interception { kind, target })
    })
}

/// A way an interceptor method's signature diverges from the base method it weaves onto.
///
/// 1C requires every weaving method to declare the same parameter list as the extended
/// method "up to the `Знач` keyword" and, for a `&Вместо` replacement, the same
/// procedure/function kind (an extended *function* may only be replaced, and the
/// replacement must itself be a function). Parameter values are shared across the whole
/// chain at runtime, so an arity or by-value divergence is a genuine applicability defect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureMismatch {
    /// The interceptor declares a different number of parameters than the base method.
    ParamCount { base: usize, interceptor: usize },
    /// Positional parameter `index` (0-based) differs in its by-value (`Знач`) flag.
    /// `param` is the interceptor's name for that position; `base_is_val` is the base
    /// method's declaration.
    ByVal { index: usize, param: String, base_is_val: bool },
    /// A `&Вместо` interceptor's procedure/function kind differs from the base method.
    /// `base_is_function` is the base method's kind (the kind the interceptor must match).
    MethodKind { base_is_function: bool },
}

/// Compare a weaving interceptor's signature against the base method it targets, returning
/// the first divergence (if any) in the order: method kind (`&Вместо` only) → parameter
/// count → first by-value flag difference. Returns `None` when the signatures are
/// equivalent for the purposes of extension applicability.
///
/// The procedure/function check applies only to [`InterceptionKind::Around`]: `&Перед` /
/// `&После` on an extended *function* are rejected wholesale by
/// [`interception_applicable`] (a separate applicability rule), so here only the parameter
/// shape is validated for those kinds.
pub fn signature_mismatch(
    kind: InterceptionKind,
    interceptor: &MethodSymbol,
    base: &MethodSymbol,
) -> Option<SignatureMismatch> {
    if kind == InterceptionKind::Around && interceptor.is_function != base.is_function {
        return Some(SignatureMismatch::MethodKind { base_is_function: base.is_function });
    }

    if interceptor.params.len() != base.params.len() {
        return Some(SignatureMismatch::ParamCount {
            base: base.params.len(),
            interceptor: interceptor.params.len(),
        });
    }

    for (index, (ip, bp)) in interceptor.params.iter().zip(base.params.iter()).enumerate() {
        if ip.is_val != bp.is_val {
            return Some(SignatureMismatch::ByVal {
                index,
                param: ip.name.as_str().to_owned(),
                base_is_val: bp.is_val,
            });
        }
    }

    None
}

/// Whether an interception actually RUNS: the one predicate, so nobody spells it twice.
///
/// Three conditions, and each of them alone is enough to make the interceptor dead code: the
/// target must be exported (an extension cannot weave onto a private method), the annotation
/// must be applicable to it, and the signatures must agree. Anything reporting interceptors
/// to a user — effective exports, a symbol card — answers this question, and answering it
/// with a subset publishes a handler that never executes as though it did.
pub fn interception_effective(
    kind: InterceptionKind,
    interceptor: &MethodSymbol,
    base: &MethodSymbol,
) -> bool {
    base.is_export
        && interception_applicable(kind, base)
        && signature_mismatch(kind, interceptor, base).is_none()
}

/// Whether a weaving interception of `kind` may target `base` at all. 1C allows an extended
/// *function* to be extended only with `&Вместо` (`&Перед` / `&После` are unavailable for
/// functions); every interception of a *procedure* is allowed. Returns `false` only for the
/// inapplicable `&Перед` / `&После`-on-a-function combination.
///
/// This is a precondition for [`signature_mismatch`]: when the annotation itself cannot apply
/// to the base method, comparing parameter shapes is moot.
pub fn interception_applicable(kind: InterceptionKind, base: &MethodSymbol) -> bool {
    !(base.is_function && matches!(kind, InterceptionKind::Before | InterceptionKind::After))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method_node(code: &str) -> SyntaxNode {
        let parse = parser::parse(code);
        parse
            .syntax_node()
            .children()
            .find(|n| matches!(n.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
            .unwrap_or_else(|| panic!("no method node in: {code:?}"))
    }

    #[test]
    fn recognizes_around() {
        let m = method_node("&Вместо(\"М\")\nПроцедура Расш1_М()\nКонецПроцедуры");
        assert_eq!(
            interceptor_target(&m),
            Some(Interception { kind: InterceptionKind::Around, target: "М".into() })
        );
    }

    #[test]
    fn recognizes_before() {
        let m =
            method_node("&Перед(\"ПриЗаписи\")\nПроцедура Расш1_ПриЗаписи(Отказ)\nКонецПроцедуры");
        assert_eq!(
            interceptor_target(&m),
            Some(Interception {
                kind: InterceptionKind::Before, target: "ПриЗаписи".into()
            })
        );
    }

    #[test]
    fn recognizes_after() {
        let m = method_node("&После(\"Функция1\")\nФункция Расш1_Функция1()\nКонецФункции");
        assert_eq!(
            interceptor_target(&m),
            Some(Interception { kind: InterceptionKind::After, target: "Функция1".into() })
        );
    }

    #[test]
    fn recognizes_english_instead() {
        // 1C's English form of `&Вместо` is `&Instead`.
        let m = method_node("&Instead(\"M\")\nProcedure Ext1_M()\nEndProcedure");
        assert_eq!(
            interceptor_target(&m),
            Some(Interception { kind: InterceptionKind::Around, target: "M".into() })
        );
    }

    #[test]
    fn plain_method_is_not_an_interceptor() {
        let m = method_node("Процедура Обычная()\nКонецПроцедуры");
        assert_eq!(interceptor_target(&m), None);
    }

    #[test]
    fn change_and_validate_is_not_a_weaving_interceptor() {
        // `&ИзменениеИКонтроль` is handled by the text-splice merge, not weaving.
        let m = method_node("&ИзменениеИКонтроль(\"М\")\nПроцедура Расш1_М()\nКонецПроцедуры");
        assert_eq!(interceptor_target(&m), None);
    }

    #[test]
    fn nested_annotation_arg_is_rejected() {
        // The target string lives inside a nested annotation, not as the direct first
        // string arg → no direct STRING token → degrade to no-weave.
        let m = method_node("&Вместо(&Ann(\"М\"))\nПроцедура Расш1_М()\nКонецПроцедуры");
        assert_eq!(interceptor_target(&m), None);
    }

    #[test]
    fn missing_target_string_is_rejected() {
        let m = method_node("&Вместо()\nПроцедура Расш1_М()\nКонецПроцедуры");
        assert_eq!(interceptor_target(&m), None);
    }

    fn method(is_function: bool, params: &[(&str, bool)]) -> MethodSymbol {
        use crate::name::Name;
        use crate::symbol_tree::ParamSymbol;
        use crate::{MethodId, ModuleId};
        MethodSymbol {
            decl: crate::module_interface::MethodDecl {
                id: MethodId {
                    module: ModuleId::new(vfs::FileId(0)),
                    local_id: crate::MethodKey::first("М"),
                },
                name: Name::new("m"),
                is_function,
                is_export: false,
                params: params
                    .iter()
                    .map(|(n, is_val)| ParamSymbol {
                        name: Name::new(n),
                        is_val: *is_val,
                        has_default: false,
                        type_ref: None,
                    })
                    .collect(),
                directives: Box::new([]),
                preproc_env: crate::execution_env::EnvFlags::ALL,
                preproc_undecided: crate::execution_env::EnvFlags::EMPTY,
                docs: None,
                return_type_ref: None,
            },
            annotations: Vec::new(),
            source_range: syntax::TextRange::empty(0.into()),
            name_range: syntax::TextRange::empty(0.into()),
        }
    }

    #[test]
    fn identical_signature_has_no_mismatch() {
        let base = method(false, &[("А", false), ("Б", true)]);
        let ext = method(false, &[("Парам1", false), ("Парам2", true)]);
        assert_eq!(signature_mismatch(InterceptionKind::Around, &ext, &base), None);
    }

    #[test]
    fn param_count_divergence_is_reported() {
        let base = method(false, &[("А", false)]);
        let ext = method(false, &[("А", false), ("Б", false)]);
        assert_eq!(
            signature_mismatch(InterceptionKind::Before, &ext, &base),
            Some(SignatureMismatch::ParamCount { base: 1, interceptor: 2 })
        );
    }

    #[test]
    fn by_val_divergence_is_reported_with_position() {
        let base = method(false, &[("А", false), ("Б", false)]);
        let ext = method(false, &[("А", false), ("Б", true)]);
        assert_eq!(
            signature_mismatch(InterceptionKind::After, &ext, &base),
            Some(SignatureMismatch::ByVal { index: 1, param: "Б".into(), base_is_val: false })
        );
    }

    #[test]
    fn around_kind_divergence_is_reported() {
        // base is a function, interceptor is a procedure → must match for &Вместо.
        let base = method(true, &[]);
        let ext = method(false, &[]);
        assert_eq!(
            signature_mismatch(InterceptionKind::Around, &ext, &base),
            Some(SignatureMismatch::MethodKind { base_is_function: true })
        );
    }

    #[test]
    fn proc_func_kind_is_ignored_for_before_after() {
        // `&Перед`/`&После` do not carry the kind constraint here; only param shape.
        let base = method(true, &[("А", false)]);
        let ext = method(false, &[("А", false)]);
        assert_eq!(signature_mismatch(InterceptionKind::Before, &ext, &base), None);
        assert_eq!(signature_mismatch(InterceptionKind::After, &ext, &base), None);
    }

    #[test]
    fn kind_divergence_takes_priority_over_param_count() {
        let base = method(true, &[("А", false)]);
        let ext = method(false, &[]);
        assert_eq!(
            signature_mismatch(InterceptionKind::Around, &ext, &base),
            Some(SignatureMismatch::MethodKind { base_is_function: true })
        );
    }

    #[test]
    fn before_after_on_a_function_is_not_applicable() {
        let base = method(true, &[]);
        assert!(!interception_applicable(InterceptionKind::Before, &base));
        assert!(!interception_applicable(InterceptionKind::After, &base));
    }

    #[test]
    fn around_on_a_function_is_applicable() {
        let base = method(true, &[]);
        assert!(interception_applicable(InterceptionKind::Around, &base));
    }

    #[test]
    fn any_interception_of_a_procedure_is_applicable() {
        let base = method(false, &[]);
        assert!(interception_applicable(InterceptionKind::Around, &base));
        assert!(interception_applicable(InterceptionKind::Before, &base));
        assert!(interception_applicable(InterceptionKind::After, &base));
    }
}
