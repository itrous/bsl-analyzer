# Platform help corpus — notice

`platform_data.json` in this package is a structured extract of the
1C:Enterprise context-help archives (`shcntx_ru.hbk`, `shlang_ru.hbk`) made for
use with bsl-analyzer. `manifest.json` names the platform version it was taken
from, the extractor, and the SHA-256 of the corpus.

## Ownership

Content in this file originates from 1C products. In particular, the
`description`, `param_descriptions`, and `examples` fields — is the
copyright of **ООО «1С-Софт»** («1C-Soft» LLC). It is reproduced here
under the practical assumption that it is necessary for interoperability
with the 1C:Enterprise platform and that it is not being redistributed
as a standalone documentation product.

Structured interface facts (type and method names, parameter lists,
return types, availability context, minimum platform versions) describe
the platform's application-programming interface and are treated as
factual information about the platform rather than protectable
expression. Descriptive text, example code and parameter descriptions
are clearly 1C's copyrighted expression.

## Licensing

This package is **not** covered by bsl-analyzer's MIT / Apache-2.0 /
LGPL-3.0 licensing. The code that extracts and consumes it is the project's
own work and stays under the licenses declared in the bsl-analyzer
repository. Whoever redistributes this package inherits an obligation to
respect 1C's rights over its content.
