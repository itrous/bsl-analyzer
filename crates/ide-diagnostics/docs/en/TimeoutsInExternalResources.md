# Timeouts when working with external resources (TimeoutsInExternalResources)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

When code creates objects for working with external resources, it should explicitly limit the waiting time. Otherwise a network call or remote service operation can hang for too long and make part of the application unavailable.

This diagnostic reports known constructors when no timeout is passed in the constructor call and no subsequent assignment to the `Timeout` / `Таймаут` property is found for the same simple variable.

The current implementation checks these object types:

- `FTPConnection` / `FTPСоединение`
- `HTTPConnection` / `HTTPСоединение`
- `WSDefinitions` / `WSОпределения`
- `WSProxy` / `WSПрокси`
- `InternetMailProfile` / `ИнтернетПочтовыйПрофиль`

For `InternetMailProfile`, the check can be disabled through configuration because the platform already has a default timeout value.

For `WSProxy`, the timeout is the sixth argument. Omitted slots preserve argument positions; an Internet proxy in the fifth slot or a secure connection in the seventh does not supply a timeout. An omitted argument or `Undefined` does not set an explicit limit.

## Examples

Incorrect:

```bsl
Connection = New HTTPConnection("api.example.com", 443);
```

```bsl
Definitions = New WSDefinitions("http://localhost/test.asmx?WSDL");
```

Correct:

```bsl
Connection = New HTTPConnection("api.example.com", 443,,,, 30);
```

```bsl
Connection = New HTTPConnection("api.example.com", 443);
Connection.Timeout = 30;
```

## Configuration

- `analyzeInternetMailProfileZeroTimeout` (Boolean, default: `true`)  
  Enables or disables checks for `InternetMailProfile`.

## Sources

- [#std748: Timeouts when working with external resources (RU)](https://its.1c.ru/db/v8std#content:748:hdoc)
- [InternetMailProfile default timeout (RU)](https://its.1c.ru/db/metod8dev/content/2358/hdoc)
- [v8std.ru: #std748](https://v8std.ru/std/748/)
