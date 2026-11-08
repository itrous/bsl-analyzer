use super::context::LoweringContext;
use super::missing_row::{AliasMatch, MissingRowConditions, QueryFacts};
use crate::diagnostics::{SdblDiagnostic, UnprotectedFieldRef};
use crate::hir::{
    BinaryOp, ExprHir, FunctionKind, InValues, JoinHir, JoinType, Name, SdblHir, TableRef, UnaryOp,
    WhenClause,
};
use crate::standard_fields::{
    is_schedule_data_value_field, register_table_of_path, VirtualTableType,
};
use crate::types::{MdoRef, SdblType};
use bsl_metadata::MdoType;
use stdx::case::CaseExt;
use syntax::ast::AstNode;

impl LoweringContext<'_> {
    /// Reports fields of the optional side of an outer join that reach the query result
    /// without NULL handling.
    ///
    /// Rows of the preserved side that found no pair get NULL in every field of the
    /// optional side. NULL is neither zero nor an empty string: arithmetic with it yields
    /// NULL, so a selected value has to be replaced through `ЕСТЬNULL` or reached only where
    /// the missing row cannot arrive. A use in a condition only filters the row out and is
    /// not reported. The owner of a tabular section joined by its own ref always has a row
    /// while the section has one, so its own fields are not reported. One diagnostic per
    /// join lists every unprotected use, so the fix can be made in one place.
    pub(super) fn check_joins_for_unprotected_fields(
        &mut self,
        hir: &SdblHir,
        query: &syntax::ast::SdblQuery,
    ) {
        let has_optional_side = |join: &JoinHir| {
            matches!(join.join_type, JoinType::Left | JoinType::Right | JoinType::Full)
        };
        if !hir.joins.iter().any(has_optional_side) {
            return;
        }
        let facts = QueryFacts::of(query);
        let owners = owners_present_for_every_row(hir, query);

        for join in &hir.joins {
            // The preserved side of ПРАВОЕ and the other side of ПОЛНОЕ are the query's own
            // sources.
            let optional_sides: Vec<&TableRef> = match join.join_type {
                JoinType::Inner | JoinType::Cross => continue,
                JoinType::Left => vec![&join.table],
                JoinType::Right => hir.from.iter().collect(),
                JoinType::Full => std::iter::once(&join.table).chain(&hir.from).collect(),
            };

            let mut unprotected_fields = Vec::new();
            for side in optional_sides {
                let owner_present = owners.iter().any(|owner| std::ptr::eq(*owner, side));
                collect_missing_row_uses(side, &facts, owner_present, hir, &mut unprotected_fields);
            }

            if !unprotected_fields.is_empty() {
                self.diagnostics.push(SdblDiagnostic::FieldsFromJoinWithoutNullCheck {
                    join_type: join.join_type,
                    range: join.range,
                    unprotected_fields,
                });
            }
        }
    }

    /// Reports schedule values of a calculation register's `ДанныеГрафика` used without NULL
    /// handling.
    ///
    /// The table returns a row for every register record even when the schedule has no data
    /// for the period, and then its schedule values are NULL. A use already reported as an
    /// optional join side is left to the join check, so one use gets one message.
    pub(super) fn check_schedule_data_for_unprotected_fields(&mut self, hir: &SdblHir) {
        let reported_by_joins: Vec<_> = self
            .diagnostics
            .iter()
            .filter_map(|diag| match diag {
                SdblDiagnostic::FieldsFromJoinWithoutNullCheck { unprotected_fields, .. } => {
                    Some(unprotected_fields.iter().map(|field| field.range))
                }
                _ => None,
            })
            .flatten()
            .collect();

        let sources = hir.from.iter().chain(hir.joins.iter().map(|join| &join.table));
        for source in sources {
            if register_table_of_path(&source.parts)
                != Some((MdoType::CalculationRegister, VirtualTableType::ScheduleData))
            {
                continue;
            }

            let mut uses = Vec::new();
            NullableUses::new(source.effective_name(), NullSource::EmptyValue)
                .collect(hir, &mut uses);
            uses.retain(|field| {
                is_schedule_data_value_field(&field.field_name)
                    && !reported_by_joins.contains(&field.range)
            });

            if !uses.is_empty() {
                self.diagnostics.push(SdblDiagnostic::ScheduleDataFieldWithoutNullCheck {
                    range: source.range,
                    unprotected_fields: uses,
                });
            }
        }
    }

    /// Reports every dotted path that dereferences a field of a composite reference type.
    ///
    /// A step through a field of one reference type is an implicit join with that one table.
    /// A step through a field of a composite type joins every table the type may point to,
    /// which is what the platform cannot optimise away; `ВЫРАЗИТЬ` narrows it to one table.
    /// A field whose type the metadata does not establish is not reported. A path from a
    /// source alias carries its length so the IDE layer can apply the user's threshold.
    /// Inside the condition of a virtual table and after `ВЫРАЗИТЬ` the path starts from a
    /// field rather than from a source alias, so such paths carry no length.
    ///
    /// A nested query is checked by its own lowering, so it is not walked again here.
    pub(super) fn check_nested_fields_by_dot(&mut self, hir: &SdblHir) {
        for field in &hir.select.fields {
            self.report_dotted_paths(&field.expr, PathStart::SourceAlias);
        }
        for table in &hir.from {
            self.report_dotted_paths_in_source(table);
        }
        for join in &hir.joins {
            self.report_dotted_paths_in_source(&join.table);
            if let Some(condition) = &join.condition {
                self.report_dotted_paths(condition, PathStart::SourceAlias);
            }
        }
        if let Some(where_expr) = &hir.where_clause {
            self.report_dotted_paths(where_expr, PathStart::SourceAlias);
        }
        if let Some(group_by) = &hir.group_by {
            for expr in &group_by.exprs {
                self.report_dotted_paths(expr, PathStart::SourceAlias);
            }
        }
        if let Some(having) = &hir.having {
            self.report_dotted_paths(having, PathStart::SourceAlias);
        }
        if let Some(order_by) = &hir.order_by {
            for item in &order_by.items {
                self.report_dotted_paths(&item.expr, PathStart::SourceAlias);
            }
        }
    }

    fn report_dotted_paths_in_source(&mut self, table: &TableRef) {
        for param in &table.virtual_table_params {
            self.report_dotted_paths(param, PathStart::VirtualTableField(table));
        }
    }

    fn report_dotted_paths(&mut self, expr: &ExprHir, start: PathStart<'_>) {
        if let ExprHir::ColumnRef { parts, range, .. } = expr {
            let reported = match start {
                PathStart::SourceAlias => self
                    .source_path_dereferences_composite(parts)
                    .then_some(Some(parts.len() as u32)),
                PathStart::VirtualTableField(table) => {
                    self.virtual_table_path_dereferences_composite(table, parts).then_some(None)
                }
            };
            if let Some(parts_count) = reported {
                self.diagnostics
                    .push(SdblDiagnostic::QueryNestedFieldsByDot { range: *range, parts_count });
            }
            return;
        }
        for child in direct_operands(expr) {
            self.report_dotted_paths(child, start);
        }
        // `ВЫРАЗИТЬ(... КАК Справочник.Х).Поле` is the remedy the standard recommends: it
        // reads one field of the one table named in the cast. A further step dereferences
        // that field, which is reported only when its own type is a composite reference.
        if let ExprHir::FunctionCall {
            function: FunctionKind::Cast,
            member_access,
            cast_target: Some(target),
            range,
            ..
        } = expr
        {
            if self.cast_chain_dereferences_composite(target, member_access) {
                self.diagnostics.push(SdblDiagnostic::QueryNestedFieldsByDot {
                    range: *range,
                    parts_count: None,
                });
            }
        }
    }

    /// `Источник.Поле1.Поле2...`: some field followed by another member is a composite
    /// reference. The last name is read, not dereferenced.
    fn source_path_dereferences_composite(&self, parts: &[Name]) -> bool {
        if parts.len() < 3 || crate::is_mdo_type(parts[0].as_str()) {
            return false;
        }
        let alias = parts[0].as_str();
        if self.field_is_provisional(self.scope.find_table(alias), parts[1].as_str()) {
            return false;
        }
        let chain: Vec<String> = parts[1..].iter().map(|n| n.to_string()).collect();
        (1..chain.len()).any(|hops| {
            self.is_composite_reference(
                &self.scope.resolve_nested_field_type(alias, &chain[..hops]),
            )
        })
    }

    /// `(Поле1.Поле2...)` in a virtual table's parameter: the head is a field of that table.
    fn virtual_table_path_dereferences_composite(&self, table: &TableRef, parts: &[Name]) -> bool {
        if parts.len() < 2 || self.field_is_provisional(Some(table), parts[0].as_str()) {
            return false;
        }
        let chain: Vec<String> = parts.iter().map(|n| n.to_string()).collect();
        (1..chain.len()).any(|hops| {
            self.is_composite_reference(
                &self.scope.resolve_nested_field_type(table.effective_name(), &chain[..hops]),
            )
        })
    }

    /// `ВЫРАЗИТЬ(... КАК Вид.Объект).Поле1.Поле2...`: the members are fields of the cast's
    /// target, and the chain is reported when one of them is followed by another member
    /// and is itself a composite reference.
    fn cast_chain_dereferences_composite(&self, target: &MdoRef, members: &[Name]) -> bool {
        let Some((_, hops)) = members.split_last() else {
            return false;
        };
        let mut fields = self.scope.get_fields_for_ref(target);
        for member in hops {
            let Some(field) = fields.iter().find(|f| f.matches_name(member.as_str())) else {
                return false;
            };
            if self.is_composite_reference(&field.ty) {
                return true;
            }
            fields = match &field.ty {
                SdblType::Ref(mdo_ref) => self.scope.get_fields_for_ref(mdo_ref),
                SdblType::Composite { types } => self.scope.get_fields_for_composite(types),
                SdblType::DefinedType { name, underlying_type } => {
                    self.scope.get_fields_for_defined_type(name, underlying_type)
                }
                _ => return false,
            };
        }
        false
    }

    fn field_is_provisional(&self, table: Option<&TableRef>, field: &str) -> bool {
        table
            .and_then(|table| table.metadata.as_ref())
            .is_some_and(|metadata| metadata.column_is_provisional(field))
    }

    /// Whether a field of type `ty` is a composite reference: dereferencing it joins every
    /// table the type may point to. `AnyRef` and `AnyObjectRef` stand for "any reference"
    /// and "any reference of this kind", which the metadata records as composite. A single
    /// `Ref`, a composite of primitives and a type the metadata does not establish are not.
    fn is_composite_reference(&self, ty: &SdblType) -> bool {
        let (ty, may_resolve) = self.expanded_resolving(ty, true);
        match ty.as_ref() {
            SdblType::AnyRef | SdblType::AnyObjectRef { .. } => true,
            SdblType::Composite { types } => {
                types.len() >= 2 && types.iter().any(|arm| self.is_reference_arm(arm, may_resolve))
            }
            _ => false,
        }
    }

    fn is_reference_arm(&self, ty: &SdblType, may_resolve: bool) -> bool {
        let (ty, may_resolve) = self.expanded_resolving(ty, may_resolve);
        match ty.as_ref() {
            SdblType::Ref(_) | SdblType::AnyRef | SdblType::AnyObjectRef { .. } => true,
            SdblType::Composite { types } => {
                types.iter().any(|arm| self.is_reference_arm(arm, may_resolve))
            }
            _ => false,
        }
    }

    /// [`expanded`], with a defined type whose underlying type is unknown looked up once.
    ///
    /// A field reached through a reference keeps its defined type unresolved. The lookup
    /// resolves every defined type inside it, so what it leaves unresolved is a cycle or a
    /// missing type and is not looked up again: the returned flag says whether a lookup is
    /// still allowed below.
    fn expanded_resolving<'t>(
        &self,
        ty: &'t SdblType,
        may_resolve: bool,
    ) -> (std::borrow::Cow<'t, SdblType>, bool) {
        match expanded(ty) {
            SdblType::DefinedType { name, underlying_type: None } if may_resolve => {
                let resolved =
                    self.resolve_attribute_type(&bsl_metadata::AttributeType::DefinedType {
                        name: name.clone(),
                    });
                (std::borrow::Cow::Owned(expanded(&resolved).clone()), false)
            }
            ty => (std::borrow::Cow::Borrowed(ty), may_resolve),
        }
    }

    /// Reports selected fields whose result-column name is not given with `КАК`.
    ///
    /// The name of a result column is what the code reading the result relies on. Without
    /// an explicit alias the platform derives it from the expression, so renaming an
    /// attribute silently renames the column; an alias written without `КАК` is legal but
    /// easy to misread as part of the expression.
    pub(super) fn check_alias_without_as_keyword(&mut self, hir: &SdblHir) {
        for field in &hir.select.fields {
            // A field the parser could not finish is reported by the parser itself.
            if field.is_asterisk || field.has_parse_error {
                continue;
            }
            if field.alias.is_some() && field.has_as_keyword {
                continue;
            }
            self.diagnostics.push(SdblDiagnostic::AliasWithoutAsKeyword {
                field_name: field.alias.as_ref().map(|alias| alias.to_string()),
                raw_name: field.raw_name.as_ref().map(|name| name.to_string()),
                range: field.diagnostic_range,
            });
        }
    }

    pub(super) fn check_ref_overuse(&mut self, hir: &SdblHir) {
        self.check_ref_overuse_in_select(hir);
        self.check_ref_overuse_in_where(hir);
        self.check_ref_overuse_in_group_by(hir);
        self.check_ref_overuse_in_having(hir);
        self.check_ref_overuse_in_order_by(hir);
        self.check_ref_overuse_in_joins(hir);

        for union in &hir.unions {
            self.check_ref_overuse(&union.query);
        }
    }

    fn check_ref_overuse_in_select(&mut self, hir: &SdblHir) {
        for field in &hir.select.fields {
            self.check_expr_for_ref_overuse(&field.expr);
        }
    }

    fn check_ref_overuse_in_where(&mut self, hir: &SdblHir) {
        if let Some(ref where_expr) = hir.where_clause {
            self.check_expr_for_ref_overuse(where_expr);
        }
    }

    fn check_ref_overuse_in_group_by(&mut self, hir: &SdblHir) {
        if let Some(ref group_by) = hir.group_by {
            for expr in &group_by.exprs {
                self.check_expr_for_ref_overuse(expr);
            }
        }
    }

    fn check_ref_overuse_in_having(&mut self, hir: &SdblHir) {
        if let Some(ref having) = hir.having {
            self.check_expr_for_ref_overuse(having);
        }
    }

    fn check_ref_overuse_in_order_by(&mut self, hir: &SdblHir) {
        if let Some(ref order_by) = hir.order_by {
            for item in &order_by.items {
                self.check_expr_for_ref_overuse(&item.expr);
            }
        }
    }

    fn check_ref_overuse_in_joins(&mut self, hir: &SdblHir) {
        for join in &hir.joins {
            if let Some(ref cond) = join.condition {
                self.check_expr_for_ref_overuse(cond);
            }
        }
    }

    fn check_expr_for_ref_overuse(&mut self, expr: &ExprHir) {
        match expr {
            ExprHir::ColumnRef { parts, range, .. } => {
                self.check_column_ref_for_ref_overuse(parts, *range);
            }

            ExprHir::BinaryOp { lhs, rhs, .. } => {
                self.check_expr_for_ref_overuse(lhs);
                self.check_expr_for_ref_overuse(rhs);
            }

            ExprHir::UnaryOp { expr: inner, .. } => {
                self.check_expr_for_ref_overuse(inner);
            }

            ExprHir::FunctionCall { args, .. } => {
                for arg in args {
                    self.check_expr_for_ref_overuse(arg);
                }
            }

            ExprHir::Case { operand, when_clauses, else_expr, .. } => {
                if let Some(op) = operand {
                    self.check_expr_for_ref_overuse(op);
                }
                for clause in when_clauses {
                    self.check_expr_for_ref_overuse(&clause.condition);
                    self.check_expr_for_ref_overuse(&clause.result);
                }
                if let Some(else_e) = else_expr {
                    self.check_expr_for_ref_overuse(else_e);
                }
            }

            ExprHir::Subquery { query, .. } => {
                self.check_ref_overuse(query);
            }

            ExprHir::In { expr: inner, values, .. } => {
                self.check_expr_for_ref_overuse(inner);
                match values {
                    crate::hir::InValues::List(items) => {
                        for item in items {
                            self.check_expr_for_ref_overuse(item);
                        }
                    }
                    crate::hir::InValues::Subquery(sq) => {
                        self.check_ref_overuse(sq);
                    }
                }
            }

            ExprHir::Between { expr: inner, low, high, .. } => {
                self.check_expr_for_ref_overuse(inner);
                self.check_expr_for_ref_overuse(low);
                self.check_expr_for_ref_overuse(high);
            }

            ExprHir::Like { expr: inner, pattern, escape, .. } => {
                self.check_expr_for_ref_overuse(inner);
                self.check_expr_for_ref_overuse(pattern);
                if let Some(esc) = escape {
                    self.check_expr_for_ref_overuse(esc);
                }
            }

            ExprHir::IsNull { expr: inner, .. } => {
                self.check_expr_for_ref_overuse(inner);
            }

            ExprHir::Tuple { elements, .. } => {
                for elem in elements {
                    self.check_expr_for_ref_overuse(elem);
                }
            }

            ExprHir::Literal { .. } | ExprHir::Parameter { .. } | ExprHir::Missing { .. } => {}
        }
    }

    fn check_column_ref_for_ref_overuse(
        &mut self,
        parts: &[crate::hir::Name],
        range: text_size::TextRange,
    ) {
        if parts.len() < 2 {
            return;
        }

        for ref_idx in 0..parts.len() {
            let p_lower = parts[ref_idx].fold_lower();
            if p_lower != "ссылка" && p_lower != "reference" {
                continue;
            }

            if ref_idx == 0 {
                continue;
            }

            if ref_idx == 1 {
                continue;
            }

            let (alias, chain_start) = if crate::is_mdo_type(parts[0].as_str()) {
                if parts.len() < 3 {
                    continue;
                }
                (parts[1].as_str(), 2usize)
            } else {
                (parts[0].as_str(), 1usize)
            };

            if chain_start >= ref_idx {
                continue;
            }

            let chain: Vec<String> =
                parts[chain_start..ref_idx].iter().map(|n| n.to_string()).collect();

            let field_type = self.scope.resolve_nested_field_type(alias, &chain);

            if field_type.is_ref() {
                self.diagnostics.push(SdblDiagnostic::RefOveruse { range });
                return;
            }
        }
    }

    pub(super) fn check_unlimited_string_usage(&mut self, hir: &SdblHir) {
        use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

        for field in &hir.select.fields {
            if hir.select.distinct {
                self.flag_unlimited_string(&field.expr, Ctx::Distinct);
            }
            self.check_expr_for_unlimited_string(&field.expr);
        }

        if let Some(ref where_expr) = hir.where_clause {
            self.check_expr_for_unlimited_string(where_expr);
        }

        if let Some(ref group_by) = hir.group_by {
            for expr in &group_by.exprs {
                self.flag_unlimited_string(expr, Ctx::GroupBy);
                self.check_expr_for_unlimited_string(expr);
            }
        }

        if let Some(ref having) = hir.having {
            self.check_expr_for_unlimited_string(having);
        }

        if let Some(ref order_by) = hir.order_by {
            for item in &order_by.items {
                self.flag_unlimited_string(&item.expr, Ctx::OrderBy);
                self.check_expr_for_unlimited_string(&item.expr);
            }
        }

        for join in &hir.joins {
            if let Some(ref cond) = join.condition {
                self.check_expr_for_unlimited_string(cond);
            }
        }

        for union in &hir.unions {
            self.check_unlimited_string_usage(&union.query);
        }
    }

    fn check_expr_for_unlimited_string(&mut self, expr: &ExprHir) {
        use crate::diagnostics::UnlimitedStringUsageContext as Ctx;

        match expr {
            ExprHir::BinaryOp { lhs, op, rhs, .. } => {
                if op.is_comparison() {
                    self.flag_unlimited_string(lhs, Ctx::Comparison);
                    self.flag_unlimited_string(rhs, Ctx::Comparison);
                }
                self.check_expr_for_unlimited_string(lhs);
                self.check_expr_for_unlimited_string(rhs);
            }

            ExprHir::UnaryOp { expr: inner, .. } => {
                self.check_expr_for_unlimited_string(inner);
            }

            ExprHir::FunctionCall { args, .. } => {
                for arg in args {
                    self.check_expr_for_unlimited_string(arg);
                }
            }

            ExprHir::Case { operand, when_clauses, else_expr, .. } => {
                if let Some(op) = operand {
                    self.check_expr_for_unlimited_string(op);
                }
                for clause in when_clauses {
                    self.check_expr_for_unlimited_string(&clause.condition);
                    self.check_expr_for_unlimited_string(&clause.result);
                }
                if let Some(else_e) = else_expr {
                    self.check_expr_for_unlimited_string(else_e);
                }
            }

            // Вложенные запросы проходят через lower_query и проверяются при
            // построении собственного SdblHir — повторный обход даёт дубликаты.
            ExprHir::Subquery { .. } => {}

            ExprHir::In { expr: inner, values, .. } => {
                match inner.as_ref() {
                    ExprHir::Tuple { elements, .. } => {
                        for elem in elements {
                            self.flag_unlimited_string(elem, Ctx::In);
                        }
                    }
                    _ => self.flag_unlimited_string(inner, Ctx::In),
                }
                self.check_expr_for_unlimited_string(inner);
                if let crate::hir::InValues::List(items) = values {
                    for item in items {
                        self.flag_unlimited_string(item, Ctx::In);
                        self.check_expr_for_unlimited_string(item);
                    }
                }
            }

            ExprHir::Between { expr: inner, low, high, .. } => {
                self.flag_unlimited_string(inner, Ctx::Between);
                self.flag_unlimited_string(low, Ctx::Between);
                self.flag_unlimited_string(high, Ctx::Between);
                self.check_expr_for_unlimited_string(inner);
                self.check_expr_for_unlimited_string(low);
                self.check_expr_for_unlimited_string(high);
            }

            // ПОДОБНО платформа разрешает для полей неограниченной длины.
            ExprHir::Like { expr: inner, pattern, escape, .. } => {
                self.check_expr_for_unlimited_string(inner);
                self.check_expr_for_unlimited_string(pattern);
                if let Some(esc) = escape {
                    self.check_expr_for_unlimited_string(esc);
                }
            }

            ExprHir::IsNull { expr: inner, .. } => {
                self.check_expr_for_unlimited_string(inner);
            }

            ExprHir::Tuple { elements, .. } => {
                for elem in elements {
                    self.check_expr_for_unlimited_string(elem);
                }
            }

            ExprHir::ColumnRef { .. }
            | ExprHir::Literal { .. }
            | ExprHir::Parameter { .. }
            | ExprHir::Missing { .. } => {}
        }
    }

    pub(super) fn flag_unlimited_string(
        &mut self,
        expr: &ExprHir,
        context: crate::diagnostics::UnlimitedStringUsageContext,
    ) {
        if !expr.ty().is_unlimited_string() {
            return;
        }

        let field_name = match expr {
            ExprHir::ColumnRef { parts, .. } => {
                Some(parts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("."))
            }
            _ => None,
        };

        self.diagnostics.push(SdblDiagnostic::UnlimitedStringUsage {
            field_name,
            context,
            range: expr.range(),
        });
    }
}

