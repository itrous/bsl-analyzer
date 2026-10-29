use std::sync::Arc;
use stdx::case::CaseExt;

use bsl_metadata::QueryMetadataResolver;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::hir::{FieldDef, Name, TableRef};
use crate::types::SdblType;

/// Where a [`Scope`] gets its metadata answers.
///
/// `Config` owns an `Arc<Configuration>` so a scope built for completion can be
/// returned by value (no borrow escapes). `Resolver` borrows a db-backed
/// per-MDO resolver used during query lowering or completion; the caller keeps
/// the resolver alive while using the scope.
#[derive(Debug)]
enum MetaSource<'a> {
    Config(Arc<bsl_metadata::Configuration>),
    Resolver(&'a dyn QueryMetadataResolver),
}

impl MetaSource<'_> {
    fn resolver(&self) -> &dyn QueryMetadataResolver {
        match self {
            MetaSource::Config(config) => &**config,
            MetaSource::Resolver(resolver) => *resolver,
        }
    }
}

#[derive(Debug)]
pub struct Scope<'a> {
    frames: Vec<ScopeFrame>,

    metadata: Option<MetaSource<'a>>,
}

impl Default for Scope<'_> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct ScopeFrame {
    tables: FxHashMap<String, TableRef>,

    temp_tables: FxHashMap<String, TempTableDef>,

    /// A bare column name that this frame does not offer is not looked up further out.
    opaque: bool,

    /// The frame holds the select list's outputs: a name it offers is that output, whatever
    /// the output's type.
    outputs: bool,
}

#[derive(Debug, Clone)]
pub struct TempTableDef {
    pub name: String,

    pub fields: Vec<FieldDef>,
}

impl<'a> Scope<'a> {
    pub fn new() -> Self {
        Self { frames: vec![ScopeFrame::default()], metadata: None }
    }

    pub fn new_with_metadata(metadata: Option<Arc<bsl_metadata::Configuration>>) -> Self {
        Self { frames: vec![ScopeFrame::default()], metadata: metadata.map(MetaSource::Config) }
    }

    pub fn new_with_resolver(resolver: &'a dyn QueryMetadataResolver) -> Self {
        Self { frames: vec![ScopeFrame::default()], metadata: Some(MetaSource::Resolver(resolver)) }
    }

    pub fn push_frame(&mut self) {
        self.frames.push(ScopeFrame::default());
    }

    /// A frame whose bare column names never reach the enclosing query: the parameter list of
    /// a virtual table, where a condition can only speak of the table's own fields. Without the
    /// barrier, `Счет` in the condition of the second balance table of a join resolved against
    /// the first one and was reported ambiguous.
    pub fn push_opaque_frame(&mut self) {
        self.frames.push(ScopeFrame { opaque: true, ..ScopeFrame::default() });
    }

    /// A frame for the outputs of a select list, read by `УПОРЯДОЧИТЬ ПО` and `ИТОГИ`. An output
    /// of unknown type (a parameter, an expression the lowering does not type) still owns its
    /// name; looking past it reached the sources and reported an ambiguity the platform does
    /// not see.
    pub fn push_outputs_frame(&mut self, opaque: bool) {
        self.frames.push(ScopeFrame { opaque, outputs: true, ..ScopeFrame::default() });
    }

    pub fn pop_frame(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }

    /// Registers a source under its effective name, reporting whether that name was already
    /// taken.
    ///
    /// The answer matters beyond the diagnostic: a second source under the same name evicts
    /// the first from the scope, so every later reference to it silently resolves against the
    /// wrong table. Callers that drop this result are choosing to let that happen quietly.
    #[must_use = "a colliding alias silently evicts the source already registered under it"]
    pub fn add_table(&mut self, table: TableRef) -> Option<String> {
        let frame = self.frames.last_mut()?;
        let name = table.effective_name().to_string();
        let key = name.fold_lower();
        frame.tables.insert(key, table).map(|_| name)
    }

    pub fn add_temp_table(&mut self, name: String, fields: Vec<FieldDef>) {
        if let Some(frame) = self.frames.last_mut() {
            let key = name.fold_lower();
            tracing::debug!(name = %name, fields = fields.len(), "Adding temporary table to scope");
            frame.temp_tables.insert(key, TempTableDef { name, fields });
        }
    }

