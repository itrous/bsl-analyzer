# Possibly undefined value (PossiblyUndefinedValue)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

A weak hint: a call argument gets a value of the expected type on one branch of the code and stays `Undefined` on another, which the parameter does not accept.

The code is not necessarily wrong: a value that is `Undefined` on one branch is legal BSL. The hint only points at a place where the call will fail if execution takes that branch.

### When it fires

`TypeMismatch` stays silent when the expected and the inferred type sets overlap: some value of the expression is acceptable to the parameter. `PossiblyUndefinedValue` picks out one case of that rule - the inferred set contains `Undefined` and the expected set does not.

### Key Rules

- **Decided by type, not by text**: the presence of `Undefined` is determined from the expression's type, not from its display name, so the rule does not depend on the interface language.
- **Parameter accepts `Undefined`**: nothing is reported when the expected set itself contains `Undefined`.
- **Disjoint types**: when the sets do not overlap, `TypeMismatch` applies instead of this hint.
- **Overlap without `Undefined`**: stays fully silent.
- **One argument per call**: when a call has an argument with a disjoint type, that argument gets `TypeMismatch` and the hint is not reported.

## Examples

```bsl
Procedure Test(Condition)
    Position = Undefined;
    If Condition Then
        Position = 5;
    EndIf;
    // Left expects Number, Position is Number | Undefined
    Result = Left("abc", Position);
EndProcedure
```

## Sources

- Internal type-inference based diagnostic in `hir-ty`
