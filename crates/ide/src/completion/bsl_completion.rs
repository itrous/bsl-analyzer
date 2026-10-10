use std::collections::HashSet;
use stdx::case::CaseExt;

use bsl_metadata::MdoType;
use bsl_platform::{GlobalFunction, PlatformDataInner, PlatformGlobalCatalog, PlatformGlobalKind};
use either::Either;
use hir::{DefWithBodyId, ExprScopes, ScopeDef};
use ide_db::base_db::Locale;
use ide_db::{RootDatabase, TextRange};
use rustc_hash::FxHashMap;
use syntax::{ast::AstNode, SyntaxKind};

use super::env_filter::EnvFilter;
use super::fuzzy::{MatchResult, MatchTier, PrefixMatcher};
use super::{templates, CompletionItem, CompletionItemKind, CompletionPosition};
use crate::completion::platform_completion::{render_mdo_field, render_platform_property};

pub(super) fn bsl_completions<DB: RootDatabase>(
    db: &DB,
    position: CompletionPosition,
) -> Option<Vec<CompletionItem>> {
    let _span = tracing::info_span!("bsl_completions").entered();

    tracing::info!("bsl_completions called");

    let parse = db.parse(position.file_id);
    let root = parse.syntax_node();

    let token = match root.token_at_offset(position.offset) {
        syntax::TokenAtOffset::None => {
            tracing::info!("No token at position - returning None");
            return None;
        }
        syntax::TokenAtOffset::Single(t) => t,
        syntax::TokenAtOffset::Between(left, right) => {
            if left.kind() == SyntaxKind::IDENT || left.kind().is_keyword() {
                left
            } else {
                right
            }
        }
    };

    tracing::debug!(token_kind = ?token.kind(), token_text = ?token.text(), "BSL completion token");

    if let Some(prev) = token.prev_sibling_or_token() {
        if prev.kind() == SyntaxKind::DOT {
            tracing::info!("After DOT - skipping BSL completion");
            return None;
        }
    }

    tracing::debug!("Not after DOT, checking if typing...");

    let token_text = token.text();

    let is_typing = if token.kind() == SyntaxKind::IDENT {
        true
    } else if token.kind().is_keyword() {
        let token_start = token.text_range().start();
        let cursor_in_token = position.offset.checked_sub(token_start);
        if let Some(offset_in_token) = cursor_in_token {
            let offset_in_token: usize = offset_in_token.into();
            offset_in_token < token_text.len()
        } else {
            false
        }
    } else {
        false
    };

    tracing::debug!(is_typing = is_typing, "Checked is_typing");

    if is_typing {
        let token_start = token.text_range().start();
        let cursor_in_token = position.offset.checked_sub(token_start).unwrap_or_default();
        let cursor_in_token: usize = cursor_in_token.into();

        let prefix = &token_text[..cursor_in_token.min(token_text.len())];

        tracing::info!(
            prefix = ?prefix,
            token_kind = ?token.kind(),
            full_text = ?token_text,
            "Completing with prefix"
        );

        let mut completions = Vec::new();

        if token.kind().is_keyword() {
            let (detail, documentation) = get_keyword_info(token_text);
            // The cursor sits on a fully-typed keyword: an exact self-match, so
            // rank it in the best tier within the template/keyword band to keep
            // the composite-sort-key contract uniform across all items.
            let exact = MatchResult { tier: MatchTier::Prefix, score: u16::MAX };
            let keyword_item = CompletionItem {
                label: token_text.to_string(),
                detail: Some(detail),
                kind: CompletionItemKind::Keyword,
                insert_text: token_text.to_string(),
                documentation: Some(documentation),
                sort_text: Some(sort_key(exact, TEMPLATE_BAND, '1', '1', token_text)),
                filter_text: None,
                source: None,
            };
            completions.push(keyword_item);
        }

        completions.extend(complete_top_level(
            db,
            position.file_id,
            position.offset,
            prefix,
            position.locale,
        ));

        tracing::info!(count = completions.len(), "Returning BSL completions");
        return Some(completions);
    }

    if is_expression_start_position(&token) {
        tracing::info!(token_kind = ?token.kind(), "Expression start position - completing with empty prefix");
        let completions =
            complete_top_level(db, position.file_id, position.offset, "", position.locale);

        tracing::info!(count = completions.len(), "Returning BSL completions (trigger position)");
        return Some(completions);
    }

    tracing::info!("No BSL completion context - returning None");
    None
}

/// Band prefix for code templates / keywords: below the user's own symbols
/// (locals, module members) but above metadata and platform globals, so the
/// constructs you reach for sit near the top without burying your own names.
const TEMPLATE_BAND: &str = "18_";

