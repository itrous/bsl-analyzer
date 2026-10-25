use crate::diagnostics::SdblDiagnostic;
use crate::hir::{FieldDef, Name, ParameterSource, ResolvedTable, TableRef};
use crate::standard_fields::{
    is_parameterized_virtual_table, register_has_open_table_set, register_table_kinds,
    register_table_of_path, virtual_table_parameters, ParameterSlot, VirtualTableType,
};
use crate::SdblType;
use bsl_metadata::{is_standard_attribute_name, MdoType};
use stdx::case::CaseExt;
use syntax::ast::AstNode;
use text_size::TextRange;

use super::context::LoweringContext;

impl LoweringContext<'_> {
    pub(super) fn lower_from_clause(
        &mut self,
        from_clause: Option<syntax::ast::SdblFromClause>,
    ) -> Vec<TableRef> {
        let Some(from) = from_clause else {
            return Vec::new();
        };

        self.record_keyword_by_text(
            from.syntax(),
            "FROM",
            "ИЗ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        from.data_sources().map(|ds| self.lower_data_source_in_from(&ds)).collect()
    }

    fn lower_data_source_in_from(&mut self, ds: &syntax::ast::SdblDataSource) -> TableRef {
        // The first source takes part in every join that follows it, so a nested query
        // or a virtual table here is as costly as one on the right-hand side.
        let has_joins = ds.join_clauses().next().is_some();

        if has_joins {
            if let Some(subquery) = ds.subquery() {
                self.diagnostics.push(SdblDiagnostic::JoinWithSubQuery {
                    range: subquery.syntax().text_range(),
                });
            }

            if let Some(table_ref) = ds.table_ref() {
                let parts = self.parse_table_name(&table_ref);
                if is_parameterized_virtual_table(&parts) {
                    self.report_virtual_table_join(&parts, table_ref.syntax().text_range());
                }
            }
        }

        self.lower_data_source(ds)
    }

    /// Reports a virtual table that takes part in a join. The virtual table is computed by
    /// a nested query of its own, so the DBMS cannot estimate its size when choosing how to
    /// join it; the standard recommends materialising it into a temporary table first.
    pub(super) fn report_virtual_table_join(
        &mut self,
        parts: &[impl AsRef<str>],
        range: TextRange,
    ) {
        // Root-aware: a suffix is a virtual table only under a register kind that owns it,
        // and the stored `Субконто` / `Изменения` tables take no parameters.
        let Some((_, kind)) = register_table_of_path(parts).filter(|(_, vt)| vt.takes_parameters())
        else {
            return;
        };
        let table_name = parts.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(".");
        self.diagnostics.push(SdblDiagnostic::JoinWithVirtualTable {
            table_name,
            virtual_table_type: kind.as_str().to_string(),
            range,
        });
    }

    pub(super) fn lower_data_source(&mut self, ds: &syntax::ast::SdblDataSource) -> TableRef {
        if let Some(subquery) = ds.subquery() {
            let mut all_hirs = Vec::new();
            let mut all_fields = Vec::new();

            let queries: Vec<_> = subquery.queries().collect();
            let has_union_siblings = queries.len() > 1;

            for (query_index, query) in queries.into_iter().enumerate() {
                self.scope.push_frame();

                let nested_hir = self.lower_query(&query, has_union_siblings, query_index == 0);

                self.scope.pop_frame();

                if all_fields.is_empty() {
                    all_fields = nested_hir
                        .select
                        .fields
                        .iter()
                        .filter_map(|f| {
                            f.alias_or_name()
                                .map(|name| crate::hir::FieldDef::new(name.as_str(), f.ty.clone()))
                        })
                        .collect();
                }

                all_hirs.push(Box::new(nested_hir));
            }

            if all_hirs.is_empty() {
                return TableRef::missing(ds.syntax().text_range());
            }

            let alias_name = ds.alias().and_then(|a| a.name().map(|n| Name::from(n.as_str())));

            return TableRef {
                parts: Vec::new(),
                full_name: alias_name.as_ref().map(|a| a.to_string()).unwrap_or_default(),
                alias: alias_name.clone(),
                metadata: Some(crate::hir::ResolvedTable::TempTable {
                    name: alias_name.map(|a| a.to_string()).unwrap_or_default(),
                    fields: all_fields,
                    field_model_complete: false,
                }),
                is_virtual_table: false,
                virtual_table_params: Vec::new(),
                subquery: all_hirs,
                parameter: None,
                range: ds.syntax().text_range(),
            };
        }

        let Some(table_ref) = ds.table_ref() else {
            return TableRef::missing(ds.syntax().text_range());
        };

        self.lower_table_ref(&table_ref, ds.alias())
    }

    fn lower_table_ref(
        &mut self,
        table_ref: &syntax::ast::SdblTableRef,
        alias: Option<syntax::ast::SdblAlias>,
    ) -> TableRef {
        let parts = self.parse_table_name(table_ref);
        let full_name = parts.join(".");

        let register_table = register_table_of_path(&parts);
        let is_virtual = is_parameterized_virtual_table(&parts);

        let (_metadata, resolved) = self.resolve_table(&parts, table_ref.syntax().text_range());

        let ident_ranges: Vec<TextRange> = table_ref
            .syntax()
            .children_with_tokens()
            .filter_map(|child| match child {
                syntax::NodeOrToken::Token(token) if token.kind().is_name_token() => {
                    Some(token.text_range())
                }
                _ => None,
            })
            .collect();

        for (idx, (part, range)) in parts.iter().zip(ident_ranges.iter()).enumerate() {
            let category = if resolved.is_some() {
                if idx == 0 && parts.len() > 1 {
                    crate::source_map::TokenCategory::MdoType
                } else {
                    crate::source_map::TokenCategory::TableName
                }
            } else {
                crate::source_map::TokenCategory::UnresolvedTableName
            };
            self.source_map.add_token(
                crate::source_map::TokenInfo::new(*range, syntax::SyntaxKind::IDENT, part.as_str()),
                category,
            );
        }

        let alias_name = alias
            .and_then(|a| {
                if a.has_as_keyword() {
                    self.record_keyword_by_text(
                        a.syntax(),
                        "AS",
                        "КАК",
                        crate::source_map::TokenCategory::SpecialKeyword,
                    );
                }

                if let Some(ident_token) = a.identifier() {
                    self.source_map.add_token(
                        crate::source_map::TokenInfo::new(
                            ident_token.text_range(),
                            ident_token.kind(),
                            ident_token.text(),
                        ),
                        crate::source_map::TokenCategory::TableAlias,
                    );
                }

                a.name()
            })
            .map(|s| Name::from(s.as_str()));

        let virtual_table_params = if is_virtual {
            tracing::debug!(table_name = %full_name, "Lowering virtual table parameters");

            // A condition inside the parameters speaks only of the table's own fields, so the
            // frame is opaque whether or not those fields are known.
            self.scope.push_opaque_frame();
            if let Some(ref r) = resolved {
                let own_table = TableRef {
                    parts: Vec::new(),
                    full_name: String::new(),
                    alias: None,
                    metadata: Some(ResolvedTable::Metadata {
                        mdo_type: register_table
                            .map(|(mdo_type, _)| mdo_type)
                            .unwrap_or(MdoType::AccumulationRegister),
                        name: String::new(),
                        fields: r.fields().to_vec(),
                        field_model_complete: false,
                    }),
                    is_virtual_table: false,
                    virtual_table_params: Vec::new(),
                    subquery: Vec::new(),
                    parameter: None,
                    range: table_ref.syntax().text_range(),
                };
                let own_range = own_table.range;
                if let Some(alias) = self.scope.add_table(own_table) {
                    self.diagnostics
                        .push(SdblDiagnostic::DuplicateAlias { alias, range: own_range });
                }
            }

            let has_periodicity =
                register_table.map(|(_, vt)| vt.has_periodicity()).unwrap_or(false);

            let param_nodes: Vec<_> = table_ref
                .syntax()
                .children()
                .filter(|n| {
                    matches!(
                        n.kind(),
                        syntax::SyntaxKind::SDBL_LOGICAL_OR_EXPR
                            | syntax::SyntaxKind::SDBL_LOGICAL_AND_EXPR
                            | syntax::SyntaxKind::SDBL_COMPARISON_EXPR
                            | syntax::SyntaxKind::SDBL_ADDITIVE_EXPR
                            | syntax::SyntaxKind::SDBL_MULTIPLICATIVE_EXPR
                            | syntax::SyntaxKind::SDBL_UNARY_EXPR
                            | syntax::SyntaxKind::SDBL_COLUMN_REF
                            | syntax::SyntaxKind::SDBL_LITERAL
                            | syntax::SyntaxKind::SDBL_FUNCTION_CALL
                            | syntax::SyntaxKind::SDBL_PARAMETER
                            | syntax::SyntaxKind::SDBL_PAREN_EXPR
                            | syntax::SyntaxKind::SDBL_TUPLE_EXPR
                            | syntax::SyntaxKind::SDBL_IN_EXPR
                            | syntax::SyntaxKind::SDBL_MISSING_ARG
                            | syntax::SyntaxKind::ERROR
                    )
                })
                .collect();

            let params: Vec<_> = param_nodes
                .into_iter()
                .enumerate()
                .map(|(idx, expr)| {
                    if idx == 2 && has_periodicity {
                        let col_ref = if expr.kind() == syntax::SyntaxKind::SDBL_COLUMN_REF {
                            Some(expr.clone())
                        } else {
                            expr.descendants()
                                .find(|n| n.kind() == syntax::SyntaxKind::SDBL_COLUMN_REF)
                        };
                        if let Some(ref col) = col_ref {
                            if let Some(token) = col.first_token() {
                                if token.kind() == syntax::SyntaxKind::IDENT
                                    && crate::standard_fields::is_periodicity_value(token.text())
                                {
                                    self.source_map.add_token(
                                        crate::source_map::TokenInfo::new(
                                            token.text_range(),
                                            syntax::SyntaxKind::IDENT,
                                            token.text(),
                                        ),
                                        crate::source_map::TokenCategory::SpecialKeyword,
                                    );
                                    return crate::hir::ExprHir::Literal {
                                        value: crate::hir::LiteralValue::String(
                                            token.text().to_string(),
                                        ),
                                        ty: SdblType::string(),
                                        range: expr.text_range(),
                                    };
                                }
                            }
                        }
                    }
                    self.lower_expr(&expr)
                })
                .collect();

            self.scope.pop_frame();

            params
        } else {
            Vec::new()
        };

        if let Some((mdo_type, vt_type)) = register_table.filter(|_| is_virtual) {
            self.check_virtual_table_params(
                &full_name,
                mdo_type,
                vt_type,
                &virtual_table_params,
                table_ref.syntax(),
            );
        }

        TableRef {
            parts: parts.iter().map(|s| Name::from(s.as_str())).collect(),
            full_name,
            alias: alias_name,
            metadata: resolved,
            is_virtual_table: is_virtual,
            virtual_table_params,
            subquery: Vec::new(),
            parameter: parameter_source(table_ref),
            range: table_ref.syntax().text_range(),
        }
    }

    fn parse_table_name(&self, table_ref: &syntax::ast::SdblTableRef) -> Vec<String> {
        table_ref
            .syntax()
            .children_with_tokens()
            .filter_map(|child| match child {
                syntax::NodeOrToken::Token(token) if token.kind().is_name_token() => {
                    Some(token.text().to_string())
                }
                _ => None,
            })
            .collect()
    }

    fn resolve_table(
        &mut self,
        parts: &[String],
        range: TextRange,
    ) -> (Option<MdoType>, Option<ResolvedTable>) {
        tracing::debug!(parts = ?parts, "Resolving table");

        if parts.len() == 1 {
            let table_name = &parts[0];
            if let Some(temp_table) = self.scope.find_temp_table(table_name) {
                tracing::debug!(name = %table_name, fields = temp_table.fields.len(), "Resolved as temporary table");
                return (
                    None,
                    Some(ResolvedTable::TempTable {
                        name: temp_table.name.clone(),
                        fields: temp_table.fields.clone(),
                        field_model_complete: false,
                    }),
                );
            }
        }

        if parts.len() < 2 {
            tracing::debug!("Table parts < 2, skipping resolution");
            return (None, None);
        }

        let mdo_type_str = &parts[0];
        let Ok(mdo_type) = mdo_type_str.parse::<MdoType>() else {
            tracing::debug!(mdo_type_str = mdo_type_str, "Failed to parse MDO type");
            return (None, None);
        };

        let object_name = &parts[1];

        if mdo_type == MdoType::ExternalDataSource && parts.len() >= 4 {
            return self.resolve_external_data_source(parts, range);
        }

        let is_register = matches!(
            mdo_type,
            MdoType::InformationRegister
                | MdoType::AccumulationRegister
                | MdoType::AccountingRegister
                | MdoType::CalculationRegister
        );

        // Under a register the third segment names one of the register's own tables; under any
        // other root it names a tabular section.
        let tabular_section_name =
            if parts.len() == 3 && !is_register { Some(parts[2].as_str()) } else { None };
        let register_table = register_table_of_path(parts).map(|(_, table)| table);

        if let Some(resolver) = self.resolver {
            let exists = match mdo_type {
                MdoType::InformationRegister
                | MdoType::AccumulationRegister
                | MdoType::AccountingRegister
                | MdoType::CalculationRegister => {
                    let found = resolver.resolve_register(mdo_type, object_name).is_some();
                    tracing::debug!(
                        mdo_type = ?mdo_type,
                        object_name = %object_name,
                        found = found,
                        "Checking register in metadata"
                    );
                    found
                }
                _ => {
                    let found = resolver.resolve_metadata_object(mdo_type, object_name).is_some();
                    tracing::debug!(
                        mdo_type = ?mdo_type,
                        object_name = %object_name,
                        found = found,
                        "Checking metadata object"
                    );
                    found
                }
            };

            if !exists {
                tracing::debug!(
                    mdo_type = ?mdo_type,
                    object_name = object_name,
                    "Table not found in metadata"
                );
                self.diagnostics.push(SdblDiagnostic::QueryToMissingMetadata {
                    table_name: parts.join("."),
                    range,
                });
                return (Some(mdo_type), None);
            }
        } else {
            tracing::debug!("No metadata available for validation");
        }

        let full_name_for_logging = parts.join(".");

        if is_register
            && parts.len() == 3
            && !self.register_owns_table(mdo_type, object_name, register_table)
        {
            // The register itself was found above when a resolver is present, so the table is
            // what is missing — unless the kind names tables the platform does not fix.
            if self.resolver.is_some() && !register_has_open_table_set(mdo_type) {
                self.diagnostics.push(SdblDiagnostic::QueryToMissingMetadata {
                    table_name: full_name_for_logging,
                    range,
                });
                return (Some(mdo_type), None);
            }
            return (
                Some(mdo_type),
                Some(ResolvedTable::Metadata {
                    mdo_type,
                    name: object_name.clone(),
                    fields: Vec::new(),
                    field_model_complete: false,
                }),
            );
        }

        tracing::debug!(
            full_name = %full_name_for_logging,
            mdo_type = ?mdo_type,
            object_name = %object_name,
            tabular_section = ?tabular_section_name,
            is_register = is_register,
            has_metadata = self.resolver.is_some(),
            "resolve_table: Starting field resolution"
        );

        if is_register {
            let resolved = self.build_register_resolved(
                mdo_type,
                object_name,
                &full_name_for_logging,
                register_table,
            );
            return (Some(mdo_type), resolved);
        }

        let mut fields = Vec::new();
        let complete = if self.resolver.is_some() {
            self.add_metadata_fields(
                mdo_type,
                object_name,
                tabular_section_name,
                &full_name_for_logging,
                &mut fields,
            )
        } else {
            false
        };

        tracing::debug!(
            mdo_type = ?mdo_type,
            object_name = object_name,
            total_fields = fields.len(),
            "Resolved table with fields"
        );

        let resolved = ResolvedTable::Metadata {
            mdo_type,
            name: object_name.clone(),
            fields,
            field_model_complete: complete,
        };

        (Some(mdo_type), Some(resolved))
    }

    /// Whether the register named by a three-segment path has the table its last segment
    /// names. `table` is the segment already classified against the register KIND; what is left
    /// to ask is a property of the register itself that removes tables: a turnovers-only
    /// accumulation register has no balance, and an accounting register without correspondence
    /// has no debit-credit turnovers. An absent property is unknown, not false — reporting the
    /// table missing then would be a false positive.
    fn register_owns_table(
        &self,
        mdo_type: MdoType,
        object_name: &str,
        table: Option<VirtualTableType>,
    ) -> bool {
        let Some(table) = table else {
            return false;
        };
        debug_assert!(register_table_kinds(mdo_type).contains(&table));
        let register =
            self.resolver.and_then(|resolver| resolver.resolve_register(mdo_type, object_name));
        match (mdo_type, table) {
            (
                MdoType::AccumulationRegister,
                VirtualTableType::Balance | VirtualTableType::BalanceAndTurnovers,
            ) => {
                register.and_then(|register| register.register_type())
                    != Some(bsl_metadata::AccumulationRegisterType::Turnovers)
            }
            (MdoType::AccountingRegister, VirtualTableType::DrCrTurnovers) => {
                register.and_then(|register| register.correspondence()) != Some(false)
            }
            _ => true,
        }
    }

    fn build_register_resolved(
        &self,
        mdo_type: MdoType,
        object_name: &str,
        full_name: &str,
        table: Option<VirtualTableType>,
    ) -> Option<ResolvedTable> {
        let register = self.resolver?.resolve_register(mdo_type, object_name)?;

        if mdo_type == MdoType::AccountingRegister {
            return Some(self.build_accounting_register_table(&register, table));
        }

        let mut dimensions = Vec::new();
        for dim in register.dimensions() {
            let ty = dim
                .attr_type()
                .map(|at| self.resolve_attribute_type(at))
                .unwrap_or(SdblType::Unknown);
            dimensions.push(FieldDef::new(dim.name(), ty));
        }

        let mut resources = Vec::new();
        for res in register.resources() {
            let ty = res
                .attr_type()
                .map(|at| self.resolve_attribute_type(at))
                .unwrap_or(SdblType::Unknown);
            resources.push(FieldDef::new_with_names(
                res.name().to_string(),
                res.name_en().map(|s| s.to_string()),
                ty,
                false,
            ));
        }

        // A separator is part of the record key and a column of every virtual table, which is
        // what a dimension is; a plain common attribute behaves like a register attribute.
        for common in register.common_attributes().iter().filter(|c| c.separator) {
            dimensions
                .push(FieldDef::new(&common.name, self.resolve_attribute_type(&common.attr_type)));
        }

        let mut attributes = Vec::new();
        for attr in register.attributes() {
            let ty = attr
                .attr_type()
                .map(|at| self.resolve_attribute_type(at))
                .unwrap_or(SdblType::Unknown);
            // Only the synthesiser writes `name_en`, so its presence marks exactly the
            // reader-injected standard attributes; a user attribute named like another
            // object's standard (`Код`) must not be misclassified as one.
            let is_synthesized = attr.name_en().is_some();
            let mut field = FieldDef::new_with_names(
                attr.name().to_string(),
                attr.name_en().map(|s| s.to_string()),
                ty,
                is_synthesized,
            );
            // The metadata reader injects the recorder-mode fields into every register's
            // attribute list, so the mark has to be applied HERE and not only to the standard
            // set: `attributes` is what the virtual-table transform rebuilds its field list
            // from, and a mark placed only on the standard set does not survive that rebuild.
            field.provisional = Self::is_conditional_standard_field(mdo_type, attr.name());
            attributes.push(field);
        }
        for common in register.common_attributes().iter().filter(|c| !c.separator) {
            attributes.push(FieldDef::new_with_names(
                common.name.clone(),
                None,
                self.resolve_attribute_type(&common.attr_type),
                false,
            ));
        }

        let mut fields = Vec::new();
        fields.extend(dimensions.iter().cloned());
        fields.extend(resources.iter().cloned());
        fields.extend(attributes.iter().cloned());

        // The register's object-model standard fields (`Регистратор`/`Активность`/`НомерСтроки`
        // [/`Период`]) are synthesised by the metadata XML reader and already sit in
        // `attributes` above; they are NOT repeated here, so each name is offered exactly
        // once. Only the query table's own additions are appended: virtual columns and the
        // conditional names the reader did not emit for this very register. What the reader
        // did emit is answered by `attributes` itself, not by re-deriving the condition.
        let reader_has_period = attributes.iter().any(|field| field.matches_name("Период"));
        let (query_only_fields, standard_complete) =
            Self::register_query_only_fields(mdo_type, reader_has_period);
        let field_model_complete = standard_complete && !register.common_attributes_open();
        fields.extend(query_only_fields);

        tracing::debug!(
            mdo_type = ?mdo_type,
            object_name = object_name,
            full_name = full_name,
            dimensions = dimensions.len(),
            resources = resources.len(),
            attributes = attributes.len(),
            total_fields = fields.len(),
            field_model_complete,
            "Built Register resolved table"
        );

        let main = ResolvedTable::Register {
            mdo_type,
            name: object_name.to_string(),
            fields,
            dimensions,
            resources,
            attributes,
            field_model_complete,
        };
        Some(match table {
            Some(table) => Self::transform_for_virtual_table(main, table),
            None => main,
        })
    }

    /// Whether a register field of this kind exists only under object settings the metadata
    /// model does not read.
    ///
    /// Only the information register qualifies: its recorder-mode fields are absent from an
    /// independent register. An accumulation register is always subordinate to a recorder, so
    /// the same names are unconditional there.
    fn is_conditional_standard_field(mdo_type: MdoType, name: &str) -> bool {
        if mdo_type != MdoType::InformationRegister {
            return false;
        }
        const RECORDER_MODE_ONLY: &[&str] =
            &["Регистратор", "Активность", "НомерСтроки", "МоментВремени"];
        RECORDER_MODE_ONLY.iter().any(|known| stdx::case::eq_ignore_case(known, name))
    }

    /// Fields the register MAIN table has on top of the object-model set — query-only virtual
    /// columns and the conditional names the metadata reader did not synthesise for this very
    /// register — paired with whether the resulting field model is exhaustive enough to drive
    /// the unknown-field diagnostic. Sets follow the ITS query-language reference (pubqlang
    /// ch.82/92/111/130).
    ///
    /// The object-model standard fields themselves are owned by the metadata reader
    /// (`register.attributes()`); repeating them here is what offered every one of them twice.
    /// `reader_has_period` answers what the reader actually emitted for this register, so the
    /// two layers cannot drift apart on the periodicity condition.
    fn register_query_only_fields(
        mdo_type: MdoType,
        reader_has_period: bool,
    ) -> (Vec<FieldDef>, bool) {
        match mdo_type {
            // The reader emits `Период` only for a periodic register, while the query
            // reference lists it for every register — so it is over-added here, provisional,
            // when the reader did not emit it. `МоментВремени` is query-only (like on
            // documents) and exists only in "Подчинение регистратору" mode.
            MdoType::InformationRegister => {
                let mut fields = vec![FieldDef::provisional_standard(
                    "МоментВремени",
                    "PointInTime",
                    SdblType::DateTime,
                )];
                if !reader_has_period {
                    fields.push(FieldDef::provisional_standard("Период", "Period", SdblType::Date));
                }
                (fields, true)
            }
            MdoType::AccumulationRegister => (
                vec![
                    FieldDef::standard("МоментВремени", "PointInTime", SdblType::DateTime),
                    FieldDef::standard("ВидДвижения", "RecordType", SdblType::string()),
                ],
                true,
            ),
            // The metadata reader has no standard-attribute synthesis for the kinds below,
            // so these sets remain the only source of their standard fields.
            //
            // No plain `Период` here — `ПериодРегистрации` is the anchor. The
            // action-/base-period fields are conditional on register properties the metadata
            // model does not read, so they are listed provisionally.
            MdoType::CalculationRegister => (
                vec![
                    FieldDef::standard("Регистратор", "Recorder", SdblType::AnyRef),
                    FieldDef::standard("НомерСтроки", "LineNumber", SdblType::number()),
                    FieldDef::standard("Активность", "Active", SdblType::Boolean),
                    FieldDef::standard("ВидРасчета", "CalculationType", SdblType::AnyRef),
                    FieldDef::standard("Сторно", "Reversal", SdblType::Boolean),
                    FieldDef::standard("ПериодРегистрации", "RegistrationPeriod", SdblType::Date),
                    FieldDef::provisional_standard(
                        "ПериодДействия",
                        "ActionPeriod",
                        SdblType::Date,
                    ),
                    FieldDef::provisional_standard(
                        "ПериодДействияНачало",
                        "ActionPeriodBegin",
                        SdblType::Date,
                    ),
                    FieldDef::provisional_standard(
                        "ПериодДействияКонец",
                        "ActionPeriodEnd",
                        SdblType::Date,
                    ),
                    FieldDef::provisional_standard(
                        "БазовыйПериодНачало",
                        "BasePeriodBegin",
                        SdblType::Date,
                    ),
                    FieldDef::provisional_standard(
                        "БазовыйПериодКонец",
                        "BasePeriodEnd",
                        SdblType::Date,
                    ),
                ],
                true,
            ),
            // Accounting main-table fields need per-dimension/resource "balanced"
            // flags and the register's correspondence-support flag to synthesise
            // the Дт/Кт-suffixed names and СчетДт/СчетКт (ITS ch.111) — neither is
            // parsed into the metadata model yet. Until that lands, expose only
            // the always-fixed fields for completion and keep the model
            // INCOMPLETE so the unknown-field diagnostic stays silent (no FPs).
            MdoType::AccountingRegister => (
                vec![
                    FieldDef::standard("Период", "Period", SdblType::Date),
                    FieldDef::standard("Регистратор", "Recorder", SdblType::AnyRef),
                    FieldDef::standard("НомерСтроки", "LineNumber", SdblType::number()),
                    FieldDef::standard("Активность", "Active", SdblType::Boolean),
                    FieldDef::standard("МоментВремени", "PointInTime", SdblType::DateTime),
                ],
                false,
            ),
            _ => (Vec::new(), false),
        }
    }

    /// Which of a register's fields may not exist is a property of the REGISTER, not of the
    /// table shape it is read through, so it is decided once — when the main table is built —
    /// and inherited by every virtual table derived from it.
    ///
    /// This is the single point where a `ResolvedTable::Register` leaving this module gets its
    /// marks. Each virtual-table branch rebuilds its own field list from literals, and marking
    /// them branch by branch is what made the same false positive come back three times: the
    /// main table fell silent while the slice kept firing, then `Активность` was fixed and
    /// `Период` repeated it. A branch added later inherits the marks without knowing they exist.
    pub(crate) fn transform_for_virtual_table(
        resolved: ResolvedTable,
        vt_type: crate::standard_fields::VirtualTableType,
    ) -> ResolvedTable {
        let provisional: Vec<String> = resolved
            .fields()
            .iter()
            .filter(|field| field.provisional)
            .map(|field| field.name.clone())
            .collect();

        let mut transformed = Self::rebuild_for_virtual_table(resolved, vt_type);

        if let ResolvedTable::Register { fields, dimensions, resources, attributes, .. } =
            &mut transformed
        {
            for list in [fields, dimensions, resources, attributes] {
                for field in list.iter_mut() {
                    if provisional.iter().any(|name| field.matches_name(name)) {
                        field.provisional = true;
                    }
                }
            }
        }

        transformed
    }

    fn rebuild_for_virtual_table(
        resolved: ResolvedTable,
        vt_type: crate::standard_fields::VirtualTableType,
    ) -> ResolvedTable {
        use crate::standard_fields::VirtualTableType;

        let ResolvedTable::Register {
            mdo_type,
            name,
            dimensions,
            resources,
            attributes,
            field_model_complete: main_complete,
            ..
        } = resolved
        else {
            return resolved;
        };

        // The shapes below are exhaustive only for the register kind whose table they model
        // (ITS «Язык запросов», гл. 3, разделы 97-106 and the slice tables); the same suffix
        // reached through another kind keeps the old, silent verdict.
        let complete_for = |kind: MdoType| main_complete && mdo_type == kind;

        let derive = |suffix_ru: &str, suffix_en: &str| -> Vec<FieldDef> {
            resources
                .iter()
                .map(|r| {
                    FieldDef::new_with_names(
                        format!("{}{suffix_ru}", r.name),
                        Some(format!("{}{suffix_en}", r.name_en.as_deref().unwrap_or(&r.name))),
                        r.ty.clone(),
                        false,
                    )
                    .derived_from(r.source.clone().unwrap_or_else(|| r.name.clone()))
                })
                .collect()
        };

        match vt_type {
            VirtualTableType::Turnovers => {
                let mut new_resources = derive("Оборот", "Turnover");
                new_resources.extend(derive("Приход", "Receipt"));
                new_resources.extend(derive("Расход", "Expense"));

                let mut fields = vec![
                    FieldDef::standard("Период", "Period", SdblType::Date),
                    FieldDef::standard("Регистратор", "Recorder", SdblType::AnyRef),
                    FieldDef::standard("НомерСтроки", "LineNumber", SdblType::number()),
                ];
                super::register_tables::period_breakdown_fields(&mut fields);
                fields.extend(dimensions.iter().cloned());
                fields.extend(new_resources.iter().cloned());
                // The reference lists no attributes here, yet code shipped in a typical
                // configuration selects a turnovers-register attribute from this table. Until
                // that is settled against the platform, the attributes are offered, provisionally.
                fields.extend(attributes.iter().cloned().map(|mut field| {
                    field.provisional = true;
                    field
                }));

                ResolvedTable::Register {
                    mdo_type,
                    name,
                    fields,
                    dimensions,
                    resources: new_resources,
                    attributes: Vec::new(),
                    field_model_complete: complete_for(MdoType::AccumulationRegister),
                }
            }
            VirtualTableType::Balance => {
                let new_resources = derive("Остаток", "Balance");

                let mut fields = Vec::new();
                fields.extend(dimensions.iter().cloned());
                fields.extend(new_resources.iter().cloned());

                ResolvedTable::Register {
                    mdo_type,
                    name,
                    fields,
                    dimensions,
                    resources: new_resources,
                    attributes: Vec::new(),
                    field_model_complete: complete_for(MdoType::AccumulationRegister),
                }
            }
            VirtualTableType::BalanceAndTurnovers => {
                let mut new_resources = derive("НачальныйОстаток", "OpeningBalance");
                new_resources.extend(derive("Оборот", "Turnover"));
                new_resources.extend(derive("Приход", "Receipt"));
                new_resources.extend(derive("Расход", "Expense"));
                new_resources.extend(derive("КонечныйОстаток", "ClosingBalance"));

                let mut fields = vec![
                    FieldDef::standard("Период", "Period", SdblType::Date),
                    FieldDef::standard("Регистратор", "Recorder", SdblType::AnyRef),
                    FieldDef::standard("НомерСтроки", "LineNumber", SdblType::number()),
                ];
                super::register_tables::period_breakdown_fields(&mut fields);
                fields.extend(dimensions.iter().cloned());
                fields.extend(new_resources.iter().cloned());

                ResolvedTable::Register {
                    mdo_type,
                    name,
                    fields,
                    dimensions,
                    resources: new_resources,
                    attributes: Vec::new(),
                    field_model_complete: complete_for(MdoType::AccumulationRegister),
                }
            }
            VirtualTableType::SliceLast | VirtualTableType::SliceFirst => {
                // The register's own `Период` (a periodic register carries it as a standard
                // attribute) must not be joined by a second, literal copy.
                let mut fields = Vec::new();
                if !attributes.iter().any(|field| field.matches_name("Период")) {
                    fields.push(FieldDef::standard("Период", "Period", SdblType::Date));
                }
                fields.extend(dimensions.iter().cloned());
                fields.extend(resources.iter().cloned());
                fields.extend(attributes.iter().cloned());

                ResolvedTable::Register {
                    mdo_type,
                    name,
                    fields,
                    dimensions,
                    resources,
                    attributes,
                    field_model_complete: complete_for(MdoType::InformationRegister),
                }
            }
            _ => ResolvedTable::Register {
                mdo_type,
                name: name.clone(),
                fields: {
                    let mut f = Vec::new();
                    f.extend(dimensions.iter().cloned());
                    f.extend(resources.iter().cloned());
                    f.extend(attributes.iter().cloned());
                    f
                },
                dimensions,
                resources,
                attributes,
                field_model_complete: false,
            },
        }
    }

    /// Returns `true` only when the produced field set is exhaustive for the
    /// table's schema (so an unknown-field diagnostic is false-positive safe).
    fn add_metadata_fields(
        &self,
        mdo_type: MdoType,
        object_name: &str,
        tabular_section_name: Option<&str>,
        full_name: &str,
        fields: &mut Vec<FieldDef>,
    ) -> bool {
        let Some(resolver) = self.resolver else {
            tracing::debug!("No metadata available for field resolution");
            return false;
        };

        if let Some(ts_name) = tabular_section_name {
            return self.add_tabular_section_fields(
                mdo_type,
                object_name,
                ts_name,
                full_name,
                fields,
            );
        }

        match mdo_type {
            MdoType::Catalog
            | MdoType::Document
            | MdoType::BusinessProcess
            | MdoType::Task
            | MdoType::ExchangePlan
            | MdoType::ChartOfCharacteristicTypes
            | MdoType::ChartOfAccounts
            | MdoType::ChartOfCalculationTypes => {
                tracing::debug!(
                    full_name = %full_name,
                    mdo_type = ?mdo_type,
                    object_name = %object_name,
                    "add_metadata_fields: Looking up metadata object"
                );

                if let Some(obj) = resolver.resolve_metadata_object(mdo_type, object_name) {
                    let initial_count = fields.len();

                    for attribute in &obj.attributes {
                        let ty = self.resolve_attribute_type(&attribute.attr_type);
                        let is_standard = is_standard_attribute_name(&attribute.name)
                            || attribute.name_en.as_deref().is_some_and(is_standard_attribute_name);
                        fields.push(FieldDef::new_with_names(
                            attribute.name.clone(),
                            attribute.name_en.clone(),
                            ty,
                            is_standard,
                        ));
                    }

                    // Virtual query fields present on every reference table but
                    // absent from StandardAttributeKind (ITS ch.18 / §8.3).
                    fields.push(FieldDef::standard(
                        "Представление",
                        "Presentation",
                        SdblType::string(),
                    ));
                    fields.push(FieldDef::standard(
                        "ВерсияДанных",
                        "DataVersion",
                        SdblType::string(),
                    ));

                    // МоментВремени is a query-only virtual field of document
                    // tables (date + ref); it has no BSL object-model
                    // counterpart, so it lives here rather than in the
                    // standard-attribute synthesiser.
                    if mdo_type == MdoType::Document {
                        fields.push(FieldDef::standard(
                            "МоментВремени",
                            "PointInTime",
                            SdblType::DateTime,
                        ));
                    }

                    for common in &obj.common_attributes {
                        fields.push(FieldDef::new_with_names(
                            common.name.clone(),
                            None,
                            self.resolve_attribute_type(&common.attr_type),
                            false,
                        ));
                    }

                    // Tabular-section names are valid columns of the parent
                    // (ITS ch.26, type РезультатЗапроса).
                    for ts in &obj.tabular_sections {
                        fields.push(FieldDef::new_with_names(
                            ts.name().to_string(),
                            ts.name_en().map(|s| s.to_string()),
                            SdblType::TabularSectionRef {
                                parent_mdo_type: mdo_type,
                                parent_mdo_name: object_name.to_string(),
                                ts_name: ts.name().to_string(),
                            },
                            true,
                        ));
                    }

                    tracing::debug!(
                        mdo_type = ?mdo_type,
                        object_name = object_name,
                        attributes = obj.attributes.len(),
                        fields_added = fields.len() - initial_count,
                        total_fields = fields.len(),
                        "Added metadata fields to object"
                    );
                    !obj.common_attributes_open
                } else {
                    tracing::debug!(
                        full_name = %full_name,
                        mdo_type = ?mdo_type,
                        object_name = %object_name,
                        "Metadata object not found (may be from extension)"
                    );
                    false
                }
            }

            _ => false,
        }
    }

    /// Returns `true` only when both the parent object and the named tabular
    /// section resolved (the field set is then exhaustive).
    fn add_tabular_section_fields(
        &self,
        mdo_type: MdoType,
        object_name: &str,
        tabular_section_name: &str,
        full_name: &str,
        fields: &mut Vec<FieldDef>,
    ) -> bool {
        let Some(resolver) = self.resolver else {
            tracing::debug!("No metadata available for tabular section resolution");
            return false;
        };

        tracing::debug!(
            full_name = %full_name,
            mdo_type = ?mdo_type,
            object_name = %object_name,
            tabular_section_name = %tabular_section_name,
            "add_tabular_section_fields: Looking up tabular section in metadata"
        );

        match mdo_type {
            MdoType::Catalog
            | MdoType::Document
            | MdoType::BusinessProcess
            | MdoType::Task
            | MdoType::ExchangePlan
            | MdoType::ChartOfCharacteristicTypes
            | MdoType::ChartOfCalculationTypes
            | MdoType::ChartOfAccounts => {}
            _ => {
                tracing::debug!(
                    mdo_type = ?mdo_type,
                    object_name = %object_name,
                    tabular_section_name = %tabular_section_name,
                    "MDO type does not support tabular sections"
                );
                return false;
            }
        }

        let Some(parent_obj) = resolver.resolve_metadata_object(mdo_type, object_name) else {
            tracing::debug!(
                mdo_type = ?mdo_type,
                object_name = %object_name,
                "Parent object not found in metadata (may be from extension)"
            );
            return false;
        };

        let Some(tabular_section) = parent_obj.find_tabular_section(tabular_section_name) else {
            tracing::debug!(
                mdo_type = ?mdo_type,
                object_name = %object_name,
                tabular_section_name = %tabular_section_name,
                available_sections = ?parent_obj.tabular_sections.iter()
                    .map(|ts| ts.name())
                    .collect::<Vec<_>>(),
                "Tabular section not found in parent object (may be from extension)"
            );
            return false;
        };

        tracing::debug!(
            tabular_section_name = %tabular_section_name,
            attributes_count = tabular_section.attributes().len(),
            "Found tabular section in metadata"
        );

        let ref_type = SdblType::reference(mdo_type, object_name);
        fields.push(FieldDef::new_with_names(
            "Ссылка".to_string(),
            Some("Ref".to_string()),
            ref_type,
            true,
        ));

        fields.push(FieldDef::new_with_names(
            "НомерСтроки".to_string(),
            Some("LineNumber".to_string()),
            SdblType::number(),
            true,
        ));

        for attribute in tabular_section.attributes() {
            let ty = SdblType::from_attribute_type(attribute.attr_type());

            fields.push(FieldDef::new_with_names(
                attribute.name().to_string(),
                attribute.name_en().map(|s| s.to_string()),
                ty,
                false,
            ));
        }

        tracing::debug!(
            mdo_type = ?mdo_type,
            object_name = %object_name,
            tabular_section_name = %tabular_section_name,
            total_fields = fields.len(),
            "Added tabular section fields"
        );

        true
    }

    fn resolve_external_data_source(
        &mut self,
        parts: &[String],
        range: TextRange,
    ) -> (Option<MdoType>, Option<ResolvedTable>) {
        let eds_name = &parts[1];

        tracing::debug!(
            eds_name = %eds_name,
            parts_len = parts.len(),
            "Resolving ExternalDataSource path"
        );

        let Some(resolver) = self.resolver else {
            tracing::debug!("No metadata available for EDS validation");
            return (Some(MdoType::ExternalDataSource), None);
        };

        let eds_obj = resolver.resolve_metadata_object(MdoType::ExternalDataSource, eds_name);
        if eds_obj.is_none() {
            tracing::debug!(eds_name = %eds_name, "ExternalDataSource not found in metadata");
            self.diagnostics.push(SdblDiagnostic::QueryToMissingMetadata {
                table_name: format!("{}.{}", parts[0], eds_name),
                range,
            });
            return (Some(MdoType::ExternalDataSource), None);
        }

        let eds_obj = eds_obj.unwrap();

        let container_type = parts[2].fold_lower();

        if parts.len() == 4 && (container_type == "таблица" || container_type == "table") {
            tracing::debug!(
                eds_name = %eds_name,
                table_name = %parts[3],
                "EDS table path (table validation not implemented)"
            );
            return (Some(MdoType::ExternalDataSource), None);
        }

        if parts.len() == 6 && (container_type == "куб" || container_type == "cube") {
            let cube_name = &parts[3];
            let dim_table_type = parts[4].fold_lower();
            let dim_table_name = &parts[5];

            let cube_obj = eds_obj.find_child(cube_name);
            if cube_obj.is_none() {
                tracing::debug!(
                    eds_name = %eds_name,
                    cube_name = %cube_name,
                    "Cube not found in ExternalDataSource"
                );
                self.diagnostics.push(SdblDiagnostic::QueryToMissingMetadata {
                    table_name: format!("{}.{}.{}.{}", parts[0], eds_name, parts[2], cube_name),
                    range,
                });
                return (Some(MdoType::ExternalDataSource), None);
            }

            let cube_obj = cube_obj.unwrap();

            if dim_table_type == "таблицаизмерения" || dim_table_type == "dimensiontable"
            {
                let dim_table_obj = cube_obj.find_child(dim_table_name);
                if dim_table_obj.is_none() {
                    tracing::debug!(
                        cube_name = %cube_name,
                        dim_table_name = %dim_table_name,
                        "DimensionTable not found in Cube"
                    );
                    self.diagnostics.push(SdblDiagnostic::QueryToMissingMetadata {
                        table_name: format!(
                            "{}.{}.{}.{}.{}.{}",
                            parts[0], eds_name, parts[2], cube_name, parts[4], dim_table_name
                        ),
                        range,
                    });
                    return (Some(MdoType::ExternalDataSource), None);
                }
            }

            tracing::debug!(
                eds_name = %eds_name,
                cube_name = %cube_name,
                dim_table_name = %dim_table_name,
                "EDS Cube DimensionTable resolved"
            );
            return (Some(MdoType::ExternalDataSource), None);
        }

        tracing::debug!(
            parts = ?parts,
            "Unhandled EDS path pattern"
        );
        (Some(MdoType::ExternalDataSource), None)
    }

    pub(crate) fn resolve_attribute_type(
        &self,
        attr_type: &bsl_metadata::AttributeType,
    ) -> SdblType {
        let mut visited = std::collections::HashSet::new();
        self.resolve_attribute_type_inner(attr_type, &mut visited)
    }

    fn resolve_attribute_type_inner(
        &self,
        attr_type: &bsl_metadata::AttributeType,
        visited: &mut std::collections::HashSet<String>,
    ) -> SdblType {
        use bsl_metadata::AttributeType;

        match attr_type {
            AttributeType::DefinedType { name } => {
                let key = name.fold_lower();
                if !visited.insert(key.clone()) {
                    return SdblType::DefinedType { name: name.clone(), underlying_type: None };
                }
                let underlying_type =
                    self.resolver.and_then(|r| r.resolve_defined_type(name)).map(|underlying| {
                        Box::new(self.resolve_attribute_type_inner(&underlying, visited))
                    });
                visited.remove(&key);
                SdblType::DefinedType { name: name.clone(), underlying_type }
            }
            AttributeType::Composite { types } => {
                let arms: Vec<SdblType> =
                    types.iter().map(|t| self.resolve_attribute_type_inner(t, visited)).collect();
                if arms.is_empty() {
                    SdblType::Unknown
                } else if arms.len() == 1 {
                    arms.into_iter().next().unwrap()
                } else {
                    SdblType::Composite { types: arms }
                }
            }
            _ => SdblType::from_attribute_type(attr_type),
        }
    }

    fn check_virtual_table_params(
        &mut self,
        table_name: &str,
        mdo_type: MdoType,
        vt_type: VirtualTableType,
        params: &[crate::hir::ExprHir],
        table_ref_node: &syntax::SyntaxNode,
    ) {
        use crate::hir::ExprHir;

        let has_parens = table_ref_node
            .children_with_tokens()
            .any(|child| matches!(child, syntax::NodeOrToken::Token(t) if t.kind() == syntax::SyntaxKind::L_PAREN));

        let range = table_ref_node.text_range();

        if !has_parens {
            self.diagnostics.push(SdblDiagnostic::VirtualTableCallWithoutParameters {
                table_name: table_name.to_string(),
                expected_params: vec!["Период".to_string(), "Условие".to_string()],
                range,
            });
            return;
        }

        // An argument list cut off before its `)` or holding text the query language
        // does not accept is not what the platform will receive: the module completes it
        // at run time (`"…Остатки(" + Условие + ")…"`, a `//УСЛОВИЕ` or `%1` placeholder
        // replaced through СтрЗаменить / СтрШаблон). Its arguments are unknown, not absent.
        let arguments_intact = table_ref_node.children_with_tokens().any(|child| {
            matches!(child, syntax::NodeOrToken::Token(t) if t.kind() == syntax::SyntaxKind::R_PAREN)
        }) && !table_ref_node
            .descendants()
            .any(|node| node.kind() == syntax::SyntaxKind::ERROR);
        if !arguments_intact {
            return;
        }

        // "Without parameters" means no argument was actually passed: a call
        // like СрезПоследних(&Период, ) names the period and already addresses
        // the diagnostic's concern; only all-blank argument lists are flagged.
        // A data-composition argument such as `{(&Период)}` is an argument too, although it
        // lowers to nothing.
        let has_argument = params.iter().any(|p| !matches!(p, ExprHir::Missing { .. }))
            || table_ref_node
                .children()
                .any(|child| child.kind() == syntax::SyntaxKind::SDBL_QUERY_EXTENSION);
        if !has_argument {
            self.diagnostics.push(SdblDiagnostic::VirtualTableCallWithoutParameters {
                table_name: table_name.to_string(),
                expected_params: vec!["Период".to_string(), "Условие".to_string()],
                range,
            });
        }

        let Some(slots) = virtual_table_parameters(mdo_type, vt_type) else {
            return;
        };
        let Some(arguments) = call_arguments(table_ref_node) else {
            return;
        };

        // The platform reads parameters by position, so one surplus comma shifts every later
        // argument into the wrong slot — and it answers that with «Неверные параметры», not with
        // a field error. Typical cause: a condition split by a comma instead of `И`.
        if arguments.len() > slots.len() {
            let range =
                arguments[slots.len()].as_ref().map(|node| node.text_range()).unwrap_or(range);
            self.diagnostics.push(SdblDiagnostic::InvalidVirtualTableParameters {
                table_name: table_name.to_string(),
                problem: crate::diagnostics::VirtualTableParameterProblem::TooMany {
                    passed: arguments.len(),
                    allowed: slots.iter().map(|slot| slot.name().to_string()).collect(),
                },
                range,
            });
            return;
        }

        for (index, (argument, slot)) in arguments.iter().zip(slots.iter()).enumerate() {
            let (ParameterSlot::Value(slot_name), Some(argument)) = (slot, argument) else {
                continue;
            };
            if is_condition(argument) {
                self.diagnostics.push(SdblDiagnostic::InvalidVirtualTableParameters {
                    table_name: table_name.to_string(),
                    problem: crate::diagnostics::VirtualTableParameterProblem::ConditionAsValue {
                        position: index + 1,
                        slot: slot_name.to_string(),
                    },
                    range: argument.text_range(),
                });
            }
        }
    }
}

