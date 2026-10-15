use crate::BodyContext;
use bsl_platform::deprecation::{
    self, CompatibilityBucket, DeprecationEntry, DisplayKind, ElementKind, LifecycleGroup, Lookup,
};
use hir::execution_env::EnvFlags;
use hir::DeprecatedKind8312;
use stdx::case::CaseExt;

pub(crate) fn global_function_fact(
    name: &str,
    group: LifecycleGroup,
) -> Option<&'static DeprecationEntry> {
    let entry = deprecation::registry().lookup(Lookup::global_method(name))?;
    if entry.element_kind == ElementKind::GlobalMethod
        && entry.display == DisplayKind::Function
        && entry.group == group
    {
        Some(entry)
    } else {
        None
    }
}

pub(crate) fn managed_form_type_fact(name: &str) -> Option<&'static DeprecationEntry> {
    let entry = deprecation::registry().lookup(Lookup::type_(name))?;
    if entry.element_kind == ElementKind::Type
        && entry.display == DisplayKind::Type
        && entry.group == LifecycleGroup::ManagedForm
        && entry.compatibility == CompatibilityBucket::CompatibilityMode8_3_14
    {
        Some(entry)
    } else {
        None
    }
}

pub(crate) fn deprecated_method_fact(name: &str) -> Option<&'static DeprecationEntry> {
    let entry = deprecation::registry().lookup(Lookup::global_method(name))?;
    if entry.element_kind != ElementKind::GlobalMethod || entry.display != DisplayKind::Method {
        return None;
    }

    match entry.compatibility {
        CompatibilityBucket::CompatibilityMode8_3_10
        | CompatibilityBucket::CompatibilityMode8_3_17 => Some(entry),
        CompatibilityBucket::Any
        | CompatibilityBucket::CompatibilityMode8_3_6
        | CompatibilityBucket::CompatibilityMode8_3_12
        | CompatibilityBucket::CompatibilityMode8_3_14 => None,
    }
}

pub(crate) fn canonical_name_for(
    entry: &DeprecationEntry,
    input_name: &str,
) -> Option<&'static str> {
    let lower = input_name.fold_lower();
    if lower == entry.ru.fold_lower() {
        return Some(entry.ru);
    }
    if !entry.en.is_empty() && lower == entry.en.fold_lower() {
        return Some(entry.en);
    }
    None
}

pub(crate) fn replacement_for_name(
    entry: &DeprecationEntry,
    input_name: &str,
) -> Option<&'static str> {
    let replacement = entry.replacement?;
    let lower = input_name.fold_lower();
    if lower == entry.ru.fold_lower() {
        return Some(replacement.ru);
    }
    if !entry.en.is_empty() && lower == entry.en.fold_lower() {
        return Some(replacement.en);
    }
    None
}

pub(crate) fn is_russian_alias(entry: &DeprecationEntry, input_name: &str) -> bool {
    input_name.fold_lower() == entry.ru.fold_lower()
}

/// Whether a call to the global function `replacement` compiles here: the
/// compatibility mode does not hide it, the project's minimum platform release
/// does not predate it, and it is not missing from an environment this call is
/// compiled for. A quick fix that swaps in a name the code cannot compile with
/// would trade a warning for an error. An unknown mode, release or environment
/// does not withhold the fix — absence of data is not evidence the replacement fails.
pub(crate) fn replacement_compiles_here(replacement: &str, ctx: &BodyContext) -> bool {
    if let (Some(mode), Some(visible_from)) =
        (ctx.compatibility_mode(), hir::compat_mode::hidden_global(replacement))
    {
        if visible_from.release_newer_than(mode) {
            return false;
        }
    }
    if let (Some(min), Some(since)) =
        (ctx.min_platform_version(), hir::min_platform::global_function(replacement))
    {
        if since.release_newer_than(min) {
            return false;
        }
    }
    !replacement_missing_from_call_env(replacement, ctx)
}

