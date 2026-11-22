//! The callback-description constructor facts shared by syntax compatibility and semantic
//! normalization. The compatibility path runs in `hir-def`, before project help is available,
//! so its conservative signature projection must come from the bundled platform facts rather
//! than a second list of constructor names and positional slots.

use std::sync::OnceLock;

use smol_str::SmolStr;
use stdx::case::CaseExt;

use crate::{MethodParam, PlatformSnapshot};

/// The positional parameters that carry callback names and their receiver modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackDescriptionSlots {
    pub handler: usize,
    pub receiver: usize,
    pub error_handler: usize,
    pub error_receiver: usize,
    pub arity: usize,
    pub min_arity: usize,
}

#[derive(Debug)]
struct CallbackDescriptionFacts {
    type_names: [SmolStr; 2],
    slots: CallbackDescriptionSlots,
}

fn facts() -> &'static CallbackDescriptionFacts {
    static FACTS: OnceLock<CallbackDescriptionFacts> = OnceLock::new();
    FACTS.get_or_init(|| {
        let snapshot =
            PlatformSnapshot::from_corpus_json(include_bytes!("../data/platform_facts.json"))
                .expect("bundled platform facts must decode");
        let callback_type = snapshot
            .types
            .iter()
            .find(|ty| ty.english_name.eq_ignore_ascii_case("CallbackDescription"))
            .expect("bundled platform facts must contain CallbackDescription");
        let constructor = snapshot
            .constructors
            .iter()
            .find(|constructor| constructor.type_name.eq_ignore_ascii_case("CallbackDescription"))
            .expect("bundled platform facts must contain its constructor");
        let slots = callback_description_slots_for_parameters(&constructor.parameters)
            .expect("bundled CallbackDescription constructor must expose callback slots");
        CallbackDescriptionFacts {
            type_names: [callback_type.name.clone(), callback_type.english_name.clone()],
            slots,
        }
    })
}

/// Whether `name` is one of the source spellings whose compatibility behavior was present
/// before semantic platform normalization: the Russian fact name or its legacy English alias.
/// The current English spelling is intentionally not included.
pub fn is_callback_description_compatibility_type_name(name: &str) -> bool {
    let folded = name.fold_lower();
    let russian_name = &facts().type_names[0];
    russian_name.fold_lower() == folded
        || crate::db::LEGACY_ENGLISH_TYPE_ALIASES.iter().any(|(russian, legacy)| {
            russian.fold_lower() == russian_name.fold_lower() && legacy.fold_lower() == folded
        })
}

/// The callback slots attested by the bundled constructor facts. Used by syntax-only consumers
/// which must retain compatibility when the configured platform-help source is empty.
pub fn bundled_callback_description_slots() -> CallbackDescriptionSlots {
    facts().slots
}

/// Derive callback positions from the parameter names of a platform constructor. This keeps
/// platform-backed normalization and the bundled compatibility projection on one interpretation
/// of the constructor schema.
pub fn callback_description_slots_for_parameters(
    parameters: &[MethodParam],
) -> Option<CallbackDescriptionSlots> {
    if parameters.len() != 5 {
        return None;
    }
    let unique_index = |expected: &str| {
        let expected = expected.fold_lower();
        let mut matches = parameters
            .iter()
            .enumerate()
            .filter(|(_, parameter)| parameter.name.fold_lower() == expected);
        let index = matches.next()?.0;
        matches.next().is_none().then_some(index)
    };

    let handler = unique_index("ИмяПроцедуры")?;
    let receiver = unique_index("Модуль")?;
    let extra_parameters = unique_index("ДополнительныеПараметры")?;
    let error_handler = unique_index("ИмяПроцедурыОбработкиОшибки")?;
    let error_receiver = unique_index("МодульОбработкиОшибки")?;
    let min_arity = parameters
        .iter()
        .enumerate()
        .filter(|(_, parameter)| !parameter.is_optional && !parameter.is_variadic)
        .map(|(index, _)| index + 1)
        .max()
        .unwrap_or(0);

    (extra_parameters != handler
        && extra_parameters != receiver
        && extra_parameters != error_handler
        && extra_parameters != error_receiver)
        .then_some(CallbackDescriptionSlots {
            handler,
            receiver,
            error_handler,
            error_receiver,
            arity: parameters.len(),
            min_arity,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_callback_description_slots_are_projected_from_platform_facts() {
        let slots = bundled_callback_description_slots();
        assert_eq!(
            (slots.handler, slots.receiver, slots.error_handler, slots.error_receiver),
            (0, 1, 3, 4)
        );
        assert!(is_callback_description_compatibility_type_name("ОписаниеОповещения"));
        assert!(!is_callback_description_compatibility_type_name("CallbackDescription"));
        assert!(is_callback_description_compatibility_type_name("NotifyDescription"));
    }

    #[test]
    fn constructor_slot_names_are_matched_case_insensitively() {
        let parameters = [
            "имяпроцедуры",
            "модуль",
            "дополнительныепараметры",
            "имяпроцедурыобработкиошибки",
            "модульобработкиошибки",
        ]
        .map(|name| MethodParam {
            name: name.into(),
            param_type: None,
            is_optional: true,
            is_variadic: false,
            passing: None,
            result_follows_argument_text: false,
        });
        let slots = callback_description_slots_for_parameters(&parameters).unwrap();
        assert_eq!(
            (slots.handler, slots.receiver, slots.error_handler, slots.error_receiver),
            (0, 1, 3, 4)
        );
    }
}
