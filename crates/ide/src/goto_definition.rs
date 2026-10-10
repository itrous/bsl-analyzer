use std::path::PathBuf;

use hir::{
    Definition, MetadataReferenceKind, ModuleId, SemanticSymbol, SemanticSymbolKind, Semantics,
    TypeKind,
};
use ide_db::{base_db::METADATA_SOURCE_ROOT, RootDatabase};
use syntax::{ast, ast::AstNode, SyntaxKind, TextRange, TextSize};
use vfs::FileId;

use crate::{NavigationTarget, SymbolKind};

pub fn goto_definition<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
) -> Option<NavigationTarget> {
    let _span =
        tracing::info_span!("goto_definition", ?file_id, offset = u32::from(offset)).entered();

    let sema = Semantics::new(db);
    if let Some(symbol) = sema.symbol_at(file_id, offset) {
        if let Some(nav) = semantic_symbol_to_navigation_target(db, &symbol) {
            return Some(nav);
        }
        if let Some(ty) = symbol.ty {
            if let Some(nav) = metadata_reference_to_navigation_target(db, file_id, ty) {
                return Some(nav);
            }
        }
    }

    // Fallback: the cursor is on the base method name inside a configuration-extension
    // annotation (`&Вместо`/`&Перед`/`&После`/`&ИзменениеИКонтроль`), which carries no
    // semantic symbol — jump to that method in the paired base module.
    goto_extension_annotation_target(db, file_id, offset)
}

pub(crate) fn metadata_reference_to_navigation_target<DB: RootDatabase>(
    db: &DB,
    from_file_id: FileId,
    ty: hir::TypeId,
) -> Option<NavigationTarget> {
    let TypeKind::MetadataReference { kind, name } = db.lookup_type(ty) else {
        return None;
    };
    let file_id = resolve_metadata_reference_xml_file(db, from_file_id, *kind, name.as_str())?;
    let range = metadata_reference_name_range(db, file_id, name.as_str())?;
    Some(NavigationTarget { file_id, range, name: name.clone(), kind: SymbolKind::Variable })
}

fn resolve_metadata_reference_xml_file<DB: RootDatabase>(
    db: &DB,
    from_file_id: FileId,
    kind: MetadataReferenceKind,
    name: &str,
) -> Option<FileId> {
    let candidates = metadata_reference_xml_relative_paths(db, from_file_id, kind, name);
    let source_root_id = db.file_source_root_input(from_file_id).source_root_id(db);
    for (relative, modes) in &candidates {
        for (_config_name, config_root) in db.all_config_paths() {
            let candidate = config_root.join(relative).to_string_lossy().into_owned();
            for (idx, candidate_source_root) in
                [source_root_id, METADATA_SOURCE_ROOT].into_iter().enumerate()
            {
                if idx == 1 && candidate_source_root == source_root_id {
                    continue;
                }
                if let Some(file_id) = ide_db::base_db::resolve_vfs_path_ci_query(
                    db,
                    db.source_root_input(candidate_source_root),
                    candidate.clone(),
                    modes,
                ) {
                    return Some(file_id);
                }
            }
        }
    }
    None
}

fn metadata_reference_xml_relative_paths<DB: RootDatabase>(
    db: &DB,
    from_file_id: FileId,
    kind: MetadataReferenceKind,
    name: &str,
) -> Vec<(PathBuf, Vec<bsl_conventions::SegmentMatch>)> {
    use bsl_conventions::SegmentMatch as M;
    // Каталог — конвенционный (ci), а `{name}` — имя объекта: стебель точный,
    // регистронезависимо только расширение.
    let flat = |dir: &str| {
        (PathBuf::from(dir).join(format!("{name}.xml")), vec![M::Ci, M::StemExactExtCi])
    };
    match kind {
        MetadataReferenceKind::Role => vec![flat("Roles")],
        MetadataReferenceKind::EventSubscription => vec![flat("EventSubscriptions")],
        MetadataReferenceKind::ScheduledJob => vec![flat("ScheduledJobs")],
        MetadataReferenceKind::HttpService => vec![flat("HTTPServices")],
        MetadataReferenceKind::WebService => vec![flat("WebServices")],
        // Subsystems nest on disk as `<Parent>/Subsystems/<Child>.xml`; the parent chain is
        // recorded in each subsystem's `child_subsystems`, so reconstruct the nested path and
        // keep the flat top-level path as a fallback.
        MetadataReferenceKind::Subsystem => {
            let mut candidates = Vec::new();
            let subsystems = db
                .subsystem_names(from_file_id)
                .into_iter()
                .filter_map(|subsystem_name| db.resolve_subsystem(from_file_id, &subsystem_name))
                .collect::<Vec<_>>();
            if let Some(nested) = subsystem_xml_relative_path(
                subsystems.iter().map(|subsystem| subsystem.as_ref()),
                name,
            ) {
                // Каждый уровень: `Subsystems` — конвенционный, имя предка —
                // точное; последний компонент — имя со свободным расширением.
                let levels = nested.components().count() / 2;
                let mut modes = Vec::with_capacity(levels * 2);
                for _ in 0..levels.saturating_sub(1) {
                    modes.extend([M::Ci, M::Exact]);
                }
                modes.extend([M::Ci, M::StemExactExtCi]);
                candidates.push((nested, modes));
            }
            candidates.push(flat("Subsystems"));
            candidates
        }
    }
}

