//! One rule for placing a watch target, and one place that applies it.
//!
//! `armed` pairs every watch the backend holds with WHERE that target lies, and three
//! separate comparisons read the second half of that pair: whether a desired target is
//! already held, whether the cover the declaration asks for is the cover in force, and
//! whether some recursive watch already reaches a path. The rule is
//! `resolve_as_far_as_it_goes` — the longest ancestor that still resolves, plus the rest as
//! spelled. A whole-path `canonicalize` with a fallback to the raw spelling is the rule it
//! replaced, and the two answer differently for exactly one path: the one whose leaf cannot
//! be resolved. Under a symlinked ancestor the raw spelling and the resolved one are two
//! different paths that no comparison will ever bring together.
//!
//! Nothing observable separates them today, and nothing can. A successful arm means the
//! path resolved a moment earlier, so the two rules agree on every target that is actually
//! watched — and they cannot be made to disagree from a test either, because `notify`'s
//! FSEvents backend refuses outright to arm a path that does not exist (`append_path`
//! checks `path.exists()` first). A producer that quietly reverts to the other rule
//! therefore breaks no test. This is what holds the rule in place instead: every
//! `canonicalize` left in the module is listed with what it is for, and an unlisted one is
//! either a second placement rule — call `resolve_as_far_as_it_goes` — or a site that owes
//! its reason here.
//!
//! Structural, not textual: the source is PARSED, so the word inside a doc comment or a
//! string does not count. Test-only items are skipped; a test may resolve paths however it
//! likes, and the rule is about what the daemon records.

use std::path::{Path, PathBuf};

use syn::visit::Visit;

/// Every whole-path resolution left in the watched file, with what it places.
///
/// A number rather than a line, because lines drift and the count does not.
const WATCHED: &[(&str, usize, &str)] = &[(
    "src/change_hub.rs",
    3,
    "two inside `resolve_as_far_as_it_goes` itself, which IS the rule, and one describing a \
     declared target's fingerprint — the only caller that needs the ERROR and not the path, \
     because a `NotFound` there is what tells an absent target from an undescribable one. \
     Every other place a path becomes a key goes through the rule: the watch targets, the \
     change keyed to a file that is there, the change keyed to one that is gone, and the \
     files a freshly walked subtree hands over",
)];

#[derive(Default)]
struct Resolutions {
    count: usize,
}

impl<'ast> Visit<'ast> for Resolutions {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "canonicalize" {
            self.count += 1;
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if is_test_only(&item.attrs) {
            return;
        }
        syn::visit::visit_item_mod(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if is_test_only(&item.attrs) {
            return;
        }
        syn::visit::visit_item_impl(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if is_test_only(&item.attrs) {
            return;
        }
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if is_test_only(&item.attrs) {
            return;
        }
        syn::visit::visit_impl_item_fn(self, item);
    }
}

/// `#[cfg(test)]`, and every `cfg` that reaches it through an `all`/`any`/`not`: the
/// seams this module builds for its own tests are gated `#[cfg(all(test, unix))]`, and a
/// gate that saw only the bare form would count them as daemon code.
fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr.parse_args::<syn::Meta>().is_ok_and(|meta| mentions_test(&meta))
    })
}

fn mentions_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => list
            .parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            )
            .is_ok_and(|nested| nested.iter().any(mentions_test)),
        syn::Meta::NameValue(_) => false,
    }
}

fn count_in(path: &Path) -> usize {
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", path.display()));
    let file = syn::parse_file(&source)
        .unwrap_or_else(|error| panic!("{} does not parse: {error}", path.display()));
    let mut found = Resolutions::default();
    found.visit_file(&file);
    found.count
}

#[test]
fn a_watch_target_is_placed_by_one_rule() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for (relative, expected, reason) in WATCHED {
        let path = crate_dir.join(relative);
        assert_eq!(
            count_in(&path),
            *expected,
            "{}: a whole path is resolved here {expected} times ({reason}). A new one is \
             either a second rule for placing a watch target — call \
             `resolve_as_far_as_it_goes`, which the armed set's one constructor already \
             does — or a site that belongs in this list with its own reason.",
            relative
        );
    }
}

/// The gate must be able to fail, and it must not fail on the things it promises to skip.
#[test]
fn the_gate_sees_a_second_rule_and_skips_what_is_not_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("copy.rs");
    std::fs::write(
        &copy,
        r#"
        //! A doc comment saying canonicalize must not be counted.
        const NOTE: &str = "canonicalize in a string is not code either";
        fn place(path: &std::path::Path) -> std::path::PathBuf {
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
        }
        #[cfg(all(test, unix))]
        struct Seam;
        #[cfg(all(test, unix))]
        impl Seam {
            fn key(path: &std::path::Path) -> std::path::PathBuf {
                path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
            }
        }
        #[cfg(test)]
        mod tests {
            fn helper(p: &std::path::Path) -> std::path::PathBuf { p.canonicalize().unwrap() }
        }
        "#,
    )
    .unwrap();

    assert_eq!(count_in(&copy), 1, "the gate missed a second rule, or counted test-only seams");
}