    pub fn remove_temp_table(&mut self, name: &str) {
        let name_lower = name.fold_lower();
        for frame in self.frames.iter_mut().rev() {
            if frame.temp_tables.remove(&name_lower).is_some() {
                tracing::debug!(name = %name, "Removed temporary table from scope");
                return;
            }
        }
    }

    pub fn find_temp_table(&self, name: &str) -> Option<&TempTableDef> {
        let name_lower = name.fold_lower();
        for frame in self.frames.iter().rev() {
            if let Some(temp_table) = frame.temp_tables.get(&name_lower) {
                tracing::debug!(name = %name, "Found temporary table in scope");
                return Some(temp_table);
            }
        }
        None
    }

    pub fn find_table(&self, name: &str) -> Option<&TableRef> {
        let name_lower = name.fold_lower();
        for frame in self.frames.iter().rev() {
            if let Some(table) = frame.tables.get(&name_lower) {
                return Some(table);
            }
        }
        None
    }

    pub fn current_tables(&self) -> impl Iterator<Item = &TableRef> {
        self.frames.last().into_iter().flat_map(|f| f.tables.values())
    }

    pub fn all_tables(&self) -> impl Iterator<Item = &TableRef> {
        self.frames.iter().flat_map(|f| f.tables.values())
    }

    /// Whether a real source has metadata. The select-list outputs frame is not a source: it
    /// holds a synthetic table so `УПОРЯДОЧИТЬ ПО` and `ИТОГИ` can name a result column, and
    /// counting that table would judge a bare name even when no source has a field set.
    pub(crate) fn source_has_metadata(&self, alias: Option<&str>) -> bool {
        let sources = self.frames.iter().filter(|frame| !frame.outputs);
        match alias {
            Some(alias) => {
                let key = alias.fold_lower();
                sources
                    .rev()
                    .find_map(|frame| frame.tables.get(&key))
                    .is_some_and(|table| table.metadata.is_some())
            }
            None => sources
                .flat_map(|frame| frame.tables.values())
                .any(|table| table.metadata.is_some()),
        }
    }

    pub fn resolve_column_type(&self, table_alias: Option<&str>, column_name: &str) -> SdblType {
        if let Some(alias) = table_alias {
            if let Some(table) = self.find_table(alias) {
                return self.find_column_type_in_table(table, column_name);
            }
            return SdblType::Unknown;
        }

        // Resolution walks frames from the innermost outward and STOPS at the first frame that
        // offers the name. A subquery is its own scope: a name it resolves on its own sources
        // shadows the same name outside, exactly as it does in SQL, and only the sources of one
        // level can compete for it. Searching every live frame at once made a self-contained
        // subquery collide with its enclosing query over names like `Код` or `Ссылка` — the
        // commonest names there are.
        //
        // Provisional fields are skipped while COUNTING sources, not while typing: a field the
        // platform may not expose cannot be one of the two occurrences that make a bare name
        // ambiguous. A field that is offered still counts when its type is unknown: the type
        // lookup answers `Unknown` both for "not in this table" and for "offered, type not
        // known" (`Субконто<N>`), and treating the second as the first hid the collision.
        for frame in self.frames.iter().rev() {
            if frame.outputs {
                if let Some(field) = Self::output_named(frame, column_name) {
                    // An output that is itself ambiguous was reported where it is written.
                    return if field.ty == SdblType::Error { SdblType::Unknown } else { field.ty };
                }
            }

            let mut found_type: Option<SdblType> = None;
            let mut provisional_type: Option<SdblType> = None;

            for table in frame.tables.values() {
                let Some(resolved) = table.metadata.as_ref() else {
                    continue;
                };
                let Some(field) = resolved.find_field(column_name) else {
                    continue;
                };
                // An error type is not an occurrence: the name is already broken, and counting
                // it would report the same reference ambiguous a second time.
                if field.ty == SdblType::Error {
                    continue;
                }
                let ty = field.ty.clone();
                if resolved.column_is_provisional(column_name) {
                    provisional_type.get_or_insert(ty);
                    continue;
                }
                if found_type.is_some() {
                    return SdblType::Error;
                }
                found_type = Some(ty);
            }

            if let Some(ty) = found_type.or(provisional_type) {
                return ty;
            }
            if frame.opaque {
                break;
            }
        }

        SdblType::Unknown
    }

