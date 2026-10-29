use bsl_metadata::MdoType;
use std::fmt;
use stdx::case::CaseExt;

/// Every table a register exposes besides its main one, by the name the query writes after
/// `<Вид>.<Имя>.`. Names follow the 1C:Enterprise syntax assistant («Работа с запросами →
/// Таблицы запросов»); which kind of register owns which table is decided by
/// [`register_table_kinds`], not by this list.
const REGISTER_TABLES: &[(&str, &str, VirtualTableType)] = &[
    ("срезпоследних", "slicelast", VirtualTableType::SliceLast),
    ("срезпервых", "slicefirst", VirtualTableType::SliceFirst),
    ("остатки", "balance", VirtualTableType::Balance),
    ("обороты", "turnovers", VirtualTableType::Turnovers),
    ("остаткииобороты", "balanceandturnovers", VirtualTableType::BalanceAndTurnovers),
    ("оборотыдткт", "drcrturnovers", VirtualTableType::DrCrTurnovers),
    ("движенияссубконто", "recordswithextdimensions", VirtualTableType::RecordsWithExtDimensions),
    ("субконто", "extdimensions", VirtualTableType::ExtDimensions),
    ("данныеграфика", "scheduledata", VirtualTableType::ScheduleData),
    (
        "фактическийпериоддействия",
        "adjustedeffectiveperiod",
        VirtualTableType::AdjustedEffectivePeriod,
    ),
    ("изменения", "changes", VirtualTableType::Changes),
];

/// Period kinds a `ДанныеГрафика` value field ends with, already folded. The query field is the
/// schedule resource name glued to one of them (`ЧасыФактическийПериодДействия`); the model does
/// not read the register's schedule, so the resource name itself is unknown here.
const SCHEDULE_DATA_VALUE_SUFFIXES: &[(&str, &str)] = &[
    ("периоддействия", "actionperiod"),
    ("фактическийпериоддействия", "actualactionperiod"),
    ("периодрегистрации", "registrationperiod"),
    ("базовыйпериод", "baseperiod"),
];

/// Whether a field of `ДанныеГрафика` is a schedule value, which is NULL when the schedule has
/// no data for the period. The bare period name is a field of the register itself, so the
/// resource prefix must not be empty.
pub fn is_schedule_data_value_field(name: &str) -> bool {
    let name = name.fold_lower();
    SCHEDULE_DATA_VALUE_SUFFIXES.iter().any(|(ru, en)| {
        [*ru, *en].into_iter().any(|suffix| name.len() > suffix.len() && name.ends_with(suffix))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualTableType {
    SliceLast,
    SliceFirst,
    Balance,
    Turnovers,
    BalanceAndTurnovers,
    DrCrTurnovers,
    RecordsWithExtDimensions,
    /// The ext-dimension values table of an accounting register. A stored table, not a computed
    /// one: it takes no parameters.
    ExtDimensions,
    ScheduleData,
    AdjustedEffectivePeriod,
    /// The change-registration table. Stored too, and parameterless.
    Changes,
}

impl VirtualTableType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SliceLast => "срезпоследних",
            Self::SliceFirst => "срезпервых",
            Self::Balance => "остатки",
            Self::Turnovers => "обороты",
            Self::BalanceAndTurnovers => "остаткииобороты",
            Self::DrCrTurnovers => "оборотыдткт",
            Self::RecordsWithExtDimensions => "движенияссубконто",
            Self::ExtDimensions => "субконто",
            Self::ScheduleData => "данныеграфика",
            Self::AdjustedEffectivePeriod => "фактическийпериоддействия",
            Self::Changes => "изменения",
        }
    }

    pub fn has_periodicity(self) -> bool {
        matches!(self, Self::Turnovers | Self::BalanceAndTurnovers | Self::DrCrTurnovers)
    }

    /// A computed table, called with a parameter list. The stored subordinate tables share the
    /// `<Вид>.<Имя>.<Таблица>` path shape but are read like any other table, so the rules about
    /// virtual-table calls must not reach them.
    pub fn takes_parameters(self) -> bool {
        !matches!(self, Self::ExtDimensions | Self::Changes)
    }
}

impl fmt::Display for VirtualTableType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The table a suffix names, judged by the name alone. Only for callers that do not know the
/// root; a table path is classified by [`register_table_of_path`].
pub fn virtual_table_type(name: &str) -> Option<VirtualTableType> {
    let name_lower = name.fold_lower();
    REGISTER_TABLES
        .iter()
        .find(|(ru, en, _)| *ru == name_lower || *en == name_lower)
        .map(|(_, _, table)| *table)
}