fn complete_top_level<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    offset: syntax::TextSize,
    prefix: &str,
    locale: Locale,
) -> Vec<CompletionItem> {
    let _span = tracing::debug_span!("complete_top_level").entered();

    let mut matcher = PrefixMatcher::new(prefix);
    let freq = build_frequency(db, file_id);
    let env_filter = EnvFilter::at(db, file_id, offset);
    let in_method = cursor_in_method(db, file_id, offset);
    // Expected type(s) at the cursor (call argument, assignment RHS, or return) —
    // used to float type-matching candidates up (RDT1C's biggest rating factor).
    let expected = hir::Semantics::new(db).expected_types_at(file_id, offset);
    // Computing a function's return type to type it as a candidate is only worth
    // it when there is an expected type and the prefix narrows the list.
    let want_fn_types = !expected.is_empty() && !prefix.is_empty();

    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let locals = complete_local_symbols(db, file_id, offset, &matcher);
    push_band(&mut out, &mut seen, &mut matcher, db, &freq, &expected, "00_", locals);
    let user = complete_user_defined_symbols(db, file_id, &matcher, want_fn_types, &env_filter);
    push_band(&mut out, &mut seen, &mut matcher, db, &freq, &expected, "10_", user);
    let self_attrs = untyped(complete_module_self_attributes(db, file_id, &matcher, locale));
    push_band(&mut out, &mut seen, &mut matcher, db, &freq, &expected, "15_", self_attrs);
    let plurals = untyped(complete_mdo_plurals(&matcher, &env_filter));
    push_band(&mut out, &mut seen, &mut matcher, db, &freq, &expected, "20_", plurals);
    let (export_items, blocked_exports) =
        complete_global_module_exports(db, file_id, &matcher, &env_filter);
    let exports = untyped(export_items);
    push_band(&mut out, &mut seen, &mut matcher, db, &freq, &expected, "24_", exports);
    // A filtered-out global export still shadows the same-named platform
    // symbol at call time (Global-CM → Platform precedence), so the label
    // must not resurface from the platform band.
    for label in blocked_exports {
        seen.insert(label.fold_lower());
    }
    let globals = untyped(complete_hbk_globals(db, file_id, &matcher, locale, &env_filter));
    push_band(&mut out, &mut seen, &mut matcher, db, &freq, &expected, "25_", globals);
    let modules = untyped(complete_user_common_modules(db, file_id, &matcher, &env_filter));
    push_band(&mut out, &mut seen, &mut matcher, db, &freq, &expected, "30_", modules);

    for scored in templates::complete_templates(&mut matcher, in_method) {
        let mut item = scored.item;
        if !seen.insert(item.label.fold_lower()) {
            continue;
        }
        item.sort_text = Some(sort_key(scored.result, TEMPLATE_BAND, '1', '1', &item.label));
        out.push(item);
    }

    out
}

/// Candidate type carried alongside a completion item so the central ranking can
/// apply the context-type boost. `None` for bands whose types we don't compute.
type Candidate = (CompletionItem, Option<hir::TypeId>);

fn untyped(items: Vec<CompletionItem>) -> Vec<Candidate> {
    items.into_iter().map(|item| (item, None)).collect()
}

/// `'0'` when the candidate's type is assignable to an expected argument type
/// (sorts ahead of `'1'`), else `'1'`. Neutral when there is no expected type.
fn type_match_digit<DB: RootDatabase>(
    db: &DB,
    type_id: Option<hir::TypeId>,
    expected: &[hir::TypeId],
) -> char {
    match type_id {
        Some(tid) if expected.iter().any(|&want| hir::type_assignable(db, tid, want)) => '0',
        _ => '1',
    }
}

/// Compose the LSP `sort_text`, mirroring RDT1C's
/// `ПремияФильтра ↓, Рейтинг ↓, частота, слово` ordering: match quality first,
/// then the candidate's band (≈ usefulness), then how often the name occurs in
/// the current file, then the raw match score, then the name itself. All numeric
/// fields are fixed-width so the client's lexicographic ascending sort is exact.
fn sort_key(result: MatchResult, band: &str, typematch: char, freq: char, label: &str) -> String {
    format!(
        "{}{}{}{}{:05}{}",
        result.tier as u8,
        band,
        typematch,
        freq,
        u16::MAX - result.score,
        label.fold_lower(),
    )
}

#[allow(clippy::too_many_arguments)]
fn push_band<DB: RootDatabase>(
    out: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    matcher: &mut PrefixMatcher,
    db: &DB,
    freq: &FxHashMap<String, u32>,
    expected: &[hir::TypeId],
    band_prefix: &str,
    items: Vec<Candidate>,
) {
    for (mut item, type_id) in items {
        let key = item.label.fold_lower();
        if !seen.insert(key) {
            continue;
        }
        let result = best_match(matcher, &item);
        let typematch = type_match_digit(db, type_id, expected);
        item.sort_text = Some(sort_key(
            result,
            band_prefix,
            typematch,
            freq_bucket(freq, &item.label),
            &item.label,
        ));
        out.push(item);
    }
}

/// Score a candidate by its label and bilingual aliases; the pre-gated unqualified
/// candidates always match, so fall back to the worst tier only defensively.
fn best_match(matcher: &mut PrefixMatcher, item: &CompletionItem) -> MatchResult {
    super::fuzzy::score_item(matcher, &item.label, item.filter_text.as_deref())
        .unwrap_or(MatchResult { tier: MatchTier::Fuzzy, score: 0 })
}

/// Count identifier occurrences in the current file as a cheap "used here"
/// signal (RDT1C boosts words frequent in the current code).
fn build_frequency<DB: RootDatabase>(db: &DB, file_id: vfs::FileId) -> FxHashMap<String, u32> {
    let parse = db.parse(file_id);
    let mut freq: FxHashMap<String, u32> = FxHashMap::default();
    for element in parse.syntax_node().descendants_with_tokens() {
        if let Some(token) = element.as_token() {
            if token.kind() == SyntaxKind::IDENT {
                *freq.entry(token.text().fold_lower()).or_insert(0) += 1;
            }
        }
    }
    freq
}

fn freq_bucket(freq: &FxHashMap<String, u32>, label: &str) -> char {
    match freq.get(&label.fold_lower()).copied().unwrap_or(0) {
        0 => '2',
        1..=2 => '1',
        _ => '0',
    }
}

fn cursor_in_method<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    offset: syntax::TextSize,
) -> bool {
    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    match root.token_at_offset(offset).right_biased() {
        Some(token) => find_method_for_token(&token).is_some(),
        None => false,
    }
}

fn complete_module_self_attributes<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    matcher: &PrefixMatcher,
    locale: ide_db::base_db::Locale,
) -> Vec<CompletionItem> {
    let _span = tracing::debug_span!("complete_module_self_attributes").entered();
    hir::module_implicit_fields(db, file_id)
        .into_iter()
        .filter(|field| matcher.admits(field.name.as_str()))
        .map(|field| render_mdo_field(db, &field, locale))
        .collect()
}

