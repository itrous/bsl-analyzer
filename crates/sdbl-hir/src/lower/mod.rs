mod clauses;
mod context;
mod diagnostics;
mod expr;
mod from_clause;
mod join_clause;
mod missing_row;
#[cfg(test)]
mod register_table_tests;
mod register_tables;
mod select_fields;
#[cfg(test)]
mod tests;
mod union;

use syntax::ast::{AstNode, SdblQueryPackage, SdblSelectQuery};
use syntax::Parse;

use crate::hir::SdblHir;
use crate::source_map::SdblSourceMap;

pub use context::LoweringContext;

use bsl_metadata::{Configuration, QueryMetadataResolver};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdblLowerResult {
    pub hir: SdblHir,

    pub source_map: SdblSourceMap,
}

/// Lower SDBL against a whole `Configuration`. Used by the cold call sites
/// (graph build, streaming, tests) that carry a merged config. The hot, Salsa-
/// cached path goes through [`lower_sdbl_to_hir_with_resolver`] with a db-backed
/// per-MDO resolver so it depends only on the metadata objects it touches.
pub fn lower_sdbl_to_hir(
    sdbl_ast: &Parse<syntax::SyntaxNode>,
    metadata: Option<std::sync::Arc<Configuration>>,
) -> crate::hir::SdblPackage {
    lower_sdbl_to_hir_with_resolver(
        sdbl_ast,
        metadata.as_deref().map(|config| config as &dyn QueryMetadataResolver),
    )
}

