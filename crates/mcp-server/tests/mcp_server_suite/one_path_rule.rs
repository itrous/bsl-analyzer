//! One implementation of "is this path inside a declared root, or inside a hole in one".
//!
//! The rule used to live in three independent copies — the watcher's scope, the walk's
//! pruning, and the boot refusal — and they drifted apart twice in a row without any
//! test noticing: each copy is correct on the inputs its own tests use, and the failure
//! is a file that one of them follows while another does not. It now lives once, in
//! `project_model::PathScope`, and this gate is what keeps a fourth copy from appearing.
//!
//! Structural, not textual: the source is PARSED, so the word inside a doc comment or a
//! string does not count, and a copy written with `strip_prefix` counts just as much as
//! one written with `starts_with` — the precedent for that spelling is already in the
//! repository (`bsl-search/src/workspace_roots.rs::starts_at`).
//!
//! Test modules are skipped. A test may compare paths however it likes; the gate is
//! about what the daemon does.

use std::path::{Path, PathBuf};

use syn::visit::Visit;

/// Every path-containment call left in a watched file, with why it is not the rule.
///
/// A number rather than a line, because lines drift and the count does not. A change
/// here means either a fourth copy of the rule — move it into `PathScope` — or a new
/// containment site that genuinely is not about roots and holes, which belongs in this
/// list with its reason.
const WATCHED: &[(&str, usize, &str)] = &[
    ("../project-model/src/workspace_walk.rs", 0, "the walk asks PathScope and nothing else"),
    ("../project-model/src/source_set.rs", 0, "the partitioning pass asks the same scope"),
    ("src/state/bootstrap.rs", 0, "the boot refusal asks PathScope::hole_covering_a_root"),
    (
        "src/change_hub.rs",
        9,
        "three that compute which watch targets COVER which — an operation about the \
         target set, not about holes, and one PathScope deliberately does not offer. The \
         third of them, `an_armed_recursive_watch_reaches`, asks whether the watcher is \
         ALREADY holding a path, which the blind set and the re-watch both need and must \
         not answer differently. It cannot be PathScope: the root's side is the resolution \
         captured at watch time, and re-deriving it would claim coverage a retargeted \
         symlink has lost. The fourth, `an_arm_already_recorded_covers`, is not about \
         coverage at all: it asks whether one REGISTRATION lies inside another, which is \
         what decides whether un-watching the one takes the other with it, and it compares \
         the spellings the backend was given rather than where they lead. The subtree walk \
         computes no relative path any more — it keys each file by its own resolved \
         directory, the rule the workspace walk uses. The fifth, `restore_watches_beneath`, \
         asks which registrations an unwatch took with it, and the sixth asks the same of \
         one record before deciding whether a failed re-arm leaves it naming anything — \
         both about the spellings the backend was given and about nothing else. The \
         seventh sits beside the fourth: a record may be left out only for a path that lies \
         inside another registration BOTH ways — under its spelling, so the unwatch takes \
         it, and inside the tree it reaches, so the arm that follows puts it back. The \
         eighth, in the poll's `take`, asks whether a file the walk did not see lies beneath \
         a path that walk could not read, comparing the walk's own spellings: such a file is \
         carried, not reported removed. The ninth, `an_unwatch_would_strip_a_record`, asks \
         before a dropped record's unwatch whether a staying record is spelled beneath it — \
         the question `restore_watches_beneath` asks after the fact, asked first so the \
         unwatch is skipped instead of taking a live registration with it",
    ),
];

#[derive(Default)]
struct Containments {
    count: usize,
}

impl<'ast> Visit<'ast> for Containments {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "starts_with" || call.method == "strip_prefix" {
            self.count += 1;
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    /// `#[cfg(test)]` items are not the daemon.
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if item.attrs.iter().any(is_cfg_test) {
            return;
        }
        syn::visit::visit_item_mod(self, item);
    }
}

fn is_cfg_test(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("cfg")
        && attr.parse_args::<syn::Meta>().is_ok_and(|meta| meta.path().is_ident("test"))
}

fn count_in(path: &Path) -> usize {
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", path.display()));
    let file = syn::parse_file(&source)
        .unwrap_or_else(|error| panic!("{} does not parse: {error}", path.display()));
    let mut found = Containments::default();
    found.visit_file(&file);
    found.count
}

#[test]
fn the_root_and_hole_rule_has_one_implementation() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for (relative, expected, reason) in WATCHED {
        let path = crate_dir.join(relative);
        assert_eq!(
            count_in(&path),
            *expected,
            "{}: path containment is compared here {expected} times ({reason}). \
             A new one is either a fourth copy of the root-and-hole rule — ask \
             project_model::PathScope instead — or a site that belongs in this list \
             with its own reason.",
            relative
        );
    }
}

/// The gate must be able to fail, and it must fail on the spelling a copy would
/// actually use. A file that plainly contains the rule is counted as containing it.
#[test]
fn the_gate_sees_a_copy_written_either_way() {
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("copy.rs");
    std::fs::write(
        &copy,
        r#"
        //! A doc comment saying starts_with must not be counted.
        const NOTE: &str = "strip_prefix in a string is not code either";
        struct Copy { holes: Vec<std::path::PathBuf> }
        impl Copy {
            fn is_hole(&self, path: &std::path::Path) -> bool {
                self.holes.iter().any(|hole| path.strip_prefix(hole).is_ok())
            }
        }
        #[cfg(test)]
        mod tests {
            fn helper(p: &std::path::Path) -> bool { p.starts_with("/anything") }
        }
        "#,
    )
    .unwrap();

    assert_eq!(count_in(&copy), 1, "the gate missed a copy, or counted comments and tests");
}
