//! Source code must not embed, at compile time, a file that the public GitHub mirror
//! leaves out (`scripts/github-mirror-exclude.txt`).
//!
//! The internal tree has every excluded path, so such an `include_str!` compiles and
//! passes here and breaks only the mirror, after the sync has already been pushed.
//! Optional material from an excluded path is read at run time and skipped when its
//! directory is absent, as `crates/parser/tests/parser_suite/grammar_attestation.rs` does.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const INCLUDE_MACROS: [&str; 2] = ["include_str", "include_bytes"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn exclusion_patterns(root: &Path) -> Vec<String> {
    let list = root.join("scripts/github-mirror-exclude.txt");
    let text = fs::read_to_string(&list)
        .unwrap_or_else(|err| panic!("{} не прочитан: {err}", list.display()));
    let patterns: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect();
    assert!(!patterns.is_empty(), "список исключений зеркала пуст");
    patterns
}

/// Whether a repo-relative path falls under a git pathspec from the exclusion list:
/// a trailing `/` names a directory, `*` matches any run of characters including `/`.
fn is_excluded(path: &str, pattern: &str) -> bool {
    if let Some(dir) = pattern.strip_suffix('/') {
        return path == dir || path.starts_with(pattern);
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return path == pattern || path.starts_with(&format!("{pattern}/"));
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !path.starts_with(first) || path.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &path[first.len()..];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

#[derive(Debug, PartialEq)]
enum Token {
    Ident(String),
    Punct(char),
    Str(String),
}

/// Rust tokens of `source` with comments and whitespace dropped, just enough lexing for the
/// scan: a macro name inside a comment or a string is not a call, and a bracket inside one
/// does not close the call's arguments. Number and lifetime details are irrelevant here.
fn tokens(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let at = |i: usize| chars.get(i).copied();
    while let Some(c) = at(i) {
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && at(i + 1) == Some('/') {
            while at(i).is_some_and(|c| c != '\n') {
                i += 1;
            }
        } else if c == '/' && at(i + 1) == Some('*') {
            let mut depth = 0;
            loop {
                match (at(i), at(i + 1)) {
                    (Some('/'), Some('*')) => {
                        depth += 1;
                        i += 2;
                    }
                    (Some('*'), Some('/')) => {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    }
                    (Some(_), _) => i += 1,
                    (None, _) => break,
                }
            }
        } else if let Some(hashes) = raw_string_hashes(&chars, i) {
            i += usize::from(c == 'b') + 1 + hashes + 1;
            let mut text = String::new();
            while let Some(c) = at(i) {
                if c == '"' && (1..=hashes).all(|k| at(i + k) == Some('#')) {
                    i += hashes + 1;
                    break;
                }
                text.push(c);
                i += 1;
            }
            out.push(Token::Str(text));
        } else if c == '"' || c == 'b' && at(i + 1) == Some('"') {
            i += usize::from(c == 'b') + 1;
            let mut text = String::new();
            while let Some(c) = at(i) {
                i += 1;
                match c {
                    '"' => break,
                    '\\' if at(i) == Some('\n') => {
                        while at(i).is_some_and(char::is_whitespace) {
                            i += 1;
                        }
                    }
                    '\\' => {
                        if let Some(escaped) = at(i) {
                            text.push(escaped);
                            i += 1;
                        }
                    }
                    other => text.push(other),
                }
            }
            out.push(Token::Str(text));
        } else if c == '\'' {
            // A char literal is closed within a few chars; anything else is a lifetime.
            let close = if at(i + 1) == Some('\\') {
                (i + 3..i + 12).find(|&j| at(j) == Some('\''))
            } else {
                (at(i + 2) == Some('\'')).then_some(i + 2)
            };
            i = close.map_or(i + 1, |close| close + 1);
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while at(i).is_some_and(|c| c.is_alphanumeric() || c == '_') {
                i += 1;
            }
            out.push(Token::Ident(chars[start..i].iter().collect()));
        } else {
            out.push(Token::Punct(c));
            i += 1;
        }
    }
    out
}

/// Number of `#` of a raw string literal starting at `at` (`r"`, `r#"`, `br##"`, ...), or
/// `None` when it is not one: `r#type` is a raw identifier, whose `#` is never followed by
/// a quote after the run of hashes.
fn raw_string_hashes(chars: &[char], at: usize) -> Option<usize> {
    let after_prefix = match (chars.get(at), chars.get(at + 1)) {
        (Some('r'), _) => at + 1,
        (Some('b'), Some('r')) => at + 2,
        _ => return None,
    };
    if at > 0 && chars.get(at - 1).is_some_and(|c| c.is_alphanumeric() || *c == '_') {
        return None;
    }
    let hashes = chars[after_prefix..].iter().take_while(|c| **c == '#').count();
    (chars.get(after_prefix + hashes) == Some(&'"')).then_some(hashes)
}

/// Argument tokens of every include macro call, between its balanced delimiters.
fn include_arguments(tokens: &[Token]) -> Vec<&[Token]> {
    let mut arguments = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let Token::Ident(name) = token else { continue };
        if !INCLUDE_MACROS.contains(&name.as_str()) || tokens.get(i + 1) != Some(&Token::Punct('!'))
        {
            continue;
        }
        let Some(Token::Punct(open @ ('(' | '[' | '{'))) = tokens.get(i + 2) else { continue };
        let close = match open {
            '(' => ')',
            '[' => ']',
            _ => '}',
        };
        let start = i + 3;
        let mut depth = 1;
        let mut end = tokens.len();
        for (offset, token) in tokens[start..].iter().enumerate() {
            match token {
                Token::Punct(c) if c == open => depth += 1,
                Token::Punct(c) if *c == close => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = start + offset;
                break;
            }
        }
        arguments.push(&tokens[start..end]);
    }
    arguments
}

/// Repo-relative path an include argument resolves to, or `None` when it is built from
/// something other than literals (`OUT_DIR`, a macro variable) and cannot be checked here.
fn resolve(argument: &[Token], file: &Path, manifest_dir: &Path, root: &Path) -> Option<String> {
    let mut base = file.parent()?;
    let mut literals = String::new();
    let mut i = 0;
    while i < argument.len() {
        if let [Token::Ident(env), Token::Punct('!'), Token::Punct('('), Token::Str(var), Token::Punct(')'), ..] =
            &argument[i..]
        {
            if env == "env" {
                if var != "CARGO_MANIFEST_DIR" {
                    return None;
                }
                base = manifest_dir;
                i += 5;
                continue;
            }
        }
        if let Token::Str(text) = &argument[i] {
            literals.push_str(text);
        }
        i += 1;
    }
    if literals.is_empty() {
        return None;
    }
    let joined = base.join(literals.trim_start_matches('/'));
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    let relative = normal.strip_prefix(root).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn manifest_dir_of(file: &Path, root: &Path) -> PathBuf {
    let mut dir = file.parent().unwrap();
    while dir != root && !dir.join("Cargo.toml").is_file() {
        dir = dir.parent().unwrap();
    }
    dir.to_path_buf()
}

/// Tracked `.rs` files: the mirror is built from the tracked tree, so this is exactly the
/// set that compiles there, with no guessing about build or vendor directories.
fn tracked_rust_sources(root: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "*.rs"])
        .output()
        .expect("git ls-files не запустился");
    assert!(output.status.success(), "git ls-files: {}", String::from_utf8_lossy(&output.stderr));
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| root.join(String::from_utf8_lossy(path).as_ref()))
        .collect()
}

