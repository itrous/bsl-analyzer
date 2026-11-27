use sdbl_hir::{ExprHir, ResolvedTable, SdblHir, SdblPackage};
use syntax::TextRange;

#[derive(Debug, Clone, Default)]
pub(crate) struct QueryOutputPolicy {
    pub preserve_aliases: Vec<(TextRange, String)>,
    pub incomplete: Vec<TextRange>,
}

/// An implicit field name is an output contract, independently of its metadata origin.
pub(crate) fn metadata_query_output_policy(
    package: &SdblPackage,
    field_references: &[TextRange],
    container_references: &[TextRange],
    object_target: bool,
) -> QueryOutputPolicy {
    let mut policy = QueryOutputPolicy::default();
    let mut covered_sources = Vec::new();
    for query in package.queries() {
        visit_query(
            &query.hir,
            field_references,
            container_references,
            object_target,
            &mut covered_sources,
            &mut policy,
        );
    }
    policy.incomplete.extend(
        container_references
            .iter()
            .copied()
            .filter(|reference| !covered_sources.contains(reference)),
    );
    policy.preserve_aliases.sort_by_key(|(range, name)| (range.start(), range.end(), name.clone()));
    policy.preserve_aliases.dedup();
    policy.incomplete.sort_by_key(|range| (range.start(), range.end()));
    policy.incomplete.dedup();
    policy
}

fn visit_query(
    query: &SdblHir,
    references: &[TextRange],
    containers: &[TextRange],
    object_target: bool,
    covered: &mut Vec<TextRange>,
    policy: &mut QueryOutputPolicy,
) {
    let target_tables = query
        .all_tables()
        .filter(|table| {
            matches!(
                table.metadata.as_ref(),
                Some(ResolvedTable::Metadata { .. } | ResolvedTable::Register { .. })
            ) && containers.iter().any(|reference| table.range.contains_range(*reference))
        })
        .collect::<Vec<_>>();
    covered.extend(containers.iter().copied().filter(|reference| {
        target_tables.iter().any(|table| table.range.contains_range(*reference))
    }));
    if object_target {
        for table in &target_tables {
            if table.alias.is_some() {
                continue;
            }
            let matching = references
                .iter()
                .filter(|reference| table.range.contains_range(**reference))
                .copied()
                .collect::<Vec<_>>();
            if table.parts.len() == 2
                && !table.is_virtual_table
                && matching.len() == 1
                && crate::rename::is_valid_identifier(table.binding_name())
            {
                policy.preserve_aliases.push((matching[0], table.binding_name().to_string()));
            } else {
                policy.incomplete.push(table.range);
            }
        }
    } else {
        for field in &query.select.fields {
            if field.is_asterisk
                && target_tables.iter().any(|table| {
                    field.asterisk_qualifier.as_ref().is_none_or(|qualifier| {
                        stdx::case::eq_ignore_case(qualifier, table.binding_name())
                    })
                })
            {
                policy.incomplete.push(field.range);
                continue;
            }
            if field.alias.is_some() {
                continue;
            }
            let ExprHir::ColumnRef { parts, range, .. } = &field.expr else { continue };
            let Some(name) = parts.last() else { continue };
            if !crate::rename::is_valid_identifier(name.as_str()) {
                if references.iter().any(|reference| range.contains_range(*reference)) {
                    policy.incomplete.push(*range);
                }
                continue;
            }
            if let Some(reference) = references.iter().find(|reference| {
                reference.end() == range.end() && range.contains_range(**reference)
            }) {
                policy.preserve_aliases.push((*reference, name.to_string()));
            }
        }
    }
    for table in query.all_tables() {
        for subquery in &table.subquery {
            visit_query(subquery, references, containers, object_target, covered, policy);
        }
    }
    for union in &query.unions {
        visit_query(&union.query, references, containers, object_target, covered, policy);
    }
}
