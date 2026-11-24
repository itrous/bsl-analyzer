//! Proven conflicts among declarations of one physical source module.

use std::sync::Arc;

use hir_def::execution_env::{self, EnvFlags};
use hir_def::item_tree::{AnnotationKind, MethodItem};
use hir_def::{MethodId, ModuleId, Name};
use intern::NormName;
use rustc_hash::FxHashMap;
use syntax::{SyntaxKind, SyntaxNode, TextRange};
use vfs::FileId;

use crate::db::HirDatabase;

/// Conflicting declarations of one physical source module, in source order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MethodDeclarationConflicts {
    pub methods: Vec<MethodDeclarationConflict>,
    pub interceptions: Vec<InterceptionDeclarationConflict>,
}

/// A conflicting physical declaration and its original name range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDeclarationConflict {
    pub name: Name,
    pub range: TextRange,
}

/// An annotation kind and target repeated by a physical method declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterceptionDeclarationConflict {
    pub method: MethodDeclarationConflict,
    pub target: Name,
    pub kind: AnnotationKind,
}

/// Read physical declarations; effective extension text must not be used to
/// compare a base declaration with its valid extension override.
pub fn method_declaration_conflicts(
    db: &dyn HirDatabase,
    file: FileId,
) -> Arc<MethodDeclarationConflicts> {
    let tree = db.item_tree(file);
    let module = ModuleId::new(file);
    let interface = db.module_interface(module);
    let metadata = db.module_metadata(module);
    if metadata.compiles_nowhere() {
        return Arc::new(MethodDeclarationConflicts::default());
    }
    let options = db.env_options_for_file(file);
    let context = db.file_compilation_context(file);
    let module_env = context.as_ref().map_or_else(
        || execution_env::module_base_env(&metadata, &options),
        |context| context.module_environment(&metadata),
    );
    let parse = db.parse(file);
    let nodes: FxHashMap<_, _> = parse
        .syntax_node()
        .descendants()
        .filter(|node| matches!(node.kind(), SyntaxKind::FUNCTION_DEF | SyntaxKind::PROCEDURE_DEF))
        .map(|node| (node.text_range(), node))
        .collect();
    let methods: Vec<_> = tree
        .top_level_items()
        .iter()
        .filter_map(|item| tree.method_item(item))
        .filter(|method| !method.name().is_missing())
        .collect();
    let environments: Vec<_> = methods
        .iter()
        .map(|method| {
            let Some(decl) =
                interface.find_method_by_id(MethodId { module, local_id: method.key() })
            else {
                return EnvFlags::EMPTY;
            };
            let directives: Vec<_> =
                decl.directives.iter().copied().filter(|kind| !kind.is_interception()).collect();
            if directives.len() > 1 {
                return EnvFlags::EMPTY;
            }
            let directive_env = if metadata.is_ordinary_form_module()
                || (directives.is_empty()
                    && decl.directives.iter().any(|kind| kind.is_interception()))
            {
                EnvFlags::ALL
            } else {
                execution_env::directive_env(
                    directives.first().copied(),
                    metadata.module_type,
                    &options,
                )
            };
            // Unconditional source duplicates do not require a proven runtime owner.
            let base = if module_env.is_empty() { EnvFlags::ALL } else { module_env };
            base & directive_env & decl.preproc_env
        })
        .collect();
    let mut names: FxHashMap<NormName, Vec<usize>> = FxHashMap::default();
    let mut interceptions: FxHashMap<(NormName, AnnotationKind), Vec<usize>> = FxHashMap::default();
    let mut targets = vec![None; methods.len()];
    for (index, method) in methods.iter().enumerate() {
        names.entry(NormName::intern(method.name().as_str())).or_default().push(index);
        let Some(node) = nodes.get(&method.source_range()) else { continue };
        let Some((kind, target)) = interception(*method, node) else { continue };
        if target.is_empty() {
            continue;
        }
        let target = Name::new(target.as_str());
        interceptions.entry((NormName::intern(target.as_str()), kind)).or_default().push(index);
        targets[index] = Some((kind, target));
    }
    let duplicate_names = overlapping_declarations(names.values(), &environments);
    let duplicate_interceptions = overlapping_declarations(interceptions.values(), &environments);
    let mut conflicts = MethodDeclarationConflicts::default();
    for (index, method) in methods.iter().enumerate() {
        let declaration =
            MethodDeclarationConflict { name: method.name().clone(), range: method.name_range() };
        if duplicate_names[index] {
            conflicts.methods.push(declaration.clone());
        }
        if duplicate_interceptions[index] {
            if let Some((kind, target)) = &targets[index] {
                conflicts.interceptions.push(InterceptionDeclarationConflict {
                    method: declaration,
                    kind: *kind,
                    target: target.clone(),
                });
            }
        }
    }
    Arc::new(conflicts)
}

fn overlapping_declarations<'a>(
    groups: impl Iterator<Item = &'a Vec<usize>>,
    environments: &[EnvFlags],
) -> Vec<bool> {
    let mut overlapping = vec![false; environments.len()];
    for group in groups {
        let mut seen = EnvFlags::EMPTY;
        let mut shared = EnvFlags::EMPTY;
        for &index in group {
            shared = shared | (seen & environments[index]);
            seen = seen | environments[index];
        }
        for &index in group {
            overlapping[index] |= environments[index].intersects(shared);
        }
    }
    overlapping
}

fn interception(method: MethodItem<'_>, node: &SyntaxNode) -> Option<(AnnotationKind, String)> {
    let mut annotations =
        method.annotations().iter().filter(|annotation| annotation.kind.is_interception());
    let kind = annotations.next()?.kind;
    if annotations.next().is_some() {
        return None;
    }
    if kind == AnnotationKind::ChangeAndValidate {
        return hir_def::extension_merge::extract_change_and_validate(node)
            .map(|control| (kind, control.target));
    }
    hir_def::weaving::interceptor_target(node).map(|interception| (kind, interception.target))
}
