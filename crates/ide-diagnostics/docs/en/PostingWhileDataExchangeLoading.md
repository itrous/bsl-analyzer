# Posting write mode used while data exchange loading is on (PostingWhileDataExchangeLoading)

## Description

Setting the data exchange load flag of an object (`DataExchange.Load = True`) and then writing it with a posting write mode always throws at runtime. In the load mode the platform allows nothing but a plain write; posting is not available at all.

Measured on platform build 8.3.27.2214, on one postable document:

- `Doc.DataExchange.Load = True;` and `Doc.Write(DocumentWriteMode.Posting);` throw «Действие не может выполняться в режиме загрузки данных» («The action cannot be performed in the data loading mode»).
- The same with `DocumentWriteMode.UndoPosting` throws the same text: both posting modes are affected, not only posting.
- The same `Doc.Write(DocumentWriteMode.Posting)` without the load flag succeeds and the document is posted.
- The load flag together with `Doc.Write(DocumentWriteMode.Write)` succeeds: this is the legal, idiomatic combination and it is never reported.

The diagnostic fires on the `Write` call when, in the same procedure, the same local variable received `Variable.DataExchange.Load = True` (the literal `True`) earlier in the statement list that holds the write or in an enclosing one, no `Variable.DataExchange.Load = False` (or any other assignment to the flag or to the variable) came in between, and the argument is the literal `DocumentWriteMode.Posting` or `DocumentWriteMode.UndoPosting`. Anything that cannot be established statically is not reported: the mode taken from a variable, the flag taken from an expression, a receiver that is not a bare variable, a flag set only inside a nested block.

To recalculate the movements of a document, write it with the posting mode in a separate operation, without the load flag.

## Examples

Incorrect:

```bsl
Doc = Ref.GetObject();
Doc.DataExchange.Load = True;
Doc.Write(DocumentWriteMode.Posting);
```

Correct:

```bsl
Doc = Ref.GetObject();
Doc.DataExchange.Load = True;
Doc.Write(DocumentWriteMode.Write);
```

## Sources

- Measured on the 1C:Enterprise platform build 8.3.27.2214: the refusal text and the behaviour of both posting modes are quoted verbatim above.
