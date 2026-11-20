# Ordering by a document date in a query (QueryOrderByDocumentDate)

## Description

This diagnostic reports, in the `ORDER BY` section, a key that orders by the `Date` field of
a document reference: `Batches.Batch.Date`.

The date of a document does not define a definite order: documents of the same date come in
arbitrary order, which may differ from one query to the next. The definite chronological key
is `PointInTime`: it accounts for both the date and the position of the document among the
documents of the same date.

The message reads: "Ordering by «{field}.Date» does not define a definite order: documents of
the same date come in arbitrary order. Order by «{field}.PointInTime»" (`{field}` is the
reference field as written in the query).

It fires when all of the following hold:

- the key has exactly three members: `<source>.<field>.Date` (`Дата`);
- the first member is a source of this query (by alias or name) whose metadata is known,
  including a virtual table of a register;
- the second member is a field of that source whose type is a document reference: a single
  document, "any document reference", or a composite type all of whose arms are document
  references;
- the same `ORDER BY` has no key `<source>.<field>.PointInTime` (`МоментВремени`) over the
  same reference.

The direction (`ASC`, `DESC`) does not matter: both are reported.

It does not fire:

- on a two-part key where the source is the document table itself (`Doc.Date`): the standard
  (item 1.3) expects results shown to the user to be ordered by fields of primitive types, so
  a plain date column is not a defect there;
- on a key of four or more members (`Batches.Batch.Owner.Date`): the middle chain is not
  resolved here;
- on a key that is a result-column alias or an expression rather than a field path;
- on a field whose type is a catalog reference or another kind of object, nor on a composite
  type that holds anything besides documents;
- on an unresolved source (no metadata is loaded or the table does not exist);
- in `WHERE`, `ON`, `TOTALS`, `GROUP BY`: this rule is only about `ORDER BY`.

## Examples

Incorrect (documents of one date come in arbitrary order):

```bsl
ВЫБРАТЬ
    Партии.Партия КАК Партия
ИЗ
    РегистрНакопления.ПартииТоваров.Остатки(&Момент, Номенклатура = &Номенклатура) КАК Партии
УПОРЯДОЧИТЬ ПО
    Партии.Партия.Дата
```

Correct:

```bsl
ВЫБРАТЬ
    Партии.Партия КАК Партия
ИЗ
    РегистрНакопления.ПартииТоваров.Остатки(&Момент, Номенклатура = &Номенклатура) КАК Партии
УПОРЯДОЧИТЬ ПО
    Партии.Партия.МоментВремени
```

## Sources

- [#std412: Ordering of query results](https://its.1c.ru/db/v8std#content:412:hdoc)
- [v8std.ru: #std412](https://v8std.ru/std/412/)
