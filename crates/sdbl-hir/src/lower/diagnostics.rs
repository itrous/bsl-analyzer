use super::context::LoweringContext;
use crate::diagnostics::{SdblDiagnostic, UnprotectedFieldRef};
use crate::hir::{
    BinaryOp, ExprHir, FunctionKind, InValues, JoinType, Name, SdblHir, TableRef, UnaryOp,
};
use crate::standard_fields::{
    is_schedule_data_value_field, register_table_of_path, VirtualTableType,
};
use bsl_metadata::MdoType;
use stdx::case::CaseExt;

impl LoweringContext<'_> {
    /// Reports fields of the optional side of an outer join that are used without NULL
    /// handling.
    ///
    /// Rows of the preserved side that found no pair get NULL in every field of the
    /// optional side. NULL is neither zero nor an empty string: arithmetic and comparisons
    /// with it yield NULL, so a field used as a value has to be replaced through `ЕСТЬNULL`
    /// or reached only after an `ЕСТЬ NULL` test. One diagnostic per join lists every
    /// unprotected use, so the fix can be made in one place.
    pub(super) fn check_joins_for_unprotected_fields(&mut self, hir: &SdblHir) {
        // The preserved side of ПРАВОЕ and the other side of ПОЛНОЕ are the query's own
        // sources; a source without an alias is referred to by its table name.
        let sources = hir.from.iter().map(TableRef::effective_name);

        for join in &hir.joins {
            let joined = join.table.effective_name();
            let optional_sides: Vec<&str> = match join.join_type {
                JoinType::Inner | JoinType::Cross => continue,
                JoinType::Left => vec![joined],
                JoinType::Right => sources.clone().collect(),
                JoinType::Full => std::iter::once(joined).chain(sources.clone()).collect(),
            };

            let mut unprotected_fields = Vec::new();
            for side in optional_sides {
                NullableUses::new(side).collect(hir, &mut unprotected_fields);
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
            NullableUses::new(source.effective_name()).collect(hir, &mut uses);
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

    /// Reports every dotted path that starts from a query source, with its length.
    ///
    /// Each step after a reference field is an implicit join with the referenced table, and
    /// for a field of a composite type with every table it may point to. The length lets
    /// the IDE layer apply the user's threshold. Inside the condition of a virtual table
    /// the path starts from the virtual table's own field rather than from a source alias,
    /// so any dot there already dereferences; such paths carry no length.
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
        for nested in &table.subquery {
            self.check_nested_fields_by_dot(nested);
        }
        for param in &table.virtual_table_params {
            self.report_dotted_paths(param, PathStart::VirtualTableField);
        }
    }

    fn report_dotted_paths(&mut self, expr: &ExprHir, start: PathStart) {
        if let ExprHir::ColumnRef { parts, range, .. } = expr {
            if parts.len() >= 2 && !crate::is_mdo_type(parts[0].as_str()) {
                let parts_count = match start {
                    PathStart::SourceAlias => Some(parts.len() as u32),
                    PathStart::VirtualTableField => None,
                };
                self.diagnostics
                    .push(SdblDiagnostic::QueryNestedFieldsByDot { range: *range, parts_count });
            }
            return;
        }
        for child in direct_operands(expr) {
            self.report_dotted_paths(child, start);
        }
        if let ExprHir::In { values: InValues::Subquery(nested), .. } = expr {
            self.check_nested_fields_by_dot(nested);
        }
        // `ВЫРАЗИТЬ(... КАК Справочник.Х).Поле` is the remedy the standard recommends: it
        // reads one field of the one table named in the cast. Every further step
        // dereferences that field, and there is no source alias to count the depth from.
        if let ExprHir::FunctionCall {
            function: FunctionKind::Cast, member_access, range, ..
        } = expr
        {
            if member_access.len() >= 2 {
                self.diagnostics.push(SdblDiagnostic::QueryNestedFieldsByDot {
                    range: *range,
                    parts_count: None,
                });
            }
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
enum PathStart {
    SourceAlias,
    VirtualTableField,
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

/// Uses of one optional join side that may observe its NULL.
///
/// A use is safe when NULL cannot reach it: inside `ЕСТЬNULL`, in a `ВЫБОР` branch that is
/// taken only after a test has shown the side is present, next to an `ЕСТЬ NULL` test in a
/// disjunction, or anywhere in a query whose selection condition already tests the side
/// for presence. The null test itself is not a use.
struct NullableUses<'a> {
    side: &'a str,
}

impl<'a> NullableUses<'a> {
    fn new(side: &'a str) -> Self {
        Self { side }
    }

    fn collect(&self, hir: &SdblHir, out: &mut Vec<UnprotectedFieldRef>) {
        let filtered_by_presence = hir
            .where_clause
            .as_ref()
            .is_some_and(|condition| self.tests_presence_anywhere(condition));

        for field in &hir.select.fields {
            self.walk(&field.expr, filtered_by_presence, out);
        }
        if let Some(condition) = &hir.where_clause {
            self.walk(condition, filtered_by_presence, out);
        }
    }

    fn walk(&self, expr: &ExprHir, protected: bool, out: &mut Vec<UnprotectedFieldRef>) {
        match expr {
            ExprHir::ColumnRef { parts, range, .. } => {
                if !protected && self.is_side_field(parts) {
                    out.push(UnprotectedFieldRef {
                        table_alias: parts[0].to_string(),
                        field_name: parts[1].to_string(),
                        range: *range,
                    });
                }
            }
            ExprHir::IsNull { .. } => {}
            ExprHir::FunctionCall { function: FunctionKind::Isnull, .. } => {}
            ExprHir::Case { operand, when_clauses, else_expr, .. } => {
                if let Some(operand) = operand {
                    self.walk(operand, protected, out);
                }
                let mut present = protected;
                for clause in when_clauses {
                    self.walk(&clause.condition, present, out);
                    let branch_present =
                        present || self.proves_presence_when_true(&clause.condition);
                    self.walk(&clause.result, branch_present, out);
                    present = present || self.proves_presence_when_false(&clause.condition);
                }
                if let Some(else_expr) = else_expr {
                    self.walk(else_expr, present, out);
                }
            }
            ExprHir::BinaryOp { lhs, op: BinaryOp::Or, rhs, .. } => {
                let guarded = protected || self.has_null_test(lhs) || self.has_null_test(rhs);
                self.walk(lhs, guarded, out);
                self.walk(rhs, guarded, out);
            }
            ExprHir::In { values: InValues::Subquery(nested), .. } => {
                for operand in direct_operands(expr) {
                    self.walk(operand, protected, out);
                }
                for field in &nested.select.fields {
                    self.walk(&field.expr, protected, out);
                }
            }
            _ => {
                for operand in direct_operands(expr) {
                    self.walk(operand, protected, out);
                }
            }
        }
    }

    fn is_side_field(&self, parts: &[Name]) -> bool {
        // ASCII-only folding keeps the long-standing behaviour for Latin aliases; Cyrillic
        // aliases are matched as spelled.
        parts.len() >= 2 && parts[0].as_str().eq_ignore_ascii_case(self.side)
    }

    fn is_null_test(&self, expr: &ExprHir, negated_test: bool) -> bool {
        matches!(
            expr,
            ExprHir::IsNull { expr: tested, negated, .. }
                if *negated == negated_test
                    && matches!(&**tested, ExprHir::ColumnRef { parts, .. } if self.is_side_field(parts))
        )
    }

    /// `ЕСТЬ НЕ NULL`, `НЕ ... ЕСТЬ NULL`, or a conjunction containing one of them.
    fn proves_presence_when_true(&self, condition: &ExprHir) -> bool {
        match condition {
            ExprHir::BinaryOp { lhs, op: BinaryOp::And, rhs, .. } => {
                self.proves_presence_when_true(lhs) || self.proves_presence_when_true(rhs)
            }
            ExprHir::UnaryOp { op: UnaryOp::Not, expr, .. } => self.is_null_test(expr, false),
            ExprHir::BinaryOp { lhs, op: BinaryOp::Ne, rhs, .. } => {
                self.differs_from_its_replacement(lhs, rhs)
                    || self.differs_from_its_replacement(rhs, lhs)
            }
            _ => self.is_null_test(condition, true),
        }
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
