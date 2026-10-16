//! Fuzzy matching and match-quality scoring for unqualified completion.
//!
//! Mirrors the RDT1C autocomplete model where the dominant ordering key is the
//! *quality* of how the typed text hit the candidate (their `ПремияФильтра`):
//! an exact prefix beats a CamelCase sub-word hit, which beats an interior
//! substring, which beats a scattered subsequence. The actual subsequence
//! matching and intra-tier scoring is delegated to `nucleo-matcher`; the tier is
//! derived from the match indices it reports.

use nucleo_matcher::{Config, Matcher, Utf32Str};
use stdx::case::CaseExt;

/// Quality of a match, dominant completion sort key. Lower is better, so the
/// numeric value can be emitted directly into an LSP `sort_text` (sorted
/// lexicographically ascending by the client).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum MatchTier {
    /// Candidate starts with the typed text (`Спр` → `Справочники`).
    Prefix = 0,
    /// Contiguous match starting at a sub-word boundary — CamelCase hump or the
    /// char after a separator (`ОН` → `ОбщегоНазначения`).
    WordBoundary = 1,
    /// Contiguous match inside a sub-word.
    Substring = 2,
    /// Scattered subsequence.
    Fuzzy = 3,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MatchResult {
    pub tier: MatchTier,
    /// Raw `nucleo` score, higher is a better match; used as an intra-tier
    /// tiebreak.
    pub score: u16,
    /// The hit came only through the wrong-keyboard-layout alternative of the
    /// typed text. The client filters the list again by the typed word, so such
    /// an item needs [`layout_filter_text`] or the client drops it.
    pub layout: bool,
}

/// A matcher bound to a single typed prefix, reused across all candidates of one
/// completion request. An empty prefix matches everything at [`MatchTier::Prefix`].
pub(super) struct PrefixMatcher {
    matcher: Matcher,
    needle_lower: String,
    needle_chars: Vec<char>,
    /// Wrong-keyboard-layout alternative of the needle (ЙЦУКЕН<->QWERTY by
    /// physical key): typing Latin while a Cyrillic word was meant, or vice versa.
    /// `Some` only when transposing actually changes the text. Matches against it
    /// are demoted to [`MatchTier::Fuzzy`] so a correct-layout hit always wins.
    alt: Option<(String, Vec<char>)>,
    empty: bool,
}

impl PrefixMatcher {
    pub(super) fn new(prefix: &str) -> Self {
        let needle_lower = prefix.fold_lower();
        let needle_chars = needle_lower.chars().collect();
        let alt = transpose_layout(&needle_lower).map(|t| {
            let chars = t.chars().collect();
            (t, chars)
        });
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            empty: needle_lower.is_empty(),
            needle_lower,
            needle_chars,
            alt,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.empty
    }

    /// Cheap, allocation-light gate used at the candidate source to bound the set
    /// before precise scoring. A case-insensitive subsequence test: it admits
    /// exactly what [`PrefixMatcher::score`] would later match, so it never drops
    /// a valid candidate.
    pub(super) fn admits(&self, candidate: &str) -> bool {
        if self.empty {
            return true;
        }
        if is_subsequence(&self.needle_chars, candidate) {
            return true;
        }
        matches!(&self.alt, Some((_, chars)) if is_subsequence(chars, candidate))
    }

    /// Stricter gate requiring the typed text as a contiguous substring (still
    /// covers prefix and CamelCase-contiguous hits, drops scattered ones). Used
    /// for the large platform / metadata candidate sets, where scattered fuzzy
    /// matches on a short prefix would flood the list with noise.
    pub(super) fn admits_contiguous(&self, candidate: &str) -> bool {
        if self.empty {
            return true;
        }
        let hay = candidate.fold_lower();
        hay.contains(self.needle_lower.as_str())
            || matches!(&self.alt, Some((alt, _)) if hay.contains(alt.as_str()))
    }

    /// Bilingual variant of [`PrefixMatcher::admits_contiguous`].
    pub(super) fn admits_contiguous_bilingual(&self, ru: &str, en: &str) -> bool {
        self.admits_contiguous(ru) || self.admits_contiguous(en)
    }

