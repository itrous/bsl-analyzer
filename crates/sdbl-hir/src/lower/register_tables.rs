//! Field sets of the tables an accounting register exposes to a query.
//!
//! An accounting register is the one register kind whose query field names are not its
//! metadata names: a correspondence register keeps a debit and a credit side of every
//! non-balance dimension and resource (`<Измерение>Дт`, `<Ресурс>Кт`), its virtual tables derive
//! a family of names per resource (`<Ресурс>ОстатокДт`, `<Ресурс>КорОборот`), and the number of
//! `Субконто<N>` fields comes from the chart of accounts the register is kept on. The sets
//! below follow the ITS query-language reference («Язык запросов 1С:Предприятия», гл. 3,
//! разделы 111-127).
//!
//! The contract of the unknown-field rule decides how doubt is spent: a name that may or may
//! not exist is listed as provisional (the rule stays silent on it and the ambiguity rule does
//! not count it), and a table whose shape is not modelled is reported incomplete.

use bsl_metadata::{MdoType, Register};

use crate::hir::{FieldDef, ResolvedTable};
use crate::standard_fields::VirtualTableType;
use crate::SdblType;

use super::context::LoweringContext;

/// How a dimension or resource is named in a table that keeps both sides of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sides {
    /// One field under its own name: a balance member, or any member of a register without
    /// correspondence.
    Single,
    /// Two fields, `<Имя>Дт` and `<Имя>Кт`, and none under the bare name.
    Split,
    /// The metadata does not say which; every spelling is offered provisionally.
    Unknown,
}

fn sides(correspondence: Option<bool>, balance: Option<bool>) -> Sides {
    match (correspondence, balance) {
        (Some(false), _) | (_, Some(true)) => Sides::Single,
        (Some(true), Some(false)) => Sides::Split,
        _ => Sides::Unknown,
    }
}

/// A dimension or resource with what the register says about it.
struct Member {
    name: String,
    name_en: Option<String>,
    ty: SdblType,
    balance: Option<bool>,
}

impl Member {
    /// `<Имя><suffix>`, in both languages. The English spelling appends to the member's English
    /// name when it has one, and to its own name otherwise: a query written in English still
    /// spells metadata names the way the configuration does.
    fn derived(
        &self,
        suffix_ru: &str,
        suffix_en: &str,
        ty: SdblType,
        provisional: bool,
    ) -> FieldDef {
        let base_en = self.name_en.as_deref().unwrap_or(&self.name);
        FieldDef {
            name: format!("{}{suffix_ru}", self.name),
            name_en: Some(format!("{base_en}{suffix_en}")),
            ty,
            is_standard: false,
            provisional,
            source: Some(self.name.clone()),
        }
    }

    fn plain(&self, provisional: bool) -> FieldDef {
        FieldDef {
            name: self.name.clone(),
            name_en: self.name_en.clone(),
            ty: self.ty.clone(),
            is_standard: false,
            provisional,
            source: None,
        }
    }

    /// The member as a main-table or records table names it: once, or once per side.
    fn sided(&self, sides: Sides, out: &mut Vec<FieldDef>) {
        match sides {
            Sides::Single => out.push(self.plain(false)),
            Sides::Split => {
                out.push(self.derived("Дт", "Dr", self.ty.clone(), false));
                out.push(self.derived("Кт", "Cr", self.ty.clone(), false));
            }
            Sides::Unknown => {
                out.push(self.plain(true));
                out.push(self.derived("Дт", "Dr", self.ty.clone(), true));
                out.push(self.derived("Кт", "Cr", self.ty.clone(), true));
            }
        }
    }
}

/// Safety bound for expanding `Субконто<N>`. A chart's ext-dimension count is a small metadata
/// property; a larger value is not trusted, because [`numbered`] allocates one field per
/// number and an unbounded `u32` does not fit in memory. The table is then left unjudged,
/// as when the chart is absent.
const EXT_DIMENSION_COUNT_CEILING: u32 = 64;

