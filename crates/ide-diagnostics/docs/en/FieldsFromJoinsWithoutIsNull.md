# No NULL checks for fields from joined tables (FieldsFromJoinsWithoutIsNull)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description
<!-- Описание диагностики заполняется вручную. Необходимо понятным языком описать смысл и схему работу -->
This diagnostic checks fields coming from `LEFT`, `RIGHT`, and `FULL JOIN` sources and reports usages that are not protected with `ISNULL()` or an explicit `IS NULL` / `IS NOT NULL` check.

When an outer join does not find a matching row, fields from the joined side can contain `NULL`. Using them as ordinary values can produce wrong comparisons, wrong filters, and unexpected query results.

To make such queries explicit and stable, wrap the field in `ISNULL()`, check it with `IS NULL` / `IS NOT NULL`, or replace the outer join with `INNER JOIN` when that better matches the business meaning.

The current implementation treats `ISNULL(field, default)`, `field IS NULL`, `field IS NOT NULL`, and `NOT (field IS NULL)` as valid protection patterns.

The calculation register virtual table ScheduleData returns one row per register record even when the schedule has no data for the period. The fields <schedule resource>ActionPeriod, <schedule resource>ActualActionPeriod, <schedule resource>RegistrationPeriod and <schedule resource>BasePeriod are then NULL. Replacing a join with an inner join does not remove this NULL: the source is not a join. Wrap such a field in `ISNULL()` or check it with `IS NULL`. There is no numbered standard for NULL from an unfilled schedule.

The rule is on by default since version 0.4.2: it reports both outer-join fields and ScheduleData fields.

### When an outer-join field is not reported

The NULL of a missing row matters where it reaches the query result. The rule therefore does not report outer-join fields in these cases.

* **A field used only in a condition.** A field in `WHERE`, in a `WHEN` condition (including the expression after `CASE` and the `WHEN` values of a simple `CASE`) or in `ON` does not reach the result: the condition only decides whether the row is selected. A comparison with NULL is false, and `IS NULL` selects exactly the missing row; neither puts NULL into the selected values. There is no separate warning that a `WHERE` condition turns a left join into an inner one in effect. When the same field is also selected, the selected use is reported.
* **A `THEN` branch under a comparison.** When the `WHEN` condition directly compares a field of the joined table with `=`, `<>`, `<`, `<=`, `>` or `>=` against anything (a literal, a parameter, a field, an expression), the `THEN` branch runs only when the row exists, and fields of the same table in it are not reported. The comparison may be one operand of `AND`. These do not protect: a comparison inside `OR` or under `NOT`; a field inside a function or an expression (`ISNULL(T.Field, 0) = 0`, `T.Field + 1 > 0`); `IN`, `BETWEEN`, `LIKE`; a bare boolean field; a simple `CASE T.Field WHEN 0 THEN …`. The comparison protects only its own branch: fields in the following `WHEN` branches and in `ELSE` are reported, and fields of another joined table are not protected even in that branch.
* **The owner of its own tabular section.** When a tabular section is joined with its own owner (`Document.X.Section` and `Document.X`, `Catalog.X.Section` and `Catalog.X`) on `Section.Ref = Owner.Ref`, the owner row always exists and its fields are not reported. This covers `Section LEFT JOIN Owner`, `Owner RIGHT JOIN Section` and the owner side of a `FULL JOIN`. Fields of the tabular section are still reported in `Owner LEFT JOIN Section` and in a `FULL JOIN`: the owner may have no rows. The exception holds only when the query has exactly these two tables and the `ON` condition is that equality alone (the order of the operands and parentheses do not matter, the same equality repeated with `AND` is allowed). An extra filter (`AND Owner.Posted = TRUE`), `OR`, another field or another object, or a third table in the join cancels the exception.
* **A test in `WHERE`.** `T.Field IS NOT NULL` or `NOT (T.Field IS NULL)` in `WHERE` protects the selected fields of that table. An ordinary comparison in `WHERE` does not protect them.

In the owner and `THEN` exceptions, Latin table aliases are matched regardless of case, Cyrillic ones as written. The test in `WHERE` matches the alias exactly as written, Latin ones included: `WHERE couriers.Transport IS NOT NULL` with the alias `Couriers` does not protect the fields.

These exceptions prove that the joined row exists and do not apply to ScheduleData, whose values are NULL in an existing row as well. A schedule value in a `WHERE` or `WHEN` condition is reported, and a comparison of another field of the row (such as `LineNumber`) does not protect the schedule value.

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
A `THEN` branch under a comparison of a field of the same table (not reported): without a rate row the `WHEN` condition is false and the division does not run.
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
The owner of a tabular section (not reported): every row of the section has its owner document.
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
