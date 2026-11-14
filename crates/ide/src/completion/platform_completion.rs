use bsl_platform::{
    manager_methods_query, type_methods_query, type_properties_query, PlatformDataInner,
    PlatformMethod, PlatformProperty, TypeNameInput,
};
use hir::{
    coerce_to_metadata_ref_id, platform_type_key_id, Field, HirFieldOrigin, MethodSymbol, Name,
    Semantics, TyLoweringContext, Type as HirType, TypeId, TypeKind,
};
use ide_db::RootDatabase;
use stdx::case::CaseExt;
use symbol_info::{
    build_signature, from_platform_method, render_completion_detail, CalleeKind, CompletionDetail,
    MethodKind, SignatureSource, SymbolSignature,
};
use syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use vfs::FileId;

use super::env_filter::EnvFilter;
use super::fuzzy::{MatchTier, PrefixMatcher};
use super::{CompletionItem, CompletionItemKind, CompletionPosition};

/// Collect the right-hand expression texts of `//: <var> = <expr>` type-hint
/// comments for `var_name`, best candidate first: hints above the receiver
/// nearest-first (a hint describes the variable from where it stands), then
/// hints below it in text order. Scans the enclosing method; module-level code
/// sees only comments outside methods, since a method's hint describes its own
/// local. The discriminator is the colon right after `//` (`//:`), so ordinary
/// comments never match.
fn find_type_hints(receiver: &SyntaxNode, var_name: &str) -> Vec<String> {
    let want = var_name.fold_lower();
    let is_method =
        |n: &SyntaxNode| matches!(n.kind(), SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF);
    let enclosing = receiver.ancestors().find(is_method);
    let scope = enclosing
        .clone()
        .or_else(|| receiver.ancestors().last())
        .unwrap_or_else(|| receiver.clone());
    let at = receiver.text_range().start();

    let (mut above, mut below) = (Vec::new(), Vec::new());
    for token in scope.descendants_with_tokens().filter_map(|e| e.into_token()) {
        if token.kind() != SyntaxKind::COMMENT {
            continue;
        }
        if enclosing.is_none() && token.parent_ancestors().any(|n| is_method(&n)) {
            continue;
        }
        let Some(rest) = token.text().strip_prefix("//").and_then(|s| s.strip_prefix(':')) else {
            continue;
        };
        let Some((lhs, rhs)) = rest.split_once('=') else {
            continue;
        };
        if lhs.trim().fold_lower() != want {
            continue;
        }
        let expr = rhs.trim().trim_end_matches(';').trim();
        if expr.is_empty() {
            continue;
        }
        if token.text_range().start() < at {
            above.push(expr.to_string());
        } else {
            below.push(expr.to_string());
        }
    }
    above.reverse();
    above.extend(below);
    above
}

