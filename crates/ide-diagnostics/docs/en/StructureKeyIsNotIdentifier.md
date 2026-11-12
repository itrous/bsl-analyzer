# Structure key is not an identifier (StructureKeyIsNotIdentifier)

## Description

A structure key must be an identifier: a letter (Latin or Cyrillic) or `_` first, then letters, digits and `_`; a bare `_` is legal. Any other string (with a dot, a space, a hyphen, a leading digit) is refused by the platform at runtime.

Measured on platform build 8.3.27.2214:

- The `New Structure` constructor takes a COMMA-SEPARATED LIST of keys. Whitespace around each key is trimmed and an empty entry is skipped: `New Structure("a, b")`, `("a,b")`, `(" a , b ")`, `("a,, b")` and `("a,")` are accepted with the keys `a` and `b`. The empty literal `New Structure("")` is legal and yields a structure with no keys; it is never reported.
- `New Structure("a.b")`, `("a b")`, `("a-b")`, `("1a")` and `("a, b c")` are refused with «Ошибка при вызове конструктора (Структура): Недопустимое значение параметра (параметр номер '1')» (“Error calling the constructor (Structure): invalid parameter value (parameter number '1')”).
- The methods `Insert`, `Property` and `Delete` take ONE key, with no splitting on commas and no empty key; whitespace around the key (space, tab, no-break space, line feed) is trimmed: `Insert("a ")` and `Insert(" a")` are accepted with the key `a`, and a key of whitespace only counts as empty. `Insert("a.b")`, `("a b")`, `("a-b")`, `("1a")`, `("a, b")`, `("")` and `("   ")` are refused with «Ошибка при вызове метода контекста (Вставить): Задано неправильное имя атрибута структуры» (“Error calling the context method (Insert): invalid structure attribute name”); `Property` and `Delete` give the same text with their own method name.
- `_a`, `a_1`, `Name1`, a bare `_` and `ЁЙ` are accepted.

A `Map` does not restrict its keys: `New Map` followed by `Map.Insert("a.b", 1)` is legal and very common, so the methods are checked only on a receiver whose type is inferred as `Structure` or `FixedStructure`. A `FixedStructure` has no `Insert` or `Delete`, so only its `Property` is checked. A receiver of unknown type, a `Map` and any other type are not reported.

The diagnostic fires on the string literal that holds the key, once per call: for the constructor, when at least one non-empty entry of the list (after trimming) is not an identifier. A key that comes from a variable or an expression is not statically known and is not reported.

## Examples

Incorrect:

```bsl
Params = New Structure("Item.Code, Quantity");
Params.Insert("Price per unit", 1);
```

Correct:

```bsl
Params = New Structure("ItemCode, Quantity");
Params.Insert("PricePerUnit", 1);
```

## Sources

- Measured on the 1C:Enterprise platform build 8.3.27.2214: the refusal texts, the constructor's key-list parsing and the accepted identifiers are quoted verbatim above.
