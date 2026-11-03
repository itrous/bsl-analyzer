# GetForm method call (GetFormMethod)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

Standard `#std487` (“Minimizing the number of server calls and traffic”),
p. 4.1, requires a form that is opened from code to be opened in a single call
with the global context method `OpenForm()`. Obtaining the form with
`GetForm()` and then opening it by hand costs an extra server call.

The `GetForm()` call itself works and returns a form object; the standard does
not forbid obtaining one, for example to get a navigation link, to print, or to
check that a form compiles. `OpenForm()` is no substitute there: it opens a
window, `GetForm()` does not. The implementation is intentionally conservative:
it reports any direct `GetForm()` / `ПолучитьФорму()` call, including object
method calls, not only the “get form and then call `Open()`” pattern, so where
the form is not meant to be opened the diagnostic can be suppressed.

## Examples
```bsl
Procedure OpenCatalog()
    Form = GetForm("Catalog.Products.FormList");
    Form.Open();
EndProcedure
```

```bsl
Procedure OpenDocument()
    DocumentObject = Documents.SalesInvoice.CreateDocument();
    Form = DocumentObject.GetForm("DocumentForm");
EndProcedure
```

```bsl
Procedure OpenCatalog()
    OpenForm("Catalog.Products.FormList");
EndProcedure
```

## Sources
* Primary source: [ITS / v8std #std487: Minimizing the number of server calls and traffic (RU)](https://its.1c.ru/db/v8std#content:487:hdoc)
* Secondary source: [v8std.ru: #std487](https://v8std.ru/std/487/)
* Secondary source: [v8std.ru: GetFormMethod](https://v8std.ru/diagnostics/bslls/GetFormMethod/)