pub(super) fn platform_completions<DB: RootDatabase>(
    db: &DB,
    position: CompletionPosition,
) -> Option<Vec<CompletionItem>> {
    let _span = tracing::info_span!("platform_completions").entered();

    let parse = db.parse(position.file_id);
    let root = parse.syntax_node();

    let token = root.token_at_offset(position.offset).left_biased()?;

    tracing::debug!(token_kind = ?token.kind(), token_text = ?token.text(), "Completion token");

    let (dot_token, prefix) = resolve_dot_anchor(&token, position.offset)?;
    let env_filter = EnvFilter::at(db, position.file_id, position.offset);

    let receiver_expr = find_receiver_expr(&dot_token)?;

    if let Some(receiver_name) = extract_receiver_ident(&receiver_expr) {
        tracing::debug!(receiver_name = %receiver_name, "Trying CommonModule fast path");
        // A statically named module the caller's environments cannot reach
        // offers no members — the diagnostic would underline any such call
        // (typed variables holding a module stay permissive, mirroring the
        // diagnostic skipping flow-insensitive receivers).
        if hir::Resolver::with_workspace_scope(hir::ModuleId::new(position.file_id))
            .user_common_module_exists(db, &Name::new(&receiver_name))
            && !env_filter.admits_common_module(db, position.file_id, &receiver_name)
        {
            return Some(Vec::new());
        }
        if let Some(items) = complete_common_module_methods(db, &position, &receiver_name) {
            return Some(apply_prefix_filter(items, &prefix, db));
        }
    }

    let sema = Semantics::new(db);
    let mut receiver_id = sema.type_of_expr(position.file_id, &receiver_expr);

    // An explicit `//: Имя = Выражение` type hint overrides a missing (`Unknown`)
    // or too-generic (`Произвольный` -> `Any`) inferred type - the cases where
    // member completion is otherwise empty. `Произвольный` is what the engine gives
    // a standard event-handler parameter (e.g. `ДанныеЗаполнения` of
    // `ОбработкаЗаполнения`), so gating only on `Unknown` would miss exactly those.
    if matches!(db.lookup_type(receiver_id), TypeKind::Unknown | TypeKind::Any) {
        if let Some(name) = extract_receiver_ident(&receiver_expr) {
            if let Some(id) = find_type_hints(&receiver_expr, &name)
                .iter()
                .find_map(|expr_text| sema.type_of_expr_text(position.file_id, expr_text))
            {
                receiver_id = id;
            }
        }
    }

    // `Unknown` here means inference has no type for the receiver, which happens
    // both when a user symbol holds the name and when the name is a bare TYPE
    // (`XBase.`, `Запрос.`) that the cascade never types. Only the claim below
    // tells those apart - the lowering status cannot, since both are lowered.
    if matches!(db.lookup_type(receiver_id), TypeKind::Unknown) {
        if let Some(name) = extract_receiver_ident(&receiver_expr) {
            let name_node = Name::new(&name);
            let (claim, owner) = hir::bare_root::claim_at_node(
                db,
                position.file_id,
                position.offset,
                &name_node,
                &receiver_expr,
            );
            match claim {
                // A user symbol holds the name, so it does not denote the global.
                // Its reaching assignment carries the receiver's real type; with no
                // reaching write (a declared-but-unwritten local, a module item)
                // there is nothing to complete against, and naming the global here
                // is exactly the suggestion the availability diagnostic rejects.
                Some(claim) => {
                    let value_id = claim.reaching_value?;
                    receiver_id =
                        hir::bare_root::reaching_value_ty(db, position.file_id, owner, value_id)?;
                }
                None => {
                    if let Some(id) = hir::resolve_platform_global_property_type(db, &name_node) {
                        receiver_id = id;
                    }
                    if matches!(db.lookup_type(receiver_id), TypeKind::Unknown) {
                        receiver_id = TyLoweringContext::new().lower_bare_name_id(db, &name_node);
                    }
                }
            }
        }
    }

    tracing::debug!(receiver_id = ?receiver_id, "Resolved receiver type");

    // A variable typed as a common module (e.g. `М = ОбщегоНазначения.ОбщийМодуль("Имя")`)
    // completes against that module's exported methods. The name-keyed fast path above only
    // fires for a bare module identifier, not for such a variable.
    if let TypeKind::CommonModule(facet) = db.lookup_type(receiver_id) {
        if let Some(items) = complete_common_module_methods(db, &position, &facet.name) {
            return Some(apply_prefix_filter(items, &prefix, db));
        }
    }

    if let Some(items) = complete_prefix_methods_for_receiver(
        db,
        receiver_id,
        position.file_id,
        position.locale,
        &env_filter,
    ) {
        return Some(apply_prefix_filter(items, &prefix, db));
    }

    if hir::is_form_items_collection_ty(db, receiver_id) {
        if let Some(items) =
            complete_form_elements_collection(db, position.file_id, position.locale, &env_filter)
        {
            return Some(apply_prefix_filter(items, &prefix, db));
        }
    }

    if let TypeKind::FormControl { kind, .. } = db.lookup_type(receiver_id) {
        let chain = hir::form_control_platform_type_chain(*kind);
        if !chain.is_empty() {
            let mut items: Vec<CompletionItem> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for type_name in chain.iter().rev() {
                for item in complete_platform_methods(db, type_name, position.locale, &env_filter) {
                    if seen.insert(item.label.fold_lower()) {
                        items.push(item);
                    }
                }
            }
            return Some(apply_prefix_filter(items, &prefix, db));
        }
    }

    if let Some(type_name) = platform_type_key_id(db, receiver_id) {
        tracing::debug!(type_name = ?type_name, "Platform type for completion");
        let mut items = projection_column_items(db, receiver_id, position.locale);
        items.extend(complete_platform_methods(db, &type_name, position.locale, &env_filter));
        return Some(apply_prefix_filter(items, &prefix, db));
    }

    if let TypeKind::Union(members) = db.lookup_type(receiver_id) {
        let members = members.to_vec();
        let mut items: Vec<CompletionItem> = Vec::new();
        let mut seen_labels: std::collections::HashSet<String> = std::collections::HashSet::new();
        // The diagnostics judge a union member against the UNION of the arm
        // masks; filtering each arm alone would hide a label that is legal
        // through another arm, so unions are not judged here.
        let union_filter = EnvFilter::permissive();
        for m in members
            .into_iter()
            .filter(|m| !matches!(db.lookup_type(*m), TypeKind::Undefined | TypeKind::Null))
        {
            for item in projection_column_items(db, m, position.locale) {
                if seen_labels.insert(item.label.clone()) {
                    items.push(item);
                }
            }
            let Some(type_name) = platform_type_key_id(db, m) else { continue };
            for item in complete_platform_methods(db, &type_name, position.locale, &union_filter) {
                if seen_labels.insert(item.label.clone()) {
                    items.push(item);
                }
            }
        }
        if !items.is_empty() {
            return Some(apply_prefix_filter(items, &prefix, db));
        }
    }

    None
}

fn complete_prefix_methods_for_receiver<DB: RootDatabase>(
    db: &DB,
    receiver: TypeId,
    file_id: FileId,
    locale: ide_db::base_db::Locale,
    env: &EnvFilter,
) -> Option<Vec<CompletionItem>> {
    let effective = coerce_to_metadata_ref_id(db, receiver).unwrap_or(receiver);

    let (
        is_manager,
        is_metadata_ref,
        is_metadata_ref_collection,
        is_union_with_metadata_ref,
        is_form_data_with_underlying,
    ) = match db.lookup_type(effective) {
        TypeKind::ObjectManager(_) | TypeKind::ManagerCollection(_) => {
            (true, false, false, false, false)
        }
        TypeKind::MetadataRef(_) | TypeKind::MetadataObject(_) => {
            (false, true, false, false, false)
        }
        TypeKind::MetadataReferenceCollection(_) => (false, false, true, false, false),
        TypeKind::Union(arms) => {
            let has_ref = arms.iter().any(|a| {
                matches!(
                    db.lookup_type(*a),
                    TypeKind::MetadataRef(_)
                        | TypeKind::MetadataObject(_)
                        | TypeKind::MetadataReferenceCollection(_)
                )
            });
            (false, false, false, has_ref, false)
        }
        TypeKind::FormData { underlying: Some(_), .. } => (false, false, false, false, true),
        _ => (false, false, false, false, false),
    };

    if is_manager {
        return collect_platform_items_or_none(db, effective, locale, env);
    }

    if !is_metadata_ref
        && !is_metadata_ref_collection
        && !is_union_with_metadata_ref
        && !is_form_data_with_underlying
    {
        return None;
    }

    let mdo_fields = HirType::from_id(db, file_id, effective).fields();
    let platform_items = collect_platform_items_for_effective(db, effective, locale, env);

    if mdo_fields.is_empty() && platform_items.is_empty() {
        return None;
    }

    let mut items: Vec<CompletionItem> =
        mdo_fields.iter().map(|f| render_mdo_field(db, f, locale)).collect();
    let mut seen: std::collections::HashSet<String> =
        items.iter().map(|i| i.label.fold_lower()).collect();
    for p in platform_items {
        if seen.insert(p.label.fold_lower()) {
            items.push(p);
        }
    }
    tracing::debug!(
        mdo_field_count = mdo_fields.len(),
        platform_item_count = items.len(),
        "MDO+platform completion for MetadataRef receiver"
    );
    Some(items)
}

