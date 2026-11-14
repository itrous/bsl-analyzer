# Assigning aliases to selected fields in a query (AssignAliasFieldsInQuery)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description
This diagnostic checks fields inside subqueries and requires aliases to be written explicitly with the `AS` keyword.

Explicit aliases make query results more stable and easier to read. If a field is selected without an alias, the platform derives the result column name automatically. That derived name may change after a metadata rename, or it may simply be less obvious than the name expected by the code that reads the query result.

This is especially important for composite expressions such as `Items.Supplier.Name`, where the generated name may not match the developer's intention.

The diagnostic also reports implicit aliases without `AS`, for example `Items.Price SalePrice`. Asterisk fields (`*`, `Table.*`) are ignored.

In `UNION` queries, result column names are defined by the first `SELECT`, so secondary `UNION` branches do not require selected-field aliases.

A string with no sign of the 1C query language — neither a metadata table such as `Catalog.Products` nor a Cyrillic keyword outside string literals and comments — is not checked: it is usually WQL for `ExecQuery` or SQL for an external DBMS. As a consequence, a query written with English keywords that reads only a temporary table is skipped as well.

## Examples

```bsl   
Query = New Query;
Query.Text =
"SELECT
|   Items.Article, // Incorrect
|   Items.Article AS ItemArticle, // Correct
|   Items.Price SalePrice // Incorrect: alias without AS
|FROM
|   Catalog.Products AS Items // Source alias is ignored
|
|UNION ALL
|
|SELECT
|   Services.Article, // Correct: name is defined by the first branch
|   Services.Article, // Correct: name is defined by the first branch
|   Services.Price // Correct: name is defined by the first branch
|FROM
|   Catalog.Services AS Services";

Query1 = New Query;
Query1.Text =
"SELECT
|   Data.ItemName AS ItemName
|FROM
|   (SELECT
|       Items.Description ItemName // Incorrect: missing AS
|   FROM
|       Catalog.Products AS Items) AS Data";
```

## Sources
Primary source: [Making query text](https://its.1c.ru/db/v8std#content:437:hdoc)

Secondary source: [v8std.ru: #std437 Query text formatting](https://v8std.ru/std/437/)

Additional reference: [v8std.ru: AssignAliasFieldsInQuery](https://v8std.ru/diagnostics/bslls/AssignAliasFieldsInQuery/)
