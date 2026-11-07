//! A derived table, `ИЗ (...) КАК П`, does not see the aliases of the query around it, while a
//! correlated subquery in a condition does.
//!
//! The platform accepts `Внеш.Ссылка` in `ГДЕ ... В (ВЫБРАТЬ ...)` and rejects it with
//! `Поле не найдено "Внеш.Ссылка"` inside a nested query of `ИЗ` (issue #82). Every rejection
//! below is paired with the same reference written where the platform accepts it, so a lowering
//! that forbade all correlation, or none, fails one of the two.
//!
//! The finding is recognised by its public text, which the contract fixes, and by the text its
//! range covers: the alias token and nothing else.

use std::sync::Arc;

use bsl_metadata::{Attribute, AttributeType, Configuration, MdoType, MetadataObject};
use sdbl_hir::{
    lower_sdbl_to_hir, ExprHir, InValues, SdblDiagnostic, SdblHir, SdblPackage, SdblType, TableRef,
    TokenCategory,
};

const UNAVAILABLE_TAIL: &str = "внешнего запроса недоступен внутри производной таблицы";

fn message(alias: &str) -> String {
    format!("Псевдоним \"{alias}\" {UNAVAILABLE_TAIL}")
}

fn is_unavailable(diagnostic: &SdblDiagnostic) -> bool {
    diagnostic.message().ends_with(UNAVAILABLE_TAIL)
}

/// The ticket's error, verbatim.
const D: &str = "ВЫБРАТЬ П.Ссылка КАК Р
ИЗ Документ.АктИнвентаризацииСАТУРН КАК Внеш
    ВНУТРЕННЕЕ СОЕДИНЕНИЕ (
        ВЫБРАТЬ Т.Ссылка КАК Ссылка
        ИЗ Документ.АктИнвентаризацииСАТУРН КАК Т
        ГДЕ Т.Ссылка = Внеш.Ссылка
    ) КАК П
    ПО П.Ссылка = Внеш.Ссылка";

/// The ticket's accepted correlation, verbatim.
const C: &str = "ВЫБРАТЬ Внеш.Ссылка КАК П
ИЗ Документ.АктИнвентаризацииСАТУРН КАК Внеш
ГДЕ Внеш.Ссылка В (
    ВЫБРАТЬ Т.Ссылка ИЗ Документ.АктИнвентаризацииСАТУРН КАК Т
    ГДЕ Т.Ссылка = Внеш.Ссылка
)";

/// D and C over a catalog of the tests' own metadata, written with a field whose type is
/// known (`Цена`, a number): the `Ссылка` of the ticket is typed on no source, so a lost or
/// leaked type could not be seen on it.
const TYPED_D: &str = "ВЫБРАТЬ П.Цена КАК Р
ИЗ Справочник.Товары КАК Внеш
    ВНУТРЕННЕЕ СОЕДИНЕНИЕ (
        ВЫБРАТЬ Т.Цена КАК Цена
        ИЗ Справочник.Товары КАК Т
        ГДЕ Т.Цена = Внеш.Цена
    ) КАК П
    ПО П.Цена = Внеш.Цена";

const TYPED_C: &str = "ВЫБРАТЬ Внеш.Цена КАК П
ИЗ Справочник.Товары КАК Внеш
ГДЕ Внеш.Цена В (
    ВЫБРАТЬ Т.Цена ИЗ Справочник.Товары КАК Т
    ГДЕ Т.Цена = Внеш.Цена
)";

/// A catalog and a document with fields whose types differ.
fn configuration() -> Arc<Configuration> {
    let attribute =
        |name: &str, attr_type| Attribute { name: name.into(), name_en: None, attr_type };
    let reference = |mdo_type, name: &str| AttributeType::Ref { mdo_type, name: name.to_string() };

    let mut config = Configuration::new("TestConfig");
    config.add_metadata_object(MetadataObject::new(MdoType::Catalog, "Виды"));

    let mut goods = MetadataObject::new(MdoType::Catalog, "Товары");
    goods.add_attribute(attribute("Цена", AttributeType::Number { precision: 10, scale: 2 }));
    goods.add_attribute(attribute("Вид", reference(MdoType::Catalog, "Виды")));
    config.add_metadata_object(goods);

    // `Цена` is a string here and a number in `Товары`: which of the two a reference resolved
    // to is visible in its type.
    let mut stores = MetadataObject::new(MdoType::Catalog, "Склады");
    stores.add_attribute(attribute("Цена", AttributeType::String { length: Some(10) }));
    config.add_metadata_object(stores);

    // `Склад` is a field of the document and, in the tests of the head collision, also the
    // alias of a catalog.
    let mut order = MetadataObject::new(MdoType::Document, "Заказ");
    order.add_attribute(attribute("Склад", reference(MdoType::Catalog, "Склады")));
    config.add_metadata_object(order);

    Arc::new(config)
}

struct Lowered {
    query: String,
    package: SdblPackage,
}

fn lower(query: &str, metadata: Option<Arc<Configuration>>) -> Lowered {
    let parse = parser::parse_sdbl(query);
    assert!(parse.errors().is_empty(), "the fixture must parse: {:?}\n{query}", parse.errors());
    Lowered { query: query.to_string(), package: lower_sdbl_to_hir(&parse, metadata) }
}

