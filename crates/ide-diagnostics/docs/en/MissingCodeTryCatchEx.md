# Exception block silently swallows the error (MissingCodeTryCatchEx)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

It is unacceptable to catch any exception, without any trace for system administrator.

*Incorrect*

```bsl
Try
    // code causing exception
    ....
Raise // catch any exception
EndTry;

```

As a rule, such a design hides a real problem, which is subsequently impossible to diagnose.

The current implementation classifies the `Exception` body and fires for
three cases:

- the block is empty (no statements);
- the block silently swallows the failure — it has statements, but none of
  them re-raise (`Raise` / `ВызватьИсключение`) or report the error;
- the block only rolls back the transaction (`RollbackTransaction` /
  `ОтменитьТранзакцию`) without recording or propagating the failure —
  this case emits a rollback-specific message recommending to add a log
  call or `Raise`.

Reporting covers both the platform logging API (`Message`, `WriteLogEvent`,
`Сообщить`, `ЗаписьЖурналаРегистрации`) and application/BSP helpers
recognized by name: a record-or-notify verb together with an error, log, or
event noun — `ЗаписатьОшибкуВЖурналРегистрации`,
`ДобавитьСообщениеДляЖурналаРегистрации`, `СообщитьОбОшибке`, `ShowMessageBox`,
`ЗаписатьСобытиеПодсистемы`, etc. The callee body is not read: the decision
depends only on the name. Pure formatters
(`ПодробноеПредставлениеОшибки`, `ErrorDescription`) do not count as
reporting: a block that only formats the error stays "silent".

Proper recovery paths — re-raise, logging, or a combination — do not
trigger the diagnostic.

An observable result also counts as recovery: an error structure returned to
the caller, an output counter of skipped items, a local counter returned after
the loop, or accumulated errors used by a later escaping `Raise`. The analysis
tracks a reachable value flow to a return, output parameter, or escaping
exception. An assignment inside a catch alone does not suppress the warning.

Overwrite, collection clearing, mutation through a known alias, and an unknown
call with a by-reference argument invalidate the proof. Unreachable returns do
not count. Supported formatter calls must resolve to a platform callable;
a same-named application function does not prove propagation. Incomplete
analysis or unproven value preservation keeps the diagnostic. Platform ownership also requires a complete global callable surface; an isolated file without a configuration index does not establish it.

There is one configuration nuance:

- `commentAsCode = true` changes the behavior so that a comment-only `Exception` block is not reported.

*Correct*

```bsl
Try
    // code causing exception
    ....
Raise
    // Explanation why catching all exceptions untraceable for enduser.
    // ....
    // Write to log for system administrator.
    WriteLogEvent(NStr("en = 'Action'"),
       EventLogLevel.Error,,,
       DetailErrorDescription(ErrorInfo()));
EndTry;
```

## Sources

* [Catching Exceptions in Code (RU)](https://its.1c.ru/db/v8std#content:499:hdoc)
* [v8std.ru: MissingCodeTryCatchEx (RU)](https://v8std.ru/diagnostics/bslls/MissingCodeTryCatchEx/)