pub fn lower_sdbl_to_hir_with_resolver(
    sdbl_ast: &Parse<syntax::SyntaxNode>,
    resolver: Option<&dyn QueryMetadataResolver>,
) -> crate::hir::SdblPackage {
    let _span = tracing::debug_span!("lower_sdbl_to_hir").entered();

    let root = sdbl_ast.syntax_node();

    let Some(package) = SdblQueryPackage::cast(root) else {
        tracing::debug!("Failed to cast root as SdblQueryPackage");
        return crate::hir::SdblPackage::empty();
    };

    let mut ctx = LoweringContext::new(resolver);

    let mut sdbl_queries = Vec::new();
    let mut parameter_sources = Vec::new();
    let mut select_index = 0;
    let mut has_query_item = false;

    for query_item in package.syntax().children() {
        match query_item.kind() {
            syntax::SyntaxKind::SDBL_SELECT_QUERY => {
                has_query_item = true;
            }
            syntax::SyntaxKind::SDBL_DROP_QUERY => {
                has_query_item = true;
                ctx.lower_drop_query(&query_item);
                continue;
            }
            _ => continue,
        }

        let Some(select_query) = SdblSelectQuery::cast(query_item) else {
            continue;
        };

        let Some(subquery) = select_query.subquery() else {
            tracing::debug!(select_index, "No subquery in SdblSelectQuery");
            select_index += 1;
            continue;
        };

        let has_union_siblings = subquery.union_clauses().next().is_some();
        let first_member = sdbl_queries.len();

        if let Some(main_query) = subquery.main_query() {
            ctx.scope.push_frame();

            let mut query_hir = ctx.lower_query(&main_query, has_union_siblings, true);
            ctx.lower_totals_by_clause(select_query.syntax(), &query_hir.select);
            // Диагностики, возникшие при лоуверинге ИТОГИ, иначе достанутся
            // следующему запросу пакета или потеряются на последнем.
            query_hir.diagnostics.extend(std::mem::take(&mut ctx.diagnostics));
            let range = select_query.syntax().text_range();

            tracing::debug!(
                select_index,
                query_index = 0,
                from_tables = query_hir.from.len(),
                join_tables = query_hir.joins.len(),
                range = ?range,
                "lowered main query"
            );

            ctx.scope.pop_frame();

            if let Some(ref temp_name) = query_hir.into_table {
                let temp_fields: Vec<crate::hir::FieldDef> = query_hir
                    .select
                    .fields
                    .iter()
                    .filter_map(|f| {
                        f.alias_or_name()
                            .map(|name| crate::hir::FieldDef::new(name.as_str(), f.ty.clone()))
                    })
                    .collect();

                tracing::debug!(
                    temp_table = %temp_name,
                    fields_count = temp_fields.len(),
                    "registering temporary table in scope for subsequent queries"
                );

                ctx.scope.add_temp_table(temp_name.to_string(), temp_fields);
            }

            sdbl_queries.push(crate::hir::SdblQuery { hir: query_hir, range });
        }

        for (union_index, union_clause) in subquery.union_clauses().enumerate() {
            ctx.record_keyword_by_text(
                union_clause.syntax(),
                "UNION",
                "ОБЪЕДИНИТЬ",
                crate::source_map::TokenCategory::Modifier,
            );

            if union_clause.has_all() {
                ctx.record_keyword_by_text(
                    union_clause.syntax(),
                    "ALL",
                    "ВСЕ",
                    crate::source_map::TokenCategory::Modifier,
                );
            } else {
                let union_range = union_clause
                    .syntax()
                    .children_with_tokens()
                    .filter_map(|it| it.into_token())
                    .find(|t| {
                        if t.kind() == syntax::SyntaxKind::IDENT {
                            let text = t.text();
                            text.eq_ignore_ascii_case("UNION")
                                || text.eq_ignore_ascii_case("ОБЪЕДИНИТЬ")
                        } else {
                            false
                        }
                    })
                    .map(|t| t.text_range())
                    .unwrap_or_else(|| union_clause.syntax().text_range());

                ctx.diagnostics.push(crate::diagnostics::SdblDiagnostic::UnionWithoutAll {
                    range: union_range,
                });
            }

            if let Some(union_query) = union_clause.query() {
                ctx.scope.push_frame();

                let query_hir = ctx.lower_query(&union_query, true, false);
                let range = union_query.syntax().text_range();

                tracing::debug!(
                    select_index,
                    union_index,
                    from_tables = query_hir.from.len(),
                    join_tables = query_hir.joins.len(),
                    range = ?range,
                    "lowered UNION query"
                );

                ctx.scope.pop_frame();

                if let Some(ref temp_name) = query_hir.into_table {
                    let temp_fields: Vec<crate::hir::FieldDef> = query_hir
                        .select
                        .fields
                        .iter()
                        .filter_map(|f| {
                            f.alias_or_name()
                                .map(|name| crate::hir::FieldDef::new(name.as_str(), f.ty.clone()))
                        })
                        .collect();

                    tracing::debug!(
                        temp_table = %temp_name,
                        fields_count = temp_fields.len(),
                        "registering temporary table in scope for subsequent queries"
                    );

                    ctx.scope.add_temp_table(temp_name.to_string(), temp_fields);
                }

                sdbl_queries.push(crate::hir::SdblQuery { hir: query_hir, range });
            }
        }

        collect_parameter_sources(
            &select_query,
            &sdbl_queries[first_member..],
            &mut parameter_sources,
        );

        select_index += 1;
    }

    if !has_query_item {
        tracing::debug!("No queries in package");
        return crate::hir::SdblPackage::empty();
    }

    ctx.source_map.finalize();

    crate::hir::SdblPackage { queries: sdbl_queries, parameter_sources, source_map: ctx.source_map }
}

/// Records the parameter sources of one batch statement. `INTO` and `TOTALS` belong to the
/// statement, not to the member naming the source: `INTO` in any `UNION` member puts the whole
/// statement into a temporary table. Nested queries are not walked.
fn collect_parameter_sources(
    select_query: &SdblSelectQuery,
    members: &[crate::hir::SdblQuery],
    out: &mut Vec<crate::hir::ParameterSourceUse>,
) {
    let statement_has_into = members.iter().any(|q| q.hir.into_table.is_some());
    let statement_totals = select_query
        .syntax()
        .children()
        .find(|n| n.kind() == syntax::SyntaxKind::SDBL_TOTALS_BY)
        .and_then(|totals| syntax::ast_utils::significant_tokens(&totals).next())
        .map(|keyword| keyword.text_range());

    for member in members {
        for table in member.hir.all_tables() {
            if let Some(source) = &table.parameter {
                out.push(crate::hir::ParameterSourceUse {
                    source: source.clone(),
                    statement_has_into,
                    statement_totals,
                });
            }
        }
    }
}

