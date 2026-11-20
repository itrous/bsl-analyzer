# Object Attributes Modified in the OnWrite Handler (ObjectModifiedInOnWrite)

## Description

Standard #std465 says: «Запрещается в данном обработчике изменять содержимое записываемого объекта, поскольку на момент выполнения обработчика, объект уже записан в БД.» (it is forbidden to change the contents of the object being written in this handler, because by the time the handler runs the object is already written to the database).

Assigning to an attribute or a tabular section of the object itself inside the `OnWrite` / `ПриЗаписи` handler of an object module does not reach the database: the object is already written. Object attributes must be filled in the `BeforeWrite` / `ПередЗаписью` handler (#std464).

The diagnostic fires on an assignment to a name that is an attribute, a common attribute or a tabular section of the object itself (`Warehouse = ...` or `ThisObject.Warehouse = ...`). These are not reported: procedure parameters, variables declared with `Var` / `Перем` in the procedure body and `For` / `For Each` loop variables (they shadow the attribute; a write through `ThisObject.Warehouse` is still reported), platform properties (`AdditionalProperties`, `RegisterRecords`, `DataExchange`), and writing data of OTHER objects, which is exactly what the `OnWrite` handler is for.

Standard attributes (`Description`, `Code`, `Date`, `Number`) are not seen by the diagnostic yet.

## Examples

Incorrect:

```bsl
Procedure OnWrite(Cancel)

    Warehouse = GetDefaultWarehouse();

EndProcedure
```

Correct:

```bsl
Procedure BeforeWrite(Cancel)

    Warehouse = GetDefaultWarehouse();

EndProcedure
```

## Sources

- [ITS: OnWrite handler (RU)](https://its.1c.ru/db/v8std#content:465)
- [ITS: BeforeWrite handler (RU)](https://its.1c.ru/db/v8std#content:464)
- [v8std: #std465 OnWrite handler](https://v8std.ru/std/465/)
- [v8std: #std464 BeforeWrite handler](https://v8std.ru/std/464/)