    fn output_named(frame: &ScopeFrame, column_name: &str) -> Option<FieldDef> {
        frame
            .tables
            .values()
            .filter_map(|table| table.metadata.as_ref())
            .find_map(|resolved| resolved.fields().iter().find(|f| f.matches_name(column_name)))
            .cloned()
    }

    fn find_column_type_in_table(&self, table: &TableRef, column_name: &str) -> SdblType {
        if let Some(ref resolved) = table.metadata {
            if let Some(field) = resolved.find_field(column_name) {
                return field.ty.clone();
            }
        }
        SdblType::Unknown
    }

    /// The sources that make `column_name` ambiguous, from the frame that owns the name.
    ///
    /// Mirrors [`Self::resolve_column_type`]: naming a table from an enclosing query would
    /// point the reader at a source the reference could not have meant.
    pub fn find_tables_with_column(&self, column_name: &str) -> Vec<String> {
        for frame in self.frames.iter().rev() {
            if frame.outputs && Self::output_named(frame, column_name).is_some() {
                return Vec::new();
            }
            let mut result = Vec::new();
            for table in frame.tables.values() {
                let Some(resolved) = table.metadata.as_ref() else { continue };
                // Same rule as the ambiguity decision itself: a provisional field is not an
                // occurrence, so naming its table as a candidate would mislead the reader.
                let offered = resolved.fields().iter().any(|f| f.matches_name(column_name));
                if offered && !resolved.column_is_provisional(column_name) {
                    result.push(table.effective_name().to_string());
                }
            }
            if !result.is_empty() {
                result.sort();
                return result;
            }
            if frame.opaque {
                break;
            }
        }

        Vec::new()
    }

    /// The sources that also offer `head` as a field, when `head` is an alias at their level.
    ///
    /// The platform rejects `Имя.Поле` outright when `Имя` reads both ways, and it decides that
    /// per query level: the same collision inside a subquery is accepted (verified against
    /// 8.3.27). The field's type is irrelevant — a String attribute collides just as a tabular
    /// section does — so this is a name question, settled before any type is known.
    ///
    /// Provisional fields are not occurrences here for the same reason as everywhere else: a
    /// field the platform may not create cannot be the half of a collision it never sees.
    pub fn qualified_head_collision(&self, head: &str) -> Vec<String> {
        let head_lower = head.fold_lower();
        for frame in self.frames.iter().rev() {
            if !frame.tables.contains_key(&head_lower) {
                continue;
            }
            let mut offered: Vec<String> = frame
                .tables
                .values()
                .filter(|table| {
                    table.metadata.as_ref().is_some_and(|resolved| {
                        resolved.fields().iter().any(|f| f.matches_name(head))
                            && !resolved.column_is_provisional(head)
                    })
                })
                .map(|table| table.effective_name().to_string())
                .collect();
            offered.sort();
            return offered;
        }
        Vec::new()
    }

    pub fn find_field_def(
        &self,
        table_alias: Option<&str>,
        column_name: &str,
    ) -> Option<&FieldDef> {
        if let Some(alias) = table_alias {
            if let Some(table) = self.find_table(alias) {
                if let Some(ref resolved) = table.metadata {
                    return resolved.find_field(column_name);
                }
            }
            return None;
        }

        for table in self.all_tables() {
            if let Some(ref resolved) = table.metadata {
                if let Some(field) = resolved.find_field(column_name) {
                    return Some(field);
                }
            }
        }

        None
    }

    pub fn is_table_alias(&self, name: &str) -> bool {
        self.find_table(name).is_some()
    }

    pub fn column_completions(&self, table_alias: Option<&str>) -> Vec<ColumnCompletion> {
        let mut result = Vec::new();

        let tables: Vec<&TableRef> = if let Some(alias) = table_alias {
            self.find_table(alias).into_iter().collect()
        } else {
            self.all_tables().collect()
        };

        for table in tables {
            let table_name = &table.full_name;

            tracing::info!(
                full_name = %table_name,
                alias = ?table.alias,
                has_metadata = table.metadata.is_some(),
                fields_count = table.metadata.as_ref().map(|m| m.fields().len()).unwrap_or(0),
                "column_completions: Processing table"
            );

            if let Some(ref resolved) = table.metadata {
                tracing::info!(
                    full_name = %table_name,
                    total_fields = resolved.fields().len(),
                    field_names = ?resolved.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
                    "column_completions: Fields in metadata"
                );

                for field in resolved.fields() {
                    result.push(ColumnCompletion {
                        column_name: Name::from(field.name.as_str()),
                        table_name: Name::from(table_name.as_str()),
                        ty: field.ty.clone(),
                        is_standard: field.is_standard,
                    });
                }
            } else {
                tracing::debug!(
                    full_name = %table_name,
                    "column_completions: Table has no resolved fields (may be from extension)"
                );
            }
        }

        result
    }