/// Reconstruct the on-disk relative path of a (possibly nested) subsystem from the flat
/// subsystem list, where each subsystem records its direct children by name.
fn subsystem_xml_relative_path<'a>(
    subsystems: impl IntoIterator<Item = &'a bsl_metadata::Subsystem>,
    name: &str,
) -> Option<PathBuf> {
    use std::collections::HashMap;
    use stdx::case::CaseExt;

    let subsystems = subsystems.into_iter().collect::<Vec<_>>();
    let target = subsystems.iter().find(|s| s.name().fold_lower() == name.fold_lower())?;
    let parent_of: HashMap<String, &str> = subsystems
        .iter()
        .flat_map(|parent| {
            parent.child_subsystems().iter().map(move |child| (child.fold_lower(), parent.name()))
        })
        .collect();

    let mut chain = vec![target.name()];
    let mut current = target.name();
    while let Some(parent) = parent_of.get(&current.fold_lower()) {
        if chain.iter().any(|n| n.fold_lower() == parent.fold_lower()) {
            break; // defensive: a malformed cyclic parent chain must not loop forever
        }
        chain.push(parent);
        current = parent;
    }
    chain.reverse();

    let mut path = PathBuf::new();
    for ancestor in &chain[..chain.len() - 1] {
        path.push("Subsystems");
        path.push(ancestor);
    }
    path.push("Subsystems");
    path.push(format!("{}.xml", chain[chain.len() - 1]));
    Some(path)
}

pub(crate) fn metadata_reference_name_range<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    name: &str,
) -> Option<TextRange> {
    let text = db.file_text(file_id);
    // Bind to the `<Name>…</Name>` element so a bare substring search cannot land on a
    // longer sibling name (`Роль1` inside `Роль10`) or on the name echoed in a `<Synonym>`.
    let (start, end) = text.match_indices(name).find_map(|(idx, matched)| {
        let end = idx.checked_add(matched.len())?;
        let before = text.get(..idx)?.trim_end();
        let after = text.get(end..)?.trim_start();
        (before.ends_with("<Name>") && after.starts_with("</Name>")).then_some((idx, end))
    })?;
    let start = u32::try_from(start).ok()?;
    let end = u32::try_from(end).ok()?;
    Some(TextRange::new(TextSize::from(start), TextSize::from(end)))
}

/// Resolve goto-definition when `offset` sits on the method-name string of a
/// configuration-extension annotation. The name resolves to the extended method in the base
/// module paired with this extension file; returns `None` for every other position.
fn goto_extension_annotation_target<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    offset: TextSize,
) -> Option<NavigationTarget> {
    const ANNOTATION_KINDS: [SyntaxKind; 4] = [
        SyntaxKind::ANN_AROUND,
        SyntaxKind::ANN_BEFORE,
        SyntaxKind::ANN_AFTER,
        SyntaxKind::ANN_CHANGE_AND_VALIDATE,
    ];

    let root = db.parse(file_id).syntax_node();
    // The target name is a string literal; only trigger when the cursor is on that token.
    let token = root.token_at_offset(offset).find(|t| t.kind() == SyntaxKind::STRING)?;
    let annotation = token.parent_ancestors().find_map(ast::Annotation::cast)?;
    if !ANNOTATION_KINDS.contains(&annotation.kind_token()?.kind()) {
        return None;
    }

    // Resolve the method named by the literal directly under the cursor — not by re-scanning
    // the method's annotations — so an in-progress edit with several annotations still jumps to
    // exactly what the cursor is on.
    let target_name = hir::unquote_bsl_string(token.text());

    let base_file = ide_db::weaving_target(db, file_id)?.base_file(db);
    let base_symbols = db.symbol_tree(ModuleId::new(base_file));
    let base_method = base_symbols.find_method(&hir::Name::new(&target_name))?;

    Some(NavigationTarget {
        file_id: base_file,
        range: base_method.source_range,
        name: base_method.name.as_str().to_string(),
        kind: if base_method.is_function { SymbolKind::Function } else { SymbolKind::Procedure },
    })
}

