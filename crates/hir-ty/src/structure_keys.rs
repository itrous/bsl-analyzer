//! Inference of `Структура` keys from literal construction.
//!
//! A local built as `Новый Структура("Ключ1, Ключ2")` and/or extended with
//! `.Вставить("Ключ", Значение)` in the same body has a knowable set of keys. This module collects
//! that shape syntactically (Phase 0, no `db`) and materialises it into a typed
//! [`bsl_types::facet::StructureFacet`] during inference (Phase 1), so member completion and hover
//! after a dot can surface the keys (and the keys of a simple nested structure value).
//!
//! Known fields stay soft unless the same whole-body pass also proves a non-empty complete shape.
//! Any alias, escape, dynamic key, or unknown mutation opens that shape conservatively.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::{eq_ignore_case, CaseExt};

use bsl_types::builders::Builders;
use bsl_types::facet::StructureFacet;
use bsl_types::kind::{
    Projection, ProjectionField, ProjectionFieldSource, ProjectionOrigin, TypeId, TypeOrigin,
};
use cfg_types::{ExprId, IdConversion, StmtId};
use hir_def::body::Body;
use hir_def::hir::{Expr, Literal, Stmt};

use crate::db::HirDatabase;

/// Hard cap on nested-structure recursion and receiver-chain depth, applied in the collector and
/// the materialiser. Beyond it a nested value degrades to an untyped structure.
const MAX_NEST_DEPTH: usize = 4;

/// Where a structure key's value type comes from. Deliberately depends only on syntax (Phase 0) and
/// already-stable types — NEVER on the inference cache (`expr_types`). A structure used as a
/// method's return value flows into `method_return_type`'s fixpoint; if its interned type shifted
/// with provisional value types across cycle iterations, that fixpoint would never converge. So a
/// non-literal value contributes only its key *name* (`Unknown` type), not its inferred type.
pub(crate) enum ValueSource {
    /// The value is itself a literal `Новый Структура(...)` whose shape we collected.
    Literal(StructureShape),
    /// The value is a scalar literal — typed directly and stably.
    Scalar(ScalarKind),
    /// A type already resolved elsewhere — used by Stage 2 to inject a callee summary's field types
    /// (an interned `TypeId`, never an `ExprId`; the summary itself is computed stably).
    Resolved(TypeId),
    /// No value, or a non-literal expression whose type we do not pin (key still surfaced).
    Unknown,
}

/// Scalar literal kinds we can type without consulting inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarKind {
    String,
    Number,
    Boolean,
    Date,
    Null,
    Undefined,
}

struct StructField {
    /// Original-case key name (display + projection).
    name: String,
    source: ValueSource,
}

/// Collected keys (and nested shapes) of one structure local, flow-insensitive whole-body union.
/// Known keys are retained for completion/hover even when an unsafe event opens the shape.
#[derive(Default)]
pub(crate) struct StructureShape {
    /// Keys in first-seen order; de-duplicated last-wins on value by case-insensitive name.
    fields: Vec<StructField>,
    invalidated: bool,
}

impl StructureShape {
    pub(crate) fn upsert(&mut self, name: &str, source: ValueSource) {
        if let Some(existing) = self.fields.iter_mut().find(|f| eq_ignore_case(&f.name, name)) {
            existing.source = source;
        } else {
            self.fields.push(StructField { name: name.to_string(), source });
        }
    }

    fn merge(&mut self, other: StructureShape) {
        self.invalidated |= other.invalidated;
        for f in other.fields {
            self.upsert(&f.name, f.source);
        }
    }

    /// Открывает форму вместе со всеми вложенными: значение, о котором больше ничего не
    /// известно, ничего не говорит и о составе ключей своих вложенных структур.
    fn invalidate(&mut self) {
        self.invalidated = true;
        for field in &mut self.fields {
            if let ValueSource::Literal(child) = &mut field.source {
                child.invalidate();
            }
        }
    }

    fn is_closed(&self) -> bool {
        !self.invalidated && !self.fields.is_empty()
    }
}

