# Licensing

This workspace is dual-licensed per crate. The default license for new
code is `MIT OR Apache-2.0`; one library crate, `ide-diagnostics`, remains
under `LGPL-3.0-or-later` for the reasons given in the Tier B section.

The shipped LSP server binary (`bsl-analyzer-app`) statically links
both tiers and is therefore distributed under `LGPL-3.0-or-later`,
because it links `ide-diagnostics`.

Provenance analysis lives in `docs/legal/`. Start with
`docs/legal/sdbl-provenance-2026-07-audit.md`: it supersedes the April 2026
estimates on the state of the code and carries the exit criteria for moving a
crate from Tier B to Tier A. For `sdbl-hir`, the current position is
`docs/legal/sdbl-clean-room-slice13.md` together with
`docs/legal/sdbl-corpus-integration.md`; for `parser` and `lexer`, it is
`docs/legal/bsl-clean-room-slices.md` together with
`docs/legal/sdbl-rule-naming-attestation.md`.

In short: the SDBL and BSL grammar layers were originally written with the
upstream `bsl-parser` grammar files open — this is established by this
repository's own history, not inferred, and `NOTICE` keeps that record. The
SDBL layer was then rewritten slice by slice, the BSL layer was re-derived or
attested rule by rule, and the programme that did so is complete; the
limits it accepted are listed below and in the documents it names.

## Tier A — `MIT OR Apache-2.0`

SPDX: `MIT OR Apache-2.0`. Anyone may take the code of any of these
crates — the crate's own sources, in isolation — and redistribute it under
either MIT or Apache-2.0. The permission does not extend to a build of the
crate: what such a build may be redistributed under also depends on the
tiers of the crates it pulls in.

Two caveats apply to the phrase "in isolation", which earlier versions
of this document used without qualification:

- **A crate is only as reusable as its dependencies.** `hir-ty`, `hir`
  and `dataflow` depend on `cfg`; fourteen crates depend on
  `bsl-metadata`; the crates marked in the last column of the table
  below pull in Tier B crates. Taking such a crate means taking its
  dependency tree under whatever those crates are licensed. The
  per-crate SPDX describes that crate's own code, not the terms on which
  the resulting build can be redistributed.
- **The identified `cfg` taxonomy/API slice has been independently
  re-derived and verified.** The bounded derivation and analysis-preservation
  checks are recorded in [the CFG attestation](docs/legal/cfg-clean-room-slice.md).
  The history of its copyleft reference remains in `NOTICE`; closing this
  slice changes neither the per-crate SPDX nor dependency restrictions.
  **The identified `bsl-metadata` model slice has been re-derived** from
  1C Designer XML artifacts and the analyzer's own needs: a recorded
  observation of 97 996 documents, an old-to-derived registry for every
  decision of that slice, and the removals it called for are documented in
  [the bsl-metadata attestation](docs/legal/bsl-metadata-clean-room-slice.md).
  The crate's test fixtures listed below keep their own provenance and
  remain under review; closing this slice changes neither the per-crate
  SPDX nor dependency restrictions.
- **The `sdbl-hir` clean-room replacement is complete.** A line-by-line
  audit of all production lines of the crate, the replacements of its ten
  findings (F01–F10) and an independent test corpus are recorded in
  [the comparison](docs/legal/sdbl-hir-upstream-comparison.md),
  [the Slice 13 attestation](docs/legal/sdbl-clean-room-slice13.md) and
  [the corpus integration record](docs/legal/sdbl-corpus-integration.md).
  The crate moves to Tier A on that basis. Its normal dependencies reach
  `parser` and `lexer` through `base-db` and `syntax`; when it moved, that kept
  it in the last column of the table below. Both have since moved to Tier A, so
  its entry there is now a dash. Its own `parser` dependency is a
  dev-dependency and is not counted.
