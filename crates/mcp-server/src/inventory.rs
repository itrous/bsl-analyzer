use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source directory is readable") {
        let path = entry.expect("source entry is readable").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && path.file_name().is_none_or(|name| name != "inventory.rs")
        {
            out.push(path);
        }
    }
}

fn production_sources() -> Vec<PathBuf> {
    let mut sources = Vec::new();
    rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut sources);
    sources
}

/// The production half of a Rust source: every `#[cfg(test)]` element removed, everything
/// else kept verbatim except that checkout CRLF line endings are normalized to LF.
///
/// Cutting the file at the first `#[cfg(test)]` instead would read almost nothing: in
/// `lib.rs` the first one is the module DECLARATION `#[cfg(test)] mod inventory;` on line 13,
/// and production code continues past every later test module. Scanning the whole file
/// instead would read too much: `tools/search/hybrid.rs` closes with
/// `#[cfg(test)] pub(super) mod tests { … }`. Removing the elements is the only reading that
/// is neither.
pub(crate) fn production_source(source: &str) -> String {
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

#[test]
fn production_source_handles_checkout_line_endings() {
    for newline in ["\n", "\r\n"] {
        let source = format!("fn production() {{}}{newline}#[cfg(test)]{newline}mod tests {{}}");
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
    let variant = "enum E {\n    A,\n    #[cfg(test)]\n    B,\n}\nfn prod() { x.owns_caches(); }\n";
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

/// The gate is only as good as the text it reads. These pin it against the two real shapes
/// that made the textual prefix read the wrong half of the crate.
/// The other half of the real-source anchors: a gate that reads too LITTLE is the defect this
/// scanner exists to remove, and a source with a `#[cfg(test)]` enum variant is the shape that
/// brings it back — the variant is not an item, so an item-shaped cut runs past the enum and
/// takes whatever follows it with it.
#[test]
fn production_source_keeps_what_follows_a_gated_enum_variant_of_a_real_source() {
    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/change_hub.rs"))
            .expect("source is readable");
    let source = source.replace("\r\n", "\n");
    assert!(source.contains("    #[cfg(test)]\n    Tick,"), "the gated variant is still there");
    assert!(
        production_source(&source).contains("pub(crate) struct WorkspaceChangeHub"),
        "the item after the gated enum variant is invisible to the gate",
    );
}

#[test]
fn production_source_keeps_the_request_handlers_of_lib_rs() {
    let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("lib.rs is readable");
    let production = production_source(&source);
    for handler in ["async fn metadata(", "async fn graph(", "pub async fn serve_stdio("] {
        assert!(
            production.contains(handler),
            "{handler} is outside the gate: {} of {} lines survive",
            production.lines().count(),
            source.lines().count(),
        );
    }
    assert!(
        !production.contains("mod resident_state_tests"),
        "a test module is read as production"
    );
}

#[test]
fn production_source_drops_the_test_modules_of_real_sources() {
    let cases = [
        (
            "src/tools/search/hybrid.rs",
            "assert_all_search_modes_are_resident_only_under_held_lease",
        ),
        ("src/state/mod.rs", "fn workspace_search_missing_engine_is_an_operation_error"),
        ("src/graph/snapshot.rs", "let disk = self.current_disk_fp();"),
        ("src/state/sync.rs", "fn a_point_refresh_feeds_a_nested_config_from_the_resident"),
        ("src/workspace_lease.rs", "fn hold_file_lock_for_test"),
        ("src/change_hub.rs", "fn events_seen_counts_every_raw_event"),
    ];
    for (relative, test_only) in cases {
        let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative))
            .expect("source is readable");
        // A marker that is gone from the file would pass the check below for nothing.
        assert!(source.contains(test_only), "{relative}: the marker {test_only} is gone");
        assert!(
            !production_source(&source).contains(test_only),
            "{relative}: {test_only} is read as production code",
        );
    }
}

#[test]
fn no_generic_or_unclassified_production_lease_callers() {
    let forbidden = ["with_ownership_outcome", "with_ownership_checkpointed", "LeaseOutcome"];
    for path in production_sources() {
        let source = std::fs::read_to_string(&path).expect("Rust source is readable");
        for token in forbidden {
            assert!(!source.contains(token), "{} still contains {token}", path.display());
        }
    }
}

#[test]
fn fence_callers_are_exactly_classified() {
    let expected = [
        ("graph/build.rs", 3),
        ("graph/snapshot.rs", 1),
        ("state/bootstrap.rs", 3),
        ("state/embed.rs", 4),
        ("state/mod.rs", 2),
        ("workspace_lease.rs", 1),
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in production_sources() {
        let source = std::fs::read_to_string(&path).expect("Rust source is readable");
        let source = production_source(&source);
        let actual = source.matches(".publish_short(").count()
            + source.matches(".publish_checkpointed(").count();
        let relative = path.strip_prefix(&root).unwrap();
        let classified = expected
            .iter()
            .find_map(|(path, count)| (Path::new(path) == relative).then_some(*count))
            .unwrap_or(0);
        assert_eq!(actual, classified, "unclassified fence caller count in {}", relative.display());
    }
}

/// A graph database is not copied by production code at all: reads use the file itself, a full
/// build writes its own replacement, and a point patch writes into the file in one transaction.
/// A copy anywhere would be a full copy of the file the counters and the disk audit do not see.
#[test]
fn production_code_copies_no_graph_database() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut copies = Vec::new();
    for path in production_sources() {
        if is_test_only_module(&path) {
            continue;
        }
        let source =
            production_source(&std::fs::read_to_string(&path).expect("Rust source is readable"));
        for (index, _) in source.match_indices("fs::copy(") {
            copies.push((path.strip_prefix(&root).unwrap().to_path_buf(), index));
        }
        assert!(
            !source.contains("NamedTempFile") || !path.starts_with(root.join("graph")),
            "{} makes a temporary copy of graph data",
            path.display()
        );
    }
    assert!(copies.is_empty(), "graph copies in production code: {copies:?}");
}

/// The published graph file is opened only by the graph module that lends its handles: a
/// consumer opening it itself keeps a handle no publication can account for.
#[test]
fn only_the_graph_module_opens_graph_databases() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let owners = [root.join("graph"), root.join("graph_query.rs")];
    for path in production_sources() {
        if owners.iter().any(|owner| path.starts_with(owner)) {
            continue;
        }
        let source =
            production_source(&std::fs::read_to_string(&path).expect("Rust source is readable"));
        assert!(
            !source.contains("GraphDb::open"),
            "{} opens a graph database past the graph store",
            path.display()
        );
    }
}