fn collect_platform_items_for_effective<DB: RootDatabase>(
    db: &DB,
    effective: TypeId,
    locale: ide_db::base_db::Locale,
    env: &EnvFilter,
) -> Vec<CompletionItem> {
    let TypeKind::Union(arms) = db.lookup_type(effective) else {
        return collect_platform_items(db, effective, locale, env);
    };
    let arms = arms.to_vec();
    let mut out: Vec<CompletionItem> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // See the union handling in `platform_completions`: arm masks merge, so
    // per-arm judgement over-hides.
    let env = &EnvFilter::permissive();
    for arm in arms
        .into_iter()
        .filter(|t| !matches!(db.lookup_type(*t), TypeKind::Undefined | TypeKind::Null))
    {
        for item in collect_platform_items(db, arm, locale, env) {
            if seen.insert(item.label.fold_lower()) {
                out.push(item);
            }
        }
    }
    out
}

fn collect_platform_items<DB: RootDatabase>(
    db: &DB,
    receiver: TypeId,
    locale: ide_db::base_db::Locale,
    env: &EnvFilter,
) -> Vec<CompletionItem> {
    enum Shape {
        MetaKind(hir::MetadataKind),
        FormData,
        Manager(bsl_metadata::MdoType),
        Other,
    }
    let shape = match db.lookup_type(receiver) {
        TypeKind::MetadataRef(f) => Shape::MetaKind(f.kind),
        TypeKind::MetadataObject(f) => Shape::MetaKind(f.kind),
        TypeKind::FormData { .. } => Shape::FormData,
        TypeKind::ObjectManager(f) => Shape::Manager(f.mdo),
        TypeKind::AnyMetadataRef { mdo_type } => match hir::MetadataKind::ref_kind_for(*mdo_type) {
            Some(kind) => Shape::MetaKind(kind),
            None => Shape::Other,
        },
        _ => Shape::Other,
    };

    let prefix = match shape {
        Shape::MetaKind(kind) => {
            if let Some(scalar_key) = tabular_section_scalar_key(kind) {
                tracing::debug!(scalar_key, "Tabular section scalar completion");
                return complete_platform_methods(db, scalar_key, locale, env);
            }
            if let Some(scalar_key) = kind.scalar_platform_key() {
                tracing::debug!(scalar_key, "Synthetic-kind scalar completion");
                return complete_platform_methods(db, scalar_key, locale, env);
            }
            // An external object's members live under its bare platform type;
            // the prefix index of dotted names holds nothing for it.
            if let Some(bare_type) = kind.bare_platform_type() {
                tracing::debug!(bare_type, "Bare-type completion for an external object");
                return complete_platform_methods(db, bare_type, locale, env);
            }
            kind.platform_prefix()
        }
        Shape::FormData => {
            let Some(type_key) = platform_type_key_id(db, receiver) else { return Vec::new() };
            return complete_platform_methods(db, &type_key, locale, env);
        }
        Shape::Manager(mdo) => mdo.manager_type_prefix(),
        Shape::Other => None,
    };
    let Some(prefix) = prefix else { return Vec::new() };
    tracing::debug!(prefix, "Prefix-based completion for manager / metadata-ref receiver");
    let input = TypeNameInput::new(db, prefix.to_string());
    let methods = manager_methods_query(db, input);
    let judge_env = !PlatformDataInner::instance().is_ambiguous_type_name(prefix);
    methods
        .iter()
        .filter(|m| !judge_env || env.admits_context(m.context.as_ref()))
        .map(render_manager_method)
        .collect()
}

fn collect_platform_items_or_none<DB: RootDatabase>(
    db: &DB,
    receiver: TypeId,
    locale: ide_db::base_db::Locale,
    env: &EnvFilter,
) -> Option<Vec<CompletionItem>> {
    let items = collect_platform_items(db, receiver, locale, env);
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn tabular_section_scalar_key(kind: hir::MetadataKind) -> Option<&'static str> {
    match kind {
        hir::MetadataKind::TabularSection { .. } => Some("Tabular section"),
        hir::MetadataKind::TabularSectionRow { .. } => Some("Line of a tabular section"),
        _ => None,
    }
}

fn resolve_dot_anchor(
    token: &SyntaxToken,
    offset: syntax::TextSize,
) -> Option<(SyntaxToken, String)> {
    if token.kind() == SyntaxKind::DOT {
        return Some((token.clone(), String::new()));
    }
    if !token.kind().is_name_token() {
        return None;
    }
    let mut cur = syntax::prev_token_past_empty(token);
    while let Some(t) = cur.clone() {
        if t.kind().is_trivia() {
            cur = syntax::prev_token_past_empty(&t);
        } else {
            break;
        }
    }
    let dot = cur.filter(|t| t.kind() == SyntaxKind::DOT)?;
    let token_start = token.text_range().start();
    let cursor_in_token: usize = offset.checked_sub(token_start)?.into();
    let text = token.text();
    let prefix = text[..cursor_in_token.min(text.len())].to_string();
    Some((dot, prefix))
}

