//! Salsa queries producing the *effective* module of a configuration extension's
//! `&ИзменениеИКонтроль` modules (Phase 2, increment 2).
//!
//! An effective module is the base module text with each
//! `&ИзменениеИКонтроль("M")` extension method's directive-stripped body spliced
//! into the body of base method `M`. The result reparses as one coherent module,
//! so the base method's siblings are in scope for inserted code — that is the
//! fix for the spurious diagnostics an extension method gets when analyzed in
//! isolation.
//!
//! The heavy splice logic lives in the db-free [`assemble_effective`] helper so it
//! can be unit-tested without a Salsa database; the tracked queries are thin
//! wrappers over it. The pure merge primitives it builds on live in
//! [`crate::extension_merge`].

use std::sync::Arc;

use stdx::case::CaseExt;
use syntax::{
    ast::{self, AstNode},
    Parse, SyntaxKind, SyntaxNode, TextRange, TextSize,
};

use crate::sdbl_cache::{all_sdbl_in_file_heap, sdbl_hir_for_file_heap};
use crate::{
    extension_merge::{extract_change_and_validate, strip_directives, Segment},
    item_tree::ItemTree,
    symbol_tree::SymbolTree,
    ModuleBodies, ModuleId,
};

/// Interned identity of an effective module: the (base, extension) file pair it is
/// merged from. Mirrors the `#[salsa::interned(debug)]` style of `MethodIdInput`.
#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct EffectiveModuleId<'db> {
    #[returns(copy)]
    pub base_file: vfs::FileId,
    #[returns(copy)]
    pub ext_file: vfs::FileId,
}

/// The merged effective module text plus the segment map remapping spans of that
/// text back to the extension source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveModule {
    pub text: Arc<str>,
    /// Segments mapping spans of `text` back to the extension source, in FULL
    /// effective-text coordinates (each `strip_directives` segment offset by where
    /// its body was spliced into the base text).
    pub segments: Vec<Segment>,
    pub base_file: vfs::FileId,
    pub ext_file: vfs::FileId,
}

/// One resolved replacement: the base method's `STMT_LIST` range to overwrite plus
/// the stripped extension body and its segment map (in stripped-body coordinates).
struct Replacement {
    base_body_range: TextRange,
    stripped_body: String,
    segments: Vec<Segment>,
}