#[test]
fn request_paths_do_not_call_lease_or_mutation_helpers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let lib = root.join("lib.rs");
    let tools = root.join("tools");
    let forbidden = [
        ".publish_short(",
        ".publish_checkpointed(",
        ".owns_caches(",
        ".owns_caches_now(",
        "apply_workspace_search(",
        "apply_workspace_search_checkpointed(",
        "capture_point_refresh(",
        "publish_point_refresh(",
        "snapshot_blocking(",
        "read_blocking(",
    ];
    for path in
        production_sources().into_iter().filter(|path| path == &lib || path.starts_with(&tools))
    {
        let source = std::fs::read_to_string(&path).expect("Rust source is readable");
        let source = production_source(&source);
        for token in forbidden {
            assert!(!source.contains(token), "{} contains request-time {token}", path.display());
        }
    }

    // One frame further out. The helpers a request path IS allowed to call have to answer
    // from what the process already knows: an accessor that goes to the lease itself moves
    // the call out of the scan above without moving it off the request thread, which is how
    // a status answer came to wait on another daemon's lock file.
    let state = production_source(
        &std::fs::read_to_string(root.join("state").join("mod.rs"))
            .expect("Rust source is readable"),
    );
    for accessor in ["owns_caches_for_status"] {
        let body = fn_body(&state, accessor);
        for token in forbidden {
            assert!(
                !body.contains(token),
                "state/mod.rs::{accessor} answers a request with {token}: {body}",
            );
        }
    }
}

/// The body of `fn <name>` — from its opening brace to the one that closes it.
#[cfg(test)]
fn fn_body<'a>(source: &'a str, name: &str) -> &'a str {
    let (_, tail) = source.split_once(&format!("fn {name}")).expect("the function is declared");
    let open = tail.find('{').expect("the function has a body");
    let mut depth = 0usize;
    for (at, ch) in tail[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &tail[open..open + at + 1];
                }
            }
            _ => {}
        }
    }
    panic!("the body of {name} is not closed");
}