/// What seeds the tracked roots of a structure-shape collection.
#[derive(Clone, Copy)]
pub(crate) enum SeedRoots<'a> {
    /// Stage 1: roots are locals assigned a `Новый Структура` literal in this body.
    NewLiterals,
    /// Stage 2: roots are the (by-reference) parameter names — pre-tracked even though they are not
    /// constructed here; only `.Вставить` and forwarding contribute their keys.
    ParamNames(&'a [String]),
}

/// Collect, per local/param name (lowercased), the structure shape built in this body.
///
/// A single pass in statement (≈ source) order, so a seed assignment and a `.Вставить` writing the
/// same key interleave correctly: the last write in the body wins. An insert before its root is
/// tracked is ignored (you cannot insert into an unconstructed structure).
pub(crate) fn collect_structure_shapes(
    body: &Body,
    seed_roots: SeedRoots,
    forwarder: Option<&crate::structure_param_keys::Forwarder>,
) -> FxHashMap<String, StructureShape> {
    let mut shapes: FxHashMap<String, StructureShape> = FxHashMap::default();
    // Roots that may no longer be extended: a by-reference param that has been reassigned keeps the
    // keys accumulated while it still aliased the caller, but ignores later inserts/forwards.
    let mut frozen: FxHashSet<String> = FxHashSet::default();
    // Names already used by an earlier statement. The union below is whole-body, but completeness
    // is claimed at the point of the read: a name that lived before its constructor (a parameter, a
    // module-level variable) was something else there, and this literal does not describe it.
    let mut used_before_seed: FxHashSet<String> = FxHashSet::default();
    // Записи внутри ветки, цикла или попытки могут не состояться, поэтому доказательством
    // полного состава ключей они не служат: их форма остаётся открытой.
    let conditional = conditionally_executed_stmts(body);

    if let SeedRoots::ParamNames(names) = seed_roots {
        for name in names {
            shapes.entry(name.clone()).or_default();
        }
    }

    // One pass in statement (≈ source) order. Seeds, `.Вставить` inserts, and interprocedural
    // forwarding all interleave here, so a root is only ever extended after it is live and the last
    // write to a key wins.
    for (id, stmt) in body.stmts_iter() {
        let conditionally_executed = conditional.contains(&id);
        match stmt {
            Stmt::Assign { target, value } => {
                invalidate_expression_escapes(body, &mut shapes, *target, forwarder, false);
                invalidate_expression_escapes(body, &mut shapes, *value, forwarder, true);
                // Seed: `Local = Новый Структура(...)` — Stage-1 (`NewLiterals`) only; a constructor
                // assignment to a parameter is a reassignment that breaks aliasing (handled below).
                if matches!(seed_roots, SeedRoots::NewLiterals) {
                    // Правая часть читается ДО того, как имя получит новое значение:
                    // `С = Новый Структура("А", С.Б)` — это чтение прежнего `С`.
                    record_used_names_in_expr(body, *value, &mut used_before_seed);
                    if let Expr::Path(name) = body.expr_idx(*target) {
                        if let Some(mut shape) =
                            constructor_shape_of(body, ExprId::from_idx(*value), 0)
                        {
                            let key = name.as_str().fold_lower();
                            if let Some(existing) = shapes.get_mut(&key) {
                                existing.merge(shape);
                                existing.invalidate();
                            } else {
                                if conditionally_executed || used_before_seed.contains(&key) {
                                    shape.invalidate();
                                }
                                shapes.insert(key, shape);
                            }
                            record_used_names(body, stmt, &mut used_before_seed);
                            continue;
                        }
                    }
                }
                // The RHS may forward a tracked root to a helper (`Х = Заполнить(С)`), including a
                // by-ref param filled during the call (`П = F(П)`) — those keys reach the caller
                // before the reassignment completes, so fold BEFORE freezing the root below.
                if let Some(fw) = forwarder {
                    fw.fold_call(body, &mut shapes, &frozen, body.expr_idx(*value));
                }

                let target_path = receiver_root_path(body, ExprId::from_idx(*target), 0);
                if let Some((root, path)) = target_path {
                    invalidate_shape_path(&mut shapes, &root, &path);
                }
                // Stage 2: reassigning a tracked parameter breaks its aliasing with the caller's
                // argument. Freeze it in source order — keys inserted earlier still reach the
                // caller, but later inserts into the fresh value do not.
                if matches!(seed_roots, SeedRoots::ParamNames(_)) {
                    if let Expr::Path(name) = body.expr_idx(*target) {
                        let key = name.as_str().fold_lower();
                        if shapes.contains_key(&key) {
                            frozen.insert(key);
                        }
                    }
                }
            }
            Stmt::Expr(expr_idx) => {
                let call = body.expr_idx(*expr_idx);
                // Insert: `Receiver.Вставить("Ключ", Значение)` on an already-tracked root.
                if let Some((receiver, method, args)) = as_method_call(body, call) {
                    if is_insert_method(method) {
                        if let Some((root, path)) = receiver_root_path(body, receiver, 0) {
                            if shapes.contains_key(&root) && !frozen.contains(&root) {
                                let root_shape =
                                    shapes.get_mut(&root).expect("checked contains_key");
                                if let Some(target_shape) = navigate_mut(root_shape, &path) {
                                    apply_insert(body, target_shape, args);
                                }
                                // Запись под условием и добавляет ключ, которого может не
                                // быть, и подменяет значение, которое могло остаться
                                // прежним, — набор ключей корня после неё не доказан.
                                if conditionally_executed {
                                    root_shape.invalidate();
                                }
                            }
                        }
                    }
                }
                // A non-insert call statement may forward a tracked root to a helper (`Заполнить(С)`).
                if let Some(fw) = forwarder {
                    fw.fold_call(body, &mut shapes, &frozen, call);
                }
                invalidate_expression_escapes(body, &mut shapes, *expr_idx, forwarder, false);
            }
            Stmt::Return { value: Some(value) } => {
                let expr = body.expr_idx(*value);
                if let Some(fw) = forwarder {
                    fw.fold_call(body, &mut shapes, &frozen, expr);
                }
                invalidate_expression_escapes(body, &mut shapes, *value, forwarder, true);
            }
            Stmt::Raise { value: Some(value) } => {
                invalidate_escape_expression(body, &mut shapes, *value, forwarder);
            }
            Stmt::Execute { expr } => {
                // Текст исполняемого кода произволен: он может вставить ключ в любую структуру
                // тела, не называя её ни одним операндом этого оператора.
                invalidate_escape_expression(body, &mut shapes, *expr, forwarder);
                invalidate_every_shape(&mut shapes);
            }
            Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
                invalidate_escape_expression(body, &mut shapes, *event, forwarder);
                invalidate_escape_expression(body, &mut shapes, *handler, forwarder);
            }
            Stmt::If(if_stmt) => {
                invalidate_expression_escapes(
                    body,
                    &mut shapes,
                    if_stmt.condition,
                    forwarder,
                    false,
                );
                for (condition, _) in if_stmt.elsif_branches.iter() {
                    invalidate_expression_escapes(body, &mut shapes, *condition, forwarder, false);
                }
            }
            Stmt::While { condition, .. } => {
                invalidate_expression_escapes(body, &mut shapes, *condition, forwarder, false);
            }
            Stmt::For { from, to, .. } => {
                invalidate_expression_escapes(body, &mut shapes, *from, forwarder, false);
                invalidate_expression_escapes(body, &mut shapes, *to, forwarder, false);
            }
            Stmt::ForEach { collection, .. } => {
                invalidate_expression_escapes(body, &mut shapes, *collection, forwarder, false);
            }
            _ => {}
        }
        record_used_names(body, stmt, &mut used_before_seed);
    }

    shapes
}