/// `<Имя>1..<Имя>N`: the numbered ext-dimension fields.
fn numbered(
    ru: &str,
    en: &str,
    count: u32,
    ty: &SdblType,
    provisional: bool,
    out: &mut Vec<FieldDef>,
) {
    for n in 1..=count {
        let mut field = FieldDef::standard(format!("{ru}{n}"), format!("{en}{n}"), ty.clone());
        field.provisional = provisional;
        out.push(field);
    }
}

/// The English spelling of `РазвернутыйОстаток` is not attested in the reference this module
/// follows, so whichever of the two candidate spellings the tables above did not use is offered
/// too, provisionally: an English query must not be told that a field it spelled correctly is
/// missing.
///
/// The entry is that English spelling alone. The Russian name is already an explicit field of
/// the same table; a second, provisional entry under it would make
/// [`ResolvedTable::column_is_provisional`] treat the known field as optional, and two sources
/// that both offer it would not be reported ambiguous.
fn split_balance_spellings(member: &Member, prefix_en: &str, out: &mut Vec<FieldDef>) {
    let other = match prefix_en {
        "" => "Splitted",
        _ => "Extended",
    };
    let base_en = member.name_en.as_deref().unwrap_or(&member.name);
    for side_en in ["Dr", "Cr"] {
        let mut field = FieldDef::new(
            format!("{base_en}{prefix_en}{other}Balance{side_en}"),
            member.ty.clone(),
        )
        .derived_from(member.name.clone());
        field.provisional = true;
        out.push(field);
    }
}

fn standard(ru: &str, en: &str, ty: SdblType, provisional: bool) -> FieldDef {
    let mut field = FieldDef::standard(ru, en, ty);
    field.provisional = provisional;
    field
}

/// The fields every turnover-shaped table adds to its dimensions and resources.
fn turnover_period_fields(out: &mut Vec<FieldDef>) {
    out.push(FieldDef::standard("Период", "Period", SdblType::Date));
    out.push(FieldDef::standard("Регистратор", "Recorder", SdblType::AnyRef));
    out.push(FieldDef::standard("НомерСтроки", "LineNumber", SdblType::number()));
    period_breakdown_fields(out);
}

/// The per-period breakdown of a turnover-shaped table (`ПериодМесяц`, …) and its point in
/// time. Which of them a call produces depends on the `Периодичность` argument, not on
/// metadata, so all are offered provisionally: present for the unknown-field rule, absent for
/// the ambiguity rule.
pub(super) fn period_breakdown_fields(out: &mut Vec<FieldDef>) {
    out.push(standard("МоментВремени", "PointInTime", SdblType::DateTime, true));
    for (ru, en) in [
        ("ПериодСекунда", "SecondPeriod"),
        ("ПериодМинута", "MinutePeriod"),
        ("ПериодЧас", "HourPeriod"),
        ("ПериодДень", "DayPeriod"),
        ("ПериодНеделя", "WeekPeriod"),
        ("ПериодДекада", "TenDaysPeriod"),
        ("ПериодМесяц", "MonthPeriod"),
        ("ПериодКвартал", "QuarterPeriod"),
        ("ПериодПолугодие", "HalfYearPeriod"),
        ("ПериодГод", "YearPeriod"),
    ] {
        out.push(standard(ru, en, SdblType::Date, true));
    }
}