/// The gate above reads `lib.rs` and `tools/`; this proves it can still fail there. Without
/// it a future truncation would make every assertion above vacuous and silent.
#[test]
fn the_request_path_gate_fails_on_an_injected_lease_call() {
    let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("lib.rs is readable");
    let mut lines: Vec<&str> = source.lines().collect();
    let handler = lines
        .iter()
        .position(|line| line.contains("async fn metadata("))
        .expect("lib.rs serves a metadata handler");
    lines.insert(handler + 1, "        let _ = self.state.owns_caches();");
    let mutant = production_source(&lines.join("\n"));
    assert!(
        mutant.contains(".owns_caches("),
        "a request-time lease call injected into a handler is invisible to the gate",
    );
}

/// Every hold of the search engine goes through its admission queue, which alone decides who
/// is next: the raw mutex is a private field of `AdmittedEngine`, and no other production type
/// wraps an engine in a mutex someone could lock past the queue.
#[test]
fn the_search_engine_is_held_only_through_its_admission() {
    let admission = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools/search/acquire.rs");
    // Assembled at run time, so this file's own text is no match.
    let raw = [
        ["Mutex<Option<", "SearchEngine>>"].concat(),
        ["Mutex<Option<bsl_search::", "SearchEngine>>"].concat(),
    ];
    let mut holders = Vec::new();
    for path in production_sources() {
        let source = std::fs::read_to_string(&path).expect("Rust source is readable");
        let compact: String =
            production_source(&source).chars().filter(|c| !c.is_whitespace()).collect();
        let count: usize = raw.iter().map(|needle| compact.matches(needle.as_str()).count()).sum();
        if count > 0 {
            holders.push((path, count));
        }
    }
    assert_eq!(holders, [(admission, 1)], "a raw engine mutex outside the admission queue");
}

/// What a wait or an engine hold in production text is bounded by. The gate below is this
/// classification: a key nobody classified is as red as a key that vanished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waiting {
    /// The background contract's own primitives, and the waits built on them: released by
    /// `OwnerStop::stop` itself, directly or through a registered waker.
    Owner,
    /// Bounded by something other than a stop, and bounded in the source: a lock wait, a
    /// busy timeout, an acknowledgement with a deadline, a poll interval.
    Bounded,
    /// A request's own wait, called off by the request's cancellation.
    Request,
    /// A subsystem that stops on a protocol of its own — the lease heartbeat, the external
    /// baseline service, diagnostics — whose shutdown is called before the owners' stop.
    OwnProtocol,
}

