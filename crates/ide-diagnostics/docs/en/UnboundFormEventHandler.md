# Unbound form event handler (UnboundFormEventHandler)

## Description

The platform binds a managed form event handler only through an `<Event>` element in `Ext/Form.xml`. The procedure name in the form module means nothing: an `OnOpen` procedure that no form event points to is never called.

This was measured on platform 8.3.27.2214: there is no build error, no warning, no runtime exception and no event log entry - only the absence of an effect shows. The defect is found by an expected action not happening.

The diagnostic reports a procedure or function when all of the following hold:

- the module belongs to a managed form that has no base form;
- the method name (case-insensitive) equals the canonical name of a form-level event handler: `OnCreateAtServer`, `OnOpen`, `BeforeWrite`, `NotificationProcessing`, `ChoiceProcessing`, `AfterWrite` and others (the Russian canonical names are matched);
- no form event and no command points to the method;
- the method is not exported;
- the method is not called anywhere in the module, and its name does not appear in a string literal of the module (wait handlers, notifications, binding by name).

Most often the fix is to ADD the event binding in the form, not to delete the procedure: judging by the name, its author expected it to work.

The diagnostic is deliberately silent about exported methods (another module may call them), forms with a base form (the handlers of a borrowed form live in the base configuration's module), names outside the handler table and ordinary (non-managed) forms. One declaration does not get a second `UnusedLocalMethod` finding.

## Examples

### Incorrect

```bsl
&AtClient
Procedure OnOpen()
    // the OnOpen event in Form.xml does not point to this procedure
EndProcedure
```

### Correct

```xml
<Events>
    <Event name="OnOpen">OnOpen</Event>
</Events>
```

## Sources

- Measurement on platform 8.3.27.2214: a form handler is bound only through `<Event>` in `Ext/Form.xml`, never by procedure name.
- 1C:Enterprise 8.3 developer guide - managed form events and their handlers.