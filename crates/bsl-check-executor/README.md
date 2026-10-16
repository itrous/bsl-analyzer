# bsl-check-executor

This crate transfers the native Designer backend from commit
`724155b2ec2f4e5b9fe9189de92d05c482eb5bd1` in this repository. The source
files and synthetic XML fixtures are covered by the workspace dual license
(`LICENSE-MIT` and `LICENSE-APACHE`); the fixtures contain no business data.

Transferred implementation: `commands.rs`, `compiler.rs`, `profile.rs`,
`runtime.rs`, `source.rs`, `types.rs`, `tests.rs`, and the XML/Python files in
`fixtures/`. The Python helper remains a local implementation dependency.

Narrow direct dependencies are `check-contract`, `onec-client`,
`bsl-conventions`, `bsl-metadata`, `blake3`, `quick-xml`, `schemars`,
`serde`, `serde_json`, `tokio`, `tokio-util`, `clap`, and platform process/filesystem
support (`libc` on Unix; `win-security-identifier` and `windows-sys` on Windows).
This crate does not depend
on `mcp-server`, IDE crates, or analyzer release targets.

The crate license covers its own sources. Retain the workspace
[licensing statement](../../LICENSING.md) and [NOTICE](../../NOTICE):
`bsl-metadata` provenance remains under review, and the transitive
`bsl-platform` dependency carries 1C data outside the workspace dual license.
See its [data provenance](../bsl-platform/data/PROVENANCE.md).
