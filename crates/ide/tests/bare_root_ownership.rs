//! Every surface that types a bare name must agree on who owns it.
//!
//! A user symbol holding a manager-collection name means the platform
//! collection is not what the name denotes, and no surface may answer as if it
//! were. The owners are enumerated rather than sampled: fixing one representative
//! and calling the class closed is what let the same defect return through a
//! different owner four times.
//!
//! Each surface asserts the platform answer is present for the unheld controls
//! and absent for every owner. Without the controls an absent answer would
//! equally mean "ownership is respected" and "this surface answers nothing here".

#[path = "bare_root_ownership/support.rs"]
mod support;

use ide::Locale;
use support::{scenario, Owner, ROOT};

/// The chain whose resolution every surface is asked about.
const CHAIN: &str = "Справочники.Справочник1.НайтиПоКоду(\"К\");";

/// The platform manager type that must never surface for a held name.
const PLATFORM_MANAGER: &str = "СправочникМенеджер";

fn signature_names_platform_manager(owner: Owner) -> bool {
    let s = scenario(owner, CHAIN);
    let offset = s.offset_of("\"К\"");
    s.analysis
        .signature_help(s.file_id, offset)
        .map(|help| help.signatures.iter().any(|sig| sig.signature.contains(PLATFORM_MANAGER)))
        .unwrap_or(false)
}

fn hints_label_the_argument(owner: Owner) -> bool {
    let s = scenario(owner, CHAIN);
    let range = s.whole_range();
    s.analysis.inlay_hints(s.file_id, range).iter().any(|hint| hint.label.starts_with("Код"))
}

fn completion_offers_the_metadata_object(owner: Owner) -> bool {
    let s = scenario(owner, "Справочники.");
    let offset = s.offset_of("Справочники.") + "Справочники.".len() as u32;
    s.analysis
        .completions(s.file_id, offset, None, Locale::Ru)
        .iter()
        .any(|item| item.label == "Справочник1")
}

fn hover_names_platform_manager(owner: Owner) -> bool {
    let s = scenario(owner, CHAIN);
    // The chain's own root, not the first spelling of the name in the module:
    // the assignment control spells it earlier, and an assignment target is not
    // the read this test is about.
    let offset = s.offset_of(CHAIN);
    s.analysis
        .hover(s.file_id, offset, Locale::Ru)
        .map(|hover| hover.markup.contains(PLATFORM_MANAGER))
        .unwrap_or(false)
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn signature_help_respects_every_owner() {
    for owner in Owner::UNHELD {
        assert!(
            signature_names_platform_manager(owner),
            "control {owner:?}: an unheld chain must offer the manager method's signature"
        );
    }
    for owner in Owner::HELD {
        assert!(
            !signature_names_platform_manager(owner),
            "{owner:?} holds {ROOT} — signature help must not name {PLATFORM_MANAGER}"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn inlay_hints_respect_every_owner() {
    for owner in Owner::UNHELD {
        assert!(
            hints_label_the_argument(owner),
            "control {owner:?}: an unheld chain must label its argument"
        );
    }
    for owner in Owner::HELD {
        assert!(
            !hints_label_the_argument(owner),
            "{owner:?} holds {ROOT} — the manager method's parameters must not label the call"
        );
    }
}

#[test]
fn completion_respects_every_owner() {
    for owner in Owner::UNHELD {
        assert!(
            completion_offers_the_metadata_object(owner),
            "control {owner:?}: an unheld collection must offer its metadata objects"
        );
    }
    for owner in Owner::HELD {
        assert!(
            !completion_offers_the_metadata_object(owner),
            "{owner:?} holds {ROOT} — completion must not offer the collection's objects"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn hover_respects_every_owner() {
    for owner in Owner::UNHELD {
        assert!(
            hover_names_platform_manager(owner),
            "control {owner:?}: an unheld collection must hover as the platform manager"
        );
    }
    for owner in Owner::HELD {
        assert!(
            !hover_names_platform_manager(owner),
            "{owner:?} holds {ROOT} — hover must not describe it as {PLATFORM_MANAGER}"
        );
    }
}
