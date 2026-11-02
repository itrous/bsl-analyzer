# Unknown function in a query (UnknownQueryFunction)

## Description

This diagnostic reports a query that calls a function whose name does not exist in the 1C query language, for example a misspelled `SUBSTRNG` instead of `SUBSTRING`.

A name is considered unknown only when the query language lexer does not recognise it as any function, virtual table, keyword or type. Real functions are never reported, including those that the analyzer does not model individually (`TRIMALL`, `INT`, `ROUND`, `SQRT`, `LEFT`, `RIGHT`, `UUID`, `REFPRESENTATION`).

## Examples

A misspelled function name:
```sdbl
SELECT
    SUBSTRNG(Goods.Name, 1, 2) AS ShortName
FROM
    Catalog.Goods AS Goods
```

## Sources

* [ITS: query language functions](https://its.1c.ru/db/pubqlang)