- **The `parser` and `lexer` clean-room programme is complete.** The BSL
  token inventory was rewritten
  ([B1](docs/legal/bsl-clean-room-slice-b1.md)), the preprocessor symbols
  were re-derived ([B2](docs/legal/bsl-clean-room-slice-b2.md)), every
  grammar function carries a verdict against Chapter 4 of the 1C
  Developer's Guide — 86 functions, 30 re-derived, 56 attested, none open
  ([B3](docs/legal/bsl-clean-room-slice-b3.md)) — and the origin of the test
  material is recorded ([B4](docs/legal/bsl-clean-room-slice-b4.md)). The
  acceptance of parsing compatibility is recorded in
  [the compatibility decision](docs/legal/bsl-compatibility-decision.md).
  The SDBL rule names were reviewed and kept by owner decision O1
  ([the attestation](docs/legal/sdbl-rule-naming-attestation.md), 2026-10-05):
  that is a review, not a proof of independent authorship, and it does not
  erase the grammar consultation that `NOTICE` records. Both crates move to
  Tier A on that basis; neither pulls in a Tier B crate. Limits the
  programme accepted stay as recorded: the boundary of table 4.5.4 that the
  tree does not make observable (B3, tracked in
  <https://github.com/itrous/bsl-analyzer/issues/51>); the handler-name
  limit of B1 and the divergence of `SyntaxKind::is_preprocessor` from the
  inventory (B1, tracked as github#47);
  `crates/parser/tests/fixtures/Module.bsl`, third-party content listed
  below; and the historical record of
  `crates/parser/tests/fixtures/user_query_with_highlighting_issue.sdbl`, an
  artifact deleted from the tree by `f9053fce` before the base of this move,
  whose origin was never established and whose absence of replacement the
  owner accepted (issues 52 and 245).

The last column lists the Tier B crates reachable from a crate through
its normal and build dependencies, followed transitively inside this
workspace; dev-dependencies are not counted. A dash means none is
reachable. How the column is derived, the provenance walk of Tier A
done under #151 and the additions made under #154 are recorded in
`docs/legal/tier-a-provenance-audit.md`.

| Crate | Purpose | Pulls in Tier B |
|---|---|---|
| `syntax` | Rowan-based lossless CST wrapper | — |
| `base-db` | Salsa foundation, VFS integration | — |
| `vfs`, `vfs-notify` | Virtual file system and file watching | — |
| `project-model` | Project configuration loader | — |
| `intern`, `stdx`, `profile`, `line-index`, `paths` | Utility crates | — |
| `cfg`, `cfg-types`, `dataflow` | Control-flow graph and dataflow analysis | — |
| `hir-def`, `hir-ty`, `hir` | High-level IR: ItemTree, SymbolTree, type inference | — |
| `ide-db`, `ide-assists`, `ide` | IDE database and high-level IDE API | `ide-db`, `ide-assists`: —; `ide`: `ide-diagnostics` |
| `lexer` | BSL and SDBL lexers. BSL inventory: [B1](docs/legal/bsl-clean-room-slice-b1.md); SDBL: attestations of Slices 1–5 listed in [the audit](docs/legal/sdbl-provenance-2026-07-audit.md); owner decision O1: [rule-name attestation](docs/legal/sdbl-rule-naming-attestation.md) | — |
| `parser` | BSL and SDBL parsers. BSL grammar attestation: [B3](docs/legal/bsl-clean-room-slice-b3.md); preprocessor symbols: [B2](docs/legal/bsl-clean-room-slice-b2.md); SDBL rule names: owner decision O1, [attestation](docs/legal/sdbl-rule-naming-attestation.md) | — |
| `sdbl-hir` | SDBL HIR: semantic representation of queries | — |
| `bsl-metadata` | Configuration XML/Config parsing | — |
| `bsl-platform` | Platform types and methods catalog | — |
| `bsl-config` | Visible configurations and `ConfigId` | — |
| `bsl-types` | Type kernel | — |
| `bsl-search`, `symbol-info` | Search and symbol indexing | — |
| `code-chunk` | Splitting sources into fragments | — |
| `parser-error` | Parse error types | — |
| `ide-host-core` | Shared analysis host | `ide-diagnostics` |
| `vcs` | Git diff reports for analysis scoping | — |
| `test-fixture`, `test-utils` | Test infrastructure | — |
| `mcp-server` | MCP protocol server | `ide-diagnostics` |
| `bsl-debug` | DAP protocol support | — |
| `bsl-launcher` | Process launcher | — |
| `naparnik` | AI completion integration layer | — |
| `onec-client` | 1C integration client | — |
| `xtask` | Workspace tooling | — |

## Tier B — `LGPL-3.0-or-later`

SPDX: `LGPL-3.0-or-later`. These crates are kept under the upstream
license because they currently contain grammar, token or test material
that traces back to the `bsl-parser` project (LGPL-3.0-or-later).

| Crate | Blocker | Tracking document |
|---|---|---|
| `ide-diagnostics` | 17 diagnostics depend on the SDBL parser chain | `docs/legal/ide-diagnostics-licensing-summary.md` |
| `bsl-analyzer` | Top-level LSP server, statically links `ide-diagnostics` | — |

A Tier B crate moves to Tier A when its clean-room replacement is
complete and the corresponding provenance note is updated in
`docs/legal/`. The concrete checklist is in
`docs/legal/sdbl-provenance-2026-07-audit.md`, section “Exit criteria”.

`parser` and `lexer` left Tier B on 2026-10-06, once the BSL programme (B1–B4)
and the SDBL exit criteria were closed; the record is in
`docs/legal/tier-a-provenance-audit.md`, section 11. Moving them does not
change the licence of the shipped binary while `ide-diagnostics` stays here.

## Clean-room replacement policy

When working on code that is about to move from Tier B to Tier A:

1. The implementation source of truth is:
   - official 1C documentation (https://its.1c.ru/db/pubqlang,
     https://its.1c.ru/db/v8std);
   - independently authored local specifications
     (see `docs/legal/sdbl-select-mini-spec.md`);
   - observed local parser behavior only where it is explicitly
     preserved for IDE or recovery reasons.
2. The `bsl-parser` grammar files (`BSLParser.g4`, `BSLLexer.g4`,
   `SDBLParser.g4`, `SDBLLexer.g4`) must not be consulted while
   writing replacement code. Consulting them to audit what was or was
   not derived is a separate activity: it is permitted, it must be
   recorded in `docs/legal/`, and whoever performs it must not also
   write replacement code afterwards without a clean context.
3. Commit messages for replacement work should state the primary
   source used (for example, a specific ITS page).

The full rewrite plan is in `docs/legal/sdbl-clean-room-slices.md`.

## License files

The repository ships four license texts. Which one applies to a given
file or crate follows the per-crate SPDX identifier in that crate's
`Cargo.toml`.

| File | Applies to |
|---|---|
| `LICENSE-MIT` | Tier A crates, at the recipient's option |
| `LICENSE-APACHE` | Tier A crates, at the recipient's option |
| `LICENSE-LGPL` | Tier B crates and the shipped binary |
| `LICENSE-GPL` | Accompanies `LICENSE-LGPL` as required by the LGPL |

## External fixtures and third-party content

The workspace license does **not** cover the files below. They are
bundled for build reproducibility and interoperability with the
1C:Enterprise platform. Downstream redistribution of the final binary
inherits the obligations of the original sources.

| Path | Source | Status |
|---|---|---|
| `crates/parser/tests/fixtures/Module.bsl` | ООО «1С-Софт» | CC BY 4.0. Licence header preserved in the file and gated by a test; body modified — see below |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Commands/Команда1/Ext/CommandModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Ext/ManagerModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Ext/ObjectModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаВыбора/Ext/Form/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаСписка/Ext/Form/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/СправочникСМенеджером/Ext/ManagerModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/СправочникСМенеджером/Ext/ObjectModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/CommonModules/ГлобальныйСерверныйМодуль/Ext/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/CommonModules/КлиентскийОбщийМодуль/Ext/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаВыбора/Ext/Form/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаСписка/Ext/Form/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Ext/ExternalConnectionModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Ext/ManagedApplicationModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Ext/SessionModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/HTTPServices/HTTPСервис1/Ext/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/InformationRegisters/РегистрСведений1/Ext/ManagerModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/InformationRegisters/РегистрСведений1/Ext/RecordSetModule.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/WebServices/WebСервис1/Ext/Module.bsl` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаВыбора/Ext/Form.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаВыбора.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаСписка/Ext/Form.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаСписка.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаЭлемента/Ext/Form.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаЭлемента.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1/Templates/Макет.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/Справочник1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Catalogs/СправочникСМенеджером.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/CommandGroups/ГруппаКоманд1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/CommonModules/ГлобальныйСерверныйМодуль.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/CommonModules/КлиентскийОбщийМодуль.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/CommonModules/ПервыйОбщийМодуль.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ConfigDumpInfo.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Configuration.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb`; extended afterwards in this repository, see below |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаВыбора/Ext/Form.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаВыбора.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаДокумента/Ext/Form.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаДокумента.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаСписка/Ext/Form.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1/Forms/ФормаСписка.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Documents/Документ1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/EventSubscriptions/ВерсионированиеПриЗаписи.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/EventSubscriptions/ПередЗаписьюДокумента.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/EventSubscriptions/ПередЗаписьюКонстанты.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/EventSubscriptions/ПриЗаписиДокумента.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/EventSubscriptions/ПриЗаписиСправочника.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/EventSubscriptions/ПриУстановкеНовогоКода.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/EventSubscriptions/РегистрацияИзмененийПередУдалением.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ExternalDataSources/ВнешнийИсточникДанных1/Cubes/Куб1/DimensionTables/ТаблицаИзмерения1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ExternalDataSources/ВнешнийИсточникДанных1/Cubes/Куб1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ExternalDataSources/ВнешнийИсточникДанных1/Cubes/Куб3/DimensionTables/ТаблицаИзмерения2.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ExternalDataSources/ВнешнийИсточникДанных1/Cubes/Куб3.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ExternalDataSources/ВнешнийИсточникДанных1/Tables/Таблица1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ExternalDataSources/ВнешнийИсточникДанных1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/HTTPServices/HTTPСервис1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/InformationRegisters/РегистрСведений1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Languages/Русский.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Roles/ПолныеПрава/Ext/Rights.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Roles/ПолныеПрава.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Roles/Роль1/Ext/Rights.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Roles/Роль1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Roles/Роль2/Ext/Rights.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Roles/Роль2.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ScheduledJobs/РегламентноеЗадание1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ScheduledJobs/РегламентноеЗадание2.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ScheduledJobs/РегламентноеЗаданиеНесуществующийМетод.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ScheduledJobs/РегламентноеЗаданиеПредопределенноеНесколькоПараметров.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/ScheduledJobs/РегламентноеЗаданиеПриватныйМетод.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/Subsystems/Подсистема1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/bsl-metadata/fixtures/designer/WebServices/WebСервис1.xml` | bsl-language-server-rust | test fixture, added by `4601ffcb` |
| `crates/ide-diagnostics/test_data/metadata/designer/` (51 files, whole directory) | bsl-language-server (Java), by the message of `8591e553`; content identical to the `4601ffcb` fixtures | removed test fixture, added by `8591e553`, not referenced by any test; present in history only — see below |
| `crates/ide-diagnostics/test_data/set_permissions_for_new_objects/Roles/` (6 XML files) | not named by its commit; content identical to the `designer/Roles/` files above | test fixture, first added by `0629d987`, moved by `4527ea15` — see below |
| `crates/bsl-platform/data/platform_data.json` (removed by #312; history only), platform help packages and the pinned corpus `auto` downloads | ООО «1С-Софт» | 1C copyright, see `crates/bsl-platform/data/PROVENANCE.md` — not covered by MIT / Apache-2.0 / LGPL-3.0; packages carry their own `NOTICE.md` |
| `crates/bsl-platform/data/platform_facts.json` (compiled into the binary) | structured interface facts of the 1C:Enterprise platform, derived from the corpus above with every text removed | factual information about the platform's API, treated as not protectable expression — see `PROVENANCE.md`; contains no description, parameter documentation, example or note; a test fails if any such field appears |

The preprocessor symbol list, formerly in
`crates/ide-diagnostics/src/utils/preprocessor_symbols.rs` and now in
`crates/syntax/src/preproc_symbols.rs`, was re-derived from section 4.8.1.2
of the 1C Developer's Guide. Both lines of its provenance — the list and the
test material of its diagnostic — are described in
`docs/legal/bsl-clean-room-slice-b2.md`. This is a provenance record, not a
change of licence: the file stays under the licence of its crate.

Test material that came from `bsl-language-server` — the fixture of the
unknown-preprocessor-symbol diagnostic — is **no longer present**. It was
replaced on 2026-08-21 with material derived from section 4.8.1.2 of the 1C
Developer's Guide, so it is not an entry in the table above: the table lists
third-party content that is still bundled. See
`docs/legal/bsl-clean-room-slice-b2.md`;
`crates/ide-diagnostics/tests/retired_material.rs` fails if the retired
material reappears verbatim in any Git-tracked file of that crate, whatever
its extension.

### The body of `Module.bsl` was modified here

CC BY 4.0 asks that modifications be indicated. The file's own header records
the adaptation made before we received it. This repository changed two further
lines in `f1fc00ff` (2026-05-13), correcting an event-handler statement to the
syntax section 4.6.11.1 gives. The licence header itself has never been touched:
`crates/parser/tests/fixture_licence.rs` fails if it is removed or altered.

### XML metadata fixtures

The `.bsl` rows above leave the XML beside them unrecorded. This section covers
the 73 non-`.bsl` files under `crates/bsl-metadata/fixtures/designer/` (72 `.xml`
and one `.bin`), checked on 2026-10-03 by the same walk used for the `.bsl` files:
the commit that introduced each file, its content, and the markers `Source:`,
`Ported`, `copied from`, `bsl-language-server`, `bsl-parser`, `1c-syntax`,
`mdclasses` in the files and in the messages of every commit that touched them.

- **51 files** were introduced by `4601ffcb`, whose message says "Comprehensive
  test fixtures copied from bsl-language-server-rust". The commit lists them; the
  list is not a similarity search. Seven of them (`Catalogs/Справочник1.xml`,
  `Catalogs/СправочникСМенеджером.xml`, three `CommonModules/*.xml`,
  `Documents/Документ1.xml`, `InformationRegisters/РегистрСведений1.xml`) were
  moved up one directory by `a3f23d68` as pure renames. Fifty of the 51 are
  byte-identical to the version `4601ffcb` introduced. `Configuration.xml` was
  extended afterwards in this repository (three further commits adding the
  objects that other fixtures need); the file as a whole is listed because its
  base is the imported one, and the extensions are ours.
- **22 files** were introduced by later commits (`1ded061e`, `35e7a03e`,
  `595edb80`, `6194c691`, `799733cf`, `9575b32c`, `18cdcfa8`, `cc23cf16`,
  `d03d8281`, `e949a468`), each as part of a change to this repository's own code.
  Their messages name no external source and no file carries a marker. They are
  **not** listed above and stay under the workspace licence. This is the absence
  of a trace, not proof of independent authorship — the same limit as for the
  `.bsl` files.
- The word `MDClasses` inside the files is the 1C configuration-export XML
  namespace, which every Designer export carries. It is not a reference to the
  `mdclasses` project.

The origin of the 51 files is the channel stated by the commit message,
`bsl-language-server-rust`. Its own sources were not opened for this record, so
what that project took from elsewhere is not established here. The commit
establishes the channel only, not a copyright holder or licence for any file:
none of them carries a licence header of its own, and the terms are those of
whoever authored the material, which this repository's history does not show.
That is the same standing as the `.bsl` rows above.

`crates/ide-diagnostics/test_data/metadata/designer/` held 51 files before its
removal, all introduced by `8591e553` and nothing else — checked against every tracked file in
the directory, not only `.xml`. That commit added 72 files: the 51 XML and 21
`.bsl`, the latter byte-identical to the 21 `.bsl` rows above and deleted by
`07d2b977`. The commit says "Copy test configuration
metadata/designer/ from Java project", so the stated channel here is
`bsl-language-server`, not `bsl-language-server-rust`. Fifty of the 51 are
byte-identical to the `bsl-metadata` files above; the 51st is `Configuration.xml`
in the form `4601ffcb` introduced it, before this repository extended it. The two
copies therefore carry one body of material under two stated channels, and both
are recorded. A directory row was justified here only because the check above
found no file in the directory from another commit. No code or test referenced
this directory before its removal: every `ide-diagnostics` test that reads a designer configuration
reads `crates/bsl-metadata/fixtures/designer`.

`crates/ide-diagnostics/test_data/set_permissions_for_new_objects/Roles/` holds
six XML files byte-identical to `designer/Roles/ПолныеПрава`, `Роль1` and `Роль2`
above. The commits that placed them (`0629d987`, moved by `4527ea15`) name no
source, so the row rests on content identity alone, not on a stated channel.

The `.bsl`, `.xml` and `.bin` files elsewhere under `crates/bsl-metadata/fixtures/`
(`cfe_dependencies/`, `extension_common_module/`, `extension_metadata/`) were not
part of this check. This is a provenance record, not a change of licence.

### Test material embedded in Rust sources

The rows above name files, not directories. An earlier revision of this table
named the seven directories those files sit in, on the reasoning that each
directory held only files from `4601ffcb`. That reasoning was wrong and the check
behind it was narrower than the claim it licensed: it counted only `.bsl` files.
`designer/Catalogs/` alone holds eighteen files — eight BSL and ten XML — and
several of them were introduced by other commits. A directory row would have
taken those out of the workspace licence and attributed them to a project that
never supplied them, which is an error in the direction of someone else's rights.

The other 15 BSL files under `crates/bsl-metadata/fixtures/` — in
`cfe_dependencies/`, `extension_common_module/` and seven `designer/`
subdirectories — came from different commits and are **not** covered by these
rows.

A second body of test material has no path of its own. Diagnostic fixtures that
were once `.bsl` files now live inside Rust test sources, and the Rust code
around them is ours. Listing those Rust files here would say the wrong thing
twice: it would put our own code outside the workspace licence, and it would
still not say which part of the file is not ours.

**What this document states about that material is a class, not a map.** The
commits that introduced those fixtures record, in their own words, that test
files were copied from `bsl-language-server` and from `bsl-language-server-rust`.
`07d2b977` deleted 187 such fixtures when the tests moved inline, and the
material of a large part of them is still present in `crates/ide-diagnostics`.

A per-file map is deliberately not published. Four independent methods of
building one were tried and each failed in its own direction — attributing a
fixture to the commit that merely moved it, crediting shared boilerplate as
surviving material, matching a line that any test could contain, and following a
rename into a sibling fixture. A textual match also cannot see material a test
builds programmatically. A map that looks precise and is repeatedly wrong is a
worse record than a class statement that is right, and the remedy for the
uncertainty is not a better map but replacement of the material, tracked in
`https://github.com/itrous/bsl-analyzer/issues/52`.

The evidence for the class statement — the commit messages, the migration commit,
and the four failure modes in full — is in
`docs/legal/bsl-clean-room-slice-b4.md`.

**Replacement of the named class (2026-10-06).** The diagnostic test material
whose introducing commits name an external source for the *test* material —
40 current carriers in `crates/ide-diagnostics/src/handlers/`, listed with their
introducing commits in `docs/legal/ide-diagnostics-test-material.md` — has been
replaced by inputs composed from the normative sections of this repository's
own diagnostic documentation and the current rules. Every old scenario of those
carriers is mapped to a new test, the new tests were shown to fail on mutations
of their own inputs (the two exceptions are recorded there), and
`crates/ide-diagnostics/tests/retired_material.rs` fails if a retired literal of
those carriers reappears byte for byte anywhere in the crate — four generic
blocks that also occur in files outside the queue are recorded there instead of
being fingerprinted. The paragraphs above remain the historical record of the import and are
not withdrawn by this one.

This replacement is bounded. It covers that queue only. Test material in the
crate whose introducing commit names no source is neither declared external nor
declared cleaned; the gate catches verbatim return, not paraphrase or computed
text; and nothing here changes the tier of any crate or the SDBL caveat recorded
in `docs/legal/ide-diagnostics-licensing-summary.md`.

See `NOTICE` for upstream acknowledgements.
