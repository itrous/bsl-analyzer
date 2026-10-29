# No NULL checks for fields from joined tables (FieldsFromJoinsWithoutIsNull)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description
<!-- Описание диагностики заполняется вручную. Необходимо понятным языком описать смысл и схему работу -->
This diagnostic checks fields coming from `LEFT`, `RIGHT`, and `FULL JOIN` sources and reports usages that are not protected with `ISNULL()` or an explicit `IS NULL` / `IS NOT NULL` check.

When an outer join does not find a matching row, fields from the joined side can contain `NULL`. Using them as ordinary values can produce wrong comparisons, wrong filters, and unexpected query results.

To make such queries explicit and stable, wrap the field in `ISNULL()`, check it with `IS NULL` / `IS NOT NULL`, or replace the outer join with `INNER JOIN` when that better matches the business meaning.

The current implementation treats `ISNULL(field, default)`, `field IS NULL`, `field IS NOT NULL`, and `NOT (field IS NULL)` as valid protection patterns.

The calculation register virtual table ScheduleData returns one row per register record even when the schedule has no data for the period. The fields <schedule resource>ActionPeriod, <schedule resource>ActualActionPeriod, <schedule resource>RegistrationPeriod and <schedule resource>BasePeriod are then NULL. Replacing a join with an inner join does not remove this NULL: the source is not a join. Wrap such a field in `ISNULL()` or check it with `IS NULL`. There is no numbered standard for NULL from an unfilled schedule.

The rule is on by default: it reports both outer-join fields and ScheduleData fields.

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