/// Every name this statement mentions, at any depth of its expressions.
///
/// Служит одному вопросу: жило ли имя до своего конструктора. Поэтому берутся и цели
/// присваивания — имя, которому уже присваивали, к моменту литерала было чем-то другим.
fn record_used_names(body: &Body, stmt: &Stmt, used: &mut FxHashSet<String>) {
    let record_expr = |expr: hir_def::hir::ExprIdx, used: &mut FxHashSet<String>| {
        record_used_names_in_expr(body, expr, used);
    };
    match stmt {
        Stmt::Assign { target, value } => {
            record_expr(*target, used);
            record_expr(*value, used);
        }
        Stmt::Expr(expr) | Stmt::Execute { expr } => record_expr(*expr, used),
        Stmt::Return { value } | Stmt::Raise { value } => {
            if let Some(value) = value {
                record_expr(*value, used);
            }
        }
        Stmt::AddHandler { event, handler } | Stmt::RemoveHandler { event, handler } => {
            record_expr(*event, used);
            record_expr(*handler, used);
        }
        Stmt::If(if_stmt) => {
            record_expr(if_stmt.condition, used);
            for (condition, _) in if_stmt.elsif_branches.iter() {
                record_expr(*condition, used);
            }
        }
        Stmt::While { condition, .. } => record_expr(*condition, used),
        Stmt::For { from, to, .. } => {
            record_expr(*from, used);
            record_expr(*to, used);
        }
        Stmt::ForEach { collection, .. } => record_expr(*collection, used),
        _ => {}
    }
}

