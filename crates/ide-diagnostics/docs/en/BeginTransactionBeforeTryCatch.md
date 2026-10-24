# Violating transaction rules for the 'BeginTransaction' method (BeginTransactionBeforeTryCatch)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

`BeginTransaction()` must be called immediately before `Try` and outside the
`Try ... Except` block.

If executable code appears between `BeginTransaction()` and `Try`, an exception
may leave the transaction open before the rollback handler is reached. Starting
the transaction inside `Try` is also unsafe: the exception handler may then work
with a transaction state different from the one the code expects.

The `Try` that matters is the transaction's own one. A `BeginTransaction()`
followed directly by its own `Try` is correct even when the whole pair sits
inside an outer `Try` (for example, one that releases a lock for editing): the
pair is the same as a call of a method that contains it.

## Examples

### Incorrect

```bsl
BeginTransaction();
PrepareData();
Try
    SaveDocument();
Except
    RollbackTransaction();
EndTry;
```

### Correct

```bsl
BeginTransaction();
Try
    SaveDocument();
    CommitTransaction();
Except
    RollbackTransaction();
    Raise;
EndTry;
```

The transaction may be started conditionally when `BeginTransaction()` ends a
branch of `If` and `Try` follows `EndIf` directly: nothing runs between the
start of the transaction and `Try`.

```bsl
If OwnTransaction Then
    BeginTransaction();
EndIf;
Try
    SaveDocument();
    If OwnTransaction Then
        CommitTransaction();
    EndIf;
Except
    If OwnTransaction Then
        RollbackTransaction();
    EndIf;
    Raise;
EndTry;
```

## Sources

Primary source: [Transactions: Rules of Use (RU)](https://its.1c.ru/db/v8std/content/783/hdoc/_top/)

Secondary source: [v8std.ru: #std783 Transactions: Rules of Use](https://v8std.ru/std/783/)

Additional reference: [v8std.ru: begin-transaction](https://v8std.ru/diagnostics/v8-code-style/begin-transaction/)
