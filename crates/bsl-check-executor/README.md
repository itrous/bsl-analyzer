# bsl-check-executor

This crate transfers the native Designer backend from commit
`724155b2ec2f4e5b9fe9189de92d05c482eb5bd1` in this repository. The source
files and synthetic XML fixtures are covered by the workspace MIT license
(`LICENSE-MIT`); the fixtures contain no business data.

Transferred implementation: `commands.rs`, `compiler.rs`, `profile.rs`,
`runtime.rs`, `source.rs`, `types.rs`, `tests.rs`, and the XML/Python files in
`fixtures/`. The Python helper remains a local implementation dependency.

File sources must be offline during snapshot creation, with no concurrent
access, and remain unchanged until the check completes. Platform processes
open a private full database snapshot, not the original. The snapshot contains
business data and is removed by the owned workspace cleanup; the compilation
infobase itself contains only metadata. See the [FILE source prerequisites and
platform-specific isolation limits](../../docs/mcp/NATIVE_CHECK_EXECUTOR.md).

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