fn record_used_names_in_expr(
    body: &Body,
    expr: hir_def::hir::ExprIdx,
    used: &mut FxHashSet<String>,
) {
    if let Expr::Path(name) = body.expr_idx(expr) {
        used.insert(name.as_str().fold_lower());
    }
    crate::narrow::for_each_expr_child(body, expr, &mut |child| {
        record_used_names_in_expr(body, child, used);
    });
}

/// Операторы, исполнение которых зависит от условия: тела ветвей, циклов и попытки.
///
/// Плоский обход арены их не отличает — вложенный оператор приходит наравне с внешним и,
/// более того, РАНЬШЕ него. Поэтому список составляется отдельным проходом по спискам
/// блоков.
fn conditionally_executed_stmts(body: &Body) -> FxHashSet<StmtId> {
    let mut nested: FxHashSet<StmtId> = FxHashSet::default();
    let extend = |branch: &[hir_def::hir::StmtIdx], nested: &mut FxHashSet<StmtId>| {
        nested.extend(branch.iter().map(|idx| StmtId::from_idx(*idx)));
    };
    for (_id, stmt) in body.stmts_iter() {
        match stmt {
            Stmt::If(if_stmt) => {
                extend(&if_stmt.then_branch, &mut nested);
                for (_, branch) in if_stmt.elsif_branches.iter() {
                    extend(branch, &mut nested);
                }
                if let Some(branch) = &if_stmt.else_branch {
                    extend(branch, &mut nested);
                }
            }
            Stmt::PreprocIf(preproc) => {
                extend(&preproc.then_branch, &mut nested);
                for (_, _, branch) in preproc.elsif_branches.iter() {
                    extend(branch, &mut nested);
                }
                if let Some(branch) = &preproc.else_branch {
                    extend(branch, &mut nested);
                }
            }
            Stmt::While { body: branch, .. }
            | Stmt::For { body: branch, .. }
            | Stmt::ForEach { body: branch, .. } => extend(branch, &mut nested),
            Stmt::Try { body: branch, except } => {
                extend(branch, &mut nested);
                extend(except, &mut nested);
            }
            _ => {}
        }
    }
    nested
}

/// Открывает формы всех корней, названных в выражении.
fn invalidate_named_roots(
    body: &Body,
    shapes: &mut FxHashMap<String, StructureShape>,
    expr: hir_def::hir::ExprIdx,
) {
    let mut named = FxHashSet::default();
    record_used_names_in_expr(body, expr, &mut named);
    for name in named {
        if let Some(shape) = shapes.get_mut(&name) {
            shape.invalidate();
        }
    }
}

/// Открывает все формы тела разом — ответ на исполнение произвольного кода, который не
/// называет ни одного корня своим операндом.
fn invalidate_every_shape(shapes: &mut FxHashMap<String, StructureShape>) {
    for shape in shapes.values_mut() {
        shape.invalidate();
    }
}

/// Decompose a method/qualified call into `(receiver, method_name, args)`. A BSL `recv.Method(args)`
/// lowers to `Expr::Call { callee: Field { base, field }, .. }`; the dedicated `Expr::MethodCall`
/// form is also accepted defensively.
pub(crate) fn as_method_call<'b>(
    body: &'b Body,
    expr: &'b Expr,
) -> Option<(ExprId, &'b hir_def::Name, &'b [hir_def::hir::ExprIdx])> {
    match expr {
        Expr::MethodCall { receiver, method, args } => {
            Some((ExprId::from_idx(*receiver), method, args))
        }
        Expr::Call { callee, args } => match body.expr_idx(*callee) {
            Expr::Field { base, field } => Some((ExprId::from_idx(*base), field, args)),
            _ => None,
        },
        _ => None,
    }
}