impl LoweringContext<'_> {
    /// The main table of an accounting register (`table == None`) or one of its own tables.
    pub(super) fn build_accounting_register_table(
        &self,
        register: &Register,
        table: Option<VirtualTableType>,
    ) -> ResolvedTable {
        let correspondence = register.correspondence();
        let chart = register.chart_of_accounts();
        let ext_count = chart.and_then(|chart| {
            let count = self
                .resolver?
                .resolve_metadata_object(MdoType::ChartOfAccounts, chart)?
                .max_ext_dimension_count?;
            (count <= EXT_DIMENSION_COUNT_CEILING).then_some(count)
        });
        let account_ty = chart
            .map(|chart| SdblType::reference(MdoType::ChartOfAccounts, chart))
            .unwrap_or(SdblType::AnyRef);

        let typed = |attr_type: Option<&bsl_metadata::AttributeType>| {
            attr_type.map(|at| self.resolve_attribute_type(at)).unwrap_or(SdblType::Unknown)
        };
        // A separator is part of the record key and a column of every table of the register,
        // which is what a dimension is; it has one column, never a debit and a credit side.
        let separators =
            register.common_attributes().iter().filter(|c| c.separator).map(|c| Member {
                name: c.name.clone(),
                name_en: None,
                ty: self.resolve_attribute_type(&c.attr_type),
                balance: Some(true),
            });
        let dimensions: Vec<Member> = register
            .dimensions()
            .iter()
            .map(|d| Member {
                name: d.name().to_string(),
                name_en: None,
                ty: typed(d.attr_type()),
                balance: d.balance(),
            })
            .chain(separators)
            .collect();
        let resources: Vec<Member> = register
            .resources()
            .iter()
            .map(|r| Member {
                name: r.name().to_string(),
                name_en: r.name_en().map(str::to_string),
                ty: typed(r.attr_type()),
                balance: r.balance(),
            })
            .collect();
        let attributes: Vec<FieldDef> = register
            .attributes()
            .iter()
            .map(|a| {
                FieldDef::new_with_names(
                    a.name().to_string(),
                    a.name_en().map(str::to_string),
                    typed(a.attr_type()),
                    false,
                )
            })
            .chain(register.common_attributes().iter().filter(|c| !c.separator).map(|c| {
                FieldDef::new_with_names(
                    c.name.clone(),
                    None,
                    self.resolve_attribute_type(&c.attr_type),
                    false,
                )
            }))
            .collect();

        // Without the chart's ext-dimension count the `Субконто<N>` family cannot be listed
        // exactly, and listing too few would report fields the platform has.
        let ext = ext_count.unwrap_or(0);
        let mut complete = ext_count.is_some() && !register.common_attributes_open();
        let ext_ty = SdblType::Unknown;
        let ext_kind_ty = SdblType::AnyRef;
        let has_corr = correspondence != Some(false);
        let corr_provisional = correspondence.is_none();
        let has_single_account = correspondence != Some(true);
        let single_provisional = correspondence.is_none();

        let mut fields = Vec::new();
        let mut table_resources = Vec::new();

        match table {
            None | Some(VirtualTableType::RecordsWithExtDimensions) => {
                let records = table.is_some();
                fields.push(FieldDef::standard("Период", "Period", SdblType::Date));
                fields.push(FieldDef::standard("Регистратор", "Recorder", SdblType::AnyRef));
                fields.push(FieldDef::standard("НомерСтроки", "LineNumber", SdblType::number()));
                fields.push(FieldDef::standard("Активность", "Active", SdblType::Boolean));
                fields.push(FieldDef::standard("МоментВремени", "PointInTime", SdblType::DateTime));
                if has_corr {
                    for (ru, en) in [("СчетДт", "AccountDr"), ("СчетКт", "AccountCr")] {
                        fields.push(standard(ru, en, account_ty.clone(), corr_provisional));
                    }
                }
                if has_single_account {
                    fields.push(standard(
                        "Счет",
                        "Account",
                        account_ty.clone(),
                        single_provisional,
                    ));
                    fields.push(standard(
                        "ВидДвижения",
                        "RecordType",
                        SdblType::AnyRef,
                        single_provisional,
                    ));
                }
                for member in &dimensions {
                    member.sided(sides(correspondence, member.balance), &mut fields);
                }
                for member in &resources {
                    member.sided(sides(correspondence, member.balance), &mut table_resources);
                }
                fields.extend(attributes.iter().cloned());

                // The records table joins the ext-dimension values in; whether the main table
                // offers them too is not settled by the reference, so there they stay
                // provisional.
                let ext_provisional = !records;
                if has_corr {
                    let p = ext_provisional || corr_provisional;
                    numbered("СубконтоДт", "ExtDimensionDr", ext, &ext_ty, p, &mut fields);
                    numbered("СубконтоКт", "ExtDimensionCr", ext, &ext_ty, p, &mut fields);
                    for (ru, en) in [
                        ("ВидСубконтоДт", "ExtDimensionTypeDr"),
                        ("ВидСубконтоКт", "ExtDimensionTypeCr"),
                    ] {
                        numbered(ru, en, ext, &ext_kind_ty, p, &mut fields);
                    }
                }
                if has_single_account {
                    let p = ext_provisional || single_provisional;
                    numbered("Субконто", "ExtDimension", ext, &ext_ty, p, &mut fields);
                    numbered("ВидСубконто", "ExtDimensionType", ext, &ext_kind_ty, p, &mut fields);
                }
            }
            Some(VirtualTableType::Balance) => {
                fields.push(FieldDef::standard("Счет", "Account", account_ty.clone()));
                numbered("Субконто", "ExtDimension", ext, &ext_ty, false, &mut fields);
                fields.extend(dimensions.iter().map(|member| member.plain(false)));
                for member in &resources {
                    for (ru, en) in [
                        ("Остаток", "Balance"),
                        ("ОстатокДт", "BalanceDr"),
                        ("ОстатокКт", "BalanceCr"),
                        ("РазвернутыйОстатокДт", "ExtendedBalanceDr"),
                        ("РазвернутыйОстатокКт", "ExtendedBalanceCr"),
                    ] {
                        table_resources.push(member.derived(ru, en, member.ty.clone(), false));
                    }
                    split_balance_spellings(member, "", &mut table_resources);
                }
            }
            Some(VirtualTableType::Turnovers) => {
                turnover_period_fields(&mut fields);
                fields.push(FieldDef::standard("Счет", "Account", account_ty.clone()));
                numbered("Субконто", "ExtDimension", ext, &ext_ty, false, &mut fields);
                if has_corr {
                    fields.push(standard(
                        "КорСчет",
                        "BalancedAccount",
                        account_ty.clone(),
                        corr_provisional,
                    ));
                    numbered(
                        "КорСубконто",
                        "BalancedExtDimension",
                        ext,
                        &ext_ty,
                        corr_provisional,
                        &mut fields,
                    );
                }
                for member in &dimensions {
                    fields.push(member.plain(false));
                    match sides(correspondence, member.balance) {
                        Sides::Single => {}
                        Sides::Split => {
                            fields.push(member.derived("Кор", "Balanced", member.ty.clone(), false))
                        }
                        Sides::Unknown => {
                            fields.push(member.derived("Кор", "Balanced", member.ty.clone(), true))
                        }
                    }
                }
                for member in &resources {
                    for (ru, en) in [
                        ("Оборот", "Turnover"),
                        ("ОборотДт", "TurnoverDr"),
                        ("ОборотКт", "TurnoverCr"),
                    ] {
                        table_resources.push(member.derived(ru, en, member.ty.clone(), false));
                    }
                    let correspondent = match sides(correspondence, member.balance) {
                        Sides::Single => None,
                        Sides::Split => Some(false),
                        Sides::Unknown => Some(true),
                    };
                    if let Some(provisional) = correspondent {
                        for (ru, en) in [
                            ("КорОборот", "BalancedTurnover"),
                            ("КорОборотДт", "BalancedTurnoverDr"),
                            ("КорОборотКт", "BalancedTurnoverCr"),
                        ] {
                            table_resources.push(member.derived(
                                ru,
                                en,
                                member.ty.clone(),
                                provisional,
                            ));
                        }
                    }
                }
            }
            Some(VirtualTableType::DrCrTurnovers) => {
                // A register without correspondence has no such table; the missing-table rule
                // does not know the flag, so the field rule must not pretend to.
                complete &= correspondence == Some(true);
                turnover_period_fields(&mut fields);
                fields.push(FieldDef::standard("СчетДт", "AccountDr", account_ty.clone()));
                fields.push(FieldDef::standard("СчетКт", "AccountCr", account_ty.clone()));
                numbered("СубконтоДт", "ExtDimensionDr", ext, &ext_ty, false, &mut fields);
                numbered("СубконтоКт", "ExtDimensionCr", ext, &ext_ty, false, &mut fields);
                for member in &dimensions {
                    member.sided(sides(correspondence, member.balance), &mut fields);
                }
                for member in &resources {
                    match sides(correspondence, member.balance) {
                        Sides::Single => table_resources.push(member.derived(
                            "Оборот",
                            "Turnover",
                            member.ty.clone(),
                            false,
                        )),
                        Sides::Split => {
                            for (ru, en) in [("ОборотДт", "TurnoverDr"), ("ОборотКт", "TurnoverCr")]
                            {
                                table_resources.push(member.derived(
                                    ru,
                                    en,
                                    member.ty.clone(),
                                    false,
                                ));
                            }
                        }
                        Sides::Unknown => {
                            for (ru, en) in [
                                ("Оборот", "Turnover"),
                                ("ОборотДт", "TurnoverDr"),
                                ("ОборотКт", "TurnoverCr"),
                            ] {
                                table_resources.push(member.derived(
                                    ru,
                                    en,
                                    member.ty.clone(),
                                    true,
                                ));
                            }
                        }
                    }
                }
            }
            Some(VirtualTableType::BalanceAndTurnovers) => {
                turnover_period_fields(&mut fields);
                fields.push(FieldDef::standard("Счет", "Account", account_ty.clone()));
                numbered("Субконто", "ExtDimension", ext, &ext_ty, false, &mut fields);
                fields.extend(dimensions.iter().map(|member| member.plain(false)));
                for member in &resources {
                    for (ru, en) in [
                        ("НачальныйОстаток", "OpeningBalance"),
                        ("НачальныйОстатокДт", "OpeningBalanceDr"),
                        ("НачальныйОстатокКт", "OpeningBalanceCr"),
                        ("НачальныйРазвернутыйОстатокДт", "OpeningSplittedBalanceDr"),
                        ("НачальныйРазвернутыйОстатокКт", "OpeningSplittedBalanceCr"),
                        ("Оборот", "Turnover"),
                        ("ОборотДт", "TurnoverDr"),
                        ("ОборотКт", "TurnoverCr"),
                        ("КонечныйОстаток", "ClosingBalance"),
                        ("КонечныйОстатокДт", "ClosingBalanceDr"),
                        ("КонечныйОстатокКт", "ClosingBalanceCr"),
                        ("КонечныйРазвернутыйОстатокДт", "ClosingSplittedBalanceDr"),
                        ("КонечныйРазвернутыйОстатокКт", "ClosingSplittedBalanceCr"),
                    ] {
                        table_resources.push(member.derived(ru, en, member.ty.clone(), false));
                    }
                    split_balance_spellings(member, "Opening", &mut table_resources);
                    split_balance_spellings(member, "Closing", &mut table_resources);
                }
            }
            Some(_) => {
                complete = false;
                fields.extend(dimensions.iter().map(|member| member.plain(false)));
                table_resources.extend(resources.iter().map(|member| member.plain(false)));
                fields.extend(attributes.iter().cloned());
            }
        }

        let dimension_fields: Vec<FieldDef> =
            dimensions.iter().map(|member| member.plain(false)).collect();
        fields.extend(table_resources.iter().cloned());

        tracing::debug!(
            register = register.name(),
            ?table,
            ?correspondence,
            ?ext_count,
            fields = fields.len(),
            complete,
            "built accounting register table"
        );

        ResolvedTable::Register {
            mdo_type: MdoType::AccountingRegister,
            name: register.name().to_string(),
            fields,
            dimensions: dimension_fields,
            resources: table_resources,
            attributes,
            field_model_complete: complete,
        }
    }
}
