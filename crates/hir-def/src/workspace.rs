use rustc_hash::FxHashMap;
use std::sync::Arc;
use stdx::case::CaseExt;
use tracing::debug;
use vfs::{FileId, FileSet};

use crate::{DefDatabase, MethodSymbol, ModuleId, Name, VariableSymbol};

pub fn is_bsl_source(file_set: &FileSet, file_id: FileId) -> bool {
    let Some(vfs_path) = file_set.path_for_file(&file_id) else {
        return false;
    };
    project_model::is_bsl_source_path(vfs_path.as_path())
}

/// One module's externally addressable surface.
///
/// Only exported members are kept: a non-exported method cannot be called from
/// outside its module, `symbol_info` refuses to resolve it, and offering it as a
/// search hit would produce a candidate nothing accepts back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleMembers {
    pub file_id: FileId,

    pub module_id: ModuleId,

    /// Path-derived module name. Ambiguous by construction — every object
    /// module of every metadata object is spelled `ObjectModule.bsl` — so it is
    /// carried as a field and never used as a table key.
    pub module_name: Name,

    pub methods: Vec<MethodSymbol>,

    pub variables: Vec<VariableSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceMembers {
    pub modules: FxHashMap<FileId, ModuleMembers>,
}

/// Approximate live heap bytes for Salsa's `memory_usage` report: the `modules`
/// table, each module's name, and its member vectors — summing each method's
/// name, params, and annotations, and each variable's name and annotations.
/// `docs`/`return_type_ref`/param `type_ref` payloads are not followed (mirrors
/// `symbol_tree_heap`'s convention), a mild undercount.
/// New heap-owning fields must be added here too.
pub(crate) fn module_members_heap(v: &Arc<WorkspaceMembers>) -> usize {
    use crate::heap_estimate::{map_table_bytes, name_bytes, vec_bytes};
    use crate::item_tree::Annotation;
    use crate::ParamSymbol;

    let s = &**v;
    let mut bytes = map_table_bytes::<FileId, ModuleMembers>(s.modules.len());
    for info in s.modules.values() {
        bytes += name_bytes(&info.module_name);
        bytes += vec_bytes::<MethodSymbol>(info.methods.len());
        for method in &info.methods {
            bytes += name_bytes(&method.name);
            bytes += vec_bytes::<ParamSymbol>(method.params.len());
            for param in &method.params {
                bytes += name_bytes(&param.name);
            }
            bytes += vec_bytes::<Annotation>(method.annotations.len());
        }
        bytes += vec_bytes::<VariableSymbol>(info.variables.len());
        for variable in &info.variables {
            bytes += name_bytes(&variable.name);
            bytes += vec_bytes::<Annotation>(variable.annotations.len());
        }
    }
    bytes
}

pub fn module_members(db: &dyn DefDatabase, files: &[FileId]) -> WorkspaceMembers {
    let mut modules = FxHashMap::default();

    debug!(file_count = files.len(), "Building module member index");

    for &file_id in files {
        let module_id = ModuleId::new(file_id);
        let symbol_tree = db.symbol_tree(module_id);

        let methods: Vec<_> = symbol_tree.exported_methods().cloned().collect();
        let variables: Vec<_> = symbol_tree.exported_variables().cloned().collect();
        if methods.is_empty() && variables.is_empty() {
            continue;
        }

        let module_name = extract_module_name_from_file(db, file_id).unwrap_or_else(|| {
            debug!(
                file_id = ?file_id,
                "Could not extract module name, using FileId"
            );
            Name::new(&format!("Module{}", file_id.0))
        });

        debug!(
            file_id = ?file_id,
            module_name = %module_name,
            method_count = methods.len(),
            variable_count = variables.len(),
            "Indexed module"
        );

        modules
            .insert(file_id, ModuleMembers { file_id, module_id, module_name, methods, variables });
    }

    debug!(module_count = modules.len(), "Module member index built");

    WorkspaceMembers { modules }
}

fn extract_module_name_from_file(db: &dyn DefDatabase, file_id: FileId) -> Option<Name> {
    let file_source_root_input = db.file_source_root_input(file_id);
    let source_root_id = file_source_root_input.source_root_id(db);
    let source_root_input = db.source_root_input(source_root_id);
    let source_root = source_root_input.root(db);
    let file_set = source_root.file_set();

    let vfs_path = file_set.path_for_file(&file_id)?;
    let path_str = vfs_path.as_path().to_string_lossy();
    module_name_from_path(&path_str)
}

/// The common module a file path belongs to: the segment after `CommonModules/`
/// (localized `ОбщиеМодули/`) when a deeper path follows it, else `None`.
pub(crate) fn common_module_name_from_path(path: &str) -> Option<Name> {
    let path_str = path.replace('\\', "/");
    let mut segments = path_str.split('/');
    while let Some(segment) = segments.next() {
        if segment.eq_ignore_ascii_case("CommonModules") || segment.fold_lower() == "общиемодули"
        {
            let module_name = segments.next()?;
            segments.next()?;
            return Some(Name::new(module_name));
        }
    }
    None
}

