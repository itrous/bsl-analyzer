use crate::help::{
    InstallOutcome, PlatformHelp, PlatformHelpOrigin, PlatformHelpRequest, PlatformHelpStatus,
    RestartRequired,
};
use crate::snapshot::PlatformSnapshot;
use crate::types::{
    ConstructorDocs, GlobalFunction, MethodDocs, PlatformConstructor, PlatformMethod,
    PlatformProperty, PlatformType, PropertyDocs,
};
use once_cell::sync::OnceCell;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::sync::Arc;
use stdx::case::CaseExt;

static PLATFORM_DATA_SINGLETON: OnceCell<PlatformDataInner> = OnceCell::new();

pub const GLOBAL_CONTEXT_OWNER: &str = "Global context";

/// Fixes the process snapshot to `help` unless one is already fixed.
pub(crate) fn install(help: PlatformHelp) -> Result<InstallOutcome, RestartRequired> {
    if let Some(active) = PLATFORM_DATA_SINGLETON.get() {
        return compare_active(active, help.request);
    }
    let requested = help.request.clone();
    match PLATFORM_DATA_SINGLETON.set(PlatformDataInner::from_help(help)) {
        Ok(()) => Ok(InstallOutcome::Installed),
        Err(_) => compare_active(
            PLATFORM_DATA_SINGLETON.get().expect("a failed set means a value is present"),
            requested,
        ),
    }
}

fn compare_active(
    active: &PlatformDataInner,
    requested: PlatformHelpRequest,
) -> Result<InstallOutcome, RestartRequired> {
    if active.request == requested {
        Ok(InstallOutcome::AlreadyActive)
    } else {
        Err(RestartRequired { active: active.request.clone(), requested })
    }
}

pub(crate) fn installed_request() -> Option<&'static PlatformHelpRequest> {
    PLATFORM_DATA_SINGLETON.get().map(|data| &data.request)
}

pub struct PlatformDataInner {
    request: PlatformHelpRequest,
    origin: Option<PlatformHelpOrigin>,
    missing_reason: Option<String>,
    method_docs: Vec<MethodDocs>,
    global_function_docs: Vec<MethodDocs>,
    constructor_docs: Vec<ConstructorDocs>,
    property_docs: Vec<PropertyDocs>,
    types: Vec<PlatformType>,
    types_by_name: FxHashMap<SmolStr, usize>,
    /// Case-folded English name per type, parallel to `types`, so resolving a
    /// type's canonical key costs no per-call `to_lowercase`.
    type_en_folded: Vec<SmolStr>,
    methods: Vec<PlatformMethod>,
    methods_by_name: FxHashMap<(SmolStr, SmolStr), usize>,
    /// Folded English type name → indices of that type's methods. Replaces the
    /// O(all-methods) per-call `to_lowercase` scan in [`get_type_methods`].
    methods_by_type: FxHashMap<SmolStr, Vec<usize>>,
    /// Folded first segment of a dotted type name (the manager prefix) → indices
    /// of methods on that manager's types. Replaces the O(all-methods) per-call
    /// `to_lowercase` scan in [`get_manager_methods`].
    manager_methods_by_prefix: FxHashMap<SmolStr, Vec<usize>>,
    global_functions: Vec<GlobalFunction>,
    global_functions_by_name: FxHashMap<SmolStr, usize>,
    method_docs_by_id: FxHashMap<u32, usize>,
    global_function_docs_by_id: FxHashMap<u32, usize>,
    constructors: Vec<PlatformConstructor>,
    constructors_by_type: FxHashMap<SmolStr, Vec<usize>>,
    constructor_docs_by_id: FxHashMap<u32, usize>,
    properties: Vec<PlatformProperty>,
    properties_by_name: FxHashMap<(SmolStr, SmolStr), usize>,
    /// Folded English type name → indices of that type's properties. Replaces the
    /// O(all-properties) per-call `to_lowercase` scan in [`get_type_properties`].
    properties_by_type: FxHashMap<SmolStr, Vec<usize>>,
    /// Folded first segment of a dotted type name (the manager prefix) → indices
    /// of properties on that manager's types. Replaces the O(all-properties)
    /// per-call `to_lowercase` scan in [`get_manager_properties`].
    manager_properties_by_prefix: FxHashMap<SmolStr, Vec<usize>>,
    property_docs_by_id: FxHashMap<u32, usize>,
    global_properties_by_name: FxHashMap<SmolStr, usize>,
    /// Type names (folded, RU or EN) shared by more than one platform type —
    /// e.g. `ЭлементыФормы` is both managed-form `FormItems` and legacy
    /// `Controls`. A by-name lookup resolves to an arbitrary one of them, so
    /// per-entry facts that differ between the homonyms (availability,
    /// deprecation) are unreliable for these names.
    ambiguous_type_names: rustc_hash::FxHashSet<SmolStr>,
    /// Folded names (RU and EN) of every standard property contributed by a
    /// per-MDO form extension (`… form extension …`), e.g. the report form's
    /// `ВариантМодифицирован`. The base form contract is deliberately excluded
    /// here — callers pick the managed/ordinary base themselves so an
    /// ordinary-only property is not skipped in a managed form and vice versa.
    /// Writing an extension property is a side effect on the form context, never
    /// a dead local, so the unused-local-variable check skips these.
    form_extension_property_names: rustc_hash::FxHashSet<SmolStr>,
}