fn violations(source: &str, file: &Path, root: &Path, patterns: &[String]) -> Vec<String> {
    let manifest_dir = manifest_dir_of(file, root);
    let tokens = tokens(source);
    include_arguments(&tokens)
        .into_iter()
        .filter_map(|argument| resolve(argument, file, &manifest_dir, root))
        .filter_map(|target| {
            let pattern = patterns.iter().find(|p| is_excluded(&target, p))?;
            let file = file.strip_prefix(root).unwrap_or(file).display();
            Some(format!("{file}: встраивает {target} (исключено правилом `{pattern}`)"))
        })
        .collect()
}

#[test]
fn no_source_embeds_a_path_left_out_of_the_mirror() {
    let root = repo_root();
    let patterns = exclusion_patterns(&root);
    let files = tracked_rust_sources(&root);
    assert!(files.len() > 100, "git ls-files нашёл подозрительно мало файлов: {}", files.len());

    let found: Vec<String> = files
        .iter()
        .flat_map(|file| {
            let source = fs::read_to_string(file).unwrap_or_default();
            violations(&source, file, &root, &patterns)
        })
        .collect();
    assert!(
        found.is_empty(),
        "в публичном зеркале этих файлов нет, и сборка там упадёт; читайте их во время \
         выполнения с пропуском при отсутствии каталога:\n{}",
        found.join("\n")
    );
}

