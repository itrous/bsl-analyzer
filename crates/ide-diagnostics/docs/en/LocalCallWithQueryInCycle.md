# Local Call Reaching a Query Inside a Loop (LocalCallWithQueryInCycle)

## Description

This diagnostic marks a call to a local method of the same module in a loop body when a query
execution is reachable from the called method, directly or through a chain of other local methods.
Every iteration of the loop may cause a database round-trip, so the required data should be fetched
with one query before the loop.

The finding sits on the CALL in the loop body. The query execution inside the called method is
reported separately by `CreateQueryInCycle`, when it is lexically in a loop body. The call chain may
be of any length and goes only through methods of the same module.

Incorrect:

```bsl
Function FindByValue(Value)
    Query = New Query("SELECT 1 AS Field");
    Query.SetParameter("Value", Value);
    Selection = Query.Execute().Select();
    Return ?(Selection.Next(), Selection.Field, Undefined);
EndFunction

Procedure FillRows(Table, Reader)
    While Reader.Read() Do
        NewRow = Table.Add();
        NewRow.Field = FindByValue(Reader.Value);
    EndDo;
EndProcedure
```

Correct:

```bsl
Procedure FillRows(Table, Values)
    Query = New Query("SELECT T.Code AS Value, T.Description AS Field FROM Catalog.Items AS T WHERE T.Code IN (&Values)");
    Query.SetParameter("Values", Values);
    Selection = Query.Execute().Select();

    While Selection.Next() Do
        NewRow = Table.Add();
        NewRow.Field = Selection.Field;
    EndDo;
EndProcedure
```

## Analysis boundaries

This is a reachability candidate, not a proven query on every iteration: the called method may reach
the query execution not on every path (for example, only under a condition). That is why the severity
is below the direct query execution in a loop (`CreateQueryInCycle`) and equals that of
`ImplicitDatabaseReadInCycle`.

The rule looks at one module only. There is no finding when the called method belongs to another
module, when the call could not be resolved, or for a call by string. Suppressing the query execution
in the called method with a `CreateQueryInCycle` directive also clears the findings of every chain
based on it alone.

## Sources

- [Standard: Repeated execution of similar queries (RU)](https://its.1c.ru/db/v8std#content:436)
- [v8std: #std436 Repeated execution of similar queries](https://v8std.ru/std/436/)
