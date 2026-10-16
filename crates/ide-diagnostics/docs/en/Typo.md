# Typo (Typo)

## Description
This diagnostic is not a full-featured spell checker.

The current implementation is only a rough Hunspell-based heuristic:

- it checks selected identifiers and string literals against embedded Russian and English dictionaries;
- camelCase identifiers are split into separate word chunks;
- identifier checks cover procedure and function names, parameters, variable declarations, and assignment targets; other identifier uses are not checked;
- each finding points to the word in the original source, including quoted text and every part of a multiline literal;
- a large built-in exception list is used to suppress common false positives;
- users can add extra ignored words through configuration.

This approach is useful only for catching some obvious typos. It does **not** provide reliable spelling or grammar validation for real-world 1C code and domain vocabulary.

Because of the large number of false positives and false negatives, the diagnostic is disabled by default.

### Parameters

- `minWordLength`: minimum number of Unicode scalar values in a word, default `3`
- `userWordsToIgnore`: comma-separated custom ignore list
- `caseInsensitive`: case-insensitive matching for `userWordsToIgnore`

## Examples

### Possible hit

```bsl
Function ВаринатыОплаты()
    Message("Атмена");
EndFunction
```

### Important limitation

Domain-specific terms, abbreviations, mixed-language identifiers, and many valid 1C-specific words may still be reported incorrectly or missed entirely.

## Sources

- Hunspell dictionaries embedded in this repository.
- [Hunspell / spellbook-based dictionary checking](https://github.com/helix-editor/spellbook)