impl PlatformDataInner {
    /// The process snapshot. Without a prior [`crate::install_platform_help`] the
    /// first access fixes [`PlatformHelp::unselected`].
    pub fn instance() -> &'static Self {
        PLATFORM_DATA_SINGLETON.get_or_init(|| {
            let help = PlatformHelp::unselected();
            // The corpus-contract suite asserts on the texts of its pinned
            // corpus: running it on the built-in facts or on nothing is an
            // error, never a silent skip.
            #[cfg(corpus_contract)]
            if help
                .origin
                .as_ref()
                .is_none_or(|origin| origin.source != crate::help::PlatformHelpSourceKind::External)
            {
                panic!(
                    "corpus contract run without a usable corpus: {}",
                    help.missing_reason.as_deref().unwrap_or(
                        "the built-in interface facts are served; set BSL_PLATFORM_HELP_CORPUS"
                    )
                );
            }
            Self::from_help(help)
        })
    }

    /// A standalone instance over `help`, outside the process snapshot. Lets tests
    /// and tools compare sources side by side.
    pub fn from_help(help: PlatformHelp) -> Self {
        let PlatformHelp { request, snapshot, origin, missing_reason } = help;
        let PlatformSnapshot {
            mut types,
            mut methods,
            mut global_functions,
            mut constructors,
            properties,
            method_docs,
            global_function_docs,
            constructor_docs,
            property_docs,
        } = snapshot;

        apply_docs_gap_iter_types_overlay(&mut types);
        apply_docs_gap_type_context_overlay(&mut types);

        let mut types_by_name = FxHashMap::default();

        // Several types can declare the same XDTO name (e.g. `Field`, `Parameter`
        // across data-composition types); such an alias is ambiguous and must not
        // be indexed, or it would resolve to an arbitrary one of them.
        let mut xdto_counts: FxHashMap<SmolStr, u32> = FxHashMap::default();
        for ty in &types {
            if let Some(xdto) = &ty.xdto_name {
                *xdto_counts.entry(xdto.fold_lower().into()).or_insert(0) += 1;
            }
        }

        let mut type_en_folded: Vec<SmolStr> = Vec::with_capacity(types.len());
        let mut ambiguous_type_names = rustc_hash::FxHashSet::default();
        for (idx, ty) in types.iter().enumerate() {
            let ru_key: SmolStr = ty.name.fold_lower().into();
            let en_key: SmolStr = ty.english_name.fold_lower().into();
            type_en_folded.push(en_key.clone());
            if let Some(prev) = types_by_name.insert(ru_key.clone(), idx) {
                if prev != idx {
                    ambiguous_type_names.insert(ru_key);
                }
            }
            if let Some(prev) = types_by_name.insert(en_key.clone(), idx) {
                if prev != idx {
                    ambiguous_type_names.insert(en_key);
                }
            }
            // The XDTO name is an additional, non-overriding alias: index it only
            // when unambiguous, and never let it shadow a type's own key.
            if let Some(xdto) = &ty.xdto_name {
                let key: SmolStr = xdto.fold_lower().into();
                if xdto_counts.get(&key) == Some(&1) {
                    types_by_name.entry(key).or_insert(idx);
                }
            }
        }

        apply_docs_gap_method_overlay(&mut methods, &global_functions);
        apply_docs_gap_member_context_overlay(&mut methods, &mut global_functions, &types);

        let mut methods_by_name = FxHashMap::default();
        let mut methods_by_type: FxHashMap<SmolStr, Vec<usize>> = FxHashMap::default();
        let mut manager_methods_by_prefix: FxHashMap<SmolStr, Vec<usize>> = FxHashMap::default();

        let mut type_en_to_ru: FxHashMap<SmolStr, SmolStr> = FxHashMap::default();
        for ty in &types {
            let en_key: SmolStr = ty.english_name.fold_lower().into();
            let ru_key: SmolStr = ty.name.fold_lower().into();
            type_en_to_ru.insert(en_key, ru_key);
        }

        for (idx, method) in methods.iter().enumerate() {
            let en_type_key: SmolStr = method.type_name.fold_lower().into();
            let ru_method_key: SmolStr = method.name.fold_lower().into();
            let en_method_key: SmolStr = method.english_name.fold_lower().into();

            methods_by_type.entry(en_type_key.clone()).or_default().push(idx);
            if let Some((manager_prefix, _)) = en_type_key.split_once('.') {
                manager_methods_by_prefix.entry(manager_prefix.into()).or_default().push(idx);
            }
            methods_by_name.insert((en_type_key.clone(), ru_method_key.clone()), idx);
            methods_by_name.insert((en_type_key.clone(), en_method_key.clone()), idx);

            if let Some(ru_type_key) = type_en_to_ru.get(&en_type_key) {
                methods_by_name.insert((ru_type_key.clone(), ru_method_key), idx);
                methods_by_name.insert((ru_type_key.clone(), en_method_key), idx);
            }
        }

        let mut global_functions_by_name = FxHashMap::default();

        for (idx, function) in global_functions.iter().enumerate() {
            let ru_key: SmolStr = function.name.fold_lower().into();
            let en_key: SmolStr = function.english_name.fold_lower().into();

            global_functions_by_name.insert(ru_key, idx);
            global_functions_by_name.insert(en_key, idx);
        }

        for (ru, legacy_en) in LEGACY_GLOBAL_FUNCTION_EN_ALIASES {
            let ru_key: SmolStr = ru.fold_lower().into();
            if let Some(&idx) = global_functions_by_name.get(&ru_key) {
                let alias_key: SmolStr = legacy_en.fold_lower().into();
                global_functions_by_name.entry(alias_key).or_insert(idx);
            }
        }

        let mut method_docs_by_id = FxHashMap::default();
        for (idx, docs) in method_docs.iter().enumerate() {
            method_docs_by_id.insert(docs.method_id, idx);
        }

        let mut global_function_docs_by_id = FxHashMap::default();
        for (idx, docs) in global_function_docs.iter().enumerate() {
            global_function_docs_by_id.insert(docs.method_id, idx);
        }

        apply_docs_only_variadic_overlay(&mut constructors);

        let mut constructors_by_type: FxHashMap<SmolStr, Vec<usize>> = FxHashMap::default();
        for (idx, ctor) in constructors.iter().enumerate() {
            let en_key: SmolStr = ctor.type_name.fold_lower().into();
            constructors_by_type.entry(en_key.clone()).or_default().push(idx);
            if let Some(ru_key) = type_en_to_ru.get(&en_key) {
                constructors_by_type.entry(ru_key.clone()).or_default().push(idx);
            }
        }

        let mut constructor_docs_by_id = FxHashMap::default();
        for (idx, docs) in constructor_docs.iter().enumerate() {
            constructor_docs_by_id.insert(docs.constructor_id, idx);
        }

        let mut properties_by_name = FxHashMap::default();
        let mut properties_by_type: FxHashMap<SmolStr, Vec<usize>> = FxHashMap::default();
        let mut manager_properties_by_prefix: FxHashMap<SmolStr, Vec<usize>> = FxHashMap::default();
        for (idx, prop) in properties.iter().enumerate() {
            let en_type_key: SmolStr = prop.type_name.fold_lower().into();
            let ru_prop_key: SmolStr = prop.name.fold_lower().into();
            let en_prop_key: SmolStr = prop.english_name.fold_lower().into();

            properties_by_type.entry(en_type_key.clone()).or_default().push(idx);
            if let Some((manager_prefix, _)) = en_type_key.split_once('.') {
                manager_properties_by_prefix.entry(manager_prefix.into()).or_default().push(idx);
            }
            properties_by_name.insert((en_type_key.clone(), ru_prop_key.clone()), idx);
            properties_by_name.insert((en_type_key.clone(), en_prop_key.clone()), idx);

            if let Some(ru_type_key) = type_en_to_ru.get(&en_type_key) {
                properties_by_name.insert((ru_type_key.clone(), ru_prop_key), idx);
                properties_by_name.insert((ru_type_key.clone(), en_prop_key), idx);
            }
        }

        let mut property_docs_by_id = FxHashMap::default();
        for (idx, docs) in property_docs.iter().enumerate() {
            property_docs_by_id.insert(docs.property_id, idx);
        }

        let mut global_properties_by_name = FxHashMap::default();
        for (idx, prop) in properties.iter().enumerate() {
            if prop.type_name.as_str() != GLOBAL_CONTEXT_OWNER {
                continue;
            }
            let ru_key: SmolStr = prop.name.fold_lower().into();
            let en_key: SmolStr = prop.english_name.fold_lower().into();
            global_properties_by_name.insert(ru_key, idx);
            global_properties_by_name.insert(en_key, idx);
        }

        // `ВариантМодифицирован` and the rest of the per-MDO extension surface
        // live on `… form extension …` types, not on the base form contract.
        // Union every such extension property name so the unused-local check does
        // not have to know which extension a given form pulls in; the base
        // contract stays out so the caller's managed/ordinary base choice is
        // preserved. `Form extension for a … field` types are field/control
        // extensions, not form-context ones — their properties (`Документ`,
        // `Ориентация`, …) are ordinary member names, so excluding them keeps a
        // dead `Документ = …` local reportable.
        let mut form_extension_property_names: rustc_hash::FxHashSet<SmolStr> =
            rustc_hash::FxHashSet::default();
        for prop in &properties {
            let type_key = prop.type_name.fold_lower();
            if type_key.contains("form extension") && !type_key.contains("form extension for a") {
                form_extension_property_names.insert(prop.name.fold_lower().into());
                form_extension_property_names.insert(prop.english_name.fold_lower().into());
            }
        }

        Self {
            request,
            origin,
            missing_reason,
            method_docs,
            global_function_docs,
            constructor_docs,
            property_docs,
            types,
            types_by_name,
            type_en_folded,
            methods,
            methods_by_name,
            methods_by_type,
            manager_methods_by_prefix,
            global_functions,
            global_functions_by_name,
            method_docs_by_id,
            global_function_docs_by_id,
            constructors,
            constructors_by_type,
            constructor_docs_by_id,
            properties,
            properties_by_name,
            properties_by_type,
            manager_properties_by_prefix,
            property_docs_by_id,
            global_properties_by_name,
            ambiguous_type_names,
            form_extension_property_names,
        }
    }

    /// The source this instance serves.
    pub fn help_request(&self) -> &PlatformHelpRequest {
        &self.request
    }

    /// Where the served snapshot came from; `None` when no snapshot is served.
    pub fn help_origin(&self) -> Option<&PlatformHelpOrigin> {
        self.origin.as_ref()
    }

    /// Why the requested source serves no corpus of its own: the whole story
    /// when [`Self::help_origin`] is `None`, and why `auto` degraded to the
    /// built-in facts when it serves them.
    pub fn help_missing_reason(&self) -> Option<&str> {
        self.missing_reason.as_deref()
    }

    /// Trust in the served help for a project target. Informational: it never
    /// proves that an API is absent.
    pub fn help_status_for_target(&self, target: Option<&str>) -> PlatformHelpStatus {
        crate::help::status_for_target(self.origin.as_ref(), target)
    }

    /// Whether `name` (any case, RU or EN) names more than one platform type,
    /// making per-entry facts resolved through it unreliable.
    pub fn is_ambiguous_type_name(&self, name: &str) -> bool {
        let key: SmolStr = name.fold_lower().into();
        self.ambiguous_type_names.contains(&key)
    }

    pub fn get_type(&self, name: &str) -> Option<&PlatformType> {
        let key: SmolStr = name.fold_lower().into();
        let idx = *self.types_by_name.get(&key)?;
        self.types.get(idx)
    }

    pub fn all_types(&self) -> &[PlatformType] {
        &self.types
    }

    pub fn get_method(&self, type_name: &str, method_name: &str) -> Option<&PlatformMethod> {
        let type_key: SmolStr = type_name.fold_lower().into();
        let method_key: SmolStr = method_name.fold_lower().into();
        let idx = *self.methods_by_name.get(&(type_key, method_key))?;
        self.methods.get(idx)
    }

    pub fn all_methods(&self) -> &[PlatformMethod] {
        &self.methods
    }

    pub fn get_type_methods(&self, type_name: &str) -> Vec<&PlatformMethod> {
        let type_key: SmolStr = type_name.fold_lower().into();
        let en_type_key: SmolStr = match self.types_by_name.get(&type_key) {
            Some(&idx) => self.type_en_folded[idx].clone(),
            None => type_key,
        };
        match self.methods_by_type.get(&en_type_key) {
            Some(idxs) => idxs.iter().map(|&i| &self.methods[i]).collect(),
            None => Vec::new(),
        }
    }

    pub fn get_manager_methods(&self, manager_prefix: &str) -> Vec<&PlatformMethod> {
        let folded = manager_prefix.fold_lower();
        // The index is keyed by the segment before the first dot; a dotted
        // prefix narrows the bucket with the original `starts_with` check.
        let (head, dotted_rest) = match folded.split_once('.') {
            Some((head, _)) => (head, true),
            None => (folded.as_str(), false),
        };
        let Some(indices) = self.manager_methods_by_prefix.get(head) else {
            return Vec::new();
        };
        if !dotted_rest {
            return indices.iter().filter_map(|&idx| self.methods.get(idx)).collect();
        }
        let full_prefix = format!("{folded}.");
        indices
            .iter()
            .filter_map(|&idx| self.methods.get(idx))
            .filter(|m| m.type_name.fold_lower().starts_with(&full_prefix))
            .collect()
    }

    pub fn get_global_function(&self, name: &str) -> Option<&GlobalFunction> {
        let key: SmolStr = name.fold_lower().into();
        let idx = *self.global_functions_by_name.get(&key)?;
        self.global_functions.get(idx)
    }

    pub fn all_global_functions(&self) -> &[GlobalFunction] {
        &self.global_functions
    }

    pub fn get_method_docs(&self, method_id: u32) -> Option<MethodDocs> {
        let idx = *self.method_docs_by_id.get(&method_id)?;
        self.method_docs.get(idx).cloned()
    }

    pub fn get_constructors(&self, type_name: &str) -> Vec<&PlatformConstructor> {
        let key: SmolStr = type_name.fold_lower().into();
        match self.constructors_by_type.get(&key) {
            Some(indices) => indices.iter().filter_map(|&i| self.constructors.get(i)).collect(),
            None => Vec::new(),
        }
    }

    pub fn all_constructors(&self) -> &[PlatformConstructor] {
        &self.constructors
    }

    pub fn get_constructor_docs(&self, constructor_id: u32) -> Option<ConstructorDocs> {
        let idx = *self.constructor_docs_by_id.get(&constructor_id)?;
        self.constructor_docs.get(idx).cloned()
    }

    pub fn get_global_function_docs(&self, function_id: u32) -> Option<MethodDocs> {
        let idx = *self.global_function_docs_by_id.get(&function_id)?;
        self.global_function_docs.get(idx).cloned()
    }

    pub fn get_property(&self, type_name: &str, prop_name: &str) -> Option<&PlatformProperty> {
        let type_key: SmolStr = type_name.fold_lower().into();
        let prop_key: SmolStr = prop_name.fold_lower().into();
        let idx = *self.properties_by_name.get(&(type_key, prop_key))?;
        self.properties.get(idx)
    }

    pub fn get_type_properties(&self, type_name: &str) -> Vec<&PlatformProperty> {
        let type_key: SmolStr = type_name.fold_lower().into();
        let en_type_key: SmolStr = match self.types_by_name.get(&type_key) {
            Some(&idx) => self.type_en_folded[idx].clone(),
            None => type_key,
        };
        match self.properties_by_type.get(&en_type_key) {
            Some(idxs) => idxs.iter().map(|&i| &self.properties[i]).collect(),
            None => Vec::new(),
        }
    }

    /// Folded names (RU and EN) of every standard property contributed by a
    /// per-MDO form extension. The base form contract is not included — resolve
    /// it via [`get_type_properties`] with the correct managed/ordinary form
    /// type. Assigning an extension property from a form module writes the form
    /// context, so the unused-local-variable check treats these as non-locals.
    pub fn form_extension_property_names(&self) -> &rustc_hash::FxHashSet<SmolStr> {
        &self.form_extension_property_names
    }

    pub fn get_manager_properties(&self, manager_prefix: &str) -> Vec<&PlatformProperty> {
        let folded = manager_prefix.fold_lower();
        // The index is keyed by the segment before the first dot; a dotted
        // prefix narrows the bucket with the original `starts_with` check.
        let (head, dotted_rest) = match folded.split_once('.') {
            Some((head, _)) => (head, true),
            None => (folded.as_str(), false),
        };
        let Some(indices) = self.manager_properties_by_prefix.get(head) else {
            return Vec::new();
        };
        if !dotted_rest {
            return indices.iter().filter_map(|&idx| self.properties.get(idx)).collect();
        }
        let full_prefix = format!("{folded}.");
        indices
            .iter()
            .filter_map(|&idx| self.properties.get(idx))
            .filter(|p| p.type_name.fold_lower().starts_with(&full_prefix))
            .collect()
    }

    pub fn all_properties(&self) -> &[PlatformProperty] {
        &self.properties
    }

    pub fn get_global_property(&self, name: &str) -> Option<&PlatformProperty> {
        let key: SmolStr = name.fold_lower().into();
        let idx = *self.global_properties_by_name.get(&key)?;
        self.properties.get(idx)
    }

    pub fn all_global_properties(&self) -> Vec<&PlatformProperty> {
        let mut seen: Vec<usize> = self.global_properties_by_name.values().copied().collect();
        seen.sort_unstable();
        seen.dedup();
        seen.into_iter().filter_map(|i| self.properties.get(i)).collect()
    }

    pub fn resolve_global_member(
        &self,
        global_name: &str,
        member_name: &str,
    ) -> Option<&PlatformMethod> {
        let prop = self.get_global_property(global_name)?;
        let declared_type = prop.property_types.first()?;
        self.get_method(declared_type.as_str(), member_name)
    }

    pub fn get_property_docs(&self, property_id: u32) -> Option<PropertyDocs> {
        let idx = *self.property_docs_by_id.get(&property_id)?;
        self.property_docs.get(idx).cloned()
    }

    pub fn get_keyword_docs(&self, keyword: &str) -> Option<crate::types::KeywordDocs> {
        get_keyword_docs_static(keyword)
    }
}

