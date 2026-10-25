# Uninitialized Local Variable (UninitializedLocalVariable)

## Description

This diagnostic marks a read of an implicit local variable that at least one possible execution
path reaches without assigning the variable. On that path the variable holds `Undefined`. An
assignment in a loop body does not initialize the variable for a read made earlier on the first
iteration; an assignment in only some branches of a condition does not initialize it for the other
branches.

The message names the path on which no value is assigned: the first iteration of a loop, a loop
that may run zero times, a branch of a condition, or another path.

Incorrect:

```bsl
Процедура Обработать()
    Для Номер = 1 По 2 Цикл
        Копия = ПозднееЗначение;
        ПозднееЗначение = 7;
        Сообщить(Копия);
    КонецЦикла;
КонецПроцедуры
```

Correct:

```bsl
Процедура Обработать()
    ПозднееЗначение = 0;
    Для Номер = 1 По 2 Цикл
        Копия = ПозднееЗначение;
        ПозднееЗначение = 7;
        Сообщить(Копия);
    КонецЦикла;
КонецПроцедуры
```

## Analysis boundaries

The diagnostic checks only implicit local variables — those introduced by assignment — and the
variables of `For` and `For Each` loops. The analysis does not run the code and evaluates no
conditions except the `While True` header and a `For` header with numeric literal bounds. Nothing
is reported:

- for a method parameter;
- for a variable declared with `Var`;
- for a module variable and other names owned by the module, the form or the object;
- for a read after a call that receives the variable as an argument and is not known to declare
  that parameter `Val`: such a call may assign it;
- for a read after an `Execute` statement, which may assign any variable;
- in a method with an incomplete control-flow graph: a parse error, a recovered statement, a `Goto`
  without its label, or an analysis that did not fit its iteration limit.

## Sources

- [Standard: Preliminary initialization of local variables (RU)](https://its.1c.ru/db/v8std#content:494)
- [v8std: #std494 Preliminary initialization of local variables](https://v8std.ru/std/494/)