#[derive(Clone, Copy)]
enum PathStart<'t> {
    SourceAlias,
    /// A parameter of this virtual table: the path's head is one of its fields.
    VirtualTableField(&'t TableRef),
}

/// `ty` without the wrappers that do not change what it may hold: a defined type whose
/// underlying type is known and an aggregate.
pub(super) fn expanded(ty: &SdblType) -> &SdblType {
    match ty {
        SdblType::DefinedType { underlying_type: Some(inner), .. } | SdblType::Aggregate(inner) => {
            expanded(inner)
        }
        _ => ty,
    }
}

/// The operands an expression evaluates directly. A nested query in `В (...)` is not an
/// operand: it is a query of its own and every check decides separately how to treat it.
fn direct_operands(expr: &ExprHir) -> Vec<&ExprHir> {
    match expr {
        ExprHir::BinaryOp { lhs, rhs, .. } => vec![lhs, rhs],
        ExprHir::UnaryOp { expr, .. } | ExprHir::IsNull { expr, .. } => vec![expr],
        ExprHir::FunctionCall { args, .. } => args.iter().collect(),
        ExprHir::Case { operand, when_clauses, else_expr, .. } => {
            let mut operands: Vec<&ExprHir> = operand.iter().map(|op| &**op).collect();
            for clause in when_clauses {
                operands.push(&clause.condition);
                operands.push(&clause.result);
            }
            operands.extend(else_expr.iter().map(|e| &**e));
            operands
        }
        ExprHir::In { expr, values, .. } => {
            let mut operands = vec![&**expr];
            if let InValues::List(items) = values {
                operands.extend(items);
            }
            operands
        }
        ExprHir::Between { expr, low, high, .. } => vec![expr, low, high],
        ExprHir::Like { expr, pattern, escape, .. } => {
            let mut operands = vec![&**expr, &**pattern];
            operands.extend(escape.iter().map(|e| &**e));
            operands
        }
        ExprHir::Tuple { elements, .. } => elements.iter().collect(),
        ExprHir::ColumnRef { .. }
        | ExprHir::Literal { .. }
        | ExprHir::Subquery { .. }
        | ExprHir::Parameter { .. }
        | ExprHir::Missing { .. } => Vec::new(),
    }
}