    pub fn resolve_nested_field_type(&self, table_alias: &str, field_chain: &[String]) -> SdblType {
        const MAX_DEPTH: usize = 10;

        if field_chain.len() > MAX_DEPTH {
            tracing::warn!(
                depth = field_chain.len(),
                "exceeded max nesting depth, possible circular reference"
            );
            return SdblType::Error;
        }

        let Some(table) = self.find_table(table_alias) else {
            tracing::warn!(alias = %table_alias, "table not found in scope");
            return SdblType::Unknown;
        };

        tracing::info!(alias = %table_alias, table_name = %table.full_name, "found table");

        let Some(metadata) = &table.metadata else {
            tracing::warn!(alias = %table_alias, "table has no metadata");
            return SdblType::Unknown;
        };

        let mut current_fields = metadata.fields().to_vec();
        let mut current_type = SdblType::Unknown;

        tracing::info!(
            alias = %table_alias,
            initial_fields_count = current_fields.len(),
            "starting field resolution"
        );

        for (i, field_name) in field_chain.iter().enumerate() {
            tracing::info!(
                step = i + 1,
                field = %field_name,
                available_fields = current_fields.len(),
                "resolving nested field"
            );

            let Some(field) = current_fields.iter().find(|f| f.matches_name(field_name)) else {
                tracing::warn!(
                    field = %field_name,
                    available_fields = ?current_fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
                    "field not found"
                );
                return SdblType::Unknown;
            };

            tracing::info!(
                field = %field_name,
                field_type = ?field.ty,
                "found field"
            );

            current_type = field.ty.clone();

            // The navigation `match` below only prepares `current_fields` for the
            // next hop. On the terminal field there is no next hop: return its
            // type directly so primitive attributes (Дата, Строка, …) surface
            // instead of collapsing to Unknown via the non-navigable `_` arm.
            if i + 1 == field_chain.len() {
                return current_type;
            }

            match &current_type {
                SdblType::Ref(mdo_ref) => {
                    tracing::info!(
                        mdo_ref = ?mdo_ref,
                        "following reference"
                    );
                    match self.resolve_ref_fields(mdo_ref) {
                        Some(fields) => {
                            tracing::info!(
                                fields_count = fields.len(),
                                "resolved reference fields"
                            );
                            current_fields = fields;
                        }
                        None => {
                            tracing::warn!(
                                mdo_ref = ?mdo_ref,
                                has_metadata = self.metadata.is_some(),
                                "failed to resolve reference fields"
                            );
                            return SdblType::Unknown;
                        }
                    }
                }
                SdblType::Composite { types } => match self.resolve_composite_fields(types) {
                    Some(fields) => current_fields = fields,
                    None => {
                        tracing::debug!("failed to resolve composite fields");
                        return SdblType::Unknown;
                    }
                },
                SdblType::DefinedType { name, underlying_type } => {
                    match self.resolve_defined_type_fields(name, underlying_type) {
                        Some(fields) => current_fields = fields,
                        None => {
                            tracing::debug!(
                                defined_type = %name,
                                "failed to resolve DefinedType fields"
                            );
                            return SdblType::Unknown;
                        }
                    }
                }
                SdblType::TabularSectionRef { parent_mdo_type, parent_mdo_name, ts_name } => {
                    tracing::info!(
                        parent = %parent_mdo_name,
                        ts_name = %ts_name,
                        "following tabular section reference"
                    );
                    match self.resolve_tabular_section_fields(
                        *parent_mdo_type,
                        parent_mdo_name,
                        ts_name,
                    ) {
                        Some(fields) => {
                            tracing::info!(
                                fields_count = fields.len(),
                                "resolved tabular section fields"
                            );
                            current_fields = fields;
                        }
                        None => {
                            tracing::warn!(
                                parent = %parent_mdo_name,
                                ts_name = %ts_name,
                                "failed to resolve tabular section fields"
                            );
                            return SdblType::Unknown;
                        }
                    }
                }
                SdblType::AnyRef | SdblType::AnyObjectRef { .. } => {
                    tracing::debug!(
                        ty = ?current_type,
                        "reached AnyRef/AnyObjectRef, cannot traverse further"
                    );
                    return SdblType::Unknown;
                }
                _ => {
                    tracing::debug!(
                        field = %field_name,
                        ty = ?current_type,
                        "reached primitive type, cannot traverse further"
                    );
                    return SdblType::Unknown;
                }
            }
        }

        current_type
    }

