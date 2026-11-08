# Reading a registrar document attribute in a register query (RegisterRecorderFieldInQuery)

## Description

This diagnostic reports, in a query over a register, a path through the standard field
`Recorder` that reads not a standard field of the document but its attribute, common
attribute or tabular section: `Records.Recorder.Counterparty`.

A register must be self-sufficient: whatever its logic needs is kept in the register's own
dimensions, resources and attributes. Reading data through the recorder joins the query to
the tables of every registrar document and ties the register logic to the set of those
documents.

It fires when all of the following hold:

- the path starts from a register table (accumulation, accounting, information with a
  recorder, calculation), including its virtual tables;
- the first member after the source is the standard field `Recorder` (`Регистратор`);
- the member that follows it is not a standard field of a document.

It does not fire:

- on reading the standard fields of the document that identify and present the recorder:
  reference, date, number, posted flag, deletion mark, presentation, point in time, data
  version;
- on reading `Recorder` itself, with no further member after it;
- on the register's own dimensions, resources and attributes;
- on temporary tables and on sources that are not registers;
- when the type of the source is not established: no metadata is loaded or the field is not
  listed;
- on an independent information register: it has no `Recorder` field. An information
  register subordinate to a recorder is covered.

Any non-standard name after the recorder is reported, including a name no document declares:
it is still a recorder dereference, and the unknown-field diagnostic covers the typo separately.

The path is checked in every query section: the select list, join sources and conditions,
`WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` and virtual table parameters.

## Examples

Incorrect (`Контрагент` is an attribute of the registrar document):

```bsl
ВЫБРАТЬ
    Остатки.Регистратор.Контрагент КАК Контрагент
ИЗ
    РегистрНакопления.ТоварыНаСкладах КАК Остатки
```

Correct (the register keeps the data itself; standard fields of the recorder may be read):

```bsl
ВЫБРАТЬ
    Остатки.Склад КАК Склад,
    Остатки.Регистратор.Дата КАК Дата
ИЗ
    РегистрНакопления.ТоварыНаСкладах КАК Остатки
```

## Sources

- [#std477: Self-sufficiency of registers](https://its.1c.ru/db/v8std#content:477)
- [v8std.ru: #std477](https://v8std.ru/std/477/)
