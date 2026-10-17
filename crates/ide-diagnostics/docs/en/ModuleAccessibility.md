# Access to user code unavailable in the execution environment (ModuleAccessibility)

## Description

User code, like the platform API, is not uniformly available across execution environments. The diagnostic reports two kinds of violations:

- **Calling a common module from an environment it is not compiled for.** A server common module without the "Server call" flag cannot be invoked from client code; a client common module does not exist on the server. A module with the "Server call" flag IS callable from the client — that is the regular remote call and is not reported.
- **Calling a local method across compilation directives.** Code running on the server cannot invoke a method behind `&AtClient` — the symbol does not exist there. The opposite direction (a client form method calling an `&AtServer` method) is the form's regular remote server call and is not reported. In a managed form module a call through `ЭтотОбъект`/`ЭтаФорма` is the same local call and is judged identically to the bare one.

The call site's environment set is computed exactly as for `UnavailableInEnvironment`: module environments ∩ compilation directive ∩ preprocessor `#If` narrowing (including regions around whole methods). The message qualifier lists only the violating environments — mirroring 1C:EDT verdicts "Module 'X' is not accessible at client" and "Procedure 'X' is not defined [Server]".

The callee module's flags come from the configuration visible to the caller: an extension that adopts the module replaces its flags wholesale, so "Server call" enabled in the extension silences the diagnostic for that extension's code.

Limitations (deliberately conservative): extension interceptors (`&Instead`/`&Before`/`&After`, `&ChangeAndValidate`) are not checked; calls through a variable holding a module value (`M = Common.CommonModule("Name"); M.Method()`) are not checked — control flow selects the module dynamically; the mobile client, the external connection, and the legacy thick client (ordinary application) are excluded from the checked environment set by default; the ordinary-application client enters the environments of the calling code only when ordinary application support is enabled (an ordinary form module being the exception), while the callee common module's "Client (ordinary application)" flag always counts: a module flagged with it alone is not accessible in the managed clients or on the server.

## Configuration

The checked environment set is the `checked_environments` list in the `[features]` section of `bsl-analyzer.toml`, shared with `UnavailableInEnvironment`; see that diagnostic's documentation for details and an example.

## Examples

Incorrect:

```bsl
// CommonModule.ServerSide: Server = True, ServerCall = False
&AtClient
Procedure Save()
    ServerSide.Write(); // the module is not accessible at client
EndProcedure
```

```bsl
&AtClient
Procedure Show()
EndProcedure

&AtServer
Procedure Process()
    Show(); // the procedure is not defined on the server
EndProcedure
```

Correct:

```bsl
// CommonModule.ServerSideServerCall: Server = True, ServerCall = True
&AtClient
Procedure Save()
    ServerSideServerCall.Write(); // the regular remote call
EndProcedure
```

## Sources

- [Standard: Rules for creating common modules](https://its.1c.ru/db/v8std#content:469:hdoc)