fn plain(query: &str) -> Lowered {
    lower(query, None)
}

fn typed(query: &str) -> Lowered {
    lower(query, Some(configuration()))
}

impl Lowered {
    fn text(&self, diagnostic: &SdblDiagnostic) -> &str {
        let range = diagnostic.range();
        &self.query[usize::from(range.start())..usize::from(range.end())]
    }

    /// The text each finding covers, in source order. Each finding must also carry its own
    /// covered text as the alias of the message, so a range that is wider, narrower or shifted
    /// fails here, not only in a test that happens to compare ranges.
    fn unavailable(&self) -> Vec<String> {
        let mut found: Vec<_> = self
            .package
            .all_diagnostics()
            .filter(|d| is_unavailable(d))
            .map(|d| {
                assert!(d.is_error(), "an unreachable alias is an error: {d:?}");
                assert_eq!(d.message(), message(self.text(d)), "{d:?}");
                (d.range().start(), self.text(d).to_string())
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, text)| text).collect()
    }

    /// Findings of other kinds, as `(kind, covered text)`.
    fn others(&self) -> Vec<(&'static str, String)> {
        let mut found: Vec<_> = self
            .package
            .all_diagnostics()
            .filter(|d| !is_unavailable(d))
            .filter_map(|d| {
                let kind = match d {
                    SdblDiagnostic::UnknownField { .. } => "UnknownField",
                    SdblDiagnostic::AmbiguousColumnRef { .. } => "AmbiguousColumnRef",
                    SdblDiagnostic::AmbiguousQualifiedHead { .. } => "AmbiguousQualifiedHead",
                    SdblDiagnostic::DuplicateAlias { .. } => "DuplicateAlias",
                    _ => return None,
                };
                Some((d.range().start(), kind, self.text(d).to_string()))
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, kind, text)| (kind, text)).collect()
    }

    fn count_others(&self, kind: &str) -> usize {
        self.others().iter().filter(|(k, _)| *k == kind).count()
    }

    /// The category the source map gives the token written exactly `text`.
    fn category_of(&self, text: &str) -> TokenCategory {
        let mut found =
            self.package.source_map.all_tokens().filter(|(token, _)| token.text == text);
        let (_, category) = found.next().unwrap_or_else(|| panic!("no token `{text}`"));
        assert!(found.next().is_none(), "the token `{text}` must be unique in the fixture");
        category
    }

    /// The type of the `nth` (from 0, in source order) column reference written exactly `text`.
    fn type_of(&self, text: &str, nth: usize) -> SdblType {
        let mut refs = Vec::new();
        for query in self.package.queries() {
            collect_hir(&query.hir, &mut refs);
        }
        refs.sort_by_key(|(range, _)| range.start());
        refs.iter()
            .filter(|(range, _)| {
                &self.query[usize::from(range.start())..usize::from(range.end())] == text
            })
            .nth(nth)
            .unwrap_or_else(|| panic!("no column reference #{nth} written `{text}`: {refs:?}"))
            .1
            .clone()
    }
}

type Refs = Vec<(text_size::TextRange, SdblType)>;

fn collect_hir(hir: &SdblHir, out: &mut Refs) {
    for field in &hir.select.fields {
        collect_expr(&field.expr, out);
    }
    for table in &hir.from {
        collect_table(table, out);
    }
    for join in &hir.joins {
        collect_table(&join.table, out);
        if let Some(condition) = &join.condition {
            collect_expr(condition, out);
        }
    }
    for expr in hir.where_clause.iter().chain(hir.having.iter()) {
        collect_expr(expr, out);
    }
    if let Some(group_by) = &hir.group_by {
        group_by.exprs.iter().for_each(|expr| collect_expr(expr, out));
    }
    if let Some(order_by) = &hir.order_by {
        order_by.items.iter().for_each(|item| collect_expr(&item.expr, out));
    }
    for union in &hir.unions {
        collect_hir(&union.query, out);
    }
}

fn collect_table(table: &TableRef, out: &mut Refs) {
    table.subquery.iter().for_each(|query| collect_hir(query, out));
    table.virtual_table_params.iter().for_each(|param| collect_expr(param, out));
}

fn collect_expr(expr: &ExprHir, out: &mut Refs) {
    match expr {
        ExprHir::ColumnRef { range, ty, .. } => out.push((*range, ty.clone())),
        ExprHir::Literal { .. } | ExprHir::Parameter { .. } | ExprHir::Missing { .. } => {}
        ExprHir::BinaryOp { lhs, rhs, .. } => {
            collect_expr(lhs, out);
            collect_expr(rhs, out);
        }
        ExprHir::UnaryOp { expr, .. } | ExprHir::IsNull { expr, .. } => collect_expr(expr, out),
        ExprHir::FunctionCall { args, .. } => args.iter().for_each(|arg| collect_expr(arg, out)),
        ExprHir::Case { operand, when_clauses, else_expr, .. } => {
            operand.iter().for_each(|expr| collect_expr(expr, out));
            for clause in when_clauses {
                collect_expr(&clause.condition, out);
                collect_expr(&clause.result, out);
            }
            else_expr.iter().for_each(|expr| collect_expr(expr, out));
        }
        ExprHir::Subquery { query, .. } => collect_hir(query, out),
        ExprHir::In { expr, values, .. } => {
            collect_expr(expr, out);
            match values {
                InValues::List(items) => items.iter().for_each(|item| collect_expr(item, out)),
                InValues::Subquery(query) => collect_hir(query, out),
            }
        }
        ExprHir::Between { expr, low, high, .. } => {
            collect_expr(expr, out);
            collect_expr(low, out);
            collect_expr(high, out);
        }
        ExprHir::Like { expr, pattern, escape, .. } => {
            collect_expr(expr, out);
            collect_expr(pattern, out);
            escape.iter().for_each(|expr| collect_expr(expr, out));
        }
        ExprHir::Tuple { elements, .. } => elements.iter().for_each(|e| collect_expr(e, out)),
    }
}

