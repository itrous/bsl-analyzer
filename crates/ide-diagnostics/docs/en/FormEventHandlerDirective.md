# Form event handler directive (FormEventHandlerDirective)

## Description

A managed form event handler declared with a compilation directive that does not compile it into the context in which the event calls it is not bound by the platform: the handler never runs.

This was measured on platform 8.3.27.2214: with a mismatched directive there is no build error, no warning and no runtime exception - the handler is silently never called. The mistake shows only as an expected action not happening.

The required context depends on the event: for example `OnCreateAtServer` requires `&AtServer`, `OnOpen` requires `&AtClient`, `OnGetDataAtServer` requires `&AtServerNoContext`. The event name is taken from the form's `Ext/Form.xml`; a procedure is bound to it by handler name, compared case-insensitively.

Only combinations verified by a live probe are reported:

- the event requires `&AtServer`: reported when the handler is declared with `&AtServerNoContext`, `&AtClient` or `&AtClientAtServerNoContext`; `&AtServer` and no directive are accepted;
- the event requires `&AtClient`: reported when the handler is declared with `&AtServer` or without a directive;
- the event requires `&AtServerNoContext`: reported when the handler is declared with `&AtServer`, `&AtClient` or without a directive.

Combinations whose behaviour was not measured are deliberately not reported. The `&AtClientAtServer` directive is not touched: in a form module it is already reported by `AtClientAtServerFormMethod`.

The diagnostic works in a managed form module only and skips a form borrowed by an extension (its handlers live in the base configuration's form module). A procedure bound to several events with different requirements is skipped. The finding is placed on the procedure or function name.

## Examples

### Incorrect

```bsl
&AtClient
Procedure OnCreateAtServer(Cancel, StandardProcessing)
    // the platform will never call this procedure
EndProcedure
```

### Correct

```bsl
&AtServer
Procedure OnCreateAtServer(Cancel, StandardProcessing)
    // ...
EndProcedure
```

## Sources

- Measurement on platform 8.3.27.2214: a handler declared with a directive that does not match the event's context is not bound - no build error, no warning, no exception.
- 1C:Enterprise 8.3 developer guide, section 4.8.1.3 - the availability table of compilation directives.
