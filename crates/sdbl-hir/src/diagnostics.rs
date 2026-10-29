use text_size::TextRange;

use crate::types::SdblType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdblDiagnostic {
    QueryToMissingMetadata {
        table_name: String,
        range: TextRange,
    },

    JoinWithVirtualTable {
        table_name: String,
        virtual_table_type: String,
        range: TextRange,
    },

    VirtualTableCallWithoutParameters {
        table_name: String,
        expected_params: Vec<String>,
        range: TextRange,
    },

    /// A call the platform rejects with «Неверные параметры»: the argument list does not fit
    /// the table's parameter list.
    InvalidVirtualTableParameters {
        table_name: String,
        problem: VirtualTableParameterProblem,
        range: TextRange,
    },

    UnknownField {
        table_name: String,
        field_name: String,
        range: TextRange,
    },

    AmbiguousColumnRef {
        column_name: String,
        possible_tables: Vec<String>,
        range: TextRange,
    },

    /// `Имя.Поле`, where `Имя` names both a source and a field at the same query level.
    ///
    /// A separate variant from [`Self::AmbiguousColumnRef`] because the reader's fix differs:
    /// there a qualifier must be added, here the SOURCE has to be renamed — the qualifier is
    /// already present and is itself the problem.
    AmbiguousQualifiedHead {
        head: String,
        offered_by: Vec<String>,
        range: TextRange,
    },

    TypeMismatch {
        expected: SdblType,
        actual: SdblType,
        context: String,
        range: TextRange,
    },

    UnknownFunction {
        function_name: String,
        range: TextRange,
    },

    InvalidArgumentCount {
        function_name: String,
        expected: String,
        actual: usize,
        range: TextRange,
    },

    AliasRequired {
        name: String,
        range: TextRange,
    },

    DuplicateAlias {
        alias: String,
        range: TextRange,
    },

    FullOuterJoin {
        range: TextRange,
    },

    JoinWithSubQuery {
        range: TextRange,
    },

    AliasWithoutAsKeyword {
        field_name: Option<String>,
        raw_name: Option<String>,
        range: TextRange,
    },

    LogicalOrInWhere {
        range: TextRange,
    },

    LogicalOrInJoin {
        range: TextRange,
    },

    FieldsFromJoinWithoutNullCheck {
        join_type: crate::hir::JoinType,
        range: TextRange,
        unprotected_fields: Vec<UnprotectedFieldRef>,
    },

    /// Schedule values of a `ДанныеГрафика` source read without NULL handling. `range` is the
    /// source's table reference.
    ScheduleDataFieldWithoutNullCheck {
        range: TextRange,
        unprotected_fields: Vec<UnprotectedFieldRef>,
    },

    MultilineString {
        range: TextRange,
    },

    QueryNestedFieldsByDot {
        range: TextRange,
        parts_count: Option<u32>,
    },

    RefOveruse {
        range: TextRange,
    },

    UnionWithoutAll {
        range: TextRange,
    },

    LikeUsage {
        range: TextRange,
        kind: LikeUsageKind,
    },

    SelectTopWithoutOrderBy {
        top_value: u32,
        in_union: bool,
        has_where: bool,
        range: TextRange,
    },

    UnlimitedStringUsage {
        field_name: Option<String>,
        context: UnlimitedStringUsageContext,
        range: TextRange,
    },
}

/// Позиция запроса, в которой платформа запрещает поля неограниченной длины
/// (ошибка исполнения «Нельзя сравнивать поля неограниченной длины…»).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlimitedStringUsageContext {
    Comparison,
    In,
    Between,
    GroupBy,
    OrderBy,
    Distinct,
    TotalsBy,
}