/// English synonyms the help archive renamed while the runtime keeps accepting
/// the old spelling. Each `(russian_name, legacy_english)` pair is indexed as
/// an extra lookup key of the canonical entry, so legacy English code still
/// resolves; the canonical (renamed) English name stays the display name.
pub const LEGACY_GLOBAL_FUNCTION_EN_ALIASES: &[(&str, &str)] = &[
    // The 8.3.27 help renamed КопироватьФайл's English synonym to CopyFile
    // (aligning with CopyFileAsync); FileCopy remains callable.
    ("КопироватьФайл", "FileCopy"),
];

/// Since the 8.3.27 help archives, the `СписокЭлементовDOM` page lists only
/// HTML element classes as collection elements, while the same collection is
/// returned by XML DOM traversal too and its own `Элемент` method is still
/// documented as returning `ЭлементDOM`. Keep the generic DOM node union
/// alongside the HTML classes so `Для каждого` over XML DOM lists does not
/// degrade to HTML-only inference.
fn apply_docs_gap_iter_types_overlay(types: &mut [PlatformType]) {
    const DOM_NODE_UNION: &[&str] = &[
        "ЭлементDOM",
        "АтрибутDOM",
        "ДокументDOM",
        "ОпределениеТипаДокументаDOM",
        "НотацияDOM",
        "СущностьDOM",
        "ФрагментДокументаDOM",
        "ТекстDOM",
        "КомментарийDOM",
        "СекцияCDATADOM",
        "ИнструкцияОбработкиDOM",
        "СсылкаНаСущностьDOM",
        "ПространствоИменXPath",
    ];

    let Some(ty) = types.iter_mut().find(|t| t.english_name == "DOMElementList") else {
        return;
    };
    let mut merged: Vec<SmolStr> = DOM_NODE_UNION.iter().map(|s| SmolStr::new(*s)).collect();
    for elem in ty.iter_element_types.drain(..) {
        if !merged.contains(&elem) {
            merged.push(elem);
        }
    }
    ty.iter_element_types = merged;
}

/// Type pages in the help archive occasionally omit environments where the
/// platform demonstrably supports the type: the standard library constructs
/// these types there unconditionally (shortcuts are assigned to form items in
/// `&НаСервере` code, choice parameters and the color chooser are created in
/// `&НаКлиенте` code that also runs in the web client), and 1C:EDT's own
/// availability model agrees. Union the missing environments in so the
/// availability check follows observed platform behavior, not the help text.
fn apply_docs_gap_type_context_overlay(types: &mut [PlatformType]) {
    for ty in types {
        let Some(context) = &mut ty.context else { continue };
        match ty.english_name.as_str() {
            "Shortcut" => context.server = true,
            "ChoiceParameter" | "ColorChooseDialog" => context.web_client = true,
            _ => {}
        }
    }
}

/// Member pages in the help archive systematically understate the web client.
/// Promise-style `*Асинх` members exist precisely so web-client code can drop
/// modal and blocking calls, yet many of their pages omit the web client —
/// 1C:EDT shipped the same corrupted metadata once (1c-edt-issues#783) and
/// corrected its own model; the help itself was never fixed and is not
/// expected to be. The standard library also formats errors through
/// `ОбработкаОшибок` in universal client code that runs in the web client.
/// Union the missing environment in.
fn apply_docs_gap_member_context_overlay(
    methods: &mut [PlatformMethod],
    global_functions: &mut [GlobalFunction],
    types: &[PlatformType],
) {
    const WEB_CAPABLE_ERROR_PROCESSING: &[&str] =
        &["DetailErrorDescription", "ErrorDescriptionForUser"];

    let is_async = |ru: &SmolStr, en: &SmolStr| ru.ends_with("Асинх") || en.ends_with("Async");

    // Thin-client capability marks a genuinely client-side async API: the
    // mobile-only entries (`КаталогБиблиотекиМобильногоУстройстваАсинх`) stay
    // untouched.
    for func in global_functions.iter_mut() {
        if let Some(context) = &mut func.context {
            if is_async(&func.name, &func.english_name) && context.thin_client {
                context.web_client = true;
            }
        }
    }

    // For type methods, follow the (already overlaid) type: an async method of
    // a web-capable type runs in the web client; a type absent from the web
    // client altogether (`HTTPСоединение`) keeps its consistent markup.
    let web_capable_types: rustc_hash::FxHashSet<&str> = types
        .iter()
        .filter(|ty| ty.context.as_ref().is_some_and(|c| c.web_client))
        .map(|ty| ty.english_name.as_str())
        .collect();
    for method in methods.iter_mut() {
        let Some(context) = &mut method.context else { continue };
        if is_async(&method.name, &method.english_name)
            && context.thin_client
            && web_capable_types.contains(method.type_name.as_str())
        {
            context.web_client = true;
        }
        if method.type_name == "ErrorProcessingManager"
            && WEB_CAPABLE_ERROR_PROCESSING.contains(&method.english_name.as_str())
        {
            context.web_client = true;
        }
    }
}

