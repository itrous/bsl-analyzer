# Declared language with an empty translation (MultilingualStringHasEmptyTranslation)

## Description

In `NStr("ru = 'Balance'; uk = ''")` the language `uk` is declared but its translation is empty. The platform accepts such a literal without any error, so only the analyzer catches the defect: in a session running under `uk` the function returns an empty string and the interface silently loses the text.

The rule fires when:

- the language is in the declared list (the `declaredLanguages` setting of `MultilingualStringHasAllDeclaredLanguages`, `ru` only by default);
- the language entry is present in the literal and there is nothing between its quotes;
- a value made of whitespace only counts as empty: a space is not a translation;
- the same literal has an entry with non-empty text in another language: the text exists, but is lost for this language.

Not reported: a literal that is empty in every language (`ru = ''; uk = ''` is a deliberate empty string, nothing is lost), an entry whose closing quote is followed by something other than `;` or the end of the text (`ru='' on '`), non-empty values, empty values of languages that are not declared, and `NStr` calls whose first argument is not a single string literal (a variable, a concatenation) - its text is not known statically. A doubled quote inside a value (`''`) is an escaped quote, not an empty value.

Difference from `MultilingualStringHasAllDeclaredLanguages`: that diagnostic finds a language that is missing from the literal altogether; this one finds a language that is listed but left blank. The two never fire on the same entry. Unlike the first one, an empty translation is also reported inside `StrTemplate`.

## Examples

The configuration supports languages `ru` and `uk`.

Incorrect:

```bsl
Message = NStr("ru = 'Balance'; uk = ''");
Message = NStr("ru = 'Balance'; uk = ' '");
```

Correct:

```bsl
Message = NStr("ru = 'Balance'; uk = 'Remainder'");
```

## Sources

- [Localization requirements - Standard 1C (RU)](https://its.1c.ru/db/v8std/content/763/hdoc)
