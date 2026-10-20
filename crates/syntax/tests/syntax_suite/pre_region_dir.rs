//! `PreRegionDir` over real lexer output.
//!
//! Region directives are case-insensitive in 1C and the lexer also accepts blanks
//! between `#` and the keyword, so the AST wrappers must classify every spelling
//! the lexer accepts. Only a full parse exercises that chain, hence an integration
//! test rather than a unit test on a hand-built tree.

use syntax::ast::{AstNode, PreRegionDir};

#[track_caller]
fn single(code: &str) -> PreRegionDir {
    let parsed = parser::parse(code);
    let mut dirs =
        parsed.syntax_node().descendants().filter_map(PreRegionDir::cast).collect::<Vec<_>>();
    assert_eq!(dirs.len(), 1, "expected exactly one region directive in {code:?}");
    dirs.pop().unwrap()
}

#[track_caller]
fn first(code: &str) -> PreRegionDir {
    parser::parse(code)
        .syntax_node()
        .descendants()
        .find_map(PreRegionDir::cast)
        .unwrap_or_else(|| panic!("no region directive in {code:?}"))
}

#[test]
fn start_marker_recognized_in_any_case() {
    for code in [
        "#Область Имя",
        "#область Имя",
        "#ОБЛАСТЬ Имя",
        "#ОблАсть Имя",
        "#Region Name",
        "#region Name",
        "#REGION Name",
    ] {
        let dir = single(code);
        assert!(dir.is_start(), "{code}");
        assert!(!dir.is_end(), "{code}");
    }
}

#[test]
fn end_marker_recognized_in_any_case() {
    for code in [
        "#КонецОбласти",
        "#конецобласти",
        "#Конецобласти",
        "#КОнецОбласти",
        "#КОНЕЦОБЛАСТИ",
        "#EndRegion",
        "#endregion",
        "#ENDREGION",
        "#EndRegioN",
    ] {
        let dir = single(code);
        assert!(dir.is_end(), "{code}");
        assert!(!dir.is_start(), "{code}");
    }
}

#[test]
fn blank_after_hash_is_accepted() {
    let start = single("# Область Имя");
    assert!(start.is_start());
    assert_eq!(start.name().as_deref(), Some("Имя"));

    let end = single("#\tКонецОбласти");
    assert!(end.is_end());
}

#[test]
fn name_is_read_regardless_of_directive_case() {
    for code in ["#Область Имя", "#ОБЛАСТЬ Имя", "#облАсть Имя"] {
        assert_eq!(single(code).name().as_deref(), Some("Имя"), "{code}");
    }
    for code in ["#Region Name", "#REGION Name"] {
        assert_eq!(single(code).name().as_deref(), Some("Name"), "{code}");
    }
}

#[test]
fn end_marker_has_no_name() {
    for code in ["#КонецОбласти", "#конецобласти", "#EndRegion"] {
        assert_eq!(single(code).name(), None, "{code}");
    }
}

#[test]
fn start_marker_without_name_has_no_name() {
    assert_eq!(single("#Область\n").name(), None);
}

/// A region name sits on the directive's own line. An identifier on the next
/// line belongs to the code, not to the directive.
#[test]
fn name_does_not_reach_the_next_line() {
    assert_eq!(first("#Область\nИмя = 1;\n#КонецОбласти\n").name(), None);
    assert_eq!(first("#Область\n// хвост\nИмя = 1;\n").name(), None);
}

#[test]
fn name_stops_at_trailing_comment() {
    assert_eq!(single("#Область Имя // хвост\n").name().as_deref(), Some("Имя"));
}
