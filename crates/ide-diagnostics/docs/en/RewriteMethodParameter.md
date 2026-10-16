# Rewrite method parameter (RewriteMethodParameter)

## Description

A by-value parameter that is overwritten before any meaningful use is suspicious: the caller passes a value, but the method immediately ignores it.

Usually this means one of two things:

- the parameter should be removed and replaced with a local variable;
- the parameter name or method contract is misleading.

## Examples

### Incorrect

```bsl
Procedure Configor(Val ConnectionString, Val User = "", Val Pass = "") Export
    ConnectionString = "/F""" + DataBaseDir + """";
EndProcedure
```

### Correct: use a local variable

```bsl
Procedure Configor(Val User = "", Val Pass = "") Export
    ConnectionString = "/F""" + DataBaseDir + """";
EndProcedure
```

### Correct: use the parameter value for its intended purpose

```bsl
Procedure Configor(Val DataBaseDir, Val User = "", Val Pass = "") Export
    If Not EmptyString(DataBaseDir) Then
        NewConnectionString = "/F""" + DataBaseDir + """";
    Else
        NewConnectionString = DefaultConnectionString;
    EndIf;
EndProcedure
```

The current implementation is narrower and more precise than a simple textual scan:

- it only checks by-value parameters (`Val` / `Знач`);
- for a target inside `While`, it follows reachable paths from method entry: a first blind write is not hidden by loop-back definitions, while an earlier real write on the same path suppresses a later finding; an enclosing `For` or `For Each` header counts as a write to its loop variable;
- it suppresses diagnostics when the parameter is read in the right-hand side or in any statement that executes before the rewrite — including the condition of an enclosing `If`/`While`/loop header (the `If Param = Undefined Then Param = …` default-initialization idiom is *not* reported);
- self-assignments like `Param = Param` are skipped and do not count as meaningful use;
- unreachable writes after `Break` or `Continue` are not reported;
- targets outside `While` retain the previous reaching-definitions check.

Parameter names are compared case-insensitively, including Cyrillic letters.

## Sources

- Generic static-analysis rule about overwritten parameters.
- [PVS-Studio V763. Parameter is always rewritten in function body before being used](https://pvs-studio.com/ru/docs/warnings/v6023)
