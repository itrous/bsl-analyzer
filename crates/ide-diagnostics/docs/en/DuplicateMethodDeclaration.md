# Duplicate method declaration (DuplicateMethodDeclaration)

Reports procedures or functions with the same case-insensitive name in one
physical module when a known compilation context admits both declarations.
Different parameter counts and procedure/function kinds do not create overloads.
External data processor and external report modules are included.

```bsl
Procedure Apply(Area, FullName)
EndProcedure

Procedure Apply(Area, Department, FullName)
EndProcedure
```

Both declarations are reported as errors. Combine them into one method or give
different operations different names. There is no automatic fix because removing
a declaration can change behavior.

Declarations in different modules do not conflict. Known mutually exclusive
preprocessor branches and valid separate client/server contexts are excluded.
Unknown conditions do not establish that both declarations exist together.
Variables belong to a separate namespace.

The check reads the source declarations of one file. An extension method and the
base method it overrides are not duplicates in one physical module.

This is a separate error in the full-module result. The absence of `ParseError`
alone does not establish that a module passes compilation checks.
