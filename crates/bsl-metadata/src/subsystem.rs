use serde::{Deserialize, Serialize};
use stdx::case::CaseExt;

use crate::metadata_object::{MdoType, Name};

/// A configuration subsystem (`Подсистема`) — an organisational container that lists the
/// metadata objects belonging to it and its directly-nested child subsystems. Subsystems
/// are not data-bearing objects (no manager, ref type, or attributes); they exist here so
/// the call graph can answer "which subsystems contain this object" for impact analysis.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Subsystem {
    name: String,
    /// Member metadata objects from `<Content>`, as the (type, name) pairs that parsed.
    content: Vec<(MdoType, Name)>,
    /// Names of directly-nested child subsystems from `<ChildObjects><Subsystem>`.
    child_subsystems: Vec<Name>,

    #[serde(default)]
    pub picture: Option<String>,

    #[serde(default)]
    pub content_refs: Option<Vec<String>>,
    #[serde(default)]
    pub qualified_name: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub source_roots: Vec<String>,
    #[serde(default)]
    pub uuid: Option<uuid::Uuid>,
    #[serde(default)]
    pub object_belonging: crate::ObjectBelonging,
    #[serde(default)]
    pub extended_configuration_object: Option<uuid::Uuid>,
    #[serde(default)]
    pub content_unread: bool,
    /// Selected declarations could not be composed into one proved effective object.
    #[serde(default)]
    pub resolution_incomplete: bool,
}

impl Subsystem {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            content: Vec::new(),
            child_subsystems: Vec::new(),
            picture: None,
            content_refs: None,
            qualified_name: None,
            parent: None,
            source_roots: Vec::new(),
            uuid: None,
            object_belonging: crate::ObjectBelonging::Unknown,
            extended_configuration_object: None,
            content_unread: false,
            resolution_incomplete: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn address(&self) -> &str {
        self.qualified_name.as_deref().unwrap_or(&self.name)
    }

    /// The source path identifies same-named children under different parents.
    pub fn set_source_identity(&mut self, root: &std::path::Path, path: &std::path::Path) {
        let Ok(relative) = path.strip_prefix(root) else {
            return;
        };
        let parts: Vec<_> = relative.iter().filter_map(|part| part.to_str()).collect();
        if parts.len() < 2
            || !parts.len().is_multiple_of(2)
            || !parts.iter().step_by(2).all(|part| part.eq_ignore_ascii_case("Subsystems"))
        {
            return;
        }
        let mut names: Vec<String> =
            parts.iter().skip(1).step_by(2).map(|part| part.to_string()).collect();
        let Some(stem) = path.file_stem().and_then(|part| part.to_str()) else {
            return;
        };
        *names.last_mut().expect("a source identity has at least one name") = stem.to_string();
        self.parent = (names.len() > 1).then(|| names[..names.len() - 1].join(".Subsystem."));
        self.qualified_name = Some(names.join(".Subsystem."));
        self.source_roots = vec![root.to_string_lossy().into_owned()];
    }

    pub fn adopts(&self, base: &Self) -> bool {
        self.object_belonging == crate::ObjectBelonging::Adopted
            && base.uuid.is_some()
            && self.extended_configuration_object == base.uuid
    }

    pub fn content(&self) -> &[(MdoType, Name)] {
        &self.content
    }

    pub fn child_subsystems(&self) -> &[Name] {
        &self.child_subsystems
    }

    pub fn with_content(mut self, content: Vec<(MdoType, Name)>) -> Self {
        self.content = content;
        self
    }

    pub fn with_child_subsystems(mut self, children: Vec<Name>) -> Self {
        self.child_subsystems = children;
        self
    }

    /// Merge another subsystem's members and child subsystems into this one — an extension
    /// overlay that adds to a base subsystem of the same name. New entries are appended;
    pub fn merge_from(&mut self, other: &Subsystem) {
        self.resolution_incomplete |= other.resolution_incomplete;
        if other.picture.is_some() {
            self.picture.clone_from(&other.picture);
        }
        self.content_unread |=
            self.content_refs.is_none() || other.content_refs.is_none() || other.content_unread;
        if let Some(items) = &other.content_refs {
            let existing = self.content_refs.get_or_insert_default();
            for item in items {
                if !existing.iter().any(|value| stdx::case::eq_ignore_case(value, item)) {
                    existing.push(item.clone());
                }
            }
        }
        for root in &other.source_roots {
            if !self.source_roots.contains(root) {
                self.source_roots.push(root.clone());
            }
        }
        for entry in &other.content {
            if !self.content.iter().any(|existing| {
                existing.0 == entry.0 && existing.1.fold_lower() == entry.1.fold_lower()
            }) {
                self.content.push(entry.clone());
            }
        }
        for child in &other.child_subsystems {
            if !self.child_subsystems.iter().any(|c| c.fold_lower() == child.fold_lower()) {
                self.child_subsystems.push(child.clone());
            }
        }
    }

    /// Heap bytes owned by this subsystem, memoised by `ide-db`'s
    /// `parse_subsystem_query` for Salsa's `heap_size` hook: its name plus the
    /// content and child-subsystem-name vecs. New heap-owning fields must be
    /// added here too.
    pub fn estimated_heap_size(&self) -> usize {
        self.name.capacity()
            + self.qualified_name.as_ref().map_or(0, String::capacity)
            + self.parent.as_ref().map_or(0, String::capacity)
            + stdx::heap::vec_bytes::<String>(self.source_roots.len())
            + self.source_roots.iter().map(String::capacity).sum::<usize>()
            + self.picture.as_ref().map_or(0, String::capacity)
            + self.content_refs.as_ref().map_or(0, |items| {
                stdx::heap::vec_bytes::<String>(items.len())
                    + items.iter().map(String::capacity).sum::<usize>()
            })
            + stdx::heap::vec_bytes::<(MdoType, Name)>(self.content.len())
            + self.content.iter().map(|(_, name)| name.capacity()).sum::<usize>()
            + stdx::heap::vec_bytes::<Name>(self.child_subsystems.len())
            + self.child_subsystems.iter().map(String::capacity).sum::<usize>()
    }
}