/// Filter and rank after-dot member completions for the typed prefix. Matching is
/// fuzzy (prefix / sub-word boundary / substring), and the match quality is
/// prepended to each item's existing per-source `sort_text` band so quality
/// dominates while the original ordering (form-element band, field origin, …) is
/// preserved as the secondary key. Scattered (non-contiguous) matches are dropped
/// to keep large receivers (unions, wide forms/projections) from flooding.
fn apply_prefix_filter(
    items: Vec<CompletionItem>,
    prefix: &str,
    _db: &dyn RootDatabase,
) -> Vec<CompletionItem> {
    if prefix.is_empty() {
        return items;
    }
    let mut matcher = PrefixMatcher::new(prefix);
    items
        .into_iter()
        .filter_map(|mut item| {
            let result =
                super::fuzzy::score_item(&mut matcher, &item.label, item.filter_text.as_deref())?;
            if result.tier == MatchTier::Fuzzy {
                return None;
            }
            let existing = item.sort_text.take().unwrap_or_else(|| item.label.fold_lower());
            item.sort_text = Some(format!("{}{}", result.tier as u8, existing));
            Some(item)
        })
        .collect()
}

fn find_receiver_expr(dot_token: &SyntaxToken) -> Option<SyntaxNode> {
    let Some(parent) = dot_token.parent() else {
        tracing::debug!("find_receiver_expr: dot has no parent");
        return None;
    };
    tracing::debug!(parent_kind = ?parent.kind(), "find_receiver_expr: DOT parent kind");

    if parent.kind() == SyntaxKind::FIELD_EXPR {
        let child = parent.children().next();
        tracing::debug!(child_found = child.is_some(), child_kind = ?child.as_ref().map(|c| c.kind()), "find_receiver_expr: FIELD_EXPR first child");
        return child;
    }

    for sibling in dot_token.siblings_with_tokens(syntax::Direction::Prev).skip(1) {
        if sibling.kind() == SyntaxKind::WHITESPACE {
            continue;
        }
        if let Some(node) = sibling.as_node() {
            tracing::debug!(sibling_kind = ?node.kind(), "find_receiver_expr: fallback found node sibling");
            return Some(node.clone());
        }
        if let Some(token) = sibling.as_token() {
            tracing::debug!(token_kind = ?token.kind(), "find_receiver_expr: fallback returning token.parent()");
            return token.parent();
        }
        return None;
    }

    tracing::debug!("find_receiver_expr: fallback exhausted, returning None");
    None
}

/// A common-module name qualifies only when the whole receiver is a *bare*
/// identifier. A call/field/index chain such as `Module.Method().` or `Foo.Bar`
/// shares the same leading ident token, but its value is the chain's result, not
/// the module — those must resolve by type, so this returns `None` for them.
fn extract_receiver_ident(node: &SyntaxNode) -> Option<String> {
    let mut ident: Option<String> = None;
    for element in node.descendants_with_tokens() {
        let Some(token) = element.as_token() else { continue };
        match token.kind() {
            SyntaxKind::WHITESPACE => continue,
            SyntaxKind::IDENT if ident.is_none() => ident = Some(token.text().to_string()),
            _ => return None,
        }
    }
    ident
}

fn complete_common_module_methods(
    db: &dyn RootDatabase,
    position: &CompletionPosition,
    module_name: &str,
) -> Option<Vec<CompletionItem>> {
    let name = Name::new(module_name);
    let resolver = hir::Resolver::with_workspace_scope(hir::ModuleId::new(position.file_id));
    if !resolver.user_common_module_exists(db, &name) {
        return None;
    }

    let source_root_input = db.file_source_root_input(position.file_id);
    let source_root_id = source_root_input.source_root_id(db);
    let module_index = db.module_index(source_root_id);
    let module_file_id = module_index.resolve_common_module(&name)?;

    tracing::debug!(
        module_name = %module_name,
        file_id = ?module_file_id,
        "Found CommonModule in module_index"
    );

    let module_id = hir::ModuleId::new(module_file_id);
    let symbol_tree = db.symbol_tree_ref(module_id);

    let items: Vec<CompletionItem> = symbol_tree
        .methods()
        .filter(|m| m.is_export)
        .map(|method| render_common_module_method(db, position.file_id, &name, method))
        .collect();

    tracing::debug!(item_count = items.len(), "CommonModule completion items");

    if items.is_empty() {
        return None;
    }

    Some(items)
}

pub(super) fn render_common_module_method(
    db: &dyn RootDatabase,
    file_id: FileId,
    module_name: &Name,
    method: &MethodSymbol,
) -> CompletionItem {
    let callee =
        CalleeKind::CommonModuleMethod { module: module_name.clone(), method: method.name.clone() };
    match build_signature(db, file_id, &callee) {
        Some(sigs) => {
            sigs.first().map(item_from_signature).unwrap_or_else(|| fallback_item(method))
        }
        None => fallback_item(method),
    }
}

pub(super) fn item_from_signature(sig: &SymbolSignature) -> CompletionItem {
    let detail = render_completion_detail(sig);
    let kind = match sig.source {
        SignatureSource::Platform | SignatureSource::PlatformManager => CompletionItemKind::Method,
        SignatureSource::PlatformConstructor => CompletionItemKind::Constructor,
        SignatureSource::GlobalFunction => CompletionItemKind::Function,
        SignatureSource::CommonModule | SignatureSource::ManagerModule | SignatureSource::Local => {
            match sig.kind {
                MethodKind::Function => CompletionItemKind::Function,
                MethodKind::Procedure => CompletionItemKind::Method,
            }
        }
    };
    item_from_detail(detail, kind)
}

