/// The production half of a Rust source: every `#[cfg(test)]` element removed, everything
/// else kept verbatim except that checkout CRLF line endings are normalized to LF.
///
/// Cutting the file at the first `#[cfg(test)]` instead would read almost nothing: in
/// `lib.rs` the first one is the module DECLARATION `#[cfg(test)] mod inventory;` on line 13,
/// and production code continues past every later test module. Scanning the whole file
/// instead would read too much: `tools/search/hybrid.rs` closes with
/// `#[cfg(test)] pub(super) mod tests { … }`. Removing the elements is the only reading that
/// is neither.
pub fn production_source(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut kept_from = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = skip_trivia(bytes, i) {
            i = next;
            continue;
        }
        if bytes[i..].starts_with(b"#[cfg(test)]") {
            let end = element_end(bytes, i + b"#[cfg(test)]".len());
            out.push_str(&source[kept_from..i]);
            kept_from = end;
            i = end;
            continue;
        }
        i += 1;
    }
    out.push_str(&source[kept_from..]);
    out.replace("\r\n", "\n")
}

/// Advance past one comment or literal starting at `i`; `None` when `i` starts plain code.
///
/// Braces and semicolons only bound an element when they are code. A `"{"` in a message, a
/// `'{'` char literal and a `/* } */` comment all occur in these sources, and a lifetime
/// (`&'a str`) opens no literal at all — telling those apart is what keeps the boundary walk
/// honest.
fn skip_trivia(b: &[u8], i: usize) -> Option<usize> {
    if b[i..].starts_with(b"//") {
        let mut j = i + 2;
        while j < b.len() && b[j] != b'\n' {
            j += 1;
        }
        return Some(j);
    }
    if b[i..].starts_with(b"/*") {
        let mut j = i + 2;
        let mut depth = 1usize;
        while j < b.len() && depth > 0 {
            if b[j..].starts_with(b"/*") {
                depth += 1;
                j += 2;
            } else if b[j..].starts_with(b"*/") {
                depth -= 1;
                j += 2;
            } else {
                j += 1;
            }
        }
        return Some(j);
    }
    if b[i] == b'r' {
        let mut hashes = 0usize;
        let mut j = i + 1;
        while j < b.len() && b[j] == b'#' {
            hashes += 1;
            j += 1;
        }
        if j < b.len() && b[j] == b'"' {
            j += 1;
            let close: Vec<u8> =
                std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
            while j < b.len() && !b[j..].starts_with(&close) {
                j += 1;
            }
            return Some((j + close.len()).min(b.len()));
        }
    }
    if b[i] == b'"' {
        let mut j = i + 1;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'"' => return Some(j + 1),
                _ => j += 1,
            }
        }
        return Some(b.len());
    }
    if b[i] == b'\'' {
        let mut j = i + 1;
        if j < b.len() && b[j] == b'\\' {
            j += 2;
            while j < b.len() && b[j] != b'\'' {
                j += 1;
            }
            return Some((j + 1).min(b.len()));
        }
        // One character then a closing quote is a literal; anything else is a lifetime, which
        // never closes and must not swallow the code after it.
        let mut k = j;
        while k < b.len() && (b[k] & 0xC0) == 0x80 {
            k += 1;
        }
        k = (k + 1).min(b.len());
        if k < b.len() && b[k] == b'\'' {
            return Some(k + 1);
        }
        return None;
    }
    None
}

