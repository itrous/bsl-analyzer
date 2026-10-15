# Assignment to a read-only property

Fires on an assignment whose left-hand side is a field access on a platform-type property that is read-only. Examples: `Query.Parameters`, `QueryResult.Columns`.

The read-only fact comes from the platform data. Its main source is the platform help book (`Использование: Только чтение`), but not the only one: where the help page carries no mark while the platform refuses the write, the fact is restored by a correction in `platform_overlays.json` (`type_property_corrections`, see `crates/bsl-platform/data/OVERLAYS.md`). `SpreadsheetDocumentRange.Значение` is fixed that way.

## Why this is a problem

The parser accepts the statement, but at runtime the assignment either raises an exception or silently fails to modify the object. The idiomatic replacement is typically a dedicated setter method (`SetParameter` for query parameters) or a full rebuild of the object.

## Examples

Wrong:

```bsl
Query = New Query;
Query.Parameters = New Structure; // <-- read-only property
```

Correct:

```bsl
Query = New Query;
Query.SetParameter("Key", Value);
```

## Suppression

If the platform data is stale for a given property, or the diagnostic fires on legitimate code, it can be disabled through the analyzer configuration in the usual way.
