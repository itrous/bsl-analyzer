# Calculation register virtual table without a recorder filter (CalculationRegisterVirtualTableWithoutRecorder)

## Description

The calculation register virtual tables `ScheduleData`, `AdjustedEffectivePeriod` and `Base<Register>` without a filter are built over every record of the register, and the platform computes values for each of them. A calculation needs only the records of the record set being calculated, so the table's condition parameter filters by the recorder.

The diagnostic reports a call whose condition parameter has no filter by `Recorder` (`Регистратор`). The condition parameter is the first parameter of `ScheduleData` and `AdjustedEffectivePeriod` and the fourth of `Base<Register>`.

The rule is a heuristic: it does not know whether the place calculates its own record set. That is why the default level is a warning. Reading an employee's history across other documents is legitimate, and a filter by the document's own recorder would change the result of such a query: add it only where the document's own record set is really being calculated.

A filter is:

- a comparison `Recorder = <expression>`, either way round;
- `Recorder IN (<list>)`;
- such a comparison as a conjunct of an `AND` condition, parentheses included.

An explicit exclusion `Recorder <> <expression>`, either way round, alone or under `AND` and parentheses, is not reported either: it shows that the records of other documents are wanted on purpose.

Not a filter: a condition in the query's `WHERE`, a filter by another field, the comparisons `<`, `>`, `<=`, `>=`, `NOT`, `NOT IN`, `IN HIERARCHY`, a comparison only under `OR` (including `Recorder = &A OR Recorder = &B`), a dotted field such as `Recorder.Number`.

A table already reported by `VirtualTableCallWithoutParameters` is not reported again: the fix there is to move the conditions into the table's parameters, not to add a recorder. The recorder message does not come back when `VirtualTableCallWithoutParameters` is turned off in the settings. `VirtualTableCallWithoutParameters` does not check a `Base<Register>` table, so one without a condition in its fourth parameter is reported even with empty parentheses.

An argument list the module completes at run time (`"…ScheduleData(" + Condition + ")"`) and a condition given only as a data-composition extension `{...}` are not judged: the condition is unknown, not absent.

A `Base<Register>` table is recognised by a name starting with `Base` (`База`). A recalculation named that way is checked as a base table too.

## Parameters

- `onlyInModulesWritingRegister` — when `true`, a report in a module stays only if the module writes the same register: it contains `RegisterRecords.<Name>` or `Движения.<Регистр>` (on any receiver, such as `ThisObject.RegisterRecords.<Name>`). Reports, which do not need a recorder filter, are then left alone. `RegisterRecords["<Name>"]` and a mention in a comment are not a write; preprocessor branches are not evaluated, and a write made some other way (through a common module, for example) is not recognised. A query text without a module is not narrowed. Default `true`; `false` brings back the check of every module — only the query text decides.

## Examples

### Reported in a module that writes the register

```bsl
RegisterRecords.Accruals.Write();

Query.Text =
"SELECT
|	ScheduleData.LineNumber AS LineNumber
|FROM
|	CalculationRegister.Accruals.ScheduleData(CalculationType = &CalculationType) AS ScheduleData";
```

### Preferred when calculating the document's own record set

```bsl
Query.Text =
"SELECT
|	ScheduleData.LineNumber AS LineNumber
|FROM
|	CalculationRegister.Accruals.ScheduleData(Recorder = &Recorder) AS ScheduleData";
```

### An employee's history across other documents

A module that does not write the register gets no report. In a module that writes it such a query gets a warning, because the rule cannot tell it from a calculation of the document's own set; where the document itself is not wanted, an explicit exclusion says so and removes the report. A filter by the document's own recorder does not belong in such a query.

```bsl
Query.Text =
"SELECT
|	T.Result AS Result
|FROM
|	CalculationRegister.Accruals.AdjustedEffectivePeriod(
|			Employee = &Employee
|				AND Recorder <> &Ref) AS T";
```

## Sources

There is no direct 1C standard for the recorder filter. The requirement is an item of the error list of the «1C:Specialist in the platform» exam, lines 125–126: «Missing virtual table parameter values or a WHERE condition used instead of them». The standard «Accessing virtual tables» requires passing conditions in the table's parameters; it does not forbid reading the records of other documents and says nothing about the recorder.

* Source: [ATT83PL — «1C:Specialist in the platform» exam requirements (RU)](https://static.1c.ru/rus/partners/training/files/ATT83PL.rtf)

Related public context:

* [Standard: Accessing virtual tables (RU)](https://its.1c.ru/db/v8std#content:657:hdoc)
