# Implicit Database Read Inside a Loop (ImplicitDatabaseReadInCycle)

## Description

This diagnostic marks static candidates for database reads inside a loop body: accessing an
attribute through a reference, obtaining a reference's presentation with `String()`, and searching
through a manager's `FindBy*` method. Fetch the required values with one query before the loop.

### Reference attribute access

Accessing an object's attribute through a reference loads the object. Select the required
attributes in a query before the loop.

Incorrect:

```bsl
Request = New Query;
Request.Text = "SELECT T.Ref AS Item FROM Catalog.Items AS T";
Selection = Request.Execute().Select();

While Selection.Next() Do
    Description = Selection.Item.Description;
    ItemCode = Selection.Item.Code;
EndDo;
```

Correct:

```bsl
Request = New Query;
Request.Text = "SELECT T.Description AS Description, T.Code AS ItemCode FROM Catalog.Items AS T";
Selection = Request.Execute().Select();

While Selection.Next() Do
    Description = Selection.Description;
    ItemCode = Selection.ItemCode;
EndDo;
```

### Reference presentation with `String()`

`String(Ref)` obtains the reference's presentation. If it is needed in the loop, select it in a query
before the loop with `PRESENTATION`.

Incorrect:

```bsl
For Each Ref In Refs() Do
    View = String(Ref);
EndDo;
```

Correct:

```bsl
Request = New Query;
Request.Text = "SELECT PRESENTATION(T.Ref) AS ItemPresentation FROM Catalog.Items AS T";
Selection = Request.Execute().Select();

While Selection.Next() Do
    View = Selection.ItemPresentation;
EndDo;
```

### Manager search

The manager methods `FindByCode`, `FindByDescription`, `FindByNumber`, and `FindByAttribute` search
the database. If many values need to be processed, find them with one query before the loop.

Incorrect:

```bsl
For Each Code In Codes Do
    Item = Catalogs.Items.FindByCode(Code);
    ProcessItem(Item);
EndDo;
```

Correct:

```bsl
Request = New Query;
Request.Text =
    "SELECT T.Ref AS Item " +
    "FROM Catalog.Items AS T " +
    "WHERE T.Code IN (&Codes)";
Request.SetParameter("Codes", Codes);
Selection = Request.Execute().Select();

While Selection.Next() Do
    ProcessItem(Selection.Item);
EndDo;
```

## Analysis boundaries

The diagnostic analyzes only expressions in the lexical body of a loop in the current method. A
recursive or cyclic call, as well as a call whose body is inaccessible, is not by itself evidence of a
database read. The called method's body is not expanded at the call site; it is analyzed separately,
according to its own loops.

The requirements depend on the candidate family:

- For attribute access through a reference, the expression's base must have a known object-reference
  type. `Unknown`, `Any`, numbers, and strings do not qualify.
- For `String(Ref)`, the argument must have a known object-reference type and the call must resolve to
  the built-in platform function. `Unknown`, `Any`, numbers, and strings, as well as a same-name
  user-defined function, do not qualify.
- For a manager search, the receiver must have type `ObjectManager` and the call must bind to a
  platform method. The search result need not have a reference type; a same-name user-defined method
  does not qualify.

This is a static candidate: the diagnostic does not claim that the platform executes exactly one database query on every iteration; caching and actual query plans require measurement.

## Sources

- [Standard: Reading individual object attributes from the database (RU)](https://its.1c.ru/db/v8std#content:496)
- [v8std: #std496 Reading individual object attributes from the database](https://v8std.ru/std/496/)
- [Standard: Repeated execution of similar queries (RU)](https://its.1c.ru/db/v8std#content:436)
- [v8std: #std436 Repeated execution of similar queries](https://v8std.ru/std/436/)
