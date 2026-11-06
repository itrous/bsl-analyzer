# Query Execution Inside a Loop (CreateQueryInCycle)

## Description

Executing a query repeatedly inside a loop is a serious performance problem.
Each iteration adds an extra database round-trip, increases server load, and
often turns a linear task into an expensive N-query pattern.

In most cases, similar data should be fetched with one query by using list
parameters, temporary tables, `IN`, `UNION ALL`, or a batch query.

## Examples

Incorrect:

```bsl
// BanksToProcess contains a list of banks

IndividualQuery = New Query("
  |SELECT
  |   BankAccounts.Ref AS Account
  |FROM
  |   Catalog.BankAccounts AS BankAccounts
  |WHERE
  |   BankAccounts.Bank = &Bank");

For Each Bank From BanksToProcess Do
  IndividualQuery.SetParameter("Bank", Bank);
  AccountsSelection = IndividualQuery.Execute().Select();
  While AccountsSelection.Next() Do
    ProcessBankAccount(AccountsSelection.Account);
  EndDo;
EndDo;
```

Correct:

```bsl
// BanksToProcess contains a list of banks

MergedQuery = New Query("
  |SELECT
  |   BankAccounts.Ref AS Account
  |FROM
  |   Catalog.BankAccounts AS BankAccounts
  |WHERE
  |   BankAccounts.Bank IN (&BanksToProcess)");

MergedQuery.SetParameter("BanksToProcess", BanksToProcess);
AccountsSelection = MergedQuery.Execute().Select();
While AccountsSelection.Next() Do
  ProcessBankAccount(AccountsSelection.Account);
EndDo;
```

## What counts as executing a query

The diagnostic is on by default, its severity is critical, and it applies to all modules. It uses the
same settings as every other rule: the `[diagnostics]` section and severity overrides.

A finding is placed on a call in the body of a `For`, `For Each` or `While` loop, in a procedure, a
function or module code. The header of an outer loop is not a loop body.

- `Query.Execute()` and `Query.ExecuteBatch()` on a receiver that is a query on every path. The query
  text and its parameters do not have to be known.
- `QueryBuilder.Execute()` and `ReportBuilder.Execute()`. They have no batch form.
- A call to a local procedure or function of the same module from which a query execution is
  reachable, directly or through other local methods, recursion included. The chain may be of any
  length and the declaration order does not matter.

The rule looks at one module and only at what is proved. There is no finding when the receiver's type
is unknown or mixed with other types, when the call could not be resolved, or for a method of another
module or a call by string. No finding does not mean the code stays away from the database.
`ExecuteBatchWithIntermediateData` is not counted.

A finding on a call to a local method from which a query execution is reachable carries its own
code, `LocalCallWithQueryInCycle`, with a lower severity
(see [LocalCallWithQueryInCycle](LocalCallWithQueryInCycle.md)); this code keeps the query execution
itself in a loop body.

## A justified exception

The standard allows justified exceptions. When a query in a helper is intended, suppress that
execution: a directive before it clears the findings of every chain based on it alone. Write the
explanation as a separate comment, not in the code list of the directive, where every word is read as
a code.

```bsl
Procedure Read()
    Request = New Query;
    // Reading in portions limits the amount of data processed.
    // bsl-analyzer:disable-next-line CreateQueryInCycle
    Request.ExecuteBatch();
EndProcedure
```

The directive addresses the line the call starts on: one before `Request.Text = ...` or any other line
does not clear the finding. When a helper has two executions and one is suppressed, its callers keep
their findings because of the other.

A suppression in the calling code hides only the finding at that place and does not clear the helper
for other callers:

```bsl
// bsl-analyzer:off CreateQueryInCycle
For Each Item In Items Do
    Read();
EndDo;
// bsl-analyzer:on CreateQueryInCycle
```

A directive on the loop header alone does not suppress the other lines of the body. An `off` without a
matching `on` lasts to the end of the file, and an `on` without a matching `off` suppresses nothing.
There is no separate report of an unused directive. `@skip-check` is an ordinary comment.

## Sources

- [ITS: Repeated execution of similar queries (RU)](https://its.1c.ru/db/v8std#content:436)
- [v8std: #std436 Repeated execution of similar queries](https://v8std.ru/std/436/)
