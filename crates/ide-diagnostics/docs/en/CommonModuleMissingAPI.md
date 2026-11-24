# Common module should have a programming interface (CommonModuleMissingAPI)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

Confirmed common modules that contain methods are expected to expose a
clear API structure. In practice this means two things:

- there is at least one exported method that can serve as an entry point;
- API methods are grouped into `Public` / `Internal` regions (or their Russian
  equivalents `ПрограммныйИнтерфейс` / `СлужебныйПрограммныйИнтерфейс`).

If a module has methods but lacks exported members or lacks API regions, its
public contract becomes unclear and the module structure stops matching the
usual 1C module layout.

Manager, object and form modules are excluded, including extension modules
containing interceptors and private helpers. An unknown module kind is not
assumed to be common. A common module containing only interceptors still needs
no artificial exported method; ordinary methods retain the API requirement.

## Examples

### Wrong

```bsl
Procedure Test(A)
    A = A + 1;
EndProcedure
```

### Correct

```bsl
#Region Internal
Procedure Test(A) Export
    A = A + 1;
EndProcedure
#EndRegion
```

## Sources

Primary source: [Standard: module structure (RU)](https://its.1c.ru/db/v8std#content:455:hdoc)

Secondary source: [v8std.ru: #std455](https://v8std.ru/std/455/)

Additional context: [v8std.ru: CommonModuleMissingAPI](https://v8std.ru/diagnostics/bslls/CommonModuleMissingAPI/)
