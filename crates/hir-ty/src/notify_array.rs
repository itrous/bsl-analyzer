//! Proof-oriented extraction of notification handlers from a locally-built argument array.
//!
//! Dynamic `Новый("Тип", МассивПараметров)` receives one array, not positional callback arguments.
//! This module only exposes handlers when a body-local array has a unique declaration, a known
//! `Массив` construction, and an unbroken straight-line sequence of platform `Добавить` calls up
//! to the exact constructor read. Any other use that could alias or mutate the array opens the
//! state. Type and constructor facts come from the platform snapshot, not syntax lowering.

use bsl_platform::PlatformDataInner;
use cfg_types::{ExprId, IdConversion};
use hir_def::body::events::{for_each_expr_child, for_each_stmt_expr_root};
use hir_def::body::Body;
use hir_def::call_graph::NotifyReceiverSyntax;
use hir_def::hir::{Expr, ExprIdx, Literal, Stmt, StmtIdx};
use hir_def::Name;
use rustc_hash::{FxHashMap, FxHashSet};
use stdx::case::{eq_ignore_case, CaseExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotifyArrayHandler {
    pub constructor: ExprId,
    pub callback_name: Name,
    pub callback_expr: ExprId,
    pub receiver: Option<ExprId>,
    pub receiver_syntax: NotifyReceiverSyntax,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotifyArrayRegistration {
    pub constructor: ExprId,
    pub handlers: Vec<NotifyArrayHandler>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArrayState {
    Uninitialized,
    Known(Vec<ExprId>),
    Unknown,
}

#[derive(Clone, Copy)]
pub(crate) struct CallbackSlots {
    pub(crate) handler: usize,
    pub(crate) receiver: usize,
    pub(crate) error_handler: usize,
    pub(crate) error_receiver: usize,
    arity: usize,
    min_arity: usize,
}

/// Collect dynamic notification constructors whose argument array is exact at the read point.
///
/// The current proof boundary is deliberately narrow: the array must be an explicitly declared
/// local, initialized by a zero-argument platform `Array` constructor, and extended only by direct
/// one-argument `Array.Add`/`Массив.Добавить` expression statements. Constructor reads must be the
/// direct right-hand side of an assignment. Aliases, other calls, reassignment, indexed/field
/// writes, and control-flow blocks invalidate the proof. These constraints trade recall for a
/// defensible receiver expression and parameter order.
pub(crate) fn collect_notify_array_registrations(
    body: &Body,
    platform: &PlatformDataInner,
) -> Vec<NotifyArrayRegistration> {
    if body.is_lowering_incomplete() {
        return Vec::new();
    }

    let local_declarations = unique_local_declarations(body);
    let mut states: FxHashMap<String, ArrayState> =
        local_declarations.keys().map(|name| (name.clone(), ArrayState::Uninitialized)).collect();
    let mut used_before_initialization = FxHashSet::default();
    let mut registrations = Vec::new();

    for (position, &stmt_idx) in body.body_stmts_typed().iter().enumerate() {
        let stmt = body.stmt_idx(stmt_idx);
        if is_control_flow(stmt) {
            let mentioned = stmt_path_names(body, stmt_idx);
            for name in mentioned {
                if matches!(states.get(&name), Some(ArrayState::Uninitialized)) {
                    used_before_initialization.insert(name);
                }
            }
            for state in states.values_mut() {
                if matches!(state, ArrayState::Known(_)) {
                    *state = ArrayState::Unknown;
                }
            }
            continue;
        }

        // Dynamic code can write any local without naming it in this statement's syntax tree.
        // Reaching definitions models `Выполнить` as a possible write to every local for this
        // reason; do not trust a tracked array after it.
        if matches!(stmt, Stmt::Execute { .. }) {
            for state in states.values_mut() {
                if matches!(state, ArrayState::Known(_)) {
                    *state = ArrayState::Unknown;
                }
            }
            continue;
        }

        if let Stmt::Assign { target, value } = stmt {
            if let Some(target_name) = path_name(body, *target) {
                let key = target_name.as_str().fold_lower();
                if let Some(&declaration_position) = local_declarations.get(&key) {
                    if is_empty_array_constructor(body, *value, platform) {
                        let state = if declaration_position < position
                            && !used_before_initialization.contains(&key)
                            && matches!(states.get(&key), Some(ArrayState::Uninitialized))
                        {
                            ArrayState::Known(Vec::new())
                        } else {
                            ArrayState::Unknown
                        };
                        states.insert(key, state);
                        continue;
                    }
                }

                if let Some((constructor, array_name, slots)) =
                    dynamic_notify_array_call(body, *value, platform)
                {
                    let array_key = array_name.as_str().fold_lower();
                    if let Some(ArrayState::Known(elements)) = states.get(&array_key) {
                        if accepts_argument_count(slots, elements.len()) {
                            let handlers =
                                handlers_from_elements(body, constructor, elements, slots);
                            if !handlers.is_empty() {
                                registrations
                                    .push(NotifyArrayRegistration { constructor, handlers });
                            }
                        }
                    }
                }

                if let Some(state) = states.get_mut(&key) {
                    *state = ArrayState::Unknown;
                }

                let mut mentioned = stmt_path_names(body, stmt_idx);
                mentioned.remove(&key);
                if let Some((_, array_name, _)) = dynamic_notify_array_call(body, *value, platform)
                {
                    mentioned.remove(&array_name.as_str().fold_lower());
                }
                invalidate_or_record_uses(&mut states, &mut used_before_initialization, mentioned);
                continue;
            }

            if let Some((constructor, array_name, slots)) =
                dynamic_notify_array_call(body, *value, platform)
            {
                let array_key = array_name.as_str().fold_lower();
                if let Some(ArrayState::Known(elements)) = states.get(&array_key) {
                    if accepts_argument_count(slots, elements.len()) {
                        let handlers = handlers_from_elements(body, constructor, elements, slots);
                        if !handlers.is_empty() {
                            registrations.push(NotifyArrayRegistration { constructor, handlers });
                        }
                    }
                }

                let mut mentioned = stmt_path_names(body, stmt_idx);
                mentioned.remove(&array_key);
                invalidate_or_record_uses(&mut states, &mut used_before_initialization, mentioned);
                continue;
            }
        }

        if let Stmt::Expr(expr) = stmt {
            if let Some((receiver_name, argument)) = array_add_statement(body, *expr, platform) {
                let key = receiver_name.as_str().fold_lower();
                let argument_names = expression_path_names(body, argument);
                let argument_escapes = argument_names.iter().any(|name| states.contains_key(name));
                if !argument_escapes {
                    if let Some(ArrayState::Known(elements)) = states.get_mut(&key) {
                        elements.push(ExprId::from_idx(argument));
                    } else if matches!(states.get(&key), Some(ArrayState::Uninitialized)) {
                        used_before_initialization.insert(key.clone());
                    }
                } else if let Some(state) = states.get_mut(&key) {
                    *state = ArrayState::Unknown;
                }

                invalidate_or_record_uses(
                    &mut states,
                    &mut used_before_initialization,
                    argument_names,
                );
                let mut other_names = stmt_path_names(body, stmt_idx);
                other_names.remove(&key);
                for name in expression_path_names(body, argument) {
                    other_names.remove(&name);
                }
                invalidate_or_record_uses(
                    &mut states,
                    &mut used_before_initialization,
                    other_names,
                );
                continue;
            }
        }

        invalidate_or_record_uses(
            &mut states,
            &mut used_before_initialization,
            stmt_path_names(body, stmt_idx),
        );
    }

    registrations
}

fn unique_local_declarations(body: &Body) -> FxHashMap<String, usize> {
    let mut binding_counts: FxHashMap<String, usize> = FxHashMap::default();
    for (_, binding) in body.bindings_iter() {
        *binding_counts.entry(binding.name.as_str().fold_lower()).or_default() += 1;
    }

    let parameters: FxHashSet<String> =
        body.params().map(|id| body.binding(id).name.as_str().fold_lower()).collect();
    let mut declarations: FxHashMap<String, Vec<usize>> = FxHashMap::default();
    for (position, &stmt_idx) in body.body_stmts_typed().iter().enumerate() {
        let Stmt::VarDecl { bindings } = body.stmt_idx(stmt_idx) else { continue };
        for &binding in bindings.iter() {
            let name = body.binding_idx(binding).name.as_str().fold_lower();
            declarations.entry(name).or_default().push(position);
        }
    }

    declarations
        .into_iter()
        .filter_map(|(name, positions)| {
            (positions.len() == 1
                && binding_counts.get(&name) == Some(&1)
                && !parameters.contains(&name))
            .then_some((name, positions[0]))
        })
        .collect()
}

fn is_empty_array_constructor(body: &Body, expr: ExprIdx, platform: &PlatformDataInner) -> bool {
    let Expr::New { type_name: Some(type_name), args } = body.expr_idx(expr) else {
        return false;
    };
    if !args.is_empty() || !resolves_to_type(platform, type_name.as_str(), "Array") {
        return false;
    }
    platform.get_constructors(type_name.as_str()).iter().any(|constructor| {
        constructor.parameters.iter().all(|param| param.is_optional || param.is_variadic)
    })
}

fn dynamic_notify_array_call(
    body: &Body,
    expr: ExprIdx,
    platform: &PlatformDataInner,
) -> Option<(ExprId, Name, CallbackSlots)> {
    let Expr::New { type_name: None, args } = body.expr_idx(expr) else { return None };
    if args.len() != 2 {
        return None;
    }
    let Expr::Literal(Literal::String(type_name)) = body.expr_idx(args[0]) else {
        return None;
    };
    if !platform.is_notify_description_type_name(type_name) {
        return None;
    }
    let Expr::Path(array_name) = body.expr_idx(args[1]) else { return None };
    let slots = callback_slots(platform, type_name)?;
    Some((ExprId::from_idx(expr), array_name.clone(), slots))
}

pub(crate) fn callback_slots(
    platform: &PlatformDataInner,
    type_name: &str,
) -> Option<CallbackSlots> {
    let constructors = platform.get_constructors(type_name);
    let [constructor] = constructors.as_slice() else { return None };
    let parameters = &constructor.parameters;
    if parameters.len() != 5 {
        return None;
    }

    let handler = unique_parameter_index(parameters, "ИмяПроцедуры")?;
    let receiver = unique_parameter_index(parameters, "Модуль")?;
    let error_handler = unique_parameter_index(parameters, "ИмяПроцедурыОбработкиОшибки")?;
    let error_receiver = unique_parameter_index(parameters, "МодульОбработкиОшибки")?;
    let extra_parameters = unique_parameter_index(parameters, "ДополнительныеПараметры")?;
    if extra_parameters != 2
        || handler != 0
        || receiver != 1
        || error_handler != 3
        || error_receiver != 4
    {
        return None;
    }

    Some(CallbackSlots {
        handler,
        receiver,
        error_handler,
        error_receiver,
        arity: parameters.len(),
        min_arity: parameters
            .iter()
            .enumerate()
            .filter(|(_, parameter)| !parameter.is_optional && !parameter.is_variadic)
            .map(|(index, _)| index + 1)
            .max()
            .unwrap_or(0),
    })
}

fn unique_parameter_index(
    parameters: &[bsl_platform::MethodParam],
    expected: &str,
) -> Option<usize> {
    let mut matches = parameters
        .iter()
        .enumerate()
        .filter(|(_, parameter)| eq_ignore_case(parameter.name.as_str(), expected));
    let index = matches.next()?.0;
    matches.next().is_none().then_some(index)
}

pub(crate) fn accepts_argument_count(slots: CallbackSlots, count: usize) -> bool {
    (slots.min_arity..=slots.arity).contains(&count)
}

fn handlers_from_elements(
    body: &Body,
    constructor: ExprId,
    elements: &[ExprId],
    slots: CallbackSlots,
) -> Vec<NotifyArrayHandler> {
    let mut handlers = Vec::with_capacity(2);
    if let Some(handler) =
        handler_from_elements(body, constructor, elements, slots.handler, slots.receiver)
    {
        handlers.push(handler);
    }
    if let Some(handler) = handler_from_elements(
        body,
        constructor,
        elements,
        slots.error_handler,
        slots.error_receiver,
    ) {
        handlers.push(handler);
    }
    handlers
}

fn handler_from_elements(
    body: &Body,
    constructor: ExprId,
    elements: &[ExprId],
    handler_index: usize,
    receiver_index: usize,
) -> Option<NotifyArrayHandler> {
    let callback_expr = *elements.get(handler_index)?;
    let Expr::Literal(Literal::String(callback_name)) = body.expr(callback_expr) else {
        return None;
    };
    let receiver = elements.get(receiver_index).copied();
    let receiver_syntax = match receiver.map(|expr| body.expr(expr)) {
        Some(Expr::Path(name)) => NotifyReceiverSyntax::for_bare_name(name),
        _ => NotifyReceiverSyntax::Other,
    };
    Some(NotifyArrayHandler {
        constructor,
        callback_name: Name::new(callback_name),
        callback_expr,
        receiver,
        receiver_syntax,
    })
}

fn array_add_statement(
    body: &Body,
    expr: ExprIdx,
    platform: &PlatformDataInner,
) -> Option<(Name, ExprIdx)> {
    let (receiver, method_name, args) = match body.expr_idx(expr) {
        Expr::MethodCall { receiver, method, args } => (*receiver, method.as_str(), args.as_ref()),
        Expr::Call { callee, args } => {
            let Expr::Field { base, field } = body.expr_idx(*callee) else { return None };
            (*base, field.as_str(), args.as_ref())
        }
        _ => return None,
    };
    let Expr::Path(receiver_name) = body.expr_idx(receiver) else { return None };
    if args.len() != 1 {
        return None;
    }
    let method = platform.get_method("Array", method_name)?;
    if !eq_ignore_case(method.english_name.as_str(), "Add")
        || method.parameters.len() != 1
        || !accepts_method_argument_count(method, args.len())
    {
        return None;
    }
    Some((receiver_name.clone(), args[0]))
}

fn accepts_method_argument_count(method: &bsl_platform::PlatformMethod, count: usize) -> bool {
    count <= method.parameters.len()
        && method
            .parameters
            .iter()
            .enumerate()
            .skip(count)
            .all(|(_, parameter)| parameter.is_optional || parameter.is_variadic)
}

fn resolves_to_type(platform: &PlatformDataInner, name: &str, canonical: &str) -> bool {
    if platform.is_ambiguous_type_name(name) {
        return false;
    }
    let Some(resolved) = platform.get_type(name) else { return false };
    let Some(expected) = platform.get_type(canonical) else { return false };
    std::ptr::eq(resolved, expected)
}

fn path_name(body: &Body, expr: ExprIdx) -> Option<&Name> {
    match body.expr_idx(expr) {
        Expr::Path(name) => Some(name),
        _ => None,
    }
}

fn is_control_flow(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::If(_)
            | Stmt::PreprocIf(_)
            | Stmt::While { .. }
            | Stmt::For { .. }
            | Stmt::ForEach { .. }
            | Stmt::Try { .. }
    )
}

fn invalidate_or_record_uses(
    states: &mut FxHashMap<String, ArrayState>,
    used_before_initialization: &mut FxHashSet<String>,
    mentioned: FxHashSet<String>,
) {
    for name in mentioned {
        match states.get_mut(&name) {
            Some(ArrayState::Uninitialized) => {
                used_before_initialization.insert(name);
            }
            Some(ArrayState::Known(_)) => {
                states.insert(name, ArrayState::Unknown);
            }
            Some(ArrayState::Unknown) | None => {}
        }
    }
}

fn stmt_path_names(body: &Body, stmt: StmtIdx) -> FxHashSet<String> {
    let mut names = FxHashSet::default();
    match body.stmt_idx(stmt) {
        Stmt::If(if_stmt) => {
            collect_expression_paths(body, if_stmt.condition, &mut names);
            for (_, branch) in if_stmt.elsif_branches.iter() {
                collect_statements_paths(body, branch, &mut names);
            }
            collect_statements_paths(body, &if_stmt.then_branch, &mut names);
            if let Some(branch) = &if_stmt.else_branch {
                collect_statements_paths(body, branch, &mut names);
            }
        }
        Stmt::PreprocIf(if_stmt) => {
            for branch in if_stmt.branches() {
                collect_statements_paths(body, branch.stmts, &mut names);
            }
        }
        Stmt::While { condition, body: branch } => {
            collect_expression_paths(body, *condition, &mut names);
            collect_statements_paths(body, branch, &mut names);
        }
        Stmt::For { from, to, body: branch, .. } => {
            collect_expression_paths(body, *from, &mut names);
            collect_expression_paths(body, *to, &mut names);
            collect_statements_paths(body, branch, &mut names);
        }
        Stmt::ForEach { collection, body: branch, .. } => {
            collect_expression_paths(body, *collection, &mut names);
            collect_statements_paths(body, branch, &mut names);
        }
        Stmt::Try { body: attempt, except } => {
            collect_statements_paths(body, attempt, &mut names);
            collect_statements_paths(body, except, &mut names);
        }
        _ => {
            for_each_stmt_expr_root(body, stmt, &mut |root| {
                collect_expression_paths(body, root, &mut names);
            });
        }
    }
    names
}

fn collect_statements_paths(body: &Body, statements: &[StmtIdx], names: &mut FxHashSet<String>) {
    for &stmt in statements {
        names.extend(stmt_path_names(body, stmt));
    }
}

fn expression_path_names(body: &Body, expr: ExprIdx) -> FxHashSet<String> {
    let mut names = FxHashSet::default();
    collect_expression_paths(body, expr, &mut names);
    names
}

fn collect_expression_paths(body: &Body, expr: ExprIdx, names: &mut FxHashSet<String>) {
    if let Expr::Path(name) = body.expr_idx(expr) {
        names.insert(name.as_str().fold_lower());
        return;
    }
    for_each_expr_child(body, expr, &mut |child| collect_expression_paths(body, child, names));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> (hir_def::ModuleBodies, Vec<NotifyArrayRegistration>) {
        let parse = parser::parse(source);
        let bodies = hir_def::ModuleBodies::from_parse(&parse);
        let body = bodies.iter_bodies().next().expect("the fixture declares a method").1;
        let registrations = collect_notify_array_registrations(body, PlatformDataInner::instance());
        (bodies, registrations)
    }

    fn registrations(source: &str) -> Vec<NotifyArrayRegistration> {
        parsed(source).1
    }

    #[test]
    fn exact_local_array_produces_a_callback_handler() {
        let (bodies, found) = parsed(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
КонецПроцедуры
"#,
        );

        let body = bodies.iter_bodies().next().expect("the fixture declares a method").1;
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].handlers.len(), 1, "{found:#?}");
        assert_eq!(found[0].handlers[0].callback_name.as_str(), "Готово");
        assert_eq!(found[0].handlers[0].receiver_syntax, NotifyReceiverSyntax::SelfName);
        let receiver = found[0].handlers[0].receiver.expect("receiver comes from the array");
        assert!(matches!(
            body.expr(receiver),
            Expr::Path(name) if name.as_str() == "ЭтотОбъект"
        ));
    }

    #[test]
    fn exact_array_uses_the_platform_fourth_and_fifth_constructor_parameters() {
        let found = registrations(
            r#"
Procedure Test()
    Перем Parameters;
    Перем Notification;
    Parameters = New Array;
    Parameters.Add("Done");
    Parameters.Add(ThisObject);
    Parameters.Add(Extra);
    Parameters.Add("Failed");
    Parameters.Add(ErrorModule);
    Notification = New("NotifyDescription", Parameters);
EndProcedure
"#,
        );

        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].handlers.len(), 2, "{found:#?}");
        assert_eq!(found[0].handlers[0].callback_name.as_str(), "Done");
        assert_eq!(found[0].handlers[0].receiver_syntax, NotifyReceiverSyntax::SelfName);
        assert_eq!(found[0].handlers[1].callback_name.as_str(), "Failed");
        assert_eq!(found[0].handlers[1].receiver_syntax, NotifyReceiverSyntax::BareName);
    }

    #[test]
    fn receiver_syntax_comes_from_the_value_added_to_the_array() {
        let found = registrations(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ПолучитьОбъект());
    Оповещение = Новый("ОписаниеОповещения", Параметры);
КонецПроцедуры
"#,
        );

        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].handlers[0].receiver_syntax, NotifyReceiverSyntax::Other);
    }

    #[test]
    fn later_mutation_does_not_change_the_array_at_the_constructor_read() {
        let found = registrations(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
    Параметры.Добавить("Позже");
КонецПроцедуры
"#,
        );

        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn aliases_reassignments_unknown_mutations_and_unknown_calls_open_the_array() {
        let cases = [
            (
                "array parameter is unknown",
                r#"Процедура Test(Параметры)
Параметры.Добавить("Готово");
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "array without a unique local declaration is unknown",
                r#"Процедура Test()
Параметры = Новый Массив;
Параметры.Добавить("Готово");
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "alias escapes the tracked identity",
                r#"Процедура Test()
Перем Параметры;
Перем Алиас;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Алиас = Параметры;
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "reassignment invalidates prior contents",
                r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Параметры = ПолучитьМассив();
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "unknown mutation invalidates prior contents",
                r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Параметры.Изменить();
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "passing the array to an unknown call may mutate it by reference",
                r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Изменить(Параметры);
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
            (
                "a possible write through a nested call invalidates the array",
                r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
Результат = Изменить(Параметры);
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            ),
        ];

        for (reason, source) in cases {
            assert!(registrations(source).is_empty(), "{reason}");
        }
    }

    #[test]
    fn execute_may_write_a_tracked_array_without_naming_it_syntactically() {
        let source = r#"
Процедура Test()
    Перем Параметры;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Выполнить("Параметры = Новый Массив");
    О = Новый("CallbackDescription", Параметры);
КонецПроцедуры
"#;

        assert!(registrations(source).is_empty());
    }

    #[test]
    fn an_earlier_registration_is_not_changed_by_later_execute() {
        let found = registrations(
            r#"
Процедура Test()
    Перем Параметры;
    Перем Оповещение;
    Параметры = Новый Массив;
    Параметры.Добавить("Готово");
    Параметры.Добавить(ЭтотОбъект);
    Оповещение = Новый("CallbackDescription", Параметры);
    Выполнить("Параметры.Добавить(1)");
КонецПроцедуры
"#,
        );

        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn conditional_loop_and_try_mutations_are_unknown() {
        let cases = [
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Если Истина Тогда
    Параметры.Добавить("Готово");
КонецЕсли;
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Пока Ложь Цикл
    Параметры.Добавить("Готово");
КонецЦикла;
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Попытка
    Параметры.Добавить("Готово");
Исключение
КонецПопытки;
О = Новый("CallbackDescription", Параметры);
КонецПроцедуры"#,
        ];
        for source in cases {
            assert!(registrations(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn no_notify_candidate_is_created_for_an_unresolved_type_or_extra_constructor_argument() {
        let cases = [
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
О = Новый("UnknownDescription", Параметры);
КонецПроцедуры"#,
            r#"Процедура Test()
Перем Параметры;
Параметры = Новый Массив;
Параметры.Добавить("Готово");
Параметры.Добавить(ЭтотОбъект);
О = Новый("CallbackDescription", Параметры, Лишний);
КонецПроцедуры"#,
        ];
        for source in cases {
            assert!(registrations(source).is_empty(), "{source}");
        }
    }
}
