# Access to API unavailable in the execution environment (UnavailableInEnvironment)

## Description

The 1C:Enterprise platform API is not uniformly available across execution environments: some types, methods, properties, and global functions exist only on the server, others only in specific client kinds (the platform syntax helper documents this under "Availability"). This diagnostic reports access to a platform member from code that runs in an environment where the member does not exist: for example, calling server-side API from a method behind the `&AtClient` directive, or constructing `New TextReader` in code compiled for the web client.

The call site's environment set is the intersection of the module's environments (module kind, common-module flags: Server, ClientManagedApplication, ServerCall, ExternalConnection) and the method's compilation directive (`&AtClient`, `&AtServer`, `&AtClientAtServer`, …). The message qualifier lists only the environments where the member is missing — mirroring 1C:EDT verdicts like "… is not defined [Web client]".

Global manager collections of configuration objects (`Catalogs`, `Documents`, `Enums`, `InformationRegisters`, …) are checked as well: they are server-side surface — the thin, web, and mobile clients have no such global properties, and a module accessing `Enums.<Name>.<Value>` does not compile there. The diagnostic is reported once, on the collection root name, regardless of the chain length. The client-safe replacement is `PredefinedValue("Enum.<Name>.<Value>")`.

Preprocessor conditions are understood: inside `#If … #EndIf` only the intersection of the method's environments with the environments the branch compiles for is checked (`#If Not WebClient Then` removes the web client; the `#Else` branch receives the complement). Environments for which the condition stays undecidable (e.g. it involves an OS name and is not resolved by И/ИЛИ absorption) are skipped throughout the branch chain. This also covers preprocessor regions around whole methods: a method declared inside `#If Not WebClient Then` is not checked against the web client.

An ordinary form module runs only in a thick client: the ordinary application's, and the managed application's only when the main configuration enables `UseOrdinaryFormInManagedApplication`. With the property off, such a module is not checked against the managed thick client; without a main configuration (a project of external data processors alone) both thick clients count.

Object, manager, record set and value manager modules run on the server and in the external connection, and — when the main configuration's `DefaultRunMode` is `ManagedApplication` or `Auto` — also in the managed thick client. That client is judged as in the client-server variant: there a common module flagged only "Server" is not visible (one with "Server call" or "Client (managed application)" is) and an `&AtServer` method of the module is not compiled. In the file variant the platform compiles server code in the client process and sees such modules, so a finding there names code that will not compile against a client-server infobase. In an `OrdinaryApplication` configuration and without a main configuration the managed thick client is not added: the platform also compiles these modules there, but such a configuration is normally not run in that client.

Current limitations (deliberately conservative):

- extension modules (interceptors `&Instead`/`&Before`/`&After`, `&ChangeAndValidate`) are not checked;
- the mobile client, the external connection, and the legacy thick client (ordinary application) are excluded from the checked environment set by default (`checked_environments`);
- when a platform type name is ambiguous (e.g. `ЭлементыФормы` names both the managed-form items collection and the legacy form controls), availability of its members is not judged.

## Configuration

The checked environment set comes from `bsl-analyzer.toml`, named like preprocessor symbols (case-insensitive, Russian and English names are equivalent). A configuration that never runs in the web client drops it from the list:

```toml
[features]
checked_environments = ["ThinClient", "ThickClientManagedApplication", "Server"]
```

The default set is the thin client, the web client, the managed thick client, and the server. Listing `MobileClient` or `ThickClientOrdinaryApplication` also opts those environments into the execution model (the mobile client joins the client environments; the ordinary application enables ordinary-app support). A list of only unrecognized names keeps the default set (with a logged warning); an explicit empty list `[]` turns both checks off. The setting is shared by `UnavailableInEnvironment` and `ModuleAccessibility`.

## Examples

Incorrect:

```bsl
&AtClient
Procedure ReadFile()
    // TextReader is unavailable in the web client
    Reader = New TextReader;
    Line = Reader.ReadLine();
EndProcedure
```

Correct:

```bsl
&AtServer
Procedure ReadFileAtServer()
    Reader = New TextReader;
    Line = Reader.ReadLine();
EndProcedure
```

or with an explicit preprocessor guard (the diagnostic understands such conditions and stays silent):

```bsl
&AtClient
Procedure ReadFile()
    #If Not WebClient Then
    Reader = New TextReader;
    Line = Reader.ReadLine();
    #EndIf
EndProcedure
```

## Sources

- [1C:Enterprise syntax helper, the "Availability" section of each method/property](https://its.1c.ru/db/v8std)
