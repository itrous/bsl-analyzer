# Form method with the &AtClientAtServer directive (AtClientAtServerFormMethod)

## Description

A method of a managed form module declared with the `&AtClientAtServer` compilation directive is visible to neither the client nor the server, so it cannot be called from anywhere.

This was measured on platform 8.3.27.2214 (an external data processor with a managed form, the probe method called from an `&AtServer` procedure): the form fails to initialize and `GetForm` ends with "Module initialization error: ... Procedure or function with the specified name is not defined". Without any call the form still opens, so the declaration alone does not break the form - it is dead code that fails as soon as it is called.

The `&AtClientAtServerNoContext` directive is legal in a form module: a method with it is visible to both sides. This diagnostic does not touch it - it is the directive to use when a method must be available on both sides.

The diagnostic works in a managed form module only. The `&AtClientAtServer` directive itself is part of the language and is legal, for example, in a command module.

- top-level procedures and functions are checked;
- module variables are not checked;
- the finding is placed on the method name.

## Examples

### Incorrect

```bsl
&AtClientAtServer
Procedure RefreshData()
    // ...
EndProcedure
```

### Correct

```bsl
&AtClientAtServerNoContext
Procedure RefreshData()
    // ...
EndProcedure
```

## Sources

- 1C:Enterprise 8.3 developer guide, section 4.8.1.3 - the availability table of compilation directives.
- Measurement on platform 8.3.27.2214: a method with `&AtClientAtServer` called from `&AtServer` fails the form module initialization; the same code with `&AtClientAtServerNoContext` passes.
