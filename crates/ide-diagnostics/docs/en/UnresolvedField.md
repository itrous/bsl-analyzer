# Access to an unknown field (UnresolvedField)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

The diagnostic reports field access on an expression whose type is known well enough for the analyzer to say that the requested field does not exist.

This usually means one of the following:

- the field name contains a typo;
- the code expects a different metadata object type;
- the expression can be `Undefined` or of another type, but the current code does not reflect that explicitly.

The current implementation is conservative. It reports only cases where type inference has a confident metadata reference type and the field lookup fails for that type.

A function's documented return type ("Возвращаемое значение") that names a metadata object, alone or as the element of `Массив из`, is checked against the function body. If the body returns a value of another type (for example, a `Структура`), the result of a call has both types and its fields are not reported. A documented object missing from a visible configuration is ignored. If the body returns the documented object or nothing definite, the documented type stays closed and a missing field is still reported.

A field read through a local variable is reported only if every write reaching the read gives the variable a metadata object without that field (or `Undefined`). A variable assigned objects of different kinds in `Если`/`ИначеЕсли` branches is silent for a field one of them has; a parameter, a declaration or an element of a loop over an unknown collection among the writes also keeps it silent.


Metadata collections are checked the same way:

- `Метаданные.Справочники.NoSuchObject` — the configuration has no object of that name. The platform compiles the module and fails at run time with "Поле объекта не обнаружено" (object field not found). Only the collections of manager-backed kinds are checked (catalogs, documents, registers, charts, enumerations, exchange plans, constants, data processors, reports, business processes, tasks) — the same kinds whose manager collection (`Справочники.NoSuchObject`) is already checked — and only when the configuration is visible to the analyzer; in an external data processor the access stays silent.
- `Метаданные.Справочники.Products.ПредопределенныеЗначения` — the catalog description has no such property. The platform answers "Поле объекта не обнаружено" (object field not found) at run time. Descriptions of manager-backed objects are checked (catalogs, documents, registers, charts, enumerations, exchange plans, constants, data processors, reports, business processes, tasks), whose platform property lists are verified complete. The property is checked both in a chain written in the expression itself (`Метаданные.Справочники.Products.<member>`) and through a variable, including a `Для Каждого Описание Из Метаданные.Справочники` loop variable, when on every path reaching the access the variable holds a description with a closed member list and none of them has the property. The same holds for a variable the description is read through (`Коллекция.Products.<member>`, `Для Каждого Описание Из Коллекция`). If the value is unknown on at least one path (`Описание = Метаданные[Вид][Имя]`, a parameter, an `Если` branch, a loop back-edge, the variable after a loop over the same name), or the variable may be rewritten where the analysis cannot follow (a variable passed as a call argument — arguments are passed by reference, a body with `Выполнить`, a write inside `Попытка` read in its handler or after it), the check stays silent. Descriptions of nested objects (`Реквизиты.X`, forms, templates) and the result of `Метаданные.НайтиПоПолномуИмени(...)` are not checked.

Platform types are checked only where the completeness of their member surface is attested (`type_member_surface_attestations` in `crates/bsl-platform/data/platform_overlays.json`):

- the type must be one of the attested ones, and the platform data the analyzer serves must be of the release the surface was verified against or newer. When that version cannot be determined, the check stays silent;
- for any other platform type a miss proves nothing: the extract is not guaranteed to list every member (some types yielded no members at all, individual help pages are missing from the archive, and a type name may belong to two namesakes), so the access stays silent.

```bsl
Spreadsheet = New SpreadsheetDocument;
Area = Spreadsheet.CurrentArea;
None = Area.CurrentArea; // <-- a cell area has no such member
```

A value table built by `New ValueTable` on every path counts as the attested type `ValueTable`; its columns belong to its rows, not to the table.

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
