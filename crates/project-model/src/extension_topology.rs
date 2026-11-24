//! Normalized dependency topology of configuration extensions (CFE).
//!
//! The project config declares extensions either as bare path strings (legacy,
//! always independent) or as structured entries with a stable `name` and
//! directed `dependsOn` edges. This module owns the domain model those
//! declarations normalize into: validated nodes, a deterministic topological
//! order, per-node transitive dependency closures, and a byte-stable
//! fingerprint that changes whenever the topology's identity (paths, names,
//! edges, or order) changes.
//!
//! The model is deliberately independent of the configuration syntax so a
//! pre-validated graph from an external build tool can later be adopted
//! without touching consumers.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use stdx::case::fold_lower_per_char;

/// Bumped whenever a digest of the new recipe could equal a digest of an
/// older one for a topology with different semantics, so persisted identities
/// derived from a fingerprint can never match across incompatible formats. A
/// format bump also invalidates topology-derived builds made before the
/// extension-register overlay semantics changed.
/// A recipe change confined to nodes whose own bytes change with it (the
/// visibility mode of external objects) needs no bump: every such digest is
/// new, and the unchanged nodes still digest as before.
pub const TOPOLOGY_FORMAT_VERSION: u32 = 4;

/// Index of a node inside [`ExtensionTopology`]; stable for the lifetime of
/// the topology value it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

pub use bsl_metadata::ExternalObjectKind;

/// The kind of a topology node. An extension takes part in `dependsOn` edges and
/// overlays the base for its dependents; an external object is a leaf: it may
/// depend on extensions, which narrows what it sees, but nothing depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Extension,
    External(ExternalObjectKind),
}

impl NodeKind {
    pub fn is_external(self) -> bool {
        matches!(self, Self::External(_))
    }

    /// One byte per kind for the fingerprint; the kind is part of the node's
    /// identity because the same path declared as an extension and as an
    /// external object composes differently.
    fn fingerprint_tag(self) -> u8 {
        match self {
            Self::Extension => 0,
            Self::External(ExternalObjectKind::DataProcessor) => 1,
            Self::External(ExternalObjectKind::Report) => 2,
        }
    }
}

/// Raw material for one extension node, produced by the config resolver.
#[derive(Debug, Clone)]
pub struct ExtensionNodeSpec {
    pub kind: NodeKind,
    pub name: String,
    /// Path as configured/expanded — what consumers watch and scan.
    pub path: PathBuf,
    /// Canonicalized identity path — what dedup and the fingerprint use.
    pub canonical_path: PathBuf,
    /// Direct dependency names as declared (`dependsOn`); `None` when the
    /// declaration carried no such key. For an extension the two spell the same
    /// graph; for an external object they do not: without the key it sees every
    /// extension, with it exactly the declared closure — an empty one is the base
    /// alone.
    pub depends_on: Option<Vec<String>>,
    /// Declared installation scope for an extension. Legacy/discovered
    /// candidates remain unset until project exclusions have been applied.
    pub scope: Option<bsl_metadata::ExtensionScope>,
    /// The name is a user-visible identity, unique among roots: duplicate names
    /// are errors and the node may own a baseline partition. Legacy string
    /// entries are not — they keep their historical lenient semantics.
    pub structured: bool,
}

#[derive(Debug, Clone)]
pub struct ExtensionNode {
    kind: NodeKind,
    name: String,
    path: PathBuf,
    canonical_path: PathBuf,
    depends_on: Vec<NodeId>,
    closure: Vec<NodeId>,
    sees_every_extension: bool,
    structured: bool,
    scope: Option<bsl_metadata::ExtensionScope>,
}

impl ExtensionNode {
    pub fn kind(&self) -> NodeKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn depends_on(&self) -> &[NodeId] {
        &self.depends_on
    }

    /// Ordered transitive dependencies (global topological order, diamond
    /// dependencies included once). The node itself is *not* part of its
    /// closure: visibility composition appends it last. For an external object
    /// declared without `dependsOn` this is every extension, in topological
    /// order: the object runs in the infobase the project describes, extensions
    /// attached.
    pub fn closure(&self) -> &[NodeId] {
        &self.closure
    }

    /// True for an external object whose closure is every extension because
    /// it declared no `dependsOn`, as opposed to one that narrowed its view.
    /// Always false for an extension.
    pub fn sees_every_extension(&self) -> bool {
        self.sees_every_extension
    }

    pub fn is_structured(&self) -> bool {
        self.structured
    }

    pub fn scope(&self) -> Option<bsl_metadata::ExtensionScope> {
        self.scope
    }
}

