# Designer XML rename contracts

The native XML corpus is reused from `fixtures/extension_common_attributes/base`
and `fixtures/extension_common_attributes/extensions/no-common`. The base export
was produced by platform 8.3.27.1989, compatibility `Version8_3_27`.
`native-common-provenance.json` records its unchanged source hashes.
`native-config-dump-info.xml` restores the version manifest from that same export;
its hash is checked against the provenance alongside the other base XML files.
`config-dump-info.xml` is a separate span/negative-control fixture, not a native export.

The property reader distinguishes scalar/localized nonreference values, format-typed
metadata paths, and unproved properties or populated compound shapes. Unknown fields,
namespaces and unsupported populated collections remain incomplete. Standard attribute
suffixes are not assigned a user declaration UUID. Configuration has a separate raw
container identity: native CFE `Adopted` without an extended-object UUID does not weaken
adoption requirements for user declarations.

ConfigDumpInfo supports the observed hierarchical 2.20 format. Each edited name is
attested against declaration/container UUIDs within the same immutable snapshot root.
External-property IDs can use a declaration UUID or a contained ObjectId from that
same source owner. The suffix is never interpreted or generated. Only name spans
change; opaque `id` and `configVersion` bytes remain unchanged.

GeneratedType names support the observed Catalog, Document and ExchangePlan
categories Object, Ref, Selection, List, Manager; Report categories Object, Manager;
and InformationRegister categories Record, RecordSet, RecordKey, RecordManager,
Selection, List, Manager. Catalog and Document TabularSection declarations own the
TabularSection/TabularSectionRow categories with three-part generated names. Each
owner and member name is attested against the declaration path and UUID. Namespace,
category, type-name prefix, TypeId/ValueId children and duplicate-category checks
remain strict. Only the separately mapped owner/member name spans change; opaque
TypeId and ValueId are preserved rather than recalculated. Unknown categories or
deeper owner paths remain `GeneratedTypeContract`.

`generated-types/provenance.json` binds full sanitized native Document, ExchangePlan,
Report and Catalog/Document table descriptors to their preserved exports. Their
native scalar/reference property contracts let unrelated descriptors remain in a
complete Catalog rename read-set. The fixtures retain all Properties and generated
IDs; they are not reduced to a Name-only object. Source substitutions are recorded
separately and do not claim a new native import qualification.

`common-contracts/provenance.json` binds two full native CommonModule and two
CommonForm descriptors. Their observed boolean, local-string, ReturnValuesReuse
and UsePurposes property shapes are read explicitly; unknown properties and foreign
namespaces still make the result incomplete. Configuration.ChildObjects recognizes
the existing CommonForm class without admitting arbitrary class names.

Native CommandInterface and ExchangePlanContent roots may have exactly one
unqualified `version="2.20"` attribute. Legacy roots without attributes remain
readable. Other versions, qualified version attributes and additional attributes
are refused. `full-corpus/` preserves the prepared 17-file base/CFE source corpus;
its integrated IDE regression covers object/member edits, DataPath/DCS, commands,
exchange content and same-named non-target controls. These composed sources still
require the native baseline and after-import qualification below.

These are source/parser/plan checks. A new native rename/import/export roundtrip,
platform regeneration of version information, ordinary binary form references, and
uncovered DCS expression/output or XML contracts remain required before full metadata
rename acceptance. The tests do not claim that the entire platform XML schema is supported.
