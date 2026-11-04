# Type mismatch (TypeMismatch)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

This diagnostic is reported when a call argument's inferred type is incompatible with the parameter types of every arity-compatible candidate.

Currently, `TypeMismatch` is active for call-argument validation. When a function or method is called, the analyzer collects all candidate signatures, evaluates arity and applicability, and reports a mismatch only when the call is rejected.

### Candidate Resolution Flow

The analyzer resolves each call into exactly one of three mutually exclusive selection states:

1. **Unique**: After ranking, exactly one candidate is the best known-applicable survivor. Its parameter types are used for validation.
2. **Ambiguous**: Either multiple known-applicable candidates tie for the best score, or one or more indeterminate candidates survive while no known-applicable candidate exists. If at least one candidate survives, no `TypeMismatch` is reported.
3. **Rejected**: No candidate survives. The arguments are incompatible with every arity-compatible signature, or no candidate matches the call's arity. `TypeMismatch` fires only for a type rejection, reporting the deterministic least-incompatible rejected candidate's first incompatible argument.

### Key Rules

- **Doc-Comment Split**: Mismatches where the expected type is derived from JSDoc comments are reported as `TypeMismatchByDocComment` to distinguish between platform-enforced types and user-defined documentation.
- **Unknown Arguments**: An argument or parameter of type `Unknown` is indeterminate and does not itself cause `TypeMismatch`. Other concrete incompatible arguments can still reject the call.
- **Overlapping Types**: The diagnostic fires only when the expected and the actual type sets share no member. A value of type `Number | Undefined` inferred from code branches is not reported in a `Number` parameter, because some of its values fit. When a call has several such arguments, the first argument sharing no member is reported.
- **Ambiguity**: Ambiguity in type resolution does not trigger a `TypeMismatch`. The diagnostic only fires when the candidate selection is explicitly rejected.

## Examples

```bsl
// ValueTable.Insert expects a Number for the index
ValueTable.Insert("not a number"); // Type mismatch: expected 'Number', found 'String'

Position = Undefined;
If Condition Then
    Position = 5;
EndIf;
Result = Left("abc", Position); // No TypeMismatch: 'Number | Undefined' overlaps 'Number'; this is the PossiblyUndefinedValue hint's case
```

## Sources

- Internal type-inference based diagnostic in `hir-ty`
- Platform metadata and curated overlays in `bsl-platform`
