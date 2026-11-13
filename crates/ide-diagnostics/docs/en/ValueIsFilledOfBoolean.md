# ValueIsFilled of a Boolean (ValueIsFilledOfBoolean)

## Description

`ValueIsFilled(X)` answers whether a value differs from the default value of its type. For the `Boolean` type the answer is always `True`: the platform counts `False` as a filled value too. So the condition `If ValueIsFilled(Flag) Then` with a Boolean `Flag` is always taken and verifies nothing; the author almost certainly meant `If Flag Then`.

Measured on two platform builds, with identical results:

| Expression | 8.3.17.1549 | 8.3.27.2214 |
|---|---|---|
| `ValueIsFilled(True)` | Yes | Yes |
| `ValueIsFilled(False)` | Yes | Yes |
| `ValueIsFilled(0)` | No | No |
| `ValueIsFilled("")` (empty string) | No | No |
| `ValueIsFilled(Undefined)` | No | No |

The platform accepts such a call without an error or a warning: the function is generic and legitimately returns "Yes" or "No" for any value, so there is no compile error, and the check is shown to be pointless only by the type of the argument.

The diagnostic fires on a `ValueIsFilled` call with one argument that is `Boolean` by construction: a `True` or `False` literal, a comparison result (`A = B`), `And`, `Or`, `Not`, a Boolean attribute read through a reference to a metadata object (`Ref.Flag`; it has no empty value, unlike a field of a query selection or a value-table row, where a Boolean column can arrive as `NULL`), or a local variable to which this procedure assigns only such values, whose first assignment stands at the top level of the procedure before the call, in a procedure without `Goto` (a jump could bypass that assignment), and which is never passed to another call (it could receive its value through an out-parameter, as in `Structure.Property("Key", Variable)`).

Everything else is not reported: type inference drops `Undefined` and unknown arms of a union, so a variable that is `Undefined` on one path and Boolean on another, or the result of a function that returns both a Boolean and something unknown, looks like a plain `Boolean` although the check on it is meaningful. Also not reported: a composite type that merely contains `Boolean`, a value of unknown type, a parameter, a module variable, a field of a query selection, a value-table row or a structure, a field whose receiver is of unknown type, an index and a call result.

## Examples

Incorrect:

```bsl
If ValueIsFilled(IsDone) Then
    StartProcessing();
EndIf;
```

Correct:

```bsl
If IsDone Then
    StartProcessing();
EndIf;

If Not IsDone Then
    PrepareProcessing();
EndIf;
```

## Sources

- Measured on the 1C:Enterprise platform builds 8.3.17.1549 and 8.3.27.2214: the values in the table above come from a live call on both builds.