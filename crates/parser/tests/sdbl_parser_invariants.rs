//! Properties the SDBL parser must hold for *any* input, checked by
//! generating inputs rather than by naming them.
//!
//! The package loop and its recovery grew by accretion, and hand-written
//! cases only cover the edges someone thought of. These properties are the
//! ones the recovery work is for: nothing leaves the parser unaccounted for,
//! nothing is invented that the input does not contain, every complaint
//! points somewhere real, and no input makes the parser stop responding.
//!
//! Provenance: `docs/legal/sdbl-clean-room-slice12.md`.

use parser::parse_sdbl;
use syntax::SyntaxKind;

mod common;

use common::{generate, Rng, FRAGMENTS};

/// Every property, checked on one input. Returns the first breach found.
fn breach(input: &str) -> Option<String> {
    let parse = parse_sdbl(input);
    let root = parse.syntax_node();
    let shared = parser::parse_sdbl_with_shared_cache(input);

    let covered = usize::from(root.text_range().len());
    if root.text() != input || covered != input.len() {
        return Some(format!("tree does not reproduce all {} input bytes", input.len()));
    }
    for token in lexer::sdbl::tokenize_sdbl(input) {
        let start = token.offset;
        let end = start + token.text.len();
        if end > input.len() || !input.is_char_boundary(start) || !input.is_char_boundary(end) {
            return Some(format!("token range {start}..{end} is not a valid input slice"));
        }
        if input.get(start..end) != Some(token.text.as_str()) {
            return Some(format!("token at {start} does not equal its source slice"));
        }
    }

    // Every reported range must be inside the input and land between
    // characters — a range that splits one panics whoever slices by it.
    for e in parse.errors() {
        let (start, end) = (usize::from(e.range().start()), usize::from(e.range().end()));
        if start > end || end > input.len() {
            return Some(format!("error range {start}..{end} outside 0..{}", input.len()));
        }
        if !input.is_char_boundary(start) || !input.is_char_boundary(end) {
            return Some(format!("error range {start}..{end} splits a character"));
        }
    }
    for e in shared.errors() {
        let (start, end) = (usize::from(e.range().start()), usize::from(e.range().end()));
        if start > end || end > input.len() {
            return Some(format!("shared-cache error range {start}..{end} outside input"));
        }
        if !input.is_char_boundary(start) || !input.is_char_boundary(end) {
            return Some(format!("shared-cache error range {start}..{end} splits a character"));
        }
    }
    for tree in [root.clone(), shared.syntax_node()] {
        for token in tree.descendants_with_tokens().filter_map(|element| element.into_token()) {
            let start = usize::from(token.text_range().start());
            let end = usize::from(token.text_range().end());
            let text = token.text().to_string();
            if end > input.len()
                || !input.is_char_boundary(start)
                || !input.is_char_boundary(end)
                || input.get(start..end) != Some(text.as_str())
            {
                return Some(format!("tree token range {start}..{end} differs from input"));
            }
        }
    }

    // A query node needs something in the text to justify it: a query
    // keyword, a separator that starts another member, or a combiner, which
    // says in so many words that there is a query on each side of it.
    // Anything above that sum is a node the parser invented.
    let keywords = count_query_keywords(input);
    let separators = input.matches(';').count();
    let combiners = count_combiners(input);
    let ceiling = keywords + separators + combiners + 1;

    let queries = root.descendants().filter(|n| n.kind() == SyntaxKind::SDBL_QUERY).count();
    if queries > ceiling {
        return Some(format!(
            "{queries} query nodes, but at most {ceiling} could be justified \
             ({keywords} query keywords, {separators} separators, {combiners} combiners)"
        ));
    }

    // Each member is complained about at most once for holding no query, so
    // the same ceiling bounds those complaints.
    let complaints = parse
        .errors()
        .iter()
        .filter(|e| {
            // Matched exactly: "ожидался список полей выборки после
            // 'ВЫБРАТЬ' / 'SELECT'" is a different gap and mentions the
            // same words.
            let m = e.message().to_lowercase();
            m == "ожидалось 'выбрать' / 'select'" || m.contains("между разделителями")
        })
        .count();
    if complaints > ceiling {
        return Some(format!("{complaints} missing-member complaints, ceiling {ceiling}"));
    }

    // Parsing is a function of the input.
    let again = parse_sdbl(input);
    if again.errors().len() != parse.errors().len()
        || usize::from(again.syntax_node().text_range().len()) != covered
        || shared.syntax_node().text() != input
        || shared.syntax_node().text_range() != root.text_range()
        || shared.errors().len() != parse.errors().len()
    {
        return Some("plain and shared-cache parsing disagreed".to_string());
    }

    None
}

