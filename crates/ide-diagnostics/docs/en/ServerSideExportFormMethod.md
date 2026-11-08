# Server-side export form method (ServerSideExportFormMethod)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

Export methods of managed forms should remain client-side. In practice, external access to a form goes through `GetForm`, so export procedures and functions of the form only make sense in the client context.

The current implementation reports exported procedures and functions in managed forms when they do not have the `&AtClient` / `&НаКлиенте` annotation.

Server-side annotations such as `&AtServer` or `&AtServerNoContext`, as well as the absence of an annotation, are treated as errors for this rule.

The exception is a method whose name starts with `Подключаемый_` or `Attachable_` (case-insensitive): these are the BSP pluggable command handlers, called by name from a client common module on the form passed in, and without `Export` that call does not work.

## Examples

Incorrect:

```bsl
Procedure One() Export
  // procedure without directive is available on the server
EndProcedure

&AtServerNoContext
Procedure AtServerNoContext() Export
EndProcedure

&AtServer
Procedure AtServer() Export
EndProcedure
```

Correct:

```bsl
&AtClient
Procedure OnNotification() Export
EndProcedure
```

## Sources

- [#std630: Form module rules (RU)](https://its.1c.ru/db/v8std#content:630:hdoc)
- [#std544: Restrictions on export procedures and functions (RU)](https://its.1c.ru/db/v8std#content:544:hdoc)
- [1C UI development guide, chapter 3.5 (RU)](https://its.1c.ru/db/pubv8devui/content/191/hdoc)
