use hir::ModItem;
use vfs::FileId;

use crate::{
    AnnotationKind, DocumentSymbol, MethodDetail, OutlineMode, ParamDefault, ParamDetail,
    SymbolDetail, VariableDetail,
};

pub(crate) fn document_symbols(
    db: &dyn ide_db::RootDatabase,
    file_id: FileId,
) -> Vec<DocumentSymbol> {
    let item_tree = db.item_tree(file_id);
    let region_tree = db.region_tree(file_id);

    let mut items: Vec<DocumentSymbol> = Vec::new();
    for mod_item in item_tree.top_level_items() {
        let sym = match mod_item {
            ModItem::Procedure(idx) => {
                let proc = item_tree.procedure(*idx);
                DocumentSymbol {
                    name: proc.name.as_str().to_string(),
                    range: proc.source_range,
                    selection_range: proc.name_range,
                    detail: SymbolDetail::Procedure(MethodDetail {
                        is_export: proc.is_export,
                        directives: directives_of(&proc.annotations),
                        params: params_of(&proc.params),
                    }),
                    children: Vec::new(),
                }
            }
            ModItem::Function(idx) => {
                let func = item_tree.function(*idx);
                DocumentSymbol {
                    name: func.name.as_str().to_string(),
                    range: func.source_range,
                    selection_range: func.name_range,
                    detail: SymbolDetail::Function(MethodDetail {
                        is_export: func.is_export,
                        directives: directives_of(&func.annotations),
                        params: params_of(&func.params),
                    }),
                    children: Vec::new(),
                }
            }
            ModItem::Variable(idx) => {
                let var = item_tree.variable(*idx);
                DocumentSymbol {
                    name: var.name.as_str().to_string(),
                    range: var.source_range,
                    selection_range: var.name_range,
                    detail: SymbolDetail::Variable(VariableDetail {
                        is_export: var.is_export,
                        directives: directives_of(&var.annotations),
                    }),
                    children: Vec::new(),
                }
            }
        };
        items.push(sym);
    }

    if region_tree.is_empty() {
        sort_by_position(&mut items);
        return items;
    }

    let mut result: Vec<DocumentSymbol> = Vec::new();
    for &root_idx in region_tree.root_regions() {
        result.extend(build_region(&region_tree, root_idx, &mut items));
    }

    result.append(&mut items);
    sort_by_position(&mut result);

    result
}

/// The file's map at the requested breadth.
///
/// `RegionsOnly` is sifted OUT of the full tree rather than built by a second traversal:
/// then the region skeleton is the same skeleton in both modes by construction, and cannot
/// drift into two shapes that merely look alike.
pub(crate) fn file_outline(
    db: &dyn ide_db::RootDatabase,
    file_id: FileId,
    mode: OutlineMode,
) -> Vec<DocumentSymbol> {
    let symbols = document_symbols(db, file_id);
    match mode {
        OutlineMode::Full => symbols,
        OutlineMode::RegionsOnly => regions_only(symbols),
    }
}

fn regions_only(symbols: Vec<DocumentSymbol>) -> Vec<DocumentSymbol> {
    symbols
        .into_iter()
        .filter(|symbol| symbol.detail == SymbolDetail::Region)
        .map(|mut region| {
            region.children = regions_only(std::mem::take(&mut region.children));
            region
        })
        .collect()
}

fn directives_of(annotations: &[hir::Annotation]) -> Vec<AnnotationKind> {
    annotations.iter().map(|annotation| annotation.kind).collect()
}

fn params_of(params: &[hir::Param]) -> Vec<ParamDetail> {
    params
        .iter()
        .map(|param| ParamDetail {
            name: param.name.as_str().to_string(),
            by_value: param.is_val,
            default: param_default(param),
        })
        .collect()
}

/// Which of the three answers a parameter's default is.
///
/// The mapping is total, and deliberately does not read `default_value` alone: the parser
/// always builds an expression node after `=`, so a default it could not parse arrives as
/// an EMPTY text rather than as no text at all. Judging by `Option` alone would call such a
/// parameter required — and a caller generating a wrapper from the signature would drop an
/// argument. `None` is folded into the same answer: it is the shape the item tree documents
/// for an absent expression, and both mean "optional, text unknown".
fn param_default(param: &hir::Param) -> ParamDefault {
    if !param.has_default {
        return ParamDefault::Required;
    }
    match &param.default_value {
        Some(text) if !text.trim().is_empty() => ParamDefault::Value(text.to_string()),
        _ => ParamDefault::Unknown,
    }
}