/// A combiner names a query on either side of itself, so it justifies a
/// slot even where neither side has a keyword of its own.
fn count_combiners(input: &str) -> usize {
    let upper = input.to_uppercase();
    ["ОБЪЕДИНИТЬ", "UNION"].iter().map(|k| upper.matches(k).count()).sum()
}

fn count_query_keywords(input: &str) -> usize {
    let upper = input.to_uppercase();
    ["ВЫБРАТЬ", "SELECT", "УНИЧТОЖИТЬ", "DROP"].iter().map(|k| upper.matches(k).count()).sum()
}

#[test]
fn properties_hold_across_generated_input() {
    let mut failures = Vec::new();

    for seed in 1..=400u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for pieces in [1usize, 2, 3, 5, 8, 13] {
            let input = generate(&mut rng, pieces);
            if let Some(why) = breach(&input) {
                failures.push(format!("seed {seed}, {pieces} pieces: {why}\n  input: {input:?}"));
                if failures.len() >= 5 {
                    break;
                }
            }
        }
    }

    assert!(failures.is_empty(), "properties broken:\n{}", failures.join("\n"));
}

#[test]
fn properties_hold_on_every_truncation_of_a_real_query() {
    // Truncation is what an editor shows the parser on every keystroke, and
    // it reaches states no hand-written case does.
    let queries = [
        "ВЫБРАТЬ РАЗЛИЧНЫЕ ПЕРВЫЕ 10 Т.Код КАК К ИЗ Справочник.Товары КАК Т \
         ГДЕ Т.Активен СГРУППИРОВАТЬ ПО Т.Код ИМЕЮЩИЕ КОЛИЧЕСТВО(*) > 1 \
         УПОРЯДОЧИТЬ ПО Т.Код ИЕРАРХИЯ УБЫВ \
         ИТОГИ СУММА(Т.Код) ПО Т.Код ПЕРИОДАМИ(ДЕНЬ, &Н, &К) КАК Г",
        "ВЫБРАТЬ А ПОМЕСТИТЬ Врем ИЗ Т ЛЕВОЕ СОЕДИНЕНИЕ У ПО Т.А = У.А; \
         ВЫБРАТЬ Б ИЗ Врем {ГДЕ Б.Поле} ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ В ИЗ W",
        "ВЫБРАТЬ Т.Номенклатура ИЗ РегистрНакопления.Т.Остатки(&Период, \
         Номенклатура В (ВЫБРАТЬ С.Ссылка ИЗ Справочник.Номенклатура КАК С)) КАК Т",
        "ВЫБРАТЬ \"А\"\"Б\" КАК Имя, \"строка\n    |продолжение\" КАК Текст ИЗ Т ГДЕ Т.Код = 1",
    ];

    for query in queries {
        for cut in 0..=query.len() {
            if !query.is_char_boundary(cut) {
                continue;
            }
            let prefix = &query[..cut];
            if let Some(why) = breach(prefix) {
                panic!("truncated at {cut}: {why}\n  input: {prefix:?}");
            }
        }
    }
}

#[test]
fn a_long_malformed_package_does_not_stall() {
    // Not a timing assertion — a quadratic path simply never returns, so
    // reaching the assert at all is the property.
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    let mut input = String::new();
    for _ in 0..20_000 {
        input.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
        input.push(' ');
    }

    assert!(breach(&input).is_none(), "{:?}", breach(&input));
}
