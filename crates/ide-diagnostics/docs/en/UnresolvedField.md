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

Platform types are checked only where the completeness of their member surface is attested (`type_member_surface_attestations` in `crates/bsl-platform/data/platform_overlays.json`):

- the type must be one of the attested ones, and the platform data the analyzer serves must be of the release the surface was verified against or newer. When that version cannot be determined, the check stays silent;
- for any other platform type a miss proves nothing: the extract is not guaranteed to list every member (some types yielded no members at all, individual help pages are missing from the archive, and a type name may belong to two namesakes), so the access stays silent.

```bsl
Spreadsheet = New SpreadsheetDocument;
Area = Spreadsheet.CurrentArea;
None = Area.CurrentArea; // <-- a cell area has no such member
```

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