/// The tables each register kind owns besides its main table.
///
/// A calculation register also owns `База<Регистр>` tables and one table per recalculation,
/// both named by the configuration rather than the platform; [`register_has_open_table_set`]
/// is what keeps the missing-table rule away from them.
pub fn register_table_kinds(mdo_type: MdoType) -> &'static [VirtualTableType] {
    use VirtualTableType::*;
    match mdo_type {
        MdoType::InformationRegister => &[SliceLast, SliceFirst, Changes],
        MdoType::AccumulationRegister => &[Balance, Turnovers, BalanceAndTurnovers, Changes],
        MdoType::AccountingRegister => &[
            Balance,
            Turnovers,
            DrCrTurnovers,
            BalanceAndTurnovers,
            RecordsWithExtDimensions,
            ExtDimensions,
            Changes,
        ],
        MdoType::CalculationRegister => &[ScheduleData, AdjustedEffectivePeriod, Changes],
        _ => &[],
    }
}

/// Whether a register kind may own tables whose names only the configuration knows.
pub fn register_has_open_table_set(mdo_type: MdoType) -> bool {
    mdo_type == MdoType::CalculationRegister
}

/// The register table a `<Вид>.<Имя>.<Таблица>` path names, if the path has that shape and the
/// kind really owns such a table.
///
/// Judging by the last segment alone is what this replaces: a document's tabular section named
/// `Остатки` read as a balance table and drew the virtual-table rules onto a stored table.
pub fn register_table_of_path<S: AsRef<str>>(parts: &[S]) -> Option<(MdoType, VirtualTableType)> {
    let [root, _, table] = parts else {
        return None;
    };
    let mdo_type = root.as_ref().parse::<MdoType>().ok()?;
    let table = virtual_table_type(table.as_ref())?;
    register_table_kinds(mdo_type).contains(&table).then_some((mdo_type, table))
}

/// Whether a path ends in a computed table that is called with parameters.
pub fn is_parameterized_virtual_table<S: AsRef<str>>(parts: &[S]) -> bool {
    register_table_of_path(parts).is_some_and(|(_, table)| table.takes_parameters())
}

/// What a virtual-table parameter must be, as far as the text can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterSlot {
    /// A value: a date or boundary, a list of ext-dimension kinds, a row count.
    Value(&'static str),
    /// A query-language condition.
    Condition(&'static str),
    /// A keyword or construct with a grammar of its own (periodicity, supplement method, order).
    Construct(&'static str),
}

impl ParameterSlot {
    pub fn name(self) -> &'static str {
        match self {
            Self::Value(name) | Self::Condition(name) | Self::Construct(name) => name,
        }
    }
}

/// The parameter list of a computed register table, in the order the platform reads it
/// (ITS «Язык запросов 1С:Предприятия», гл. 3: «Параметры следует задавать строго в порядке их
/// описания»). `None` where the list is not modelled — the parameter rules stay silent there.
pub fn virtual_table_parameters(
    mdo_type: MdoType,
    table: VirtualTableType,
) -> Option<&'static [ParameterSlot]> {
    use ParameterSlot::{Condition as C, Construct as K, Value as V};
    use VirtualTableType::*;
    let slots: &'static [ParameterSlot] = match (mdo_type, table) {
        (MdoType::InformationRegister, SliceLast | SliceFirst) => &[V("Период"), C("Условие")],
        (MdoType::AccumulationRegister, Balance) => &[V("Период"), C("Условие")],
        (MdoType::AccumulationRegister, Turnovers) => {
            &[V("НачалоПериода"), V("КонецПериода"), K("Периодичность"), C("Условие")]
        }
        (MdoType::AccumulationRegister, BalanceAndTurnovers) => &[
            V("НачалоПериода"),
            V("КонецПериода"),
            K("Периодичность"),
            K("МетодДополнения"),
            C("Условие"),
        ],
        (MdoType::AccountingRegister, Balance) => {
            &[V("Период"), C("УсловиеСчета"), V("Субконто"), C("Условие")]
        }
        (MdoType::AccountingRegister, Turnovers) => &[
            V("НачалоПериода"),
            V("КонецПериода"),
            K("Периодичность"),
            C("УсловиеСчета"),
            V("Субконто"),
            C("Условие"),
            C("УсловиеКорСчета"),
            V("КорСубконто"),
        ],
        (MdoType::AccountingRegister, DrCrTurnovers) => &[
            V("НачалоПериода"),
            V("КонецПериода"),
            K("Периодичность"),
            C("УсловиеСчетаДт"),
            V("СубконтоДт"),
            C("УсловиеСчетаКт"),
            V("СубконтоКт"),
            C("Условие"),
        ],
        (MdoType::AccountingRegister, BalanceAndTurnovers) => &[
            V("НачалоПериода"),
            V("КонецПериода"),
            K("Периодичность"),
            K("МетодДополнения"),
            C("УсловиеСчета"),
            V("Субконто"),
            C("Условие"),
        ],
        (MdoType::AccountingRegister, RecordsWithExtDimensions) => {
            &[V("НачалоПериода"), V("КонецПериода"), C("Условие"), K("Порядок"), V("Первые")]
        }
        _ => return None,
    };
    Some(slots)
}

pub const PERIODICITY_VALUES: &[(&str, &str)] = &[
    ("авто", "auto"),
    ("год", "year"),
    ("полугодие", "halfyear"),
    ("квартал", "quarter"),
    ("месяц", "month"),
    ("декада", "tendays"),
    ("неделя", "week"),
    ("день", "day"),
    ("секунда", "second"),
    ("запись", "record"),
];