/// One join with the sources of its two operands: everything joined so far on the left, the
/// joined source with the joins nested in it on the right.
struct JoinOperands<'h> {
    join: &'h JoinHir,
    left: Vec<&'h TableRef>,
    right: Vec<&'h TableRef>,
}

impl JoinOperands<'_> {
    fn makes_optional(&self, source: &TableRef) -> bool {
        let contains = |sources: &[&TableRef]| sources.iter().any(|s| std::ptr::eq(*s, source));
        match self.join.join_type {
            JoinType::Inner | JoinType::Cross => false,
            JoinType::Left => contains(&self.right),
            JoinType::Right => contains(&self.left),
            JoinType::Full => contains(&self.left) || contains(&self.right),
        }
    }
}

/// The sources whose row is present whenever the row of their tabular section is.
///
/// Every row of a tabular section belongs to an existing owner, so a join by nothing but the
/// equality of their refs always finds the owner. The proof needs the owner to be a whole
/// operand of that join, since a join nested with it could filter it out, and needs the
/// section not to be optional because of any other join. Any other conjunct of the condition
/// may filter the owner out as well, so it voids the proof. The flat join list does not say
/// which side a join preserves, hence the walk over the syntax; a join that cannot be matched
/// to its HIR leaves the proof out.
fn owners_present_for_every_row<'h>(
    hir: &'h SdblHir,
    query: &syntax::ast::SdblQuery,
) -> Vec<&'h TableRef> {
    // Joins of a later `ИЗ` source are not lowered and cannot touch the rows of the first one.
    let first_source = query.from_clause().and_then(|from| from.data_sources().next());
    let (Some(first_source), Some(base)) = (first_source, hir.from.first()) else {
        return Vec::new();
    };
    let mut joins = Vec::new();
    if collect_join_operands(&first_source, base, hir, &mut joins).is_none() {
        return Vec::new();
    }

    let mut owners = Vec::new();
    for (index, operands) in joins.iter().enumerate() {
        let Some(condition) = operands.join.condition.as_ref() else {
            continue;
        };
        for (owner_side, section_side) in
            [(&operands.left, &operands.right), (&operands.right, &operands.left)]
        {
            let [owner] = owner_side.as_slice() else {
                continue;
            };
            let proven = section_side.iter().any(|section| {
                owns_section(owner, section)
                    && joins_by_refs(condition, section.effective_name(), owner.effective_name())
                    && !joins
                        .iter()
                        .enumerate()
                        .any(|(other, operands)| other != index && operands.makes_optional(section))
            });
            if proven {
                owners.push(*owner);
            }
        }
    }
    owners
}