fn complete_local_symbols<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    offset: syntax::TextSize,
    matcher: &PrefixMatcher,
) -> Vec<Candidate> {
    let _span = tracing::debug_span!("complete_local_symbols").entered();

    let mut completions: Vec<Candidate> = Vec::new();

    let parse = db.parse(file_id);
    let root = parse.syntax_node();
    let token = match root.token_at_offset(offset).right_biased() {
        Some(t) => t,
        None => return completions,
    };

    let (method_def, method_range) = match find_method_for_token(&token) {
        Some((def, range)) => (def, range),
        None => return completions,
    };

    let scopes = match &method_def {
        Either::Left(proc) => ExprScopes::from_procedure(proc),
        Either::Right(func) => ExprScopes::from_function(func),
    };

    let root_scope = scopes.root_scope();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (name, scope_def) in scopes.all_entries_in_scope(root_scope) {
        let name_str = name.as_str();
        seen.insert(name_str.fold_lower());

        if !matcher.admits(name_str) {
            continue;
        }

        let (kind, detail) = match scope_def {
            ScopeDef::Parameter => (CompletionItemKind::Field, "Параметр"),
            ScopeDef::LocalVariable => (CompletionItemKind::Field, "Локальная переменная"),
        };

        completions.push((
            CompletionItem {
                label: name_str.to_string(),
                detail: Some(detail.to_string()),
                kind,
                insert_text: name_str.to_string(),
                documentation: None,
                sort_text: None,
                filter_text: None,
                source: None,
            },
            None,
        ));
    }

    if let Some(owner) = owner_for_method_range(db, file_id, method_range) {
        let routed = hir::infer_owner(db, file_id, owner);
        let implicit_locals = routed.implicit_locals();
        if !implicit_locals.is_empty() {
            for (lower, info) in implicit_locals {
                if seen.contains(lower) {
                    continue;
                }
                if !matcher.admits(lower) {
                    seen.insert(lower.clone());
                    continue;
                }
                seen.insert(lower.clone());
                let text = info.name.as_str().to_string();
                completions.push((
                    CompletionItem {
                        label: text.clone(),
                        detail: Some("Переменная".to_string()),
                        kind: CompletionItemKind::Field,
                        insert_text: text,
                        documentation: None,
                        sort_text: None,
                        filter_text: None,
                        source: None,
                    },
                    Some(info.ty),
                ));
            }
        }
    }

    tracing::debug!(
        count = completions.len(),
        method_range = ?method_range,
        "Completed local symbols"
    );

    completions
}

fn owner_for_method_range<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    method_range: TextRange,
) -> Option<DefWithBodyId> {
    let tree = db.item_tree(file_id);
    let key = tree.methods().find(|method| method.source_range() == method_range)?.key();
    Some(DefWithBodyId::Method(key))
}

fn find_method_for_token(
    token: &syntax::SyntaxToken,
) -> Option<(Either<syntax::ast::ProcedureDef, syntax::ast::FunctionDef>, TextRange)> {
    method_of_ancestors(token).or_else(|| method_of_unterminated_method_before(token))
}

fn method_of_ancestors(
    token: &syntax::SyntaxToken,
) -> Option<(Either<syntax::ast::ProcedureDef, syntax::ast::FunctionDef>, TextRange)> {
    use syntax::ast;

    for ancestor in token.parent()?.ancestors() {
        if let Some(proc) = ast::ProcedureDef::cast(ancestor.clone()) {
            let method_range = proc.syntax().text_range();
            return Some((Either::Left(proc), method_range));
        }
        if let Some(func) = ast::FunctionDef::cast(ancestor.clone()) {
            let method_range = func.syntax().text_range();
            return Some((Either::Right(func), method_range));
        }
    }
    None
}

/// Метод, внутри которого курсор стоит по смыслу, хотя по дереву — снаружи.
///
/// Так выглядит метод, который ещё печатают: закрывающего слова нет, поэтому
/// узел метода кончается на последнем набранном токене, а пробел за ним
/// принадлежит уже корню — тривия достаётся общему предку соседних значимых
/// токенов, и другого предка у хвоста файла нет.
///
/// Закрытый метод сюда не попадает: за его `КонецПроцедуры` стоит уже
/// модульный уровень, и предлагать там его локальные имена неверно.
fn method_of_unterminated_method_before(
    token: &syntax::SyntaxToken,
) -> Option<(Either<syntax::ast::ProcedureDef, syntax::ast::FunctionDef>, TextRange)> {
    if !token.kind().is_trivia() {
        return None;
    }

    let previous = std::iter::successors(syntax::prev_token_past_empty(token), |t| {
        syntax::prev_token_past_empty(t)
    })
    .find(|t| !t.kind().is_trivia())?;
    let found = method_of_ancestors(&previous)?;
    let method = match &found.0 {
        Either::Left(proc) => proc.syntax(),
        Either::Right(func) => func.syntax(),
    };

    let terminated = method
        .children_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|t| matches!(t.kind(), SyntaxKind::KW_END_PROCEDURE | SyntaxKind::KW_END_FUNCTION));

    (!terminated).then_some(found)
}

fn is_expression_start_position(token: &syntax::SyntaxToken) -> bool {
    match token.kind() {
        SyntaxKind::R_PAREN | SyntaxKind::L_PAREN | SyntaxKind::COMMA => true,
        SyntaxKind::SEMICOLON => true,
        SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE => {
            let mut prev = syntax::prev_token_past_empty(token);
            while let Some(ref t) = prev {
                if !t.kind().is_trivia() {
                    break;
                }
                prev = syntax::prev_token_past_empty(t);
            }
            match prev {
                Some(t) => !matches!(t.kind(), SyntaxKind::DOT),
                None => true,
            }
        }
        _ => false,
    }
}

