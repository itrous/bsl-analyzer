# No NULL checks for fields from joined tables (FieldsFromJoinsWithoutIsNull)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description
<!-- Описание диагностики заполняется вручную. Необходимо понятным языком описать смысл и схему работу -->
This diagnostic checks fields coming from `LEFT`, `RIGHT`, and `FULL JOIN` sources and reports usages that are not protected with `ISNULL()` or an explicit `IS NULL` / `IS NOT NULL` check.

When an outer join does not find a matching row, fields from the joined side can contain `NULL`. Using them as ordinary values can produce wrong comparisons, wrong filters, and unexpected query results.

To make such queries explicit and stable, wrap the field in `ISNULL()`, check it with `IS NULL` / `IS NOT NULL`, or replace the outer join with `INNER JOIN` when that better matches the business meaning.

`ISNULL(field, default)` protects the wrapped use. Conditions such as `field IS NULL`, `field IS NOT NULL` and `NOT (field IS NULL)` protect through the reachability rules below.

The calculation register virtual table ScheduleData returns one row per register record even when the schedule has no data for the period. The fields <schedule resource>ActionPeriod, <schedule resource>ActualActionPeriod, <schedule resource>RegistrationPeriod and <schedule resource>BasePeriod are then NULL. Replacing a join with an inner join does not remove this NULL: the source is not a join. Wrap such a field in `ISNULL()` or check it with `IS NULL`. There is no numbered standard for NULL from an unfilled schedule.

The rule is on by default since version 0.4.2: it reports both outer-join fields and ScheduleData fields.

### When an outer-join field is not reported

The NULL of a missing row matters where it reaches the query result. To decide that, the rule substitutes NULL for the direct fields of the missing row and evaluates conditions in three-valued logic: a condition is true, false or undefined. A comparison with NULL is undefined. That means "not true", not "false": `NOT` of an undefined condition stays undefined. Parameters, fields of other tables and anything the rule cannot evaluate are unknown: such a condition may have any outcome. The query itself is not executed, so the result is a conservative proof, not a value from the database.

The rule does not report outer-join fields in these cases.