fn is_unknown(ty: &SdblType) -> bool {
    matches!(ty, SdblType::Unknown)
}

fn catalog_ref(name: &str) -> SdblType {
    SdblType::reference(MdoType::Catalog, name)
}

// ---- I1, I2: the ticket's pair --------------------------------------------------------------

#[test]
fn the_ticket_error_is_reported_once_on_the_inner_alias() {
    let lowered = plain(D);

    assert_eq!(lowered.unavailable(), ["Внеш"]);

    // The one finding is the `Внеш` inside the derived table, not the legal one in the outer `ПО`.
    let finding = lowered
        .package
        .all_diagnostics()
        .find(|d| is_unavailable(d))
        .expect("the error of the ticket is reported");
    let derived_start = D.find("(\n").expect("the derived table opens");
    let derived_end = D.find(") КАК П").expect("the derived table closes");
    let at = usize::from(finding.range().start());
    assert!(
        derived_start < at && at < derived_end,
        "the finding is outside the derived table: {at}"
    );
    assert_eq!(&D[at..at + "Внеш".len()], "Внеш");
}

#[test]
fn the_message_is_the_contracted_text_and_the_finding_is_an_error() {
    let lowered = plain(D);
    let finding = lowered
        .package
        .all_diagnostics()
        .find(|d| is_unavailable(d))
        .expect("the error of the ticket is reported");

    assert_eq!(
        finding.message(),
        "Псевдоним \"Внеш\" внешнего запроса недоступен внутри производной таблицы"
    );
    assert!(finding.is_error());
}

#[test]
fn the_ticket_correlation_is_not_reported() {
    assert_eq!(plain(C).unavailable(), Vec::<String>::new());
}

#[test]
fn the_error_needs_no_metadata_and_survives_it() {
    let query = D.replace("АктИнвентаризацииСАТУРН", "Заказ");

    assert_eq!(plain(&query).unavailable(), ["Внеш"]);
    assert_eq!(typed(&query).unavailable(), ["Внеш"]);
}

#[test]
fn a_hidden_reference_is_untyped_while_the_legal_one_around_it_keeps_its_type() {
    let lowered = typed(TYPED_D);

    assert_eq!(lowered.unavailable(), ["Внеш"]);
    // `Внеш.Цена` is written twice: in the derived table's `ГДЕ` and in the outer `ПО`.
    assert!(
        is_unknown(&lowered.type_of("Внеш.Цена", 0)),
        "a hidden alias must not lend its type: {:?}",
        lowered.type_of("Внеш.Цена", 0),
    );
    assert!(
        matches!(lowered.type_of("Внеш.Цена", 1), SdblType::Number { .. }),
        "the outer reference is legal and keeps the type of its source: {:?}",
        lowered.type_of("Внеш.Цена", 1),
    );
}

#[test]
fn the_correlated_reference_keeps_the_type_of_the_outer_source() {
    let lowered = typed(TYPED_C);

    assert_eq!(lowered.unavailable(), Vec::<String>::new());
    // The third `Внеш.Цена` is the one inside the subquery.
    assert!(
        matches!(lowered.type_of("Внеш.Цена", 2), SdblType::Number { .. }),
        "the correlation reaches the outer source: {:?}",
        lowered.type_of("Внеш.Цена", 2),
    );
}

#[test]
fn an_uncorrelated_subquery_is_quiet() {
    let query = "ВЫБРАТЬ Внеш.Ссылка КАК П ИЗ Справочник.Товары КАК Внеш \
                 ГДЕ Внеш.Ссылка В (ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Товары КАК Т)";

    assert_eq!(typed(query).unavailable(), Vec::<String>::new());
}

#[test]
fn successive_and_nested_in_subqueries_keep_the_correlation() {
    let query = "ВЫБРАТЬ Внеш.Ссылка КАК П ИЗ Справочник.Товары КАК Внеш \
                 ГДЕ Внеш.Ссылка В (ВЫБРАТЬ А.Ссылка ИЗ Справочник.Товары КАК А ГДЕ А.Вид = Внеш.Вид) \
                 И Внеш.Ссылка В (ВЫБРАТЬ Б.Ссылка ИЗ Справочник.Товары КАК Б ГДЕ Б.Ссылка В \
                 (ВЫБРАТЬ Г.Ссылка ИЗ Справочник.Товары КАК Г ГДЕ Г.Вид = Внеш.Вид И Г.Цена = Б.Цена))";
    let lowered = typed(query);

    assert_eq!(lowered.unavailable(), Vec::<String>::new());
    // All three `Внеш.Вид` — in the second and the third level — resolved to the outer source.
    assert_eq!(lowered.type_of("Внеш.Вид", 0), catalog_ref("Виды"));
    assert_eq!(lowered.type_of("Внеш.Вид", 1), catalog_ref("Виды"));
}