fn complete_mdo_plurals(matcher: &PrefixMatcher, env: &EnvFilter) -> Vec<CompletionItem> {
    let _span = tracing::debug_span!("complete_mdo_plurals").entered();

    let mut completions = Vec::new();

    for prop in PlatformDataInner::instance().all_global_properties() {
        let Some(mdo_type) = MdoType::from_plural(prop.name.as_str())
            .or_else(|| MdoType::from_plural(prop.english_name.as_str()))
        else {
            continue;
        };
        if !matcher.admits_contiguous_bilingual(&prop.name, &prop.english_name) {
            continue;
        }
        if !env.admits_context(prop.context.as_ref()) {
            continue;
        }
        completions.push(render_mdo_plural_with_hbk(mdo_type, prop));
    }

    completions
}

fn render_mdo_plural_with_hbk(
    mdo_type: bsl_metadata::MdoType,
    prop: &bsl_platform::PlatformProperty,
) -> CompletionItem {
    let label = prop.name.to_string();
    let mut detail = format!("Коллекция метаданных ({})", mdo_type.russian_name());
    if prop.is_readonly {
        detail.push_str(" [Только чтение]");
    }
    let documentation = compose_mdo_plural_documentation(mdo_type, prop);
    let filter_text = format!("{} {}", prop.name, prop.english_name);

    CompletionItem {
        label: label.clone(),
        detail: Some(detail),
        kind: CompletionItemKind::MdoType,
        insert_text: label,
        documentation: Some(documentation),
        sort_text: None,
        filter_text: Some(filter_text),
        source: None,
    }
}

fn compose_mdo_plural_documentation(
    mdo_type: bsl_metadata::MdoType,
    prop: &bsl_platform::PlatformProperty,
) -> String {
    let mut out = format!("{} / {}\n\n", prop.name, prop.english_name);
    let docs = PlatformDataInner::instance().get_property_docs(prop.id);
    match docs {
        Some(d) if !d.description.trim().is_empty() => {
            out.push_str(d.description.trim());
            if let Some(notes) = d.notes.as_ref().filter(|n| !n.trim().is_empty()) {
                out.push_str("\n\n");
                out.push_str(notes.trim());
            }
            if !d.see_also.is_empty() {
                out.push_str("\n\nСм. также:");
                for link in &d.see_also {
                    out.push_str("\n- ");
                    out.push_str(link);
                }
            }
        }
        _ => {
            out.push_str("Коллекция объектов метаданных типа ");
            out.push_str(mdo_type.russian_name());
            out.push('.');
        }
    }
    out
}

fn complete_user_common_modules<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    matcher: &PrefixMatcher,
    env: &EnvFilter,
) -> Vec<CompletionItem> {
    let _span = tracing::debug_span!("complete_user_common_modules").entered();

    let mut completions = Vec::new();

    for name in module_index_for(db, file_id).common_module_display_names() {
        if !matcher.admits(name) {
            continue;
        }
        if !env.admits_common_module(db, file_id, name) {
            continue;
        }
        completions.push(CompletionItem {
            label: name.to_string(),
            detail: Some("Общий модуль".to_string()),
            kind: CompletionItemKind::MdoObject,
            insert_text: name.to_string(),
            documentation: Some(format!("{name}\n\nОбщий модуль конфигурации.")),
            sort_text: None,
            filter_text: None,
            source: None,
        });
    }

    completions
}

fn module_index_for<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
) -> std::sync::Arc<hir::ModuleIndex> {
    let source_root_id = db.file_source_root_input(file_id).source_root_id(db);
    db.module_index(source_root_id)
}

/// Exported methods of the GLOBAL common modules, offered unqualified because a global
/// common module extends the global context. Placed in a band before the platform globals
/// so a global export shadows a same-named platform function (matching name resolution).
fn complete_global_module_exports<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    matcher: &PrefixMatcher,
    env: &EnvFilter,
) -> (Vec<CompletionItem>, Vec<String>) {
    let _span = tracing::debug_span!("complete_global_module_exports").entered();

    let module_id = hir::ModuleId::new(file_id);
    let resolver = hir::Resolver::with_workspace_scope(module_id);

    let mut items = Vec::new();
    let mut blocked = Vec::new();
    let common = resolver.global_common_module_exports(db);
    let application = resolver.application_module_exports(db);
    for entry in common.entries.into_iter().chain(application.entries) {
        if !matcher.admits(entry.name.as_str()) {
            continue;
        }
        let admitted = match entry.host {
            hir::GlobalExportHost::CommonModule => {
                env.admits_common_module(db, file_id, entry.module.as_str())
            }
            hir::GlobalExportHost::ApplicationModule(kind) => {
                let metadata = hir::ModuleMetadata::unknown(kind.module_type());
                env.admits(hir::execution_env::module_base_env(&metadata, &db.env_options()))
            }
        };
        if !admitted {
            blocked.push(entry.name.as_str().to_string());
            continue;
        }
        match entry.definition {
            hir::GlobalExportDefinition::Method(method_id)
                if entry.capabilities.callable == Some(true) =>
            {
                let symbol_tree = db.symbol_tree(method_id.module);
                let Some(method) = symbol_tree.find_method_by_id(method_id) else {
                    continue;
                };
                items.push(super::platform_completion::render_common_module_method(
                    db,
                    file_id,
                    &entry.module,
                    method,
                ));
            }
            hir::GlobalExportDefinition::Variable(_)
                if entry.capabilities.readable_as_value == Some(true) =>
            {
                items.push(CompletionItem {
                    label: entry.name.as_str().to_string(),
                    detail: Some("Глобальная переменная приложения".to_string()),
                    kind: CompletionItemKind::Field,
                    insert_text: entry.name.as_str().to_string(),
                    documentation: None,
                    sort_text: None,
                    filter_text: None,
                    source: None,
                });
            }
            _ => {}
        }
    }
    (items, blocked)
}