/// The platform ships manager methods whose pages are missing from the help
/// archive, so HBK extraction cannot see them. Each entry pairs the manager's
/// English type name with the method's Russian name; the synthesized method
/// takes its whole signature from the same-named global-context function, so
/// a regenerated corpus keeps the overlay in sync automatically, and the
/// overlay retires itself once the archive gains the real page.
const DOCS_GAP_MANAGER_METHODS: &[(&str, &str)] = &[
    // The `МенеджерОбработкиСтрокиXML` page says the manager both finds and
    // removes disallowed XML characters, and the "see also" of its only
    // documented method links `УдалитьНедопустимыеСимволыXML` as a manager
    // method — but the archive has no page for it; the runtime (and EDT)
    // accept the call.
    ("XMLStringProcessingManager", "УдалитьНедопустимыеСимволыXML"),
];

fn apply_docs_gap_method_overlay(
    methods: &mut Vec<PlatformMethod>,
    global_functions: &[GlobalFunction],
) {
    use crate::types::MethodVariant;

    for (type_name, method_name) in DOCS_GAP_MANAGER_METHODS {
        let exists = methods
            .iter()
            .any(|m| m.type_name == *type_name && m.name.fold_lower() == method_name.fold_lower());
        if exists {
            continue;
        }

        let Some(source) =
            global_functions.iter().find(|f| f.name.fold_lower() == method_name.fold_lower())
        else {
            continue;
        };

        // A corpus whose ids already reach the top of the range leaves no id for
        // the synthesized method; it then stays without it rather than alias one.
        let Some(next_id) = methods.iter().map(|m| m.id).max().unwrap_or(0).checked_add(1) else {
            continue;
        };
        methods.push(PlatformMethod {
            id: next_id,
            type_name: SmolStr::new(*type_name),
            name: source.name.clone(),
            english_name: source.english_name.clone(),
            return_type: source.return_type.clone(),
            parameters: source.parameters.clone(),
            variants: source
                .variants
                .iter()
                .map(|v| MethodVariant {
                    variant_name: v.variant_name.clone(),
                    parameters: v.parameters.clone(),
                })
                .collect(),
            min_version: source.min_version.clone(),
            context: source.context,
        });
    }
}

fn apply_docs_only_variadic_overlay(constructors: &mut [PlatformConstructor]) {
    const OVERLAY: &[(&str, &str)] = &[
        ("Structure", "По ключам и значениям"),
        ("FixedStructure", "По ключам и значениям"),
        ("DynamicListRowKey", "На основе путей и значений полей"),
    ];

    for ctor in constructors.iter_mut() {
        let variant = ctor.variant_name.as_deref().unwrap_or("");
        if !OVERLAY.iter().any(|(t, v)| *t == ctor.type_name.as_str() && *v == variant) {
            continue;
        }
        if let Some(last) = ctor.parameters.last_mut() {
            last.is_variadic = true;
        }
    }
}

fn get_keyword_docs_static(keyword: &str) -> Option<crate::types::KeywordDocs> {
    use crate::types::{KeywordDocs, ParamDocs};
    use smol_str::SmolStr;

    let keyword_lower = keyword.fold_lower();

    match keyword_lower.as_str() {
        "вызватьисключение" | "raise" => Some(KeywordDocs {
            keyword_ru: SmolStr::new("ВызватьИсключение"),
            keyword_en: SmolStr::new("Raise"),
            syntax: "ВызватьИсключение;".to_string(),
            description: "Оператор позволяет вызвать исключение в тех случаях, когда несмотря на отработку исключительной ситуации операторами исключения необходимо прервать выполнение модуля с ошибкой времени выполнения.\n\nОператор допустим только внутри операторных скобок Исключение – КонецПопытки.\n\nВыполнение данного оператора прекращает выполнение последовательности операторов исключения и производит поиск более \"внешнего\" обработчика исключения (при вложенных попытках). Если таковой есть, то управление передается на его первый оператор. Если нет, то исключительная ситуация обрабатывается системно, выдается сообщение о первоначально возникшей ошибке, а выполнение модуля прекращается.".to_string(),
            params: vec![],
            min_version: Some("8.0".to_string()),
        }),
        "для" | "for" => Some(KeywordDocs {
            keyword_ru: SmolStr::new("Для"),
            keyword_en: SmolStr::new("For"),
            syntax: "Для <Имя переменной> = <Выражение 1> По <Выражение 2> Цикл\n    // Операторы\n    [Прервать;]\n    [Продолжить;]\nКонецЦикла;".to_string(),
            description: "Оператор цикла Для предназначен для циклического повторения операторов, находящихся внутри конструкции Цикл – КонецЦикла.\n\nПеред началом выполнения цикла значение <Выражение 1> присваивается переменной <Имя переменной>. Значение <Имя переменной> автоматически увеличивается при каждом проходе цикла. Величина приращения счетчика при каждом выполнении цикла равна 1.\n\nЦикл выполняется, пока значение переменной меньше или равно значению <Выражение 2>. Условие выполнения цикла всегда проверяется в начале, перед выполнением цикла.".to_string(),
            params: vec![
                ParamDocs {
                    name: SmolStr::new("Имя переменной"),
                    description: "Идентификатор переменной (счетчика цикла), значение которой автоматически увеличивается на 1 при каждом повторении цикла.".to_string(),
                    default_value: None,
                },
                ParamDocs {
                    name: SmolStr::new("Выражение 1"),
                    description: "Числовое выражение, которое задает начальное значение, присваиваемое счетчику цикла при первом проходе цикла.".to_string(),
                    default_value: None,
                },
                ParamDocs {
                    name: SmolStr::new("Выражение 2"),
                    description: "Максимальное значение счетчика цикла. Когда переменная становится больше чем <Выражение 2>, выполнение оператора цикла Для прекращается.".to_string(),
                    default_value: None,
                },
            ],
            min_version: Some("8.0".to_string()),
        }),
        "если" | "if" => Some(KeywordDocs {
            keyword_ru: SmolStr::new("Если"),
            keyword_en: SmolStr::new("If"),
            syntax: "Если <Условие> Тогда\n    // Операторы\n[ИначеЕсли <Условие> Тогда]\n    // Операторы\n[Иначе]\n    // Операторы\nКонецЕсли;".to_string(),
            description: "Условный оператор Если предназначен для организации выполнения или невыполнения некоторого набора операторов в зависимости от заданных условий.\n\nУсловие – это выражение булева типа. Если значение выражения равно Истина, то выполняются операторы, следующие за ключевым словом Тогда до ближайшего ключевого слова ИначеЕсли, Иначе или КонецЕсли.".to_string(),
            params: vec![
                ParamDocs {
                    name: SmolStr::new("Условие"),
                    description: "Выражение булева типа. Если значение равно Истина, выполняются операторы в соответствующем блоке.".to_string(),
                    default_value: None,
                },
            ],
            min_version: Some("8.0".to_string()),
        }),
        "попытка" | "try" => Some(KeywordDocs {
            keyword_ru: SmolStr::new("Попытка"),
            keyword_en: SmolStr::new("Try"),
            syntax: "Попытка\n    // Операторы попытки\nИсключение\n    // Операторы исключения\n    [ВызватьИсключение;]\nКонецПопытки;".to_string(),
            description: "Оператор Попытка управляет выполнением программы, основываясь на возникающих при выполнении модуля ошибочных (исключительных) ситуациях, и определяет обработку этих ситуаций.\n\nЕсли при выполнении последовательности операторов попытки произошла ошибка времени выполнения, то выполнение оператора, вызвавшего ошибку, прерывается и управление передается на первый оператор последовательности операторов исключения.\n\nКонструкции Попытка – Исключение – КонецПопытки могут быть вложенными.".to_string(),
            params: vec![],
            min_version: Some("8.0".to_string()),
        }),
        _ => None,
    }
}

pub type PlatformData = PlatformDataInner;

/// `heap_size` estimators for Salsa's `memory_usage` report on the platform
/// lookup queries. Each thinly wraps the target type's own `estimated_heap_size`
/// (defined next to the type in `types.rs`) for the `Option`/`Arc<Vec<_>>`
/// shape the query actually memoises.
mod heap_estimate {
    use std::sync::Arc;

    use super::{
        GlobalFunction, PlatformConstructor, PlatformMethod, PlatformProperty, PlatformType,
    };

    pub(super) fn platform_type_heap(v: &Option<PlatformType>) -> usize {
        v.as_ref().map_or(0, PlatformType::estimated_heap_size)
    }

    pub(super) fn platform_method_heap(v: &Option<PlatformMethod>) -> usize {
        v.as_ref().map_or(0, PlatformMethod::estimated_heap_size)
    }

    pub(super) fn method_vec_heap(v: &Arc<Vec<PlatformMethod>>) -> usize {
        stdx::heap::vec_bytes::<PlatformMethod>(v.len())
            + v.iter().map(PlatformMethod::estimated_heap_size).sum::<usize>()
    }