/// Byte-stable identity of a topology. Any change to the base path, node set,
/// node names, canonical paths, edges, or order produces a different value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TopologyFingerprint([u8; 32]);

impl TopologyFingerprint {
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyError {
    InvalidExternalConfigurationOwner {
        name: String,
        reason: String,
    },
    EmptyName,
    DuplicateName {
        name: String,
    },
    DuplicatePath {
        path: PathBuf,
        first: String,
        second: String,
    },
    UnknownDependency {
        from: String,
        name: String,
    },
    SelfReference {
        name: String,
    },
    DuplicateEdge {
        from: String,
        name: String,
    },
    /// The dependency name exists but is ambiguous: several legacy entries
    /// share it, so an edge cannot pick a target.
    AmbiguousDependency {
        from: String,
        name: String,
    },
    Cycle {
        path: Vec<String>,
    },
    GlobInStructuredEntry {
        name: String,
        pattern: String,
    },
    StructuredPathMissing {
        name: String,
        path: PathBuf,
    },
    StructuredNotAnExtension {
        name: String,
        path: PathBuf,
    },
    /// A `dependsOn` edge pointing at an external object.
    ExternalInDependency {
        from: String,
        name: String,
    },
    ExternalPathMissing {
        name: String,
        path: PathBuf,
    },
    /// The declared external root carries `Configuration.xml`: it is a
    /// configuration or an extension declared under the wrong key.
    ExternalIsAConfiguration {
        name: String,
        path: PathBuf,
    },
    ExternalNoObjectXml {
        name: String,
        path: PathBuf,
    },
    ExternalAmbiguousObjectXml {
        name: String,
        path: PathBuf,
    },
    /// The object XML is there but its `MetaDataObject` carries no element at
    /// all: an export truncated before its object.
    ExternalObjectElementMissing {
        name: String,
        path: PathBuf,
    },
    /// The object XML is there but its element is not an external object's.
    ExternalNotAnExternalObject {
        name: String,
        path: PathBuf,
        tag: String,
    },
    /// `<Name>.xml` is there but the `<Name>/` directory the export keeps its
    /// modules, forms and templates in is not: a copy that stopped halfway.
    ExternalObjectDirMissing {
        name: String,
        path: PathBuf,
        object: String,
    },
    /// Two external roots export one object: every surface keyed by
    /// `(MdoType, name)` — the graph, the MCP name dictionary — would keep only
    /// the first, so the pair is refused by name instead of losing one silently.
    ExternalObjectNameDuplicate {
        object: String,
        first: String,
        second: String,
    },
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TopologyError::InvalidExternalConfigurationOwner { name, reason } => {
                write!(f, "invalid configuration owner for external {name}: {reason}")
            }
            TopologyError::EmptyName => {
                write!(f, "extension entry has an empty name")
            }
            TopologyError::DuplicateName { name } => write!(
                f,
                "duplicate root name: {name}; extensions and external objects share one \
                 namespace"
            ),
            TopologyError::DuplicatePath { path, first, second } => {
                write!(
                    f,
                    "extensions '{first}' and '{second}' resolve to the same path: {}",
                    path.display()
                )
            }
            TopologyError::UnknownDependency { from, name } => {
                write!(f, "'{from}' depends on unknown extension '{name}'")
            }
            TopologyError::SelfReference { name } => {
                write!(f, "'{name}' depends on itself")
            }
            TopologyError::DuplicateEdge { from, name } => {
                write!(f, "'{from}' declares a duplicate dependency on '{name}'")
            }
            TopologyError::AmbiguousDependency { from, name } => {
                write!(f, "'{from}' depends on '{name}', which several extensions share as a name")
            }
            TopologyError::Cycle { path } => {
                write!(f, "extension dependency cycle: {}", path.join(" -> "))
            }
            TopologyError::GlobInStructuredEntry { name, pattern } => {
                write!(
                    f,
                    "extension '{name}': glob patterns are not allowed in a named entry's path: {pattern}"
                )
            }
            TopologyError::StructuredPathMissing { name, path } => {
                write!(f, "extension '{name}': path not found: {}", path.display())
            }
            TopologyError::StructuredNotAnExtension { name, path } => {
                write!(f, "extension '{name}': no Configuration.xml under {}", path.display())
            }
            TopologyError::ExternalInDependency { from, name } => write!(
                f,
                "'{from}' depends on '{name}', but an external data processor or report \
                 is seen by none: it may depend on extensions, nothing may depend on it"
            ),
            TopologyError::ExternalPathMissing { name, path } => {
                write!(f, "external '{name}': no directory at {}", path.display())
            }
            TopologyError::ExternalIsAConfiguration { name, path } => write!(
                f,
                "external '{name}': {} carries Configuration.xml, so it is a configuration \
                 or an extension; declare it under [source].root or [source].extensions",
                path.display()
            ),
            TopologyError::ExternalNoObjectXml { name, path } => write!(
                f,
                "external '{name}': no <Name>.xml of an external data processor or report \
                 directly under {}",
                path.display()
            ),
            TopologyError::ExternalAmbiguousObjectXml { name, path } => write!(
                f,
                "external '{name}': several object .xml files directly under {}; an \
                 external root holds exactly one export",
                path.display()
            ),
            TopologyError::ExternalObjectElementMissing { name, path } => write!(
                f,
                "external '{name}': the object XML under {} carries no element inside \
                 MetaDataObject; the export stopped before its object",
                path.display()
            ),
            TopologyError::ExternalNotAnExternalObject { name, path, tag } => write!(
                f,
                "external '{name}': {} describes a <{tag}>, not an ExternalDataProcessor \
                 or ExternalReport",
                path.display()
            ),
            TopologyError::ExternalObjectDirMissing { name, path, object } => write!(
                f,
                "external '{name}': {object}.xml under {} has no {object}/ directory beside \
                 it; an export keeps the object's modules and forms there",
                path.display()
            ),
            TopologyError::ExternalObjectNameDuplicate { object, first, second } => write!(
                f,
                "externals '{first}' and '{second}' both export the object '{object}'; one \
                 project keeps one export per object name"
            ),
        }
    }
}

