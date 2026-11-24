# Violation of pairing using methods "BeginTransaction()" & "CommitTransaction()" / "RollbackTransaction()" (PairingBrokenTransaction)

## Description

This diagnostic reports broken transaction pairing inside a single method.

The public rule is straightforward: `BeginTransaction()` must be matched by
either `CommitTransaction()` or `RollbackTransaction()`. In the current project
the check is path-sensitive: it analyzes all execution paths and reports both
unclosed transactions and orphaned `CommitTransaction()` /
`RollbackTransaction()` calls.

The implementation also supports a local safety limit, `maxTransactionLevel`,
to avoid pathological traversal depth.

The analysis correlates simple flag conditions: when `BeginTransaction()` and
its matching `CommitTransaction()` / `RollbackTransaction()` are guarded by the
same condition (e.g. `If LocalTransaction Then …`), infeasible paths where the
flag would be both true and false are dropped, so no false positive is raised.
Correlation applies only to stable variables that are assigned at most once.

When a path already holds an unfinished local transaction, a direct
`TransactionActive()` check uses that state. A guarded rollback in the catch
therefore does not invent a path that skips rollback. An early `Continue`
before the transaction and a `Return` after rollback do not break pairing.
An arbitrary flag, a qualified same-named method, or a check without a proven
local begin does not prove a valid pair. A reachable exit with an unfinished
transaction remains a diagnostic. The active-state proof requires a confirmed platform call and a complete global callable index; a same-named application function does not satisfy it.

## Examples

Correct:

```bsl
Procedure SaveData()
    BeginTransaction();
    Try
        DocumentObject.Write();
        CommitTransaction();
    Except
        RollbackTransaction();
        Raise;
    EndTry;
EndProcedure
```

Incorrect:

```bsl
Procedure StartWrite()
    BeginTransaction();
    WriteDocument();
EndProcedure

Procedure WriteDocument()
    Try
        DocumentObject.Write();
        CommitTransaction();
    Except
        RollbackTransaction();
    EndTry;
EndProcedure
```

## Sources

- Source: [1C standard: Transactions, rules of use (#std783)](https://its.1c.ru/db/v8std#content:783:hdoc)
- Secondary reference: [v8std.ru: PairingBrokenTransaction](https://v8std.ru/diagnostics/bslls/PairingBrokenTransaction/)
