# Internal synthetic EPF carrier

`admit(&CheckRequest)` accepts a bounded, explicit `module` request with
`context.kind=synthetic` and `context.module_type=object`. Its result can package
the module into the embedded external data processor. Admission and packaging
are pure Rust operations: they do not open files, create 1C objects, or invoke BSL.
The caller must still perform native compilation in safe mode.

The admitted subset contains plain procedure/function and variable declarations,
with optional region markers. Executable module-level statements are refused in
every parsed branch. Parse errors, recovered trees, conditional compilation,
extension directives, annotations, asynchronous/client contexts and unsupported
declarations are refused before packaging. Parameters, explicit and implicit
locals, module variables, own methods and known server platform globals provide
the name boundary. Unknown free names, unknown constructors, configuration
callables and `ThisObject`/`ЭтотОбъект` require context the carrier does not own.
Properties of an admitted local or parameter retain their normal dynamic meaning.

Admission refuses with existing `context_required` or `unsupported` status and
never emits native `valid`/`invalid`. A syntactically complete semantic error,
such as returning a value from a procedure, remains admissible so that the
native compiler can assess it. The input ceiling is 1 MiB of UTF-8, additionally
bounded by the caller's unchanged `max_code_bytes`; the carrier ceiling is 2 MiB.

`AdmittedModule::pack()` accepts no EPF path, external template or file bytes.
It uses only `fixtures/reference.epf`, an own synthetic artifact built by ibcmd
8.3.27.2130. The exact source/reference and prior native receipt hashes are in
`fixtures/reference.json`. The six preceding native feasibility controls are
reused; they do not certify the new Rust-generated file or a future HTTP route.
Other platform versions have not been certified for this carrier.

The private format code handles this one reference: the little-endian V8
container header and directory triples, bounded ASCII page headers, raw DEFLATE
outer payload, and the nested object module `info`/`text` entries. It copies all
metadata and module-info bytes and replaces only the text payload. File layout
is reconstructed deterministically. This is a private carrier implementation;
there is no public arbitrary-container decoder or format configuration.

Text uses the native reference's UTF-8 signature. An existing leading BOM is
preserved as that signature; otherwise the signature is added. Source content
and line terminators are retained, with no wrapper lines. `source()` and
`position()` refer to the original input and one-based UTF-16 positions,
including an original BOM. Unit checks cover these byte/coordinate properties;
native compiler-coordinate mapping, including first-line BOM behavior, requires
acceptance of the generated carrier before it can be exposed as a backend.

Integration belongs to the check backend: retain executor priority and
`handles_snippet`, enable the EPF route explicitly, create only an own temporary
file in safe mode, and report native compilation and cleanup independently.
Old profiles and the version 1 check envelope remain unchanged. Native creation,
no-execution sentinels, deadlines/cancellation/cleanup and source-coordinate
acceptance are still required on the generated candidate.