* **A field used only in a condition.** A field in `WHERE`, in a `WHEN` condition (including the expression after `CASE` and the `WHEN` values of a simple `CASE`) or in `ON` does not reach the result: the condition only decides whether the row is selected. A comparison with NULL is not true, and `IS NULL` selects exactly the missing row; neither puts NULL into the selected values. When the same field is also selected, the selected use is reported, except in the cases below.
* **A `THEN` branch unreachable without the row.** When the `WHEN` condition cannot be true after the substitution, fields of that table in the `THEN` branch are not reported. This covers the comparisons `=`, `<>`, `<`, `<=`, `>`, `>=` against anything, `BETWEEN`, `LIKE`, `REFS`, `IN`, `IN HIERARCHY`, a bare boolean field, `IS NOT NULL`, `NOT` over them (subject to the empty-list note below) and a simple `CASE T.Field WHEN 0 THEN …`. `AND` protects when at least one operand cannot be true, `OR` only when no operand can: `T.Field = 0 OR &Flag` does not protect the branch. Fields of another joined table are not protected in that branch.
* **Branches after a condition that is surely true.** When a `WHEN` condition is always true after the substitution (`T.Field IS NULL`, `T.Field IS NULL OR &Flag`, `ISNULL(T.Field, 0) = 0` with a known numeric type), the following `WHEN` branches and `ELSE` do not run without the row, and fields of that table in them are not reported. Such a condition protects nothing in its own `THEN` branch: `WHEN T.Employee IS NULL THEN T.CalculationKind` is reported, the value there is always NULL. An undefined condition does not count as true: `WHEN T.Field = 0 THEN 0 ELSE T.Amount` and a simple `CASE T.Field WHEN 0 THEN 0 ELSE T.Amount` report `T.Amount`.
* **What passes NULL through.** NULL comes from a direct field of the table, the arithmetic `+`, `-`, `*`, `/`, `%`, the unary `+` and `-`, and `DATEDIFF` when NULL stands in at least one of its dates; an unknown second date does not change that. No other function is treated as passing NULL through: `ISNULL`, `CASE` as a value, `PRESENTATION` (which gives an empty string for NULL), `COUNT` and an unknown function are unknown. So `PRESENTATION(T.Field) = ""` does not protect the branch.
* **`ISNULL` with a computed replacement.** Without the row `ISNULL(T.Field, R)` equals `R` only when the field type is known and converting `R` to it certainly keeps the value: an integer literal that fits the known digits of a number (zero under any qualifiers); a boolean literal for a boolean field; `DATETIME(1, 1, 1)` for a date; a string that fits the known length of a real table's field, or any string for an unlimited length (`Length = 0`). For a direct field of a temporary table with a known string type the whole replacement is kept even when it is longer than the field: this was observed on the platform and does not carry over to real tables. The result stays unknown for an unknown, composite or unexpanded defined type, for a fractional, signed, mismatching or unrepresentable literal, for a string of unknown length, and for a parameter, which may be NULL itself. Other `DATETIME` calls are unknown too. Strings spelled differently are not taken as unequal: that depends on the database collation, so such a comparison is unknown.
* **Without field types such conditions are not evaluated.** A type is never guessed from a field name. Without metadata `WHEN ISNULL(Rates.Rate, 0) = 0 THEN 0 ELSE Rates.Rate` and `WHEN ISNULL(Assets.WriteOffDate, DATETIME(1, 1, 1)) <> DATETIME(1, 1, 1) THEN Assets.WriteOffDate` stay reported; with loaded metadata and a suitable type they are not.
* **`IN` and `IN HIERARCHY`.** `T.Field IN (…)` and `T.Field IN HIERARCHY (…)` are not true without the row for any list. `NOT` over them and `NOT IN` protect only when the list holds an explicit literal (including `NULL` or `VALUE(Catalog.X.EmptyRef)`): the condition is then undefined. A parameter, several parameters and a nested query may turn out empty, NULL in an empty list is false, and `NOT` makes that true, so `NOT (T.Field IN (&Values))` does not protect. The parameter's value in BSL code and the nested query are not analysed.
* **The owner of its own tabular section.** When a tabular section is joined with its own owner (`Document.X.Section` and `Document.X`, `Catalog.X.Section` and `Catalog.X`) on `Section.Ref = Owner.Ref`, every row of the section has its owner row, and the owner's direct fields are not reported. This covers `Section LEFT JOIN Owner`, `Owner RIGHT JOIN Section` and the owner side of a `FULL JOIN`. Third tables do not affect the exception as long as the section itself is required. Fields of the tabular section are still reported in `Owner LEFT JOIN Section` and in a `FULL JOIN`: the owner may have no rows. The exception is cancelled when another join makes the section optional (it stands on the optional side of an outer `LEFT`, `RIGHT` or `FULL`), when the owner is joined together with another table as one operand, and by an extra filter in `ON` (`AND Owner.Posted = TRUE`), `OR`, another field or another object. The order of the operands and parentheses do not matter, the same equality repeated with `AND` is allowed.
* **A `WHERE` condition that excludes the row.** When the `WHERE` condition cannot be true after the substitution, the unmatched row does not reach the result, and the direct fields of that table are not reported. One top-level conjunct is enough: `Prices.Period BETWEEN &Start AND &End`, `Doc.Posted AND Prices.PriceType IN (&PriceTypes)`, `T.Field IS NOT NULL`, `NOT (T.Field IS NULL)`. A conjunct with `OR` is evaluated as a whole: `T.Field IS NOT NULL OR &Flag` and `T.Field = 0 OR &Flag` do not protect, while `T.Field = 0 OR T.Other = 1` does. The proof applies only to the table of the condition: in a `FULL JOIN` a condition on one side does not protect the other, and it does not protect other joined tables. `HAVING`, `ON` and an alias from a nested query do not count as the outer query's `WHERE`. There is no separate warning that `WHERE` turns the join into an inner one in effect.
* **A dereference stays reported.** All the exceptions about a present row and about `WHERE` apply to direct fields. `T.Ref.Attribute`, `(T.Field).Attribute` and `CAST(T.Field AS Catalog.X).Attribute` are reported even for an existing row, because the reference may be empty. Such an expression in a condition does not count as a proof.

