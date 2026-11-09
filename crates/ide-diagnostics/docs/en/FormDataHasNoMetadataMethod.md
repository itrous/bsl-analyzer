# Form data has no Metadata() method (FormDataHasNoMetadataMethod)

## Description

The form attribute that holds the edited object (the main attribute `Object`) is not the object itself but form data (`FormDataStructure`). That type has no `Metadata()` method, so the call `Object.Metadata()` in a managed form module fails at runtime with `Метод объекта не обнаружен (Метаданные)` («Object method not found (Metadata)»).

Measured on platform build 8.3.27.2214, in the thin client, on a managed form: `TypeOf(Form.Object)` is `FormDataStructure`, and `Form.Object.Metadata()` is refused with `Метод объекта не обнаружен (Метаданные)`. The SAME text comes back for `Form.Object.Ref.Metadata()`: `Metadata()` is a server-side method, and a reference does not have it in the thin client either. On the server both a reference (`Catalogs.<Name>.EmptyRef().Metadata()`) and the object itself answer.

So adding `.Ref` is not enough: get the metadata on the server - call `Object.Ref.Metadata()` in a procedure or function with the `&AtServer` directive (or `Metadata.<Collection>[ObjectName]`, for example `Metadata.Catalogs[ObjectName]`, when the object name is known) - and pass only the needed value, such as the name or presentation, back to the client. A `Metadata()` call left in client code is reported separately by the `UnavailableInEnvironment` diagnostic, so after moving the call into an `&AtServer` method both findings go away.

The diagnostic fires on a `Metadata` (`Метаданные`) call without arguments whose receiver type, as inferred by the analyzer, is the form data of an object (`FormDataStructure`, with or without a collection), and only in a form module. The check goes by the receiver's TYPE, not by its name: a method parameter named `Object` that holds a real reference (for example in a method with the `&AtClientAtServerNoContext` directive) is not reported. A receiver of unknown type and a union of types are not reported either. Whether the call works on a reference receiver depends on where it runs (see above); the diagnostic does not judge that.

## Examples

Incorrect:

```bsl
&AtClient
Procedure Check()
    ObjectName = Object.Metadata().Name;
EndProcedure
```

Correct:

```bsl
&AtServer
Function ObjectName()
    Return Object.Ref.Metadata().Name;
EndFunction

&AtClient
Procedure Check()
    ObjectName = ObjectName();
EndProcedure
```

Not reported (the parameter holds a real reference, not form data; the call stays server-side):

```bsl
&AtClientAtServerNoContext
Function ObjectName(Object)
    Return Object.Metadata().Name;
EndFunction
```

## Sources

- Measured on the 1C:Enterprise platform build 8.3.27.2214: the refusal text and the behaviour of a reference on the client and on the server are quoted above.