fn complete_hbk_globals<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    matcher: &PrefixMatcher,
    locale: Locale,
    env: &EnvFilter,
) -> Vec<CompletionItem> {
    let _span = tracing::debug_span!("complete_hbk_globals").entered();

    let data = PlatformDataInner::instance();
    let module_id = hir::ModuleId::new(file_id);
    let resolver = hir::Resolver::with_workspace_scope(module_id);

    let mut items = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for prop in data.all_global_properties() {
        let ru_lower = prop.name.fold_lower();

        if MdoType::from_plural(prop.name.as_str()).is_some()
            || MdoType::from_plural(prop.english_name.as_str()).is_some()
        {
            continue;
        }
        if resolver.user_common_module_exists(db, &hir::Name::new(&prop.name)) {
            continue;
        }
        if !matcher.admits_contiguous_bilingual(&prop.name, &prop.english_name) {
            continue;
        }
        if !env.admits_context(prop.context.as_ref()) {
            continue;
        }
        if !seen.insert(ru_lower) {
            continue;
        }

        items.push(render_platform_property(prop, locale));
    }

    items.extend(complete_global_functions(matcher, env));
    items
}

fn complete_user_defined_symbols<DB: RootDatabase>(
    db: &DB,
    file_id: vfs::FileId,
    matcher: &PrefixMatcher,
    want_fn_types: bool,
    env: &EnvFilter,
) -> Vec<Candidate> {
    let _span = tracing::debug_span!("complete_user_defined_symbols").entered();

    let mut completions: Vec<Candidate> = Vec::new();

    let sema = hir::Semantics::new(db);
    let module = sema.module_from_file(file_id);

    let opts = db.env_options();
    let metadata = db.module_metadata(hir::ModuleId::new(file_id));
    let symbol_tree = db.symbol_tree(hir::ModuleId::new(file_id));
    let local_admits = |env: &EnvFilter, name: &hir::Name| {
        let Some(method) = symbol_tree.find_method(name) else { return true };
        env.admits_local_method(hir::execution_env::body_env(&metadata, &method.directives, &opts))
    };

    for procedure in module.procedures() {
        let name = procedure.name();
        let name_str = name.as_str();

        if !matcher.admits(name_str) {
            continue;
        }
        if !local_admits(env, &name) {
            continue;
        }

        let detail = if procedure.is_export() {
            "Процедура Экспорт"
        } else {
            "Процедура"
        };

        // A procedure has no value type, so it never carries the type boost.
        completions.push((
            CompletionItem {
                label: name_str.to_string(),
                detail: Some(detail.to_string()),
                kind: CompletionItemKind::Function,
                insert_text: format!("{}()$0", name_str),
                documentation: None,
                sort_text: None,
                filter_text: None,
                source: None,
            },
            None,
        ));
    }

    for function in module.functions() {
        let name = function.name();
        let name_str = name.as_str();

        if !matcher.admits(name_str) {
            continue;
        }
        if !local_admits(env, &name) {
            continue;
        }

        let detail =
            if function.is_export() { "Функция Экспорт" } else { "Функция" };

        // The name completes a call, so the candidate's value type is the
        // function's inferred return type. Only computed when an expected type and
        // a typed prefix make it worthwhile (return-type inference is not free).
        let type_id = if want_fn_types {
            let ret = hir::method_return_type(db, function.id());
            (!matches!(db.lookup_type(ret), hir::TypeKind::Unknown)).then_some(ret)
        } else {
            None
        };

        completions.push((
            CompletionItem {
                label: name_str.to_string(),
                detail: Some(detail.to_string()),
                kind: CompletionItemKind::Function,
                insert_text: format!("{}()$0", name_str),
                documentation: None,
                sort_text: None,
                filter_text: None,
                source: None,
            },
            type_id,
        ));
    }

    for variable in module.variables() {
        let name = variable.name();
        let name_str = name.as_str();

        if !matcher.admits(name_str) {
            continue;
        }

        let detail = if variable.is_export() {
            "Переменная Экспорт"
        } else {
            "Переменная"
        };

        completions.push((
            CompletionItem {
                label: name_str.to_string(),
                detail: Some(detail.to_string()),
                kind: CompletionItemKind::Field,
                insert_text: name_str.to_string(),
                documentation: None,
                sort_text: None,
                filter_text: None,
                source: None,
            },
            None,
        ));
    }

    tracing::debug!(count = completions.len(), "Completed user-defined symbols");

    completions
}

fn complete_global_functions(matcher: &PrefixMatcher, env: &EnvFilter) -> Vec<CompletionItem> {
    let data = PlatformDataInner::instance();
    let all_functions = data.all_global_functions();
    let mut seen = HashSet::new();
    let mut items = Vec::new();

    for function in all_functions
        .iter()
        .filter(|f| matcher.admits_contiguous_bilingual(&f.name, &f.english_name))
        .filter(|f| env.admits_context(f.context.as_ref()))
    {
        seen.insert(function.name.fold_lower());
        seen.insert(function.english_name.fold_lower());
        items.push(render_global_function(function));
    }

    // The exact EDT catalog can contain callable globals absent from the HBK help
    // corpus. They have no rich signature to render, but they are still valid names.
    for symbol in PlatformGlobalCatalog::instance()
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind == PlatformGlobalKind::Function)
    {
        if !matcher.admits_contiguous_bilingual(&symbol.canonical_ru, &symbol.canonical_en)
            || !env.admits_context(symbol.context.as_ref())
            || seen.contains(&symbol.canonical_ru.as_str().fold_lower())
            || seen.contains(&symbol.canonical_en.as_str().fold_lower())
        {
            continue;
        }
        let label = if symbol.canonical_ru.is_empty() {
            symbol.canonical_en.as_str()
        } else {
            symbol.canonical_ru.as_str()
        };
        seen.insert(symbol.canonical_ru.as_str().fold_lower());
        seen.insert(symbol.canonical_en.as_str().fold_lower());
        items.push(CompletionItem {
            label: label.to_string(),
            detail: Some("Глобальная функция платформы".to_string()),
            kind: CompletionItemKind::Function,
            insert_text: format!("{label}()$0"),
            documentation: None,
            sort_text: None,
            filter_text: None,
            source: None,
        });
    }

    tracing::debug!(
        total_functions = all_functions.len(),
        matching_count = items.len(),
        "Filtered global functions"
    );
    items
}