/// Records the joins of `source`, nested ones first, and returns all the sources under it.
/// `None` when a join has no HIR or no source of its own.
fn collect_join_operands<'h>(
    source: &syntax::ast::SdblDataSource,
    table: &'h TableRef,
    hir: &'h SdblHir,
    out: &mut Vec<JoinOperands<'h>>,
) -> Option<Vec<&'h TableRef>> {
    if !source.syntax().text_range().contains_range(table.range) {
        return None;
    }
    let mut joined = vec![table];
    for clause in source.join_clauses() {
        let range = clause.syntax().text_range();
        let join = hir.joins.iter().find(|join| join.range == range)?;
        let right = collect_join_operands(&clause.data_source()?, &join.table, hir, out)?;
        out.push(JoinOperands { join, left: joined.clone(), right: right.clone() });
        joined.extend(right);
    }
    Some(joined)
}

/// `Документ.X` for `Документ.X.Секция`, and the same for a catalog, in either language.
fn owns_section(owner: &TableRef, section: &TableRef) -> bool {
    let ([owner_kind, owner_name], [section_kind, section_name, _]) =
        (owner.parts.as_slice(), section.parts.as_slice())
    else {
        return false;
    };
    let kind = |part: &Name| part.as_str().parse::<MdoType>().ok();
    matches!(kind(owner_kind), Some(MdoType::Document | MdoType::Catalog))
        && kind(owner_kind) == kind(section_kind)
        && stdx::case::eq_ignore_case(owner_name.as_str(), section_name.as_str())
}

