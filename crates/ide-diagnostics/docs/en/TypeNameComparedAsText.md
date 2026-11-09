# Type name compared as text (TypeNameComparedAsText)

## Description

The expression `String(TypeOf(Value))` turns a type name into text, and that text depends on the interface language: the same value renders differently in the Russian and the English interface. If a decision is taken on such text, then when the language changes the condition silently stops matching: there is no error, the intended branch just no longer runs.

The platform does not catch it: the comparison runs without an error and gives the expected result in the developer's language. This is a code-standards finding, not a runtime error.

The correct way to compare types is `Type()`: `TypeOf(Value) = Type("CatalogRef.Products")`. Such a comparison does not depend on the interface language.

The diagnostic fires when the type name is COMPARED WITH A STRING LITERAL. All three parts are required: a type name rendered as text, a comparison, and a literal on the other side. A literal is a string in the source, `NStr("...")`, or a local variable assigned only string literals in the procedure; a parameter, a variable of unknown provenance or any other expression does not count (the comparison may then be type-to-type).

- The type name is `String(TypeOf(...))` (`Строка(ТипЗнч(...))`), written in place or taken from a local variable that is assigned only such a call in the procedure (one level of variable; if the variable is also assigned anything else, there is no finding).
- The name may be wrapped in `Left`, `Right`, `TrimAll`, `TrimL`, `TrimR`, `Upper`, `Lower`, `StrLen`, in any nesting.
- A comparison is `=` or `<>` with a string literal on either side, `Find(name, "literal")` compared with any number literal (`> 0`, `= 1`, `<> 0` and so on, the number on either side), `StrStartsWith` and `StrEndsWith` with a literal.

The finding sits on the `String(...)` call when it is written in place, and on the comparison when the name comes through a variable.

A type name that is merely printed, logged, put into an exception text, concatenated, returned or stored is NOT reported: nothing depends on the text there. A comparison with another type name or with a variable, rather than a literal, is not reported either.

## Examples

Incorrect:

```bsl
If String(TypeOf(Value)) = "CatalogRef.Products" Then
    ProcessProduct(Value);
EndIf;
```

The same through a variable (also reported):

```bsl
Name = String(TypeOf(Object));
If Left(Name, 24) = "External data processor " Then
```

Correct:

```bsl
If TypeOf(Value) = Type("CatalogRef.Products") Then
    ProcessProduct(Value);
EndIf;
```

Not reported (the type name is only shown to a human):

```bsl
Raise "Unexpected type: " + String(TypeOf(Value));
```