    /// Precise score + quality tier for a candidate. `None` when neither the typed
    /// text nor its wrong-layout alternative is a subsequence of the candidate.
    pub(super) fn score(&mut self, candidate: &str) -> Option<MatchResult> {
        if self.empty {
            return Some(MatchResult { tier: MatchTier::Prefix, score: 0, layout: false });
        }

        let hay_lower = candidate.fold_lower();
        if let Some(r) = fuzzy_score(&mut self.matcher, &hay_lower, &self.needle_lower, candidate) {
            return Some(r);
        }
        // Wrong-layout fallback, only when the correct-layout needle did not hit.
        if let Some((alt, _)) = &self.alt {
            if let Some(mut r) = fuzzy_score(&mut self.matcher, &hay_lower, alt, candidate) {
                r.tier = MatchTier::Fuzzy;
                r.layout = true;
                return Some(r);
            }
        }
        None
    }
}

/// Case-insensitive subsequence test of `needle` (already lowercase) against
/// `candidate`. Shared by the primary and wrong-layout `admits` gates.
fn is_subsequence(needle: &[char], candidate: &str) -> bool {
    let mut needle = needle.iter().copied().peekable();
    for hc in candidate.chars().flat_map(|c| c.to_lowercase()) {
        match needle.peek() {
            Some(&nc) if nc == hc => {
                needle.next();
            }
            Some(_) => {}
            None => return true,
        }
    }
    needle.peek().is_none()
}

/// One `nucleo` fuzzy pass of `needle_lower` against `hay_lower`, with the tier
/// derived from the match positions in the case-preserving `candidate`.
fn fuzzy_score(
    matcher: &mut Matcher,
    hay_lower: &str,
    needle_lower: &str,
    candidate: &str,
) -> Option<MatchResult> {
    let mut hay_buf = Vec::new();
    let mut needle_buf = Vec::new();
    let hay = Utf32Str::new(hay_lower, &mut hay_buf);
    let needle = Utf32Str::new(needle_lower, &mut needle_buf);

    let mut indices = Vec::new();
    let score = matcher.fuzzy_indices(hay, needle, &mut indices)?;
    indices.sort_unstable();

    let tier = classify_tier(candidate, &indices);
    Some(MatchResult { tier, score, layout: false })
}

/// Lowercase keys of the QWERTY layout, row by row; the char at the same position
/// in [`LAYOUT_JCUKEN`] sits on the same physical key of the ЙЦУКЕН layout.
const LAYOUT_QWERTY: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,.";
const LAYOUT_JCUKEN: &str = "йцукенгшщзхъфывапролджэячсмитьбю";

/// Map one lowercase char between the QWERTY and ЙЦУКЕН layouts by physical key
/// (bidirectional). `None` for keys that don't carry a letter on either layout.
fn layout_char(c: char) -> Option<char> {
    let pairs = LAYOUT_QWERTY.chars().zip(LAYOUT_JCUKEN.chars());
    for (latin, cyrillic) in pairs {
        if c == latin {
            return Some(cyrillic);
        }
        if c == cyrillic {
            return Some(latin);
        }
    }
    None
}

/// Transpose a lowercase string between layouts. `None` if no char mapped (so the
/// caller skips a pointless second matching pass).
fn transpose_layout(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut changed = false;
    for c in s.chars() {
        match layout_char(c) {
            Some(m) => {
                out.push(m);
                changed = true;
            }
            None => out.push(c),
        }
    }
    changed.then_some(out)
}

/// Longest `filterText` the client matches in full: VS Code's `fuzzyScore` cuts
/// both the word and the candidate to 128 UTF-16 units, so anything past that is
/// invisible to it.
const CLIENT_FILTER_MAX: usize = 128;

/// `filterText` for an item admitted only through the wrong-layout alternative of
/// the typed text: the item's own filter text (or label) followed by its words
/// transposed into the other layout.
///
/// The client (VS Code) re-filters the list by the word at the cursor against
/// `filterText` and requires the first typed char to land on a word start; the
/// typed `pfg` never matches `Запрос`, but it is a prefix of the appended word
/// `pfghjc`, which starts after a space. The original text stays first, so the
/// client's highlight and score of a correct-layout match are unaffected, and the
/// appended words score below a match at position 0.
pub(super) fn layout_filter_text(label: &str, filter_text: Option<&str>) -> String {
    let mut out = filter_text.unwrap_or(label).to_string();
    let words =
        label.split_whitespace().chain(filter_text.into_iter().flat_map(str::split_whitespace));
    let mut seen: Vec<String> = Vec::new();
    for word in words {
        let Some(transposed) = transpose_layout(&word.fold_lower()) else {
            continue;
        };
        if seen.contains(&transposed) {
            continue;
        }
        let len = out.encode_utf16().count() + 1 + transposed.encode_utf16().count();
        if len > CLIENT_FILTER_MAX {
            continue;
        }
        out.push(' ');
        out.push_str(&transposed);
        seen.push(transposed);
    }
    out
}