/// `Секция.Ссылка = Владелец.Ссылка` in either order, alone or repeated in a conjunction.
fn joins_by_refs(condition: &ExprHir, section: &str, owner: &str) -> bool {
    match condition {
        ExprHir::BinaryOp { lhs, op: BinaryOp::And, rhs, .. } => {
            joins_by_refs(lhs, section, owner) && joins_by_refs(rhs, section, owner)
        }
        ExprHir::BinaryOp { lhs, op: BinaryOp::Eq, rhs, .. } => {
            (is_ref_of(lhs, section) && is_ref_of(rhs, owner))
                || (is_ref_of(lhs, owner) && is_ref_of(rhs, section))
        }
        _ => false,
    }
}

fn is_ref_of(expr: &ExprHir, source: &str) -> bool {
    let ExprHir::ColumnRef { parts, .. } = expr else {
        return false;
    };
    // The alias is matched as the uses of a side are; see `NullableUses::is_side_field`.
    matches!(parts.as_slice(), [alias, field]
        if alias.as_str().eq_ignore_ascii_case(source)
            && (stdx::case::eq_ignore_case(field.as_str(), "Ссылка")
                || stdx::case::eq_ignore_case(field.as_str(), "Ref")))
}

/// The uses of an optional side that may observe the NULL of its missing row.
///
/// A schedule value is NULL in a present row too, and new proofs of the row must not change
/// how it is treated, so it keeps its own patterns.
fn collect_missing_row_uses(
    side: &TableRef,
    facts: &QueryFacts,
    owner_present: bool,
    hir: &SdblHir,
    out: &mut Vec<UnprotectedFieldRef>,
) {
    let name = side.effective_name();
    let row = MissingRow { side, facts, owner_present };
    let mut uses = Vec::new();
    NullableUses::new(name, NullSource::MissingRow(row)).collect(hir, &mut uses);

    if register_table_of_path(&side.parts)
        == Some((MdoType::CalculationRegister, VirtualTableType::ScheduleData))
    {
        uses.retain(|field| !is_schedule_data_value_field(&field.field_name));
        let mut values = Vec::new();
        NullableUses::new(name, NullSource::ScheduleRow).collect(hir, &mut values);
        values.retain(|field| is_schedule_data_value_field(&field.field_name));
        uses.append(&mut values);
        uses.sort_by_key(|field| field.range.start());
    }
    out.append(&mut uses);
}

