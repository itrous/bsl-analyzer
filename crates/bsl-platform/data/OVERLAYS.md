# Curated platform overlays

`platform_overlays.json` is project-maintained factual correction data. It is
not generated. The analyzer loads it at runtime and applies it to every help
corpus it serves — the built-in interface facts (`platform_facts.json`), an
installed platform, or a corpus package — so a correction reaches whichever
source is configured. `build.rs` does not take part in it: the crate's build
step generates the EDT global catalog only. The base `platform_facts.json`
remains an unchanged extract of the 1C platform help archives.

## Method parameter override schema

<!-- overlay-document -->
```json
{
  "schema_version": 1,
  "method_parameter_overrides": [
    {
      "canonical_type": "EnglishCanonicalType",
      "russian_name": "РусскоеИмяМетода",
      "english_name": "EnglishMethodName",
      "min_version": "8.1",
      "max_version": "8.3.99",
      "parameter_index": 0,
      "replacement_type_list": ["TypeA", "TypeB"],
      "evidence_source": "source identifying the platform contract",
      "rationale": "why the extracted signature needs this narrow correction"
    }
  ]
}
```

`min_version` and `max_version` are optional inclusive bounds for the target
method's documented minimum platform version. Loading rejects malformed
entries, missing or ambiguous RU/EN targets, duplicate parameter overrides,
out-of-range parameter indices, invalid bounds, and duplicate type-list
members. Applying an override changes only the selected parameter type list;
method IDs and method ordering remain those of the extracted data.

## Global function parameter override schema

<!-- overlay-document -->
```json
{
  "schema_version": 1,
  "method_parameter_overrides": [],
  "global_function_parameter_overrides": [
    {
      "russian_name": "РусскоеИмяФункции",
      "english_name": "EnglishFunctionName",
      "min_version": "8.0",
      "parameter_index": 1,
      "replacement_type_list": ["TypeA"],
      "evidence_source": "source identifying the platform contract",
      "rationale": "why the extracted signature needs this narrow correction"
    }
  ]
}
```

An entry of the form `"Семейство: *"` in `replacement_type_list` (in either
section) stands for every extracted type whose name starts with `Семейство: `,
in extract order — `"ОбъектМетаданных: *"` is each `ОбъектМетаданных: <Вид>`.
It is meant for a parameter documented as a whole family: listing some seventy
kinds by hand in each override would repeat them and drift from the extract when
it is regenerated. A wildcard that matches no extracted type is rejected.

The global-function entry is the same as a method parameter override, without `canonical_type`:
global-context functions are extracted apart from type methods and resolve by
their RU/EN names alone. The section is optional; validation and the effect of
applying an entry are those of method parameter overrides.

## Parameter passing in an override

<!-- overlay-document -->
```json
{
  "schema_version": 1,
  "method_parameter_overrides": [],
  "global_function_parameter_overrides": [
    {
      "russian_name": "РусскоеИмяФункции",
      "english_name": "EnglishFunctionName",
      "min_version": "8.0",
      "parameter_index": 0,
      "passing": "by_ref",
      "evidence_source": "the help page says the call writes the result into the argument",
      "rationale": "a rule about the origin of a value must know the call overwrites the variable"
    }
  ]
}
```

`passing` states how the platform passes the parameter, which the help extract does
not record: `"by_ref"` means the call can write a value back into the caller's
variable (`ВвестиСтроку(Текст)` overwrites `Текст`), `"by_value"` that it only reads
the argument. The literals map to `ParamPassing::ByRef` and `ParamPassing::ByVal`, the
same enum the source-level `Знач` of a user method uses. Both `replacement_type_list`
and `passing` are optional, but a record needs at least one of them: an entry that
states neither is refused. A record with `passing` alone leaves the extracted type
list as it is, and its target is still resolved, version-checked and index-checked, so a
misspelt function fails the load. Two records for one parameter are refused as before.

