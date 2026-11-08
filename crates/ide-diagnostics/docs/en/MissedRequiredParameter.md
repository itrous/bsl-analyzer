# Missed a required method parameter (MissedRequiredParameter)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

A call of a procedure or function must have at least as many positions as the
method has required parameters. With fewer positions the platform rejects the call
with `Недостаточно фактических параметров` ("not enough actual parameters"). The
diagnostic reports such calls and names the parameters that have no position.

A position left empty between commas is not a platform error: the callee receives
`Undefined` there, even for a required parameter, and the diagnostic does not report
that call. Standard #std640 still advises against skipping required parameters,
because the callee may not cope with `Undefined`; this diagnostic does not check
that advice.

The current implementation reports only calls that were semantically resolved to
their target, so the analyzer knows which parameters are required.

Limitation: a call to a method of the same module that holds no expression at all,
such as `Method(, )`, counts as one position shorter, so its last parameter is
reported even though the platform accepts the call.
## Examples

Given:

```bsl
Procedure ChangeFormFieldColor(Form, FieldName, Color)
```

Incorrect:

```bsl
ChangeFormFieldColor(ThisObject); // no position for FieldName and Color
ChangeFormFieldColor(); // no position for any required parameter
```

Correct:

```bsl
ChangeFormFieldColor(ThisObject, "Result", Color);
ChangeFormFieldColor(, "Result", Color); // Form receives Undefined
```

## Sources

* [Parameters of procedures and functions (RU)](https://its.1c.ru/db/v8std#content:640:hdoc)
* [Public mirror: v8std.ru / #std640](https://v8std.ru/std/640/)