/// The scan must see the forms that occur in this repo, otherwise the guard above
/// passes on an empty result whatever the code does.
#[test]
fn scan_flags_every_include_form_that_points_into_an_excluded_path() {
    let root = repo_root();
    let patterns = exclusion_patterns(&root);
    let file = root.join("crates/parser/src/token_inventory_tests.rs");
    let flagged = |source: &str| !violations(source, &file, &root, &patterns).is_empty();

    assert!(flagged(r#"include_str!("../../../docs/legal/bsl-clean-room-slice-b1.md")"#));
    assert!(flagged(r#"include_bytes!("../../../docs/legal/x.bin")"#));
    assert!(flagged(
        r#"include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/legal/x.md"
        ))"#
    ));
    assert!(flagged(r#"include_str!("../../../scripts/sonar-triage-prompt.md")"#));
    assert!(flagged(r#"include_str!("../../../.gitlab-ci.yml")"#));

    assert!(flagged(r#"include_str! ("../../../docs/legal/x.md")"#));
    assert!(flagged("include_str !\n(\n    \"../../../docs/legal/x.md\"\n)"));
    assert!(flagged(r#"include_str!["../../../docs/legal/x.md"]"#));
    assert!(flagged(r#"include_bytes!{"../../../docs/legal/x.md"}"#));
    assert!(flagged(r#"include_str!(/* ) */ "../../../docs/legal/x.md")"#));
    assert!(flagged(r##"include_str!(r#"../../../docs/legal/x.md"#)"##));
    assert!(flagged(r#"std::include_str!("../../../docs/legal/x.md")"#));

    // Whatever comes before the call must not swallow it as a literal.
    assert!(flagged(
        "pub r#type: u8,\nconst X: &str = include_str!(\"../../../docs/legal/x.md\");"
    ));
    assert!(flagged("let q = '\"';\nconst X: &str = include_str!(\"../../../docs/legal/x.md\");"));
    assert!(flagged("let q = '\\'';\nconst X: &str = include_str!(\"../../../docs/legal/x.md\");"));
    assert!(flagged(
        "let q = '\\\\';\nconst X: &str = include_str!(\"../../../docs/legal/x.md\");"
    ));
    assert!(flagged(
        "fn f<'a>(x: &'a str) {}\nconst X: &str = include_str!(\"../../../docs/legal/x.md\");"
    ));

    assert!(!flagged(r#"// include_str!("../../../docs/legal/x.md")"#));
    assert!(!flagged(r#"/// include_str!("../../../docs/legal/x.md")"#));
    assert!(!flagged(r#"/* /* nested */ include_str!("../../../docs/legal/x.md") */"#));
    assert!(!flagged(r#"let s = "include_str!(\"../../../docs/legal/x.md\")";"#));
    assert!(!flagged(r#"my_include_str!("../../../docs/legal/x.md")"#));
    assert!(!flagged(r#"include_str!("fixtures/token_inventory_original_kinds.txt")"#));
    assert!(!flagged(r#"include_str!("../../../docs/legalese.md")"#));
    assert!(!flagged(r#"include_bytes!(concat!(env!("OUT_DIR"), "/extension.zip"))"#));
}