// ---- I3: the nearest boundary ---------------------------------------------------------------

/// Outer `Внеш`; derived table over `Л` holding an `IN` subquery over `Г`, which names `Л`
/// (inside the boundary) and `Внеш` (outside).
const IN_INSIDE_DERIVED: &str = "ВЫБРАТЬ П.К КАК К
ИЗ Справочник.Товары КАК Внеш
ВНУТРЕННЕЕ СОЕДИНЕНИЕ (
    ВЫБРАТЬ Л.Ссылка КАК К
    ИЗ Справочник.Склады КАК Л
    ГДЕ Л.Ссылка В (
        ВЫБРАТЬ Г.Ссылка ИЗ Справочник.Склады КАК Г
        ГДЕ Г.Цена = Л.Цена И Г.Цена = Внеш.Цена
    )
) КАК П ПО ИСТИНА";

#[test]
fn an_in_subquery_inside_a_derived_table_sees_the_derived_sources_but_not_the_outer_ones() {
    let lowered = typed(IN_INSIDE_DERIVED);

    assert_eq!(lowered.unavailable(), ["Внеш"]);
    // `Л` is on the near side of the boundary: it resolves to the derived table's own source.
    assert!(matches!(lowered.type_of("Л.Цена", 0), SdblType::String { .. }));
    assert!(is_unknown(&lowered.type_of("Внеш.Цена", 0)));
}

#[test]
fn a_derived_table_inside_an_in_subquery_hides_the_sources_of_the_subquery_and_the_query() {
    let query = "ВЫБРАТЬ Внеш.Ссылка КАК К ИЗ Справочник.Товары КАК Внеш \
                 ГДЕ Внеш.Ссылка В (ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Склады КАК Т \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Ч.Ссылка КАК К ИЗ Справочник.Склады КАК Ч \
                 ГДЕ Ч.Цена = Т.Цена И Ч.Цена = Внеш.Цена) КАК П ПО ИСТИНА)";

    // Both the source of the `IN` subquery and the one of the outermost query are out of reach.
    assert_eq!(typed(query).unavailable(), ["Т", "Внеш"]);
}

#[test]
fn the_second_nested_derived_table_hides_the_first_one() {
    let query = "ВЫБРАТЬ Д1.К КАК К ИЗ Справочник.Товары КАК Внеш \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ А.Ссылка КАК К ИЗ Справочник.Склады КАК А \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Б.Ссылка КАК К ИЗ Справочник.Склады КАК Б \
                 ГДЕ Б.Ссылка В (ВЫБРАТЬ Г.Ссылка ИЗ Справочник.Склады КАК Г \
                 ГДЕ Г.Цена = Б.Цена И Г.Цена = А.Цена И Г.Цена = Внеш.Цена)) КАК Д2 ПО ИСТИНА) \
                 КАК Д1 ПО ИСТИНА";

    // `Г` and `Б` are inside the innermost boundary; `А` and `Внеш` are beyond it.
    assert_eq!(typed(query).unavailable(), ["А", "Внеш"]);
}

#[test]
fn leaving_the_derived_table_gives_the_outer_scope_back() {
    let query = "ВЫБРАТЬ Внеш.Цена КАК Ц ИЗ Справочник.Товары КАК Внеш \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т \
                 ГДЕ Т.Цена = Внеш.Цена) КАК П ПО П.К = Внеш.Ссылка \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Ч.Ссылка КАК К ИЗ Справочник.Склады КАК Ч \
                 ГДЕ Ч.Цена = Внеш.Цена) КАК Р ПО Р.К = Внеш.Ссылка \
                 ГДЕ Внеш.Цена > 0";
    let lowered = typed(query);

    // One error per derived table; the references after each of them, in `ПО`, `ГДЕ` and the
    // select list, are the outer query's own and stay legal and typed.
    assert_eq!(lowered.unavailable(), ["Внеш", "Внеш"]);
    assert!(matches!(lowered.type_of("Внеш.Цена", 0), SdblType::Number { .. }));
    assert!(is_unknown(&lowered.type_of("Внеш.Цена", 1)));
    assert!(is_unknown(&lowered.type_of("Внеш.Цена", 2)));
    assert!(matches!(lowered.type_of("Внеш.Цена", 3), SdblType::Number { .. }));
}

// ---- I4: shadowing and case -----------------------------------------------------------------

#[test]
fn a_local_source_under_the_hidden_alias_shadows_it() {
    // Outer `Т` has a numeric `Цена`; the derived table's own `Т` has a string one.
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Т \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т \
                 ГДЕ Т.Цена = \"\") КАК П ПО ИСТИНА";
    let lowered = typed(query);

    assert_eq!(lowered.unavailable(), Vec::<String>::new());
    assert!(
        matches!(lowered.type_of("Т.Цена", 0), SdblType::String { .. }),
        "the local source answers, not the outer one: {:?}",
        lowered.type_of("Т.Цена", 0),
    );
}

