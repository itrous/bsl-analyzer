# Invalid character (InvalidCharacterInFile)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description

The diagnostic reports non-breaking spaces and characters that only look like the
normal hyphen-minus `-` but have a different Unicode code point, where they keep the
module from compiling or change the meaning of text that is parsed. These characters
typically appear after copying code from office documents, browsers, or rich-text
editors.

A character is reported:

- in code, where it is not a valid token: a wrong dash in an expression produces a
  syntax error;
- as a non-breaking space glued to the preceding identifier, keyword or number
  (`If\u00A0Condition`): nothing ordinary separates them, and the module does not
  compile;
- in a `//` line inside a multiline string literal: that line is text of the literal,
  not a comment;
- in a `Name - Type` line of a `Parameters:` section of a method description, when the
  character breaks the separator: the analyzer no longer sees the parameter type.

A character is not reported:

- in a string literal: it is data, and the character may be exactly what the code
  displays or searches for;
- in an ordinary comment: the platform does not compile it;
- as a non-breaking space separated from the preceding token by an ordinary space,
  following punctuation or an operator, or at the start or the end of a line: the
  module compiles.

Standard #std456 advises against these characters in the module text as a whole; this
diagnostic does not check that advice.

The current implementation detects:

- soft hyphen;
- figure dash;
- en dash;
- em dash;
- horizontal bar;
- Unicode minus sign;
- non-breaking space.

## Sources

* Primary source: [ITS / v8std #std456: Module texts (RU)](https://its.1c.ru/db/v8std#content:456:hdoc)
* Secondary source: [v8std.ru: #std456](https://v8std.ru/std/456/)