impl std::error::Error for TopologyError {}

#[derive(Debug, Clone)]
pub struct ExtensionTopology {
    /// Nodes in expanded declaration order — the order `extension_paths()`
    /// exposes and overlay composition relies on.
    nodes: Vec<ExtensionNode>,
    /// Dependencies-before-dependents order; ties broken by declaration order.
    topo_order: Vec<NodeId>,
    fingerprint: TopologyFingerprint,
}

impl ExtensionTopology {
    /// Validates the specs and builds the normalized model. `base_path` is the
    /// canonical base-configuration root, folded into the fingerprint because
    /// the same extension set over a different base is a different project.
    pub fn build(
        base_path: &Path,
        specs: Vec<ExtensionNodeSpec>,
    ) -> Result<ExtensionTopology, TopologyError> {
        // Name table. Legacy entries may share a (derived) name — historical
        // behavior that stays warning-only as long as nothing depends on the
        // ambiguous name. Structured entries must be unique outright.
        let mut by_name: HashMap<String, NameSlot> = HashMap::new();
        for (idx, spec) in specs.iter().enumerate() {
            if spec.name.trim().is_empty() {
                return Err(TopologyError::EmptyName);
            }
            let key = fold_lower_per_char(&spec.name);
            match by_name.entry(key) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(NameSlot::Unique(idx));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let first_structured = match *entry.get() {
                        NameSlot::Unique(first) => specs[first].structured,
                        NameSlot::Ambiguous => false,
                    };
                    if spec.structured || first_structured {
                        return Err(TopologyError::DuplicateName { name: spec.name.clone() });
                    }
                    tracing::warn!(
                        name = %spec.name,
                        "several legacy extension entries share a name; \
                         they stay independent and cannot be dependency targets"
                    );
                    entry.insert(NameSlot::Ambiguous);
                }
            }
        }

        // Resolve declared edges to node ids.
        let mut edges: Vec<Vec<NodeId>> = Vec::with_capacity(specs.len());
        for (idx, spec) in specs.iter().enumerate() {
            let mut resolved: Vec<NodeId> = Vec::new();
            for dep_name in spec.depends_on.iter().flatten() {
                let key = fold_lower_per_char(dep_name);
                let target = match by_name.get(&key) {
                    None => {
                        return Err(TopologyError::UnknownDependency {
                            from: spec.name.clone(),
                            name: dep_name.clone(),
                        });
                    }
                    Some(NameSlot::Ambiguous) => {
                        return Err(TopologyError::AmbiguousDependency {
                            from: spec.name.clone(),
                            name: dep_name.clone(),
                        });
                    }
                    Some(NameSlot::Unique(target)) => *target,
                };
                // Self-reference first: an external object pointing at itself
                // is a self-reference like any other, not a foreign external
                // target.
                if target == idx {
                    return Err(TopologyError::SelfReference { name: spec.name.clone() });
                }
                if specs[target].kind.is_external() {
                    return Err(TopologyError::ExternalInDependency {
                        from: spec.name.clone(),
                        name: dep_name.clone(),
                    });
                }
                let id = NodeId(target as u32);
                if resolved.contains(&id) {
                    return Err(TopologyError::DuplicateEdge {
                        from: spec.name.clone(),
                        name: dep_name.clone(),
                    });
                }
                resolved.push(id);
            }
            edges.push(resolved);
        }

        let topo_order = topological_order(&specs, &edges)?;

        // Transitive closures in topological order: a node's closure is the
        // union of its direct dependencies and their closures, ordered by the
        // global topological rank, so a diamond contributes each node once.
        let mut rank = vec![0usize; specs.len()];
        for (pos, id) in topo_order.iter().enumerate() {
            rank[id.index()] = pos;
        }
        let mut closures: Vec<Vec<NodeId>> = vec![Vec::new(); specs.len()];
        for id in &topo_order {
            let mut closure: Vec<NodeId> = Vec::new();
            for dep in &edges[id.index()] {
                for transitive in &closures[dep.index()] {
                    if !closure.contains(transitive) {
                        closure.push(*transitive);
                    }
                }
                if !closure.contains(dep) {
                    closure.push(*dep);
                }
            }
            closure.sort_by_key(|n| rank[n.index()]);
            closures[id.index()] = closure;
        }

        // An external object that declared no `dependsOn` records no edge, yet
        // sees every extension: the visibility rule lives here, not in the
        // consumers, so that every reader of `closure()` agrees.
        let every_extension: Vec<NodeId> =
            topo_order.iter().copied().filter(|id| !specs[id.index()].kind.is_external()).collect();
        let sees_every_extension =
            |spec: &ExtensionNodeSpec| spec.kind.is_external() && spec.depends_on.is_none();
        for (idx, spec) in specs.iter().enumerate() {
            if sees_every_extension(spec) {
                closures[idx] = every_extension.clone();
            }
        }

        let nodes: Vec<ExtensionNode> = specs
            .into_iter()
            .zip(edges)
            .zip(closures)
            .map(|((spec, depends_on), closure)| ExtensionNode {
                sees_every_extension: sees_every_extension(&spec),
                kind: spec.kind,
                name: spec.name,
                path: spec.path,
                canonical_path: spec.canonical_path,
                depends_on,
                closure,
                structured: spec.structured,
                scope: spec.scope,
            })
            .collect();

        let fingerprint = fingerprint(base_path, &nodes);
        Ok(ExtensionTopology { nodes, topo_order, fingerprint })
    }

    /// Nodes in expanded declaration order.
    pub fn nodes(&self) -> &[ExtensionNode] {
        &self.nodes
    }

    pub fn node(&self, id: NodeId) -> &ExtensionNode {
        &self.nodes[id.index()]
    }

    pub fn node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        (0..self.nodes.len() as u32).map(NodeId)
    }

    /// Dependencies-before-dependents; ties follow declaration order, so a
    /// graph without edges keeps the declared order exactly.
    pub fn topological_order(&self) -> &[NodeId] {
        &self.topo_order
    }

    pub fn fingerprint(&self) -> TopologyFingerprint {
        self.fingerprint
    }

    /// The same versioned topology encoding used by [`Self::fingerprint`], with
    /// paths represented in the workspace's durable address space. Roots inside
    /// the workspace become relative; external roots retain their absolute
    /// identity. Keeping this method on the topology makes the graph cache use
    /// exactly the dependency, declaration-order, visibility and node encoding
    /// that the project model uses for its physical fingerprint.
    pub fn portable_fingerprint(
        &self,
        base_path: &Path,
        workspace_root: &Path,
    ) -> TopologyFingerprint {
        let workspace =
            std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
        let portable = |path: &Path| {
            let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            path.strip_prefix(&workspace)
                .map(|relative| relative.as_os_str().as_encoded_bytes().to_vec())
                .unwrap_or_else(|_| path.as_os_str().as_encoded_bytes().to_vec())
        };
        fingerprint_with_paths(&portable(base_path), &self.nodes, portable)
    }

    /// The topology without the nodes `keep` rejects.
    ///
    /// Validation already ran over the full declaration, so a dependency on a removed
    /// node was legal; it is dropped from every edge and closure rather than kept as a
    /// way back into the removed source. The survivors keep their relative declaration
    /// and topological order and are renumbered densely. The fingerprint is taken over
    /// what remains, because that is what the project now composes.
    pub fn retain(
        &self,
        base_path: &Path,
        mut keep: impl FnMut(&ExtensionNode) -> bool,
    ) -> ExtensionTopology {
        let mut renumbered: Vec<Option<NodeId>> = Vec::with_capacity(self.nodes.len());
        let mut next = 0u32;
        for node in &self.nodes {
            if keep(node) {
                renumbered.push(Some(NodeId(next)));
                next += 1;
            } else {
                renumbered.push(None);
            }
        }
        let remap =
            |ids: &[NodeId]| ids.iter().filter_map(|id| renumbered[id.index()]).collect::<Vec<_>>();
        let nodes: Vec<ExtensionNode> = self
            .nodes
            .iter()
            .zip(&renumbered)
            .filter(|(_, id)| id.is_some())
            .map(|(node, _)| ExtensionNode {
                depends_on: remap(&node.depends_on),
                closure: remap(&node.closure),
                ..node.clone()
            })
            .collect();
        let topo_order = remap(&self.topo_order);
        let fingerprint = fingerprint(base_path, &nodes);
        ExtensionTopology { nodes, topo_order, fingerprint }
    }

    pub fn has_dependencies(&self) -> bool {
        self.nodes.iter().any(|node| !node.depends_on.is_empty())
    }
}