#[test]
fn the_case_of_the_alias_does_not_change_the_outcome() {
    let shadowed = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Т \
                    ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ т.Ссылка КАК К ИЗ Справочник.Склады КАК Т \
                    ГДЕ т.Цена = \"\") КАК П ПО ИСТИНА";
    let hidden = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Т \
                  ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Л.Ссылка КАК К ИЗ Справочник.Склады КАК Л \
                  ГДЕ Л.Цена = т.Цена) КАК П ПО ИСТИНА";

    assert_eq!(typed(shadowed).unavailable(), Vec::<String>::new());
    assert!(matches!(typed(shadowed).type_of("т.Цена", 0), SdblType::String { .. }));
    // The message keeps the spelling of the reference, not of the declaration.
    assert_eq!(typed(hidden).unavailable(), ["т"]);
}

#[test]
fn a_local_source_of_unknown_type_does_not_lend_the_outer_type() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Т \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.НетТакого КАК Т \
                 ГДЕ Т.Цена = 1) КАК П ПО ИСТИНА";
    let lowered = typed(query);

    assert_eq!(lowered.unavailable(), Vec::<String>::new());
    assert!(
        is_unknown(&lowered.type_of("Т.Цена", 0)),
        "the outer `Т` is hidden even where the local one is a stranger: {:?}",
        lowered.type_of("Т.Цена", 0),
    );
}

#[test]
fn without_the_local_declaration_the_same_reference_is_reported() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Т \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Л.Ссылка КАК К ИЗ Справочник.Склады КАК Л \
                 ГДЕ Т.Цена = \"\") КАК П ПО ИСТИНА";

    assert_eq!(typed(query).unavailable(), ["Т"]);
}

// ---- I5: the order of the sources -----------------------------------------------------------

const INNER_REFERS_TO_OUTER: &str =
    "(ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т ГДЕ Т.Цена = Внеш.Цена) КАК П";

#[test]
fn the_outer_source_declared_after_the_derived_table_still_hides() {
    let before = format!(
        "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
         {INNER_REFERS_TO_OUTER} ПО ИСТИНА"
    );
    let after_in_from =
        format!("ВЫБРАТЬ П.К КАК К ИЗ {INNER_REFERS_TO_OUTER}, Справочник.Товары КАК Внеш");
    let after_in_join = format!(
        "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Склады КАК А ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
         {INNER_REFERS_TO_OUTER} ПО ИСТИНА ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Товары КАК Внеш ПО ИСТИНА"
    );

    for (label, query) in [
        ("outer source first", before),
        ("derived table first in FROM", after_in_from),
        ("outer source joined after the derived table", after_in_join),
    ] {
        assert_eq!(plain(&query).unavailable(), ["Внеш"], "[{label}] {query}");
        assert_eq!(typed(&query).unavailable(), ["Внеш"], "[{label}] {query}");
    }
}

#[test]
fn a_name_declared_nowhere_is_not_an_unavailable_alias() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т ГДЕ Т.Цена = Нигде.Цена) \
                 КАК П ПО ИСТИНА";

    assert_eq!(typed(query).unavailable(), Vec::<String>::new());
    assert_eq!(plain(query).unavailable(), Vec::<String>::new());
}

#[test]
fn an_alias_of_a_neighbouring_union_branch_or_query_is_not_an_outer_alias() {
    let union_branch = format!(
        "ВЫБРАТЬ П.К КАК К ИЗ {INNER_REFERS_TO_OUTER} \
         ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ Внеш.Ссылка КАК К ИЗ Справочник.Товары КАК Внеш"
    );
    let next_query = format!(
        "ВЫБРАТЬ П.К КАК К ИЗ {INNER_REFERS_TO_OUTER}; \
         ВЫБРАТЬ Внеш.Ссылка КАК К ИЗ Справочник.Товары КАК Внеш"
    );
    let earlier_query = format!(
        "ВЫБРАТЬ Внеш.Ссылка КАК К ИЗ Справочник.Товары КАК Внеш; \
         ВЫБРАТЬ П.К КАК К ИЗ {INNER_REFERS_TO_OUTER}"
    );

    for (label, query) in [
        ("union branch", union_branch),
        ("next query", next_query),
        ("earlier query", earlier_query),
    ] {
        assert_eq!(typed(&query).unavailable(), Vec::<String>::new(), "[{label}] {query}");
    }
}

// ---- I6: UNION ------------------------------------------------------------------------------

#[test]
fn every_branch_of_a_derived_union_is_closed_and_the_branches_do_not_leak_into_each_other() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Л.Цена КАК К ИЗ Справочник.Склады КАК Л \
                 ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ М.Цена ИЗ Справочник.Склады КАК М \
                 ГДЕ М.Цена = Внеш.Цена И Л.НетТакогоПоля = 0) КАК П ПО ИСТИНА";
    let lowered = typed(query);

    // Only the second branch names the outer alias. `Л` belongs to the first branch alone and
    // is declared in no enclosing query, so it is neither reported nor resolved: had the first
    // branch's frame stayed open, `Л.НетТакогоПоля` would be an unknown field of `Склады`.
    assert_eq!(lowered.unavailable(), ["Внеш"]);
    assert_eq!(lowered.others(), Vec::<(&str, String)>::new());
}

#[test]
fn the_first_branch_of_a_derived_union_is_closed_too() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Л.Ссылка КАК К ИЗ Справочник.Склады КАК Л ГДЕ Л.Цена = Внеш.Цена \
                 ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ М.Ссылка ИЗ Справочник.Склады КАК М) КАК П ПО ИСТИНА";

    assert_eq!(typed(query).unavailable(), ["Внеш"]);
}

