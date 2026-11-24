# Variable is assigned to itself (SelfAssign)

## Description

Assigning a variable or property to itself has no effect and usually indicates a typo or a mistaken copy-paste edit.

The current implementation is narrow and HIR-based:

- it reports assignments where the left and right sides resolve to the same path;
- matching is case-insensitive, because BSL identifiers are case-insensitive;
- simple property self-assignments such as `Object.Field = Object.Field` are also reported.

Unary operators are preserved during comparison: `Amount = -Amount` and `Row.Amount = -Row.Amount` compute a new value and are not reported as self-assignments.

## Examples

### Incorrect

```bsl
Amount = Amount;
Structure.Field = Structure.Field;
```

### Correct

```bsl
Amount = TotalAmount;
Structure.Field = NewValue;
```

## Sources

- General BSL assignment semantics.
- [v8std.ru: SelfAssign](https://v8std.ru/diagnostics/bslls/SelfAssign/)