fn semantic_symbol_to_navigation_target<DB: RootDatabase>(
    db: &DB,
    symbol: &SemanticSymbol,
) -> Option<NavigationTarget> {
    if let Some(definition) = &symbol.definition {
        if matches!(definition, Definition::Method(_) | Definition::Variable(_)) {
            return definition_to_navigation_target(db, definition);
        }
    }

    if let Some(declaration) = &symbol.declaration {
        return Some(NavigationTarget {
            file_id: declaration.file_id,
            range: declaration.range,
            name: declaration.name.as_str().to_string(),
            kind: symbol_kind_for_semantic(declaration.kind),
        });
    }

    let definition = symbol.definition.as_ref()?;
    definition_to_navigation_target(db, definition)
}

fn symbol_kind_for_semantic(kind: SemanticSymbolKind) -> SymbolKind {
    match kind {
        SemanticSymbolKind::Function | SemanticSymbolKind::Method => SymbolKind::Function,
        SemanticSymbolKind::Parameter | SemanticSymbolKind::Variable => SymbolKind::Variable,
        SemanticSymbolKind::Property
        | SemanticSymbolKind::Type
        | SemanticSymbolKind::Class
        | SemanticSymbolKind::Namespace => SymbolKind::Variable,
    }
}

fn definition_to_navigation_target<DB: RootDatabase>(
    db: &DB,
    definition: &Definition,
) -> Option<NavigationTarget> {
    match definition {
        Definition::Method(method_id) => {
            let file_id = method_id.module.file_id;
            let tree = db.item_tree_ref(file_id);
            let method = tree.method(method_id.local_id)?;
            Some(NavigationTarget {
                file_id,
                range: method.source_range(),
                name: method.name().as_str().to_string(),
                kind: if method.is_function() {
                    SymbolKind::Function
                } else {
                    SymbolKind::Procedure
                },
            })
        }
        Definition::Variable(var_id) => {
            let file_id = var_id.module.file_id;
            let tree = db.item_tree_ref(file_id);

            for (idx, item) in tree.top_level_items().iter().enumerate() {
                if idx == var_id.local_id as usize {
                    if let hir::ModItem::Variable(var_idx) = item {
                        let var = tree.variable(*var_idx);
                        let range = var.source_range;
                        let name = var.name.as_str().to_string();
                        return Some(NavigationTarget {
                            file_id,
                            range,
                            name,
                            kind: SymbolKind::Variable,
                        });
                    }
                }
            }
            None
        }
        Definition::Parameter { method_id, param_name, .. } => {
            let file_id = method_id.module.file_id;
            let tree = db.item_tree_ref(file_id);
            let range = tree.method(method_id.local_id)?.source_range();
            Some(NavigationTarget {
                file_id,
                range,
                name: param_name.as_str().to_string(),
                kind: SymbolKind::Variable,
            })
        }
        Definition::Local { method_id, var_name } => {
            let file_id = method_id.module.file_id;
            let tree = db.item_tree_ref(file_id);
            let range = tree.method(method_id.local_id)?.source_range();
            Some(NavigationTarget {
                file_id,
                range,
                name: var_name.as_str().to_string(),
                kind: SymbolKind::Variable,
            })
        }
        Definition::BuiltinFunction(_)
        | Definition::BuiltinMethodHandle { .. }
        | Definition::MdoCollectionType(_)
        | Definition::MdoObject { .. }
        | Definition::MdoManagerModule { .. }
        | Definition::Module(_)
        | Definition::VirtualTableField { .. }
        | Definition::Unresolved => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use vfs::{file_set::FileSet, VfsPath};

    fn create_db_with_file(source: &str) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        (db, file_id)
    }

    #[test]
    fn test_goto_definition_method() {
        let source = r#"
Процедура МояПроцедура()
КонецПроцедуры

Функция Тест()
    МояПроцедура();
КонецФункции
        "#;

        let (db, file_id) = create_db_with_file(source);

        let call_offset = source.rfind("МояПроцедура").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let target = goto_definition(&db, file_id, offset);
        assert!(target.is_some());

        let target = target.unwrap();
        assert_eq!(target.file_id, file_id);
        assert_eq!(target.name, "МояПроцедура");
        assert_eq!(target.kind, SymbolKind::Procedure);
        assert!(!target.range.is_empty());
    }

    #[test]
    fn test_goto_definition_function() {
        let source = r#"
Функция МояФункция()
    Возврат 1;
КонецФункции

Процедура Тест()
    Результат = МояФункция();
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let call_offset = source.rfind("МояФункция").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let target = goto_definition(&db, file_id, offset);
        assert!(target.is_some());

        let target = target.unwrap();
        assert_eq!(target.name, "МояФункция");
        assert_eq!(target.kind, SymbolKind::Function);
    }

    #[test]
    fn test_goto_definition_variable() {
        let source = r#"
Перем МодульнаяПеременная;

Процедура Тест()
    МодульнаяПеременная = 1;
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let usage_offset = source.rfind("МодульнаяПеременная").unwrap();
        let offset = TextSize::from(usage_offset as u32);

        let target = goto_definition(&db, file_id, offset);
        assert!(target.is_some());

        let target = target.unwrap();
        assert_eq!(target.name, "МодульнаяПеременная");
        assert_eq!(target.kind, SymbolKind::Variable);
    }

    #[test]
    fn test_goto_definition_implicit_local_goes_to_first_assignment() {
        let source = r#"
Процедура Тест()
    НаборЗаписей = 10;
    Сообщить(НаборЗаписей);
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let usage_offset = source.rfind("НаборЗаписей").unwrap();
        let target = goto_definition(&db, file_id, TextSize::from(usage_offset as u32))
            .expect("implicit local should have a navigation target");

        let def_offset = source.find("НаборЗаписей").unwrap() as u32;
        assert_eq!(target.file_id, file_id);
        assert_eq!(target.name, "НаборЗаписей");
        assert_eq!(target.kind, SymbolKind::Variable);
        assert_eq!(u32::from(target.range.start()), def_offset);
        let range_start: usize = target.range.start().into();
        let range_end: usize = target.range.end().into();
        assert_eq!(&source[range_start..range_end], "НаборЗаписей");
    }

    /// Which assignment an occurrence navigates to is a per-occurrence choice, and it
    /// stays one: an occurrence goes to the nearest preceding write, not to the first.
    /// This is deliberately unlike a `Перем`-declared variable, where every occurrence
    /// goes to the one declaration — navigation is the axis on which the two differ.
    ///
    /// Regression gate: green before and after the identity change. It is the gate that
    /// keeps the reference walk from collapsing navigation along with the key, and it
    /// needs a body with TWO assignments — with one, "always the first" and "the nearest
    /// preceding" answer alike and the gate proves nothing.
    #[test]
    fn test_goto_definition_implicit_local_takes_the_nearest_preceding_assignment() {
        let source = r#"
Процедура Тест()
    НаборЗаписей = 10;
    Сообщить(НаборЗаписей);
    НаборЗаписей = 20;
    Сообщить(НаборЗаписей);
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);
        let sites: Vec<usize> = {
            let mut found = Vec::new();
            let mut from = 0usize;
            while let Some(at) = source[from..].find("НаборЗаписей") {
                found.push(from + at);
                from += at + "НаборЗаписей".len();
            }
            found
        };
        assert_eq!(sites.len(), 4, "the input must carry two assignments and two reads");

        let expected = [sites[0], sites[0], sites[2], sites[2]];
        for (occurrence, want) in sites.iter().zip(expected) {
            let target = goto_definition(&db, file_id, TextSize::from(*occurrence as u32))
                .expect("an implicit local has a navigation target");
            assert_eq!(
                usize::try_from(u32::from(target.range.start())).unwrap(),
                want,
                "the occurrence at {occurrence} navigated to the wrong assignment"
            );
        }
    }

    #[test]
    fn test_goto_definition_case_insensitive() {
        let source = r#"
Процедура МояПроцедура()
КонецПроцедуры

Функция Тест()
    мояпроцедура();
КонецФункции
        "#;

        let (db, file_id) = create_db_with_file(source);

        let call_offset = source.find("мояпроцедура").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let target = goto_definition(&db, file_id, offset);
        assert!(target.is_some());

        let target = target.unwrap();
        assert_eq!(target.name, "МояПроцедура");
    }

    #[test]
    fn test_goto_definition_not_found() {
        let source = r#"
Процедура Тест()
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let offset = source.find("Процедура").unwrap();
        let offset = TextSize::from(offset as u32);

        let target = goto_definition(&db, file_id, offset);
        assert!(target.is_none());
    }

    #[test]
    fn test_goto_definition_on_declaration() {
        let source = r#"
Процедура МояПроцедура()
КонецПроцедуры
        "#;

        let (db, file_id) = create_db_with_file(source);

        let decl_offset = source.find("МояПроцедура").unwrap();
        let offset = TextSize::from(decl_offset as u32);

        let target = goto_definition(&db, file_id, offset);
        assert!(target.is_some());

        let target = target.unwrap();
        assert_eq!(target.name, "МояПроцедура");
    }

    fn create_multi_file_db(files: &[(&str, &str)]) -> (RootDatabaseImpl, Vec<FileId>) {
        let mut db = RootDatabaseImpl::default();
        let mut file_ids = Vec::new();

        let mut file_set = FileSet::new();
        for (idx, (path, _)) in files.iter().enumerate() {
            let file_id = FileId(idx as u32);
            file_set.insert(file_id, VfsPath::new(path));
            file_ids.push(file_id);
        }

        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);

        for (file_id, (_, source)) in file_ids.iter().zip(files.iter()) {
            db.set_file_source_root(*file_id, SourceRootId(0));
            db.set_file_text(*file_id, source);
        }

        (db, file_ids)
    }

    #[test]
    fn test_goto_definition_cross_file() {
        let common_module = r#"
Функция ЭкспортнаяФункция() Экспорт
    Возврат 42;
КонецФункции
        "#;

        let form_module = r#"
Процедура Тест()
    Результат = ОбщийМодуль.ЭкспортнаяФункция();
КонецПроцедуры
        "#;

        let files = &[
            ("CommonModules/ОбщийМодуль/Ext/Module.bsl", common_module),
            ("Forms/Form1/Ext/Form/Module.bsl", form_module),
        ];

        let (db, file_ids) = create_multi_file_db(files);

        let call_offset = form_module.rfind("ЭкспортнаяФункция").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let target = goto_definition(&db, file_ids[1], offset);
        assert!(target.is_some(), "Should resolve cross-file method call");

        let target = target.unwrap();
        assert_eq!(target.file_id, file_ids[0], "Should navigate to CommonModule file");
        assert_eq!(target.name, "ЭкспортнаяФункция");
        assert_eq!(target.kind, SymbolKind::Function);
    }

    #[test]
    fn test_goto_definition_cross_file_case_insensitive() {
        let common_module = r#"
Функция МояФункция() Экспорт
    Возврат 1;
КонецФункции
        "#;

        let form_module = r#"
Процедура Тест()
    общиймодуль.мояфункция();
КонецПроцедуры
        "#;

        let files = &[
            ("CommonModules/ОбщийМодуль/Ext/Module.bsl", common_module),
            ("Forms/Form1/Ext/Form/Module.bsl", form_module),
        ];

        let (db, file_ids) = create_multi_file_db(files);

        let call_offset = form_module.find("мояфункция").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let target = goto_definition(&db, file_ids[1], offset);
        assert!(target.is_some(), "Should resolve case-insensitive");

        let target = target.unwrap();
        assert_eq!(target.file_id, file_ids[0]);
        assert_eq!(target.name, "МояФункция");
    }

    #[test]
    fn test_goto_definition_cross_file_not_found() {
        let common_module = r#"
Функция СуществуетМетод() Экспорт
    Возврат 1;
КонецФункции
        "#;

        let form_module = r#"
Процедура Тест()
    ОбщийМодуль.НеСуществуетМетод();
КонецПроцедуры
        "#;

        let files = &[
            ("CommonModules/ОбщийМодуль/Ext/Module.bsl", common_module),
            ("Forms/Form1/Ext/Form/Module.bsl", form_module),
        ];

        let (db, file_ids) = create_multi_file_db(files);

        let call_offset = form_module.find("НеСуществуетМетод").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let target = goto_definition(&db, file_ids[1], offset);
        assert!(target.is_none(), "Should not resolve non-existent method");
    }

    #[test]
    fn test_goto_definition_non_export_method() {
        let common_module = r#"
Функция ВнутренняяФункция()
    Возврат 1;
КонецФункции
        "#;

        let form_module = r#"
Процедура Тест()
    ОбщийМодуль.ВнутренняяФункция();
КонецПроцедуры
        "#;

        let files = &[
            ("CommonModules/ОбщийМодуль/Ext/Module.bsl", common_module),
            ("Forms/Form1/Ext/Form/Module.bsl", form_module),
        ];

        let (db, file_ids) = create_multi_file_db(files);

        let call_offset = form_module.find("ВнутренняяФункция").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let target = goto_definition(&db, file_ids[1], offset);
        assert!(target.is_none(), "Should not navigate to non-export method");
    }

    #[test]
    fn test_goto_definition_same_file_still_works() {
        let source = r#"
Процедура МояПроцедура()
КонецПроцедуры

Функция Тест()
    МояПроцедура();
КонецФункции
        "#;

        let (db, file_id) = create_db_with_file(source);

        let call_offset = source.rfind("МояПроцедура").unwrap();
        let offset = TextSize::from(call_offset as u32);

        let target = goto_definition(&db, file_id, offset);
        assert!(target.is_some(), "Same-file navigation should still work");

        let target = target.unwrap();
        assert_eq!(target.file_id, file_id);
        assert_eq!(target.name, "МояПроцедура");
    }

    fn create_db_with_two_files(
        caller_source: &str,
        caller_path: &str,
        manager_source: &str,
        manager_path: &str,
    ) -> (RootDatabaseImpl, FileId, FileId) {
        let mut db = RootDatabaseImpl::default();
        let caller_file = FileId(0);
        let manager_file = FileId(1);

        let mut file_set = FileSet::new();
        file_set.insert(caller_file, VfsPath::new(caller_path));
        file_set.insert(manager_file, VfsPath::new(manager_path));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(caller_file, SourceRootId(0));
        db.set_file_source_root(manager_file, SourceRootId(0));

        db.set_file_text(caller_file, caller_source);
        db.set_file_text(manager_file, manager_source);

        (db, caller_file, manager_file)
    }

    #[test]
    fn test_goto_definition_manager_module_method() {
        let caller_source = r#"
Процедура Тест()
    РегистрыСведений.ТестовыйРегистр.МетодМенеджера();
КонецПроцедуры
        "#;

        let manager_source = r#"
Процедура МетодМенеджера() Экспорт
    // Implementation
КонецПроцедуры
        "#;

        let (db, caller_file, manager_file) = create_db_with_two_files(
            caller_source,
            "/test/Catalogs/Test/Ext/ObjectModule.bsl",
            manager_source,
            "/test/InformationRegisters/ТестовыйРегистр/Ext/ManagerModule.bsl",
        );

        let offset = TextSize::from(caller_source.rfind("МетодМенеджера").unwrap() as u32);

        let result = goto_definition(&db, caller_file, offset);

        assert!(result.is_some(), "Should resolve manager module method");
        let nav = result.unwrap();
        assert_eq!(nav.file_id, manager_file);
        assert_eq!(nav.name, "МетодМенеджера");
        assert_eq!(nav.kind, SymbolKind::Procedure);
    }

    #[test]
    fn test_goto_definition_manager_module_method_not_exported() {
        let caller_source = r#"
Процедура Тест()
    Документы.ТестовыйДокумент.ВнутреннийМетод();
КонецПроцедуры
        "#;

        let manager_source = r#"
Процедура ВнутреннийМетод()
    // NOT exported - should not be resolvable
КонецПроцедуры
        "#;

        let (db, caller_file, _manager_file) = create_db_with_two_files(
            caller_source,
            "/test/Catalogs/Test/Ext/ObjectModule.bsl",
            manager_source,
            "/test/Documents/ТестовыйДокумент/Ext/ManagerModule.bsl",
        );

        let offset = TextSize::from(caller_source.rfind("ВнутреннийМетод").unwrap() as u32);
        let result = goto_definition(&db, caller_file, offset);

        assert!(result.is_none(), "Non-exported methods should not resolve");
    }

    #[test]
    fn test_goto_definition_manager_module_metadata_not_found() {
        let caller_source = r#"
Процедура Тест()
    Справочники.НесуществующийОбъект.Метод();
КонецПроцедуры
        "#;

        let (db, caller_file) = create_db_with_file(caller_source);

        let offset = TextSize::from(caller_source.rfind("Метод").unwrap() as u32);
        let result = goto_definition(&db, caller_file, offset);

        assert!(result.is_none(), "Should not resolve non-existent metadata object");
    }

    #[test]
    fn test_goto_definition_keyword_method_after_dot_returns_none() {
        let source = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ 1";
    Результат = Запрос.Выполнить();
КонецПроцедуры
        "#;
        let (db, file_id) = create_db_with_file(source);

        let offset = TextSize::from(source.rfind("Выполнить").unwrap() as u32);
        let target = goto_definition(&db, file_id, offset);
        assert!(target.is_none(), "platform method navigation must be None, got: {target:?}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_goto_definition_platform_method_does_not_leak_to_workspace_module() {
        let bogus = r#"
Функция Выполнить() Экспорт
    Возврат "не должно резолвиться";
КонецФункции
        "#;
        let caller = r#"
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ 1";
    Результат = Запрос.Выполнить();
КонецПроцедуры
        "#;

        let files = &[
            ("CommonModules/Запрос/Ext/Module.bsl", bogus),
            ("Forms/Form1/Ext/Form/Module.bsl", caller),
        ];
        let (db, file_ids) = create_multi_file_db(files);

        let offset = TextSize::from(caller.rfind("Выполнить").unwrap() as u32);
        let target = goto_definition(&db, file_ids[1], offset);

        if let Some(t) = target {
            assert_ne!(
                t.file_id, file_ids[0],
                "goto must not leak from platform method to a workspace module that happens to share the receiver's name"
            );
        }
    }

    #[test]
    fn test_goto_definition_manager_module_case_insensitive() {
        let caller_source = r#"
Процедура Тест()
    // Lowercase call
    регистрысведений.тестовыйрегистр.методменеджера();
КонецПроцедуры
        "#;

        let manager_source = r#"
Процедура МетодМенеджера() Экспорт
КонецПроцедуры
        "#;

        let (db, caller_file, manager_file) = create_db_with_two_files(
            caller_source,
            "/test/Catalogs/Test/Ext/ObjectModule.bsl",
            manager_source,
            "/test/InformationRegisters/тестовыйрегистр/Ext/ManagerModule.bsl",
        );

        let offset = TextSize::from(caller_source.rfind("методменеджера").unwrap() as u32);
        let result = goto_definition(&db, caller_file, offset);

        assert!(result.is_some());
        let nav = result.unwrap();
        assert_eq!(nav.file_id, manager_file);
        assert_eq!(nav.name, "МетодМенеджера");
    }

    #[test]
    fn subsystem_path_is_flat_for_top_level() {
        use bsl_metadata::Subsystem;
        let subsystems = vec![Subsystem::new("Продажи")];
        assert_eq!(
            subsystem_xml_relative_path(&subsystems, "Продажи"),
            Some(PathBuf::from("Subsystems").join("Продажи.xml")),
        );
    }

    #[test]
    fn subsystem_path_is_nested_for_child_subsystems() {
        use bsl_metadata::Subsystem;
        let subsystems = vec![
            Subsystem::new("Продажи").with_child_subsystems(vec!["Опт".to_string()]),
            Subsystem::new("Опт").with_child_subsystems(vec!["Регионы".to_string()]),
            Subsystem::new("Регионы"),
        ];
        assert_eq!(
            subsystem_xml_relative_path(&subsystems, "Регионы"),
            Some(
                PathBuf::from("Subsystems")
                    .join("Продажи")
                    .join("Subsystems")
                    .join("Опт")
                    .join("Subsystems")
                    .join("Регионы.xml")
            ),
        );
    }

    #[test]
    fn subsystem_path_none_for_unknown_name() {
        use bsl_metadata::Subsystem;
        let subsystems = vec![Subsystem::new("Продажи")];
        assert_eq!(subsystem_xml_relative_path(&subsystems, "НетТакой"), None);
    }

    #[test]
    fn metadata_reference_subsystem_goto_uses_listed_substrate() {
        use ide_db::metadata::{MetadataListingData, SubsystemEntry};

        fn subsystem_xml(name: &str, children: &[&str]) -> String {
            let child_tags = children
                .iter()
                .map(|child| format!("        <Subsystem>{child}</Subsystem>"))
                .collect::<Vec<_>>()
                .join("\n");
            let children_block = if child_tags.is_empty() {
                String::new()
            } else {
                format!("\n{child_tags}\n        ")
            };
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xr="http://v8.1c.ru/8.3/xcf/readable" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <Subsystem uuid="00000000-0000-0000-0000-000000000095">
        <Properties>
            <Name>{name}</Name>
            <Content/>
        </Properties>
        <ChildObjects>{children_block}</ChildObjects>
    </Subsystem>
</MetaDataObject>"#
            )
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cf");
        let parent_path = root.join("Subsystems/Родитель.xml");
        let child_path = root.join("Subsystems/Родитель/Subsystems/Дочерняя.xml");
        std::fs::create_dir_all(parent_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(child_path.parent().unwrap()).unwrap();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();

        let parent_file = FileId(30);
        let child_file = FileId(31);
        let consumer_file = FileId(32);
        let consumer_path = root.join("GotoConsumer.bsl");

        let mut db = RootDatabaseImpl::new();
        let mut file_set = FileSet::new();
        file_set.insert(parent_file, VfsPath::new(parent_path.to_string_lossy().as_ref()));
        file_set.insert(child_file, VfsPath::new(child_path.to_string_lossy().as_ref()));
        file_set.insert(consumer_file, VfsPath::new(consumer_path.to_string_lossy().as_ref()));
        db.set_source_root(SourceRootId(1), SourceRoot::new_local(file_set));
        db.set_file_source_root(parent_file, SourceRootId(1));
        db.set_file_source_root(child_file, SourceRootId(1));
        db.set_file_source_root(consumer_file, SourceRootId(1));
        db.set_file_text(parent_file, &subsystem_xml("Родитель", &["Дочерняя"]));
        db.set_file_text(child_file, &subsystem_xml("Дочерняя", &[]));
        db.set_file_text(consumer_file, "Процедура Т() КонецПроцедуры");

        db.set_all_config_paths(vec![(None, root.clone())]);
        db.set_metadata_listing(
            &root.to_string_lossy(),
            MetadataListingData {
                common_attributes: Vec::new(),
                entries: Vec::new(),
                defined_types: Vec::new(),
                common_modules: Vec::new(),
                event_subscriptions: Vec::new(),
                scheduled_jobs: Vec::new(),
                roles: Vec::new(),
                http_services: Vec::new(),
                web_services: Vec::new(),
                integration_services: Vec::new(),
                subsystems: vec![
                    SubsystemEntry { name: "Родитель".to_string(), main: parent_file },
                    SubsystemEntry { name: "Дочерняя".to_string(), main: child_file },
                ],
            },
        );

        let paths = metadata_reference_xml_relative_paths(
            &db,
            consumer_file,
            MetadataReferenceKind::Subsystem,
            "Дочерняя",
        );

        let just_paths: Vec<&PathBuf> = paths.iter().map(|(p, _)| p).collect();
        assert_eq!(
            just_paths,
            vec![
                &PathBuf::from("Subsystems")
                    .join("Родитель")
                    .join("Subsystems")
                    .join("Дочерняя.xml"),
                &PathBuf::from("Subsystems").join("Дочерняя.xml"),
            ]
        );
    }

    /// A module variable named like a metadata plural must win over the plural:
    /// the cascade asks the metadata collection before module variables, so the
    /// declaration would otherwise be unreachable from its own uses.
    #[test]
    fn goto_module_variable_named_like_metadata_plural() {
        let source = r#"Перем Справочники;

Функция Тест()
    Рез = Справочники;
    Возврат Рез;
КонецФункции
"#;
        let (db, file_id) = create_db_with_file(source);
        let use_offset = source.rfind("Справочники").unwrap();
        let target = goto_definition(&db, file_id, TextSize::from(use_offset as u32));
        assert!(
            target.is_some(),
            "a module variable named like a metadata plural must be reachable from its use"
        );
        let target = target.expect("checked above");
        assert_eq!(target.name, "Справочники");
    }
}