/// Every wait and every engine hold in production text, keyed by `(file, enclosing fn, token)`.
///
/// `thread::sleep` is spelled without its bracket on purpose: two boot retries pass the
/// function itself as an argument, and a key that ended at `(` would not see them.
const WAITS: &[(&str, &str, &str, Waiting)] = &[
    // The external baseline service: its own `shutdown`, called before the owners stop.
    ("baseline.rs", "serve", ".recv(", Waiting::OwnProtocol),
    ("baseline.rs", "shutdown", ".recv_timeout(", Waiting::OwnProtocol),
    ("baseline.rs", "wait_ready", ".wait_timeout(", Waiting::OwnProtocol),
    // The hub's own threads, stopped by `change_hub.shutdown()` — step 4 of the order.
    ("change_hub.rs", "poll_until_stopped", ".wait(", Waiting::OwnProtocol),
    ("change_hub.rs", "run_hub_thread", ".recv_timeout(", Waiting::OwnProtocol),
    ("change_hub.rs", "run_polling", ".recv_timeout(", Waiting::OwnProtocol),
    ("change_hub.rs", "wait", ".wait_timeout_while(", Waiting::OwnProtocol),
    // The re-arm handshake and the hub's stop poll: bounded in the source by
    // `REARM_ACK_TIMEOUT` and `STOP_BUDGET`.
    ("change_hub.rs", "handshake", ".recv_timeout(", Waiting::Bounded),
    ("change_hub.rs", "handshake", "thread::sleep", Waiting::Bounded),
    ("change_hub.rs", "stop", "thread::sleep", Waiting::Bounded),
    ("change_hub.rs", "stop", "thread::sleep", Waiting::Bounded),
    // What a consumer parks in: released by the hub's `closing`, which the stop raises, and
    // by the stop predicate the consumer hands in.
    ("change_hub.rs", "wait_for_change_or", ".wait_timeout(", Waiting::Owner),
    ("change_hub.rs", "watch_readiness_or", ".wait_timeout(", Waiting::Owner),
    // Diagnostics stops on its own `shutdown`, which wakes this park.
    (
        "diagnostics_state/lifecycle.rs",
        "spawn_sweeper",
        ".wait_timeout_while(",
        Waiting::OwnProtocol,
    ),
    (
        "diagnostics_state/session.rs",
        "read_retrying_a_stale_miss",
        "thread::sleep",
        Waiting::Request,
    ),
    // The build watchdog holds a stop of its own and goes with the build.
    ("graph_db.rs", "spawn_build_watchdog", ".wait_timeout(", Waiting::OwnProtocol),
    // A read waiting for a pooled graph handle to come back: bounded by the caller's wait.
    ("graph/snapshot.rs", "checkout", ".wait_timeout(", Waiting::Bounded),
    // The budget of a patch's SQL: a watchdog thread that interrupts the statement in flight.
    ("graph_db.rs", "begin_body_patch", ".recv_timeout(", Waiting::Bounded),
    // A replacement waits for the reads in flight, bounded by the installation wait.
    ("graph/snapshot.rs", "pause_for_replacement", ".wait_timeout(", Waiting::Bounded),
    // A superseded graph waits for its reads in flight before it closes the file; it ends when
    // they return, and no read is cut short.
    ("graph/snapshot.rs", "wait_until_returned", ".wait(", Waiting::OwnProtocol),
    // A new owner re-trying the graph file's access lock, one short sleep per attempt, leaving
    // on the stop it checks between them.
    ("graph/state.rs", "acquire_graph_access", "thread::sleep", Waiting::Bounded),
    // The boot's publication takes the engine for one attempt at a time; the pause between
    // lease attempts is not held under it, which is why this wait is the owner's and bounded
    // by the attempt rather than by a foreign lease holder.
    ("state/bootstrap.rs", "publish_engine_with_retry", "acquire_for_owner(", Waiting::Owner),
    ("state/bootstrap.rs", "finish_initialization", "acquire_for_owner(", Waiting::Owner),
    // The one uncancellable hold, taken only by the reference profile's own shutdown.
    ("state/bootstrap.rs", "shutdown", "take_for_shutdown(", Waiting::OwnProtocol),
    ("state/embed.rs", "kick_context_reembed", "acquire_for_owner(", Waiting::Owner),
    (
        "state/embed.rs",
        "refresh_search_contexts_after_graph_with_store",
        "acquire_for_owner(",
        Waiting::Owner,
    ),
    ("state/embed.rs", "refresh_search_roots_after_graph", "acquire_for_owner(", Waiting::Owner),
    ("state/embed.rs", "refresh_search_roots_after_graph", "acquire_for_owner(", Waiting::Owner),
    ("state/embed.rs", "run_overlay_warmup", "acquire_for_owner(", Waiting::Owner),
    ("state/embed.rs", "run_overlay_warmup", "acquire_for_owner(", Waiting::Owner),
    ("state/embed.rs", "spawn_embed_pass", "acquire_for_owner(", Waiting::Owner),
    ("state/embed.rs", "spawn_embed_pass", "acquire_for_owner(", Waiting::Owner),
    ("state/mod.rs", "apply_workspace_search", "acquire_for_owner(", Waiting::Owner),
    ("state/mod.rs", "apply_workspace_search_checkpointed", "acquire_for_owner(", Waiting::Owner),
    // `OwnerStop::sleep` itself: the primitive every retry pause is built on.
    ("state/mod.rs", "sleep", ".wait_timeout(", Waiting::Owner),
    ("state/mod.rs", "wait_empty", ".wait_timeout(", Waiting::Owner),
    ("state/overlay_backlog.rs", "backlog", "acquire_for_owner(", Waiting::Owner),
    ("state/overlay_backlog.rs", "round", "acquire_for_owner(", Waiting::Owner),
    ("state/overlay_backlog.rs", "run", ".wait(", Waiting::Owner),
    ("state/overlay_backlog.rs", "wait", ".wait_timeout(", Waiting::Owner),
    ("state/overlay_retry.rs", "run", ".wait_timeout(", Waiting::Owner),
    ("state/overlay_retry.rs", "run", ".wait_timeout(", Waiting::Owner),
    ("state/overlay_retry.rs", "run", ".wait_timeout(", Waiting::Owner),
    ("state/overlay_retry.rs", "run_pass", "acquire_for_owner(", Waiting::Owner),
    ("state/overlay_retry.rs", "should_run", "acquire_for_owner(", Waiting::Owner),
    // The retry resolves saved graph keys against the current search roots under the owner's stop.
    ("state/sync.rs", "apply_prepared_search_drift", "acquire_for_owner(", Waiting::Owner),
    ("state/sync.rs", "apply_prepared_search_drift", "acquire_for_owner(", Waiting::Owner),
    ("state/sync.rs", "materialize_search_drift", "acquire_for_owner(", Waiting::Owner),
    ("state/sync.rs", "prepare_search_drift", "acquire_for_owner(", Waiting::Owner),
    ("state/sync.rs", "registered_roots_and_exclusions", "acquire_for_owner(", Waiting::Owner),
    // The request's own acquire, called off by the request's cancellation.
    ("tools/search/acquire.rs", "acquire_engine_within", "thread::sleep", Waiting::Request),
    ("tools/search/wait.rs", "await_reply", ".recv_timeout(", Waiting::Request),
    // The admission itself: the queue wait and the head's poll, both refusing on the stop.
    ("tools/search/acquire.rs", "acquire_for_owner", "acquire_for_owner(", Waiting::Owner),
    ("tools/search/acquire.rs", "acquire_for_owner", "thread::sleep", Waiting::Owner),
    ("tools/search/acquire.rs", "wait_turn", ".wait_timeout(", Waiting::Owner),
    ("tools/search/acquire.rs", "take_for_shutdown", "take_for_shutdown(", Waiting::OwnProtocol),
    // The lease: a lock wait bounded by `LOCK_WAIT`, and the heartbeat's own slice.
    ("workspace_lease.rs", "acquire", "thread::sleep", Waiting::Bounded),
    ("workspace_lease.rs", "spawn_heartbeat", "thread::sleep", Waiting::OwnProtocol),
];

