# Built-in function name used as a declared function name (BuiltinFunctionNameAsDeclaration)

## Description

The 1C:Enterprise 8.3.27.2074 compiler rejects a function declared with the name
of the built-in function `Найти` (`Find`) with the message «Ожидается имя
функции» ("function name expected"), and the configuration does not start. The
diagnostic reports this before the run and highlights the name in the function
declaration.

The rule is enabled by default and has type Error with `Blocker` severity: the
editor shows the finding as an error. Its severity and enablement are configured
the usual way, like any other rule.

Rule boundaries:

- exactly one name pair is checked: `Найти` and `Find`, case-insensitively
  (`нАйТи`, `fInD` are reported as well); the message keeps the name as written;
- only function declarations are checked. Procedures named `Найти`/`Find`,
  variables and parameters are not reported;
- `Export`, parameters and the function body do not affect the check;
- the rule does not depend on the module kind, execution environment, the
  platform version from the project settings, or the loaded platform help: it
  fires in a plain file without metadata as well;
- calls `Find(...)` and collection methods `Array.Find(...)` are not reported;
- other built-in functions (`StrFind`, `СтрНайти` and the rest) and names that
  start with `Find` are not reported: the rule does not treat the whole platform
  surface as reserved.

## Examples

### Incorrect

The compiler rejects the declaration:

```bsl
Function Find(Value) Export
    Return Value;
EndFunction
```

### Correct

The function name does not match the built-in function:

```bsl
Function GetValue(Value) Export
    Return Value;
EndFunction
```

A collection's `Find` method called inside a function with another name:

```bsl
Function IndexInArray(Array, Value) Export
    Return Array.Find(Value);
EndFunction
```

## Evidence

- The compiler rejection was observed on the declaration
  `Функция Найти(Значение) Экспорт` in a non-global common module
  (`Global=false`, `Server=true`, `ExternalConnection=true`) on platform
  8.3.27.2074 — see the issue.
- `Find` is the English name of the same built-in function. Extending the
  rejection to `Find`, to other letter cases, to other module kinds and platform
  versions is an assumption by analogy, not a separate compiler observation.
- Procedures and other built-in names are not checked because there are no
  observations for them, not because the compiler is known to accept them.
- No 1C development standard forbidding such a name was found; the rule rests on
  the compiler observation, not on a standard.

## Sources

- [Issue #806: a function declared as `Найти` is rejected by the compiler](https://github.com/itrous/bsl-analyzer/issues/806)
