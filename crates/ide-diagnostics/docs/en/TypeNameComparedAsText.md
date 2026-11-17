# Type name compared as text (TypeNameComparedAsText)

## Description

The expression `String(TypeOf(Value))` turns a type name into text, and that text depends on the interface language: the same value renders differently in the Russian and the English interface. If a decision is taken on such text, then when the language changes the condition silently stops matching: there is no error, the intended branch just no longer runs.

The platform does not catch it: the comparison runs without an error and gives the expected result in the developer's language. This is a code-standards finding, not a runtime error.

The correct way to compare types is `Type()`: `TypeOf(Value) = Type("CatalogRef.Products")`. Such a comparison does not depend on the interface language.

The diagnostic fires when the text of a type name is COMPARED (`=` or `<>`) WITH A STRING LITERAL written in place, on either side of the comparison.

What counts as the text of a type name is not decided by the spelling of a function. It is decided by following where the value came from: it must be the text of a type name on EVERY path by which execution reaches the comparison. So a value that arrives through a local variable, or through a branch, is reported as well, provided every assignment that reaches the comparison yields such text. The text is preserved by the functions the platform data marks as preserving it: `String`, `Upper`, `Lower`, `Title`, `TrimAll`, `TrimL`, `TrimR` (and their Russian spellings), nested in any order.

The finding sits on the call when the text is produced in place, and on the comparison when the value arrives through a variable.

### When there is NO finding

- The text of a type name is only printed, logged, put into an exception text, concatenated, returned or stored: nothing depends on the text there.
- The other side of the comparison is not a literal but a parameter, a variable or any other expression: the comparison may well be type-to-type.
- Between producing the text and the comparison the value may have been overwritten in a way the walk cannot see through: a call PROVEN to take the argument by reference, or `Execute`. A call the platform data attests as taking its argument by value does not suppress the finding.
- The value comes from a parameter, a module variable, a loop variable, or a call the walk could not resolve.

### What the diagnostic does NOT catch yet

These shapes require identifying a function by name, which this diagnostic does not do; they will be covered by separate curated platform data:

- `Find(TypeName, "literal")` compared with a number;
- `StrStartsWith` and `StrEndsWith` with a literal;
- a literal arriving as `NStr("...")` or through a variable;
- the `Left` and `Right` wrappers, which take a part of the text.

## Examples

Incorrect:

```bsl
If String(TypeOf(Value)) = "CatalogRef.Products" Then
    ProcessProduct(Value);
EndIf;
```

The same through a variable and through a text-preserving wrapper (also reported):

```bsl
Name = Upper(String(TypeOf(Object)));
If Name = "CATALOGREF.PRODUCTS" Then
```

Correct:

```bsl
If TypeOf(Value) = Type("CatalogRef.Products") Then
    ProcessProduct(Value);
EndIf;
```

Not reported (the type name is only shown to a person):

```bsl
Raise "Unexpected type: " + String(TypeOf(Value));
```