#[test]
fn the_branches_of_an_in_subquery_keep_the_correlation() {
    // The HIR keeps only the diagnostics of the branches after the first one, so the
    // correlation is read from a finding only the outer source can produce: the outer `Товары`
    // has no field `НетТакогоПоля`. A silent branch would leave the list empty.
    let query = "ВЫБРАТЬ Внеш.Цена КАК К ИЗ Справочник.Товары КАК Внеш \
                 ГДЕ Внеш.Цена В (ВЫБРАТЬ Т.Цена ИЗ Справочник.Склады КАК Т \
                 ГДЕ Внеш.НетПервого = 0 \
                 ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ У.Цена ИЗ Справочник.Склады КАК У \
                 ГДЕ Внеш.НетВторого = 0)";
    let lowered = typed(query);

    assert_eq!(lowered.unavailable(), Vec::<String>::new());
    assert_eq!(
        lowered.others(),
        [("UnknownField", "НетПервого".to_string()), ("UnknownField", "НетВторого".to_string()),]
    );
}

// ---- I7: the forms of a reference and the range ---------------------------------------------

fn derived_select(select_item: &str) -> String {
    format!(
        "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
         (ВЫБРАТЬ {select_item} ИЗ Справочник.Склады КАК Т) КАК П ПО ИСТИНА"
    )
}

#[test]
fn a_field_a_chain_and_a_star_each_give_one_finding_on_the_alias_token() {
    for (label, item) in
        [("field", "Внеш.Цена КАК К"), ("chain", "Внеш.Вид.Наименование КАК К"), ("star", "Внеш.*")]
    {
        let query = derived_select(item);
        let lowered = typed(&query);

        assert_eq!(lowered.unavailable(), ["Внеш"], "[{label}] {query}");
    }
}

#[test]
fn the_range_is_the_alias_token_without_the_trailing_trivia() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т \
                 ГДЕ ВНЕШ // комментарий\n.Цена = \"\") КАК П ПО ИСТИНА";
    let lowered = typed(query);

    assert_eq!(lowered.unavailable(), ["ВНЕШ"]);
}

#[test]
fn a_comment_after_the_reference_does_not_move_the_finding() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т \
                 ГДЕ Внеш.Цена = \"\" // хвост\n) КАК П ПО ИСТИНА";

    assert_eq!(typed(query).unavailable(), ["Внеш"]);
}

#[test]
fn local_qualifiers_and_the_plain_star_are_untouched() {
    for (label, item) in
        [("local field", "Т.Цена КАК К"), ("local star", "Т.*"), ("plain star", "*")]
    {
        let query = derived_select(item);

        assert_eq!(typed(&query).unavailable(), Vec::<String>::new(), "[{label}] {query}");
    }
}

#[test]
fn a_hidden_alias_does_not_cascade_into_an_unknown_field() {
    let hidden = derived_select("Внеш.НетТакогоПоля КАК К");
    let control = "ВЫБРАТЬ Внеш.НетТакогоПоля КАК К ИЗ Справочник.Товары КАК Внеш";

    assert_eq!(typed(&hidden).unavailable(), ["Внеш"]);
    assert_eq!(typed(&hidden).count_others("UnknownField"), 0, "{:?}", typed(&hidden).others());
    // The control: the same field of the same source IS unknown where the source is visible.
    assert_eq!(typed(control).count_others("UnknownField"), 1, "{:?}", typed(control).others());
}

#[test]
fn the_hidden_alias_is_reported_in_every_clause_of_the_derived_table() {
    // The reference sits in an aggregate argument, a join condition, `СГРУППИРОВАТЬ ПО` and
    // `ИМЕЮЩИЕ`; the same four places in an `IN` subquery must stay quiet.
    let derived = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                   (ВЫБРАТЬ СУММА(Внеш.Цена) КАК К ИЗ Справочник.Склады КАК Т \
                   ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Склады КАК У ПО У.Цена = Внеш.Цена \
                   СГРУППИРОВАТЬ ПО Внеш.Цена \
                   ИМЕЮЩИЕ СУММА(Внеш.Цена) > 0) КАК П ПО ИСТИНА";
    let correlated = "ВЫБРАТЬ Внеш.Цена КАК К ИЗ Справочник.Товары КАК Внеш \
                      ГДЕ Внеш.Цена В (ВЫБРАТЬ СУММА(Внеш.Цена) ИЗ Справочник.Склады КАК Т \
                      ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Склады КАК У ПО У.Цена = Внеш.Цена \
                      СГРУППИРОВАТЬ ПО Внеш.Цена \
                      ИМЕЮЩИЕ СУММА(Внеш.Цена) > 0)";

    assert_eq!(typed(derived).unavailable(), ["Внеш", "Внеш", "Внеш", "Внеш"]);
    assert_eq!(typed(correlated).unavailable(), Vec::<String>::new());
}

