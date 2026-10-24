//! Case-insensitive string comparison tuned for BSL's bilingual alphabet.
//!
//! BSL identifiers and keywords are ASCII or Russian Cyrillic, and comparing
//! them via `a.to_lowercase() == b.to_lowercase()` is a hot-path anti-pattern:
//! it allocates two `String`s, folds both sides in full even when the first
//! characters already differ, and every Cyrillic char pays a binary search in
//! the Unicode conversion tables. The helpers here fold per char with an
//! arithmetic fast path for ASCII and the Russian block, allocate nothing, and
//! exit on the first mismatch.
//!
//! Semantics match comparing `chars().flat_map(char::to_lowercase)` streams:
//! context-sensitive mappings of `str::to_lowercase` (Greek final sigma) are
//! not applied, which no BSL keyword or identifier comparison can reach.

/// Folds a char to lowercase when the mapping is trivially single-char:
/// ASCII, the Russian А-Я block, and Ё. Returns `None` for anything that
/// needs the full Unicode tables.
#[inline]
fn fold_char_fast(c: char) -> Option<char> {
    match c {
        'A'..='Z' => Some((c as u8 | 0x20) as char),
        c if c.is_ascii() => Some(c),
        'А'..='Я' => char::from_u32(c as u32 + 0x20),
        'Ё' => Some('ё'),
        'а'..='я' | 'ё' => Some(c),
        _ => None,
    }
}

/// Unicode-aware case-insensitive comparison without allocating.
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    let mut a_chars = a.chars();
    let mut b_chars = b.chars();
    loop {
        match (a_chars.next(), b_chars.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => match (fold_char_fast(x), fold_char_fast(y)) {
                (Some(fx), Some(fy)) => {
                    if fx != fy {
                        return false;
                    }
                }
                // Multi-char lowercase expansions can desynchronise the
                // char-by-char walk, so restart stream comparison from the
                // current pair. Everything before it folded 1:1.
                _ => return eq_ignore_case_slow(x, y, a_chars, b_chars),
            },
            _ => return false,
        }
    }
}

/// Simple Unicode case folding — the equivalence the lexer's `(?i)` patterns
/// use.
///
/// This is a different relation from [`eq_ignore_case`], which folds through
/// `to_lowercase`: `ſ` (U+017F) and `K` (U+212A) lowercase to themselves yet
/// belong to the fold classes of `s` and `k`, and the historic Cyrillic
/// letters U+1C80–U+1C86 belong to the classes of `в`, `д`, `о`, `с`, `т`
/// and `ъ`. Text the lexer has already accepted must be compared with this
/// relation, or the analysis and the tokenizer disagree about what the same
/// word is. `eq_ignore_case` stays the identifier comparison: it is the hot
/// path, and its `to_lowercase` semantics are what persisted keys were built
/// from.
#[inline]
pub fn fold_case(c: char) -> char {
    match c {
        'A'..='Z' => (c as u8 | 0x20) as char,
        c if c.is_ascii() => c,
        'А'..='Я' => char::from_u32(c as u32 + 0x20).unwrap_or(c),
        'Ё' => 'ё',
        'а'..='я' | 'ё' => c,
        // The Turkic dotless i is a fold class of its own — `(?i)i` does not
        // match it — although it uppercases to `I`.
        'ı' => c,
        _ => fold_case_slow(c),
    }
}

/// The fold representative outside the fast alphabet: uppercase, then
/// lowercase. `ſ` reaches `s` only this way. A mapping that expands to
/// several chars has no simple fold, and the char stands alone (`ß`, `İ`).
#[cold]
fn fold_case_slow(c: char) -> char {
    let mut upper = c.to_uppercase();
    let upper = match (upper.next(), upper.next()) {
        (Some(single), None) => single,
        _ => c,
    };
    let mut lower = upper.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => upper,
    }
}

/// Case-insensitive comparison under [`fold_case`]: allocation-free, exiting
/// on the first mismatch. Simple folding is one char to one char, so the two
/// walks never desynchronise.
pub fn eq_case_folded(a: &str, b: &str) -> bool {
    let mut a = a.chars();
    let mut b = b.chars();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if fold_case(x) == fold_case(y) => {}
            _ => return false,
        }
    }
}