/// Score a completion candidate by its label and any bilingual aliases carried in
/// `filter_text` (platform/metadata items store `"<ru> <en>"`, possibly multi-word),
/// keeping the best tier. The whole `filter_text` is scored first so a contiguous
/// match that spans a space is never missed; individual tokens are scored too so a
/// single word starting with the input still earns the tighter [`MatchTier::Prefix`].
/// `None` when the typed text matches neither the label nor any alias.
pub(super) fn score_item(
    matcher: &mut PrefixMatcher,
    label: &str,
    filter_text: Option<&str>,
) -> Option<MatchResult> {
    let mut best = matcher.score(label);
    if let Some(filter) = filter_text {
        best = fold_best(matcher, best, filter);
        for alias in filter.split_whitespace() {
            best = fold_best(matcher, best, alias);
        }
    }
    best
}

/// Keep whichever of `best`/`score(text)` is the stronger match: lower tier wins;
/// on an equal tier, the higher raw score wins.
fn fold_best(
    matcher: &mut PrefixMatcher,
    best: Option<MatchResult>,
    text: &str,
) -> Option<MatchResult> {
    let Some(candidate) = matcher.score(text) else {
        return best;
    };
    match best {
        // On an equal tier a correct-layout hit beats a layout-corrected one, so
        // an item that matches directly never gets a transposed filter text.
        Some(current)
            if current.tier < candidate.tier
                || (current.tier == candidate.tier
                    && (!current.layout, current.score)
                        >= (!candidate.layout, candidate.score)) =>
        {
            Some(current)
        }
        _ => Some(candidate),
    }
}

/// Derive the quality tier from the matched character positions in the original
/// (case-preserving) candidate.
fn classify_tier(candidate: &str, indices: &[u32]) -> MatchTier {
    let Some(&first) = indices.first() else {
        return MatchTier::Fuzzy;
    };

    let contiguous = indices.windows(2).all(|w| w[1] == w[0] + 1);
    if !contiguous {
        return MatchTier::Fuzzy;
    }
    if first == 0 {
        return MatchTier::Prefix;
    }

    let chars: Vec<char> = candidate.chars().collect();
    let pos = first as usize;
    if pos < chars.len() && is_subword_start(&chars, pos) {
        MatchTier::WordBoundary
    } else {
        MatchTier::Substring
    }
}

