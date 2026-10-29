# Licensing

The code of this project is licensed under the MIT license. SPDX: `MIT`.
This covers every crate of the workspace, `ide-diagnostics` and
`bsl-analyzer` included: each manifest takes the license from
`[workspace.package]` in the root `Cargo.toml`.

The shipped LSP server binary (`bsl-analyzer-app`) is distributed under the
MIT license as well. Linking `ide-diagnostics` does not change that: the crate
is MIT like the rest of the code.

The only license text of the project is `LICENSE-MIT`.

Third-party material bundled in the tree is not the project's code and keeps
its own terms; it is listed in the section on external fixtures below.

## Provenance history

Provenance analysis is the history of how the code came to be, not a
statement of the current license; its records are not part of this
repository. Earlier revisions of this
document split the workspace into license tiers and kept `ide-diagnostics` and
the binary on a copyleft license while 17 SDBL diagnostics depended on the SDBL
parser chain. That arrangement has ended: the owner closed the caveat, the
contributors to `ide-diagnostics` confirmed their contributions in
<https://github.com/itrous/bsl-analyzer/issues/314>, and the whole workspace
moved to MIT (<https://github.com/itrous/bsl-analyzer/issues/733>). Where a
provenance record speaks of tiers, it records that history.

In short: the SDBL and BSL grammar layers were originally written with the
upstream `bsl-parser` grammar files open — this is established by this
repository's own history, not inferred, and `NOTICE` keeps that record. The
SDBL layer was then rewritten slice by slice, the BSL layer was re-derived or
attested rule by rule, and the programme that did so is complete. It
recorded:

- `cfg` taxonomy/API slice: the CFG attestation.
- `bsl-metadata` model slice, re-derived from 1C Designer XML artifacts:
  the bsl-metadata attestation. The crate's test fixtures listed below keep
  their own provenance.
- `sdbl-hir`: the comparison with the upstream lowering, the Slice 13
  attestation and the corpus integration record.
- `parser` and `lexer`: the BSL token inventory (B1), the preprocessor
  symbols (B2), the grammar functions against Chapter 4 of the 1C
  Developer's Guide (B3), the origin of the test material (B4), the
  compatibility decision and the SDBL rule names kept by owner decision O1.
  O1 is a review, not a proof of independent authorship, and it does not
  erase the grammar consultation that `NOTICE` records.
- SDBL lexer and parser slices: the 2026-07 provenance audit and the
  rewrite plan.

The limits the programme accepted stay as recorded: the boundary of table 4.5.4
that the tree does not make observable (B3, tracked in
<https://github.com/itrous/bsl-analyzer/issues/51>); the handler-name limit of
B1 and the divergence of `SyntaxKind::is_preprocessor` from the inventory (B1,
tracked as github#47); `crates/parser/tests/fixtures/Module.bsl`, third-party
content listed below; and the historical record of
`crates/parser/tests/fixtures/user_query_with_highlighting_issue.sdbl`, an
artifact deleted from the tree by `f9053fce`, whose origin was never established
and whose absence of replacement the owner accepted (issues 52 and 245).

## Clean-room practice

The programme above is complete; this is how its replacement work was done, and
how any further replacement of material traced to `bsl-parser` should be done:

1. The implementation source of truth is:
   - official 1C documentation (https://its.1c.ru/db/pubqlang,
     https://its.1c.ru/db/v8std);
   - independently authored local specifications;
   - observed local parser behavior only where it is explicitly
     preserved for IDE or recovery reasons.
2. The `bsl-parser` grammar files (`BSLParser.g4`, `BSLLexer.g4`,
   `SDBLParser.g4`, `SDBLLexer.g4`) must not be consulted while
   writing replacement code. Consulting them to audit what was or was
   not derived is a separate activity: it is permitted, it must be
   recorded in the provenance records, and whoever performs it must not also
   write replacement code afterwards without a clean context.
3. Commit messages for replacement work should state the primary
   source used (for example, a specific ITS page).

## External fixtures and third-party content

The project's MIT license does **not** cover the files below. They are
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
| `crates/bsl-platform/data/platform_data.json` (removed by #312; history only), platform help packages | ООО «1С-Софт» | 1C copyright, see `crates/bsl-platform/data/PROVENANCE.md` — not covered by the project's MIT license; packages carry their own `NOTICE.md` |
| `crates/bsl-platform/data/platform_facts.json` (compiled into the binary) | structured interface facts of the 1C:Enterprise platform, derived from the corpus above with every text removed | factual information about the platform's API, treated as not protectable expression — see `PROVENANCE.md`; contains no description, parameter documentation, example or note; a test fails if any such field appears |

The preprocessor symbol list, formerly in
`crates/ide-diagnostics/src/utils/preprocessor_symbols.rs` and now in
`crates/syntax/src/preproc_symbols.rs`, was re-derived from section 4.8.1.2
of the 1C Developer's Guide. Both lines of its provenance — the list and the
test material of its diagnostic — are described in
the B2 record. This is a provenance record, not a
change of licence: the file stays under the licence of its crate.

Test material that came from `bsl-language-server` — the fixture of the
unknown-preprocessor-symbol diagnostic — is **no longer present**. It was
replaced on 2026-08-21 with material derived from section 4.8.1.2 of the 1C
Developer's Guide, so it is not an entry in the table above: the table lists
third-party content that is still bundled.
`crates/retired-material-gate/tests/retired_material.rs` fails if the retired
material reappears verbatim in any Git-tracked file of that crate, whatever
its extension.

### The body of `Module.bsl` was modified here

CC BY 4.0 asks that modifications be indicated. The file's own header records
the adaptation made before we received it. This repository changed two further
lines in `f1fc00ff` (2026-05-13), correcting an event-handler statement to the
syntax section 4.6.11.1 gives. The licence header itself has never been touched:
`crates/parser/tests/parser_suite/fixture_licence.rs` fails if it is removed or altered.

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
and the four failure modes in full — is in the B4 record.

**Replacement of the named class (2026-10-06).** The diagnostic test material
whose introducing commits name an external source for the *test* material —
40 current carriers in `crates/ide-diagnostics/src/handlers/` — has been
replaced by inputs composed from the normative sections of this repository's
own diagnostic documentation and the current rules. Every old scenario of those
carriers is mapped to a new test, the new tests were shown to fail on mutations
of their own inputs (with two recorded exceptions), and
`crates/retired-material-gate/tests/retired_material.rs` fails if a retired literal of
those carriers reappears byte for byte anywhere in the crate — four generic
blocks that also occur in files outside the queue are recorded instead of
being fingerprinted. The paragraphs above remain the historical record of the import and are
not withdrawn by this one.

This replacement is bounded. It covers that queue only. Test material in the
crate whose introducing commit names no source is neither declared external nor
declared cleaned; the gate catches verbatim return, not paraphrase or computed
text; and it is a provenance record, not a change of licence. The later closure
of the SDBL caveat is described under "Provenance history" above.

See `NOTICE` for upstream acknowledgements.
