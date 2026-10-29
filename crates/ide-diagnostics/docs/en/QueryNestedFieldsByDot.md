# Getting objects nested fields data by dot in database query text (QueryNestedFieldsByDot)

## Description

This diagnostic reports, inside query text, a dot dereference of a field whose
type in the metadata is a composite reference, when the field is not cast with
`CAST` / `ВЫРАЗИТЬ`.

The platform dereferences such a field by joining every table the composite
type includes. When the field may point to several objects, the query joins the
table of each of them, even if only one is needed. `ВЫРАЗИТЬ(... КАК Справочник.Х)`
narrows the join to one table.

It fires when the field is followed by another member of the path and its type is:

- composite with at least one reference among its parts, such as "item reference
  or warehouse reference" and "item reference or string";
- any reference or any reference of one kind, such as the standard `Recorder`
  attribute of an accumulation register;
- a defined type whose underlying type is one of the above.

It does not fire:

- on a field of a single reference type: that is an ordinary dereference joining
  one table;
- on a composite type without references, such as "string or number";
- on a field whose type the metadata does not establish: no metadata is loaded,
  the field is not in the table's field list, the defined type is not found.
  Without metadata the diagnostic does not occur;
- on a path cast with `ВЫРАЗИТЬ`: `ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Код`.
  A `ССЫЛКА` / `REFS` condition does not replace the cast;
- on `T.Field`: the last name of a path is only read and dereferences nothing;
- on a field the metadata model lists without knowing whether the object has it.
  The `Recorder` of an information register is listed that way, so
  `Recorder.Date` of an information register is not reported.

Paths are checked in every query section: the select list, join sources and
conditions, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, virtual table parameters
and nested queries. Inside the argument of `ВЫРАЗИТЬ` a path is checked as an
ordinary one. After `ВЫРАЗИТЬ` the first member is a field of the one named
table; a dereference further down the chain is reported when that field's type
is a composite reference.

## Examples

Incorrect (`Объект` is an item or a warehouse reference):

```bsl
ВЫБРАТЬ
    Цены.Объект.Код КАК Код
ИЗ
    РегистрСведений.ЦеныОбъектов КАК Цены
```

Correct:

```bsl
ВЫБРАТЬ
    ВЫРАЗИТЬ(Цены.Объект КАК Справочник.Номенклатура).Код КАК Код
ИЗ
    РегистрСведений.ЦеныОбъектов КАК Цены
ГДЕ
    Цены.Объект ССЫЛКА Справочник.Номенклатура
```

## Configuration

| Parameter      | Type   | Default | Description |
|----------------|--------|---------|-------------|
| `minPathDepth` | `int`  | `3`     | Minimum number of parts of an ordinary path from a query source for a found dereference to be shown. The default `3` shows `T.Field.Attribute`; higher values hide shorter paths. A two-part `T.Field` is not reported at any value: it holds no dereference. |

`minPathDepth` applies only to paths from a query source. Dereferences inside
virtual table parameters and in chains after `CAST` / `ВЫРАЗИТЬ` have no length
from a source and are shown at any value of the setting.

To turn the diagnostic off entirely use the standard activation flag rather than
`minPathDepth`.

## Sources

- Standard: [Dereference of composite-type reference fields in the query language (#std654)](https://its.1c.ru/db/v8std#content:654:hdoc)
- Secondary reference: [v8std.ru: QueryNestedFieldsByDot](https://v8std.ru/diagnostics/bslls/QueryNestedFieldsByDot/)