The fact is carried into the loaded corpus as a `passing` key of the parameter and
nowhere else: `platform_facts.json` stays an unchanged extract, and nothing writes the
key back to disk. Records are curated one by one, not harvested from the prose: the
asynchronous twin of a function repeats the sentence of its synchronous namesake and
writes nothing back.

## Type property addition schema

<!-- overlay-document -->
```json
{
  "schema_version": 1,
  "method_parameter_overrides": [],
  "type_property_additions": [
    {
      "canonical_type": "ClientApplicationForm",
      "russian_name": "РусскоеИмяСвойства",
      "english_name": "EnglishPropertyName",
      "property_types": ["ТипЗначения"],
      "is_readonly": false,
      "min_version": "8.2",
      "evidence_source": "source identifying the platform contract",
      "rationale": "why the extracted data omits or misfiles this property"
    }
  ]
}
```

Adds a standard property the help extract omits or files under a misleading name
— for example the managed-form `РежимОткрытияОкна`, whose help page ships under
its enum type `FormWindowOpeningMode`. `canonical_type` must be a known platform
type. Names match case-insensitively. When exactly one property of that type
carries both names, the addition is skipped and the archive record, with its
metadata and documentation, takes precedence: 8.5 archives file
`РежимОткрытияОкна` on the form itself. A property that shares only one of the
names, or several properties that each carry both, are rejected: the shared name
index would hold ambiguous entries. `is_readonly` defaults to `false` and
`min_version` is optional. When the property is added it receives a synthetic id
so documentation lookups do not alias an existing property.

## Type property correction schema

<!-- overlay-document -->
```json
{
  "schema_version": 1,
  "method_parameter_overrides": [],
  "type_property_corrections": [
    {
      "type": "SpreadsheetDocumentRange",
      "name": "Значение",
      "is_readonly": true,
      "property_types": ["Произвольный"],
      "evidence_source": "runtime probe showing the platform refuses the write",
      "rationale": "the help page omits the mark the runtime enforces"
    }
  ]
}
```

`method_parameter_overrides` is a required root key (its entries are the oldest
section and the loader refuses a document without its array), which is why every
example in this document spells it out as empty: a document cut out of this page
must load as it stands, and the tests of the section schemas load exactly these
blocks (`the_documented_examples_load_as_they_stand`).

Corrects a property the extract already carries, but with a wrong fact — the
help page omits the read-only mark, so the extracted record says the property is
writable and the analyzer accepts a write the platform refuses. `type` and
`name` may be spelled in either language: the correction names a record that
exists, and the lookup resolves both aliases of the type and of the property
(`canonical_type` of an addition, in contrast, has to be the English name,
because that addition creates the record). At least one of `is_readonly` and
`property_types` must be present — an entry that changes nothing is rejected, as
are two entries for one property. The record keeps its id, its other fields and
its documentation link: a correction is applied in place.

The mark can be wrong the other way as well: the help marks some writable
properties «Только чтение» (the button shortcut, the colour of the colour
dialog), and `ReadOnlyPropertyAssignment` then reports code the platform runs.
`"is_readonly": false` lifts such a mark; its evidence is a run of the
assignment on the platform, not the help text.