/// A char begins a sub-word if it is an uppercase hump after a non-uppercase
/// char (CamelCase) or follows a non-alphanumeric separator.
fn is_subword_start(chars: &[char], pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    let cur = chars[pos];
    let prev = chars[pos - 1];
    (cur.is_uppercase() && !prev.is_uppercase()) || !prev.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(prefix: &str, candidate: &str) -> Option<MatchTier> {
        PrefixMatcher::new(prefix).score(candidate).map(|r| r.tier)
    }

    #[test]
    fn empty_prefix_matches_everything_as_prefix() {
        assert_eq!(tier("", "Справочники"), Some(MatchTier::Prefix));
    }

    #[test]
    fn exact_prefix_is_prefix_tier_case_insensitive() {
        assert_eq!(tier("спр", "Справочники"), Some(MatchTier::Prefix));
        assert_eq!(tier("СПР", "Справочники"), Some(MatchTier::Prefix));
        assert_eq!(tier("Спр", "Справочники"), Some(MatchTier::Prefix));
    }

    #[test]
    fn admits_contiguous_drops_scattered() {
        let m = PrefixMatcher::new("есл");
        assert!(!m.admits_contiguous("Перечисления"));
        assert!(m.admits_contiguous("ВводЕслиПусто"));
        assert!(m.admits("Перечисления"), "subsequence gate still admits scattered");
    }

    #[test]
    fn camel_case_abbreviation_is_word_boundary() {
        // `ОН` hits the `О`...`Н` humps of `ОбщегоНазначения` only as a
        // subsequence, but the contiguous interior hit `Назначения` start is a
        // boundary.
        assert_eq!(tier("Назн", "ОбщегоНазначения"), Some(MatchTier::WordBoundary));
    }

    #[test]
    fn interior_contiguous_is_substring() {
        assert_eq!(tier("бщег", "ОбщегоНазначения"), Some(MatchTier::Substring));
    }

    #[test]
    fn scattered_is_fuzzy() {
        assert_eq!(tier("обн", "ОбщегоНазначения"), Some(MatchTier::Fuzzy));
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(tier("xyz", "Справочники"), None);
    }

    #[test]
    fn wrong_layout_matches_but_is_demoted() {
        // "pfg" typed on a US layout is "зап" on ЙЦУКЕН -> Запрос.
        let mut m = PrefixMatcher::new("pfg");
        let r = m.score("Запрос").expect("wrong-layout prefix should still match");
        assert_eq!(r.tier, MatchTier::Fuzzy, "layout-corrected hits never outrank real ones");
        assert!(r.layout, "a hit through the transposed needle must be flagged");
        // Reverse direction: Cyrillic keys for a Latin identifier.
        assert!(PrefixMatcher::new("мфд").score("ValueTable").is_some()); // м-ф-д = v-a-l
    }

    #[test]
    fn correct_layout_is_not_demoted() {
        assert_eq!(tier("таб", "ТаблицаЗначений"), Some(MatchTier::Prefix));
        assert!(!PrefixMatcher::new("таб").score("ТаблицаЗначений").unwrap().layout);
    }

    #[test]
    fn word_stops_before_punctuation_keys() {
        // б ю ж э х ъ sit on , . ; ' [ ] of the US layout: in the editor the word
        // at the cursor ends there, so only the letters before it reach the
        // server. `nf` (= "та") still finds the name; `nf,` is never sent.
        let r = PrefixMatcher::new("nf").score("ТаблицаЗначений").expect("letters before , match");
        assert!(r.layout);
    }

    #[test]
    fn layout_filter_text_appends_transposed_words() {
        assert_eq!(layout_filter_text("Запрос", None), "Запрос pfghjc");
        assert_eq!(layout_filter_text("Запрос", Some("Запрос Query")), "Запрос Query pfghjc йгукн");
        // Cyrillic keys for a Latin name: the appended word is Cyrillic.
        assert_eq!(layout_filter_text("ValueTable", None), "ValueTable мфдгуефиду");
    }

    #[test]
    fn layout_filter_text_stays_within_client_limit() {
        // 41 + 1 + 41 chars: the transposed label still fits, the alias no longer.
        let long = "ОченьДлинноеИмяПеременнойКотороеНеВлезает";
        let alias = "VeryLongVariableNameThatDoesNotFitAnyMore";
        let original = format!("{long} {alias}");
        let text = layout_filter_text(long, Some(&original));
        assert!(text.encode_utf16().count() <= CLIENT_FILTER_MAX, "{text}");
        assert_eq!(text, format!("{original} {}", transpose_layout(&long.fold_lower()).unwrap()));
        // Already past the limit: nothing to add that the client would see.
        let huge = long.repeat(4);
        assert_eq!(layout_filter_text(&huge, None), huge);
    }

    #[test]
    fn layout_tables_map_every_key_both_ways() {
        assert_eq!(LAYOUT_QWERTY.chars().count(), LAYOUT_JCUKEN.chars().count());
        for (latin, cyrillic) in LAYOUT_QWERTY.chars().zip(LAYOUT_JCUKEN.chars()) {
            assert_eq!(layout_char(latin), Some(cyrillic));
            assert_eq!(layout_char(cyrillic), Some(latin));
        }
        assert_eq!(layout_char('1'), None);
        assert_eq!(transpose_layout("123"), None);
    }

    #[test]
    fn admits_wrong_layout() {
        assert!(PrefixMatcher::new("pfg").admits("Запрос"));
        assert!(PrefixMatcher::new("pfg").admits_contiguous("Запрос"));
    }

    #[test]
    fn admits_mirrors_score_match() {
        let m = PrefixMatcher::new("обн");
        assert!(m.admits("ОбщегоНазначения"));
        assert!(!m.admits("Справочники"));
    }
}