fn render_global_function(function: &GlobalFunction) -> CompletionItem {
    let docs = PlatformDataInner::instance().get_global_function_docs(function.id);
    let sigs = symbol_info::from_global_function(function, docs.as_ref());
    let sig = sigs.first().expect("from_global_function returns at least one signature");
    super::platform_completion::item_from_signature(sig)
}

fn get_keyword_info(keyword: &str) -> (String, String) {
    // allow: keyword docs (M3 exception)
    if let Some(keyword_docs) = bsl_platform::PlatformData::instance().get_keyword_docs(keyword) {
        let mut doc = format!("{} / {}\n\n", keyword_docs.keyword_ru, keyword_docs.keyword_en);

        if !keyword_docs.syntax.is_empty() {
            doc.push_str("**Синтаксис:**\n```bsl\n");
            doc.push_str(&keyword_docs.syntax);
            doc.push_str("\n```\n\n");
        }

        if !keyword_docs.description.is_empty() {
            doc.push_str(&keyword_docs.description);
            doc.push_str("\n\n");
        }

        if !keyword_docs.params.is_empty() {
            doc.push_str("**Параметры:**\n");
            for param in &keyword_docs.params {
                doc.push_str(&format!("- **{}**: {}\n", param.name, param.description));
            }
            doc.push('\n');
        }

        if let Some(ref version) = keyword_docs.min_version {
            doc.push_str(&format!("**Доступен с версии:** {}", version));
        }

        return ("Ключевое слово BSL".to_string(), doc);
    }

    ("Ключевое слово BSL".to_string(), format!("**{}**\n\nКлючевое слово языка BSL.", keyword))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_match_scores_whole_multiword_alias_not_only_tokens() {
        // A prefix that spans a space inside the EN name: admitted by the
        // contiguous gate, so it must be scored (not Fuzzy) via the whole alias.
        let mut matcher = PrefixMatcher::new("Load f");
        let mut item = CompletionItem::simple(
            "ФормаЗагрузки".into(),
            CompletionItemKind::Property,
            "x".into(),
        );
        item.filter_text = Some("Форма загрузки Load form".into());
        let result = best_match(&mut matcher, &item);
        assert!(
            result.tier < MatchTier::Fuzzy,
            "multi-word EN alias must score above Fuzzy; got {:?}",
            result.tier
        );
    }

    #[test]
    fn best_match_prefers_token_prefix_over_whole_string() {
        // Single-word EN token starting with the input must win the Prefix tier.
        let mut matcher = PrefixMatcher::new("Docu");
        let mut item =
            CompletionItem::simple("Документы".into(), CompletionItemKind::MdoType, "x".into());
        item.filter_text = Some("Документы Documents".into());
        let result = best_match(&mut matcher, &item);
        assert_eq!(result.tier, MatchTier::Prefix);
    }

    #[test]
    fn test_complete_global_functions_with_prefix() {
        use bsl_platform::PlatformDataInner;

        let data = PlatformDataInner::instance();
        if data.all_global_functions().is_empty() {
            println!("Skipping test: no global functions available");
            return;
        }

        let items =
            complete_global_functions(&PrefixMatcher::new("Начать"), &EnvFilter::permissive());

        println!("Found {} completions for 'Начать'", items.len());
        assert!(!items.is_empty(), "Should find functions starting with 'Начать'");

        let has_begin_transaction = items.iter().any(|i| i.label == "НачатьТранзакцию");
        assert!(has_begin_transaction, "Should contain НачатьТранзакцию");

        for item in &items {
            assert_eq!(item.kind, CompletionItemKind::Function);
        }
    }

    #[test]
    fn edt_only_global_function_is_completed_without_hbk_signature() {
        let data = PlatformDataInner::instance();
        let symbol = PlatformGlobalCatalog::instance()
            .symbols()
            .iter()
            .find(|symbol| {
                symbol.kind == PlatformGlobalKind::Function
                    && data.get_global_function(symbol.canonical_ru.as_str()).is_none()
                    && data.get_global_function(symbol.canonical_en.as_str()).is_none()
            })
            .expect("the exact EDT catalog must contain a callable absent from HBK");
        let prefix = symbol.canonical_ru.as_str();
        let items =
            complete_global_functions(&PrefixMatcher::new(prefix), &EnvFilter::permissive());
        assert!(
            items.iter().any(|item| {
                item.label == symbol.canonical_ru.as_str()
                    || item.label == symbol.canonical_en.as_str()
            }),
            "EDT-only callable must remain discoverable: {prefix}"
        );
    }

    #[test]
    fn application_module_exports_are_completed_as_globals() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, FileId, VfsPath};

        let fixture = test_fixture::CfeFixtureBuilder::new("").build();
        let caller_path = fixture.root().join("CommonModules/Caller/Ext/Module.bsl");
        std::fs::create_dir_all(caller_path.parent().unwrap()).unwrap();
        std::fs::write(&caller_path, "Процедура Тест()\nКонецПроцедуры").unwrap();
        let app_path = fixture.root().join("Ext/ManagedApplicationModule.bsl");
        std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
        let app_source = "Перем ПриложениеПеременная Экспорт;\nПроцедура ПриложениеМетод() Экспорт КонецПроцедуры";
        std::fs::write(&app_path, app_source).unwrap();

        let caller = FileId(0);
        let app = FileId(1);
        let mut db = RootDatabaseImpl::new();
        let mut file_set = FileSet::new();
        file_set.insert(caller, VfsPath::new(caller_path.to_string_lossy().as_ref()));
        file_set.insert(app, VfsPath::new(app_path.to_string_lossy().as_ref()));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(caller, SourceRootId(0));
        db.set_file_source_root(app, SourceRootId(0));
        db.set_file_text(caller, "Процедура Тест()\nКонецПроцедуры");
        db.set_file_text(app, app_source);
        db.set_all_config_paths(fixture.config_paths());

        let (items, _) = complete_global_module_exports(
            &db,
            caller,
            &PrefixMatcher::new("Приложение"),
            &EnvFilter::permissive(),
        );
        assert!(items.iter().any(|item| item.label == "ПриложениеМетод"));
        assert!(items.iter().any(|item| item.label == "ПриложениеПеременная"));
    }

    #[test]
    fn test_complete_global_functions_case_insensitive() {
        use bsl_platform::PlatformDataInner;

        let data = PlatformDataInner::instance();
        if data.all_global_functions().is_empty() {
            println!("Skipping test: no global functions available");
            return;
        }

        let items_lower =
            complete_global_functions(&PrefixMatcher::new("начать"), &EnvFilter::permissive());
        let items_upper =
            complete_global_functions(&PrefixMatcher::new("НАЧАТЬ"), &EnvFilter::permissive());
        let items_mixed =
            complete_global_functions(&PrefixMatcher::new("Начать"), &EnvFilter::permissive());

        assert_eq!(items_lower.len(), items_upper.len());
        assert_eq!(items_lower.len(), items_mixed.len());
    }

    #[test]
    fn test_render_global_function() {
        use bsl_platform::PlatformDataInner;

        let data = PlatformDataInner::instance();
        if data.all_global_functions().is_empty() {
            println!("Skipping test: no global functions available");
            return;
        }

        let function = data.get_global_function("НачатьТранзакцию").unwrap();
        let item = render_global_function(function);

        assert_eq!(item.label, "НачатьТранзакцию");
        assert_eq!(item.kind, CompletionItemKind::Function);
        assert!(item.detail.is_some());
        assert!(item.documentation.is_some());

        assert!(item.insert_text.ends_with("$0)"));
    }

    #[test]
    fn test_complete_user_defined_symbols() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = r#"
