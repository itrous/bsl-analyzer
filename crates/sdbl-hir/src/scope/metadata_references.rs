use super::*;
use crate::hir::ResolvedTable;
use crate::metadata_reference::{
    MetadataReference, MetadataReferenceIssue, MetadataReferenceIssueKind,
};
use bsl_metadata::declaration_identity::DeclarationSegment;
use text_size::TextRange;

impl Scope<'_> {
    pub(super) fn column_source_tables(&self, column_name: &str) -> Vec<&TableRef> {
        for frame in self.frames.iter().rev() {
            if frame.outputs && Self::output_named(frame, column_name).is_some() {
                return Vec::new();
            }
            let mut result = frame
                .tables
                .values()
                .filter(|table| {
                    table.metadata.as_ref().is_some_and(|resolved| {
                        resolved.fields().iter().any(|field| field.matches_name(column_name))
                            && !resolved.column_is_provisional(column_name)
                    })
                })
                .collect::<Vec<_>>();
            if !result.is_empty() {
                result.sort_by_key(|table| table.effective_name().to_string());
                return result;
            }
            if frame.opaque {
                break;
            }
        }
        Vec::new()
    }

    /// Record the exact metadata path where the normal query scope resolved it.
    pub(crate) fn metadata_column_references(
        &self,
        tokens: &[(TextRange, String)],
    ) -> (Vec<MetadataReference>, Vec<MetadataReferenceIssue>) {
        let Some((_, head)) = tokens.first() else { return (Vec::new(), Vec::new()) };
        let qualified = tokens.len() > 1;
        let column_index = usize::from(qualified);
        let (column_range, column) = &tokens[column_index];
        let issue =
            |reason| (Vec::new(), vec![MetadataReferenceIssue { range: *column_range, reason }]);
        let table = if qualified {
            if !self.qualified_head_collision(head).is_empty() {
                return issue(MetadataReferenceIssueKind::Ambiguous);
            }
            let Some(table) = self.find_table(head) else {
                return issue(MetadataReferenceIssueKind::Unresolved);
            };
            table
        } else {
            let sources = self.column_source_tables(column);
            match sources.as_slice() {
                [table] => *table,
                [] => return issue(MetadataReferenceIssueKind::OutputAlias),
                _ => return issue(MetadataReferenceIssueKind::Ambiguous),
            }
        };
        let Some(metadata) = &table.metadata else {
            return issue(MetadataReferenceIssueKind::Unresolved);
        };
        let (mut owner_kind, mut owner_name) = match metadata {
            ResolvedTable::Metadata { mdo_type, name, .. }
            | ResolvedTable::Register { mdo_type, name, .. } => (*mdo_type, name.clone()),
            ResolvedTable::TempTable { .. } => {
                return issue(MetadataReferenceIssueKind::Unresolved)
            }
        };
        let mut fields = metadata.fields().to_vec();
        let mut prefix = Vec::new();
        if table.parts.len() == 3 && !matches!(metadata, ResolvedTable::Register { .. }) {
            prefix.push(DeclarationSegment {
                kind: "TabularSection".into(),
                name: table.parts[2].to_string(),
            });
        }
        let mut references = Vec::new();
        let mut issues = Vec::new();
        for (index, (range, name)) in tokens[column_index..].iter().enumerate() {
            let candidates =
                fields.iter().filter(|field| field.matches_name(name)).collect::<Vec<_>>();
            let field = match candidates.as_slice() {
                [field] => *field,
                _ => {
                    issues.push(MetadataReferenceIssue {
                        range: *range,
                        reason: if candidates.is_empty() {
                            MetadataReferenceIssueKind::Unresolved
                        } else {
                            MetadataReferenceIssueKind::Ambiguous
                        },
                    });
                    break;
                }
            };
            if field.provisional {
                issues.push(MetadataReferenceIssue {
                    range: *range,
                    reason: MetadataReferenceIssueKind::Unresolved,
                });
                break;
            }
            if field.source.is_some() {
                issues.push(MetadataReferenceIssue {
                    range: *range,
                    reason: MetadataReferenceIssueKind::DerivedField,
                });
            } else if !field.is_standard {
                let kind = if prefix.is_empty() {
                    match metadata {
                        ResolvedTable::Register { dimensions, resources, .. } if index == 0 => {
                            if dimensions.iter().any(|member| member.matches_name(name)) {
                                "Dimension"
                            } else if resources.iter().any(|member| member.matches_name(name)) {
                                "Resource"
                            } else {
                                "Attribute"
                            }
                        }
                        _ if matches!(field.ty, SdblType::TabularSectionRef { .. }) => {
                            "TabularSection"
                        }
                        _ => "Attribute",
                    }
                } else {
                    "Attribute"
                };
                let mut members = prefix.clone();
                members.push(DeclarationSegment { kind: kind.into(), name: field.name.clone() });
                references.push(MetadataReference {
                    range: *range,
                    owner_kind,
                    owner_name: owner_name.clone(),
                    members,
                });
            }
            if index + column_index + 1 == tokens.len() {
                break;
            }
            match &field.ty {
                SdblType::Ref(reference) => {
                    owner_kind = reference.mdo_type;
                    owner_name = reference.name.clone();
                    prefix.clear();
                    let Some(next) = self.resolve_ref_fields(reference) else {
                        issues.push(MetadataReferenceIssue {
                            range: *range,
                            reason: MetadataReferenceIssueKind::Unresolved,
                        });
                        break;
                    };
                    fields = next;
                }
                SdblType::TabularSectionRef { parent_mdo_type, parent_mdo_name, ts_name } => {
                    owner_kind = *parent_mdo_type;
                    owner_name = parent_mdo_name.clone();
                    prefix = vec![DeclarationSegment {
                        kind: "TabularSection".into(),
                        name: ts_name.clone(),
                    }];
                    let Some(next) = self.resolve_tabular_section_fields(
                        *parent_mdo_type,
                        parent_mdo_name,
                        ts_name,
                    ) else {
                        issues.push(MetadataReferenceIssue {
                            range: *range,
                            reason: MetadataReferenceIssueKind::Unresolved,
                        });
                        break;
                    };
                    fields = next;
                }
                _ => {
                    issues.push(MetadataReferenceIssue {
                        range: tokens[index + column_index + 1].0,
                        reason: MetadataReferenceIssueKind::Unresolved,
                    });
                    break;
                }
            }
        }
        (references, issues)
    }
}
