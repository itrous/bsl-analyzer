# Ordinary form dialog was not read (OrdinaryFormDialogUnread)

## Description

The dialog of an ordinary (non-managed) form is kept in the binary `Ext/Form.bin`.
The analyzer reads its description from the optional `Ext/Form/Form.json` next to
the module (the sidecar), whose format is described in
`docs/configuration/ordinary-form-sidecar.md`. The diagnostic fires on an ordinary
form module when a sidecar exists but could not be used:

- `Ext/Form.bin` is newer than the sidecar: the dialog changed after it was written;
- the file is unreadable, malformed, or has an unsupported schema version.

In this state the bindings of handlers to the dialog's buttons and fields are
unknown, and any local method of the module may be a handler. The search for
unused local methods (`UnusedLocalMethod`) is therefore switched off for such a
module, and this diagnostic names the reason in parentheses: the remedy depends
on it.

The finding goes on the module's first line, once per module. Managed forms,
ordinary forms whose dialog was read, and ordinary forms without a sidecar are not
affected: the sidecar is optional and its absence is a normal state, not an error.
The search for unused local methods is switched off for a module without a sidecar
as well, but no finding is reported for it.

## Examples

Message:

```text
Диалог обычной формы не прочитан (сайдкар Ext/Form/Form.json старше Ext/Form.bin): привязки обработчиков диалога неизвестны, поэтому поиск неиспользуемых локальных методов в этом модуле отключён. Обновите Ext/Form/Form.json тем инструментом, который его создаёт, - описание файла в docs/configuration/ordinary-form-sidecar.md
```

Fix: regenerate `Ext/Form/Form.json` with the tool that produces it (the analyzer
itself does not read `Form.bin`).

## Sources

An internal analyzer diagnostic; there is no direct 1C standard.