    pub(super) fn global_function_heap(v: &Option<GlobalFunction>) -> usize {
        v.as_ref().map_or(0, GlobalFunction::estimated_heap_size)
    }

    pub(super) fn constructor_vec_heap(v: &Arc<Vec<PlatformConstructor>>) -> usize {
        stdx::heap::vec_bytes::<PlatformConstructor>(v.len())
            + v.iter().map(PlatformConstructor::estimated_heap_size).sum::<usize>()
    }

    pub(super) fn platform_property_heap(v: &Option<PlatformProperty>) -> usize {
        v.as_ref().map_or(0, PlatformProperty::estimated_heap_size)
    }

    pub(super) fn property_vec_heap(v: &Arc<Vec<PlatformProperty>>) -> usize {
        stdx::heap::vec_bytes::<PlatformProperty>(v.len())
            + v.iter().map(PlatformProperty::estimated_heap_size).sum::<usize>()
    }

    /// Heap of a [`super::TypeNameInput`]: its `name` string's own bytes. The
    /// estimator receives the tuple of ALL declared fields in order (a 1-tuple
    /// here), per the fork's `heap_size = path` convention for interned structs.
    pub(super) fn type_name_input_heap((name,): &(String,)) -> usize {
        name.capacity()
    }

    /// Heap of a [`super::MethodLookupInput`]: both owned strings.
    pub(super) fn method_lookup_input_heap((type_name, method_name): &(String, String)) -> usize {
        type_name.capacity() + method_name.capacity()
    }

    /// Heap of a [`super::PrefixedMethodLookupInput`]: both owned strings.
    pub(super) fn prefixed_method_lookup_input_heap(
        (prefix, method_name): &(String, String),
    ) -> usize {
        prefix.capacity() + method_name.capacity()
    }
}

#[salsa::interned(debug, heap_size = heap_estimate::type_name_input_heap)]
pub struct TypeNameInput {
    pub name: String,
}

#[salsa::interned(debug, heap_size = heap_estimate::method_lookup_input_heap)]
pub struct MethodLookupInput {
    pub type_name: String,
    pub method_name: String,
}

#[salsa::interned(debug, heap_size = heap_estimate::prefixed_method_lookup_input_heap)]
pub struct PrefixedMethodLookupInput {
    pub prefix: String,
    pub method_name: String,
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::platform_type_heap, returns(as_ref))]
pub fn platform_type_query<'db>(
    db: &'db dyn salsa::Database,
    input: TypeNameInput<'db>,
) -> Option<PlatformType> {
    let name = input.name(db);
    let data = PlatformDataInner::instance();
    data.get_type(name).cloned()
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::platform_method_heap, returns(as_ref))]
pub fn platform_method_query<'db>(
    db: &'db dyn salsa::Database,
    input: MethodLookupInput<'db>,
) -> Option<PlatformMethod> {
    let type_name = input.type_name(db);
    let method_name = input.method_name(db);
    let data = PlatformDataInner::instance();
    data.get_method(type_name, method_name).cloned()
}

#[salsa::tracked(lru = 128, heap_size = heap_estimate::method_vec_heap, returns(ref))]
pub fn type_methods_query<'db>(
    db: &'db dyn salsa::Database,
    input: TypeNameInput<'db>,
) -> Arc<Vec<PlatformMethod>> {
    let type_name = input.name(db);
    let data = PlatformDataInner::instance();
    Arc::new(data.get_type_methods(type_name).into_iter().cloned().collect())
}

#[salsa::tracked(lru = 128, heap_size = heap_estimate::method_vec_heap, returns(ref))]
pub fn manager_methods_query<'db>(
    db: &'db dyn salsa::Database,
    input: TypeNameInput<'db>,
) -> Arc<Vec<PlatformMethod>> {
    let prefix = input.name(db);
    let data = PlatformDataInner::instance();
    Arc::new(data.get_manager_methods(prefix).into_iter().cloned().collect())
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::platform_method_heap, returns(as_ref))]
pub fn prefixed_method_query<'db>(
    db: &'db dyn salsa::Database,
    input: PrefixedMethodLookupInput<'db>,
) -> Option<PlatformMethod> {
    let prefix = input.prefix(db);
    let method_name = input.method_name(db);
    find_prefixed_method(prefix, method_name)
}

pub fn find_prefixed_method(prefix: &str, method_name: &str) -> Option<PlatformMethod> {
    find_prefixed_methods(prefix, method_name).into_iter().next()
}

/// Отображаемые имена метода: русское и английское.
///
/// У шаблонных записей менеджера (`СправочникМенеджер.<Имя>`) поле `name` в каталоге
/// платформы усечено до `&lt;Имя`, а настоящее имя лежит в синтаксисе документации;
/// английское при этом написано целиком, вместе с владельцем. Правило нужно и поиску
/// метода по имени, и любому показу члена, поэтому живёт рядом с самим каталогом.
pub fn method_display_names(method: &PlatformMethod) -> (String, String) {
    let english = method
        .english_name
        .rsplit_once('.')
        .map(|(_, name)| name)
        .unwrap_or(method.english_name.as_str())
        .to_owned();
    if !method.name.starts_with("&lt;") && !method.name.starts_with('<') {
        return (method.name.to_string(), english);
    }
    let russian = PlatformDataInner::instance()
        .get_method_docs(method.id)
        .and_then(|docs| docs.syntax.split('(').next().map(str::trim).map(str::to_owned))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| english.clone());
    (russian, english)
}

pub fn find_prefixed_methods(prefix: &str, method_name: &str) -> Vec<PlatformMethod> {
    let method_lower = method_name.fold_lower();
    let data = PlatformDataInner::instance();
    let methods = data
        .get_manager_methods(prefix)
        .into_iter()
        .filter(|method| {
            let docs = data.get_method_docs(method.id);
            let ru_match = docs
                .as_ref()
                .and_then(|d| d.syntax.split('(').next())
                .is_some_and(|ru| ru.fold_lower() == method_lower);
            if ru_match {
                return true;
            }
            let en_name = method
                .english_name
                .rsplit_once('.')
                .map(|(_, n)| n)
                .unwrap_or(&method.english_name);
            en_name.fold_lower() == method_lower
        })
        .cloned()
        .collect();
    sort_and_deduplicate_methods(methods)
}

#[cfg(test)]
mod display_name_tests {
    use super::*;

    /// Метод не показывается усечённым шаблонным именем.
    ///
    /// Вход, на котором нарушение видно, в каталоге есть и он массовый: у 890 методов
    /// прикладных типов (`CatalogManager.&lt;Catalog name&gt;` и родня) поле `name` усечено
    /// до `&lt;Имя`, а настоящее имя лежит в синтаксисе документации. Проверка требует,
    /// чтобы такие записи в каталоге БЫЛИ, иначе она зелена вхолостую.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn template_manager_methods_display_their_real_name() {
        let data = PlatformDataInner::instance();
        let templated: Vec<_> =
            data.all_methods().iter().filter(|method| method.name.starts_with("&lt;")).collect();

        assert!(!templated.is_empty(), "в каталоге нет усечённых имён — проверять нечего");
        for method in &templated {
            let (russian, english) = method_display_names(method);
            assert!(
                !russian.starts_with("&lt;") && !russian.is_empty(),
                "{russian:?} у метода {} остаётся усечённым",
                method.english_name
            );
            assert!(!english.contains('.'), "{english:?} несёт владельца вместо имени");
        }
        assert!(
            templated.iter().any(|method| method_display_names(method).0 == "СоздатьЭлемент"),
            "СоздатьЭлемент обязан находиться среди шаблонных методов менеджера справочника"
        );
    }
}

fn sort_and_deduplicate_methods(mut methods: Vec<PlatformMethod>) -> Vec<PlatformMethod> {
    methods.sort_by_key(|method| method.id);
    methods.dedup_by_key(|method| method.id);
    methods
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::global_function_heap, returns(as_ref))]
pub fn global_function_query<'db>(
    db: &'db dyn salsa::Database,
    input: TypeNameInput<'db>,
) -> Option<GlobalFunction> {
    let name = input.name(db);
    let data = PlatformDataInner::instance();
    data.get_global_function(name).cloned()
}

#[salsa::tracked(lru = 128, heap_size = heap_estimate::constructor_vec_heap, returns(ref))]
pub fn platform_constructors_query<'db>(
    db: &'db dyn salsa::Database,
    input: TypeNameInput<'db>,
) -> Arc<Vec<PlatformConstructor>> {
    let type_name = input.name(db);
    let data = PlatformDataInner::instance();
    Arc::new(data.get_constructors(type_name).into_iter().cloned().collect())
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::platform_property_heap, returns(as_ref))]
pub fn platform_property_query<'db>(
    db: &'db dyn salsa::Database,
    input: MethodLookupInput<'db>,
) -> Option<PlatformProperty> {
    let type_name = input.type_name(db);
    let prop_name = input.method_name(db);
    let data = PlatformDataInner::instance();
    data.get_property(type_name, prop_name).cloned()
}

#[salsa::tracked(lru = 128, heap_size = heap_estimate::property_vec_heap, returns(ref))]
pub fn type_properties_query<'db>(
    db: &'db dyn salsa::Database,
    input: TypeNameInput<'db>,
) -> Arc<Vec<PlatformProperty>> {
    let type_name = input.name(db);
    let data = PlatformDataInner::instance();
    Arc::new(data.get_type_properties(type_name).into_iter().cloned().collect())
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::platform_property_heap, returns(as_ref))]
pub fn global_property_query<'db>(
    db: &'db dyn salsa::Database,
    input: TypeNameInput<'db>,
) -> Option<PlatformProperty> {
    let name = input.name(db);
    let data = PlatformDataInner::instance();
    data.get_global_property(name).cloned()
}

