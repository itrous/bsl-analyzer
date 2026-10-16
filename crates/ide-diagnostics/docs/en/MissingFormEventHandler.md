# Form handler is missing from the form module (MissingFormEventHandler)

## Description

The managed form description (`Ext/Form.xml`) binds handlers by name:

- form events — `<Events><Event name="OnOpen">ПриОткрытии</Event></Events>` on the form itself;
- item events — the same `<Events>` block on a field, table, button, group and so on;
- a form command's action — `<Command><Action>ВыполнитьКоманду</Action></Command>`.

The diagnostic reports a bound name for which the form module declares no
procedure or function. Names are compared case-insensitively, as the language
does; the compilation directive, `Асинх`, regions and preprocessor instructions
around the method do not matter. The module itself still compiles, so no other
module check sees this error. The designer's configuration check with "Existence
of assigned handlers" (`/CheckConfig -HandlersExistence`) reports it as
«Отсутствует обработчик».

It usually happens when a procedure was deleted or renamed in the module while
the binding stayed in the form, or when a form was copied without its module.

One procedure may be bound to several events; it is then reported once. A
missing procedure has no place in the module, so the finding goes on the
module's first line, or at the start of an empty module.

Not checked:

- ordinary forms: their bindings live in the binary `Ext/Form.bin`;
- forms borrowed by an extension (`Form.xml` has `<BaseForm>`): their bindings
  also name the base form's handlers, which live in the base configuration's
  module;
- forms without a module file: there is nowhere to put the finding;
- bindings to events written with an identifier instead of a name
  (`<Event name="xxxxxxxx-xxxx-…">`): this is how the designer keeps bindings to
  events the form does not have — an object-write event on a form whose main
  attribute is not an object, for example. The configuration check does not
  report them either.

## Examples

`Form.xml`:

```xml
<Events>
    <Event name="OnOpen">ПриОткрытии</Event>
</Events>
<ChildItems>
    <InputField name="Поле" id="1">
        <Events>
            <Event name="OnChange">ПолеПриИзменении</Event>
        </Events>
    </InputField>
</ChildItems>
<Commands>
    <Command name="Выполнить" id="1">
        <Action>ВыполнитьКоманду</Action>
    </Command>
</Commands>
```

Incorrect — the module declares neither `ПриОткрытии` nor `ВыполнитьКоманду`:

```bsl
&НаКлиенте
Процедура ПолеПриИзменении(Элемент)
КонецПроцедуры
```

Correct:

```bsl
&НаКлиенте
Процедура ПриОткрытии(Отказ)
КонецПроцедуры

&НаКлиенте
Процедура ПолеПриИзменении(Элемент)
КонецПроцедуры

&НаКлиенте
Процедура ВыполнитьКоманду(Команда)
КонецПроцедуры
```

Or remove the binding from the form if the handler is no longer needed.

## Sources

- The designer's configuration check, option "Existence of assigned handlers"
  (`/CheckConfig -HandlersExistence`), message «Отсутствует обработчик».
