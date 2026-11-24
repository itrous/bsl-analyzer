use std::collections::HashSet;
use std::sync::Arc;
use stdx::case::CaseExt;

use crate::metadata_object::AttributeType;
use crate::{Configuration, MdoType, MetadataObject, Register};

pub trait MetadataResolver: std::fmt::Debug {
    /// The underlying type of the defined type `name`, or `None` if unknown.
    ///
    /// Returns an owned [`AttributeType`] (not a borrow) so a db-backed resolver
    /// can hand back a value composed fresh from a per-defined-type Salsa cell,
    /// rather than borrowing from a long-lived `Configuration`.
    fn resolve_defined_type(&self, name: &str) -> Option<AttributeType>;
}

impl MetadataResolver for Configuration {
    fn resolve_defined_type(&self, name: &str) -> Option<AttributeType> {
        self.find_defined_type(name).map(|dt| dt.underlying_type().clone())
    }
}

/// Metadata-object and register resolution surface for query (SDBL) lowering.
///
/// Returning owned `Arc`s (not borrows) lets a db-backed resolver hand back
/// per-MDO objects composed fresh from their own Salsa cells, so lowering a
/// query depends on just the metadata objects it references instead of the whole
/// `Configuration`. The [`Configuration`] impl serves the cold call sites (graph
/// build, streaming, tests) that still carry a whole config.
pub trait QueryMetadataResolver: MetadataResolver {
    fn resolve_metadata_object(&self, mdo_type: MdoType, name: &str)
        -> Option<Arc<MetadataObject>>;

    fn resolve_register(&self, mdo_type: MdoType, name: &str) -> Option<Arc<Register>>;
}

impl QueryMetadataResolver for Configuration {
    fn resolve_metadata_object(
        &self,
        mdo_type: MdoType,
        name: &str,
    ) -> Option<Arc<MetadataObject>> {
        self.find_metadata_object(mdo_type, name).cloned().map(Arc::new)
    }

    fn resolve_register(&self, mdo_type: MdoType, name: &str) -> Option<Arc<Register>> {
        self.find_register_by_type_and_name(mdo_type, name).cloned().map(Arc::new)
    }
}

pub fn resolve_defined_type_terminal<R>(
    resolver: &R,
    name: &str,
    visited: &mut HashSet<String>,
) -> Option<AttributeType>
where
    R: MetadataResolver + ?Sized,
{
    let mut current = name.to_string();
    loop {
        if !visited.insert(current.fold_lower()) {
            return None;
        }
        match resolver.resolve_defined_type(&current)? {
            AttributeType::DefinedType { name: next } => {
                current = next;
            }
            other => return Some(other),
        }
    }
}

/// Why a variant cannot be resolved within the caller's metadata scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeResolutionIssueKind {
    UnresolvedDefinedType,
    ReferencedTypeUnavailable,
    TechnicalNameUnavailable,
    LimitExceeded,
}

/// An unresolved leaf in the flattened type description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeResolutionIssue {
    /// Zero-based position in the resolved composite, or zero for a scalar description.
    pub variant_index: usize,
    pub kind: TypeResolutionIssueKind,
}