#[derive(Clone, Copy)]
struct MissingRow<'a> {
    side: &'a TableRef,
    facts: &'a QueryFacts,
    owner_present: bool,
}

impl<'a> MissingRow<'a> {
    fn conditions(&self, aliases: AliasMatch) -> MissingRowConditions<'a> {
        MissingRowConditions { side: self.side, facts: self.facts, aliases }
    }
}

/// Why a side's field may be NULL, which decides where NULL can do harm and what proves the
/// field is not NULL.
#[derive(Clone, Copy)]
enum NullSource<'a> {
    /// The row of an optional join side is missing. A condition on its field only filters
    /// the row out, so only the selected values are uses; whatever a condition evaluates to
    /// without the row decides which of them the row can reach.
    MissingRow(MissingRow<'a>),
    /// The row of the `ДанныеГрафика` side is missing and the use is a schedule value: only
    /// the explicit patterns below prove the row.
    ScheduleRow,
    /// The value of a present row is empty, as a schedule value of `ДанныеГрафика` is. A
    /// condition on it is a use too, and the row's other fields say nothing about it.
    EmptyValue,
}

/// What keeps NULL away from an expression. Ordered by how much it hides.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Cover {
    Nothing,
    /// The row of the side is present. A field read through a reference may still be NULL,
    /// so only the side's direct fields are safe.
    DirectFields,
    AllFields,
}

/// Uses of one side that may observe its NULL.
///
/// A use is safe when NULL cannot reach it: inside `ЕСТЬNULL`, in a `ВЫБОР` branch that
/// the missing row cannot reach, next to an `ЕСТЬ NULL` test in a disjunction, or in a query
/// whose selection condition excludes the missing row. The null test itself is not a use.
struct NullableUses<'a> {
    side: &'a str,
    source: NullSource<'a>,
}