/// Materialise the typed structure for `local_lower` using value types known so far.
pub(crate) fn materialize(
    db: &dyn HirDatabase,
    shapes: &FxHashMap<String, StructureShape>,
    local_lower: &str,
) -> Option<TypeId> {
    let shape = shapes.get(local_lower)?;
    // No known keys → keep the plain untyped structure (unchanged display/behaviour).
    let projection = shape_to_projection(db, shape, 0)?;
    Some(if shape.is_closed() {
        db.structure_typed_closed(projection, TypeOrigin::BslLiteral)
    } else {
        db.structure_typed(projection, TypeOrigin::BslLiteral)
    })
}

/// The value type of a structure field (`facet.fields`), matched case-insensitively. `None` if the
/// structure is untyped or has no such key — callers stay permissive (no diagnostic).
pub(crate) fn structure_projection_field(facet: &StructureFacet, field: &str) -> Option<TypeId> {
    let projection = facet.fields.as_ref()?;
    projection.fields.iter().find(|f| eq_ignore_case(&f.name, field)).map(|f| f.ty)
}

/// The full typed projection of a structure, if known.
pub(crate) fn structure_projection_fields(facet: &StructureFacet) -> Option<Arc<Projection>> {
    facet.fields.clone()
}

/// Build a shape's typed projection, or `None` if it has no keys (or the depth cap is hit). Shared
/// by Stage-1 local materialisation and the Stage-2 summary's per-param projections.
pub(crate) fn shape_to_projection(
    db: &dyn HirDatabase,
    shape: &StructureShape,
    depth: usize,
) -> Option<Arc<Projection>> {
    if depth > MAX_NEST_DEPTH || shape.fields.is_empty() {
        return None;
    }
    let fields: Vec<ProjectionField> = shape
        .fields
        .iter()
        .map(|f| {
            let ty = match &f.source {
                ValueSource::Literal(child) => match shape_to_projection(db, child, depth + 1) {
                    Some(p) if child.is_closed() => {
                        db.structure_typed_closed(p, TypeOrigin::BslLiteral)
                    }
                    Some(p) => db.structure_typed(p, TypeOrigin::BslLiteral),
                    None => db.structure(None),
                },
                ValueSource::Scalar(kind) => scalar_type(db, *kind),
                ValueSource::Resolved(t) => *t,
                ValueSource::Unknown => db.unknown(),
            };
            ProjectionField::new(f.name.clone(), ty, ProjectionFieldSource::StructureLiteral)
        })
        .collect();
    Some(Arc::new(Projection::new(fields.into(), ProjectionOrigin::StructureLiteral, None)))
}

pub(crate) fn scalar_type(db: &dyn HirDatabase, kind: ScalarKind) -> TypeId {
    match kind {
        ScalarKind::String => db.string(None, false),
        ScalarKind::Number => db.number(None, None),
        ScalarKind::Boolean => db.boolean(),
        ScalarKind::Date => db.date(bsl_types::facet::DateComponent::DateTime),
        ScalarKind::Null => db.null(),
        ScalarKind::Undefined => db.undefined(),
    }
}

/// The shape of a `Новый Структура(...)` constructor expression, or `None` if `expr` is not one.
fn constructor_shape_of(body: &Body, expr: ExprId, depth: usize) -> Option<StructureShape> {
    if depth > MAX_NEST_DEPTH {
        return Some(StructureShape { invalidated: true, ..StructureShape::default() });
    }
    let Expr::New { type_name: Some(name), args } = body.expr(expr) else { return None };
    if !is_structure_name(name) {
        return None;
    }
    let mut shape = StructureShape::default();
    let Some(first) = args.first() else { return Some(shape) };
    let Expr::Literal(Literal::String(keys_str)) = body.expr_idx(*first) else {
        // First constructor arg is not a literal key string → no nameable keys.
        shape.invalidate();
        return Some(shape);
    };
    for (i, key) in keys_str.split(',').map(str::trim).filter(|k| !k.is_empty()).enumerate() {
        // Constructor positional values map to keys: `Новый Структура("К1,К2", З1, З2)`.
        let source = match args.get(i + 1) {
            Some(value) => value_source_of(body, ExprId::from_idx(*value), depth),
            None => ValueSource::Unknown,
        };
        shape.upsert(key, source);
    }
    Some(shape)
}

