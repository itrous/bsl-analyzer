# Export methods in command and general command modules (CommandModuleExportMethods)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

Command modules and common command modules do not expose a public API to other
modules. Because of that, the `Export` modifier on their procedures and
functions is misleading: it suggests external reusability that the platform
does not actually provide.

This diagnostic reports exported procedures and functions in those module types
so the declaration matches the real execution model. It exempts a method registered
as a notification handler on the current command module: the platform calls that
handler by name and requires `Export`. The registration and receiver must be recognized.

## Example

```bsl
&НаКлиенте
Процедура ВыполнитьКоманду() Экспорт
    // ...
КонецПроцедуры
```

## Sources

Primary source: [Standard: restrictions on exported procedures and functions (RU)](https://its.1c.ru/db/v8std/content/544/hdoc)

Secondary source: [v8std.ru: #std544](https://v8std.ru/std/544/)

Additional reference: [v8std.ru: CommandModuleExportMethods](https://v8std.ru/diagnostics/bslls/CommandModuleExportMethods/)
