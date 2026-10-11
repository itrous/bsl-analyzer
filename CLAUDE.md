# CLAUDE.md

Guidance for Claude Code working in the bsl-analyzer codebase.

## Project

LSP server for BSL (1C:Enterprise), written in Rust.

## Quick reference

```bash
cargo build --release
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings

./scripts/setup-hooks.sh           # install pre-commit hooks (fmt + clippy + tests)
UPDATE_EXPECT=1 cargo test         # accept new snapshot baselines
# Snapshot diagnostic tests use `check_diagnostics_snapshot[_for]` from `crates/ide-diagnostics/src/test_utils.rs`; rebase via `UPDATE_EXPECT=1 cargo test -p ide-diagnostics <name>`.

# Run the LSP server locally
cargo run -p bsl-analyzer --bin bsl-analyzer-app -- lsp
BSL_LOG=debug cargo run -p bsl-analyzer --bin bsl-analyzer-app -- lsp
BSL_PROFILE='*' cargo run -p bsl-analyzer --bin bsl-analyzer-app -- lsp
```

## Architecture

```
bsl-analyzer (LSP server / CLI binary)
  └── ide                     — high-level API (hover, completion, refs, …)
      ├── ide-diagnostics     — diagnostic registry (HIR + AST + dataflow)
      ├── ide-assists         — code actions
      └── ide-db              — Salsa database
          └── hir / hir-def / hir-ty   — semantics (ItemTree, SymbolTree, infer)
              ├── cfg + dataflow       — CFG, reaching defs, liveness
              ├── sdbl-hir             — query language HIR
              └── syntax (Rowan)       — full-fidelity AST/CST
                  └── parser → lexer
```

| Layer    | Crates                                            |
|----------|---------------------------------------------------|
| Analysis | `lexer`, `parser`, `syntax`                       |
| Semantic | `hir-def`, `hir-ty`, `hir`                        |
| IDE      | `ide-db`, `ide-diagnostics`, `ide-assists`, `ide` |
| SDBL     | `sdbl-hir`                                        |
| Dataflow | `cfg`, `dataflow`                                 |
| Metadata | `bsl-metadata`, `bsl-platform`                    |
| Infra    | `base-db`, `vfs`, `project-model`                 |

- **Salsa 0.26.x** drives incremental computation (auto-invalidate on input change, LRU eviction).
- **Rowan** for the syntax tree (immutable, full-fidelity, typed AST wrappers).
- **`bsl-platform`** is a process-wide singleton serving one platform help snapshot, loaded at startup by `platform-help` from the configured source (`[platform_help]`: installed platform HBK read in-process, help package, `auto` (saved snapshot, installed platform, then the built-in facts), `none`). No help corpus is checked in; tests that need it are `corpus_contract` tests run with `--cfg corpus_contract` and `BSL_PLATFORM_HELP_CORPUS` (see `docs/contributing/DEVELOPMENT_RULES.md`).
- **`DiagnosticMetadata`** — compile-time const metadata per diagnostic; never hardcode severity / tags inline, always `ctx.severity(code)` / `ctx.tags(code)`.

Detailed reference: `docs/architecture/ARCHITECTURE.md`, `docs/contributing/DEVELOPMENT_RULES.md`.

## Development rules

0. **Commit format — Conventional Commits**. `feat:` / `fix:` / `chore:` / `test:` / `docs:` / `refactor:`, scope in parens (`feat(ide-diagnostics): …`). Full convention in `CONTRIBUTING.md`.

1. **Library docs first**. Before reaching for an unfamiliar external crate, use Context7 (`resolve-library-id` → `query-docs`). Key crates worth re-checking: `rowan`, `salsa`, `logos`, `lsp-types`.

2. **LSP for navigation**. Hover / goto-definition / find-references / document-symbol / call-hierarchy beat grep when LSP can answer. Use Read for known paths and Grep only for true text search.

3. **Logging via `tracing` only**. `println!`, `eprintln!`, `dbg!` are forbidden in library crates (CLI binaries are the exception). Use spans for hot paths:
   ```rust
   let _span = tracing::info_span!("parse_file", len = input.len()).entered();
   ```

4. **Self-documenting code**. Comments explain WHY, not WHAT. Doc-comments (`///`) for public API. No commented-out code, no obvious-restate-of-code comments — if the code already makes its intent clear, write no comment.
   - **No process references in comments.** Never cite a plan, phase, milestone, task, PR, review, or reviewer in code/test comments (e.g. `§4.E.6e follow-up`, `Phase C`, `PR2:`, `Codex round-1 area 8`, `M4 Task 7`). They rot and mean nothing to a future reader. State the WHY directly; process/history belongs in commit messages and the tracker, not the source.

5. **Tests are mandatory** for new functionality. Use `expect-test` snapshots for parser/AST output. Fixtures live in the repo (`include_str!("fixtures/...")`) — no absolute paths, no per-machine references. **New diagnostic** = handler module + `DiagnosticMetadata` registration + test fixture + `crates/ide-diagnostics/docs/{en,ru}/<Code>.md`; full route in `CONTRIBUTING.md`.

6. **No warnings, no hook bypass**:
   - `cargo clippy --all-targets --all-features -- -D warnings` must pass.
   - `git commit --no-verify` / `git push --no-verify` are forbidden — fix the hook failure.
   - `#[allow(...)]` requires a written rationale right next to it.

## BSL language

- **Bilingual** identifiers and keywords are case-insensitive: `Процедура` ≡ `Procedure`.
- **Preprocessor** directives include localized and English forms, for example `#Если`, `#Область`, `#Вставка`.
- **Annotations** include localized and English forms, for example `&НаКлиенте`, `&НаСервере`, `&До`, `&После`, `&Вместо`.

## General Rules

- Change code only with user consent.
- Ask before installing packages; do not silently look for alternatives.
- **Layered architecture (Martin clean).** Every solution belongs to one layer from the diagram above; do not duplicate logic across layers.
  Layer ownership:
  - syntax -> `lexer` / `parser` / `syntax`;
  - semantics, name resolution, type inference -> `hir-ty` / `hir-def`;
  - diagnostic emission and message formatting -> `ide-diagnostics`;
  - IDE features (hover / completion / refs / actions) -> `ide` / `ide-assists`.
  Before writing code, answer: "Which layer does this solution belong to, and why?" If the answer is "several", that is a design smell: either the logic belongs to one layer, or a helper is needed in a common lower layer.
- **Lowering works without `db`.** `hir-def/body/lower` makes only syntactic decisions. Any decision that requires the receiver type, resolver, or configuration belongs in `hir-ty` (see the cascade gate in `infer.rs::dispatch_bare_ident_field_call` as the model). Adapters (`ide-diagnostics`, `ide-completion`, ...) are thin projections without their own business logic.
- Remove unused code and do not leave stubs.
- Do not use regex for BSL parsing or semantics: use AST / HIR / SDBL APIs. Regex is allowed only for infrastructure utilities such as text search and output formatters.
- Push only to `origin`, not to the `github` mirror.