Перем МояПеременная Экспорт;
Перем ПриватнаяПеременная;

Процедура МояПроцедура() Экспорт
    // тело
КонецПроцедуры

Функция МояФункция()
    Возврат 42;
КонецФункции

Процедура ДругаяПроцедура()
    Моя
КонецПроцедуры
"#;

        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        let items = complete_user_defined_symbols(
            &db,
            file_id,
            &PrefixMatcher::new("Моя"),
            false,
            &EnvFilter::permissive(),
        )
        .into_iter()
        .map(|(i, _)| i)
        .collect::<Vec<_>>();

        println!("Found {} items for prefix 'Моя'", items.len());
        for item in &items {
            println!("  - {} ({:?})", item.label, item.kind);
        }

        assert_eq!(items.len(), 3, "Should find 3 items starting with 'Моя'");

        let has_procedure = items.iter().any(|i| i.label == "МояПроцедура");
        assert!(has_procedure, "Should contain МояПроцедура");

        let has_function = items.iter().any(|i| i.label == "МояФункция");
        assert!(has_function, "Should contain МояФункция");

        let has_variable = items.iter().any(|i| i.label == "МояПеременная");
        assert!(has_variable, "Should contain МояПеременная");

        let procedure_item = items.iter().find(|i| i.label == "МояПроцедура").unwrap();
        assert_eq!(
            procedure_item.detail,
            Some("Процедура Экспорт".to_string()),
            "МояПроцедура should be marked as Export"
        );

        let function_item = items.iter().find(|i| i.label == "МояФункция").unwrap();
        assert_eq!(
            function_item.detail,
            Some("Функция".to_string()),
            "МояФункция should NOT be marked as Export"
        );
    }

    #[test]
    fn test_complete_user_defined_symbols_case_insensitive() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = r#"
Процедура ТестоваяПроцедура()
КонецПроцедуры
"#;

        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        let items_lower = complete_user_defined_symbols(
            &db,
            file_id,
            &PrefixMatcher::new("тест"),
            false,
            &EnvFilter::permissive(),
        )
        .into_iter()
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
        let items_upper = complete_user_defined_symbols(
            &db,
            file_id,
            &PrefixMatcher::new("ТЕСТ"),
            false,
            &EnvFilter::permissive(),
        )
        .into_iter()
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
        let items_mixed = complete_user_defined_symbols(
            &db,
            file_id,
            &PrefixMatcher::new("Тест"),
            false,
            &EnvFilter::permissive(),
        )
        .into_iter()
        .map(|(i, _)| i)
        .collect::<Vec<_>>();

        assert_eq!(items_lower.len(), 1);
        assert_eq!(items_upper.len(), 1);
        assert_eq!(items_mixed.len(), 1);

        assert_eq!(items_lower[0].label, "ТестоваяПроцедура");
        assert_eq!(items_upper[0].label, "ТестоваяПроцедура");
        assert_eq!(items_mixed[0].label, "ТестоваяПроцедура");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_complete_mdo_plural_forms() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = r#"
Процедура Тест()
    Справ
