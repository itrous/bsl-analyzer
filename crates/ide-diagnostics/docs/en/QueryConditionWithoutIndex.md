# A condition in virtual table parameters does not rely on an index (QueryConditionWithoutIndex)

## Description

This diagnostic reports a condition in the parameters of an accumulation register virtual
table (`Balance`, `Turnovers`, `BalanceAndTurnovers`) whose fields are not covered by an
index of the totals table.

For the totals tables of an accumulation register the platform builds a main index: the
dimensions in configuration order. A dimension marked as indexed in metadata additionally
gets an index of its own: that dimension first, then all the other dimensions in
configuration order. A condition uses an index only when its fields are exactly the leading
fields of one of those indexes. Otherwise the totals table is read by a full scan.

It fires when all of the following hold:

- the condition in the virtual table parameters of an accumulation register is a
  conjunction (`AND`) of equality comparisons `<field> = <expression>`; the field is written
  bare or qualified by the source alias;
- every field of the condition is a dimension of this register;
- no index starts with exactly the set of dimensions the condition names.

The period is not part of the test: a query without a period whose condition names the
leading dimensions is considered optimal.

The message names the first dimension, in configuration order, that is absent from the
condition while a later one is present. The text is:

```text
A condition in virtual table parameters does not rely on an index: the condition fields must be the leading fields of the register's dimension order, with no gaps. Dimension "<name>" is missing
```

It does not fire:

- on conditions the rule cannot judge: `OR`, `IN`, `NOT`, comparisons `>`, `<`, `>=`, `<=`,
  `<>`, `BETWEEN`, `IS NULL`, `CASE`, a nested query, a function of the field;
- on a condition that names a field which is not a dimension (a resource, an attribute, an
  unknown name);
- on an empty condition parameter;
- on information, accounting and calculation registers;
- on conditions in `WHERE` and in the `ON` of a join.

## Examples

Incorrect (register `ТоварыНаСкладах` with dimensions `Склад`, `Номенклатура`, `Качество`,
none of them indexed):

```bsl
SELECT
    Balance.КоличествоОстаток AS Quantity
FROM
    AccumulationRegister.ТоварыНаСкладах.Balance(, Номенклатура = &Номенклатура) AS Balance
```

```bsl
SELECT
    Balance.КоличествоОстаток AS Quantity
FROM
    AccumulationRegister.ТоварыНаСкладах.Balance(, Склад = &Склад AND Качество = &Качество) AS Balance
```

Correct (a condition on the leading dimensions with no gaps):

```bsl
SELECT
    Balance.КоличествоОстаток AS Quantity
FROM
    AccumulationRegister.ТоварыНаСкладах.Balance(, Склад = &Склад AND Номенклатура = &Номенклатура) AS Balance
```

Correct (the dimension `Номенклатура` is marked as indexed: a condition on it together with
the leading dimensions of the rest):

```bsl
SELECT
    Balance.КоличествоОстаток AS Quantity
FROM
    AccumulationRegister.ТоварыНаСкладах.Balance(, Номенклатура = &Номенклатура AND Склад = &Склад) AS Balance
```

Alternatively the dimension order is changed in the configuration.

## Sources

- [#std652](https://its.1c.ru/db/v8std#content:652)
- [v8std.ru: #std652](https://v8std.ru/std/652/)