fn item_from_detail(detail: CompletionDetail, kind: CompletionItemKind) -> CompletionItem {
    let CompletionDetail { label, detail, documentation, insert_text, filter_text } = detail;
    let documentation = if documentation.is_empty() { None } else { Some(documentation) };
    CompletionItem {
        label,
        detail: Some(detail),
        kind,
        insert_text,
        documentation,
        sort_text: None,
        filter_text,
        source: None,
    }
}

fn fallback_item(method: &MethodSymbol) -> CompletionItem {
    let label = method.name.to_string();
    let kind =
        if method.is_function { CompletionItemKind::Function } else { CompletionItemKind::Method };
    let insert_text = format!("{}($0)", label);
    CompletionItem {
        label: label.clone(),
        detail: Some(label),
        kind,
        insert_text,
        documentation: None,
        sort_text: None,
        filter_text: None,
        source: None,
    }
}

fn complete_platform_methods(
    db: &dyn RootDatabase,
    receiver_type: &str,
    locale: ide_db::base_db::Locale,
    env: &EnvFilter,
) -> Vec<CompletionItem> {
    let methods_input = TypeNameInput::new(db, receiver_type.to_string());
    let methods = type_methods_query(db, methods_input);
    let props_input = TypeNameInput::new(db, receiver_type.to_string());
    let properties = type_properties_query(db, props_input);

    tracing::debug!(
        method_count = methods.len(),
        property_count = properties.len(),
        "Found platform members"
    );

    // Homonym type names resolve to an arbitrary entry, so their per-member
    // availability is unreliable — offer everything (mirrors the diagnostics'
    // `is_ambiguous_type_name` gate).
    let judge_env = !PlatformDataInner::instance().is_ambiguous_type_name(receiver_type);

    let mut items: Vec<CompletionItem> = methods
        .iter()
        .filter(|m| !judge_env || env.admits_context(m.context.as_ref()))
        .map(render_platform_method)
        .collect();
    items.extend(
        properties
            .iter()
            .filter(|p| !judge_env || env.admits_context(p.context.as_ref()))
            .map(|p| render_platform_property(p, locale)),
    );
    items
}

pub(super) fn render_manager_method(method: &PlatformMethod) -> CompletionItem {
    let docs = PlatformDataInner::instance().get_method_docs(method.id);
    let mut sigs = from_platform_method(method, docs.as_ref());
    if let Some(sig) = sigs.first_mut() {
        sig.source = SignatureSource::PlatformManager;
    }
    let sig = sigs.first().expect("from_platform_method returns at least one signature");
    item_from_signature(sig)
}

pub(super) fn render_platform_method(method: &PlatformMethod) -> CompletionItem {
    let docs = PlatformDataInner::instance().get_method_docs(method.id);
    let sigs = from_platform_method(method, docs.as_ref());
    let sig = sigs.first().expect("from_platform_method returns at least one signature");
    item_from_signature(sig)
}

pub(super) fn render_platform_property(
    prop: &PlatformProperty,
    locale: ide_db::base_db::Locale,
) -> CompletionItem {
    let type_summary = if prop.property_types.is_empty() {
        String::from("Произвольный")
    } else {
        prop.property_types.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    };
    let detail = if prop.is_readonly {
        format!("{type_summary} {}", read_only_marker(locale))
    } else {
        type_summary
    };

    let documentation = PlatformDataInner::instance()
        .get_property_docs(prop.id)
        .map(|d| {
            let mut out = d.description;
            if let Some(notes) = d.notes {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&notes);
            }
            out
        })
        .filter(|s| !s.is_empty());

    CompletionItem {
        label: prop.name.to_string(),
        detail: Some(detail),
        kind: CompletionItemKind::Property,
        insert_text: prop.name.to_string(),
        documentation,
        sort_text: None,
        filter_text: Some(format!("{} {}", prop.name, prop.english_name)),
        source: None,
    }
}

fn projection_column_items<DB: RootDatabase>(
    db: &DB,
    receiver: TypeId,
    locale: ide_db::base_db::Locale,
) -> Vec<CompletionItem> {
    let projection = match db.lookup_type(receiver) {
        TypeKind::QueryResultSelection(facet) => facet.projection.clone(),
        TypeKind::ValueTable(facet) | TypeKind::ValueTableRow(facet) => facet.projection.clone(),
        // Known structure fields are emitted before the platform `Структура` methods (the caller
        // appends `complete_platform_methods` after these), so users see both fields and methods.
        TypeKind::Structure(facet) => facet.fields.clone(),
        _ => None,
    };
    let Some(projection) = projection else { return Vec::new() };
    let shadows = projection.raw_sdbl_types.as_deref();
    projection
        .fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let (name, ty) = (&field.name, field.ty);
            let label = name.as_str().to_string();
            let detail = shadows
                .and_then(|s| s.get(i))
                .map(|shadow| shadow.display.clone())
                .unwrap_or_else(|| hir::kernel_type_label(db, ty, locale, false));
            CompletionItem {
                label: label.clone(),
                detail: Some(detail),
                kind: CompletionItemKind::Field,
                insert_text: label.clone(),
                documentation: None,
                sort_text: Some(format!("0_{label}")),
                filter_text: None,
                source: None,
            }
        })
        .collect()
}