In `CASE` conditions and in the owner exception, Latin table aliases are matched regardless of case, Cyrillic ones as written. The `WHERE` condition matches the alias exactly as written, Latin ones included: `WHERE couriers.Transport IS NOT NULL` with the alias `Couriers` does not protect the fields.

These exceptions prove that the joined row exists and do not apply to ScheduleData values, which are NULL in an existing row as well. A schedule value in a `WHERE` or `WHEN` condition is reported, and a comparison of another field of the row (such as `LineNumber`) does not protect the schedule value.

## Examples
<!-- В данном разделе приводятся примеры, на которые диагностика срабатывает, а также можно привести пример, как можно исправить ситуацию -->
```sdbl
ВЫБРАТЬ 
  ДокументыПродажи.Ссылка КАК ДокПродажи,
  РегистрПродажи.Сумма КАК Сумма // здесь ошибка
ИЗ Документ.РеализацияТоваровУслуг КАК ДокументыПродажи
ЛЕВОЕ СОЕДИНЕНИЕ  РегистрНакопления.Продажи КАК РегистрПродажи
ПО ДокументыПродажи.Ссылка = РегистрПродажи.Документ
```
Correct
```sdbl
ВЫБРАТЬ 
  ДокументыПродажи.Ссылка КАК ДокПродажи,
  ЕстьNULL(РегистрПродажи.Сумма, 0) КАК Сумма
ИЗ Документ.РеализацияТоваровУслуг КАК ДокументыПродажи
ЛЕВОЕ СОЕДИНЕНИЕ  РегистрНакопления.Продажи КАК РегистрПродажи
ПО ДокументыПродажи.Ссылка = РегистрПродажи.Документ
```
Also correct:
```sdbl
ВЫБРАТЬ 
  ДокументыПродажи.Ссылка КАК ДокПродажи,
  ВЫБОР КОГДА РегистрПродажи.Сумма Есть NULL ТОГДА 0
  ИНАЧЕ  РегистрПродажи.Сумма 
  КОНЕЦ КАК Сумма
ИЗ Документ.РеализацияТоваровУслуг КАК ДокументыПродажи
ЛЕВОЕ СОЕДИНЕНИЕ  РегистрНакопления.Продажи КАК РегистрПродажи
ПО ДокументыПродажи.Ссылка = РегистрПродажи.Документ
```
Another possible variant:
```sdbl
ВЫБРАТЬ 
  ДокументыПродажи.Ссылка КАК ДокПродажи,
  РегистрПродажи.Сумма КАК Сумма
ИЗ Документ.РеализацияТоваровУслуг КАК ДокументыПродажи
ЛЕВОЕ СОЕДИНЕНИЕ  РегистрНакопления.Продажи КАК РегистрПродажи
ПО ДокументыПродажи.Ссылка = РегистрПродажи.Документ
ГДЕ
    РегистрПродажи.Документ ЕСТЬ НЕ NULL
    //или НЕ РегистрПродажи.Документ ЕСТЬ NULL
```
The last option effectively emulates an inner join. If that is the real intention, it is usually clearer to replace the outer join with `INNER JOIN`.