impl UnlimitedStringUsageContext {
    fn describe(self) -> &'static str {
        match self {
            Self::Comparison => "в операции сравнения",
            Self::In => "в операторе В",
            Self::Between => "в операторе МЕЖДУ",
            Self::GroupBy => "в предложении СГРУППИРОВАТЬ ПО",
            Self::OrderBy => "в предложении УПОРЯДОЧИТЬ ПО",
            Self::Distinct => "в выборке с ключевым словом РАЗЛИЧНЫЕ",
            Self::TotalsBy => "в предложении ИТОГИ ПО",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualTableParameterProblem {
    /// More arguments than the table has parameters.
    TooMany { passed: usize, allowed: Vec<String> },
    /// A condition passed in a parameter that takes a value. `position` is 1-based.
    ConditionAsValue { position: usize, slot: String },
}

/// The one wording of an unprotected `ДанныеГрафика` value, shared by the enum rendering and the
/// `ide-diagnostics` handler. It offers no inner join: the NULL comes from an unfilled schedule,
/// not from a join.
pub const SCHEDULE_DATA_NULL_MESSAGE: &str = "Поле данных графика может быть NULL, если график за период не заполнен: добавьте проверку через ЕСТЬ NULL или функцию ЕСТЬNULL";

/// The one wording of a parameter-list error, shared by the enum rendering and the
/// `ide-diagnostics` handler.
pub fn invalid_virtual_table_parameters_message(
    table_name: &str,
    problem: &VirtualTableParameterProblem,
) -> String {
    match problem {
        VirtualTableParameterProblem::TooMany { passed, allowed } => format!(
            "Неверные параметры виртуальной таблицы \"{table_name}\": передано {passed}, а у \
             таблицы {} ({}). Условия внутри одного параметра соединяются через И, а не запятой",
            allowed.len(),
            allowed.join(", "),
        ),
        VirtualTableParameterProblem::ConditionAsValue { position, slot } => format!(
            "Неверные параметры виртуальной таблицы \"{table_name}\": в параметр {position} \
             ({slot}) передано условие, а он принимает значение. Проверьте позицию аргумента: \
             параметры читаются строго по порядку"
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LikeUsageKind {
    Allowed,
    Incorrect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnprotectedFieldRef {
    pub table_alias: String,
    pub field_name: String,
    pub range: TextRange,
}

impl SdblDiagnostic {
    pub fn code(&self) -> Option<u32> {
        match self {
            Self::QueryToMissingMetadata { .. } => Some(122),
            Self::JoinWithVirtualTable { .. } => Some(79),
            Self::VirtualTableCallWithoutParameters { .. } => Some(174),
            Self::QueryNestedFieldsByDot { .. } => None,
            Self::RefOveruse { .. } => None,
            Self::LikeUsage { .. } => None,
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::QueryToMissingMetadata { table_name, .. } => {
                format!("В конфигурации нет объекта метаданных для источника '{}'", table_name)
            }
            Self::JoinWithVirtualTable { table_name, virtual_table_type, .. } => {
                format!(
                    "Не рекомендуется использовать соединение с виртуальной таблицей '{}' ({})",
                    table_name, virtual_table_type
                )
            }
            Self::VirtualTableCallWithoutParameters { table_name, expected_params, .. } => {
                format!(
                    "Виртуальная таблица '{}' требует параметры: {}",
                    table_name,
                    expected_params.join(", ")
                )
            }
            Self::UnknownField { table_name, field_name, .. } => {
                format!("Поле '{}' не найдено в таблице '{}'", field_name, table_name)
            }
            Self::InvalidVirtualTableParameters { table_name, problem, .. } => {
                invalid_virtual_table_parameters_message(table_name, problem)
            }
            Self::AmbiguousQualifiedHead { head, offered_by, .. } => {
                ambiguous_qualified_head_message(head, offered_by)
            }
            Self::AmbiguousColumnRef { column_name, possible_tables, .. } => {
                format!(
                    "Неоднозначная ссылка на колонку '{}'. Возможные таблицы: {}",
                    column_name,
                    possible_tables.join(", ")
                )
            }
            Self::TypeMismatch { expected, actual, context, .. } => {
                format!(
                    "Несоответствие типов в {}: ожидается {}, получен {}",
                    context, expected, actual
                )
            }
            Self::UnknownFunction { function_name, .. } => {
                format!("Неизвестная функция '{}'", function_name)
            }
            Self::InvalidArgumentCount { function_name, expected, actual, .. } => {
                format!(
                    "Функция '{}' ожидает {} аргументов, передано {}",
                    function_name, expected, actual
                )
            }
            Self::AliasRequired { name, .. } => {
                format!("Для поля '{}' требуется псевдоним (используйте AS)", name)
            }
            Self::DuplicateAlias { alias, .. } => {
                format!(
                    "Псевдоним '{}' уже занят другим источником запроса: ссылки по этому имени \
                     разрешаются в последний источник",
                    alias
                )
            }
            Self::FullOuterJoin { .. } => {
                "Использование FULL OUTER JOIN значительно снижает производительность запроса. \
                 Рассмотрите возможность переписать с использованием UNION и LEFT JOIN"
                    .to_string()
            }
            Self::JoinWithSubQuery { .. } => "Не используйте соединение с подзапросами. \
                 Соединения с подзапросами вызывают серьезные проблемы с производительностью"
                .to_string(),
            Self::AliasWithoutAsKeyword { field_name, .. } => {
                if let Some(name) = field_name {
                    format!("Поле '{}' должно иметь явный псевдоним с ключевым словом AS/КАК", name)
                } else {
                    "Поле в подзапросе должно иметь псевдоним с ключевым словом AS/КАК".to_string()
                }
            }
            Self::LogicalOrInWhere { .. } => {
                "Использование оператора ИЛИ (OR) в условии WHERE существенно снижает производительность запроса. \
                 Рассмотрите возможность переписать с использованием UNION или изменить структуру условий"
                    .to_string()
            }
            Self::LogicalOrInJoin { .. } => {
                "Использование ИЛИ (OR) в условии соединения приводит к низкой производительности запроса"
                    .to_string()
            }
            Self::FieldsFromJoinWithoutNullCheck { join_type, .. } => {
                let join_str = match join_type {
                    crate::hir::JoinType::Left => "ЛЕВОГО СОЕДИНЕНИЯ",
                    crate::hir::JoinType::Right => "ПРАВОГО СОЕДИНЕНИЯ",
                    crate::hir::JoinType::Full => "ПОЛНОГО СОЕДИНЕНИЯ",
                    _ => "СОЕДИНЕНИЯ",
                };
                format!(
                    "Для полей из {} добавьте проверку через ЕСТЬ NULL или используйте функцию ЕСТЬNULL, либо замените на ВНУТРЕННЕЕ СОЕДИНЕНИЕ",
                    join_str
                )
            }
            Self::ScheduleDataFieldWithoutNullCheck { .. } => SCHEDULE_DATA_NULL_MESSAGE.to_string(),
            Self::MultilineString { .. } => {
                "Строковый литерал запроса может продолжаться на следующей строке: перенос \
                 строки войдёт в значение литерала"
                    .to_string()
            }
            Self::QueryNestedFieldsByDot { .. } => {
                "Поле, полученное через точку от ссылочного поля, добавляет в запрос неявное \
                 соединение с таблицей, на которую указывает ссылка"
                    .to_string()
            }
            Self::RefOveruse { .. } => {
                "Поле «Ссылка», полученное через точку от ссылочного поля, совпадает с самим \
                 этим полем: лишнее соединение можно убрать"
                    .to_string()
            }
            Self::UnionWithoutAll { .. } => {
                "Использование ключевого слова ОБЪЕДИНИТЬ без ВСЕ приводит к \
                 излишней обработке для удаления дубликатов. Используйте ОБЪЕДИНИТЬ ВСЕ"
                    .to_string()
            }
            Self::LikeUsage { kind: LikeUsageKind::Allowed, .. } => {
                "Шаблон ПОДОБНО должен одинаково работать во всех СУБД: используйте только \
                 «%» и «_», а спецсимволы искомого текста экранируйте через СПЕЦСИМВОЛ"
                    .to_string()
            }
            Self::LikeUsage { kind: LikeUsageKind::Incorrect, .. } => {
                "Шаблон ПОДОБНО задаётся строковым литералом или параметром запроса, а не \
                 вычисляется в запросе"
                    .to_string()
            }
            Self::SelectTopWithoutOrderBy { .. } => {
                "Измените запрос, добавив сортировку".to_string()
            }
            Self::UnlimitedStringUsage { field_name, context, .. } => {
                let subject = match field_name {
                    Some(name) => format!("Поле неограниченной длины \"{}\"", name),
                    None => "Значение типа \"строка неограниченной длины\"".to_string(),
                };
                format!(
                    "{} нельзя использовать {}. Приведите значение к ограниченной длине: ВЫРАЗИТЬ(... КАК СТРОКА(N))",
                    subject,
                    context.describe()
                )
            }
        }
    }

    pub fn range(&self) -> TextRange {
        match self {
            Self::QueryToMissingMetadata { range, .. } => *range,
            Self::JoinWithVirtualTable { range, .. } => *range,
            Self::VirtualTableCallWithoutParameters { range, .. } => *range,
            Self::InvalidVirtualTableParameters { range, .. } => *range,
            Self::UnknownField { range, .. } => *range,
            Self::AmbiguousColumnRef { range, .. } => *range,
            Self::AmbiguousQualifiedHead { range, .. } => *range,
            Self::TypeMismatch { range, .. } => *range,
            Self::UnknownFunction { range, .. } => *range,
            Self::InvalidArgumentCount { range, .. } => *range,
            Self::AliasRequired { range, .. } => *range,
            Self::DuplicateAlias { range, .. } => *range,
            Self::FullOuterJoin { range, .. } => *range,
            Self::JoinWithSubQuery { range, .. } => *range,
            Self::AliasWithoutAsKeyword { range, .. } => *range,
            Self::LogicalOrInWhere { range } => *range,
            Self::LogicalOrInJoin { range } => *range,
            Self::FieldsFromJoinWithoutNullCheck { range, .. } => *range,
            Self::ScheduleDataFieldWithoutNullCheck { range, .. } => *range,
            Self::MultilineString { range } => *range,
            Self::QueryNestedFieldsByDot { range, .. } => *range,
            Self::RefOveruse { range } => *range,
            Self::UnionWithoutAll { range } => *range,
            Self::LikeUsage { range, .. } => *range,
            Self::SelectTopWithoutOrderBy { range, .. } => *range,
            Self::UnlimitedStringUsage { range, .. } => *range,
        }
    }

    pub fn is_error(&self) -> bool {
        match self {
            Self::QueryToMissingMetadata { .. } => true,
            Self::UnknownField { .. } => true,
            Self::InvalidVirtualTableParameters { .. } => true,
            Self::AmbiguousColumnRef { .. } => true,
            Self::AmbiguousQualifiedHead { .. } => true,
            Self::TypeMismatch { .. } => true,
            Self::UnknownFunction { .. } => true,
            Self::InvalidArgumentCount { .. } => true,

            Self::JoinWithVirtualTable { .. } => false,
            Self::VirtualTableCallWithoutParameters { .. } => false,
            Self::AliasRequired { .. } => false,
            Self::DuplicateAlias { .. } => true,
            Self::FullOuterJoin { .. } => false,
            Self::JoinWithSubQuery { .. } => false,
            Self::AliasWithoutAsKeyword { .. } => false,
            Self::LogicalOrInWhere { .. } => false,
            Self::LogicalOrInJoin { .. } => false,
            Self::FieldsFromJoinWithoutNullCheck { .. } => false,
            Self::ScheduleDataFieldWithoutNullCheck { .. } => false,
            Self::MultilineString { .. } => false,
            Self::QueryNestedFieldsByDot { .. } => false,
            Self::RefOveruse { .. } => false,
            Self::UnionWithoutAll { .. } => false,
            Self::LikeUsage { kind: LikeUsageKind::Allowed, .. } => false,
            Self::LikeUsage { kind: LikeUsageKind::Incorrect, .. } => true,
            Self::SelectTopWithoutOrderBy { .. } => false,
            Self::UnlimitedStringUsage { .. } => true,
        }
    }
}

/// The one wording for a colliding qualifier head, shared by the enum rendering and by the
/// `ide-diagnostics` handler: two copies of it had already drifted apart.
///
/// Names every source that offers the name, including the alias itself. A message that
/// mentioned only the self-collision hid the fact that a second, independent source carries
/// the same field.
pub fn ambiguous_qualified_head_message(head: &str, offered_by: &[String]) -> String {
    // The query language is case-insensitive, so the spelling at the reference site and the
    // spelling in `КАК ...` may differ; ASCII folding would call the same source a foreign one.
    let mut own = false;
    let mut others: Vec<&str> = Vec::new();
    for source in offered_by {
        if stdx::case::eq_ignore_case(source, head) {
            own = true;
        } else {
            others.push(source);
        }
    }
    let others = others.join(", ");

    let where_ = if own && others.is_empty() {
        "того же источника".to_string()
    } else if own {
        format!("того же источника, а также источника {others}")
    } else {
        format!("источника {others}")
    };

    format!(
        "Имя \"{head}\" — и псевдоним источника, и поле {where_}. Платформа отклонит такой \
         запрос: переименуйте источник"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostic_codes() {
        let diag = SdblDiagnostic::QueryToMissingMetadata {
            table_name: "Test".to_string(),
            range: syntax::MODULE_RANGE,
        };
        assert_eq!(diag.code(), Some(122));

        let diag = SdblDiagnostic::JoinWithVirtualTable {
            table_name: "Test".to_string(),
            virtual_table_type: "СрезПоследних".to_string(),
            range: syntax::MODULE_RANGE,
        };
        assert_eq!(diag.code(), Some(79));
    }

    #[test]
    fn test_diagnostic_messages() {
        let diag = SdblDiagnostic::UnknownField {
            table_name: "Справочник.Валюты".to_string(),
            field_name: "НесуществующееПоле".to_string(),
            range: syntax::MODULE_RANGE,
        };
        assert!(diag.message().contains("НесуществующееПоле"));
        assert!(diag.message().contains("Справочник.Валюты"));
    }
}