/// Classify a value expression: a nested literal structure, or an opaque expression.
fn value_source_of(body: &Body, expr: ExprId, depth: usize) -> ValueSource {
    if let Some(nested) = constructor_shape_of(body, expr, depth + 1) {
        return ValueSource::Literal(nested);
    }
    match scalar_kind_of(body, expr) {
        Some(kind) => ValueSource::Scalar(kind),
        // A non-literal value (variable, call, …): surface the key name but do not pin a type, so
        // the structure's interned type stays stable across inference iterations.
        None => ValueSource::Unknown,
    }
}

/// The scalar kind of a literal value expression; `None` for anything that is not a literal.
pub(crate) fn scalar_kind_of(body: &Body, expr: ExprId) -> Option<ScalarKind> {
    let Expr::Literal(lit) = body.expr(expr) else { return None };
    Some(match lit {
        Literal::String(_) => ScalarKind::String,
        Literal::Number(_) => ScalarKind::Number,
        Literal::Bool(_) => ScalarKind::Boolean,
        Literal::Date(_) => ScalarKind::Date,
        Literal::Null => ScalarKind::Null,
        Literal::Undefined => ScalarKind::Undefined,
    })
}

fn apply_insert(body: &Body, shape: &mut StructureShape, args: &[hir_def::hir::ExprIdx]) {
    let Some(first) = args.first() else {
        shape.invalidate();
        return;
    };
    let Expr::Literal(Literal::String(key)) = body.expr_idx(*first) else {
        // Keep known keys for IDE surfaces, but the complete set is no longer proven.
        shape.invalidate();
        return;
    };
    let key = key.trim();
    if key.is_empty() {
        return;
    }
    let source = match args.get(1) {
        Some(value) => value_source_of(body, ExprId::from_idx(*value), 0),
        None => ValueSource::Unknown,
    };
    shape.upsert(key, source);
}

/// Decompose a receiver expression into `(root_local_lowercased, field_path_lowercased)`.
/// `С.Адрес.Вставить(…)` → `("с", ["адрес"])`; bounded by [`MAX_NEST_DEPTH`].
fn receiver_root_path(body: &Body, expr: ExprId, depth: usize) -> Option<(String, Vec<String>)> {
    if depth > MAX_NEST_DEPTH {
        return None;
    }
    match body.expr(expr) {
        Expr::Path(name) => Some((name.as_str().fold_lower(), Vec::new())),
        Expr::Field { base, field } => {
            let (root, mut path) = receiver_root_path(body, ExprId::from_idx(*base), depth + 1)?;
            path.push(field.as_str().fold_lower());
            Some((root, path))
        }
        _ => None,
    }
}

/// Descend into the nested literal shape addressed by `path`; `None` if a segment is missing or is
/// not itself a literal structure.
fn navigate_mut<'a>(
    shape: &'a mut StructureShape,
    path: &[String],
) -> Option<&'a mut StructureShape> {
    let Some((seg, rest)) = path.split_first() else { return Some(shape) };
    let idx = shape.fields.iter().position(|f| eq_ignore_case(&f.name, seg))?;
    match &mut shape.fields[idx].source {
        ValueSource::Literal(child) => navigate_mut(child, rest),
        _ => None,
    }
}

fn invalidate_shape_path(
    shapes: &mut FxHashMap<String, StructureShape>,
    root: &str,
    path: &[String],
) {
    let Some(shape) = shapes.get_mut(root) else { return };
    if let Some(target) = navigate_mut(shape, path) {
        target.invalidate();
    }
}

fn invalidate_escape_expression(
    body: &Body,
    shapes: &mut FxHashMap<String, StructureShape>,
    expr: hir_def::hir::ExprIdx,
    forwarder: Option<&crate::structure_param_keys::Forwarder>,
) {
    invalidate_expression_escapes(body, shapes, expr, forwarder, true);
}