    fn resolve_ref_fields(&self, mdo_ref: &crate::types::MdoRef) -> Option<Vec<FieldDef>> {
        let resolver = self.metadata.as_ref()?.resolver();

        let mdo_object = resolver.resolve_metadata_object(mdo_ref.mdo_type, &mdo_ref.name)?;

        let mut fields = Vec::new();

        for attr in &mdo_object.attributes {
            let is_standard = bsl_metadata::is_standard_attribute_name(&attr.name);
            fields.push(FieldDef::new_with_names(
                attr.name.clone(),
                attr.name_en.clone(),
                SdblType::from_attribute_type(&attr.attr_type),
                is_standard,
            ));
        }

        for common in &mdo_object.common_attributes {
            fields.push(FieldDef::new_with_names(
                common.name.clone(),
                None,
                SdblType::from_attribute_type(&common.attr_type),
                false,
            ));
        }

        for ts in &mdo_object.tabular_sections {
            fields.push(FieldDef::new_with_names(
                ts.name().to_string(),
                ts.name_en().map(|s| s.to_string()),
                SdblType::TabularSectionRef {
                    parent_mdo_type: mdo_ref.mdo_type,
                    parent_mdo_name: mdo_ref.name.clone(),
                    ts_name: ts.name().to_string(),
                },
                false,
            ));
        }

        Some(fields)
    }

    fn resolve_tabular_section_fields(
        &self,
        parent_mdo_type: bsl_metadata::MdoType,
        parent_mdo_name: &str,
        ts_name: &str,
    ) -> Option<Vec<FieldDef>> {
        let resolver = self.metadata.as_ref()?.resolver();

        let mdo_object = resolver.resolve_metadata_object(parent_mdo_type, parent_mdo_name)?;

        let ts = mdo_object.find_tabular_section(ts_name)?;

        let mut fields = Vec::new();

        fields.push(FieldDef::standard(
            "Ссылка",
            "Ref",
            SdblType::reference(parent_mdo_type, parent_mdo_name),
        ));

        fields.push(FieldDef::standard("НомерСтроки", "LineNumber", SdblType::number()));

        for attr in ts.attributes() {
            fields.push(FieldDef::new_with_names(
                attr.name().to_string(),
                attr.name_en().map(|s| s.to_string()),
                SdblType::from_attribute_type(attr.attr_type()),
                false,
            ));
        }

        Some(fields)
    }

    fn resolve_composite_fields(&self, types: &[SdblType]) -> Option<Vec<FieldDef>> {
        self.resolve_composite_fields_in(types, &mut FxHashSet::default())
    }

    /// `resolving` holds the defined types being looked up through the resolver on the way
    /// here, so a defined type that contains itself ends the walk instead of looping.
    fn resolve_composite_fields_in(
        &self,
        types: &[SdblType],
        resolving: &mut FxHashSet<String>,
    ) -> Option<Vec<FieldDef>> {
        let mut all_fields = Vec::new();
        let mut seen_names = std::collections::HashSet::new();

        for ty in types {
            let fields = match ty {
                SdblType::Ref(mdo_ref) => self.resolve_ref_fields(mdo_ref),
                SdblType::DefinedType { name, underlying_type } => {
                    self.resolve_defined_type_fields_in(name, underlying_type, resolving)
                }
                _ => None,
            };
            for field in fields.into_iter().flatten() {
                if seen_names.insert(field.name.fold_lower()) {
                    all_fields.push(field);
                }
            }
        }

        if all_fields.is_empty() {
            None
        } else {
            Some(all_fields)
        }
    }

    fn resolve_defined_type_fields(
        &self,
        name: &str,
        underlying_type: &Option<Box<SdblType>>,
    ) -> Option<Vec<FieldDef>> {
        self.resolve_defined_type_fields_in(name, underlying_type, &mut FxHashSet::default())
    }