/// Project one region and its subtree, returning the nodes that belong on the PARENT's
/// level — a list rather than a single node, because a region declared inside a method body
/// is spliced away and its surviving children take its place.
///
/// A method-local region is a detail of a body, not the structure of a module, so the file
/// map does not show it. Removing it has two traps, and both are why this splices rather
/// than drops:
///
/// - a module-level region can be nested inside a method-local one (an unbalanced
///   `#Область` in a body), so dropping the subtree would take a real region with it;
/// - a method-local region's range can extend past its method and swallow the NEXT method,
///   so letting it collect items before it is dropped would take that method with it.
///
/// Hence: children first, no collecting for a spliced region, and every item it would have
/// covered stays in `items` for the nearest surviving ancestor — or the root.
fn build_region(
    region_tree: &hir::RegionTree,
    region_idx: hir::RegionIdx,
    items: &mut Vec<DocumentSymbol>,
) -> Vec<DocumentSymbol> {
    let region = region_tree.region(region_idx);

    let mut children: Vec<DocumentSymbol> = Vec::new();
    for &child_idx in region_tree.children(region_idx) {
        children.extend(build_region(region_tree, child_idx, items));
    }

    if region.is_method_local {
        return children;
    }

    let region_range = region.range;
    let mut i = 0;
    while i < items.len() {
        if region_range.contains_range(items[i].range) {
            children.push(items.remove(i));
        } else {
            i += 1;
        }
    }
    sort_by_position(&mut children);

    vec![DocumentSymbol {
        name: region.name.as_str().to_string(),
        range: region_range,
        selection_range: region.name_range,
        detail: SymbolDetail::Region,
        children,
    }]
}