/// Pure splice engine: build the effective module text from the base text and the
/// two parses. Db-free so the merge can be unit-tested in isolation.
///
/// Returns `None` when the extension module carries no usable
/// `&ИзменениеИКонтроль` method (no change-and-validate methods at all, or none
/// whose target resolves to a base method) — the caller then keeps standalone
/// behavior.
fn assemble_effective(
    base_text: &str,
    base_parse: &Parse<SyntaxNode>,
    ext_parse: &Parse<SyntaxNode>,
) -> Option<(String, Vec<Segment>)> {
    // Collect every change-and-validate method's target + directive-stripped body.
    let mut changes: Vec<(String, String, Vec<Segment>)> = Vec::new();
    for method in ext_parse
        .syntax_node()
        .descendants()
        .filter(|n| matches!(n.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
    {
        let Some(cc) = extract_change_and_validate(&method) else { continue };
        let Some(stmt_list) = method.children().find(|n| n.kind() == SyntaxKind::STMT_LIST) else {
            continue;
        };
        let (stripped_body, segs) = strip_directives(&stmt_list);
        changes.push((cc.target, stripped_body, segs));
    }
    if changes.is_empty() {
        return None;
    }

    // Resolve each target to the base method's STMT_LIST range. Missing targets are
    // skipped (degrade) rather than failing the whole module.
    let mut replacements: Vec<Replacement> = Vec::new();
    for (target, stripped_body, segments) in changes {
        match base_method_body_range(base_parse, &target) {
            Some(base_body_range) => {
                replacements.push(Replacement { base_body_range, stripped_body, segments })
            }
            None => {
                tracing::debug!(target = %target, "change-and-validate target not found in base")
            }
        }
    }
    if replacements.is_empty() {
        return None;
    }

    // Splice left-to-right so spliced bodies land at correct absolute offsets.
    replacements.sort_by_key(|r| r.base_body_range.start());

    let mut output = String::new();
    let mut out_segments: Vec<Segment> = Vec::new();
    let mut cursor: usize = 0;
    for r in replacements {
        let start = usize::from(r.base_body_range.start());
        // Defensive: distinct methods cannot overlap, but a malformed resolution
        // that landed inside an already-spliced range is dropped to keep offsets
        // monotonic.
        if start < cursor {
            tracing::debug!("overlapping change-and-validate replacement skipped");
            continue;
        }
        output.push_str(&base_text[cursor..start]);

        let splice_base = TextSize::new(output.len() as u32);
        output.push_str(&r.stripped_body);
        for seg in r.segments {
            out_segments.push(Segment {
                effective: TextRange::new(
                    seg.effective.start() + splice_base,
                    seg.effective.end() + splice_base,
                ),
                ext: seg.ext,
                origin: seg.origin,
            });
        }
        cursor = usize::from(r.base_body_range.end());
    }
    output.push_str(&base_text[cursor..]);

    Some((output, out_segments))
}

/// Find the `STMT_LIST` range of base method `target` (case-insensitive, bilingual
/// match — BSL identifiers fold both ASCII and Cyrillic case).
fn base_method_body_range(base_parse: &Parse<SyntaxNode>, target: &str) -> Option<TextRange> {
    let target_lc = target.fold_lower();
    for node in base_parse.syntax_node().descendants() {
        let (name, body) = match node.kind() {
            SyntaxKind::PROCEDURE_DEF => {
                let def = ast::ProcedureDef::cast(node)?;
                (def.name(), def.body())
            }
            SyntaxKind::FUNCTION_DEF => {
                let def = ast::FunctionDef::cast(node)?;
                (def.name(), def.body())
            }
            _ => continue,
        };
        let Some(name) = name else { continue };
        if name.text().fold_lower() == target_lc {
            return Some(body?.syntax().text_range());
        }
    }
    None
}

/// Compute the effective module text + segment map for an extension/base file pair.
/// `None` when the extension module yields no usable merge (callers gate on this).
#[salsa::tracked(returns(clone))]
pub fn effective_module_text<'db>(
    db: &'db dyn crate::DefDatabase,
    eid: EffectiveModuleId<'db>,
) -> Option<Arc<EffectiveModule>> {
    let base_file = eid.base_file(db);
    let ext_file = eid.ext_file(db);
    let _span = tracing::info_span!("effective_module_text", ?base_file, ?ext_file).entered();

    let base_text = db.file_text(base_file);
    let base_parse = db.parse_ref(base_file);
    let ext_parse = db.parse_ref(ext_file);

    let (text, segments) = assemble_effective(&base_text, base_parse, ext_parse)?;
    Some(Arc::new(EffectiveModule { text: Arc::from(text), segments, base_file, ext_file }))
}

/// Parse over the effective text. When there is no effective merge this falls back
/// to the base parse so the query stays total; callers gate on
/// [`effective_module_text`] being `Some`, so the fallback value is never the one
/// consumed in the merged path.
#[salsa::tracked(returns(clone))]
pub fn parse_effective<'db>(
    db: &'db dyn crate::DefDatabase,
    eid: EffectiveModuleId<'db>,
) -> Parse<SyntaxNode> {
    let Some(em) = effective_module_text(db, eid) else {
        return db.parse(eid.base_file(db));
    };
    parser::parse_with_shared_cache(&em.text)
}

/// `ItemTree` over the effective parse.
#[salsa::tracked(returns(clone))]
pub fn item_tree_effective<'db>(
    db: &'db dyn crate::DefDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<ItemTree> {
    Arc::new(ItemTree::from_parse(&parse_effective(db, eid)))
}

/// `SymbolTree` over the effective parse. The effective module's identity IS the
/// base module, so it carries the base file's `ModuleId`.
#[salsa::tracked(returns(clone))]
pub fn symbol_tree_effective<'db>(
    db: &'db dyn crate::DefDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<SymbolTree> {
    let interface = module_interface_effective(db, eid);
    let item_tree = item_tree_effective(db, eid);
    Arc::new(SymbolTree::assemble(&interface, &item_tree))
}

/// `ModuleInterface` over the effective parse, carrying the base file's `ModuleId`
/// like [`symbol_tree_effective`].
#[salsa::tracked(returns(clone))]
pub fn module_interface_effective<'db>(
    db: &'db dyn crate::DefDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<crate::module_interface::ModuleInterface> {
    let base = eid.base_file(db);
    let parse = parse_effective(db, eid);
    let item_tree = item_tree_effective(db, eid);
    let source_text: Arc<str> = match effective_module_text(db, eid) {
        Some(em) => em.text.clone(),
        None => db.file_text(base),
    };
    let conditionals = item_tree
        .has_module_preproc()
        .then(|| crate::conditional_tree::lower_conditionals(&parse.syntax_node()));
    Arc::new(crate::module_interface::ModuleInterface::from_item_tree(
        &item_tree,
        ModuleId::new(base),
        &parse,
        &source_text,
        conditionals.as_ref(),
    ))
}