pub fn is_periodicity_value(name: &str) -> bool {
    let name_lower = name.fold_lower();
    PERIODICITY_VALUES.iter().any(|(ru, en)| *ru == name_lower || *en == name_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_table_detection() {
        assert!(virtual_table_type("СрезПоследних").is_some());
        assert!(virtual_table_type("срезпоследних").is_some());
        assert!(virtual_table_type("SliceLast").is_some());
        assert!(virtual_table_type("slicelast").is_some());
        assert!(virtual_table_type("Остатки").is_some());
        assert!(virtual_table_type("Balance").is_some());
        assert!(virtual_table_type("Изменения").is_some());
        assert!(virtual_table_type("Changes").is_some());

        assert!(virtual_table_type("Справочник").is_none());
        assert!(virtual_table_type("Random").is_none());
    }

    /// The classification follows the root: the same suffix on a kind that has no such table is
    /// a stored table of that object, not a virtual one.
    #[test]
    fn a_table_suffix_is_virtual_only_under_a_register_that_owns_it() {
        assert_eq!(
            register_table_of_path(&["РегистрБухгалтерии", "Бухгалтерия1", "ОборотыДтКт"]),
            Some((MdoType::AccountingRegister, VirtualTableType::DrCrTurnovers)),
        );
        assert_eq!(
            register_table_of_path(&[
                "AccountingRegister",
                "Бухгалтерия1",
                "RecordsWithExtDimensions"
            ]),
            Some((MdoType::AccountingRegister, VirtualTableType::RecordsWithExtDimensions)),
        );
        assert!(register_table_of_path(&["Документ", "Документ1", "Остатки"]).is_none());
        assert!(register_table_of_path(&["РегистрНакопления", "Остатки1", "ОборотыДтКт"]).is_none());
        assert!(register_table_of_path(&["РегистрСведений", "Сведения1", "Остатки"]).is_none());
        assert!(is_parameterized_virtual_table(&["РегистрНакопления", "Остатки1", "Остатки"]));
        assert!(!is_parameterized_virtual_table(&[
            "РегистрБухгалтерии",
            "Бухгалтерия1",
            "Субконто"
        ]));
        assert!(!is_parameterized_virtual_table(&["РегистрСведений", "Сведения1", "Изменения"]));
    }

    #[test]
    fn every_parameterized_register_table_is_either_modelled_or_deliberately_open() {
        for mdo_type in [
            MdoType::InformationRegister,
            MdoType::AccumulationRegister,
            MdoType::AccountingRegister,
        ] {
            for table in register_table_kinds(mdo_type) {
                assert_eq!(
                    virtual_table_parameters(mdo_type, *table).is_some(),
                    table.takes_parameters(),
                    "{mdo_type:?}.{table:?}",
                );
            }
        }
    }

    #[test]
    fn test_virtual_table_type() {
        assert_eq!(virtual_table_type("СрезПоследних"), Some(VirtualTableType::SliceLast));
        assert_eq!(virtual_table_type("SliceLast"), Some(VirtualTableType::SliceLast));
        assert_eq!(virtual_table_type("Остатки"), Some(VirtualTableType::Balance));
        assert_eq!(virtual_table_type("Balance"), Some(VirtualTableType::Balance));
        assert_eq!(virtual_table_type("Обороты"), Some(VirtualTableType::Turnovers));
        assert_eq!(virtual_table_type("Turnovers"), Some(VirtualTableType::Turnovers));
        assert_eq!(virtual_table_type("Unknown"), None);
    }

    #[test]
    fn test_virtual_table_type_display() {
        assert_eq!(VirtualTableType::SliceLast.to_string(), "срезпоследних");
        assert_eq!(VirtualTableType::Balance.to_string(), "остатки");
        assert_eq!(VirtualTableType::Turnovers.to_string(), "обороты");
    }

    #[test]
    fn test_periodicity_values() {
        assert!(is_periodicity_value("Авто"));
        assert!(is_periodicity_value("авто"));
        assert!(is_periodicity_value("Auto"));
        assert!(is_periodicity_value("День"));
        assert!(is_periodicity_value("Day"));
        assert!(is_periodicity_value("Месяц"));
        assert!(is_periodicity_value("Month"));
        assert!(is_periodicity_value("Запись"));
        assert!(is_periodicity_value("Record"));

        assert!(!is_periodicity_value("Партнер"));
        assert!(!is_periodicity_value("Random"));
    }

    #[test]
    fn test_has_periodicity() {
        assert!(VirtualTableType::Turnovers.has_periodicity());
        assert!(VirtualTableType::BalanceAndTurnovers.has_periodicity());
        assert!(VirtualTableType::DrCrTurnovers.has_periodicity());
        assert!(!VirtualTableType::Balance.has_periodicity());
        assert!(!VirtualTableType::SliceLast.has_periodicity());
        assert!(!VirtualTableType::Changes.has_periodicity());
    }
}