/// The tokens the inventory is keyed by. Adding one is a widening of the gate; removing one
/// blinds it.
const WAIT_TOKENS: [&str; 9] = [
    "thread::sleep",
    ".wait(",
    ".wait_timeout(",
    ".wait_timeout_while(",
    ".recv(",
    ".recv_timeout(",
    "acquire_for_owner(",
    "lock_for_owner(",
    "take_for_shutdown(",
];

/// The `(file, enclosing fn, token)` keys of `source`, which is already production text.
///
/// The enclosing function is the last `fn` declared before the hit. A closure passed to
/// another function is attributed to the function that spells it, which is the level the
/// classification is made at anyway: what bounds a wait is the owner that reaches it.
fn waits_in(relative: &str, source: &str) -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    let mut enclosing = String::from("<file>");
    for line in source.lines() {
        if let Some(name) = line.split("fn ").nth(1).filter(|_| line.contains("fn ")) {
            let name: String =
                name.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() {
                enclosing = name;
            }
        }
        for token in WAIT_TOKENS {
            for _ in 0..line.matches(token).count() {
                found.push((relative.to_owned(), enclosing.clone(), token.to_owned()));
            }
        }
    }
    found
}

/// Whether this file is a whole module the compiler only builds for tests, read off the
/// declaration its parent makes: a `#[cfg(test)] mod x;` leaves no `mod x;` in the parent's
/// production text. Asked rather than listed, so un-gating such a module puts its waits into
/// the inventory instead of leaving them permanently exempt.
fn is_test_only_module(path: &Path) -> bool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else { return false };
    // A directory's own module is named by the directory and declared one level further up;
    // the crate root is declared by nobody.
    let (name, from) = if stem == "mod" {
        let dir = path.parent().expect("a mod.rs has a directory");
        let Some(name) = dir.file_name().and_then(|name| name.to_str()) else { return false };
        (name, dir.parent().expect("a module directory has a parent"))
    } else {
        (stem, path.parent().expect("a source file has a directory"))
    };
    if path == root.join("lib.rs") {
        return false;
    }
    let declaring = if from == root { root.join("lib.rs") } else { from.join("mod.rs") };
    let Ok(source) = std::fs::read_to_string(&declaring) else { return false };
    !production_source(&source).contains(&format!("mod {name};"))
}

