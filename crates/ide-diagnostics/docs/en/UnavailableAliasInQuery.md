# Outer query alias unavailable (UnavailableAliasInQuery)

## Description

The diagnostic reports a reference to an alias of an enclosing query made from inside a derived table — a nested query in the `ИЗ` (`FROM`) clause or in a join. The platform runs such a nested query as a source of its own and does not see the sources of the enclosing query in it: the query is rejected with `Поле не найдено "Внеш.Ссылка"` (field not found), even when the outer source does have that field.

A nested query in a condition, such as `ГДЕ ... В (...)` (`WHERE ... IN (...)`), is different: it may refer to the sources of the enclosing query (a correlated subquery), and the diagnostic is not reported for it.

Visibility ends at the nearest derived table. A correlated subquery inside a derived table sees the derived table's own sources, but not the sources beyond it. A nested derived table does not see even the sources of the derived table it is nested in. A source declared inside the derived table under the same name shadows the outer one and is not an error.

The message points at the alias itself, once per reference: `Внеш.Поле`, `Внеш.Поле.ПодПоле` and `Внеш.*`.

## Examples

The derived table refers to the outer source `Внеш`:
```sdbl
ВЫБРАТЬ П.Ссылка КАК Р
ИЗ Документ.АктИнвентаризацииСАТУРН КАК Внеш
    ВНУТРЕННЕЕ СОЕДИНЕНИЕ (
        ВЫБРАТЬ Т.Ссылка КАК Ссылка
        ИЗ Документ.АктИнвентаризацииСАТУРН КАК Т
        ГДЕ Т.Ссылка = Внеш.Ссылка
    ) КАК П
    ПО П.Ссылка = Внеш.Ссылка
```

The same correlation in a condition is allowed:
```sdbl
ВЫБРАТЬ Внеш.Ссылка КАК П
ИЗ Документ.АктИнвентаризацииСАТУРН КАК Внеш
ГДЕ Внеш.Ссылка В (
    ВЫБРАТЬ Т.Ссылка ИЗ Документ.АктИнвентаризацииСАТУРН КАК Т
    ГДЕ Т.Ссылка = Внеш.Ссылка
)
```

There is no automatic fix: moving the condition out of the derived table into `ПО` (`ON`) or replacing the derived table with a subquery in a condition may change the query result, so the choice stays with the author. Usually the link to the outer source moves into the join's `ПО`, and the derived table selects its data without it.

## Sources

* [1C:ITS — query language](https://its.1c.ru/db/pubqlang)