/// Drop-in replacement for `str::to_lowercase`.
///
/// Output is byte-identical to `str::to_lowercase` for every input: strings
/// made of ASCII and the Russian block fold arithmetically in one pass, and
/// the first char outside that alphabet falls back to the standard library
/// for the whole string (preserving its context-sensitive mappings). Folded
/// results from either path can therefore be mixed freely, including as
/// persisted map keys.
pub trait CaseExt {
    fn fold_lower(&self) -> String;
}

impl CaseExt for str {
    fn fold_lower(&self) -> String {
        // ASCII and Russian-block chars keep their UTF-8 length when folded,
        // so the fast path never reallocates.
        let mut out = String::with_capacity(self.len());
        for c in self.chars() {
            match fold_char_fast(c) {
                Some(folded) => out.push(folded),
                None => return self.to_lowercase(),
            }
        }
        out
    }
}

/// Folds per char with no contextual mappings, so two strings produce the
/// same key **iff** [`eq_ignore_case`] holds for them. `fold_lower` is not
/// that key: its `str::to_lowercase` fallback applies contextual mappings
/// (Greek final sigma), splitting `eq_ignore_case`-equal strings into
/// different keys. Use this for match buckets, `fold_lower` for display or
/// `to_lowercase`-compatible persisted keys.
pub fn fold_lower_per_char(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match fold_char_fast(c) {
            Some(folded) => out.push(folded),
            None => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// Allocation-free case-insensitive substring search.
///
/// `needle_lower` must already be per-char-lowercase, i.e.
/// `needle_lower == fold_lower_per_char(needle_lower)` — a `debug_assert!`
/// enforces this. The haystack is folded on the fly, one char at a time (same
/// arithmetic fast path as [`fold_lower_per_char`], with the same Unicode
/// fallback for chars outside it), so no `String` is ever allocated for
/// either side. An empty `needle_lower` matches everywhere, like
/// [`str::contains`].
pub fn contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    find_ignore_case(haystack, needle_lower).is_some()
}

/// Case-insensitive substring search reporting the match's byte range **in the haystack**.
///
/// The range is the point: folding a copy and searching there gives an offset into the copy, and
/// `İ`, `K`, `Å` and `ẞ` all change their byte length when folded. Applying such an offset back to
/// the original slices at the wrong place — or inside a char, which panics. Folding one char at a
/// time keeps every offset a real offset into `haystack`.
///
/// `needle_lower` must already be per-char-lowercase, the same contract
/// [`contains_ignore_case`] has. An empty needle matches at 0.
pub fn find_ignore_case(haystack: &str, needle_lower: &str) -> Option<std::ops::Range<usize>> {
    debug_assert_eq!(
        needle_lower,
        fold_lower_per_char(needle_lower),
        "needle_lower must already be per-char-lowercase"
    );

    if needle_lower.is_empty() {
        return Some(0..0);
    }

    let Some(needle_first) = needle_lower.chars().next() else {
        return Some(0..0);
    };

    for (start, candidate) in haystack.char_indices() {
        // First-char filter: most positions fail here, so the full comparison only runs where it
        // might actually match.
        if fold_one(candidate) != needle_first {
            continue;
        }
        if let Some(len) = match_len_ignore_case(&haystack[start..], needle_lower) {
            return Some(start..start + len);
        }
    }
    None
}

/// Whether `fold_lower_per_char(haystack)` starts with `needle_lower`, without folding a copy.
///
/// `needle_lower` must already be per-char-lowercase, the same contract
/// [`contains_ignore_case`] has.
pub fn starts_with_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    debug_assert_eq!(
        needle_lower,
        fold_lower_per_char(needle_lower),
        "needle_lower must already be per-char-lowercase"
    );
    match_len_ignore_case(haystack, needle_lower).is_some()
}

/// The number of haystack bytes matching `needle` from the start, if it matches at all.
///
/// The count is in haystack bytes, never in folded ones, so a char whose lowercase mapping is
/// longer or shorter (`İ`, `K`) still yields a count that lands on a char boundary. A multi-char
/// expansion is consumed the way [`fold_lower_per_char`] would produce it, so this and the search
/// built on it answer the same question the old window comparison did.
fn match_len_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    let mut needle_chars = needle.chars();
    let mut pending: Option<std::char::ToLowercase> = None;
    let mut haystack_chars = haystack.char_indices();
    let mut consumed = 0;

    loop {
        let Some(expected) = needle_chars.next() else {
            return Some(consumed);
        };
        let folded = match pending.as_mut().and_then(Iterator::next) {
            Some(folded) => folded,
            None => {
                pending = None;
                let (index, candidate) = haystack_chars.next()?;
                consumed = index + candidate.len_utf8();
                match fold_char_fast(candidate) {
                    Some(folded) => folded,
                    None => {
                        let mut expansion = candidate.to_lowercase();
                        let first = expansion.next()?;
                        pending = Some(expansion);
                        first
                    }
                }
            }
        };
        if folded != expected {
            return None;
        }
    }
}

