# Call to an undefined procedure or function (UnresolvedBareCall)

## Description

The diagnostic reports a call written without a receiver — `Name(...)` — whose name
belongs to nothing the analyzer can see:

- no method of the module the call is written in;
- no method of the module's implicit receiver: the object, record set or manager in
  their modules, the managed form and the extension its main attribute adds in a form
  module, the value manager in a constant's value-manager module;
- no export of a global common module or of an application module;
- no global function of the platform.

A variable does not own a call name: a parameter, a `Перем`, a local or an export
variable of an application module of the same name does not make `Name(...)` valid — a call looks among methods only.

In 1C:Enterprise this is not a cosmetic problem. A call to an undefined procedure
breaks compilation of the WHOLE module at run time: the thin client dies in a modal
error, the real text — "Процедура или функция с указанным именем не определена" —
reaches only the infobase event log, and every other method of the module stops
working with it.

The usual causes are a typo in the name, a method that was renamed or deleted, and a
procedure that was expected to be exported from a global common module but is not.

## When the diagnostic stays silent

Only a proven absence is reported. The rule says nothing whenever the picture it
would judge by is incomplete:

- the workspace has no configuration root, or it has not finished loading;
- a global common module or an application module exists but its body could not be
  read — an unknown export may own this very name;
- the bundled platform catalog is missing, is not attested for the target platform
  version, or the target platform is not supported;
- the platform catalog does not attest complete method and field lists for an
  implicit receiver type: without both lists, the analyzer cannot prove that the
  bare name is absent (even though this diagnostic concerns calls);
- the call is written in an ordinary form's module — its dialog is stored in binary
  form, and the module also sees the exports of the main attribute's object module —
  or in a form module whose `Form.xml` could not be read;
- the callee is a global function the platform compiles although its help does not
  describe it: `LocaleCode()`, `SetApplicationCaption()`, `GetApplicationCaption()`,
  `УстановитьЗаголовокСистемы()`, `ПолучитьЗаголовокСистемы()` (8.2 names, checked on
  8.3.17 and 8.3.27);
- the call is written in a common module with no environment selected (server, server
  call, external connection, clients): the platform compiles such a module nowhere;
- the call sits in an `#If` branch the platform compiles in none of the method's
  environments (say, `#If Client` inside a server-side form procedure): such a branch
  cannot break the module. Any `#If` branch of a body whose environment is unknown —
  say, `#Insert` code of a `&ChangeAndValidate` method — is silent too.

A call with a receiver (`CommonModule.Method()`) belongs to `UnresolvedMethodCall`.
A read of an undefined name outside a call belongs to `UnresolvedName`; while that
rule is enabled it also owns the callee token, and this one steps aside.

## Examples

Incorrect:

```bsl
Procedure Test()
    DecodeString("x");
EndProcedure
```

Correct:

```bsl
Procedure Test()
    DecodeString("x");
EndProcedure

Function DecodeString(Value)
    Return Value;
EndFunction
```