#[derive(Clone, Copy)]
enum NameSlot {
    Unique(usize),
    Ambiguous,
}

/// Kahn's algorithm with a deterministic tie-breaker: among ready nodes every
/// extension goes before every external object, and within a kind the smallest
/// declaration index goes first. External objects are leaves over the whole
/// extension set, so their place is after it whatever the declaration order.
/// On a cycle, walks the leftover nodes to report one concrete cycle path.
fn topological_order(
    specs: &[ExtensionNodeSpec],
    edges: &[Vec<NodeId>],
) -> Result<Vec<NodeId>, TopologyError> {
    let n = specs.len();
    let mut remaining_deps: Vec<usize> = edges.iter().map(Vec::len).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (idx, deps) in edges.iter().enumerate() {
        for dep in deps {
            dependents[dep.index()].push(idx);
        }
    }

    let slot = |idx: usize| (specs[idx].kind.is_external(), idx);
    let mut ready: std::collections::BTreeSet<(bool, usize)> =
        (0..n).filter(|&idx| remaining_deps[idx] == 0).map(slot).collect();
    let mut order: Vec<NodeId> = Vec::with_capacity(n);
    while let Some((_, idx)) = ready.pop_first() {
        order.push(NodeId(idx as u32));
        for &dependent in &dependents[idx] {
            remaining_deps[dependent] -= 1;
            if remaining_deps[dependent] == 0 {
                ready.insert(slot(dependent));
            }
        }
    }

    if order.len() == n {
        return Ok(order);
    }

    // Every leftover node sits on or downstream of a cycle. Walking first
    // unvisited dependencies from the smallest leftover index must revisit a
    // node — that revisit closes the reported cycle.
    let leftover: Vec<usize> = (0..n).filter(|&idx| remaining_deps[idx] > 0).collect();
    let start = leftover[0];
    let mut seen_at: HashMap<usize, usize> = HashMap::new();
    let mut path: Vec<usize> = Vec::new();
    let mut current = start;
    loop {
        if let Some(&pos) = seen_at.get(&current) {
            let mut cycle: Vec<String> =
                path[pos..].iter().map(|&idx| specs[idx].name.clone()).collect();
            cycle.push(specs[current].name.clone());
            return Err(TopologyError::Cycle { path: cycle });
        }
        seen_at.insert(current, path.len());
        path.push(current);
        current = edges[current]
            .iter()
            .map(|id| id.index())
            .find(|idx| remaining_deps[*idx] > 0)
            .expect("a node left by Kahn's algorithm keeps an unresolved dependency");
    }
}