fn production_waits() -> Vec<(String, String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for path in production_sources() {
        if is_test_only_module(&path) {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("Rust source is readable");
        let relative = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        found.extend(waits_in(&relative, &production_source(&source)));
    }
    found.sort();
    found
}

/// The module-level exemption above is real: these modules are built only for tests, and the
/// gate would otherwise have to classify every wait their helpers make.
#[test]
fn the_test_only_modules_are_the_ones_the_parent_gates() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let exempt: Vec<String> = production_sources()
        .into_iter()
        .filter(|path| is_test_only_module(path))
        .map(|path| path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"))
        .collect();
    let mut exempt = exempt;
    exempt.sort();
    assert_eq!(
        exempt,
        [
            "diagnostics_state/test_support.rs",
            "graph/portable_workspace_graph_tests.rs",
            "graph/test_support.rs",
            "indexing_runtime_tests.rs",
            "payload_smoke_tests.rs",
            "serve_stream_tests.rs",
            "state/indexing_tests.rs",
            "state/test_support.rs",
            "tools/search/cancel_tests.rs",
            "tools/search/test_support.rs",
            "walk_probe.rs",
        ],
        "the set of test-only modules moved"
    );
}

/// Remove ONE occurrence of `key` from `pool`, and say whether there was one.
fn take_one(pool: &mut Vec<(String, String, String)>, key: &(String, String, String)) -> bool {
    match pool.iter().position(|candidate| candidate == key) {
        Some(at) => {
            pool.remove(at);
            true
        }
        None => false,
    }
}

#[test]
fn every_production_wait_is_classified() {
    let actual = production_waits();
    let expected: Vec<(String, String, String)> = WAITS
        .iter()
        .map(|(file, function, token, _)| {
            ((*file).to_owned(), (*function).to_owned(), (*token).to_owned())
        })
        .collect();
    let mut expected_sorted = expected.clone();
    expected_sorted.sort();

    // Compared as multisets, not as sets: a second wait added beside an identical one in the
    // same function is a second wait, and a gate that collapsed the two would let it in.
    let mut unclassified = actual.clone();
    let mut vanished = expected_sorted.clone();
    unclassified.retain(|key| !take_one(&mut vanished, key));
    let mut left = actual.clone();
    vanished.retain(|key| !take_one(&mut left, key));
    assert!(
        unclassified.is_empty() && vanished.is_empty(),
        "unclassified waits: {unclassified:#?}\nwaits that vanished: {vanished:#?}",
    );
}

/// The gate above is worth its table only if the table can fail. A sleep injected into the
/// retry driver — the defect the background contract forbids by name, since a plain sleep
/// there outlives the daemon by a whole backoff — has to come out as a key nobody classified.
#[test]
fn the_wait_inventory_fails_on_an_injected_sleep() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/state/overlay_retry.rs");
    let source = std::fs::read_to_string(&path).expect("the retry driver is readable");
    let mut lines: Vec<&str> = source.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.contains("fn should_run("))
        .expect("the driver still asks whether to run");
    // Assembled so this file's own text is no match for the token it injects.
    let injected = ["        std::thread::", "sleep(TICK);"].concat();
    lines.insert(at + 1, &injected);
    let mutant = production_source(&lines.join("\n"));

    let keys = waits_in("state/overlay_retry.rs", &mutant);
    let expected: Vec<(String, String, String)> = WAITS
        .iter()
        .map(|(file, function, token, _)| {
            ((*file).to_owned(), (*function).to_owned(), (*token).to_owned())
        })
        .collect();
    let unclassified: Vec<_> = keys.iter().filter(|key| !expected.contains(key)).collect();

    assert_eq!(
        unclassified,
        [&(
            "state/overlay_retry.rs".to_owned(),
            "should_run".to_owned(),
            "thread::sleep".to_owned(),
        )],
        "an injected sleep in the retry driver is invisible to the inventory",
    );
}

/// Every exact filter the Windows gate runs must name a test that exists on Windows.
///
/// `run_exact` fails a filter that executes no test — that is what makes a renamed or moved
/// test visible instead of silently passing. A `#[cfg(unix)]` test copied into the Windows
/// list is exactly that empty answer: the step fails over a case that cannot exist there,
/// and it fails for a reason that has nothing to do with the code under review.
#[test]
fn the_windows_gate_names_no_test_that_only_unix_builds() {
    let workflow = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml"),
    )
    .expect("the workflow is readable");
    let blocks: Vec<&str> = workflow.split("- name: Drift contract").collect();
    assert_eq!(blocks.len(), 3, "the two Drift contract steps moved");
    let windows = blocks[2];

    let sources: Vec<String> = production_sources()
        .into_iter()
        .chain([Path::new(env!("CARGO_MANIFEST_DIR")).join("src/inventory.rs")])
        .map(|path| std::fs::read_to_string(&path).expect("Rust source is readable"))
        .collect();

    let platform_only = windows_filters_that_do_not_build_there(windows, &sources);
    assert!(
        platform_only.is_empty(),
        "the Windows gate runs filters that do not build there: {platform_only:#?}",
    );
}

