# Virtual table call without parameters (VirtualTableCallWithoutParameters)

## Description
When using virtual tables in queries, relevant filters should be passed through the virtual table parameters.

Virtual tables are classified by their full path and register family. `Catalog.Subconto` is not virtual merely because of its final name. The stored table `AccountingRegister.Name.ExtDimensions` also takes no parameters and does not require them.

The current implementation reports calls where no argument is passed to the virtual table at all, for example:

- no parentheses at all
- empty parentheses `()`
- every parameter position empty `(, )`

A call with at least one filled parameter (`(&Period, )`, `(, Warehouse = &Warehouse)`) already uses the virtual table parameter mechanism and is not reported.

This is a conservative performance-oriented rule intended to catch calls that are likely to read too much data before filtering.

## Examples
Reported:
```bsl
Query.Text = "SELECT
| Good
|FROM
| AccumulationRegister.MyGoods.Turnovers()";
```

Preferred:

```bsl
Query.Text = "SELECT
| Good
|FROM
| AccumulationRegister.MyGoods.Turnovers(, Warehouse = &Warehouse)";
```

## Sources
* Standard: [Using virtual tables (RU)](https://its.1c.ru/db/v8std#content:657:hdoc)
* Standard: [Effective use of the virtual table «Turnovers» (RU)](https://its.1c.ru/db/v8std#content:733:hdoc)
* 1C Recommendation: [Using the Condition parameter when accessing a virtual table (RU)](https://its.1c.ru/db/metod8dev/content/5457/hdoc)
