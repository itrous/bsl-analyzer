# Calculation register virtual table without a recorder filter (CalculationRegisterVirtualTableWithoutRecorder)

## Description

The calculation register virtual tables `ScheduleData`, `AdjustedEffectivePeriod` and `Base<Register>` without a filter are built over every record of the register, and the platform computes values for each of them. A calculation needs only the records of the record set being calculated, so the table's condition parameter filters by the recorder.

The diagnostic reports a call whose condition parameter has no filter by `Recorder` (`Регистратор`). The condition parameter is the first parameter of `ScheduleData` and `AdjustedEffectivePeriod` and the fourth of `Base<Register>`.

A filter is:

- a comparison `Recorder = <expression>`, either way round;
- `Recorder IN (<list>)`;
- such a comparison as a conjunct of an `AND` condition, parentheses included.

Not a filter: a condition in the query's `WHERE`, a filter by another field, `<>` and other comparisons, `NOT`, `IN HIERARCHY`, a comparison only under `OR` (including `Recorder = &A OR Recorder = &B`), a dotted field such as `Recorder.Number`.

An argument list the module completes at run time (`"…ScheduleData(" + Condition + ")"`) and a condition given only as a data-composition extension `{...}` are not judged: the condition is unknown, not absent.

A `Base<Register>` table is recognised by a name starting with `Base` (`База`). A recalculation named that way is checked as a base table too.

## Parameters

- `onlyInModulesWritingRegister` — when `true`, a report in a module stays only if the module writes the same register: it contains `RegisterRecords.<Name>` or `Движения.<Регистр>` (on any receiver, such as `ThisObject.RegisterRecords.<Name>`). Reports, which do not need a recorder filter, are then left alone. `RegisterRecords["<Name>"]` and a mention in a comment are not a write; preprocessor branches are not evaluated. A query text without a module is not narrowed. Default `false` — only the query text decides.

## Examples

### Reported

```bsl
Query.Text =
"SELECT
|	ScheduleData.LineNumber AS LineNumber
|FROM
|	CalculationRegister.Accruals.ScheduleData(CalculationType = &CalculationType) AS ScheduleData";
```

### Preferred

```bsl
Query.Text =
"SELECT
|	ScheduleData.LineNumber AS LineNumber
|FROM
|	CalculationRegister.Accruals.ScheduleData(Recorder = &Recorder) AS ScheduleData";
```

## Sources

There is no direct 1C standard for the recorder filter. The requirement is an item of the error list of the «1C:Specialist in the platform» exam, lines 125–126: «Missing virtual table parameter values or a WHERE condition used instead of them».

* Source: [ATT83PL — «1C:Specialist in the platform» exam requirements (RU)](https://static.1c.ru/rus/partners/training/files/ATT83PL.rtf)

Related public context:

* [Standard: Accessing virtual tables (RU)](https://its.1c.ru/db/v8std#content:657:hdoc)
