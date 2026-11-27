# Unknown field in a query table (UnknownFieldInQuery)

## Description

This diagnostic reports a query that references a field which does not exist on its metadata table.

To stay false-positive safe, it fires only for tables whose field model is provably complete: resolved reference objects, register main tables, and the virtual tables of information registers (`SliceFirst`, `SliceLast`), accumulation registers (`Balance`, `Turnovers`, `BalanceAndTurnovers`) and accounting registers (`Balance`, `Turnovers`, `DrCrTurnovers`, `BalanceAndTurnovers`, `RecordsWithExtDimensions`). It stays silent on incomplete models: temporary tables, subqueries, calculation-register tables, objects that resolve through an extension, and the tables of an accounting register whose chart of accounts is not found (without it the number of `ExtDimension<N>` fields is unknown).

For an accounting register with correspondence the field names follow the "Balance" property: a non-balance dimension or resource is named by its sides (`<Dimension>Dr`, `<Resource>Cr`) in the main table and in `RecordsWithExtDimensions`, and only non-balance resources have the correspondent-turnover fields of `Turnovers`. Fields whose presence depends on the call arguments (`Period`, the per-period breakdown such as `MonthPeriod`) are treated as present.

A path through a reference field is checked hop by hop: in `T.Field.SubField`, `Field` is checked against the table first, then `SubField` against the object `Field` points at (likewise `T.Field.Field2.SubField` and tabular sections: `T.Document.Goods.Column`). The first provably absent hop is reported, at most one finding per reference, and the range is the name of that very hop. The object's field set is built the same way as for a table in `FROM`: attributes, standard attributes (`Ref`, `Code`, `Number`, `Date`, ...), common attributes, tabular sections, and the fields the platform offers on a query table (`Presentation`, `PointInTime` on a document).

The diagnostic stays silent whenever absence is not proven: the hop has a composite type, a defined type, `AnyRef` or a primitive type; the object does not resolve (including an object added by an extension); the object has open common attributes; the field is declared but may not exist (a conditional table field); or the path is longer than 10 hops. A first hop that is missing from the table still yields one finding and the rest of the path is not examined.

Two deliberate limits. The check is silent behind a field whose type is a characteristic value, because the metadata model does not distinguish it from a reference to the chart of characteristic types. And it goes past the first hop only where the first hop's type is a declared attribute of a metadata table; a column of a temporary table, a nested query or a `CAST` carries a type inferred from an expression (`CAST(... AS T).Field` is typed as `T`, not as `Field`), so absence is not proven along such a path.

## Examples

Reference to a missing field on a register:
```sdbl
SELECT
    T.NoSuchField AS Value
FROM
    AccumulationRegister.Goods AS T
```

## Sources

* [ITS: query language fields](https://its.1c.ru/db/pubqlang)