/// The end (exclusive) of the element that the `#[cfg(test)]` ending at `attr_end` gates.
///
/// Items, statements and macro calls end at their block, or at the `;` that stands in for one.
/// Everything else a `#[cfg(test)]` can sit on is a member of a comma-separated list — a struct
/// field, an enum variant, a match arm — and ends at its comma. The two are told apart by the
/// first token, not by the first punctuation: a comma is not a boundary inside
/// `fn f(&self) -> Result<(), E>`, where the `()` of the return type brings bracket depth back
/// to zero before it. An enum variant read as an item is how a gate goes quiet: its cut runs
/// past the enum's `}` and takes the next item with it.
fn element_end(b: &[u8], attr_end: usize) -> usize {
    let comma_ends = ends_at_comma(b, attr_end);
    let mut i = attr_end;
    let mut depth = 0usize;
    let mut saw_block = false;
    while i < b.len() {
        if let Some(next) = skip_trivia(b, i) {
            i = next;
            continue;
        }
        match b[i] {
            b'{' | b'(' | b'[' => {
                if b[i] == b'{' && depth == 0 {
                    saw_block = true;
                }
                depth += 1;
                i += 1;
            }
            b'}' | b')' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if saw_block && depth == 0 {
                    return i;
                }
            }
            b';' if depth == 0 && !saw_block => return i + 1,
            b',' if depth == 0 && !saw_block && comma_ends => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// Whether the gated element is a member of a comma-separated list — a field, an enum variant,
/// a match arm — rather than an item, statement or macro call. Decided by the head token: an
/// item keyword (optionally behind a visibility) means it is not.
fn ends_at_comma(b: &[u8], attr_end: usize) -> bool {
    const ITEM_HEADS: [&str; 17] = [
        "fn",
        "mod",
        "use",
        "impl",
        "struct",
        "enum",
        "trait",
        "type",
        "const",
        "static",
        "let",
        "if",
        "match",
        "unsafe",
        "async",
        "extern",
        "macro_rules",
    ];
    let mut i = skip_attributes(b, attr_end);
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    let head = std::str::from_utf8(&b[start..i]).unwrap_or("");
    if ITEM_HEADS.contains(&head) {
        return false;
    }
    if head == "pub" {
        let mut word = skip_group(b, i, b'(');
        while word < b.len() && (b[word] as char).is_whitespace() {
            word += 1;
        }
        let mut end = word;
        while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_') {
            end += 1;
        }
        if ITEM_HEADS.contains(&std::str::from_utf8(&b[word..end]).unwrap_or("")) {
            return false;
        }
    }
    true
}

/// Past whitespace and any further attributes between the gate and the element it gates.
fn skip_attributes(b: &[u8], from: usize) -> usize {
    let mut i = from;
    loop {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if let Some(next) = skip_trivia(b, i) {
            i = next;
            continue;
        }
        if i < b.len() && b[i] == b'#' {
            i += 1;
            i = skip_group(b, i, b'[');
            continue;
        }
        return i;
    }
}

/// Past a balanced `open`-delimited group starting at `i`, or `i` when none starts there.
fn skip_group(b: &[u8], i: usize, open: u8) -> usize {
    if i >= b.len() || b[i] != open {
        return i;
    }
    let close = match open {
        b'(' => b')',
        b'[' => b']',
        _ => b'}',
    };
    let mut j = i;
    let mut depth = 0usize;
    while j < b.len() {
        if b[j] == open {
            depth += 1;
        } else if b[j] == close {
            depth -= 1;
            j += 1;
            if depth == 0 {
                return j;
            }
            continue;
        }
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_source_handles_checkout_line_endings() {
        for newline in ["\n", "\r\n"] {
            let source =
                format!("fn production() {{}}{newline}#[cfg(test)]{newline}mod tests {{}}");
            assert_eq!(production_source(&source), "fn production() {}\n");
        }
    }

    #[test]
    fn production_source_drops_a_gated_module_declaration_without_truncating() {
        let source = "mod a;\n#[cfg(test)]\nmod inventory;\nmod b;\nfn prod() {}\n";
        let production = production_source(source);
        assert!(!production.contains("mod inventory;"));
        assert!(production.contains("mod b;") && production.contains("fn prod() {}"));
    }

    #[test]
    fn production_source_drops_a_visibility_qualified_test_module() {
        let source = "fn prod() {}\n#[cfg(test)]\npub(super) mod tests {\n    fn secret() {}\n}\nfn after() {}\n";
        let production = production_source(source);
        assert!(!production.contains("secret"));
        assert!(production.contains("fn prod() {}") && production.contains("fn after() {}"));
    }

    #[test]
    fn production_source_keeps_code_between_two_test_modules() {
        let source = "fn a() {}\n#[cfg(test)]\nmod t1 { fn x() {} }\nfn b() {}\n#[cfg(test)]\nmod t2 { fn y() {} }\nfn c() {}\n";
        let production = production_source(source);
        assert!(!production.contains("fn x()") && !production.contains("fn y()"));
        for kept in ["fn a() {}", "fn b() {}", "fn c() {}"] {
            assert!(production.contains(kept), "{kept} was cut");
        }
    }

    #[test]
    fn production_source_ends_a_gated_function_at_its_body_not_at_a_comma() {
        // The `()` of `Result<(), E>` brings bracket depth back to zero, so a rule that ended the
        // element at the first depth-zero comma would leave the body behind as "production".
        let returns_unit_result = "fn keep() {}\n#[cfg(test)]\npub(crate) fn gated(\n    a: &A,\n) -> Result<(), crate::x::Withdrawn> {\n    secret_body();\n}\nfn after() {}\n";
        let production = production_source(returns_unit_result);
        assert!(!production.contains("secret_body"), "the gated body leaked: {production}");
        assert!(production.contains("fn keep() {}") && production.contains("fn after() {}"));
    }

    #[test]
    fn production_source_ends_a_gated_field_at_its_comma() {
        let source = "struct S {\n    a: u8,\n    #[cfg(test)]\n    secret: u8,\n    b: u8,\n}\n";
        let production = production_source(source);
        assert!(!production.contains("secret"));
        assert!(production.contains("a: u8,") && production.contains("b: u8,"));
    }

    #[test]
    fn production_source_ends_a_gated_macro_or_statement_at_its_own_block() {
        let macro_call = "fn keep() {}\n#[cfg(test)]\nthread_local! {\n    static SECRET: u8 = 0;\n}\nfn after() {}\n";
        let production = production_source(macro_call);
        assert!(!production.contains("SECRET"));
        assert!(production.contains("fn after() {}"), "the macro cut swallowed production code");

        let statement = "fn prod() {\n    #[cfg(test)]\n    SECRET_HOOK.with(|slot| {\n        slot.take();\n    });\n    keep();\n}\n";
        let production = production_source(statement);
        assert!(!production.contains("SECRET_HOOK"));
        assert!(production.contains("keep();"), "the statement cut swallowed production code");
    }

    #[test]
    fn production_source_ends_a_gated_variant_or_arm_at_its_comma() {
        let variant =
            "enum E {\n    A,\n    #[cfg(test)]\n    B,\n}\nfn prod() { x.owns_caches(); }\n";
        let production = production_source(variant);
        assert!(!production.contains("B,"), "the gated variant survived: {production}");
        assert!(
            production.contains("owns_caches"),
            "the gated variant swallowed the code after it: {production}",
        );

        let arm = "fn pick(v: u8) -> u8 {\n    match v {\n        0 => 1,\n        #[cfg(test)]\n        1 => 2,\n        _ => 3,\n    }\n}\nfn prod() { x.owns_caches(); }\n";
        let production = production_source(arm);
        assert!(!production.contains("1 => 2"), "the gated arm survived: {production}");
        assert!(
            production.contains("owns_caches"),
            "the gated arm swallowed the code after it: {production}",
        );
    }

    /// `<…>` is not a bracket to this scanner, so a gated statement whose generic arguments hold a
    /// comma ends at that comma. The cut falls short, and what it leaves stays production: a gate
    /// reads too much here, never too little.
    #[test]
    fn production_source_keeps_the_tail_of_a_gated_statement_with_generic_arguments() {
        let source =
            "fn prod() {\n    #[cfg(test)]\n    foo::<A, B>(x.owns_caches());\n    keep();\n}\n";
        let production = production_source(source);
        assert!(production.contains("owns_caches"), "the tail was read as test code: {production}");
        assert!(production.contains("keep();"), "{production}");
    }

    #[test]
    fn production_source_is_not_confused_by_literals_or_lifetimes() {
        let tricky = "fn prod<'a>(x: &'a str) -> char {\n    let s = \"{\";\n    let r = r#\"a { b \"#;\n    let _ = (s, r);\n    '{'\n}\n/* outer /* inner */ still */\n#[cfg(test)]\nmod tests { fn secret() {} }\nfn after() {}\n";
        let production = production_source(tricky);
        assert!(!production.contains("secret"));
        assert!(production.contains("fn prod<'a>") && production.contains("fn after() {}"));
    }
}