#[test]
fn the_field_of_a_hidden_source_is_not_marked_as_an_unresolved_reference() {
    let hidden = derived_select("Внеш.НетТакогоПоля КАК К");
    let control = "ВЫБРАТЬ Внеш.НетТакогоПоля КАК К ИЗ Справочник.Товары КАК Внеш";

    // The alias is already reported; judging the field against a source the reference cannot
    // reach would paint it as unresolved a second time.
    assert_eq!(typed(&hidden).category_of("НетТакогоПоля"), TokenCategory::FieldName);
    // The control: against a visible source with metadata the same field IS unresolved.
    assert_eq!(typed(control).category_of("НетТакогоПоля"), TokenCategory::UnresolvedFieldName);
}

// ---- I8: the namespaces that stay as they were ----------------------------------------------

#[test]
fn a_temporary_table_of_the_batch_is_a_source_inside_a_derived_table() {
    // The temporary table takes the numeric `Цена` of `Товары`; the outer `Склады` has a
    // string one, so the type of `Г.Ц` says which of the two the reference came from.
    let query = "ВЫБРАТЬ Т.Цена КАК Ц ПОМЕСТИТЬ ВТ ИЗ Справочник.Товары КАК Т; \
                 ВЫБРАТЬ Внеш.Цена КАК К ИЗ Справочник.Склады КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Г.Ц КАК К ИЗ ВТ КАК Г ГДЕ Г.Ц = Внеш.Цена) КАК П ПО ИСТИНА";
    let lowered = typed(query);

    // The temporary table is no outer alias; the only finding is the genuine correlation.
    assert_eq!(lowered.unavailable(), ["Внеш"]);
    assert!(
        matches!(lowered.type_of("Г.Ц", 0), SdblType::Number { .. }),
        "{:?}",
        lowered.type_of("Г.Ц", 0),
    );
}

#[test]
fn a_bare_column_still_reaches_the_enclosing_query() {
    // `Вид` is offered by the outer `Товары` only. Bare names are not aliases: they resolve as
    // they did before the boundary existed.
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т ГДЕ Вид = Т.Ссылка) \
                 КАК П ПО ИСТИНА";
    let lowered = typed(query);

    assert_eq!(lowered.unavailable(), Vec::<String>::new());
    assert_eq!(lowered.type_of("Вид", 0), catalog_ref("Виды"));
}

#[test]
fn the_order_by_outputs_frame_does_not_open_the_boundary() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т УПОРЯДОЧИТЬ ПО Внеш.Цена) \
                 КАК П ПО ИСТИНА";
    let control =
        "ВЫБРАТЬ Внеш.Ссылка КАК К ИЗ Справочник.Товары КАК Внеш УПОРЯДОЧИТЬ ПО Внеш.Цена";

    assert_eq!(typed(query).unavailable(), ["Внеш"]);
    assert_eq!(typed(control).unavailable(), Vec::<String>::new());
}

// ---- I9: the findings that are not new ------------------------------------------------------

#[test]
fn an_unknown_field_of_a_visible_local_source_is_still_reported() {
    let query = derived_select("Т.НетТакогоПоля КАК К");

    assert_eq!(typed(&query).unavailable(), Vec::<String>::new());
    assert_eq!(typed(&query).others(), [("UnknownField", "НетТакогоПоля".to_string())]);
}

#[test]
fn a_duplicate_alias_inside_a_derived_table_is_still_reported() {
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Склады КАК Т ПО ИСТИНА) КАК П ПО ИСТИНА";
    let lowered = typed(query);

    assert_eq!(lowered.count_others("DuplicateAlias"), 1, "{:?}", lowered.others());
}

#[test]
fn a_duplicate_of_a_hidden_alias_is_not_a_duplicate() {
    // The local `Внеш` does not collide with the outer one: a boundary lies between them.
    let query = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Товары КАК Внеш ВНУТРЕННЕЕ СОЕДИНЕНИЕ \
                 (ВЫБРАТЬ Внеш.Ссылка КАК К ИЗ Справочник.Склады КАК Внеш) КАК П ПО ИСТИНА";
    let lowered = typed(query);

    assert_eq!(lowered.count_others("DuplicateAlias"), 0, "{:?}", lowered.others());
    assert_eq!(lowered.unavailable(), Vec::<String>::new());
}

/// `Склад` is the alias of a catalog and a field of the document joined beside it, so `Склад.Код`
/// is ambiguous in the query that declares both. In a derived table the alias is out of reach:
/// the reference is the unavailable alias and nothing else.
#[test]
fn a_hidden_source_makes_no_qualified_head_ambiguity_inside_the_derived_table() {
    let outer = "ВЫБРАТЬ Склад.Код КАК К ИЗ Справочник.Склады КАК Склад \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ КАК З ПО ИСТИНА";
    let derived = "ВЫБРАТЬ П.К КАК К ИЗ Справочник.Склады КАК Склад \
                   ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ КАК З ПО ИСТИНА \
                   ВНУТРЕННЕЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Т.Ссылка КАК К ИЗ Справочник.Склады КАК Т \
                   ГДЕ Т.Код = Склад.Код) КАК П ПО ИСТИНА";

    // The control: the collision is real at the level that declares both.
    assert_eq!(
        typed(outer).count_others("AmbiguousQualifiedHead"),
        1,
        "{:?}",
        typed(outer).others()
    );

    let lowered = typed(derived);
    assert_eq!(lowered.unavailable(), ["Склад"]);
    assert_eq!(lowered.count_others("AmbiguousQualifiedHead"), 0, "{:?}", lowered.others());
}

// ---- the stack of frames --------------------------------------------------------------------