Unlike an addition, a correction never declares a property. The overlay serves
every corpus, and a corpus of another platform release may lack the target
(older than the property, or an external corpus from another vendor's dump); the
correction then does not apply to it and the corpus still loads. A misspelt
target therefore has no runtime symptom, so the curated entries are guarded by
the tests over the built-in facts (`crates/bsl-platform/tests/bundled_facts.rs`),
which fail if a target stops resolving.

## Method return type correction schema

<!-- overlay-document -->
```json
{
  "schema_version": 1,
  "method_parameter_overrides": [],
  "method_return_type_corrections": [
    {
      "type": "XDTOFactory",
      "name": "Создать",
      "return_types": ["ОбъектXDTO", "ЗначениеXDTO"],
      "evidence_source": "runtime probe: ТипЗнч of the result for each call variant",
      "rationale": "the help page documents one return type for every variant"
    }
  ]
}
```

Replaces the return-type list of a method the extract already carries. A help
page that describes several call variants of a method gives one return type for
all of them, and the extract keeps exactly that: `ФабрикаXDTO.Создать` creates
an `ОбъектXDTO` from an object type and a `ЗначениеXDTO` from a value type, but
its record names `ЗначениеXDTO` only. `return_types` is the whole new list, in
the spelling of the extract; it must not be empty or repeat a type. `type` and
`name` may be spelled in either language and resolve exactly like the targets
of a property correction: an entry whose type or method a corpus does not have
does not apply to it, two entries resolving to one method are refused, and the
curated entries are guarded by `crates/bsl-platform/tests/bundled_facts.rs`.

## Type member surface attestation schema

<!-- overlay-document -->
```json
{
  "schema_version": 1,
  "method_parameter_overrides": [],
  "type_member_surface_attestations": [
    {
      "type": "JSONWriter",
      "verified_against_version": "8.3.27",
      "evidence_source": "the help corpus page of the type lists every member it has",
      "rationale": "the surface is complete at that release, so a miss is a defect"
    },
    {
      "type": "SpreadsheetDocumentDrawingCollection",
      "verified_against_version": "8.3.27",
      "verified_surface": ["methods"],
      "evidence_source": "the help corpus page lists every method; a live run finds no `Найти` on the collection and resolves an element by its name",
      "rationale": "the collection addresses its elements by name, so an element name is answered as a property and only the methods are complete"
    }
  ]
}
```

Attests that the extracted member surface of a type — every method and property
by default, or the surfaces `verified_surface` names — is complete for the named
platform release. The absent-member checks (`UnresolvedField`,
`UnresolvedMethodCall`) and the system-enumeration check of `hir-ty` judge a
platform type only where an attestation like this allows it: for the other types the extract is not known to
list every member, and a member missing from it proves nothing about the
platform. `type` may be spelled in either language; `verified_against_version`
is a dotted numeric version of at least three components (`8`, `8.3` and
`+8.3.27` are all rejected: a release line would open the check on every release
of it and the sign is no version spelling), and the check opens only where the
version of the served corpus is that release or newer — an older corpus, or one
whose version the analyzer cannot determine, keeps the receiver open. A homonym
is never attested: its two namesakes share a name but not a surface, so the
lookup cannot tell which one a receiver denotes.

`verified_surface` is optional: a list of `"methods"` and `"fields"` naming what
the attestation covers. Without it both surfaces are attested, so an entry
written before the field existed reads as it always did; `schema_version` stays 1.
An empty list, a value other than those two and a value listed twice are refused
at load time, the error naming the type. Each consumer asks about the surface its
verdict rests on, at its own call site: `UnresolvedMethodCall` asks whether the
methods are complete, `UnresolvedField` whether the properties are, and the check
of a system enumeration whether the values are, which the corpus keeps among the
properties. The field is a statement about the extract, not about the type's
behaviour: a collection that addresses its elements by name (a drawing collection
of a spreadsheet document, the descriptions of standard attributes) answers an
element name as a property, so its property list is not the whole of what
`Collection.<name>` can be, while its method list is. Such a type is attested for
`["methods"]`, and the reason (with the live run that shows it) belongs in the
entry's `rationale` and `evidence_source`.

One type is attested once, and attestations are deduplicated after their type
names are resolved: two entries naming one type through different spellings
(`SpreadsheetDocumentRange` and `ОбластьЯчеекТабличногоДокумента`) are refused
rather than silently ordered. The same holds for corrections — their targets are
resolved to the record first, and two entries that end up on one property are
refused.

The attestation is a statement about the extract, and it has to be earned per
type: the type must be one whose pages yielded members at all (the extract
records 57 types with neither a method nor a property), must not be patched by
any `DOCS_GAP_*` overlay (`crates/bsl-platform/src/db.rs`) and must not share a
name with another type. A type whose member set a configuration extends must not
be attested either: a managed form, for instance, gains properties from its
per-MDO extensions, and those members are not in the platform extract. As with a
correction, an attestation whose type a corpus does not have is dropped and the
corpus still loads.

The rule is per surface. A type a `DOCS_GAP_*` overlay patches is attested for
neither surface. A type a configuration extends (a form, say) and a type that
addresses its elements by name are attested for `["methods"]` only: their
properties are open, their methods are the platform's own.

All of this is checked, not merely written down: `crates/bsl-platform/tests/bundled_facts.rs`
asserts over the curated attestations that each named type exists in the built-in
facts, is not one the docs-gap overlays synthesize members for
(`docs_gap_synthesized_member_types`), and, when its name carries a form/extension
marker or it is on the test's list of types that address their elements by name,
is attested for `["methods"]` only — so attesting such a type for its properties
fails the tests instead of passing review.

## Parameter passing attestation schema

<!-- overlay-document -->
```json
{
  "schema_version": 1,
  "method_parameter_overrides": [],
  "parameter_passing_attestations": [
    {
      "scope": "global_context",
      "verified_against_version": "8.3.27",
      "unverified_functions": ["РусскоеИмяФункции"],
      "evidence_source": "the sweep of every parameter description of the scope in the corpus",
      "rationale": "the scope is judged in full except for the functions the extract documents in part"
    }
  ]
}
```

A `passing` record says one parameter is passed by reference. The attestation is what
lets the absence of a record mean «passed by value» for the rest of the scope: without
it a parameter with no record is simply unknown, and a rule has to treat it so.
`scope` is `global_context` for the global functions (the loader supports only that
scope for now and refuses any other, though the schema is general), otherwise a platform
type by its Russian or English spelling. `verified_against_version` follows the rule of
the member surface attestation: a dotted numeric version of at least three components,
and it gates the claim the same way: it is applied only when the served help is that
release or a newer one, and a help of unknown version gets none.

`unverified_functions` is the honest hole in the claim: the functions of the scope whose
parameters the sweep could not judge, because the extract documents fewer parameters than
it declares (it merges call variants, so a by-reference parameter of an undocumented
variant is invisible to it). A consumer must treat the arguments of those functions as
unknown rather than as passed by value. Each name is resolved against the extracted
global functions by its Russian or English spelling, and a name that matches none is
dropped. A typo there would silently shrink the hole and widen the verified set, which is
the dangerous direction, so the shipped list is held to the corpus by a test instead: it
asserts that the list is exactly the set of global functions that carry call variants. The
list is optional, but not empty when present (omit it when the scope is judged in full)
and repeats no function. One scope is attested once.

## Method-local scope

A fragment of a document — an illustration, not a document — is free to be
incomplete and carries no marker:

```json
{ "replacement_type_list": ["TypeA", "TypeB"] }
```

Overlays are deliberately method-local. For example, a DOM `appendChild` correction
that widens an argument to accept an HTML element applies only to that method's
parameter and does not declare a global subtype relation between HTML and DOM
element types.

## Every example on this page loads

A fenced JSON example introduced by `<!-- overlay-document -->` is a complete
overlay document: a reader copies it out verbatim and loads it, so it carries the
required root key `method_parameter_overrides` and passes the section schema. The
blocks are checked exactly as they stand by `the_documented_examples_load_as_they_stand`
in `crates/bsl-platform/src/overlays_tests.rs`, which reads this page — an example
that stops loading fails the tests. A block without that marker is an illustration
of a shape and is free to be incomplete.

## Evidence requirements

Every entry of every section must include:
- `evidence_source`: A link to official documentation (ITS, syntax assistant), a minimal reproduction script proving the platform behavior, or a specific extracted platform record (for example, a syntax-assistant snippet or a JSON field reference) that demonstrates the contract.
- `rationale`: A clear explanation of why the current extracted data is insufficient and how the entry improves type safety without introducing false positives. For an attestation this includes why the surface of that type is complete.

Entries must be justifiable by verifiable, specific evidence that supports the narrow correction without introducing false positives.