    /// A field reached through a reference carries its defined type without the underlying
    /// type, which is then looked up through the resolver.
    fn resolve_defined_type_fields_in(
        &self,
        name: &str,
        underlying_type: &Option<Box<SdblType>>,
        resolving: &mut FxHashSet<String>,
    ) -> Option<Vec<FieldDef>> {
        if let Some(underlying) = underlying_type {
            return self.resolve_underlying_fields(underlying, resolving);
        }
        let key = name.fold_lower();
        if !resolving.insert(key.clone()) {
            return None;
        }
        let fields = self
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.resolver().resolve_defined_type(name))
            .and_then(|underlying| {
                self.resolve_underlying_fields(
                    &SdblType::from_attribute_type(&underlying),
                    resolving,
                )
            });
        resolving.remove(&key);
        fields
    }

    fn resolve_underlying_fields(
        &self,
        underlying: &SdblType,
        resolving: &mut FxHashSet<String>,
    ) -> Option<Vec<FieldDef>> {
        match underlying {
            SdblType::Ref(mdo_ref) => self.resolve_ref_fields(mdo_ref),
            SdblType::Composite { types } => self.resolve_composite_fields_in(types, resolving),
            SdblType::DefinedType { name: inner_name, underlying_type: inner_underlying } => {
                self.resolve_defined_type_fields_in(inner_name, inner_underlying, resolving)
            }
            _ => None,
        }
    }

    pub fn get_fields_for_ref(&self, mdo_ref: &crate::types::MdoRef) -> Vec<FieldDef> {
        self.resolve_ref_fields(mdo_ref).unwrap_or_default()
    }

    pub fn get_fields_for_composite(&self, types: &[SdblType]) -> Vec<FieldDef> {
        self.resolve_composite_fields(types).unwrap_or_default()
    }

    pub fn get_fields_for_defined_type(
        &self,
        name: &str,
        underlying_type: &Option<Box<SdblType>>,
    ) -> Vec<FieldDef> {
        self.resolve_defined_type_fields(name, underlying_type).unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct ColumnCompletion {
    pub column_name: Name,
    pub table_name: Name,
    pub ty: SdblType,
    pub is_standard: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir::ResolvedTable;
    use bsl_metadata::MdoType;

    fn make_table(name: &str, alias: Option<&str>, fields: Vec<FieldDef>) -> TableRef {
        TableRef {
            parts: vec![Name::from(name)],
            full_name: name.to_string(),
            alias: alias.map(Name::from),
            metadata: Some(ResolvedTable::Metadata {
                mdo_type: MdoType::Catalog,
                name: name.to_string(),
                fields,
                field_model_complete: false,
            }),
            is_virtual_table: false,
            virtual_table_params: Vec::new(),
            subquery: Vec::new(),
            parameter: None,
            range: syntax::MODULE_RANGE,
        }
    }

    #[test]
    fn test_scope_basic() {
        let mut scope = Scope::new();

        let table = make_table(
            "Справочник.Валюты",
            Some("В"),
            vec![
                FieldDef::standard(
                    "Ссылка",
                    "Ref",
                    SdblType::reference(MdoType::Catalog, "Валюты"),
                ),
                FieldDef::standard("Код", "Code", SdblType::string()),
            ],
        );

        let _ = scope.add_table(table);

        assert!(scope.find_table("в").is_some());
        assert!(scope.find_table("В").is_some());

        let ty = scope.resolve_column_type(Some("в"), "Код");
        assert_eq!(ty, SdblType::string());
    }

    #[test]
    fn test_scope_nested() {
        let mut scope = Scope::new();

        let outer_table =
            make_table("Outer", None, vec![FieldDef::new("Field1", SdblType::string())]);
        let _ = scope.add_table(outer_table);

        scope.push_frame();

        let inner_table =
            make_table("Inner", None, vec![FieldDef::new("Field2", SdblType::number())]);
        let _ = scope.add_table(inner_table);

        assert!(scope.find_table("Inner").is_some());
        assert!(scope.find_table("Outer").is_some());

        scope.pop_frame();

        assert!(scope.find_table("Outer").is_some());
        assert!(scope.find_table("Inner").is_none());
    }

    #[test]
    fn test_unqualified_column_resolution() {
        let mut scope = Scope::new();

        let table1 = make_table(
            "Table1",
            Some("T1"),
            vec![
                FieldDef::new("UniqueField", SdblType::string()),
                FieldDef::new("SharedField", SdblType::number()),
            ],
        );
        let table2 = make_table(
            "Table2",
            Some("T2"),
            vec![FieldDef::new("SharedField", SdblType::number())],
        );

        let _ = scope.add_table(table1);
        let _ = scope.add_table(table2);

        let ty = scope.resolve_column_type(None, "UniqueField");
        assert_eq!(ty, SdblType::string());

        let ty = scope.resolve_column_type(None, "SharedField");
        assert_eq!(ty, SdblType::Error);
    }

    #[test]
    fn defined_type_chained_underlying_unwraps_to_ref_fields() {
        use bsl_metadata::{Attribute, AttributeType, Configuration, MetadataObject};
        use std::sync::Arc;

        let mut config = Configuration::new("Test");
        let mut catalog = MetadataObject::new(MdoType::Catalog, "Валюты");
        catalog.add_attribute(Attribute {
            name: "Код".to_string(),
            name_en: Some("Code".to_string()),
            attr_type: AttributeType::String { length: Some(10) },
        });
        config.add_metadata_object(catalog);

        let scope = Scope::new_with_metadata(Some(Arc::new(config)));

        let inner = SdblType::DefinedType {
            name: "B".to_string(),
            underlying_type: Some(Box::new(SdblType::Ref(crate::types::MdoRef {
                mdo_type: MdoType::Catalog,
                name: "Валюты".to_string(),
            }))),
        };
        let outer_underlying = Some(Box::new(inner));

        let fields = scope.get_fields_for_defined_type("A", &outer_underlying);
        assert!(
            fields.iter().any(|f| f.name == "Код"),
            "chained DefinedType A → B → CatalogRef must surface `Код` from \
             the catalog underlying; got fields: {:?}",
            fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_duplicate_fields_when_metadata_has_standard_attributes() {
        use bsl_metadata::{Attribute, AttributeType, Configuration, MetadataObject};
        use std::sync::Arc;

        let mut config = Configuration::new("TestConfig");
        let mut obj = MetadataObject::new(MdoType::Catalog, "Валюты");

        obj.add_attribute(Attribute {
            name: "Ссылка".to_string(),
            name_en: Some("Ref".to_string()),
            attr_type: AttributeType::String { length: None },
        });
        obj.add_attribute(Attribute {
            name: "ПометкаУдаления".to_string(),
            name_en: Some("DeletionMark".to_string()),
            attr_type: AttributeType::Boolean,
        });
        obj.add_attribute(Attribute {
            name: "Код".to_string(),
            name_en: Some("Code".to_string()),
            attr_type: AttributeType::String { length: Some(10) },
        });
        obj.add_attribute(Attribute {
            name: "Наименование".to_string(),
            name_en: Some("Description".to_string()),
            attr_type: AttributeType::String { length: Some(100) },
        });
        obj.add_attribute(Attribute {
            name: "Курс".to_string(),
            name_en: Some("Rate".to_string()),
            attr_type: AttributeType::Number { precision: 15, scale: 4 },
        });

        config.add_metadata_object(obj);

        let scope = Scope::new_with_metadata(Some(Arc::new(config)));
        let mdo_ref =
            crate::types::MdoRef { mdo_type: MdoType::Catalog, name: "Валюты".to_string() };
        let fields = scope.get_fields_for_ref(&mdo_ref);

        let mut names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        names.sort_unstable();
        let original_len = names.len();
        names.dedup();
        assert_eq!(names.len(), original_len, "Duplicate fields found in resolve_ref_fields");

        let ref_field = fields.iter().find(|f| f.name == "Ссылка");
        assert!(ref_field.is_some(), "Ссылка field should be present");
        assert!(ref_field.unwrap().is_standard, "Ссылка should be marked as standard");

        let deletion_mark = fields.iter().find(|f| f.name == "ПометкаУдаления");
        assert!(deletion_mark.is_some(), "ПометкаУдаления field should be present");
        assert!(deletion_mark.unwrap().is_standard, "ПометкаУдаления should be marked as standard");

        let rate_field = fields.iter().find(|f| f.name == "Курс");
        assert!(rate_field.is_some(), "Курс field should be present");
        assert!(!rate_field.unwrap().is_standard, "Курс should not be marked as standard");
    }
}