/// `ModuleBodies` over the effective parse, carrying the base module's `ModuleId`.
/// The line index is built from the effective text (not `db.file_text`, which would
/// be the base file's), so line-dependent lowering — method size, complexity — sees
/// the merged module exactly as it reparses.
#[salsa::tracked(returns(clone))]
pub fn module_bodies_effective<'db>(
    db: &'db dyn crate::DefDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<ModuleBodies> {
    let base = eid.base_file(db);
    let parse = parse_effective(db, eid);
    let source_text: Arc<str> = match effective_module_text(db, eid) {
        Some(em) => em.text.clone(),
        None => db.file_text(base),
    };
    Arc::new(ModuleBodies::from_parse_with_text(&parse, &source_text))
}

/// Base-method identities whose bodies compile from the extension's replacements.
pub fn changed_method_keys(
    db: &dyn crate::DefDatabase,
    eid: EffectiveModuleId<'_>,
) -> rustc_hash::FxHashSet<crate::MethodKey> {
    let extension = db.parse(eid.ext_file(db));
    let targets: rustc_hash::FxHashSet<String> = extension
        .syntax_node()
        .children()
        .filter(|node| matches!(node.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF))
        .filter_map(|node| extract_change_and_validate(&node))
        .map(|change| change.target.fold_lower())
        .collect();
    module_interface_effective(db, eid)
        .methods()
        .filter(|method| targets.contains(&method.name.as_str().fold_lower()))
        .map(|method| method.id.local_id)
        .collect()
}

/// Query literals in the effective text, with the same identities as its lowered bodies.
#[salsa::tracked(lru = 128, heap_size = all_sdbl_in_file_heap, returns(clone))]
pub fn all_sdbl_in_effective_file<'db>(
    db: &'db dyn crate::DefDatabase,
    eid: EffectiveModuleId<'db>,
) -> Arc<crate::sdbl_cache::SdblInFile> {
    Arc::new(crate::sdbl_cache::queries_in_bodies(&module_bodies_effective(db, eid)))
}