#[salsa::tracked(lru = 256, heap_size = heap_estimate::platform_method_heap, returns(as_ref))]
pub fn global_member_method_query<'db>(
    db: &'db dyn salsa::Database,
    input: MethodLookupInput<'db>,
) -> Option<PlatformMethod> {
    let global_name = input.type_name(db);
    let member_name = input.method_name(db);
    let data = PlatformDataInner::instance();
    data.resolve_global_member(global_name, member_name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_data_singleton() {
        let data1 = PlatformDataInner::instance();
        let data2 = PlatformDataInner::instance();

        assert!(std::ptr::eq(data1, data2));
    }

    #[test]
    fn test_get_type_case_insensitive() {
        let data = PlatformDataInner::instance();

        if data.all_types().is_empty() {
            println!("Skipping test: no platform data available");
            return;
        }

        let ty = data.all_types().first().unwrap();
        let name = ty.name.as_str();

        assert!(data.get_type(name).is_some());
        assert!(data.get_type(&name.fold_lower()).is_some());
        assert!(data.get_type(&name.to_uppercase()).is_some());
    }

    #[test]
    fn test_get_type_by_unique_xdto_name() {
        let data = PlatformDataInner::instance();
        if data.all_types().is_empty() {
            return;
        }
        // `FlowchartContextType` is the unique XDTO name of `ГрафическаяСхема`.
        let via_xdto = data.get_type("FlowchartContextType");
        let via_class = data.get_type("GraphicalSchema");
        assert!(via_xdto.is_some(), "unique XDTO name must resolve");
        assert_eq!(
            via_xdto.map(|t| t.english_name.as_str()),
            via_class.map(|t| t.english_name.as_str()),
            "XDTO alias must resolve to the same type as the class name"
        );
    }

    #[test]
    fn test_ambiguous_xdto_name_is_not_indexed() {
        let data = PlatformDataInner::instance();
        if data.all_types().is_empty() {
            return;
        }
        // `ParameterValue` is the XDTO name of several data-composition types and
        // is not itself a class name, so it must not resolve to an arbitrary one.
        assert!(data.get_type("ParameterValue").is_none());
    }

    #[test]
    fn docs_gap_overlay_widens_type_contexts() {
        let data = PlatformDataInner::instance();
        if data.all_types().is_empty() {
            return;
        }

        let shortcut = data.get_type("СочетаниеКлавиш").expect("Shortcut must exist");
        let ctx = shortcut.context.as_ref().expect("Shortcut must carry availability");
        assert!(ctx.server, "overlay must add the server context to Shortcut");
        assert!(ctx.thin_client, "archive contexts must survive the overlay");

        for name in ["ПараметрВыбора", "ДиалогВыбораЦвета"] {
            let ty = data.get_type(name).expect("type must exist");
            let ctx = ty.context.as_ref().expect("type must carry availability");
            assert!(ctx.web_client, "overlay must add the web-client context to {name}");
        }
    }

    #[test]
    fn docs_gap_overlay_widens_member_contexts() {
        let data = PlatformDataInner::instance();
        if data.all_methods().is_empty() {
            return;
        }

        for name in ["ПредупреждениеАсинх", "ВвестиЧислоАсинх", "ОткрытьЗначениеАсинх"]
        {
            let func = data.get_global_function(name).expect("async global must exist");
            let ctx = func.context.as_ref().expect("async global must carry availability");
            assert!(ctx.web_client, "overlay must add the web client to {name}");
            assert!(!ctx.server, "async dialogs stay client-side: {name}");
        }
        let mobile_only = data
            .get_global_function("КаталогБиблиотекиМобильногоУстройстваАсинх")
            .expect("mobile async global must exist");
        assert!(
            !mobile_only.context.as_ref().unwrap().web_client,
            "a mobile-only async API must stay out of the web client"
        );

        // Async methods follow their type: web-capable types gain the method
        // in the web client, a web-absent type keeps its consistent markup.
        for (ty, method) in [
            ("МенеджерФайловыхПотоков", "ОткрытьАсинх"),
            ("СписокЗначений", "ВыбратьЭлементАсинх"),
            ("ЧтениеДанных", "ПрочитатьАсинх"),
        ] {
            let m = data.get_method(ty, method).expect("async method must exist");
            assert!(
                m.context.as_ref().unwrap().web_client,
                "overlay must add the web client to {ty}.{method}"
            );
        }
        let http = data
            .get_method("HTTPСоединение", "ПолучитьАсинх")
            .expect("HTTP async method must exist");
        assert!(
            !http.context.as_ref().unwrap().web_client,
            "HTTPСоединение is not web-capable — its async methods must not become so"
        );

        let method = data
            .get_method("МенеджерОбработкиОшибок", "ПодробноеПредставлениеОшибки")
            .expect("error-processing method must exist");
        let ctx = method.context.as_ref().expect("method must carry availability");
        assert!(ctx.web_client, "overlay must add the web client to ПодробноеПредставлениеОшибки");
    }

    #[test]
    fn docs_gap_overlay_exposes_xml_string_processing_delete() {
        let data = PlatformDataInner::instance();
        if data.all_methods().is_empty() {
            return;
        }

        let method = data
            .get_method("МенеджерОбработкиСтрокиXML", "УдалитьНедопустимыеСимволыXML")
            .expect("docs-gap overlay must expose the manager method");

        // The signature is derived from the same-named global function at
        // load time; a mismatch means the overlay picked the wrong source.
        let source = data
            .get_global_function("УдалитьНедопустимыеСимволыXML")
            .expect("the source global function must exist");
        assert_eq!(method.english_name, source.english_name);
        assert_eq!(method.parameters, source.parameters);
        assert!(!method.parameters.is_empty(), "derived signature must carry parameters");

        let via_en = data.get_method("XMLStringProcessingManager", "DeleteDisallowedXMLCharacters");
        assert!(via_en.is_some(), "overlay method must resolve by English names too");

        let via_global =
            data.resolve_global_member("ОбработкаСтрокиXML", "УдалитьНедопустимыеСимволыXML");
        assert!(via_global.is_some(), "manager method must resolve through the global property");
    }

    #[test]
    fn legacy_english_global_function_aliases_resolve() {
        let data = PlatformDataInner::instance();
        if data.all_global_functions().is_empty() {
            return;
        }

        for (ru, legacy_en) in LEGACY_GLOBAL_FUNCTION_EN_ALIASES {
            let canonical = data
                .get_global_function(ru)
                .unwrap_or_else(|| panic!("{ru} must exist in the corpus"));
            let via_alias = data
                .get_global_function(legacy_en)
                .unwrap_or_else(|| panic!("legacy alias {legacy_en} must resolve"));
            assert_eq!(via_alias.id, canonical.id, "{legacy_en} must alias {ru}");
            assert_ne!(
                canonical.english_name.fold_lower(),
                legacy_en.fold_lower(),
                "{legacy_en} duplicates the canonical English name — drop the stale alias"
            );
        }
    }

    #[test]
    fn dom_element_list_iterates_dom_nodes_and_html_elements() {
        let data = PlatformDataInner::instance();
        if data.all_types().is_empty() {
            return;
        }

        let ty = data.get_type("СписокЭлементовDOM").expect("type must exist");
        for expected in ["ЭлементDOM", "ТекстDOM", "ЭлементHTML"] {
            assert!(
                ty.iter_element_types.iter().any(|e| e == expected),
                "СписокЭлементовDOM must iterate {expected}, got {:?}",
                ty.iter_element_types
            );
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn curated_overlay_corrects_dom_append_child_in_both_languages() {
        let data = PlatformDataInner::instance();
        let via_ru = data
            .get_method("ДокументDOM", "ДобавитьДочерний")
            .expect("DOM append-child method must exist by Russian names");
        let via_en = data
            .get_method("DOMDocument", "AppendChild")
            .expect("DOM append-child method must exist by English names");
        let expected = "АтрибутDOM, ДокументDOM, ЭлементDOM, ОпределениеТипаДокументаDOM, НотацияDOM, АтрибутHTML, ЭлементHTML, ЭлементКнопкаHTML, ЭлементВводаHTML, ЭлементЗаголовокHTML";

        assert_eq!(via_ru.id, 2231, "overlay must preserve the extracted method ID");
        assert_eq!(via_ru.id, via_en.id, "RU and EN names must resolve to one method");
        assert_eq!(via_ru.parameters.len(), 1, "overlay must preserve parameter order");
        assert_eq!(via_ru.parameters[0].param_type.as_deref(), Some(expected));
        assert_eq!(via_en.parameters[0].param_type.as_deref(), Some(expected));
    }

    #[test]
    fn user_password_policies_global_property_is_typed() {
        let data = PlatformDataInner::instance();
        if data.all_types().is_empty() {
            return;
        }

        let prop = data
            .get_global_property("ПолитикиПаролейПользователей")
            .expect("global property must exist");
        assert_eq!(
            prop.property_types.first().map(|t| t.as_str()),
            Some("МенеджерПолитикПаролейПользователей")
        );

        for method in ["НайтиПоИмени", "СоздатьПолитику", "ПроверитьСоответствиеПароляПолитике"]
        {
            assert!(
                data.resolve_global_member("ПолитикиПаролейПользователей", method).is_some(),
                "{method} must resolve through the global property"
            );
        }
    }

    #[test]
    fn test_get_method_bilingual() {
        let data = PlatformDataInner::instance();

        if data.all_methods().is_empty() {
            println!("Skipping test: no platform methods available");
            return;
        }

        let method = data.all_methods().first().unwrap();
        let type_name = method.type_name.as_str();
        let ru_name = method.name.as_str();
        let en_name = method.english_name.as_str();

        let found = data.get_method(type_name, ru_name);
        assert!(found.is_some(), "Should find method by Russian name");
        assert_eq!(found.unwrap().id, method.id);

        let found = data.get_method(type_name, en_name);
        assert!(found.is_some(), "Should find method by English name");
        assert_eq!(found.unwrap().id, method.id);

        let found = data.get_method(&type_name.to_uppercase(), &ru_name.to_uppercase());
        assert!(found.is_some(), "Should be case-insensitive");
    }

    #[test]
    fn test_get_type_methods() {
        let data = PlatformDataInner::instance();

        if data.all_types().is_empty() || data.all_methods().is_empty() {
            println!("Skipping test: no platform data available");
            return;
        }

        let ty = data.all_types().first().unwrap();
        let methods = data.get_type_methods(&ty.english_name);

        if !methods.is_empty() {
            println!("Type {} has {} methods", ty.english_name, methods.len());
            for method in &methods {
                assert_eq!(method.type_name.fold_lower(), ty.english_name.fold_lower());
            }
        }
    }

    #[salsa::db]
    #[derive(Clone, Default)]
    struct TestDatabase {
        storage: salsa::Storage<Self>,
    }

    impl salsa::Database for TestDatabase {}

    #[test]
    fn test_platform_type_query() {
        let db = TestDatabase::default();

        let data = PlatformDataInner::instance();
        if data.all_types().is_empty() {
            println!("Skipping test: no platform data available");
            return;
        }

        let input1 = TypeNameInput::new(&db, "Строка".to_string());
        let input2 = TypeNameInput::new(&db, "СТРОКА".to_string());
        let input3 = TypeNameInput::new(&db, "String".to_string());

        let ty1 = platform_type_query(&db, input1);
        let ty2 = platform_type_query(&db, input2);
        let ty3 = platform_type_query(&db, input3);

        assert_eq!(ty1.is_some(), ty2.is_some());
        assert_eq!(ty1.is_some(), ty3.is_some());

        if let (Some(t1), Some(t2), Some(t3)) = (ty1, ty2, ty3) {
            assert_eq!(t1.name, t2.name);
            assert_eq!(t1.name, t3.name);
        }
    }

    #[test]
    fn test_platform_method_query() {
        let db = TestDatabase::default();

        let data = PlatformDataInner::instance();
        if data.all_methods().is_empty() {
            println!("Skipping test: no platform methods available");
            return;
        }

        let input = MethodLookupInput::new(&db, "Строка".to_string(), "ВРег".to_string());
        let method = platform_method_query(&db, input);

        if let Some(method) = method {
            assert_eq!(method.name.as_str(), "ВРег");
            assert_eq!(method.english_name.as_str(), "Upper");
        }
    }

    #[test]
    fn test_type_methods_query() {
        let db = TestDatabase::default();

        let data = PlatformDataInner::instance();
        if data.all_types().is_empty() {
            println!("Skipping test: no platform data available");
            return;
        }

        let input = TypeNameInput::new(&db, "Строка".to_string());
        let methods = type_methods_query(&db, input);

        if !methods.is_empty() {
            for method in methods.iter() {
                assert_eq!(method.type_name.fold_lower(), "строка");
            }
        }
    }

    #[test]
    fn test_global_functions() {
        let data = PlatformDataInner::instance();

        if data.all_global_functions().is_empty() {
            println!("Skipping test: no global functions available");
            return;
        }

        println!("Total global functions: {}", data.all_global_functions().len());

        let func = data.get_global_function("НачатьТранзакцию");
        assert!(func.is_some(), "Should find НачатьТранзакцию");

        let func = func.unwrap();
        assert_eq!(func.name.as_str(), "НачатьТранзакцию");
        assert_eq!(func.english_name.as_str(), "BeginTransaction");
        println!("Found function: {} / {}", func.name, func.english_name);

        let func_en = data.get_global_function("BeginTransaction");
        assert!(func_en.is_some(), "Should find by English name");
        assert_eq!(func_en.unwrap().id, func.id);

        let func_upper = data.get_global_function("НАЧАТЬТРАНЗАКЦИЮ");
        assert!(func_upper.is_some(), "Should be case-insensitive");
        assert_eq!(func_upper.unwrap().id, func.id);
    }

    #[test]
    fn test_is_variadic_marks_expected_unbounded_entries_only() {
        let data = PlatformDataInner::instance();
        let funcs = data.all_global_functions();
        if funcs.is_empty() {
            println!("Skipping test: no global functions available");
            return;
        }
        let expected_global_ru: &[&str] = &["Мин", "Макс", "ПродолжитьВызов"];
        let mut seen_global_hits = 0usize;

        for func in funcs {
            // Since the 8.3.27 help archives the unbounded tail of these
            // functions is folded into the parameter name
            // (`Значение1,...,ЗначениеN`); hir-ty's name idiom consumes that
            // shape, so no global function carries an is_variadic flag.
            if expected_global_ru.contains(&func.name.as_str()) {
                let last = func
                    .parameters
                    .last()
                    .unwrap_or_else(|| panic!("{} must declare parameters", func.name));
                // The exact folded shape matters: hir-ty's
                // `name_implies_unbounded_variadic` requires `<word><digits>`
                // before the ellipsis and the same word with a letter suffix
                // after it, so a looser substring check could pass names the
                // idiom rejects.
                assert_eq!(
                    last.name.as_str(),
                    "Значение1,...,ЗначениеN",
                    "{} last param must carry the unbounded name idiom",
                    func.name
                );
                seen_global_hits += 1;
            }
            for param in &func.parameters {
                assert!(
                    !param.is_variadic,
                    "{} param {} unexpectedly carries is_variadic=true",
                    func.name, param.name
                );
            }
            for variant in &func.variants {
                for param in &variant.parameters {
                    assert!(
                        !param.is_variadic,
                        "global variant param must stay non-variadic \
                         (function={}, variant={:?}, param={})",
                        func.name, variant.variant_name, param.name
                    );
                }
            }
        }
        assert_eq!(
            seen_global_hits,
            expected_global_ru.len(),
            "every entry in `expected_global_ru` must be present in the platform corpus \
             — a missing global silently passing the per-function loop is a regression"
        );

        let expected_ctors: &[(&str, &str)] = &[
            ("Array", "По количеству элементов"),
            ("COMSafeArray", "Из массива 1"),
            ("COMSafeArray", "По типу элемента 1"),
            ("Structure", "По ключам и значениям"),
            ("FixedStructure", "По ключам и значениям"),
            ("DynamicListRowKey", "На основе путей и значений полей"),
        ];
        let mut seen_ctor_hits = 0usize;
        for ctor in data.all_constructors() {
            let ctor_key = (ctor.type_name.as_str(), ctor.variant_name.as_deref().unwrap_or(""));
            let expected = expected_ctors.contains(&ctor_key);
            let last_idx = ctor.parameters.len().saturating_sub(1);
            for (idx, param) in ctor.parameters.iter().enumerate() {
                if expected && idx == last_idx {
                    assert!(
                        param.is_variadic,
                        "{}.{} must mark its last param ({}) as variadic",
                        ctor.type_name, ctor_key.1, param.name
                    );
                    seen_ctor_hits += 1;
                } else {
                    assert!(
                        !param.is_variadic,
                        "{}.{:?} param {} unexpectedly carries is_variadic=true",
                        ctor.type_name, ctor.variant_name, param.name
                    );
                }
            }
        }
        assert_eq!(
            seen_ctor_hits,
            expected_ctors.len(),
            "every entry in `expected_ctors` must be present in the platform corpus"
        );
    }

    #[test]
    fn test_platform_constructors_query_bilingual() {
        let db = TestDatabase::default();
        let data = PlatformDataInner::instance();
        if data.all_constructors().is_empty() {
            println!("Skipping test: no constructor data");
            return;
        }

        let ru = platform_constructors_query(&db, TypeNameInput::new(&db, "Массив".to_string()));
        let en = platform_constructors_query(&db, TypeNameInput::new(&db, "Array".to_string()));

        assert!(!ru.is_empty(), "Массив must have at least one constructor");
        assert_eq!(ru.len(), en.len(), "RU and EN lookups must return the same overload set");
        let ru_ids: Vec<u32> = ru.iter().map(|c| c.id).collect();
        let en_ids: Vec<u32> = en.iter().map(|c| c.id).collect();
        assert_eq!(ru_ids, en_ids);
    }

    #[test]
    fn test_platform_constructors_query_unknown() {
        let db = TestDatabase::default();
        let result = platform_constructors_query(
            &db,
            TypeNameInput::new(&db, "ЗаведомоНесуществующийТип".to_string()),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn test_platform_property_query_bilingual() {
        let db = TestDatabase::default();
        let data = PlatformDataInner::instance();
        if data.all_properties().is_empty() {
            println!("Skipping test: no property data");
            return;
        }

        let ru_ru = platform_property_query(
            &db,
            MethodLookupInput::new(&db, "Запрос".to_string(), "Текст".to_string()),
        );
        let en_en = platform_property_query(
            &db,
            MethodLookupInput::new(&db, "Query".to_string(), "Text".to_string()),
        );
        let en_ru = platform_property_query(
            &db,
            MethodLookupInput::new(&db, "Query".to_string(), "Текст".to_string()),
        );
        let ru_en = platform_property_query(
            &db,
            MethodLookupInput::new(&db, "Запрос".to_string(), "Text".to_string()),
        );

        for got in [&ru_ru, &en_en, &en_ru, &ru_en] {
            let prop = got.as_ref().expect("Query.Text property must exist in platform data");
            assert_eq!(prop.name.as_str(), "Текст");
            assert_eq!(prop.english_name.as_str(), "Text");
            assert_eq!(prop.property_types, vec![smol_str::SmolStr::new("Строка")]);
            assert!(!prop.is_readonly, "Текст is read-write");
        }
    }

    #[test]
    fn test_platform_property_query_readonly_union() {
        let db = TestDatabase::default();
        let data = PlatformDataInner::instance();
        if data.all_properties().is_empty() {
            println!("Skipping test: no property data");
            return;
        }

        let got = platform_property_query(
            &db,
            MethodLookupInput::new(&db, "Запрос".to_string(), "Параметры".to_string()),
        );
        let prop = got.expect("Query.Parameters property must exist");
        assert!(prop.is_readonly, "Параметры is read-only");
        assert_eq!(prop.property_types, vec![smol_str::SmolStr::new("Структура")]);

        let got = platform_property_query(
            &db,
            MethodLookupInput::new(
                &db,
                "Запрос".to_string(),
                "МенеджерВременныхТаблиц".to_string(),
            ),
        );
        let prop = got.expect("Query.TempTablesManager property must exist");
        assert!(!prop.is_readonly, "МенеджерВременныхТаблиц is read-write");
        assert_eq!(prop.property_types.len(), 2, "union property_types");
    }

    #[test]
    fn test_type_properties_query_lists_all_members() {
        let db = TestDatabase::default();
        let data = PlatformDataInner::instance();
        if data.all_properties().is_empty() {
            println!("Skipping test: no property data");
            return;
        }

        let props = type_properties_query(&db, TypeNameInput::new(&db, "Запрос".to_string()));
        let names: Vec<&str> = props.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Текст"));
        assert!(names.contains(&"Параметры"));
        assert!(names.contains(&"МенеджерВременныхТаблиц"));
    }

    #[test]
    fn test_platform_property_query_unknown_returns_none() {
        let db = TestDatabase::default();
        let got = platform_property_query(
            &db,
            MethodLookupInput::new(
                &db,
                "Запрос".to_string(),
                "ЗаведомоНесуществующееСвойство".to_string(),
            ),
        );
        assert!(got.is_none());
    }

    #[test]
    fn test_global_property_lookup_bilingual() {
        let data = PlatformDataInner::instance();
        if data.all_global_properties().is_empty() {
            println!("Skipping test: no global properties available");
            return;
        }

        let ru = data.get_global_property("ОбработкаОшибок").expect("ru name must resolve");
        let en = data.get_global_property("ErrorProcessing").expect("en name must resolve");
        assert_eq!(ru.id, en.id);
        assert_eq!(ru.property_types, vec![smol_str::SmolStr::new("МенеджерОбработкиОшибок")]);
        assert!(data.get_global_property("ОБРАБОТКАОШИБОК").is_some());
        assert!(data.get_global_property("errorprocessing").is_some());
    }

    #[test]
    fn test_global_member_resolution_bilingual() {
        let data = PlatformDataInner::instance();
        if data.all_global_properties().is_empty() {
            println!("Skipping test: no global properties available");
            return;
        }

        let m = data
            .resolve_global_member("ОбработкаОшибок", "КраткоеПредставлениеОшибки")
            .expect("ru.ru must resolve");
        assert_eq!(m.name.as_str(), "КраткоеПредставлениеОшибки");
        assert_eq!(m.english_name.as_str(), "BriefErrorDescription");

        let m_en = data
            .resolve_global_member("ErrorProcessing", "BriefErrorDescription")
            .expect("en.en must resolve");
        assert_eq!(m_en.id, m.id);

        assert!(data.resolve_global_member("ОбработкаОшибок", "ЗаведомоНеТакогоМетода").is_none());
        assert!(data.resolve_global_member("ЗаведомоНеТакогоГлобала", "X").is_none());
    }

    #[test]
    fn test_global_property_query_salsa() {
        let db = TestDatabase::default();
        let data = PlatformDataInner::instance();
        if data.all_global_properties().is_empty() {
            println!("Skipping test: no global properties available");
            return;
        }

        let got =
            global_property_query(&db, TypeNameInput::new(&db, "ОбработкаОшибок".to_string()))
                .expect("global must resolve");
        assert_eq!(got.english_name.as_str(), "ErrorProcessing");

        let got_member = global_member_method_query(
            &db,
            MethodLookupInput::new(
                &db,
                "ОбработкаОшибок".to_string(),
                "КраткоеПредставлениеОшибки".to_string(),
            ),
        )
        .expect("member must resolve");
        assert_eq!(got_member.english_name.as_str(), "BriefErrorDescription");
    }

    #[test]
    fn test_constructor_docs_by_id() {
        let data = PlatformDataInner::instance();
        if data.all_constructors().is_empty() {
            println!("Skipping test: no constructor data");
            return;
        }
        let ctor_with_docs =
            data.all_constructors().iter().find(|c| data.get_constructor_docs(c.id).is_some());
        if let Some(ctor) = ctor_with_docs {
            let docs = data.get_constructor_docs(ctor.id).unwrap();
            assert_eq!(docs.constructor_id, ctor.id);
        }
    }

    #[test]
    fn find_prefixed_method_resolves_information_register_record_set_read() {
        let data = PlatformDataInner::instance();
        if data.get_manager_methods("InformationRegisterRecordSet").is_empty() {
            println!("Skipping test: no platform data available");
            return;
        }
        let m = find_prefixed_method("InformationRegisterRecordSet", "Прочитать")
            .expect("Прочитать must resolve under InformationRegisterRecordSet");
        assert!(
            m.english_name.fold_lower().ends_with(".read"),
            "english_name must end with `.Read`, got `{}`",
            m.english_name
        );
        assert_ne!(m.id, 0);

        let m_en = find_prefixed_method("InformationRegisterRecordSet", "Read")
            .expect("Read must resolve bilingually");
        assert_eq!(m.id, m_en.id);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn prefixed_methods_resolve_bilingual_case_insensitively_in_stable_id_order() {
        let ru = find_prefixed_methods("InformationRegisterManager", "Выбрать");
        let en = find_prefixed_methods("InformationRegisterManager", "Select");
        let mixed = find_prefixed_methods("InformationRegisterManager", "sElEcT");

        let ru_ids: Vec<u32> = ru.iter().map(|method| method.id).collect();
        let en_ids: Vec<u32> = en.iter().map(|method| method.id).collect();
        let mixed_ids: Vec<u32> = mixed.iter().map(|method| method.id).collect();

        assert_eq!(ru_ids, vec![174]);
        assert_eq!(ru_ids, en_ids);
        assert_eq!(ru_ids, mixed_ids);
        assert!(ru_ids.windows(2).all(|ids| ids[0] < ids[1]));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn prefixed_methods_deduplicate_ids_and_sort_stably() {
        let information = find_prefixed_methods("InformationRegisterManager", "Select");
        let accumulation = find_prefixed_methods("AccumulationRegisterManager", "Select");

        let deduplicated = sort_and_deduplicate_methods(vec![
            accumulation[0].clone(),
            information[0].clone(),
            information[0].clone(),
        ]);
        let ids: Vec<u32> = deduplicated.iter().map(|method| method.id).collect();

        assert_eq!(ids, vec![174, 525]);
    }

    #[test]
    fn prefixed_methods_unknown_name_is_empty() {
        assert!(
            find_prefixed_methods("InformationRegisterManager", "NotAPlatformMethod").is_empty()
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn prefixed_methods_do_not_cross_manager_families() {
        assert_eq!(
            find_prefixed_methods("InformationRegisterManager", "Select")
                .iter()
                .map(|method| method.id)
                .collect::<Vec<_>>(),
            vec![174]
        );
        assert!(find_prefixed_methods("InformationRegisterRecordSet", "Select").is_empty());
    }

    #[test]
    fn prefixed_method_query_caches_through_salsa() {
        let db = TestDatabase::default();
        let data = PlatformDataInner::instance();
        if data.get_manager_methods("InformationRegisterRecordSet").is_empty() {
            println!("Skipping test: no platform data available");
            return;
        }

        let input1 = PrefixedMethodLookupInput::new(
            &db,
            "InformationRegisterRecordSet".to_string(),
            "Прочитать".to_string(),
        );
        let input2 = PrefixedMethodLookupInput::new(
            &db,
            "InformationRegisterRecordSet".to_string(),
            "Прочитать".to_string(),
        );
        assert_eq!(input1, input2);

        let m1 = prefixed_method_query(&db, input1).expect("must resolve");
        let m2 = prefixed_method_query(&db, input2).expect("must resolve");
        assert_eq!(m1.id, m2.id);
    }
}
