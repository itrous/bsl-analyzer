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