/// Domain-separated, versioned, length-prefixed digest over the normalized
/// model. Names enter case-folded (the identity the resolver compares by);
/// paths enter as their platform-encoded bytes (injective, unlike a lossy
/// UTF-8 conversion). Nodes hash in declaration order, which — together with
/// the per-node edges — fully determines the derived topological order, so
/// reordering declarations or touching any edge changes the digest: declared
/// order is part of overlay precedence and therefore of project identity.
///
/// An external object also hashes its visibility mode: with no edges, "every
/// extension" and "no extension" would otherwise digest alike while composing
/// differently. Extension nodes hash exactly as before the mode existed, so
/// the digest of a project without external objects is unchanged.
fn fingerprint(base_path: &Path, nodes: &[ExtensionNode]) -> TopologyFingerprint {
    fingerprint_with_paths(base_path.as_os_str().as_encoded_bytes(), nodes, |path| {
        path.as_os_str().as_encoded_bytes().to_vec()
    })
}

fn fingerprint_with_paths<F>(
    base_path: &[u8],
    nodes: &[ExtensionNode],
    mut path: F,
) -> TopologyFingerprint
where
    F: FnMut(&Path) -> Vec<u8>,
{
    let mut hasher = blake3::Hasher::new_derive_key("bsl-analyzer/extension-topology/v1");
    let field = |hasher: &mut blake3::Hasher, bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    hasher.update(&TOPOLOGY_FORMAT_VERSION.to_le_bytes());
    field(&mut hasher, base_path);
    hasher.update(&(nodes.len() as u64).to_le_bytes());
    for node in nodes {
        hasher.update(&[node.kind.fingerprint_tag()]);
        if node.kind.is_external() {
            hasher.update(&[u8::from(!node.sees_every_extension)]);
        }
        field(&mut hasher, fold_lower_per_char(&node.name).as_bytes());
        field(&mut hasher, &path(&node.canonical_path));
        let mut deps: Vec<String> = node
            .depends_on
            .iter()
            .map(|dep| fold_lower_per_char(&nodes[dep.index()].name))
            .collect();
        deps.sort();
        hasher.update(&(deps.len() as u64).to_le_bytes());
        for dep in &deps {
            field(&mut hasher, dep.as_bytes());
        }
        hasher.update(&[match node.scope {
            None => 0,
            Some(bsl_metadata::ExtensionScope::InfoBase) => 1,
            Some(bsl_metadata::ExtensionScope::DataSeparation) => 2,
        }]);
    }
    TopologyFingerprint(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, deps: &[&str]) -> ExtensionNodeSpec {
        ExtensionNodeSpec {
            kind: NodeKind::Extension,
            name: name.to_string(),
            path: PathBuf::from(format!("/ws/{name}")),
            canonical_path: PathBuf::from(format!("/ws/{name}")),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            structured: true,
            scope: Some(bsl_metadata::ExtensionScope::InfoBase),
        }
    }

    fn legacy_spec(name: &str) -> ExtensionNodeSpec {
        ExtensionNodeSpec { structured: false, depends_on: None, scope: None, ..spec(name, &[]) }
    }

    /// An external object; `deps` of `None` is a declaration without `dependsOn`.
    fn external_spec(name: &str, deps: Option<&[&str]>) -> ExtensionNodeSpec {
        ExtensionNodeSpec {
            kind: NodeKind::External(ExternalObjectKind::DataProcessor),
            depends_on: deps.map(|deps| deps.iter().map(|s| s.to_string()).collect()),
            scope: None,
            ..spec(name, &[])
        }
    }

    fn build(specs: Vec<ExtensionNodeSpec>) -> Result<ExtensionTopology, TopologyError> {
        ExtensionTopology::build(Path::new("/ws/base"), specs)
    }

    fn names(topology: &ExtensionTopology, ids: &[NodeId]) -> Vec<String> {
        ids.iter().map(|id| topology.node(*id).name().to_string()).collect()
    }

    /// A workspace moved as a whole keeps its portable topology — external objects included,
    /// since discovery finds them under the workspace — while an external object that moves
    /// inside the workspace still changes it.
    #[test]
    fn portable_fingerprint_survives_moving_the_workspace_with_external_objects() {
        let topology_at = |workspace: &str, external: &str| {
            let at = |relative: &str| PathBuf::from(format!("{workspace}/{relative}"));
            let specs = vec![
                ExtensionNodeSpec {
                    path: at("cfe/one"),
                    canonical_path: at("cfe/one"),
                    ..spec("one", &[])
                },
                ExtensionNodeSpec {
                    path: at(external),
                    canonical_path: at(external),
                    ..external_spec("Обработка", None)
                },
            ];
            let base = at("cf");
            let topology = ExtensionTopology::build(&base, specs).unwrap();
            topology.portable_fingerprint(&base, Path::new(workspace))
        };

        assert_eq!(
            topology_at("/old-ws", "src/epf/Обработка"),
            topology_at("/new-ws", "src/epf/Обработка"),
        );
        assert_ne!(
            topology_at("/old-ws", "src/epf/Обработка"),
            topology_at("/old-ws", "epf/Обработка"),
        );
    }

    #[test]
    fn no_edges_keeps_declaration_order() {
        let topology = build(vec![spec("B", &[]), spec("A", &[]), legacy_spec("C")]).unwrap();
        let order = names(&topology, topology.topological_order());
        assert_eq!(order, ["B", "A", "C"], "without edges the declared order is the topo order");
        for node in topology.nodes() {
            assert!(node.closure().is_empty());
        }
    }

    #[test]
    fn chain_closure_lists_transitive_dependencies_in_order() {
        let topology = build(vec![
            spec("TESTS", &["yaxunit"]),
            spec("yaxunit", &[]),
            spec("INDEPENDENT", &[]),
        ])
        .unwrap();
        let tests = &topology.nodes()[0];
        assert_eq!(names(&topology, tests.closure()), ["yaxunit"]);
        let independent = &topology.nodes()[2];
        assert!(independent.closure().is_empty());
        assert_eq!(
            names(&topology, topology.topological_order()),
            ["yaxunit", "TESTS", "INDEPENDENT"],
            "a dependency moves ahead of its dependent; unrelated nodes keep declared order"
        );
    }

    #[test]
    fn diamond_dependency_enters_closure_once() {
        let topology = build(vec![
            spec("A", &["B", "C"]),
            spec("B", &["D"]),
            spec("C", &["D"]),
            spec("D", &[]),
        ])
        .unwrap();
        let a = &topology.nodes()[0];
        assert_eq!(names(&topology, a.closure()), ["D", "B", "C"]);
    }

    #[test]
    fn direct_cycle_reports_full_path() {
        let err =
            build(vec![spec("TESTS", &["yaxunit"]), spec("yaxunit", &["TESTS"])]).unwrap_err();
        assert_eq!(err.to_string(), "extension dependency cycle: TESTS -> yaxunit -> TESTS");
    }

    #[test]
    fn transitive_cycle_reports_full_path() {
        let err = build(vec![spec("A", &["B"]), spec("B", &["C"]), spec("C", &["A"])]).unwrap_err();
        assert_eq!(err.to_string(), "extension dependency cycle: A -> B -> C -> A");
    }

    #[test]
    fn unknown_dependency_is_rejected() {
        let err = build(vec![spec("TESTS", &["missing"])]).unwrap_err();
        assert_eq!(
            err,
            TopologyError::UnknownDependency { from: "TESTS".into(), name: "missing".into() }
        );
    }

    #[test]
    fn self_reference_is_rejected_case_insensitively() {
        let err = build(vec![spec("Тесты", &["ТЕСТЫ"])]).unwrap_err();
        assert_eq!(err, TopologyError::SelfReference { name: "Тесты".into() });
    }

    #[test]
    fn duplicate_edge_is_rejected_case_insensitively() {
        let err = build(vec![spec("A", &["Base", "base"]), spec("Base", &[])]).unwrap_err();
        assert_eq!(err, TopologyError::DuplicateEdge { from: "A".into(), name: "base".into() });
    }

    #[test]
    fn duplicate_structured_name_is_rejected_case_insensitively() {
        let err = build(vec![spec("Тесты", &[]), spec("ТЕСТЫ", &[])]).unwrap_err();
        assert_eq!(err, TopologyError::DuplicateName { name: "ТЕСТЫ".into() });
    }

    #[test]
    fn legacy_duplicate_names_stay_but_cannot_be_targets() {
        let topology =
            build(vec![legacy_spec("Ext"), legacy_spec("Ext")]).expect("legacy duplicates stay");
        assert_eq!(topology.nodes().len(), 2);

        let err =
            build(vec![legacy_spec("Ext"), legacy_spec("Ext"), spec("T", &["Ext"])]).unwrap_err();
        assert_eq!(
            err,
            TopologyError::AmbiguousDependency { from: "T".into(), name: "Ext".into() }
        );
    }

    #[test]
    fn empty_name_is_rejected() {
        let err = build(vec![spec("  ", &[])]).unwrap_err();
        assert_eq!(err, TopologyError::EmptyName);
    }

    #[test]
    fn fingerprint_is_stable_and_tracks_identity() {
        let base = || vec![spec("TESTS", &["yaxunit"]), spec("yaxunit", &[]), spec("IND", &[])];
        let fp = |specs| build(specs).unwrap().fingerprint();

        assert_eq!(fp(base()), fp(base()), "identical inputs must produce identical digests");

        let mut renamed = base();
        renamed[2].name = "IND2".to_string();
        assert_ne!(fp(base()), fp(renamed));

        let mut repathed = base();
        repathed[2].canonical_path = PathBuf::from("/elsewhere/IND");
        assert_ne!(fp(base()), fp(repathed));

        let mut extra_edge = base();
        extra_edge[2].depends_on.get_or_insert_with(Vec::new).push("yaxunit".to_string());
        assert_ne!(fp(base()), fp(extra_edge));

        let reordered = vec![spec("IND", &[]), spec("TESTS", &["yaxunit"]), spec("yaxunit", &[])];
        assert_ne!(fp(base()), fp(reordered), "declared order is part of overlay identity");

        let other_base =
            ExtensionTopology::build(Path::new("/ws/other-base"), base()).unwrap().fingerprint();
        assert_ne!(fp(base()), other_base, "the base root is part of project identity");
    }

    #[test]
    fn fingerprint_distinguishes_declaration_order_even_with_equal_topo_order() {
        let a_first = build(vec![spec("A", &["B"]), spec("B", &[])]).unwrap();
        let b_first = build(vec![spec("B", &[]), spec("A", &["B"])]).unwrap();
        assert_eq!(
            names(&a_first, a_first.topological_order()),
            names(&b_first, b_first.topological_order()),
            "both declarations topo-sort to the same B -> A order"
        );
        assert_ne!(
            a_first.fingerprint(),
            b_first.fingerprint(),
            "declaration order drives overlay precedence and must change the digest"
        );
    }

    #[test]
    fn an_external_without_depends_on_closes_over_every_extension_in_order() {
        let topology = build(vec![
            external_spec("E", None),
            spec("A", &["C"]),
            spec("B", &[]),
            spec("C", &[]),
        ])
        .unwrap();
        let external = &topology.nodes()[0];
        assert!(external.sees_every_extension());
        assert!(external.depends_on().is_empty(), "no key, no edge");
        assert_eq!(
            names(&topology, external.closure()),
            ["B", "C", "A"],
            "every extension, in the global topological order"
        );
        assert_eq!(names(&topology, topology.topological_order()), ["B", "C", "A", "E"]);
    }

    #[test]
    fn an_external_with_depends_on_closes_over_the_declared_extensions_only() {
        let topology = build(vec![
            external_spec("E", Some(&["A"])),
            spec("A", &["C"]),
            spec("B", &[]),
            spec("C", &[]),
        ])
        .unwrap();
        let external = &topology.nodes()[0];
        assert!(!external.sees_every_extension());
        assert_eq!(names(&topology, external.depends_on()), ["A"]);
        assert_eq!(names(&topology, external.closure()), ["C", "A"], "transitive, B left out");
        assert_eq!(
            names(&topology, topology.topological_order()),
            ["B", "C", "A", "E"],
            "declared first, the external still goes after every extension"
        );

        let base_only = build(vec![external_spec("E", Some(&[])), spec("A", &[])]).unwrap();
        let external = &base_only.nodes()[0];
        assert!(!external.sees_every_extension());
        assert!(external.closure().is_empty(), "an empty list is the base alone");
        for node in base_only.nodes().iter().chain(topology.nodes()) {
            if !node.kind().is_external() {
                assert!(!node.sees_every_extension(), "the mode is an external's alone");
            }
        }
    }

    #[test]
    fn an_external_target_is_refused_whoever_points_at_it() {
        let from_extension = build(vec![spec("A", &["E"]), external_spec("E", None)]).unwrap_err();
        assert_eq!(
            from_extension,
            TopologyError::ExternalInDependency { from: "A".into(), name: "E".into() }
        );
        let from_external =
            build(vec![external_spec("E", Some(&["F"])), external_spec("F", None)]).unwrap_err();
        assert_eq!(
            from_external,
            TopologyError::ExternalInDependency { from: "E".into(), name: "F".into() }
        );
        assert!(!from_external.to_string().contains("takes no part"), "{from_external}");

        let on_itself = build(vec![external_spec("E", Some(&["e"]))]).unwrap_err();
        assert_eq!(on_itself, TopologyError::SelfReference { name: "E".into() });
        let unknown = build(vec![external_spec("E", Some(&["nope"]))]).unwrap_err();
        assert_eq!(
            unknown,
            TopologyError::UnknownDependency { from: "E".into(), name: "nope".into() }
        );
    }

    #[test]
    fn fingerprint_tells_an_externals_visibility_mode_apart() {
        let fp = |specs| build(specs).unwrap().fingerprint();
        let every = || vec![external_spec("E", None), spec("A", &[])];
        let none = || vec![external_spec("E", Some(&[])), spec("A", &[])];
        assert_eq!(fp(every()), fp(every()));
        assert_ne!(fp(every()), fp(none()), "no edge either way, yet different visibility");
        assert_ne!(fp(none()), fp(vec![external_spec("E", Some(&["A"])), spec("A", &[])]));
    }

    #[test]
    fn fingerprint_golden_vector() {
        let topology = build(vec![spec("TESTS", &["yaxunit"]), spec("yaxunit", &[])]).unwrap();
        expect_test::expect!["3b13a041c8c11a31560c8abdeee4edff4431c42e95b97df6c95f4003a455e98c"]
            .assert_eq(&topology.fingerprint().to_hex());
    }
}
