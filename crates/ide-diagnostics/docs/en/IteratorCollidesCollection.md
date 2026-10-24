# Iterator collides with collection (IteratorCollidesCollection)

## Description

In `For Each <Iterator> In <Collection> Do` the loop variable and the iterated collection are written with the same name. It is one variable: after the first iteration it receives an element, and the value that was the collection is lost. This differs from `UseLessForEach`: the iterator may be used in the loop body and the collection is still overwritten.

The diagnostic is reported on the loop variable when the loop source is an unqualified identifier equal to the iterator, ignoring case; parentheses around the identifier (`(Collection)`) do not change the source. Module-level variables are not excluded: overwriting a module-level collection variable is also a defect.

Not treated as a collision:

- a qualified source (`Object.Collection`): a property name of another object is not the same variable;
- a call, an index access or any other expression as the source;
- a copy of the collection in another variable (`B = A`, then `For Each A In B`): these are different variables.

## Examples

### Incorrect

```bsl
Collection = New Array;
Collection.Add(1);
For Each Collection In Collection Do
    Message(Collection);
EndDo;
```

### Correct

```bsl
Collection = New Array;
Collection.Add(1);
For Each Item In Collection Do
    Message(Item);
EndDo;
```

## Sources

- [itrous/bsl-analyzer#583](https://github.com/itrous/bsl-analyzer/issues/583)
- [1C-Company/v8-code-style#1240](https://github.com/1C-Company/v8-code-style/issues/1240)