/// Resolve declared variants through the supplied visibility scope, preserving qualifiers.
/// Unavailable variants remain unknown; the caller chooses the expansion budget.
pub fn resolve_type_description<R: QueryMetadataResolver + ?Sized>(
    declared: &AttributeType,
    resolver: &R,
    max_nodes: usize,
) -> (AttributeType, Vec<TypeResolutionIssue>) {
    if max_nodes == 0 {
        return (
            AttributeType::Unknown,
            vec![TypeResolutionIssue {
                variant_index: 0,
                kind: TypeResolutionIssueKind::LimitExceeded,
            }],
        );
    }
    enum Pending {
        Type(AttributeType),
        Leave(String),
    }
    let limit = max_nodes;
    let mut admitted = 1usize;
    let mut pending = vec![Pending::Type(declared.clone())];
    let mut active = std::collections::HashSet::new();
    let mut values = Vec::new();
    let mut failures = Vec::new();
    let mut truncated = false;
    while let Some(next) = pending.pop() {
        let ty = match next {
            Pending::Type(ty) => ty,
            Pending::Leave(name) => {
                active.remove(&name);
                continue;
            }
        };
        match ty {
            AttributeType::Composite { types } => {
                let keep = types.len().min(limit.saturating_sub(admitted));
                truncated |= keep < types.len();
                admitted += keep;
                pending.extend(types.into_iter().take(keep).rev().map(Pending::Type));
            }
            AttributeType::DefinedType { name } => {
                let key = name.fold_lower();
                if active.contains(&key) {
                    failures.push((values.len(), TypeResolutionIssueKind::UnresolvedDefinedType));
                    values.push(AttributeType::Unknown);
                    continue;
                }
                let Some(underlying) = resolver.resolve_defined_type(&name) else {
                    failures.push((values.len(), TypeResolutionIssueKind::UnresolvedDefinedType));
                    values.push(AttributeType::Unknown);
                    continue;
                };
                if admitted >= limit {
                    truncated = true;
                    continue;
                }
                admitted += 1;
                active.insert(key.clone());
                pending.push(Pending::Leave(key));
                pending.push(Pending::Type(underlying));
            }
            AttributeType::Ref { mdo_type, ref name } => {
                if resolver.resolve_metadata_object(mdo_type, name).is_some()
                    || resolver.resolve_register(mdo_type, name).is_some()
                {
                    values.push(ty);
                } else {
                    failures
                        .push((values.len(), TypeResolutionIssueKind::ReferencedTypeUnavailable));
                    values.push(AttributeType::Unknown);
                }
            }
            AttributeType::InformationRegisterRecordManager { ref name } => {
                if resolver.resolve_register(MdoType::InformationRegister, name).is_some() {
                    values.push(ty);
                } else {
                    failures
                        .push((values.len(), TypeResolutionIssueKind::ReferencedTypeUnavailable));
                    values.push(AttributeType::Unknown);
                }
            }
            AttributeType::Unknown | AttributeType::UnknownNamed(_) => {
                failures.push((values.len(), TypeResolutionIssueKind::TechnicalNameUnavailable));
                values.push(AttributeType::Unknown);
            }
            _ => values.push(ty),
        }
    }
    if truncated {
        failures.push((values.len(), TypeResolutionIssueKind::LimitExceeded));
        values.push(AttributeType::Unknown);
    }
    if values.is_empty() {
        failures.push((0, TypeResolutionIssueKind::TechnicalNameUnavailable));
        values.push(AttributeType::Unknown);
    }
    let issues = failures
        .into_iter()
        .map(|(variant_index, kind)| TypeResolutionIssue { variant_index, kind })
        .collect();
    let resolved = if values.len() > 1 {
        AttributeType::Composite { types: values }
    } else {
        values.pop().unwrap()
    };
    (resolved, issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defined_type::DefinedType;
    use crate::Configuration;
    use uuid::Uuid;

    fn dt(name: &str, underlying: AttributeType) -> DefinedType {
        DefinedType::builder().uuid(Uuid::new_v4()).name(name).underlying_type(underlying).build()
    }

    fn cfg_with(types: Vec<DefinedType>) -> Configuration {
        let mut cfg = Configuration::new("Test");
        for t in types {
            cfg.add_defined_type(t);
        }
        cfg
    }

    #[test]
    fn configuration_resolves_existing_defined_type() {
        let cfg =
            cfg_with(vec![dt("ДенежнаяСумма", AttributeType::Number { precision: 15, scale: 2 })]);
        let underlying = cfg.resolve_defined_type("ДенежнаяСумма").expect("present");
        assert!(matches!(underlying, AttributeType::Number { .. }));
    }

    #[test]
    fn configuration_returns_none_for_unknown_name() {
        let cfg = cfg_with(Vec::new());
        assert!(cfg.resolve_defined_type("Несуществует").is_none());
    }

    #[test]
    fn terminal_walk_unwraps_chain() {
        let cfg = cfg_with(vec![
            dt("A", AttributeType::DefinedType { name: "B".to_string() }),
            dt("B", AttributeType::Number { precision: 10, scale: 0 }),
        ]);
        let mut visited = HashSet::new();
        let underlying = resolve_defined_type_terminal(&cfg, "A", &mut visited)
            .expect("chain must terminate at Number");
        assert!(matches!(underlying, AttributeType::Number { .. }));
        assert!(visited.contains("a"));
        assert!(visited.contains("b"));
    }

    #[test]
    fn terminal_walk_breaks_simple_cycle() {
        let cfg = cfg_with(vec![dt("A", AttributeType::DefinedType { name: "A".to_string() })]);
        let mut visited = HashSet::new();
        assert!(resolve_defined_type_terminal(&cfg, "A", &mut visited).is_none());
    }

    #[test]
    fn terminal_walk_breaks_indirect_cycle() {
        let cfg = cfg_with(vec![
            dt("A", AttributeType::DefinedType { name: "B".to_string() }),
            dt("B", AttributeType::DefinedType { name: "A".to_string() }),
        ]);
        let mut visited = HashSet::new();
        assert!(resolve_defined_type_terminal(&cfg, "A", &mut visited).is_none());
    }

    #[test]
    fn terminal_walk_returns_none_when_chain_breaks() {
        let cfg =
            cfg_with(vec![dt("A", AttributeType::DefinedType { name: "Missing".to_string() })]);
        let mut visited = HashSet::new();
        assert!(resolve_defined_type_terminal(&cfg, "A", &mut visited).is_none());
    }

    #[test]
    fn terminal_walk_lookup_is_case_insensitive() {
        let cfg = cfg_with(vec![dt("ДенежнаяСумма", AttributeType::Boolean)]);
        let mut visited = HashSet::new();
        let underlying = resolve_defined_type_terminal(&cfg, "денежнаясумма", &mut visited)
            .expect("case-insensitive lookup");
        assert_eq!(underlying, AttributeType::Boolean);
    }
    #[test]
    fn type_description_resolution_preserves_nested_aliases_and_reports_cycle_reference_and_limit()
    {
        let cfg = cfg_with(vec![
            dt(
                "A",
                AttributeType::Composite {
                    types: vec![
                        AttributeType::DefinedType { name: "B".into() },
                        AttributeType::DefinedType { name: "Loop".into() },
                        AttributeType::Ref { mdo_type: MdoType::Catalog, name: "Missing".into() },
                    ],
                },
            ),
            dt("B", AttributeType::Number { precision: 9, scale: 2 }),
            dt("Loop", AttributeType::DefinedType { name: "LOOP".into() }),
        ]);
        let declared = AttributeType::DefinedType { name: "A".into() };
        let (resolved, issues) = resolve_type_description(&declared, &cfg, 64);
        assert_eq!(
            resolved,
            AttributeType::Composite {
                types: vec![
                    AttributeType::Number { precision: 9, scale: 2 },
                    AttributeType::Unknown,
                    AttributeType::Unknown
                ]
            }
        );
        assert_eq!(
            issues,
            vec![
                TypeResolutionIssue {
                    variant_index: 1,
                    kind: TypeResolutionIssueKind::UnresolvedDefinedType
                },
                TypeResolutionIssue {
                    variant_index: 2,
                    kind: TypeResolutionIssueKind::ReferencedTypeUnavailable
                }
            ]
        );
        for limit in [0, 1] {
            let (resolved, issues) = resolve_type_description(&declared, &cfg, limit);
            assert_eq!(resolved, AttributeType::Unknown);
            assert_eq!(
                issues,
                vec![TypeResolutionIssue {
                    variant_index: 0,
                    kind: TypeResolutionIssueKind::LimitExceeded
                }]
            );
        }
    }
}