/// Which of the Windows step's filters name a test that does not build on Windows.
#[cfg(test)]
fn windows_filters_that_do_not_build_there(windows: &str, sources: &[String]) -> Vec<String> {
    let mut platform_only = Vec::new();
    for line in windows.lines().map(str::trim).filter(|line| line.starts_with("run_exact ")) {
        let filter = line.split_whitespace().last().expect("a filter");
        let Some(name) = filter.rsplit("::").next() else { continue };
        let needle = format!("fn {name}(");
        for source in sources {
            let Some(at) = source.find(&needle) else { continue };
            if builds_elsewhere_only(head_before(source, at, 300)) {
                platform_only.push(filter.to_owned());
            }
            break;
        }
    }
    platform_only
}

/// Whether the attributes above a test exclude Windows.
///
/// Both directions have to be read, and reading only the positive one is how a fixture gated
/// `cfg(not(windows))` — the plainest spelling of "not there" — went on looking like a test
/// the Windows step could run.
#[cfg(test)]
fn builds_elsewhere_only(head: &str) -> bool {
    // The VALUE decides, not the key: `target_os = "windows"` is the one spelling of a test
    // that builds ONLY there, and `not(target_os = "linux")` builds there too.
    let targets = |prefix: &str| -> Vec<String> {
        head.match_indices(prefix)
            .filter_map(|(at, _)| {
                let rest = &head[at + prefix.len()..];
                let rest = rest.trim_start().strip_prefix('=')?.trim_start().strip_prefix('"')?;
                rest.split_once('"').map(|(value, _)| value.to_owned())
            })
            .collect()
    };
    head.contains("cfg(unix")
        || head.contains("cfg(not(windows")
        || targets("cfg(target_os").iter().any(|os| os != "windows")
        || targets("cfg(not(target_os").iter().any(|os| os == "windows")
}

/// The gate above is worth its list only if it can fail, and on every spelling of "not on
/// Windows" rather than the one that happened to be in the tree when it was written.
#[test]
fn the_windows_gate_sees_every_spelling_of_a_fixture_that_is_not_there() {
    let sources: Vec<String> = [
        "#[cfg(unix)]\n#[test]\nfn only_on_unix() {}\n",
        "#[cfg(not(windows))]\n#[test]\nfn not_on_windows() {}\n",
        "#[cfg(not(target_os = \"windows\"))]\n#[test]\nfn not_on_that_target() {}\n",
        "#[test]\nfn everywhere() {}\n",
    ]
    .iter()
    .map(|source| (*source).to_owned())
    .collect();
    let windows = "          run_exact mcp-server m::only_on_unix\n          \
                   run_exact mcp-server m::not_on_windows\n          \
                   run_exact mcp-server m::not_on_that_target\n          \
                   run_exact mcp-server m::everywhere\n";

    assert_eq!(
        windows_filters_that_do_not_build_there(windows, &sources),
        ["m::only_on_unix", "m::not_on_windows", "m::not_on_that_target"],
        "a filter naming a test that does not build on Windows is invisible to the gate",
    );
}

/// And the other direction: a test that builds ONLY on Windows, or everywhere but some other
/// target, is exactly what the Windows step may run.
#[test]
fn the_windows_gate_accepts_a_fixture_that_builds_there() {
    let sources: Vec<String> = [
        "#[cfg(target_os = \"windows\")]\n#[test]\nfn only_on_windows() {}\n",
        "#[cfg(windows)]\n#[test]\nfn windows_spelled_short() {}\n",
        "#[cfg(not(target_os = \"linux\"))]\n#[test]\nfn not_on_linux() {}\n",
        "#[cfg(target_os = \"linux\")]\n#[test]\nfn only_on_linux() {}\n",
    ]
    .iter()
    .map(|source| (*source).to_owned())
    .collect();
    let windows = "          run_exact mcp-server m::only_on_windows\n          \
                   run_exact mcp-server m::windows_spelled_short\n          \
                   run_exact mcp-server m::not_on_linux\n          \
                   run_exact mcp-server m::only_on_linux\n";

    assert_eq!(
        windows_filters_that_do_not_build_there(windows, &sources),
        ["m::only_on_linux"],
        "a filter naming a test that builds on Windows was refused by the gate",
    );
}