/// The same boundary seen through the public `Scope`, without a query: which frame lets an
/// alias through and which closes it.
mod scope_boundary {
    use sdbl_hir::{Name, Scope, TableRef};
    use text_size::TextRange;

    fn source(alias: &str) -> TableRef {
        let mut table = TableRef::missing(TextRange::default());
        table.full_name = format!("Справочник.{alias}");
        table.alias = Some(Name::new(alias));
        table
    }

    fn scope_with_outer(alias: &str) -> Scope<'static> {
        let mut scope = Scope::new();
        let _ = scope.add_table(source(alias));
        scope
    }

    #[test]
    fn a_plain_frame_lets_the_outer_alias_through_and_a_derived_table_frame_does_not() {
        let mut scope = scope_with_outer("Внеш");

        scope.push_frame();
        assert!(scope.find_table("Внеш").is_some(), "a condition's subquery sees the outer alias");
        assert!(!scope.is_inaccessible_alias("Внеш"));
        scope.pop_frame();

        scope.push_derived_table_frame();
        assert!(scope.find_table("Внеш").is_none(), "a derived table does not");
        assert!(!scope.is_table_alias("Внеш"));
        assert!(scope.is_inaccessible_alias("Внеш"));
    }

    #[test]
    fn leaving_the_derived_table_frame_gives_the_alias_back() {
        let mut scope = scope_with_outer("Внеш");

        scope.push_derived_table_frame();
        scope.pop_frame();

        assert!(scope.find_table("Внеш").is_some());
        assert!(!scope.is_inaccessible_alias("Внеш"));
    }

    #[test]
    fn a_local_source_is_visible_and_shadows_the_hidden_one_of_the_same_name() {
        let mut scope = scope_with_outer("Т");
        scope.push_derived_table_frame();
        let _ = scope.add_table(source("Т"));
        scope.push_frame();

        let found = scope.find_table("т").expect("the local source answers, in any case");
        assert_eq!(found.full_name, "Справочник.Т");
        assert!(!scope.is_inaccessible_alias("Т"), "a visible name is not a hidden one");
    }

    #[test]
    fn only_a_name_declared_in_an_outer_frame_is_inaccessible() {
        let mut scope = scope_with_outer("Внеш");
        scope.push_derived_table_frame();
        let _ = scope.add_table(source("Т"));

        assert!(scope.is_inaccessible_alias("ВНЕШ"), "case does not matter");
        assert!(!scope.is_inaccessible_alias("Т"), "the local source is on the near side");
        assert!(!scope.is_inaccessible_alias("Нигде"), "a name declared nowhere is only unknown");
    }

    #[test]
    fn the_second_boundary_hides_everything_outside_it_at_any_depth() {
        let mut scope = scope_with_outer("Внеш");
        scope.push_derived_table_frame();
        let _ = scope.add_table(source("Первый"));
        scope.push_derived_table_frame();
        let _ = scope.add_table(source("Второй"));
        scope.push_frame();

        assert!(scope.find_table("Второй").is_some());
        assert!(scope.find_table("Первый").is_none());
        assert!(scope.find_table("Внеш").is_none());
        assert!(scope.is_inaccessible_alias("Первый"));
        assert!(scope.is_inaccessible_alias("Внеш"), "the outermost alias is out of reach too");

        scope.pop_frame();
        scope.pop_frame();
        assert!(scope.find_table("Первый").is_some(), "one boundary left: its near side is back");
        assert!(scope.find_table("Внеш").is_none());
    }

    #[test]
    fn a_virtual_table_or_outputs_frame_above_the_boundary_does_not_open_it() {
        for open in [
            |scope: &mut Scope<'static>| scope.push_opaque_frame(),
            |scope: &mut Scope<'static>| scope.push_outputs_frame(false),
            |scope: &mut Scope<'static>| scope.push_outputs_frame(true),
        ] {
            let mut scope = scope_with_outer("Внеш");
            scope.push_derived_table_frame();
            open(&mut scope);

            assert!(scope.find_table("Внеш").is_none());
            assert!(scope.is_inaccessible_alias("Внеш"));
        }
    }

    /// The control of the test above: with no derived table in the way, the same two frames
    /// leave the outer alias reachable — they close bare names, not aliases.
    #[test]
    fn a_virtual_table_or_outputs_frame_alone_leaves_the_outer_alias_reachable() {
        for open in [
            |scope: &mut Scope<'static>| scope.push_opaque_frame(),
            |scope: &mut Scope<'static>| scope.push_outputs_frame(false),
            |scope: &mut Scope<'static>| scope.push_outputs_frame(true),
        ] {
            let mut scope = scope_with_outer("Внеш");
            scope.push_frame();
            open(&mut scope);

            assert!(scope.find_table("Внеш").is_some());
            assert!(!scope.is_inaccessible_alias("Внеш"));
        }
    }

    #[test]
    fn a_frame_between_the_outer_query_and_the_boundary_keeps_the_outer_alias_hidden() {
        // A condition's subquery sits between the outer query and the derived table inside it.
        let mut scope = scope_with_outer("Внеш");
        scope.push_frame();
        let _ = scope.add_table(source("Т"));
        scope.push_derived_table_frame();

        assert!(scope.is_inaccessible_alias("Внеш"));
        assert!(scope.is_inaccessible_alias("Т"));
    }
}