fn invalidate_expression_escapes(
    body: &Body,
    shapes: &mut FxHashMap<String, StructureShape>,
    expr: hir_def::hir::ExprIdx,
    forwarder: Option<&crate::structure_param_keys::Forwarder>,
    escapes: bool,
) {
    if escapes {
        if let Some((root, path)) = receiver_root_path(body, ExprId::from_idx(expr), 0) {
            invalidate_shape_path(shapes, &root, &path);
            return;
        }
    }

    let node = body.expr_idx(expr);
    if let Some((receiver, method, _)) = as_method_call(body, node) {
        if !is_key_preserving_method(method) {
            match receiver_root_path(body, receiver, 0) {
                Some((root, path)) => {
                    if !is_insert_method(method) {
                        invalidate_shape_path(shapes, &root, &path);
                    }
                }
                // Приёмник — не цепочка имён (тернарник, обращение по индексу, результат
                // вызова): какое из названных в нём значений он вернёт, неизвестно, и
                // мутация может достаться любому. `Вставить` здесь опаснее прочих: его
                // ключ не записан ни в один корень, а форма осталась бы закрытой без него.
                None => invalidate_named_roots(body, shapes, receiver.to_idx()),
            }
        }
    }

    match node {
        Expr::Call { callee, args } => {
            if let Expr::Path(name) = body.expr_idx(*callee) {
                // Имя платформенной функции разрешено переопределить своей: та строкового
                // кода не исполняет и до чужой структуры без аргумента не дотянется.
                let shadowed_by_user_method = forwarder
                    .is_some_and(|fw| fw.callee_is_user_method(body, body.expr_idx(*callee)));
                if crate::method_lookup::is_platform_name(name, "Вычислить", "Eval")
                    && !shadowed_by_user_method
                {
                    invalidate_every_shape(shapes);
                }
            }
            invalidate_expression_escapes(body, shapes, *callee, forwarder, false);
            for (index, arg) in args.iter().copied().enumerate() {
                let by_value =
                    forwarder.is_some_and(|fw| fw.argument_is_by_value(body, node, index));
                invalidate_expression_escapes(body, shapes, arg, forwarder, !by_value);
            }
        }
        Expr::MethodCall { receiver, args, .. } => {
            invalidate_expression_escapes(body, shapes, *receiver, forwarder, false);
            for arg in args.iter().copied() {
                invalidate_expression_escapes(body, shapes, arg, forwarder, true);
            }
        }
        Expr::New { args, .. } | Expr::Array(args) => {
            for arg in args.iter().copied() {
                invalidate_expression_escapes(body, shapes, arg, forwarder, true);
            }
        }
        _ => crate::narrow::for_each_expr_child(body, expr, &mut |child| {
            invalidate_expression_escapes(body, shapes, child, forwarder, escapes);
        }),
    }
}

fn is_structure_name(name: &hir_def::Name) -> bool {
    crate::method_lookup::is_platform_name(name, "Структура", "Structure")
}

/// Cheap gate: does this body construct a `Структура` literal at all? If not, there is no tracked
/// root, so the whole collection (and the Stage-2 forwarder) can be skipped — the common case for
/// most bodies, keeping the feature off the hot inference path.
pub(crate) fn body_constructs_structure(body: &Body) -> bool {
    body.exprs_iter().any(|(_, expr)| {
        matches!(expr, Expr::New { type_name: Some(name), .. } if is_structure_name(name))
    })
}

pub(crate) fn is_insert_method(name: &hir_def::Name) -> bool {
    crate::method_lookup::is_platform_name(name, "Вставить", "Insert")
}

/// Методы `Структура`, которые состава ключей не меняют.
///
/// Методов у структуры ровно пять: `Вставить`, `Удалить`, `Очистить`, `Количество` и
/// `Свойство`. Первые три состав меняют, последние два — читают, и открывать форму после
/// них значит терять диагностику там, где терять нечего. Имя не из этих пяти — чужой или
/// неизвестный метод, и он форму открывает.
fn is_key_preserving_method(name: &hir_def::Name) -> bool {
    crate::method_lookup::is_platform_name(name, "Количество", "Count")
        || crate::method_lookup::is_platform_name(name, "Свойство", "Property")
}