/// The `back` bytes of `source` before `at`, clamped OUTWARD to a character boundary.
///
/// The sources of this crate are full of Cyrillic comments, so "300 bytes back" lands inside a
/// character often enough to matter: slicing there panics, and a panic in a gate is worse than
/// the verdict it was hiding — it stops the step without answering it. Clamping outward rather
/// than inward keeps the window at least as wide as asked for, so nothing the check was
/// supposed to see falls out of it.
fn head_before(source: &str, at: usize, back: usize) -> &str {
    let mut start = at.saturating_sub(back);
    while start > 0 && !source.is_char_boundary(start) {
        start -= 1;
    }
    &source[start..at]
}

/// A window that begins inside a multi-byte character is widened to the character's start, and
/// the slice is taken rather than panicked on. The sources this runs over are Cyrillic-heavy,
/// so the arithmetic lands mid-character on real input, not on a contrived one.
#[test]
fn the_gate_window_never_splits_a_character() {
    // "…" is three bytes; a window that asks to start inside it must open on it, not panic.
    let source = "«авторитетно»fn target(";
    let at = source.find("fn target(").expect("the needle");
    for back in 0..=at {
        let head = head_before(source, at, back);
        assert!(head.len() >= back, "the window shrank below what was asked for");
        assert!(source.ends_with(&[head, &source[at..]].concat()));
    }
}

/// N2 freezes the client-visible reason codes, and this is the whole of them.
///
/// `reasonCode` travels in `data` on every gated MCP error, so a new one is a contract change
/// that reaches callers, not an implementation detail — and the profile it is answered from is
/// supposed to be the one clients already have. Read from the PRODUCTION half of the sources,
/// so a code introduced only in a test does not freeze itself in.
#[test]
fn the_reason_codes_clients_see_are_the_frozen_set() {
    // Exactly the set reachable at the base of this delta, verified against it file by file.
    const FROZEN: [&str; 6] = [
        "baseline_required",
        "baseline_unavailable",
        "expired_branch",
        "project_config_error",
        "worker_gone",
        "worker_spawn_failed",
    ];

    let needle = ["reason_code", ": \""].concat();
    let mut found: Vec<String> = Vec::new();
    for path in production_sources() {
        if is_test_only_module(&path) {
            continue;
        }
        let source = production_source(&std::fs::read_to_string(&path).expect("source"));
        let mut rest = source.as_str();
        while let Some(at) = rest.find(&needle) {
            rest = &rest[at + needle.len()..];
            if let Some(end) = rest.find('"') {
                found.push(rest[..end].to_owned());
            }
        }
        // The gating path writes the same contract as a JSON literal.
        let json = ["\"reasonCode\"", ": \""].concat();
        let mut rest = source.as_str();
        while let Some(at) = rest.find(&json) {
            rest = &rest[at + json.len()..];
            if let Some(end) = rest.find('"') {
                found.push(rest[..end].to_owned());
            }
        }
    }
    found.sort();
    found.dedup();
    assert!(!found.is_empty(), "the scan found no reason codes at all — it stopped discriminating");
    let unfrozen: Vec<&String> =
        found.iter().filter(|code| !FROZEN.contains(&code.as_str())).collect();
    assert!(
        unfrozen.is_empty(),
        "N2 freezes the client-visible reason codes; these are new: {unfrozen:#?}",
    );
}

/// What the classification is for: a request must never hold a wait that only the daemon's
/// stop can release. A request is called off by its own cancellation, and a request parked on
/// a background stop outlives the client that asked for it.
#[test]
fn no_request_path_holds_an_owner_wait() {
    for class in [Waiting::Owner, Waiting::Bounded, Waiting::Request, Waiting::OwnProtocol] {
        assert!(
            WAITS.iter().any(|(_, _, _, actual)| *actual == class),
            "{class:?} is classified nowhere; the column is not being read",
        );
    }
    for (file, function, token, class) in WAITS {
        // `acquire.rs` is where both acquires are defined, so it holds one of each by design.
        let request_path =
            (*file == "lib.rs" || file.starts_with("tools/")) && *file != "tools/search/acquire.rs";
        assert!(
            !(request_path && *class == Waiting::Owner),
            "{file}::{function} waits on {token} as a background owner, in a request path",
        );
    }
}