impl<'a> NullableUses<'a> {
    fn new(side: &'a str, source: NullSource<'a>) -> Self {
        Self { side, source }
    }

    fn collect(&self, hir: &SdblHir, out: &mut Vec<UnprotectedFieldRef>) {
        let cover = self.cover_of_every_use(hir);

        for field in &hir.select.fields {
            self.walk(&field.expr, cover, out);
        }
        if matches!(self.source, NullSource::EmptyValue) {
            if let Some(condition) = &hir.where_clause {
                self.walk(condition, cover, out);
            }
        }
    }

    fn cover_of_every_use(&self, hir: &SdblHir) -> Cover {
        let filtered = hir.where_clause.as_ref();
        match self.source {
            // A conjunction lets the row through only if every conjunct does, so evaluating
            // the whole condition asks whether any top-level conjunct excludes it.
            NullSource::MissingRow(row) => {
                let excluded = filtered.is_some_and(|condition| {
                    !row.conditions(AliasMatch::Exact).of(condition).may_be_true()
                });
                if row.owner_present || excluded {
                    Cover::DirectFields
                } else {
                    Cover::Nothing
                }
            }
            NullSource::ScheduleRow | NullSource::EmptyValue => {
                if filtered.is_some_and(|condition| self.tests_presence_anywhere(condition)) {
                    Cover::AllFields
                } else {
                    Cover::Nothing
                }
            }
        }
    }

    fn walk(&self, expr: &ExprHir, cover: Cover, out: &mut Vec<UnprotectedFieldRef>) {
        match expr {
            ExprHir::ColumnRef { parts, range, .. } => {
                let hidden = match cover {
                    Cover::Nothing => false,
                    Cover::DirectFields => self.is_direct_field(parts, *range),
                    Cover::AllFields => true,
                };
                if !hidden && self.is_side_field(parts) {
                    out.push(UnprotectedFieldRef {
                        table_alias: parts[0].to_string(),
                        field_name: parts[1].to_string(),
                        range: *range,
                    });
                }
            }
            ExprHir::IsNull { .. } => {}
            ExprHir::FunctionCall { function: FunctionKind::Isnull, .. } => {}
            // `ВЫРАЗИТЬ(Поле КАК ...).Реквизит` reads through the field, as a dotted path does.
            ExprHir::FunctionCall { member_access, .. } if !member_access.is_empty() => {
                let cover = if cover == Cover::AllFields { cover } else { Cover::Nothing };
                for operand in direct_operands(expr) {
                    self.walk(operand, cover, out);
                }
            }
            ExprHir::Case { operand, when_clauses, else_expr, .. } => {
                let operand = operand.as_deref();
                let else_expr = else_expr.as_deref();
                if let NullSource::MissingRow(row) = self.source {
                    let conditions = row.conditions(AliasMatch::AsciiFold);
                    self.walk_reachable_branches(
                        conditions,
                        operand,
                        when_clauses,
                        else_expr,
                        cover,
                        out,
                    );
                } else {
                    self.walk_branches_by_patterns(operand, when_clauses, else_expr, cover, out);
                }
            }
            ExprHir::BinaryOp { lhs, op: BinaryOp::Or, rhs, .. } => {
                let guarded = if self.has_null_test(lhs) || self.has_null_test(rhs) {
                    Cover::AllFields
                } else {
                    cover
                };
                self.walk(lhs, guarded, out);
                self.walk(rhs, guarded, out);
            }
            ExprHir::In { values: InValues::Subquery(nested), .. } => {
                for operand in direct_operands(expr) {
                    self.walk(operand, cover, out);
                }
                for field in &nested.select.fields {
                    self.walk(&field.expr, cover, out);
                }
            }
            _ => {
                for operand in direct_operands(expr) {
                    self.walk(operand, cover, out);
                }
            }
        }
    }

    /// A branch whose condition cannot be true without the row is safe; so is everything
    /// after a condition that is surely true without it.
    fn walk_reachable_branches(
        &self,
        conditions: MissingRowConditions<'_>,
        operand: Option<&ExprHir>,
        when_clauses: &[WhenClause],
        else_expr: Option<&ExprHir>,
        cover: Cover,
        out: &mut Vec<UnprotectedFieldRef>,
    ) {
        let mut cover = cover;
        for clause in when_clauses {
            let outcomes = conditions.of_when(operand, &clause.condition);
            let branch_cover =
                if outcomes.may_be_true() { cover } else { cover.max(Cover::DirectFields) };
            self.walk(&clause.result, branch_cover, out);
            if outcomes.is_always_true() {
                cover = cover.max(Cover::DirectFields);
            }
        }
        if let Some(else_expr) = else_expr {
            self.walk(else_expr, cover, out);
        }
    }

    fn walk_branches_by_patterns(
        &self,
        operand: Option<&ExprHir>,
        when_clauses: &[WhenClause],
        else_expr: Option<&ExprHir>,
        cover: Cover,
        out: &mut Vec<UnprotectedFieldRef>,
    ) {
        let conditions_are_uses = matches!(self.source, NullSource::EmptyValue);
        // The operand of a simple `ВЫБОР` is compared implicitly; that equality is not taken
        // as a proof.
        let comparison_proves_row =
            matches!(self.source, NullSource::ScheduleRow) && operand.is_none();
        if conditions_are_uses {
            if let Some(operand) = operand {
                self.walk(operand, cover, out);
            }
        }
        let mut present = cover;
        for clause in when_clauses {
            if conditions_are_uses {
                self.walk(&clause.condition, present, out);
            }
            let proven = self.proves_presence_when_true(&clause.condition, comparison_proves_row);
            let branch_present = if proven { Cover::AllFields } else { present };
            self.walk(&clause.result, branch_present, out);
            if self.proves_presence_when_false(&clause.condition) {
                present = Cover::AllFields;
            }
        }
        if let Some(else_expr) = else_expr {
            self.walk(else_expr, present, out);
        }
    }