impl LoweringContext<'_> {
    pub(crate) fn lower_query(
        &mut self,
        query: &syntax::ast::SdblQuery,
        has_union_siblings: bool,
        check_field_aliases: bool,
    ) -> SdblHir {
        self.record_keyword_by_text(
            query.syntax(),
            "SELECT",
            "ВЫБРАТЬ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        let from = self.lower_from_clause(query.from_clause());

        for table in &from {
            if let Some(alias) = self.scope.add_table(table.clone()) {
                self.diagnostics.push(crate::diagnostics::SdblDiagnostic::DuplicateAlias {
                    alias,
                    range: table.range,
                });
            }
        }

        let joins = self.lower_joins(query);

        let (distinct, top, top_range) = self.extract_limitations(query.syntax());

        let select = self.lower_field_list(query.field_list(), distinct, top);

        let where_clause = query.where_clause().map(|w| self.lower_where_clause(&w));
        let having = query.having_clause().map(|h| self.lower_having_clause(&h));

        let group_by = query.group_by_clause().map(|g| self.lower_group_by(&g));

        let order_by = query.order_by_clause().map(|o| {
            self.push_select_outputs_frame(&select, false, o.syntax().text_range());
            let order_by = self.lower_order_by(&o);
            self.scope.pop_frame();
            order_by
        });

        self.lower_index_by_clause(query.syntax(), &select);

        let into_table = self.lower_into_clause(query.syntax());

        if let (Some(top_value), Some(range)) = (top, top_range) {
            let has_order_by = order_by.is_some();
            let has_where = where_clause.is_some();

            if has_union_siblings || !has_order_by {
                self.diagnostics.push(
                    crate::diagnostics::SdblDiagnostic::SelectTopWithoutOrderBy {
                        top_value,
                        in_union: has_union_siblings,
                        has_where,
                        range,
                    },
                );
            }
        }

        let range = query.syntax().text_range();

        let mut hir = SdblHir {
            select,
            into_table,
            from,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            unions: vec![],
            diagnostics: std::mem::take(&mut self.diagnostics),
            range,
        };

        self.check_joins_for_unprotected_fields(&hir, query);
        self.check_schedule_data_for_unprotected_fields(&hir);

        if check_field_aliases {
            self.check_alias_without_as_keyword(&hir);
        }

        self.check_nested_fields_by_dot(&hir);

        self.check_ref_overuse(&hir);

        self.check_unlimited_string_usage(&hir);

        hir.diagnostics.extend(std::mem::take(&mut self.diagnostics));

        hir
    }

    pub(super) fn lower_into_clause(
        &mut self,
        query_node: &syntax::SyntaxNode,
    ) -> Option<crate::hir::Name> {
        use syntax::SyntaxKind;

        let into_clause =
            query_node.children().find(|n| n.kind() == SyntaxKind::SDBL_INTO_CLAUSE)?;

        self.record_keyword_by_text(
            &into_clause,
            "INTO",
            "ПОМЕСТИТЬ",
            crate::source_map::TokenCategory::ClauseKeyword,
        );

        let temp_table_node =
            into_clause.children().find(|n| n.kind() == SyntaxKind::SDBL_TEMP_TABLE_NAME)?;

        let table_name = temp_table_node.children_with_tokens().find_map(|element| {
            element
                .as_token()
                .filter(|t| t.kind() == SyntaxKind::IDENT)
                .map(|t| t.text().to_string())
        })?;

        tracing::debug!(table_name = %table_name, "Extracted INTO clause temporary table name");

        Some(crate::hir::Name::from(table_name))
    }
}
