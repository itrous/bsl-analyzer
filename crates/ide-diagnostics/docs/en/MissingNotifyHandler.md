# Notification handler missing or not exported (MissingNotifyHandler)

## Description

The platform type `ОписаниеОповещения` is also known as `NotifyDescription` and `CallbackDescription`. In the usual form, `New CallbackDescription("HandlerName", Receiver)` gives the handler name as a string. The platform calls the handler only as an EXPORTED method of the receiver module. If the procedure is declared without `Export`, or does not exist (for example, because the string contains a typo), the code still compiles and the module check passes; the failure occurs at run time as the modal error `Не найден экспортируемый метод: <HandlerName>`. In a form's `BeforeClose` flow, the form can no longer be closed.

This was measured on platform 8.3.17.1549 in a managed form module.

The diagnostic reports the registration (the constructor call, not the declaration) when the receiver module is known:

- for `ThisObject` / `ThisForm`, when the current module has no method with that name or declares it without `Export`; `ThisObject` is also checked in command modules;
- for a known common module, when that module has no method with that name or declares it without `Export`.

When provided, the optional error handler's name and receiver (the fourth and fifth arguments) are checked independently of the primary handler. In the functional form `New("Type", ParameterArray)`, handlers are checked only when the contents of a local array can be established from a tracked sequence of construction steps; unknown contents or changes outside that sequence are not guessed. Compilation conditions on registrations and declarations are taken into account; an unavailable registration is not reported.

The diagnostic stays silent when the receiver is not recognized as the current object or a known common module (for example, a local variable, including one containing a command module, or a module the analysis cannot see), when a module body could not be read, when the receiver is not a plain name, and in ordinary form modules. If form metadata identifies a base form, the local handler is not checked because the base form may provide it. This suppression applies to the local handler; a known common-module receiver is still checked. The rule also stays silent if the form kind or metadata is unknown. Registrations inside comments are ignored. `AttachIdleHandler` and `SetAction` handlers are not checked by this diagnostic.

An empty handler name is currently reported as missing, and the current message suggests creating an exported procedure or correcting the name. The separate `CommandModuleExportMethods` rule exempts an exported command-module method when it is recognized as a notification handler on the current object.

## Examples

### Incorrect

```bsl
Procedure BeforeCloseHandler()
    Notification = New NotifyDescription("CloseCompleted", ThisObject);
EndProcedure

Procedure CloseCompleted(Result, Parameters) // no Export
EndProcedure
```

### Correct

```bsl
Procedure CloseCompleted(Result, Parameters) Export
EndProcedure
```

## Sources

- Measurement on platform 8.3.17.1549: a non-exported or missing notification handler fails only at run time with `Не найден экспортируемый метод`.