    fn is_side_field(&self, parts: &[Name]) -> bool {
        // ASCII-only folding keeps the long-standing behaviour for Latin aliases; Cyrillic
        // aliases are matched as spelled.
        parts.len() >= 2 && parts[0].as_str().eq_ignore_ascii_case(self.side)
    }

    /// A field of the side itself rather than one read through a reference.
    fn is_direct_field(&self, parts: &[Name], range: text_size::TextRange) -> bool {
        parts.len() == 2
            && match self.source {
                NullSource::MissingRow(row) => !row.facts.is_dereferenced(range),
                NullSource::ScheduleRow | NullSource::EmptyValue => true,
            }
    }

    fn is_null_test(&self, expr: &ExprHir, negated_test: bool) -> bool {
        matches!(
            expr,
            ExprHir::IsNull { expr: tested, negated, .. }
                if *negated == negated_test
                    && matches!(&**tested, ExprHir::ColumnRef { parts, .. } if self.is_side_field(parts))
        )
    }

    /// `ЕСТЬ НЕ NULL`, `НЕ ... ЕСТЬ NULL`, `ЕСТЬNULL(Поле, З) <> З`, with
    /// `comparison_proves_row` a comparison of a field of the side itself, or a conjunction
    /// containing one of them.
    ///
    /// A comparison with NULL is never true, so a true comparison of the field proves the
    /// row; a function or an expression around the field may turn NULL into a value first.
    fn proves_presence_when_true(&self, condition: &ExprHir, comparison_proves_row: bool) -> bool {
        match condition {
            ExprHir::BinaryOp { lhs, op: BinaryOp::And, rhs, .. } => {
                self.proves_presence_when_true(lhs, comparison_proves_row)
                    || self.proves_presence_when_true(rhs, comparison_proves_row)
            }
            ExprHir::UnaryOp { op: UnaryOp::Not, expr, .. } => self.is_null_test(expr, false),
            ExprHir::BinaryOp { lhs, op, rhs, .. } if op.is_comparison() => {
                (comparison_proves_row && (self.is_side_column(lhs) || self.is_side_column(rhs)))
                    || (*op == BinaryOp::Ne
                        && (self.differs_from_its_replacement(lhs, rhs)
                            || self.differs_from_its_replacement(rhs, lhs)))
            }
            _ => self.is_null_test(condition, true),
        }
    }

    fn is_side_column(&self, expr: &ExprHir) -> bool {
        matches!(expr, ExprHir::ColumnRef { parts, .. } if self.is_side_field(parts))
    }

    /// `ЕСТЬNULL(Поле, З) <> З`: the function returns the replacement exactly when the
    /// field is NULL, so the inequality holds only for a present field.
    fn differs_from_its_replacement(&self, replaced: &ExprHir, compared: &ExprHir) -> bool {
        let ExprHir::FunctionCall { function: FunctionKind::Isnull, args, .. } = replaced else {
            return false;
        };
        let [ExprHir::ColumnRef { parts, .. }, ExprHir::Literal { value: replacement, .. }] =
            args.as_slice()
        else {
            return false;
        };
        matches!(compared, ExprHir::Literal { value, .. } if value == replacement)
            && self.is_side_field(parts)
    }

    /// `ЕСТЬ NULL`, or a disjunction containing it: when it is false, the side is present.
    fn proves_presence_when_false(&self, condition: &ExprHir) -> bool {
        match condition {
            ExprHir::BinaryOp { lhs, op: BinaryOp::Or, rhs, .. } => {
                self.proves_presence_when_false(lhs) || self.proves_presence_when_false(rhs)
            }
            _ => self.is_null_test(condition, false),
        }
    }

    /// Only a test that holds for the missing side guards its disjunction: the other
    /// operand is evaluated when the test is false, that is, when the side is present.
    /// After `ЕСТЬ НЕ NULL` the other operand is evaluated exactly for the missing side,
    /// and inside a conjunction the test no longer decides the disjunct.
    fn has_null_test(&self, expr: &ExprHir) -> bool {
        match expr {
            ExprHir::BinaryOp { lhs, op: BinaryOp::Or, rhs, .. } => {
                self.has_null_test(lhs) || self.has_null_test(rhs)
            }
            ExprHir::UnaryOp { op: UnaryOp::Not, expr, .. } => self.is_null_test(expr, true),
            _ => self.is_null_test(expr, false),
        }
    }

    /// The selection condition's presence test matches the alias exactly as spelled,
    /// unlike the uses it protects.
    fn is_exact_null_test(&self, expr: &ExprHir, negated_test: bool) -> bool {
        matches!(
            expr,
            ExprHir::IsNull { expr: tested, negated, .. }
                if *negated == negated_test
                    && matches!(&**tested, ExprHir::ColumnRef { parts, .. }
                        if parts.len() >= 2 && parts[0].as_str() == self.side)
        )
    }

    fn tests_presence_anywhere(&self, expr: &ExprHir) -> bool {
        let negated_positive_test = matches!(
            expr,
            ExprHir::UnaryOp { op: UnaryOp::Not, expr: inner, .. }
                if self.is_exact_null_test(inner, false)
        );
        negated_positive_test
            || self.is_exact_null_test(expr, true)
            || direct_operands(expr)
                .into_iter()
                .any(|operand| self.tests_presence_anywhere(operand))
    }
}
