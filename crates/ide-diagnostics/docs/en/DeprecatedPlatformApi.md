# Deprecated platform API (DeprecatedPlatformApi)

## Description

Reports calls and references to 1C:Enterprise platform APIs that are marked as deprecated by the platform documentation.

The diagnostic covers deprecated global functions, global methods, platform types, and platform attributes that have a known replacement. Message text names the specific deprecated API and keeps the replacement suggested by the platform fact registry.

`DeprecatedMethodCall` is a separate diagnostic for project source methods deprecated by documentation comments. `GetFormMethod` is also separate and can be reported together with this diagnostic for `GetForm()` / `ПолучитьФорму()`.

## Parameters

- `exclude` (list of strings, empty by default) — names of deprecated features not to report. Write a name as the message quotes the deprecated feature, without the owner type: `Get`, not `HTTPConnection.Get`. Russian and English spellings both work, case does not matter. One spelling silences both: `CurrentDate` also silences `ТекущаяДата()`. A member name silences that member on every owner type. Every other finding stays.

Example: report every deprecated feature except `CurrentDate` and the error-processing methods:

```toml
[diagnostics.parameters.DeprecatedPlatformApi]
exclude = ["CurrentDate", "BriefErrorDescription", "DetailErrorDescription", "ShowErrorInfo"]
```

`DeprecatedPlatformApi = false` switches the check off entirely. `exclude` only narrows a check that is on, and the two cannot be combined in one TOML: the `DeprecatedPlatformApi` key and the `[diagnostics.parameters.DeprecatedPlatformApi]` table are the same key written twice.

An unknown name, a typo included, and a non-string element are reported by the config loader as a warning and skipped; every other name of the list still applies. A value that is not an array (`exclude = "CurrentDate"`) is reported and ignored as a whole.

An enum-name finding is silenced by the enum name before the dot: `ОриентацияМетокДиаграммы` silences `ОриентацияМетокДиаграммы.Вертикальная`. The name quoted by the message silences such a finding only if it is a deprecated feature itself (`Авто`); for an unknown name (`Вертикальная`) the loader warns. These enums have only Russian names in the registry of deprecated features.

The keys `DeprecatedCurrentDate`, `DeprecatedErrorProcessing` and the other codes removed in v0.2.50 are not supported and switch nothing off. When the registry of deprecated features can name the features, the warning about such a key shows a ready `exclude` entry, for example `exclude = ["ТекущаяДата"]` for `DeprecatedCurrentDate`. For the other removed codes the warning only names the `exclude` parameter.

## Examples

Incorrect:

```bsl
OperationDate = CurrentDate();
Position = Find("abcdef", "cd");
Form = GetForm("Form");
Description = DetailErrorDescription(ErrorInfo());
```

Correct:

```bsl
OperationDate = CurrentSessionDate();
Position = StrFind("abcdef", "cd");
OpenForm("Form");
Description = ErrorProcessing.DetailErrorDescription(ErrorInfo());
```

The error-processing manager is reached through the `ErrorProcessing` global
property. `ErrorProcessingManager` is the type of that property and cannot be
written in code.

## Sources

There is no direct 1C standard behind this diagnostic: the rule covers any
deprecated platform API, and the platform documentation is what defines the set.
The numbers below back individual deprecated facilities, not the rule itself.

Related public context:

- [v8std: Work in different time zones](https://v8std.ru/std/643/)
- [v8std: Opening forms](https://v8std.ru/std/404/)
- 1C:Enterprise platform changelogs for deprecated APIs.