/// Query types and diagnostics resolved in each effective body's compilation context.
#[salsa::tracked(lru = 64, heap_size = sdbl_hir_for_file_heap, returns(clone))]
pub fn sdbl_hir_for_effective_file<'db>(
    db: &'db dyn crate::ConfigsDatabase,
    eid: EffectiveModuleId<'db>,
) -> crate::sdbl_cache::SdblHirEntries {
    let changed = changed_method_keys(db, eid);
    let bodies = module_bodies_effective(db, eid);
    let mut result = Vec::new();
    for (method, body) in bodies.iter_bodies() {
        let context = if changed.contains(&method) { eid.ext_file(db) } else { eid.base_file(db) };
        result.extend(
            crate::sdbl_cache::lower_body_queries(db, context, body)
                .into_iter()
                .map(|(expr, package)| (crate::SdblExprId::from_method(method, expr), package)),
        );
    }
    if let Some(body) = bodies.module_code() {
        result.extend(
            crate::sdbl_cache::lower_body_queries(db, eid.base_file(db), body)
                .into_iter()
                .map(|(expr, package)| (crate::SdblExprId::from_module_code(expr), package)),
        );
    }
    Arc::new(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension_merge::Origin;
    use crate::name::Name;

    fn parse(code: &str) -> Parse<SyntaxNode> {
        parser::parse_with_shared_cache(code)
    }

    fn assemble(base: &str, ext: &str) -> Option<(String, Vec<Segment>)> {
        assemble_effective(base, &parse(base), &parse(ext))
    }

    const BASE_POWER: &str = "\
Функция ВозведениеВСтепень(Основание, Степень)
\tРезультат = 1;
\tДля Индекс = 1 По Степень Цикл
\t\tРезультат = Результат + Основание;
\tКонецЦикла;
\tВозврат Результат;
КонецФункции

Функция Хелпер()
\tВозврат 0;
КонецФункции";

    const EXT_POWER: &str = "\
&ИзменениеИКонтроль(\"ВозведениеВСтепень\")
Функция Расш1_ВозведениеВСтепень(Основание, Степень)
\tРезультат = 1;
\tДля Индекс = 1 По Степень Цикл
#Удаление
\t\tРезультат = Результат + Основание;
#КонецУдаления
#Вставка
\t\tРезультат = Результат * Основание;
#КонецВставки
\tКонецЦикла;
\tВозврат Результат;
КонецФункции";

    #[test]
    fn power_example_splices_into_base_signature() {
        let (text, segments) = assemble(BASE_POWER, EXT_POWER).expect("a merge");

        assert!(text.contains("* Основание"), "insertion present:\n{text}");
        assert!(!text.contains("+ Основание"), "deletion dropped:\n{text}");
        assert!(text.contains("Функция Хелпер()"), "untouched sibling preserved:\n{text}");
        // Base signature line preserved; the extension method's own name is gone.
        assert!(
            text.contains("ВозведениеВСтепень(Основание, Степень)"),
            "base signature preserved:\n{text}"
        );
        assert!(!text.contains("Расш1_"), "extension method name not present:\n{text}");

        let inserted: Vec<&Segment> =
            segments.iter().filter(|s| s.origin == Origin::Inserted).collect();
        assert_eq!(inserted.len(), 1, "exactly one inserted run");
        let seg = inserted[0];
        // The segment offset must address the inserted slice inside the FULL effective text.
        let eff_slice = &text[usize::from(seg.effective.start())..usize::from(seg.effective.end())];
        let ext_slice = &EXT_POWER[usize::from(seg.ext.start())..usize::from(seg.ext.end())];
        assert_eq!(eff_slice, ext_slice);
        assert!(eff_slice.contains("* Основание"), "inserted seg covers fixed line: {eff_slice:?}");
    }

    #[test]
    fn inserted_code_sees_base_siblings() {
        let base = "\
Функция А()
\tВозврат 1;
КонецФункции

Функция Б()
\tВозврат 0;
КонецФункции";
        let ext = "\
&ИзменениеИКонтроль(\"Б\")
Функция Расш1_Б()
#Вставка
\tРезультат = А();
#КонецВставки
\tВозврат Результат;
КонецФункции";
        let (text, _) = assemble(base, ext).expect("a merge");

        // Build symbol tree on the assembled text via the pure constructors: both
        // base siblings must be visible in the effective module, which is the
        // false-positive fix at the pure level.
        let eff_parse = parse(&text);
        let item_tree = ItemTree::from_parse(&eff_parse);
        let module_id = ModuleId::new(vfs::FileId::from_raw(0));
        let symbols = SymbolTree::from_item_tree(&item_tree, module_id, &eff_parse, &text);

        assert!(symbols.find_method(&Name::new("А")).is_some(), "base method А visible");
        assert!(symbols.find_method(&Name::new("Б")).is_some(), "base method Б visible");
    }

    #[test]
    fn ext_without_change_control_yields_none() {
        let base = "Функция А()\nВозврат 1;\nКонецФункции";
        let ext = "&НаКлиенте\nПроцедура Тест()\nКонецПроцедуры";
        assert_eq!(assemble(base, ext), None);
    }

    #[test]
    fn missing_target_yields_none() {
        let base = "Функция А()\nВозврат 1;\nКонецФункции";
        let ext = "&ИзменениеИКонтроль(\"НетТакого\")\n\
Функция Расш1_НетТакого()\n\
#Вставка\n\
Х = 1;\n\
#КонецВставки\n\
Возврат 0;\n\
КонецФункции";
        assert_eq!(assemble(base, ext), None);
    }

    #[test]
    fn two_targets_both_spliced() {
        let base = "\
Функция Первая()
\tВозврат 1;
КонецФункции

Функция Вторая()
\tВозврат 2;
КонецФункции

Функция Сосед()
\tВозврат 0;
КонецФункции";
        let ext = "\
&ИзменениеИКонтроль(\"Первая\")
Функция Расш1_Первая()
#Вставка
\tМеткаА = 11;
#КонецВставки
\tВозврат 1;
КонецФункции

&ИзменениеИКонтроль(\"Вторая\")
Функция Расш1_Вторая()
#Вставка
\tМеткаБ = 22;
#КонецВставки
\tВозврат 2;
КонецФункции";
        let (text, segments) = assemble(base, ext).expect("a merge");

        assert!(text.contains("МеткаА = 11;"), "first insertion present:\n{text}");
        assert!(text.contains("МеткаБ = 22;"), "second insertion present:\n{text}");
        assert!(text.contains("Функция Сосед()"), "untouched sibling preserved:\n{text}");
        assert_eq!(
            segments.iter().filter(|s| s.origin == Origin::Inserted).count(),
            2,
            "two inserted runs"
        );
    }
}
