# Notification handler missing or not exported (MissingNotifyHandler)

## Description

`New CallbackDescription("HandlerName", Receiver)` names its handler by a string. `NotifyDescription` and `ОписаниеОповещения` are also recognized. The platform calls the handler only as an EXPORTED method of the receiver module. If the procedure is declared without `Export`, or does not exist at all (a typo in the string), the code still compiles and the module check passes; the failure shows at run time as the modal error `Не найден экспортируемый метод: <HandlerName>`, and in a form's `BeforeClose` flow the form can no longer be closed.

This was measured on platform 8.3.17.1549 in a managed form module.

The diagnostic reports the registration (the `New NotifyDescription` call, not the declaration) when:

- the receiver is `ThisObject` / `ThisForm` and the module declares no method with that name, or declares it without `Export`;
- the receiver is a common module of the configuration and that module declares no method with that name, or declares it without `Export`.

When provided, the optional error handler's name and receiver (the fourth and fifth arguments) are checked independently of the primary handler. Compilation conditions on the registration and declaration are taken into account; an unavailable registration is not reported.

The diagnostic stays silent when the receiver is not recognized as the current object or a known common module (for example, a local variable, including one containing a command module, or a module the analysis cannot see), when a module body could not be read, when the receiver is not a plain identifier, and in ordinary form modules. A form with a base form does not have its local handler checked: the base form may supply it. The rule also stays silent if the form kind or metadata is unknown. Unknown contents of the parameter array in the functional `New(Type, ParameterArray)` form are not guessed. Registrations inside comments are ignored. `AttachIdleHandler` and `SetAction` handlers are not checked.

Currently an empty handler name is also reported as missing, with a suggestion to create an exported procedure or correct the name. The separate `CommandModuleExportMethods` rule reports exported methods in command modules; the recommendations are not automatically reconciled.

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