/// The arguments of a virtual-table call as the text spells them: one entry per comma-separated
/// position, `None` for an empty one.
///
/// Read from the commas of the call itself rather than from the lowered parameter list, which
/// keeps only the expression kinds it lowers and so can drop or add an entry. `None` when the
/// call did not parse cleanly — a recovered argument list says nothing reliable about
/// positions, and a template macro such as `#ВременнаяТаблица1` is exactly such a list.
fn call_arguments(table_ref: &syntax::SyntaxNode) -> Option<Vec<Option<syntax::SyntaxNode>>> {
    use syntax::{NodeOrToken, SyntaxKind};

    if table_ref.descendants().any(|node| node.kind() == SyntaxKind::ERROR) {
        return None;
    }
    let mut arguments = Vec::new();
    let mut current: Option<syntax::SyntaxNode> = None;
    let mut inside = false;
    for element in table_ref.children_with_tokens() {
        match element {
            NodeOrToken::Token(token) => match token.kind() {
                SyntaxKind::L_PAREN if !inside => inside = true,
                SyntaxKind::COMMA if inside => arguments.push(current.take()),
                SyntaxKind::R_PAREN if inside => {
                    arguments.push(current.take());
                    return Some(arguments);
                }
                kind if inside && !kind.is_trivia() => return None,
                _ => {}
            },
            NodeOrToken::Node(node) if inside => match node.kind() {
                SyntaxKind::SDBL_MISSING_ARG | SyntaxKind::SDBL_QUERY_EXTENSION => {}
                _ if current.is_some() => return None,
                _ => current = Some(node),
            },
            NodeOrToken::Node(_) => {}
        }
    }
    None
}