/// Derive a module name from a file path: the segment after `CommonModules/`
/// (localized `ОбщиеМодули/`), else the `.bsl` file stem.
fn module_name_from_path(path: &str) -> Option<Name> {
    // The VFS stores native paths; on Windows those use `\`. The segment matching
    // below keys on `/`, so normalize first — otherwise `CommonModules/` is never
    // found and the filename split yields the whole path as the module name.
    let path_str = path.replace('\\', "/");

    debug!(path = %path_str, "Extracting module name from path");

    let mut segments = path_str.split('/');
    while let Some(segment) = segments.next() {
        if segment.eq_ignore_ascii_case("CommonModules") || segment.fold_lower() == "общиемодули"
        {
            if let Some(module_name) = segments.next() {
                if segments.next().is_some() {
                    debug!(module_name, "Extracted from CommonModules path");
                    return Some(Name::new(module_name));
                }
            }
            break;
        }
    }

    if let Some(filename) = path_str.rsplit('/').next() {
        if bsl_conventions::str_has_extension(filename, bsl_conventions::BSL_EXTENSION) {
            let name_without_ext =
                &filename[..filename.len() - bsl_conventions::BSL_EXTENSION.len() - 1];
            debug!(module_name = name_without_ext, "Extracted from filename");
            return Some(Name::new(name_without_ext));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::{FileSet, VfsPath};

    #[test]
    fn test_module_members_creation() {
        let file_id = FileId(0);
        let module_id = ModuleId::new(file_id);

        let info = ModuleMembers {
            file_id,
            module_id,
            module_name: Name::new("Товары"),
            methods: vec![],
            variables: vec![],
        };

        assert_eq!(info.file_id, file_id);
        assert_eq!(info.module_id, module_id);
        assert_eq!(info.methods.len(), 0);
        assert_eq!(info.variables.len(), 0);
    }

    fn make_file_set(entries: &[(u32, &str)]) -> FileSet {
        let mut fs = FileSet::new();
        for (id, path) in entries {
            fs.insert(FileId(*id), VfsPath::new(std::path::PathBuf::from(path)));
        }
        fs
    }

    #[test]
    fn is_bsl_source_accepts_bsl_extension() {
        let fs = make_file_set(&[
            (1, "/ws/CommonModules/MyModule/Ext/Module.bsl"),
            (2, "/ws/Documents/X/Ext/ObjectModule.bsl"),
        ]);
        assert!(is_bsl_source(&fs, FileId(1)));
        assert!(is_bsl_source(&fs, FileId(2)));
    }

    #[test]
    fn is_bsl_source_rejects_non_bsl_files() {
        let fs = make_file_set(&[
            (10, "/ws/Roles/R/Ext/Rights.xml"),
            (11, "/ws/InformationRegisters/X/Templates/T/Ext/Template.xml"),
            (12, "/ws/Configuration.xml"),
            (13, "/ws/README.md"),
            (14, "/ws/notes.txt"),
            (15, "/ws/NoExtensionAtAll"),
        ]);
        for id in 10..=15 {
            assert!(!is_bsl_source(&fs, FileId(id)), "FileId({id}) should be filtered out");
        }
    }

    #[test]
    fn is_bsl_source_is_case_insensitive() {
        let fs = make_file_set(&[
            (20, "/ws/CommonModules/A/Ext/Module.BSL"),
            (21, "/ws/CommonModules/B/Ext/Module.Bsl"),
        ]);
        assert!(is_bsl_source(&fs, FileId(20)));
        assert!(is_bsl_source(&fs, FileId(21)));
    }

    #[test]
    fn is_bsl_source_rejects_unknown_file_id() {
        let fs = make_file_set(&[(1, "/ws/a.bsl")]);
        assert!(!is_bsl_source(&fs, FileId(999)));
    }

    #[test]
    fn module_name_from_forward_slash_paths() {
        assert_eq!(
            module_name_from_path("/ws/src/cf/CommonModules/ОбщегоНазначения/Ext/Module.bsl"),
            Some(Name::new("ОбщегоНазначения"))
        );
        assert_eq!(
            module_name_from_path("/ws/Documents/Заказ/Ext/ObjectModule.bsl"),
            Some(Name::new("ObjectModule"))
        );
    }

    #[test]
    fn module_name_from_windows_backslash_paths() {
        assert_eq!(
            module_name_from_path(r"C:\ws\src\cf\CommonModules\ОбщегоНазначения\Ext\Module.bsl"),
            Some(Name::new("ОбщегоНазначения"))
        );
        assert_eq!(
            module_name_from_path(r"C:\ws\Documents\Заказ\Ext\ObjectModule.bsl"),
            Some(Name::new("ObjectModule"))
        );
    }
}