КонецПроцедуры
"#;

        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        let items = complete_mdo_plurals(&PrefixMatcher::new("Справ"), &EnvFilter::permissive());
        let _ = (&db, file_id);

        println!("Found {} MDO items for prefix 'Справ'", items.len());
        for item in &items {
            println!("  - {} ({:?})", item.label, item.kind);
        }

        let has_catalogs = items.iter().any(|i| i.label == "Справочники");
        assert!(has_catalogs, "Should contain Справочники plural form");

        let catalogs_item = items.iter().find(|i| i.label == "Справочники").unwrap();
        assert_eq!(catalogs_item.kind, CompletionItemKind::MdoType);
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_complete_mdo_symbols_bilingual() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = r#"
Процедура Тест()
    Docu
КонецПроцедуры
"#;

        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        let items = complete_mdo_plurals(&PrefixMatcher::new("Docu"), &EnvFilter::permissive());
        let _ = (&db, file_id);

        println!("Found {} MDO items for prefix 'Docu'", items.len());

        let has_documents = items.iter().any(|i| i.label == "Документы");
        assert!(has_documents, "Should contain Документы (matched by English 'Documents')");
    }

    #[test]
    fn test_complete_local_symbols() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = r#"
Процедура Тест(Первый, Второй)
    Перем МояПеременная;
    Перем ДругаяПеременная;

    // Курсор здесь - вводим "Мо"
    Мо
КонецПроцедуры
"#;

        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        let offset = source.find("Мо").expect("Should find 'Мо' in source");
        let offset = syntax::TextSize::from(offset as u32);

        let items: Vec<_> = complete_local_symbols(&db, file_id, offset, &PrefixMatcher::new("Мо"))
            .into_iter()
            .map(|(i, _)| i)
            .collect();

        println!("Found {} local items for prefix 'Мо'", items.len());
        for item in &items {
            println!("  - {} ({:?}, {:?})", item.label, item.kind, item.detail);
        }

        assert_eq!(items.len(), 1, "Should find 1 local variable starting with 'Мо'");
        assert_eq!(items[0].label, "МояПеременная");
        assert_eq!(items[0].detail, Some("Локальная переменная".to_string()));
    }

    #[test]
    fn test_complete_local_symbols_parameters() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = r#"
Функция Тест(ПервыйПараметр, ВторойПараметр)
    Перем ЛокальнаяПеременная;

    // Курсор здесь - вводим "Перв"
    Перв
КонецФункции
"#;

        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        let offset = source.find("Перв").expect("Should find 'Перв' in source");
        let offset = syntax::TextSize::from(offset as u32);

        let items: Vec<_> =
            complete_local_symbols(&db, file_id, offset, &PrefixMatcher::new("Перв"))
                .into_iter()
                .map(|(i, _)| i)
                .collect();

        println!("Found {} local items for prefix 'Перв'", items.len());
        for item in &items {
            println!("  - {} ({:?}, {:?})", item.label, item.kind, item.detail);
        }

        assert_eq!(items.len(), 1, "Should find 1 parameter starting with 'Перв'");
        assert_eq!(items[0].label, "ПервыйПараметр");
        assert_eq!(items[0].detail, Some("Параметр".to_string()));
    }

    #[test]
    fn test_complete_local_symbols_all() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = r#"
Процедура Тест(Параметр1, Параметр2)
    Перем Переменная1;
    Перем Переменная2;

    // Empty prefix - should return all

КонецПроцедуры
"#;

        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        db.set_file_text(file_id, source);

        let offset = source.find("// Empty prefix").expect("Should find comment") + 20;
        let offset = syntax::TextSize::from(offset as u32);

        let items: Vec<_> = complete_local_symbols(&db, file_id, offset, &PrefixMatcher::new(""))
            .into_iter()
            .map(|(i, _)| i)
            .collect();

        println!("Found {} total local items", items.len());
        for item in &items {
            println!("  - {} ({:?}, {:?})", item.label, item.kind, item.detail);
        }

        assert_eq!(items.len(), 4, "Should find all local symbols");

        let param_count = items.iter().filter(|i| i.detail == Some("Параметр".to_string())).count();
        assert_eq!(param_count, 2, "Should have 2 parameters");

        let var_count =
            items.iter().filter(|i| i.detail == Some("Локальная переменная".to_string())).count();
        assert_eq!(var_count, 2, "Should have 2 local variables");
    }

    #[test]
    fn test_implicit_variables_from_assignments() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = r#"
Процедура Тест(Запрос)
    Партнер = Справочники.Партнеры.НайтиПоКоду("001");
    Результат = Новый Структура;
    Результат.Вставить("Партнер", Партнер);
    ВременнаяПеременная = 42;
КонецПроцедуры
"#;
        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);
        db.set_file_text(file_id, source);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        let offset = syntax::TextSize::from(source.find("ВременнаяПеременная").unwrap() as u32);

        let items: Vec<_> = complete_local_symbols(&db, file_id, offset, &PrefixMatcher::new(""))
            .into_iter()
            .map(|(i, _)| i)
            .collect();

        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        println!("Found local symbols: {:?}", labels);

        assert!(labels.contains(&"Запрос"), "Should find parameter Запрос");

        assert!(labels.contains(&"Партнер"), "Should find implicit var Партнер");
        assert!(labels.contains(&"Результат"), "Should find implicit var Результат");
        assert!(
            labels.contains(&"ВременнаяПеременная"),
            "Should find implicit var ВременнаяПеременная"
        );

        let implicit_count =
            items.iter().filter(|i| i.detail == Some("Переменная".to_string())).count();
        assert_eq!(implicit_count, 3, "Should have 3 implicit variables");
    }

    #[test]
    fn test_complete_module_self_attributes_skips_non_self_module() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{file_set::FileSet, VfsPath};

        let source = "Процедура Test() КонецПроцедуры\n";
        let mut db = RootDatabaseImpl::default();
        let file_id = vfs::FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, source);

        let items = complete_module_self_attributes(
            &db,
            file_id,
            &PrefixMatcher::new(""),
            ide_db::base_db::Locale::Ru,
        );
        assert!(items.is_empty(), "non-self file must not surface implicit attributes");
    }
}