/// A predicate where a value is expected: the platform cannot read a comparison as a date, a
/// list of ext-dimension kinds or a row count, so it rejects the call. Only the TOP of the
/// expression is asked — a `ВЫБОР` whose branches compare things still yields a value.
///
/// The tree wraps every operand in one node per precedence level (`&Период` is an OR node
/// holding an AND node holding …), and parentheses are a wrapper too, so the top is found
/// by walking down the wrappers that carry a single child and no operator of their own.
/// Parentheses do not turn a comparison into a value: the platform still rejects the call.
fn is_condition(argument: &syntax::SyntaxNode) -> bool {
    use syntax::SyntaxKind;
    let mut node = argument.clone();
    loop {
        let kind = node.kind();
        match kind {
            SyntaxKind::SDBL_COMPARISON_EXPR
            | SyntaxKind::SDBL_NOT_EXPR
            | SyntaxKind::SDBL_IN_EXPR
            | SyntaxKind::SDBL_IN_HIERARCHY_EXPR
            | SyntaxKind::SDBL_BETWEEN_EXPR
            | SyntaxKind::SDBL_LIKE_EXPR
            | SyntaxKind::SDBL_IS_NULL_EXPR
            | SyntaxKind::SDBL_REFS_EXPR => return true,
            SyntaxKind::SDBL_PAREN_EXPR => match node.children().next() {
                Some(child) => node = child,
                None => return false,
            },
            SyntaxKind::SDBL_LOGICAL_OR_EXPR
            | SyntaxKind::SDBL_LOGICAL_AND_EXPR
            | SyntaxKind::SDBL_ADDITIVE_EXPR
            | SyntaxKind::SDBL_MULTIPLICATIVE_EXPR => {
                let has_operator = node
                    .children_with_tokens()
                    .filter_map(|element| element.into_token())
                    .any(|token| !token.kind().is_trivia());
                let mut children = node.children();
                let only = children.next();
                if has_operator || children.next().is_some() {
                    return matches!(
                        kind,
                        SyntaxKind::SDBL_LOGICAL_OR_EXPR | SyntaxKind::SDBL_LOGICAL_AND_EXPR
                    );
                }
                match only {
                    Some(child) => node = child,
                    None => return false,
                }
            }
            _ => return false,
        }
    }
}

/// The parameter a table reference names as its source. Virtual table arguments are
/// children of the reference too, so only the first significant token tells a
/// parameter source (`&Name`) from a table name.
fn parameter_source(table_ref: &syntax::ast::SdblTableRef) -> Option<ParameterSource> {
    let token = syntax::ast_utils::significant_tokens(table_ref.syntax()).next()?;
    if token.kind() != syntax::SyntaxKind::AMPERSAND {
        return None;
    }
    let name = token.text().trim_start_matches('&');
    if name.is_empty() {
        return None;
    }
    Some(ParameterSource { name: Name::from(name), range: token.text_range() })
}