Arithmetic with a joined field (reported): a NULL rate makes the whole amount NULL.
```sdbl
ВЫБРАТЬ
	Товары.Сумма * Ставки.Ставка КАК Сумма
ИЗ
	Документ.Заказ.Товары КАК Товары
		ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки
		ПО Товары.Номенклатура = Ставки.Номенклатура
```
Correct:
```sdbl
ВЫБРАТЬ
	Товары.Сумма * ЕСТЬNULL(Ставки.Ставка, 0) КАК Сумма
ИЗ
	Документ.Заказ.Товары КАК Товары
		ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки
		ПО Товары.Номенклатура = Ставки.Номенклатура
```
A `THEN` branch under a comparison of a field of the same table (not reported): without a rate row the `WHEN` condition is not true and the division does not run.
```sdbl
ВЫБРАТЬ
	ВЫБОР
		КОГДА Ставки.Ставка <> 0
			ТОГДА Товары.Сумма / Ставки.Ставка
		ИНАЧЕ 0
	КОНЕЦ КАК Сумма
ИЗ
	Документ.Заказ.Товары КАК Товары
		ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Ставки КАК Ставки
		ПО Товары.Номенклатура = Ставки.Номенклатура
```
An `ELSE` branch after a condition that is surely true (not reported with a known numeric type of `Курсы.Курс`): without a rate row the condition is `0 = 0`, so `ELSE` runs only for an existing row.
```sdbl
ВЫБРАТЬ
	ВЫБОР
		КОГДА ЕСТЬNULL(Курсы.Курс, 0) = 0
			ТОГДА 0
		ИНАЧЕ Док.Сумма * Курсы.Курс / Курсы.Кратность
	КОНЕЦ КАК СуммаРуб
ИЗ
	Документ.Заказ КАК Док
		ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.КурсыВалют КАК Курсы
		ПО Док.Валюта = Курсы.Валюта
```
A `WHERE` condition that excludes the unmatched row (not reported):
```sdbl
ВЫБРАТЬ
	Док.Ссылка КАК Ссылка,
	Цены.Цена КАК Цена
ИЗ
	Документ.Заказ КАК Док
		ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены
		ПО Док.Номенклатура = Цены.Номенклатура
ГДЕ
	Цены.Период МЕЖДУ &Начало И &Конец
```
The owner of a tabular section (not reported): every row of the section has its owner document, and a third table does not change that.
```sdbl
ВЫБРАТЬ
	Товары.Номенклатура КАК Номенклатура,
	Заказ.Дата КАК Дата
ИЗ
	Документ.Заказ.Товары КАК Товары
		ЛЕВОЕ СОЕДИНЕНИЕ Документ.Заказ КАК Заказ
		ПО Товары.Ссылка = Заказ.Ссылка
```

A schedule value read without `ISNULL()` (reported):
```sdbl
ВЫБРАТЬ
	ДанныеГрафика.НомерСтроки КАК НомерСтроки,
	ДанныеГрафика.ЧасыФактическийПериодДействия * 100 КАК Сумма
ИЗ
	РегистрРасчета.Начисления.ДанныеГрафика(Регистратор = &Регистратор) КАК ДанныеГрафика
```
Correct:
```sdbl
ВЫБРАТЬ
	ДанныеГрафика.НомерСтроки КАК НомерСтроки,
	ЕСТЬNULL(ДанныеГрафика.ЧасыФактическийПериодДействия, 0) КАК Часы
ИЗ
	РегистрРасчета.Начисления.ДанныеГрафика(Регистратор = &Регистратор) КАК ДанныеГрафика
```

## Sources
<!-- Необходимо указывать ссылки на все источники, из которых почерпнута информация для создания диагностики -->
<!-- Примеры источников

* Source: [Standard: Modules (RU)](https://its.1c.ru/db/v8std#content:456:hdoc)
* Useful information: [Refusal to use modal windows (RU)](https://its.1c.ru/db/metod8dev#content:5272:hdoc)
* Источник: [Cognitive complexity, ver. 1.4](https://www.sonarsource.com/docs/CognitiveComplexity.pdf) -->
* [Using the ISNULL() function (RU)](https://its.1c.ru/db/metod8dev/content/2653/hdoc)
* [The concept of "empty" values (RU)](https://its.1c.ru/db/metod8dev/content/2614/hdoc/_top/%D0%B5%D1%81%D1%82%D1%8C%20null)
* [What is the difference between Undefined and Null? (RU)](https://its.1c.ru/db/metod8dev/content/2516/hdoc)
* [Query fields that can contain NULL: sorting guidance (RU)](https://its.1c.ru/db/v8std/content/412/hdoc/_top/%D0%B5%D1%81%D1%82%D1%8C%20null)
