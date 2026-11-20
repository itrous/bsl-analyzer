# Using hardcode ip addresses in code (UsingHardcodeNetworkAddress)

## Description

Hardcoded network addresses should not be stored directly in source code.

This applies to both IPv4 and IPv6 literals. Such values are infrastructure settings and usually need to be changed independently from application code.

An address must be separated from letters, digits, `_`, `-`, and `.`. Names such as `migration-1.0.0.1-applied` and `sha256:1.5:` are not addresses. Brackets and other punctuation remain supported; the diagnostic highlights the address itself, including in strings with doubled quotes.

The existing exclusions for localhost, strings starting with HTTP/HTTPS/FTP URLs, and version contexts configured by the rule remain in effect.

Recommended storage options:

* constants
* information registers
* catalogs, exchange plan nodes, or other metadata objects
* a dedicated module with this rule disabled as a last resort

## Examples

Incorrect:
```bsl
NetworkAddress = "192.168.0.1";
```

Correct:
```bsl
NetworkAddress = MyModuleReUse.ServerNetworkAddress();
```

## Sources

* [v8std: UsingHardcodeNetworkAddress](https://v8std.ru/diagnostics/bslls/UsingHardcodeNetworkAddress/)