/// A method of this module named `name` owns a bare call of that spelling, ahead
/// of any platform global. True for the call's current name (the call is not the
/// global) and for the replacement (the renamed call would land on the method).
pub(crate) fn module_owns_bare_call(name: &str, ctx: &BodyContext) -> bool {
    ctx.interface_method_named(&hir::Name::new(name)).is_some()
}

fn replacement_missing_from_call_env(replacement: &str, ctx: &BodyContext) -> bool {
    let Some(member_env) = global_function_env(replacement) else {
        return false;
    };
    if member_env.is_empty() {
        return false;
    }
    let call_env = call_compile_env(ctx);
    if call_env.is_empty() {
        return false;
    }
    !(call_env.without(member_env) & ctx.env_options().checked_environments).is_empty()
}

fn global_function_env(name: &str) -> Option<EnvFlags> {
    hir::builtin_functions().get(name).and_then(|sigs| sigs.first().map(|sig| sig.env()))
}

/// Environments the call is compiled for, from the position-free declaration:
/// [`hir::execution_env::body_env`] (the method directive, or the module base when
/// a weaving interceptor has none of its own) intersected with the module-level
/// `#Если` regions around the method. Empty means unknown — a common module
/// without metadata has no flags. An intra-method `#Если` is not folded in: a
/// body check does not read file positions, and the declaration already says
/// which environments the method as a whole is compiled for.
fn call_compile_env(ctx: &BodyContext) -> EnvFlags {
    let opts = ctx.env_options();
    let metadata = ctx.module_metadata();
    if ctx.method_id().is_none() {
        return hir::execution_env::module_code_env(&metadata, &opts);
    }
    let Some(decl) = ctx.decl() else {
        return EnvFlags::EMPTY;
    };
    let env = hir::execution_env::body_env(&metadata, &decl.directives, &opts);
    if env.is_empty() {
        return EnvFlags::EMPTY;
    }
    // An undecidable module-level `#Если` poisons `preproc_env` to empty. That is
    // "the region could not be decided", not "the method compiles nowhere":
    // intersecting would erase the directive's known environments and the gate
    // would offer a replacement that does not compile in them.
    if decl.preproc_env.is_empty() {
        return env;
    }
    env.intersection(decl.preproc_env)
}

pub(crate) fn deprecated_8312_replacement(name: &str, kind: DeprecatedKind8312) -> &'static str {
    let lower = name.fold_lower();
    deprecation::registry()
        .entries()
        .iter()
        .find(|entry| is_8312_kind(entry, kind) && matches_entry_name(entry, &lower))
        .and_then(|entry| replacement_for_name(entry, name))
        .unwrap_or("")
}

fn is_8312_kind(entry: &DeprecationEntry, kind: DeprecatedKind8312) -> bool {
    if entry.compatibility != CompatibilityBucket::CompatibilityMode8_3_12 {
        return false;
    }

    match kind {
        DeprecatedKind8312::Attribute => {
            entry.element_kind == ElementKind::Attribute && entry.display == DisplayKind::Attribute
        }
        DeprecatedKind8312::Method => {
            entry.element_kind == ElementKind::Method && entry.display == DisplayKind::Method
        }
        DeprecatedKind8312::GlobalMethod => {
            entry.element_kind == ElementKind::GlobalMethod
                && entry.display == DisplayKind::GlobalMethod
        }
        DeprecatedKind8312::EnumName => {
            entry.element_kind == ElementKind::EnumName && entry.display == DisplayKind::EnumName
        }
        DeprecatedKind8312::EnumValue => {
            entry.element_kind == ElementKind::EnumValue && entry.display == DisplayKind::EnumValue
        }
    }
}

fn matches_entry_name(entry: &DeprecationEntry, lower_name: &str) -> bool {
    lower_name == entry.ru.fold_lower()
        || (!entry.en.is_empty() && lower_name == entry.en.fold_lower())
}