fn complete_form_elements_collection<DB: RootDatabase>(
    db: &DB,
    file_id: FileId,
    locale: ide_db::base_db::Locale,
    env: &EnvFilter,
) -> Option<Vec<CompletionItem>> {
    let sema = Semantics::new(db);
    let form = sema.form(file_id)?;

    let mut items: Vec<CompletionItem> =
        form.elements.iter().map(|el| render_form_element(el, locale)).collect();

    let mut seen: std::collections::HashSet<String> =
        items.iter().map(|i| i.label.fold_lower()).collect();
    for p in complete_platform_methods(db, hir::FORM_ITEMS_TYPE_RU, locale, env) {
        if seen.insert(p.label.fold_lower()) {
            items.push(p);
        }
    }

    Some(items)
}

fn render_form_element(
    element: &bsl_metadata::FormElement,
    locale: ide_db::base_db::Locale,
) -> CompletionItem {
    let detail = hir::form_element_kind_label(element.kind, locale).to_string();
    let sort_text = format!("{}_", hir::form_element_kind_sort_band(element.kind));

    CompletionItem {
        label: element.name.clone(),
        detail: Some(detail),
        kind: CompletionItemKind::Field,
        insert_text: element.name.clone(),
        documentation: None,
        sort_text: Some(sort_text),
        filter_text: Some(element.name.clone()),
        source: None,
    }
}

pub(super) fn render_mdo_field<DB: RootDatabase>(
    db: &DB,
    field: &Field,
    locale: ide_db::base_db::Locale,
) -> CompletionItem {
    let filter_text = format!("{} {}", field.name, field.english_name);
    CompletionItem {
        label: field.name.to_string(),
        detail: Some(render_field_detail(db, field, locale)),
        kind: CompletionItemKind::Field,
        insert_text: field.name.to_string(),
        documentation: None,
        sort_text: Some(sort_key_for_origin(field.origin).to_string()),
        filter_text: Some(filter_text),
        source: None,
    }
}

fn render_field_detail<DB: RootDatabase>(
    db: &DB,
    field: &Field,
    locale: ide_db::base_db::Locale,
) -> String {
    let mut body = if let Some(value_ty) = field.value_ty {
        format!(
            "{} → {}",
            render_ty_detail(db, field.ty, locale),
            render_ty_detail(db, value_ty, locale)
        )
    } else {
        render_ty_detail(db, field.ty, locale)
    };
    match field.origin {
        HirFieldOrigin::FormAttribute => body.push_str(" (реквизит формы)"),
        HirFieldOrigin::MainFormAttribute => body.push_str(" (основной реквизит формы)"),
        _ => {}
    }
    if field.is_readonly {
        format!("{body} {}", read_only_marker(locale))
    } else {
        body
    }
}

fn render_ty_detail<DB: RootDatabase>(
    db: &DB,
    id: TypeId,
    locale: ide_db::base_db::Locale,
) -> String {
    hir::kernel_type_label(db, id, locale, false)
}

fn read_only_marker(locale: ide_db::base_db::Locale) -> &'static str {
    match locale {
        ide_db::base_db::Locale::Ru => "[Только чтение]",
        ide_db::base_db::Locale::En => "[Read-only]",
    }
}

fn sort_key_for_origin(origin: HirFieldOrigin) -> &'static str {
    match origin {
        HirFieldOrigin::UserAttribute => "10_",
        HirFieldOrigin::FormAttribute | HirFieldOrigin::MainFormAttribute => "10_",
        HirFieldOrigin::TabularSection => "20_",
        HirFieldOrigin::StandardAttribute => "30_",
        HirFieldOrigin::TabularSectionRowColumn => "40_",
        HirFieldOrigin::RegisterDimension
        | HirFieldOrigin::RegisterResource
        | HirFieldOrigin::RegisterAttribute => "50_",
        HirFieldOrigin::MetadataReference => "55_",
        HirFieldOrigin::PlatformProperty => "60_",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_platform::{ContextAvailability, MethodParam, PlatformMethod};
    use hir::Builders;

    fn create_test_method() -> PlatformMethod {
        PlatformMethod {
            id: 999999,
            type_name: "Строка".into(),
            name: "ВРег".into(),
            english_name: "Upper".into(),
            return_type: Some("Строка".into()),
            parameters: vec![MethodParam {
                name: "Значение".into(),
                param_type: Some("Строка".into()),
                is_optional: false,
                is_variadic: false,
                passing: None,
                result_follows_argument_text: false,
            }],
            variants: Vec::new(),
            min_version: Some("8.0".into()),
            context: Some(ContextAvailability {
                thick_client: true,
                thin_client: true,
                web_client: true,
                server: true,
                mobile_client: false,
                external_connection: true,
            }),
        }
    }

    #[test]
    fn test_render_platform_method() {
        let method = create_test_method();
        let item = render_platform_method(&method);

        assert_eq!(item.label, "ВРег");
        assert_eq!(item.kind, CompletionItemKind::Method);
        assert!(item.insert_text.starts_with("ВРег("));
        assert!(item.insert_text.ends_with("$0)"));
        let detail = item.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("ВРег") && detail.contains("Строка"),
            "Detail should contain method name and parameter type, got: {detail}"
        );
    }

    #[test]
    fn any_metadata_ref_completes_flavour_ref_surface() {
        use bsl_platform::PlatformDataInner;
        use ide_db::base_db::Locale;
        use ide_db::RootDatabaseImpl;

        if PlatformDataInner::instance().all_methods().is_empty() {
            return;
        }
        let db = RootDatabaseImpl::new();

        let any_catalog = db.any_metadata_ref(bsl_metadata::MdoType::Catalog);
        assert!(
            !collect_platform_items(&db, any_catalog, Locale::Ru, &EnvFilter::permissive())
                .is_empty(),
            "AnyMetadataRef<Catalog> must offer the CatalogRef method surface"
        );

        let any_register = db.any_metadata_ref(bsl_metadata::MdoType::InformationRegister);
        assert!(
            collect_platform_items(&db, any_register, Locale::Ru, &EnvFilter::permissive())
                .is_empty(),
            "register-flavour any-ref has no ref method surface"
        );
    }

    /// An external object's members live under its bare platform type, not a
    /// dotted prefix; completion must take the same detour inference does.
    #[test]
    fn external_object_completes_its_bare_platform_type_surface() {
        use bsl_platform::PlatformDataInner;
        use bsl_types::testing::RootConfigCtx;
        use ide_db::base_db::Locale;
        use ide_db::RootDatabaseImpl;

        if PlatformDataInner::instance().all_methods().is_empty() {
            return;
        }
        let db = RootDatabaseImpl::new();
        let offers = |kind: hir::MetadataKind| {
            let receiver = db.metadata_ref(kind, "АРМ".to_string(), &RootConfigCtx);
            collect_platform_items(&db, receiver, Locale::Ru, &EnvFilter::permissive())
                .iter()
                .any(|item| item.label == "ПолучитьФорму")
        };
        assert!(offers(hir::MetadataKind::DataProcessorObject), "control: the internal object");
        assert!(
            offers(hir::MetadataKind::ExternalDataProcessorObject),
            "the external processor offers its own platform method"
        );
        assert!(offers(hir::MetadataKind::ExternalReportObject), "and so does the external report");
    }

    #[test]
    fn test_end_to_end_platform_completion() {
        use bsl_platform::PlatformDataInner;
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use syntax::TextSize;
        use vfs::{FileId, FileSet, VfsPath};

        let data = PlatformDataInner::instance();
        if data.all_types().is_empty() || data.all_methods().is_empty() {
            println!("Skipping test: no platform data available");
            return;
        }

        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);

        let code = r#"Процедура Тест()
    Результат = XBase.
