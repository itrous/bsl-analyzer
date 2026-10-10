use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformType {
    pub name: SmolStr,
    pub english_name: SmolStr,
    pub min_version: Option<SmolStr>,
    pub context: Option<ContextAvailability>,
    pub iter_element_types: Vec<SmolStr>,
    /// XDTO type name declared by the type's help page, when present. Some
    /// configuration XML serializes attribute types by this name rather than
    /// the class name (e.g. `ГрафическаяСхема` ↔ `FlowchartContextType`).
    pub xdto_name: Option<SmolStr>,
}

impl PlatformType {
    /// Heap bytes owned by this type, memoised by `bsl-platform`'s
    /// `platform_type_query` for Salsa's `heap_size` hook: its name/version/XDTO
    /// `SmolStr`s (spilled ones only) plus the element-type vec. `context` is
    /// `Copy` and owns no heap. New heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        stdx::heap::smol_str_bytes(self.name.len())
            + stdx::heap::smol_str_bytes(self.english_name.len())
            + self.min_version.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
            + self.xdto_name.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
            + stdx::heap::vec_bytes::<SmolStr>(self.iter_element_types.len())
            + self
                .iter_element_types
                .iter()
                .map(|s| stdx::heap::smol_str_bytes(s.len()))
                .sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformMethod {
    pub id: u32,
    pub type_name: SmolStr,
    pub name: SmolStr,
    pub english_name: SmolStr,
    pub return_type: Option<SmolStr>,
    pub parameters: Vec<MethodParam>,
    pub variants: Vec<MethodVariant>,
    pub min_version: Option<SmolStr>,
    pub context: Option<ContextAvailability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodVariant {
    pub variant_name: Option<SmolStr>,
    pub parameters: Vec<MethodParam>,
}

impl PlatformMethod {
    /// Heap bytes owned by this method, memoised by `bsl-platform`'s
    /// `platform_method_query`/`type_methods_query`/`manager_methods_query`/
    /// `prefixed_method_query`/`global_member_method_query` for Salsa's
    /// `heap_size` hook: its name/version `SmolStr`s plus the parameter and
    /// overload-variant vecs. `id`/`context` are `Copy` and own no heap. New
    /// heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        stdx::heap::smol_str_bytes(self.type_name.len())
            + stdx::heap::smol_str_bytes(self.name.len())
            + stdx::heap::smol_str_bytes(self.english_name.len())
            + self.return_type.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
            + stdx::heap::vec_bytes::<MethodParam>(self.parameters.len())
            + self.parameters.iter().map(MethodParam::estimated_heap_size).sum::<usize>()
            + stdx::heap::vec_bytes::<MethodVariant>(self.variants.len())
            + self.variants.iter().map(MethodVariant::estimated_heap_size).sum::<usize>()
            + self.min_version.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
    }
}

impl MethodVariant {
    /// Heap bytes owned by this overload variant: its name plus the parameter
    /// vec and each parameter's own owned payload.
    pub fn estimated_heap_size(&self) -> usize {
        self.variant_name.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
            + stdx::heap::vec_bytes::<MethodParam>(self.parameters.len())
            + self.parameters.iter().map(MethodParam::estimated_heap_size).sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalFunction {
    pub id: u32,
    pub name: SmolStr,
    pub english_name: SmolStr,
    pub return_type: Option<SmolStr>,
    pub parameters: Vec<MethodParam>,
    pub variants: Vec<GlobalFunctionVariant>,
    pub min_version: Option<SmolStr>,
    pub context: Option<ContextAvailability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalFunctionVariant {
    pub variant_name: Option<SmolStr>,
    pub parameters: Vec<MethodParam>,
}

impl GlobalFunction {
    /// Heap bytes owned by this global function, memoised by `bsl-platform`'s
    /// `global_function_query` for Salsa's `heap_size` hook: its name/version
    /// `SmolStr`s plus the parameter and overload-variant vecs. New
    /// heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        stdx::heap::smol_str_bytes(self.name.len())
            + stdx::heap::smol_str_bytes(self.english_name.len())
            + self.return_type.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
            + stdx::heap::vec_bytes::<MethodParam>(self.parameters.len())
            + self.parameters.iter().map(MethodParam::estimated_heap_size).sum::<usize>()
            + stdx::heap::vec_bytes::<GlobalFunctionVariant>(self.variants.len())
            + self.variants.iter().map(GlobalFunctionVariant::estimated_heap_size).sum::<usize>()
            + self.min_version.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
    }
}

impl GlobalFunctionVariant {
    /// Heap bytes owned by this overload variant: its name plus the parameter
    /// vec and each parameter's own owned payload.
    pub fn estimated_heap_size(&self) -> usize {
        self.variant_name.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
            + stdx::heap::vec_bytes::<MethodParam>(self.parameters.len())
            + self.parameters.iter().map(MethodParam::estimated_heap_size).sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformConstructor {
    pub id: u32,
    pub type_name: SmolStr,
    pub variant_name: Option<SmolStr>,
    pub parameters: Vec<MethodParam>,
    pub min_version: Option<SmolStr>,
    pub context: Option<ContextAvailability>,
}

impl PlatformConstructor {
    /// Heap bytes owned by this constructor overload, memoised by
    /// `bsl-platform`'s `platform_constructors_query` for Salsa's `heap_size`
    /// hook: its name/version `SmolStr`s plus the parameter vec. New
    /// heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        stdx::heap::smol_str_bytes(self.type_name.len())
            + self.variant_name.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
            + stdx::heap::vec_bytes::<MethodParam>(self.parameters.len())
            + self.parameters.iter().map(MethodParam::estimated_heap_size).sum::<usize>()
            + self.min_version.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformProperty {
    pub id: u32,
    pub type_name: SmolStr,
    pub name: SmolStr,
    pub english_name: SmolStr,
    pub property_types: Vec<SmolStr>,
    pub is_readonly: bool,
    pub min_version: Option<SmolStr>,
    pub context: Option<ContextAvailability>,
}

impl PlatformProperty {
    /// Heap bytes owned by this property, memoised by `bsl-platform`'s
    /// `platform_property_query`/`type_properties_query`/`global_property_query`
    /// for Salsa's `heap_size` hook: its name/version `SmolStr`s plus the
    /// property-type vec. `id`/`is_readonly`/`context` are `Copy` and own no
    /// heap. New heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        stdx::heap::smol_str_bytes(self.type_name.len())
            + stdx::heap::smol_str_bytes(self.name.len())
            + stdx::heap::smol_str_bytes(self.english_name.len())
            + stdx::heap::vec_bytes::<SmolStr>(self.property_types.len())
            + self.property_types.iter().map(|s| stdx::heap::smol_str_bytes(s.len())).sum::<usize>()
            + self.min_version.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodParam {
    pub name: SmolStr,
    pub param_type: Option<SmolStr>,
    pub is_optional: bool,
    pub is_variadic: bool,
}

impl MethodParam {
    /// Heap bytes owned by this parameter: its name/type `SmolStr`s (spilled
    /// ones only). `is_optional`/`is_variadic` are `Copy` and own no heap.
    pub fn estimated_heap_size(&self) -> usize {
        stdx::heap::smol_str_bytes(self.name.len())
            + self.param_type.as_ref().map_or(0, |s| stdx::heap::smol_str_bytes(s.len()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextAvailability {
    pub thick_client: bool,
    pub thin_client: bool,
    pub web_client: bool,
    pub server: bool,
    pub mobile_client: bool,
    pub external_connection: bool,
}

impl ContextAvailability {
    /// Availability of an entry the syntax helper leaves unmarked. The helper writes a
    /// "Доступность" list only where the platform restricts an entry, so a missing list means
    /// "everywhere" — the opposite of an entry marked available in no context at all.
    pub const UNRESTRICTED: Self = Self {
        thick_client: true,
        thin_client: true,
        web_client: true,
        server: true,
        mobile_client: true,
        external_connection: true,
    };

    /// Availability of an entry whose markup may be absent, with [`Self::UNRESTRICTED`]
    /// standing in for the missing one. Every consumer must read a missing markup the same
    /// way, so the rule lives here rather than in each of them.
    pub fn effective(context: Option<&Self>) -> Self {
        context.copied().unwrap_or(Self::UNRESTRICTED)
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawPlatformGlobalKind {
    Function,
    Property,
    SystemEnum,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct RawPlatformGlobalSymbol {
    pub canonical_ru: &'static str,
    pub canonical_en: &'static str,
    pub kind: RawPlatformGlobalKind,
    /// Bit layout is attested in `data/global_catalog.json`.
    pub environment_mask: u8,
    pub writable: bool,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct RawPlatformGlobalCatalogMetadata {
    pub schema_version: u32,
    pub platform_version: &'static str,
    pub edt_version: &'static str,
    pub global_context_sha256: &'static str,
    pub system_enums_sha256: &'static str,
    pub complete_global_context: bool,
    pub complete_system_enums: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDocs {
    pub method_id: u32,
    pub syntax: String,
    pub description: String,
    pub params: Vec<ParamDocs>,
    pub examples: Vec<CodeExample>,
    pub notes: Option<String>,
    pub see_also: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDocs {
    pub name: SmolStr,
    pub description: String,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeExample {
    pub code: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstructorDocs {
    pub constructor_id: u32,
    pub syntax: String,
    pub description: String,
    pub params: Vec<ParamDocs>,
    pub examples: Vec<CodeExample>,
    pub notes: Option<String>,
    pub see_also: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyDocs {
    pub property_id: u32,
    pub description: String,
    pub notes: Option<String>,
    pub see_also: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct KeywordDocs {
    pub keyword_ru: SmolStr,
    pub keyword_en: SmolStr,
    pub syntax: String,
    pub description: String,
    pub params: Vec<ParamDocs>,
    pub min_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_method_heap_counts_params_and_spilled_strings() {
        let long_type_name = "ПользовательскийТипСДлиннымИменем";
        let method = PlatformMethod {
            id: 1,
            type_name: SmolStr::new(long_type_name),
            name: SmolStr::new("Метод"),
            english_name: SmolStr::new("Method"),
            return_type: Some(SmolStr::new(long_type_name)),
            parameters: vec![
                MethodParam {
                    name: SmolStr::new("ПараметрСДлиннымИменемБезИнлайна"),
                    param_type: Some(SmolStr::new(long_type_name)),
                    is_optional: false,
                    is_variadic: false,
                },
                MethodParam {
                    name: SmolStr::new("Второй"),
                    param_type: None,
                    is_optional: true,
                    is_variadic: false,
                },
            ],
            variants: vec![],
            min_version: None,
            context: None,
        };

        let long_bytes = long_type_name.len();
        let bytes = method.estimated_heap_size();
        // At least the two spilled-`SmolStr` occurrences that appear verbatim
        // (`type_name`/`return_type`); well under a kilobyte for two params.
        assert!(bytes > long_bytes * 2);
        assert!(bytes < 1024);
    }
}
