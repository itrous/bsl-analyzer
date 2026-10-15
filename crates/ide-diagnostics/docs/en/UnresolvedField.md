# Access to an unknown field (UnresolvedField)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

The diagnostic reports field access on an expression whose type is known well enough for the analyzer to say that the requested field does not exist.

This usually means one of the following:

- the field name contains a typo;
- the code expects a different metadata object type;
- the expression can be `Undefined` or of another type, but the current code does not reflect that explicitly.

The current implementation is conservative. It reports only cases where type inference has a confident metadata reference type and the field lookup fails for that type.

Metadata collections are checked the same way:

- `Метаданные.Справочники.NoSuchObject` — the configuration has no object of that name. The platform compiles the module and fails at run time with "Поле объекта не обнаружено" (object field not found). Only the collections of manager-backed kinds are checked (catalogs, documents, registers, charts, enumerations, exchange plans, constants, data processors, reports, business processes, tasks) — the same kinds whose manager collection (`Справочники.NoSuchObject`) is already checked — and only when the configuration is visible to the analyzer; in an external data processor the access stays silent.
- `Метаданные.Справочники.Products.ПредопределенныеЗначения` — the catalog description has no such property. The platform answers "Поле объекта не обнаружено" (object field not found) at run time. Descriptions of manager-backed objects are checked (catalogs, documents, registers, charts, enumerations, constants, data processors, reports, business processes, tasks), whose platform property lists are verified complete. The property is checked both in a chain written in the expression itself (`Метаданные.Справочники.Products.<member>`) and through a variable, including a `Для Каждого Описание Из Метаданные.Справочники` loop variable, when on every path reaching the access the variable holds a description with a closed member list and none of them has the property. If the value is unknown on at least one path (`Описание = Метаданные[Вид][Имя]`, a parameter, an `Если` branch, a loop back-edge, the variable after a loop over the same name), the check stays silent. Descriptions of nested objects (`Реквизиты.X`, forms, templates) and the result of `Метаданные.НайтиПоПолномуИмени(...)` are not checked.

## Examples

Incorrect:

```bsl
Ref = CommonModule.GetCatalogRef();
Name = Ref.UnknownField;
```

Correct:

```bsl
Ref = CommonModule.GetCatalogRef();
Name = Ref.Description;
```