КонецПроцедуры"#;

        db.set_file_text(file_id, code);
        let mut file_set = FileSet::default();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));

        let dot_end = code.find("XBase.").unwrap() + "XBase.".len();
        let offset = TextSize::from(dot_end as u32);

        let position = CompletionPosition {
            file_id,
            offset,
            workspace_root: None,
            locale: ide_db::base_db::Locale::Ru,
        };

        let items = platform_completions(&db, position);

        assert!(items.is_some(), "Expected platform completions after DOT on XBase type");

        let items = items.unwrap();
        assert!(!items.is_empty(), "Expected at least one method completion");

        let methods: Vec<&CompletionItem> =
            items.iter().filter(|i| i.kind == CompletionItemKind::Method).collect();
        let properties: Vec<&CompletionItem> =
            items.iter().filter(|i| i.kind == CompletionItemKind::Property).collect();
        assert_eq!(
            methods.len() + properties.len(),
            items.len(),
            "Unexpected CompletionItemKind in platform member completion"
        );
        println!("Found {} method + {} property completions", methods.len(), properties.len());

        for item in methods {
            assert!(
                item.insert_text.ends_with("$0)"),
                "Method snippet should end with $0): {}",
                item.insert_text
            );
            assert!(
                item.insert_text.contains('(') && item.insert_text.contains(')'),
                "Method snippet should have parentheses: {}",
                item.insert_text
            );
        }
        for item in properties {
            assert!(
                !item.insert_text.contains('('),
                "Property insert_text must not contain '(': {}",
                item.insert_text
            );
            assert_eq!(item.insert_text, item.label);
        }
    }

    #[test]
    fn test_completion_after_platform_global() {
        use bsl_platform::PlatformDataInner;
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use syntax::TextSize;
        use vfs::{FileId, FileSet, VfsPath};

        let data = PlatformDataInner::instance();
        if data.all_global_properties().is_empty() {
            println!("Skipping: no platform global properties available");
            return;
        }

        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let code = r#"Процедура Тест()
    Текст = ОбработкаОшибок.
КонецПроцедуры"#;

        db.set_file_text(file_id, code);
        let mut file_set = FileSet::default();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));

        let dot_end = code.find("ОбработкаОшибок.").unwrap() + "ОбработкаОшибок.".len();
        let offset = TextSize::from(dot_end as u32);
        let position = CompletionPosition {
            file_id,
            offset,
            workspace_root: None,
            locale: ide_db::base_db::Locale::Ru,
        };

        let items = platform_completions(&db, position).expect(
            "platform_completions must surface МенеджерОбработкиОшибок methods after global property",
        );
        assert!(!items.is_empty(), "expected at least one method, got empty list");

        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"КраткоеПредставлениеОшибки"),
            "expected КраткоеПредставлениеОшибки in {:?}",
            labels
        );
    }

    fn form_table_binding() -> hir::FormBindingFacet {
        use bsl_metadata::MdoType;
        use hir::{FormBindingFacet, FormBindingTargetFacet, MdoRefFacet};
        FormBindingFacet::new(
            std::sync::Arc::from(["Объект".to_string(), "Переприемка".to_string()]),
            FormBindingTargetFacet::TabularSection {
                mdo_ref: MdoRefFacet::new(MdoType::Document, "ПКО".to_string()),
                section: "Переприемка".to_string(),
            },
        )
    }

    fn make_db_with_file() -> (ide_db::RootDatabaseImpl, vfs::FileId) {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{FileId, FileSet, VfsPath};

        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        db.set_file_text(file_id, "");
        let mut file_set = FileSet::default();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        (db, file_id)
    }

    #[test]
    fn complete_prefix_methods_returns_none_for_form_control_table_with_binding() {
        let (db, file_id) = make_db_with_file();
        let ty = db.mk_form_control(hir::FormElementKind::Table, Some(form_table_binding()));

        let result = complete_prefix_methods_for_receiver(
            &db,
            ty,
            file_id,
            ide_db::base_db::Locale::Ru,
            &EnvFilter::permissive(),
        );
        assert!(
            result.is_none(),
            "FormControl{{Table, Some(_)}} must not trigger MDO-field completion; got {:?}",
            result.as_ref().map(|v| v.iter().map(|i| &i.label).collect::<Vec<_>>())
        );
    }

    #[test]
    fn complete_prefix_methods_returns_none_for_form_control_table_no_binding() {
        let (db, file_id) = make_db_with_file();
        let ty = db.mk_form_control(hir::FormElementKind::Table, None);

        let result = complete_prefix_methods_for_receiver(
            &db,
            ty,
            file_id,
            ide_db::base_db::Locale::Ru,
            &EnvFilter::permissive(),
        );
        assert!(
            result.is_none(),
            "FormControl{{Table, None}} must not trigger MDO-field completion"
        );
    }

    #[test]
    fn complete_prefix_methods_returns_none_for_typed_array() {
        let (db, file_id) = make_db_with_file();
        let element = db.platform_object("СтрокаТаблицыФормы".to_string());
        let ty = db.array(Some(element));

        let result = complete_prefix_methods_for_receiver(
            &db,
            ty,
            file_id,
            ide_db::base_db::Locale::Ru,
            &EnvFilter::permissive(),
        );
        assert!(result.is_none(), "TypedArray(_) must not trigger MDO-field completion");
    }

    #[test]
    fn complete_platform_methods_for_form_table_surfaces_refined_members() {
        use bsl_platform::PlatformDataInner;
        let data = PlatformDataInner::instance();
        if data.all_properties().is_empty() {
            println!("Skipping: no platform property data available");
            return;
        }

        let (db, _) = make_db_with_file();
        let items = complete_platform_methods(
            &db,
            "ТаблицаФормы",
            ide_db::base_db::Locale::Ru,
            &EnvFilter::permissive(),
        );
        assert!(
            !items.is_empty(),
            "ТаблицаФормы platform members must not be empty; bilingual lookup misroute?"
        );
        let labels: std::collections::HashSet<&str> =
            items.iter().map(|i| i.label.as_str()).collect();
        for expected in
            ["ВыделенныеСтроки", "ТекущаяСтрока", "ТекущиеДанные", "Видимость", "Заголовок"]
        {
            assert!(
                labels.contains(expected),
                "expected platform property `{expected}` in completion; got: {:?}",
                labels
            );
        }
    }

    #[test]
    fn complete_platform_methods_for_typed_array_surfaces_massiv_members() {
        use bsl_platform::PlatformDataInner;
        let data = PlatformDataInner::instance();
        if data.all_methods().is_empty() {
            println!("Skipping: no platform method data available");
            return;
        }

        let (db, _) = make_db_with_file();
        let items = complete_platform_methods(
            &db,
            "Массив",
            ide_db::base_db::Locale::Ru,
            &EnvFilter::permissive(),
        );
        assert!(!items.is_empty(), "Массив platform members must not be empty");
        let labels: std::collections::HashSet<&str> =
            items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains("Количество"),
            "expected `Количество` (collection size) in Массив completion; got: {:?}",
            labels
        );
    }

    #[test]
    fn render_form_element_uses_entity_level_label_and_sort_band() {
        use bsl_metadata::{FormElement, FormElementKind};
        use ide_db::base_db::Locale;

        let cases = [
            (FormElementKind::Table, Locale::Ru, "Таблица", "10_"),
            (FormElementKind::Table, Locale::En, "Table", "10_"),
            (FormElementKind::Pages, Locale::Ru, "Страницы", "20_"),
            (FormElementKind::Pages, Locale::En, "Pages", "20_"),
            (FormElementKind::UsualGroup, Locale::Ru, "Обычная группа", "20_"),
            (FormElementKind::Field, Locale::Ru, "Поле", "30_"),
            (FormElementKind::Button, Locale::Ru, "Кнопка", "40_"),
            (FormElementKind::Decoration, Locale::Ru, "Декорация", "50_"),
            (FormElementKind::Addition, Locale::Ru, "Дополнение", "60_"),
            (FormElementKind::Other, Locale::Ru, "Элемент формы", "70_"),
        ];

        for (kind, locale, expected_detail, expected_sort) in cases {
            let element = FormElement::with_kind("X".to_string(), 1, None, kind, None);
            let item = render_form_element(&element, locale);
            assert_eq!(item.kind, CompletionItemKind::Field);
            assert_eq!(item.label, "X");
            assert_eq!(item.insert_text, "X");
            assert_eq!(
                item.detail.as_deref(),
                Some(expected_detail),
                "detail mismatch for {kind:?}"
            );
            assert_eq!(
                item.sort_text.as_deref(),
                Some(expected_sort),
                "sort_text mismatch for {kind:?}"
            );
        }
    }

    #[test]
    fn is_form_items_collection_ty_round_trips_bilingual() {
        let db = ide_db::RootDatabaseImpl::new();
        let platform_object = |n: &str| db.platform_object(n.to_string());
        assert!(hir::is_form_items_collection_ty(&db, platform_object(hir::FORM_ITEMS_TYPE_RU)));
        assert!(hir::is_form_items_collection_ty(&db, platform_object(hir::FORM_ITEMS_TYPE_EN)));
        assert!(hir::is_form_items_collection_ty(&db, platform_object("всеЭлементыФормы")));
        assert!(!hir::is_form_items_collection_ty(&db, platform_object("Запрос")));
        assert!(!hir::is_form_items_collection_ty(&db, db.number(None, None)));
    }
}
