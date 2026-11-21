# Notification handler missing or not exported (MissingNotifyHandler)

## Description

`New NotifyDescription("HandlerName", Receiver)` names its handler by a string. The platform calls that handler only as an EXPORTED method of the receiver module. If the procedure is declared without `Export`, or does not exist at all (a typo in the string), the code still compiles and the module check passes; the failure shows at run time as the modal error `Не найден экспортируемый метод: <HandlerName>`, and in a form's `BeforeClose` flow the form can no longer be closed.

This was measured on platform 8.3.17.1549 in a managed form module.

The diagnostic reports the registration (the `New NotifyDescription` call, not the declaration) when:

- the receiver is `ThisObject` / `ThisForm` and the module declares no method with that name, or declares it without `Export`;
- the receiver is a common module of the configuration and that module declares no method with that name, or declares it without `Export`.

It stays silent when the receiver is not a known common module (a local variable, a module the analysis cannot see), when a module body could not be read (absence is not provable), when the receiver is not a plain identifier, and in ordinary (thick client) form modules. Registrations inside comments are not registrations and are ignored. `AttachIdleHandler` and `SetAction` handlers are not checked.

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
