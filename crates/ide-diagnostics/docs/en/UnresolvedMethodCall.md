# Call to an unresolved method (UnresolvedMethodCall)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

The diagnostic reports a qualified method call that cannot be resolved confidently by the analyzer.

This usually means one of the following:

- the common module name is wrong;
- the method name contains a typo;
- the method exists but is not exported;
- metadata refers to a common module whose source file is missing from the workspace.

The current implementation is conservative and focuses on qualified calls such as `CommonModule.Method()`, where the resolver has enough information to produce a useful error.

`GetPredefinedItemName` (`ПолучитьИмяПредопределенного`) is supported by catalog, chart of characteristic types, chart of accounts and chart of calculation types managers. A separately attested fact fills the help extraction gap while preserving the reference parameter and manager availability. It neither adds the method to document managers nor accepts incompatible arguments.

## Platform types

A method call on a value of a platform type is checked only where the completeness of that type's member surface is attested (`type_member_surface_attestations` in `crates/bsl-platform/data/platform_overlays.json`). Such a type counts as closed: the platform data is known to list every method it has, so calling a method it does not have is a defect of the code rather than a gap in the data.

- the type must be attested, and the platform data the analyzer serves must be of the release the surface was verified against or newer; when that version cannot be determined, the check stays silent;
- a homonymous type name (one spelling shared by two different platform types) stays silent as well: which of them a receiver denotes is unknown;
- a call on a platform type that is not attested stays silent: its surface is not known to be complete.

```bsl
Writer = New JSONWriter;
Writer.WriteTableBegin(); // <-- JSON writing has no such method
```

A value table counts as the platform type `ValueTable`: its columns are members of its rows, so the table's own members are the attested ones whatever columns it has. The check applies where the table is built by `New ValueTable` on every path reaching the call.

```bsl
Table = New ValueTable;
Table.Read(); // <-- a value table has no such method
```

## Examples

Incorrect:

```bsl
CommonModule.UnknownMethod();
```

Correct:

```bsl
CommonModule.KnownMethod();
```

## Calls through ThisObject / ThisForm in a form module

Only the exported methods of a form module are reachable through `ThisObject` and `ThisForm`. A non-exported method does not break the module, but the call fails at run time with "Object method not found" (checked on 8.3.17 and 8.3.27, on the client and on the server).

Incorrect:

```bsl
&AtClient
Procedure Show()
EndProcedure

&AtClient
Procedure Save()
    ThisForm.Show();
EndProcedure
```

Correct:

```bsl
&AtClient
Procedure Save()
    Show();
EndProcedure
```

or declare the method `Export`.