/// Folds a single char the same way [`fold_lower_per_char`] would, for the
/// [`contains_ignore_case`] first-char filter (which only ever needs one
/// output char, never a multi-char expansion).
#[inline]
fn fold_one(c: char) -> char {
    fold_char_fast(c).unwrap_or_else(|| c.to_lowercase().next().unwrap_or(c))
}

#[cold]
fn eq_ignore_case_slow(
    x: char,
    y: char,
    a_rest: std::str::Chars<'_>,
    b_rest: std::str::Chars<'_>,
) -> bool {
    let a_folded = x.to_lowercase().chain(a_rest.flat_map(char::to_lowercase));
    let b_folded = y.to_lowercase().chain(b_rest.flat_map(char::to_lowercase));
    a_folded.eq(b_folded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii() {
        assert!(eq_ignore_case("Procedure", "PROCEDURE"));
        assert!(eq_ignore_case("Procedure", "procedure"));
        assert!(!eq_ignore_case("Procedure", "Procedures"));
        assert!(!eq_ignore_case("Procedures", "Procedure"));
        assert!(!eq_ignore_case("Procedure", "Procedurf"));
    }

    #[test]
    fn cyrillic() {
        assert!(eq_ignore_case("Процедура", "ПРОЦЕДУРА"));
        assert!(eq_ignore_case("Процедура", "процедура"));
        assert!(eq_ignore_case("ПроЦеДурА", "процедура"));
        assert!(!eq_ignore_case("Процедура", "Процедуры"));
    }

    #[test]
    fn yo_is_not_ascii_foldable() {
        assert!(eq_ignore_case("Ёлка", "ёлка"));
        assert!(eq_ignore_case("ёЖ", "Ёж"));
        assert!(!eq_ignore_case("Ёлка", "Елка"));
    }

    #[test]
    fn mixed_scripts_and_empty() {
        assert!(eq_ignore_case("Таблица1Row", "таблица1row"));
        assert!(eq_ignore_case("", ""));
        assert!(!eq_ignore_case("", "a"));
        assert!(!eq_ignore_case("а", ""));
    }

    #[test]
    fn fold_case_puts_the_lexers_extra_letters_in_the_right_class() {
        // Пары, которые лексер принимает по `(?i)`, а `eq_ignore_case`
        // отвергает: ради них эта связь и заведена. Без них проверка зелена
        // и у реализации, которая ничего, кроме `to_lowercase`, не делает.
        for (a, b) in [
            ("\u{17F}erver", "Server"),
            ("El\u{17F}if", "ElsIf"),
            ("\u{1C83}ервер", "Сервер"),
            ("\u{1C80}еб", "Веб"),
        ] {
            assert!(eq_case_folded(a, b), "{a:?} и {b:?} обязаны сложиться в один класс");
            assert!(
                !eq_ignore_case(a, b),
                "{a:?} и {b:?}: проверка зелена и без сложения регистра"
            );
        }
        // Знак Кельвина `to_lowercase` покрывает сам, поэтому здесь он
        // проверяет только то, что новая связь его не потеряла.
        assert!(eq_case_folded("brea\u{212A}", "Break"));
    }

    #[test]
    fn fold_case_keeps_apart_what_the_lexer_keeps_apart() {
        // The Turkic pair and `ß` have no simple fold onto ASCII; the lexer
        // rejects them, and so must this.
        assert!(!eq_case_folded("\u{131}f", "If"));
        assert!(!eq_case_folded("\u{130}f", "If"));
        assert!(!eq_case_folded("ß", "ss"));
        assert!(!eq_case_folded("Сервер", "Сервера"));
        assert!(!eq_case_folded("Ёлка", "Елка"));
    }

    #[test]
    fn fold_case_agrees_with_eq_ignore_case_on_the_bsl_alphabet() {
        // The registry alphabet must not shift under the new relation: for
        // ASCII and the Russian block the two comparisons are the same one.
        for (a, b) in [
            ("Сервер", "СЕРВЕР"),
            ("ВебКлиент", "вебклиент"),
            ("MobileStandaloneServer", "MOBILESTANDALONESERVER"),
            ("ThinClient", "thinclient"),
            ("Сервер", "Server"),
            ("", ""),
        ] {
            assert_eq!(eq_case_folded(a, b), eq_ignore_case(a, b), "разошлись на {a:?}/{b:?}");
        }
    }

    #[test]
    fn fast_fold_agrees_with_unicode_tables() {
        for c in ('\0'..='\u{2FF}').chain('\u{400}'..='\u{4FF}') {
            if let Some(folded) = fold_char_fast(c) {
                let expected: Vec<char> = c.to_lowercase().collect();
                assert_eq!(vec![folded], expected, "mismatch for {c:?}");
            }
        }
    }

    #[test]
    fn fold_lower_matches_std_to_lowercase() {
        for s in [
            "Процедура",
            "PROCEDURE",
            "Ёлка123_Test",
            "ОДОΣ",      // Greek triggers the fallback: contextual final sigma
            "ΟΔΟΣ ΟΔΟΣ", // word-final position matters
            "İstanbul",  // multi-char expansion
            "",
            "ß",
        ] {
            assert_eq!(s.fold_lower(), s.to_lowercase(), "mismatch for {s:?}");
        }
    }

    #[test]
    fn per_char_fold_key_agrees_with_eq_ignore_case() {
        // Same key <=> eq_ignore_case, including where contextual
        // `to_lowercase` (and therefore `fold_lower`) disagrees.
        let cases =
            [("Процедура", "ПРОЦЕДУРА"), ("İ", "i\u{307}"), ("ΟΔΟΣ", "οδοσ"), ("ΟΔΟΣ", "οδος")];
        for (a, b) in cases {
            assert_eq!(
                fold_lower_per_char(a) == fold_lower_per_char(b),
                eq_ignore_case(a, b),
                "key/eq mismatch for {a:?} vs {b:?}"
            );
        }
        assert_ne!("ΟΔΟΣ".fold_lower(), fold_lower_per_char("ΟΔΟΣ"), "final sigma is contextual");
    }

    #[test]
    fn slow_path_handles_multichar_expansion() {
        // 'İ' lowercases to two chars; the streams stay aligned.
        assert!(eq_ignore_case("İ", "i\u{307}"));
        assert!(eq_ignore_case("xİ", "xi\u{307}"));
        assert!(!eq_ignore_case("İ", "i"));
        // Final sigma keeps per-char fold semantics (no contextual mapping).
        assert!(!eq_ignore_case("ΟΔΟΣ", "οδος"));
        assert!(eq_ignore_case("ΟΔΟΣ", "οδοσ"));
    }

    #[test]
    fn contains_ascii_mixed_case() {
        assert!(contains_ignore_case("Вызвать СтрШаблон(x)", "стршаблон"));
        assert!(contains_ignore_case("CALL StrTemplate(x)", "strtemplate"));
        assert!(contains_ignore_case("call STRTEMPLATE(x)", "strtemplate"));
        assert!(!contains_ignore_case("Вызвать СтрШаблон(x)", "strtemplate"));
        assert!(!contains_ignore_case("no match here", "strtemplate"));
    }

    #[test]
    fn contains_cyrillic_mixed_case() {
        assert!(contains_ignore_case("Процедура СтрШаблон", "стршаблон"));
        assert!(contains_ignore_case("ПРОЦЕДУРА СТРШАБЛОН", "стршаблон"));
        assert!(!contains_ignore_case("Процедура Иное", "стршаблон"));
    }

    #[test]
    fn contains_yo() {
        assert!(contains_ignore_case("нашёл Ёлку", "ёлку"));
        assert!(contains_ignore_case("нашёл ЁЛКУ", "ёлку"));
        assert!(!contains_ignore_case("нашёл Елку", "ёлку"));
    }

    #[test]
    fn contains_empty_needle_matches_everywhere() {
        // Matches `str::contains("")` semantics.
        assert!(contains_ignore_case("anything", ""));
        assert!(contains_ignore_case("", ""));
    }

    #[test]
    fn contains_needle_longer_than_haystack() {
        assert!(!contains_ignore_case("abc", "abcdef"));
        assert!(!contains_ignore_case("", "a"));
    }

    #[test]
    fn contains_pattern_at_end_of_text() {
        assert!(contains_ignore_case("вызвать СтрШаблон", "стршаблон"));
        assert!(contains_ignore_case("prefix STRTEMPLATE", "strtemplate"));
    }

    #[test]
    fn contains_agrees_with_fold_lower_per_char_contains() {
        let cases: &[(&str, &str)] = &[
            ("Вызвать СтрШаблон(x)", "стршаблон"),
            ("CALL StrTemplate(x)", "strtemplate"),
            ("no match here", "strtemplate"),
            ("нашёл Ёлку", "ёлку"),
            ("нашёл Елку", "ёлку"),
            ("Таблица1Row.Добавить()", "row"),
            ("", "a"),
            ("abc", ""),
        ];
        for (haystack, needle_lower) in cases {
            assert_eq!(
                contains_ignore_case(haystack, needle_lower),
                fold_lower_per_char(haystack).contains(needle_lower),
                "mismatch for haystack {haystack:?}, needle {needle_lower:?}"
            );
        }
    }

    #[test]
    fn contains_handles_multichar_expansion_in_haystack() {
        // 'İ' lowercases to two chars ("i" + combining dot above); the needle
        // must still be found across the expansion boundary.
        assert!(contains_ignore_case("xİy", "i\u{307}"));
        assert_eq!(
            contains_ignore_case("xİy", "i\u{307}"),
            fold_lower_per_char("xİy").contains("i\u{307}")
        );
    }
}

#[cfg(test)]
mod find_ignore_case_tests {
    use super::find_ignore_case;

    #[test]
    fn the_range_is_valid_in_the_haystack_not_in_a_folded_copy() {
        // Every char here changes its byte length when lowercased, so an offset taken from a
        // folded copy would land in the wrong place — inside a char for `İ`.
        for head in ["İ", "K", "Å", "ẞ"] {
            let text = format!("{head} из Строка");
            let found = find_ignore_case(&text, " из ").expect("marker is there");
            assert_eq!(&text[found.clone()], " из ", "range must cut the marker itself");
            assert_eq!(&text[found.end..], "Строка");
        }
    }

    #[test]
    fn a_multi_char_expansion_is_found_whole() {
        // `İ` lowercases to two chars, `i` plus a combining dot. A needle spelling that expansion
        // out must match, and the reported range must cover the whole original char.
        let found = find_ignore_case("xİy", "i\u{307}").expect("the expansion is there");
        assert_eq!(&"xİy"[found.clone()], "İ");
        assert_eq!(found, 1..3);
    }

    #[test]
    fn the_search_answers_exactly_what_contains_answers() {
        // The two share a contract, and a divergence between them is invisible at the call site:
        // the same input must never be present for one and absent for the other.
        for (haystack, needle) in [
            ("xİy", "i\u{307}"),
            ("xİy", "i"),
            ("Массив ИЗ Строка", " из "),
            ("МассивСтрок", " из "),
            ("Å из Строка", " из "),
            ("", " из "),
            ("что угодно", ""),
        ] {
            assert_eq!(
                super::find_ignore_case(haystack, needle).is_some(),
                super::contains_ignore_case(haystack, needle),
                "разошлись на {haystack:?} / {needle:?}"
            );
        }
    }

    #[test]
    fn folds_both_sides_and_reports_absence() {
        // Cyrillic is two bytes per char: the head is 12 bytes, the marker itself 6.
        assert_eq!(find_ignore_case("Массив ИЗ Строка", " из "), Some(12..18));
        assert_eq!(find_ignore_case("МассивСтрок", " из "), None);
        assert_eq!(
            find_ignore_case("хвост в конце из ", " из "),
            Some(("хвост в конце".len())..("хвост в конце из ".len()))
        );
    }
}

#[cfg(test)]
mod starts_with_ignore_case_tests {
    use super::{fold_lower_per_char, starts_with_ignore_case};

    #[test]
    fn answers_what_a_folded_copy_answers() {
        for (haystack, needle) in [
            ("ПодключитьОбработчикОжидания(\"Имя\")", "подключитьобработчикожидания"),
            ("ATTACHIDLEHANDLER", "attachidlehandler"),
            ("Подключить", "подключитьобработчикожидания"),
            ("İx", "i\u{307}x"),
            ("İx", "ix"),
            ("ẞ", "ß"),
            ("", "a"),
            ("что угодно", ""),
        ] {
            assert_eq!(
                starts_with_ignore_case(haystack, needle),
                fold_lower_per_char(haystack).starts_with(needle),
                "разошлись на {haystack:?} / {needle:?}"
            );
        }
    }
}
