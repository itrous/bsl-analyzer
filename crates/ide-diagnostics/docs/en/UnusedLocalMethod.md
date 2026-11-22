# Unused local method (UnusedLocalMethod)

## Description

Modules should not contain local procedures and functions that are never called.

Unused methods make the code harder to navigate and often indicate unfinished refactoring or dead logic. The diagnostic skips several categories that are expected to be called indirectly, such as exported methods, extension methods, platform handlers, and attachable methods with configured prefixes.

A recognized notification registration counts as a use of a local handler when its receiver resolves to the current object. A registration on another or unknown receiver alone does not establish a use of a same-named local method.

In form modules the diagnostic also treats a method as used when another identifier-shaped string literal in the same module names it. Dynamic handler binding — `УстановитьДействие`, a command created in code with `Действие = "Name"`, or a helper module fed a parameter structure — leaves the handler's name as such a literal. The flip side: string *data* that happens to coincide with a method name also exempts that method, so a genuinely dead form-module method whose name appears in an unrelated string will not be reported.

## Sources

* [Standard: Module texts (RU)](https://its.1c.ru/db/v8std#content:456:hdoc)
* [v8std: UnusedLocalMethod](https://v8std.ru/diagnostics/bslls/UnusedLocalMethod/)