/// Order a level the way the file reads it. Regions were built before the items they
/// collect, so without this a method declared above a region would be listed after it.
///
/// The sort is STABLE on purpose: two variables of one `Перем А, Б` share a `source_range`
/// to the byte, and items enter this list in declaration order, so only a stable sort is
/// contractually allowed to leave them as declared. (The unstable sort happens to keep an
/// all-equal run in place today, so no test can tell the two apart — the guarantee here is
/// the one the standard library gives, not one this crate observed.)
fn sort_by_position(symbols: &mut [DocumentSymbol]) {
    symbols.sort_by_key(|symbol| (symbol.range.start(), symbol.range.end()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use hir::DefDatabase;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::vfs::{file_set::FileSet, VfsPath};
    use ide_db::{RootDatabaseImpl, SymbolKind};

    fn setup_db(code: &str) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, code);
        (db, file_id)
    }

    /// The inputs every structural check below runs over. Each one is here because it
    /// distinguishes a right implementation from a wrong one that passes on ordinary code:
    /// regions inside bodies, an unbalanced region that swallows the next method, a module
    /// region nested under a method-local one, declarations whose order differs from the
    /// order the old builder produced, and two variables sharing one range.
    fn corpus() -> Vec<(&'static str, &'static str)> {
        vec![
            ("Ф1 экспорт и директивы", "&НаКлиенте\n&НаСервере\nПроцедура П(А) Экспорт\nКонецПроцедуры"),
            ("Ф2 параметры", "Процедура П(Знач А = 1, Б, Знач В) Экспорт КонецПроцедуры"),
            ("Ф3 умолчание без текста", "Процедура П(А = ) КонецПроцедуры"),
            (
                "Ф4 локаль и регистр",
                "процедура п() конецпроцедуры\nPROCEDURE P() ENDPROCEDURE\nФункция ф() КонецФункции\nfunction f() endfunction",
            ),
            (
                "Ф5 область внутри метода",
                "Процедура П()\n#Область Внутри\nА = 1;\n#КонецОбласти\nКонецПроцедуры",
            ),
            (
                "Ф6 метод-локальная область под модульной",
                "#Область Внешняя\nПроцедура П()\n#Область Внутри\nА = 1;\n#КонецОбласти\nКонецПроцедуры\n#КонецОбласти",
            ),
            (
                "Ф7 область из тела накрывает чужой метод",
                "Процедура П()\n#Область A\nКонецПроцедуры\nПроцедура Q()\nКонецПроцедуры\n#КонецОбласти",
            ),
            (
                "Ф8 метод объявлен раньше области",
                "Процедура Первый()\nКонецПроцедуры\n#Область Служебные\nПроцедура Второй()\nКонецПроцедуры\n#КонецОбласти",
            ),
            (
                "Ф9 переменные с равными диапазонами",
                "#Область Служебные\n&НаКлиенте\nПерем А, Б Экспорт;\nПроцедура П() Экспорт\nКонецПроцедуры\n#КонецОбласти",
            ),
            (
                "Ф10 модульная область под метод-локальной",
                "Процедура П()\n#Область A\nКонецПроцедуры\n#Область B\nПроцедура Q()\nКонецПроцедуры\n#КонецОбласти\n#КонецОбласти",
            ),
            (
                "Ф11 вложенные модульные области",
                "#Область Внешняя\n#Область Внутренняя\nПроцедура П()\nКонецПроцедуры\n#КонецОбласти\n#КонецОбласти",
            ),
            ("Ф12 пустой файл", ""),
            (
                "Ф13 элемент раньше вложенной области",
                "#Область Внешняя\nПроцедура П()\nКонецПроцедуры\n#Область Внутренняя\nПроцедура Q()\nКонецПроцедуры\n#КонецОбласти\n#КонецОбласти",
            ),
        ]
    }

    fn walk<'a>(symbols: &'a [DocumentSymbol], out: &mut Vec<&'a DocumentSymbol>) {
        for symbol in symbols {
            out.push(symbol);
            walk(&symbol.children, out);
        }
    }

    fn flatten(symbols: &[DocumentSymbol]) -> Vec<&DocumentSymbol> {
        let mut out = Vec::new();
        walk(symbols, &mut out);
        out
    }

    fn is_ancestor(parent: &DocumentSymbol, target: &DocumentSymbol) -> bool {
        parent
            .children
            .iter()
            .any(|child| std::ptr::eq(child, target) || is_ancestor(child, target))
    }

    /// The map must not contradict the text it describes: a name lies inside its own node,
    /// a child lies inside its parent, and — the one that catches a flattened tree — a node
    /// whose range strictly contains another's is that node's ancestor, not its sibling.
    fn assert_outline_invariants(label: &str, symbols: &[DocumentSymbol]) {
        let all = flatten(symbols);
        for symbol in &all {
            assert!(
                symbol.range.contains_range(symbol.selection_range),
                "{label}: имя вне узла у {:?}",
                symbol.name,
            );
            for child in &symbol.children {
                assert!(
                    symbol.range.contains_range(child.range),
                    "{label}: ребёнок {:?} вне родителя {:?}",
                    child.name,
                    symbol.name,
                );
            }
        }
        for outer in &all {
            for inner in &all {
                if std::ptr::eq(*outer, *inner) || outer.range == inner.range {
                    continue;
                }
                if outer.range.contains_range(inner.range) {
                    assert!(
                        is_ancestor(outer, inner),
                        "{label}: {:?} содержит {:?}, но не предок ему",
                        outer.name,
                        inner.name,
                    );
                }
            }
        }
    }

    #[test]
    fn the_map_never_contradicts_the_text() {
        for (label, code) in corpus() {
            let (db, file_id) = setup_db(code);
            assert_outline_invariants(label, &document_symbols(&db, file_id));
        }
    }

    /// Removing regions declared inside bodies must not remove anything else. `item_tree`
    /// is the source of truth for what the file declares, so the map is compared against it
    /// rather than against another hand-written list.
    #[test]
    fn no_method_is_lost_or_duplicated() {
        for (label, code) in corpus() {
            let (db, file_id) = setup_db(code);
            let item_tree = db.item_tree(file_id);

            let mut declared: Vec<(String, u32, u32)> = item_tree
                .top_level_items()
                .iter()
                .filter_map(|item| match *item {
                    ModItem::Procedure(idx) => {
                        let proc = item_tree.procedure(idx);
                        Some((proc.name.as_str().to_string(), proc.source_range))
                    }
                    ModItem::Function(idx) => {
                        let func = item_tree.function(idx);
                        Some((func.name.as_str().to_string(), func.source_range))
                    }
                    ModItem::Variable(_) => None,
                })
                .map(|(name, range)| (name, range.start().into(), range.end().into()))
                .collect();

            let symbols = document_symbols(&db, file_id);
            let mut served: Vec<(String, u32, u32)> = flatten(&symbols)
                .into_iter()
                .filter(|s| matches!(s.kind(), SymbolKind::Procedure | SymbolKind::Function))
                .map(|s| (s.name.clone(), s.range.start().into(), s.range.end().into()))
                .collect();

            declared.sort();
            served.sort();
            assert_eq!(served, declared, "{label}: карта разошлась с объявленным");
        }
    }

    /// A region declared inside a method body is a detail of that body, so it is not part
    /// of the module's map — at any depth, not just at the top level.
    #[test]
    fn a_region_declared_inside_a_body_is_not_in_the_map() {
        let region_names = |code: &str| -> Vec<String> {
            let (db, file_id) = setup_db(code);
            let symbols = document_symbols(&db, file_id);
            flatten(&symbols)
                .into_iter()
                .filter(|s| s.kind() == SymbolKind::Region)
                .map(|s| s.name.clone())
                .collect()
        };
        let by_label: std::collections::HashMap<&str, &str> = corpus().into_iter().collect();

        assert!(region_names(by_label["Ф5 область внутри метода"]).is_empty());
        assert!(region_names(by_label["Ф7 область из тела накрывает чужой метод"]).is_empty());
        // The module-level region survives its method-local parent...
        assert_eq!(region_names(by_label["Ф10 модульная область под метод-локальной"]), ["B"]);
        // ...and the module-level parent survives its method-local child.
        assert_eq!(region_names(by_label["Ф6 метод-локальная область под модульной"]), ["Внешняя"]);
    }

    /// The map is a map of a FILE, so each level reads in the order the file declares —
    /// including inside a region, where the old builder put nested regions before the
    /// items that precede them.
    #[test]
    fn every_level_reads_in_declaration_order() {
        let by_label: std::collections::HashMap<&str, &str> = corpus().into_iter().collect();
        let names = |symbols: &[DocumentSymbol]| -> Vec<String> {
            symbols.iter().map(|s| s.name.clone()).collect()
        };

        let (db, file_id) = setup_db(by_label["Ф8 метод объявлен раньше области"]);
        let roots = document_symbols(&db, file_id);
        assert_eq!(names(&roots), ["Первый", "Служебные"]);

        let (db, file_id) = setup_db(by_label["Ф13 элемент раньше вложенной области"]);
        let roots = document_symbols(&db, file_id);
        assert_eq!(names(&roots), ["Внешняя"]);
        assert_eq!(names(&roots[0].children), ["П", "Внутренняя"]);

        // Equal ranges: `Перем А, Б` gives both variables one and the same range, so only a
        // stable sort keeps them in the order they are written.
        let (db, file_id) = setup_db(by_label["Ф9 переменные с равными диапазонами"]);
        let roots = document_symbols(&db, file_id);
        assert_eq!(names(&roots[0].children), ["А", "Б", "П"]);
    }

    /// The map as text, one line per node: kind, name, then everything the node carries
    /// beyond its name. Printed through the canonical spellings, so a renamed wire value
    /// shows up here rather than in a consumer.
    fn render(symbols: &[DocumentSymbol]) -> String {
        fn line(symbol: &DocumentSymbol, depth: usize, out: &mut String) {
            let indent = "  ".repeat(depth);
            let kind = symbol.kind().as_str();
            let mut extra = String::new();
            match &symbol.detail {
                SymbolDetail::Procedure(method) | SymbolDetail::Function(method) => {
                    if method.is_export {
                        extra.push_str(" экспорт");
                    }
                    for directive in &method.directives {
                        extra.push_str(&format!(" &{}", directive.as_str()));
                    }
                    for param in &method.params {
                        let value = match &param.default {
                            ParamDefault::Required => "обязательный".to_string(),
                            ParamDefault::Value(text) => format!("= {text}"),
                            ParamDefault::Unknown => "= <неизвестно>".to_string(),
                        };
                        let by_value = if param.by_value { "Знач " } else { "" };
                        extra.push_str(&format!(
                            "\n{indent}    параметр {by_value}{}: {value}",
                            param.name
                        ));
                    }
                }
                SymbolDetail::Variable(variable) => {
                    if variable.is_export {
                        extra.push_str(" экспорт");
                    }
                    for directive in &variable.directives {
                        extra.push_str(&format!(" &{}", directive.as_str()));
                    }
                }
                SymbolDetail::Region => {}
            }
            out.push_str(&format!("{indent}{kind} {}{extra}\n", symbol.name));
            for child in &symbol.children {
                line(child, depth + 1, out);
            }
        }

        let mut out = String::new();
        for symbol in symbols {
            line(symbol, 0, &mut out);
        }
        out
    }

    fn outline_of(code: &str) -> String {
        let (db, file_id) = setup_db(code);
        render(&document_symbols(&db, file_id))
    }

    /// The kind is the parsed item's, not the declaration's wording: BSL spells the same
    /// construct in two languages and any case, and a kind read off the text would differ
    /// per file for the same thing.
    #[test]
    fn the_kind_survives_language_and_case() {
        let by_label: std::collections::HashMap<&str, &str> = corpus().into_iter().collect();
        expect_test::expect![[r#"
            procedure п
            procedure P
            function ф
            function f
        "#]]
        .assert_eq(&outline_of(by_label["Ф4 локаль и регистр"]));
    }

    /// Export, directives and the parameter list travel with the node. Without them a
    /// consumer has to open the file again to learn what the map was supposed to tell it.
    #[test]
    fn a_method_carries_its_declaration() {
        let by_label: std::collections::HashMap<&str, &str> = corpus().into_iter().collect();

        expect_test::expect![[r#"
            procedure П экспорт &at_client &at_server
                параметр А: обязательный
        "#]]
        .assert_eq(&outline_of(by_label["Ф1 экспорт и директивы"]));

        expect_test::expect![[r#"
            procedure П экспорт
                параметр Знач А: = 1
                параметр Б: обязательный
                параметр Знач В: обязательный
        "#]]
        .assert_eq(&outline_of(by_label["Ф2 параметры"]));

        expect_test::expect![[r#"
            region Служебные
              variable А &at_client
              variable Б экспорт &at_client
              procedure П экспорт
        "#]]
        .assert_eq(&outline_of(by_label["Ф9 переменные с равными диапазонами"]));
    }

    /// `=` with an expression the parser could not read is still an OPTIONAL parameter.
    /// Calling it required would change the arity a consumer derives from the signature —
    /// and the parser hands such a default over as empty text, not as no text, so a check
    /// against `None` alone would never notice.
    #[test]
    fn a_default_whose_text_is_unreadable_stays_optional() {
        let by_label: std::collections::HashMap<&str, &str> = corpus().into_iter().collect();
        expect_test::expect![[r#"
            procedure П
                параметр А: = <неизвестно>
        "#]]
        .assert_eq(&outline_of(by_label["Ф3 умолчание без текста"]));
    }

    /// The narrowed mode must be the SAME skeleton, not a second one that resembles it:
    /// same names, same ranges, same nesting, same order — with everything that is not a
    /// region gone at every depth, not just at the top.
    #[test]
    fn regions_only_is_the_full_map_with_everything_else_sifted_out() {
        fn skeleton(symbols: &[DocumentSymbol], depth: usize, out: &mut String) {
            for region in symbols.iter().filter(|symbol| symbol.kind() == SymbolKind::Region) {
                let indent = "  ".repeat(depth);
                let start: u32 = region.range.start().into();
                let end: u32 = region.range.end().into();
                out.push_str(&format!("{indent}{} {start}..{end}\n", region.name));
                skeleton(&region.children, depth + 1, out);
            }
        }

        let rendered = |symbols: &[DocumentSymbol]| {
            let mut out = String::new();
            skeleton(symbols, 0, &mut out);
            out
        };

        for (label, code) in corpus() {
            let (db, file_id) = setup_db(code);
            let full = file_outline(&db, file_id, OutlineMode::Full);
            let regions = file_outline(&db, file_id, OutlineMode::RegionsOnly);

            assert_eq!(rendered(&regions), rendered(&full), "{label}: скелет областей разошёлся");
            assert!(
                flatten(&regions).iter().all(|s| s.kind() == SymbolKind::Region),
                "{label}: в режиме областей остался не-область",
            );
        }
    }

    #[test]
    fn test_empty_file() {
        let (db, file_id) = setup_db("");
        let symbols = document_symbols(&db, file_id);
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_procedures_and_functions() {
        let (db, file_id) = setup_db(
            r#"Процедура Проц1()
КонецПроцедуры

Функция Функ1()
КонецФункции"#,
        );
        let symbols = document_symbols(&db, file_id);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "Проц1");
        assert_eq!(symbols[0].kind(), SymbolKind::Procedure);
        assert_eq!(symbols[1].name, "Функ1");
        assert_eq!(symbols[1].kind(), SymbolKind::Function);
    }

    #[test]
    fn test_variables() {
        let (db, file_id) = setup_db("Перем МояПеременная Экспорт;");
        let symbols = document_symbols(&db, file_id);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "МояПеременная");
        assert_eq!(symbols[0].kind(), SymbolKind::Variable);
    }

    #[test]
    fn test_regions_with_nested_items() {
        let (db, file_id) = setup_db(
            r#"#Область ПрограммныйИнтерфейс

Процедура Проц1()
КонецПроцедуры

#КонецОбласти

Процедура Проц2()
КонецПроцедуры"#,
        );
        let symbols = document_symbols(&db, file_id);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "ПрограммныйИнтерфейс");
        assert_eq!(symbols[0].kind(), SymbolKind::Region);
        assert_eq!(symbols[0].children.len(), 1);
        assert_eq!(symbols[0].children[0].name, "Проц1");
        assert_eq!(symbols[1].name, "Проц2");
    }
}
