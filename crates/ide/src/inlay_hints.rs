use std::collections::HashMap;

use hir::{CandidateCallBinding, Semantics};
use ide_db::RootDatabase;
use stdx::case::CaseExt;
use symbol_info::{build_signature_from_resolution, selected_signature_index, SymbolSignature};
use syntax::{NodeOrToken, SyntaxKind, SyntaxNode, TextRange, TextSize};
use vfs::FileId;

/// A single inlay hint — a label the editor renders inline at `position`,
/// kept free of `lsp_types` so the adapter maps the offset with its encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub position: TextSize,
    pub label: String,
    pub kind: InlayHintKind,
    pub padding_left: bool,
    pub padding_right: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlayHintKind {
    Parameter,
    Type,
}

/// Inlay hints whose position falls inside `range` (the editor's visible span).
///
/// Currently emits parameter-name hints at call arguments. Inferred-type hints
/// for variables are a planned follow-up (they need the receiver-independent
/// per-binding inference surface, not yet exposed to this layer).
pub fn inlay_hints<DB: RootDatabase>(db: &DB, file_id: FileId, range: TextRange) -> Vec<InlayHint> {
    let _span = tracing::info_span!("inlay_hints", ?file_id).entered();

    // One pass for the whole file: asking per position would rescan every body
    // once per argument list. Which callee an argument list belongs to is
    // inference's answer — deriving it from the callee's identifier text
    // labelled arguments of a global whose name the surrounding code had taken.
    // First recorded binding wins, as `call_binding_at` takes its first match:
    // bodies own disjoint text, so a repeated key means one body mapped two
    // expressions onto one range, and the later one must not displace the
    // answer the positional lookup would give.
    let mut bindings: HashMap<TextRange, CandidateCallBinding> = HashMap::new();
    for (range, binding) in Semantics::new(db).call_bindings(file_id) {
        bindings.entry(range).or_insert(binding);
    }
    if bindings.is_empty() {
        return Vec::new();
    }

    let mut hints = Vec::new();
    for node in db.parse_ref(file_id).syntax_node().descendants() {
        if node.kind() != SyntaxKind::ARG_LIST {
            continue;
        }
        if range.intersect(node.text_range()).is_none() {
            continue;
        }
        parameter_hints_for_arg_list(db, &bindings, &node, range, &mut hints);
    }
    hints
}

/// The binding recorded for the call this argument list belongs to.
///
/// Inference keys ordinary calls by the callee expression and the shapes
/// lowered as a qualified path by the whole call, so both are tried — the same
/// pair `Semantics::call_binding_at` matches.
fn binding_for_arg_list<'a>(
    bindings: &'a HashMap<TextRange, CandidateCallBinding>,
    arg_list: &SyntaxNode,
) -> Option<&'a CandidateCallBinding> {
    let call = arg_list.parent()?;
    let callee = match call.kind() {
        SyntaxKind::CALL_EXPR => call.children().next()?,
        SyntaxKind::NEW_EXPR => call.clone(),
        _ => return None,
    };
    bindings.get(&callee.text_range()).or_else(|| bindings.get(&call.text_range()))
}

/// The signature whose parameter names label these arguments: the candidate
/// inference selected, falling back to the first rendered one when the
/// selection maps to no signature — an ambiguous or rejected overload set still
/// names its arguments the same way in the common case, and that fallback is
/// what parameter hints have always shown.
fn selected_signature<'a>(
    binding: &CandidateCallBinding,
    signatures: &'a [SymbolSignature],
) -> Option<&'a SymbolSignature> {
    selected_signature_index(binding, signatures)
        .and_then(|index| signatures.get(index))
        .or_else(|| signatures.first())
}

/// Projects the selected callable's parameter names onto nonempty argument slots.
fn parameter_hints_for_arg_list<DB: RootDatabase>(
    db: &DB,
    bindings: &HashMap<TextRange, CandidateCallBinding>,
    arg_list: &SyntaxNode,
    range: TextRange,
    hints: &mut Vec<InlayHint>,
) {
    if arg_list.children().next().is_none() {
        return;
    }
    let Some(binding) = binding_for_arg_list(bindings, arg_list) else {
        return;
    };
    let Some(signatures) = build_signature_from_resolution(db, binding) else {
        return;
    };
    let Some(signature) = selected_signature(binding, &signatures) else {
        return;
    };

    // Positional slot = number of commas before the argument, matching how the
    // signature's parameters are ordered (empty slots keep the count aligned).
    let mut slot = 0usize;
    for element in arg_list.children_with_tokens() {
        match element {
            NodeOrToken::Token(token) => {
                if token.kind() == SyntaxKind::COMMA {
                    slot += 1;
                }
            }
            NodeOrToken::Node(arg) => {
                if let Some(name) = signature.parameter_name_at(slot) {
                    maybe_push_param_hint(&arg, &name, range, hints);
                }
            }
        }
    }
}

fn maybe_push_param_hint(
    arg: &SyntaxNode,
    param_name: &str,
    range: TextRange,
    hints: &mut Vec<InlayHint>,
) {
    if param_name.is_empty() {
        return;
    }
    let position = arg.text_range().start();
    if !range.contains(position) {
        return;
    }
    // Drop the hint when the argument already spells the parameter — the label
    // would only echo the code, e.g. `Записать(Режим)`. Case-insensitive to
    // match BSL identifier folding.
    if arg.text().to_string().fold_lower() == param_name.fold_lower() {
        return;
    }
    hints.push(InlayHint {
        position,
        label: format!("{param_name}:"),
        kind: InlayHintKind::Parameter,
        padding_left: false,
        padding_right: true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use vfs::{file_set::FileSet, VfsPath};

    fn single_file(source: &str) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::default();
        let file_id = FileId(0);
        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, source);
        (db, file_id)
    }

    fn whole_range(source: &str) -> TextRange {
        TextRange::new(TextSize::from(0), TextSize::from(source.len() as u32))
    }

    fn labels_at(source: &str, hints: &[InlayHint]) -> Vec<(String, String)> {
        hints
            .iter()
            .map(|h| {
                let after = &source[usize::from(h.position)..];
                let word: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
                (h.label.clone(), word)
            })
            .collect()
    }

    const MODULE: &str = r#"
Функция Сложить(Первое, Второе)
    Возврат Первое + Второе;
КонецФункции

Процедура Тест()
    Сложить(10, 20);
КонецПроцедуры
"#;

    #[test]
    fn parameter_hints_label_each_argument() {
        let (db, file_id) = single_file(MODULE);
        let hints = inlay_hints(&db, file_id, whole_range(MODULE));
        assert!(hints.iter().all(|h| h.kind == InlayHintKind::Parameter));
        // Hints attach to the argument literals 10 and 20.
        let seen = labels_at(MODULE, &hints);
        assert!(seen.contains(&("Первое:".to_string(), "10".to_string())), "{seen:?}");
        assert!(seen.contains(&("Второе:".to_string(), "20".to_string())), "{seen:?}");
    }

    #[test]
    fn skips_hint_when_argument_echoes_parameter_name() {
        let source = r#"
Функция Сложить(Первое, Второе)
    Возврат Первое + Второе;
КонецФункции

Процедура Тест()
    Первое = 1;
    Сложить(Первое, 20);
КонецПроцедуры
"#;
        let (db, file_id) = single_file(source);
        let hints = inlay_hints(&db, file_id, whole_range(source));
        assert!(
            hints.iter().all(|h| h.label != "Первое:"),
            "argument spelled like the parameter must not get a hint: {hints:?}"
        );
        assert!(hints.iter().any(|h| h.label == "Второе:"));
    }

    #[test]
    fn no_hints_for_call_without_arguments() {
        let source =
            "Процедура Х()\nКонецПроцедуры\n\nПроцедура Тест()\n    Х();\nКонецПроцедуры\n";
        let (db, file_id) = single_file(source);
        assert!(inlay_hints(&db, file_id, whole_range(source)).is_empty());
    }

    #[test]
    fn hints_are_confined_to_the_requested_range() {
        let (db, file_id) = single_file(MODULE);
        // A range covering only the function declaration, not the call site.
        let decl_only = TextRange::new(
            TextSize::from(0),
            TextSize::from(MODULE.find("Процедура Тест").unwrap() as u32),
        );
        assert!(inlay_hints(&db, file_id, decl_only).is_empty());
    }

    #[test]
    fn parameter_hints_distinct_for_nested_repeated_calls() {
        let source = r#"
Функция Сложить(Первое, Второе)
    Возврат Первое + Второе;
КонецФункции

Процедура Тест()
    Сложить(Сложить(1, 2), 3);
КонецПроцедуры
"#;
        let (db, file_id) = single_file(source);
        let mut hints = inlay_hints(&db, file_id, whole_range(source));
        // Document order proves inner/outer ARG_LIST nodes remain distinct:
        // outer-arg1 hint on the inner call, then inner's two hints, then
        // outer-arg2 — four total, not collapsed or duplicated.
        hints.sort_by_key(|h| h.position);
        let labels = labels_at(source, &hints);
        assert_eq!(
            labels,
            vec![
                ("Первое:".to_string(), "Сложить".to_string()),
                ("Первое:".to_string(), "1".to_string()),
                ("Второе:".to_string(), "2".to_string()),
                ("Второе:".to_string(), "3".to_string()),
            ],
        );
    }

    /// Global aliases share the platform signature and its numbered argument group.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn parameter_hints_expand_global_parameter_series() {
        for name in ["Мин", "Макс", "ПродолжитьВызов", "Min", "Max", "ProceedWithCall"]
        {
            let source = format!("Процедура Тест()\n    {name}(10, 20, 30);\nКонецПроцедуры\n");
            let (db, file_id) = single_file(&source);
            let hints = inlay_hints(&db, file_id, whole_range(&source));
            assert_eq!(
                labels_at(&source, &hints),
                vec![
                    ("Значение1:".to_string(), "10".to_string()),
                    ("Значение2:".to_string(), "20".to_string()),
                    ("Значение3:".to_string(), "30".to_string()),
                ],
                "{name}",
            );
        }
        for name in ["СтрШаблон", "StrTemplate", "стршаблон"] {
            let source = format!(
                "Процедура Тест(Текст1, Текст2, Текст3)\n    Текст = {name}(\n        \"%1, %2, %3\",\n        Текст1,\n        Текст2,\n        Текст3);\nКонецПроцедуры\n"
            );
            let (db, file_id) = single_file(&source);
            let hints = inlay_hints(&db, file_id, whole_range(&source));
            assert_eq!(
                labels_at(&source, &hints),
                vec![
                    ("Шаблон:".to_string(), "".to_string()),
                    ("Значение1:".to_string(), "Текст1".to_string()),
                    ("Значение2:".to_string(), "Текст2".to_string()),
                    ("Значение3:".to_string(), "Текст3".to_string()),
                ],
                "{name}",
            );
        }
    }

    /// A bounded group must stop naming arguments beyond its documented endpoint.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn parameter_hints_respect_bounded_series() {
        let source = "Процедура Тест()\n    СтрШаблон(\"%10\", 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11);\nКонецПроцедуры\n";
        let (db, file_id) = single_file(source);
        let hints = inlay_hints(&db, file_id, whole_range(source));
        let expected: Vec<_> = std::iter::once(("Шаблон:".to_string(), "".to_string()))
            .chain((1..=10).map(|n| (format!("Значение{n}:"), n.to_string())))
            .collect();
        assert_eq!(labels_at(source, &hints), expected);
    }

    /// Empty slots retain their index, and expanded names still suppress redundant hints.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn parameter_series_preserve_empty_slots_and_name_suppression() {
        let source = "Процедура Тест(Значение1, зНАЧЕНИЕ2)\n    СтрШаблон(\"%3\", , , 30);\n    СтрШаблон(\"%1 %2 %3\", Значение1, зНАЧЕНИЕ2, 40);\nКонецПроцедуры\n";
        let (db, file_id) = single_file(source);
        let hints = inlay_hints(&db, file_id, whole_range(source));
        assert_eq!(
            labels_at(source, &hints),
            vec![
                ("Шаблон:".to_string(), "".to_string()),
                ("Значение3:".to_string(), "30".to_string()),
                ("Шаблон:".to_string(), "".to_string()),
                ("Значение3:".to_string(), "40".to_string()),
            ],
        );
        let start = TextSize::from(source.find("30").unwrap() as u32);
        let range = TextRange::new(start, start + TextSize::from(2));
        assert_eq!(
            labels_at(source, &inlay_hints(&db, file_id, range)),
            vec![("Значение3:".to_string(), "30".to_string())],
        );
    }

    /// Constructors use both numbered groups and explicit variadic flags from the reference.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn parameter_hints_expand_constructor_parameter_series() {
        let cases = [
            (
                "Новый Массив(10, 20, 30)",
                vec!["КоличествоЭлементов1:", "КоличествоЭлементов2:", "КоличествоЭлементов3:"],
            ),
            (
                "New Array(10, 20, 30)",
                vec!["КоличествоЭлементов1:", "КоличествоЭлементов2:", "КоличествоЭлементов3:"],
            ),
            (
                "Новый ФорматированнаяСтрока(\"a\", \"b\", \"c\", \"d\")",
                vec!["Содержимое1:", "Содержимое2:", "Содержимое3:", "Содержимое4:"],
            ),
            (
                "Новый Структура(\"Первый, Второй, Третий\", 10, 20, 30)",
                vec!["Ключи:", "Значения:", "Значения:", "Значения:"],
            ),
            (
                "Новый ФиксированнаяСтруктура(\"Первый, Второй\", 10, 20)",
                vec!["Ключ:", "Значения:", "Значения:"],
            ),
            (
                "Новый КлючСтрокиДинамическогоСписка(\"Первый, Второй\", 10, 20)",
                vec!["ПутиКлючевыхПолей:", "Значения:", "Значения:"],
            ),
        ];
        for (call, expected) in cases {
            let source = format!("Процедура Тест()\n    Результат = {call};\nКонецПроцедуры\n");
            let (db, file_id) = single_file(&source);
            let hints = inlay_hints(&db, file_id, whole_range(&source));
            let labels: Vec<_> = hints.iter().map(|hint| hint.label.as_str()).collect();
            assert_eq!(labels, expected, "{call}");
        }
    }

    /// A numbered local parameter is positional unless its callable declares a variadic tail.
    #[test]
    fn numbered_local_parameter_does_not_create_a_series() {
        let source = "Процедура Одно(Значение1)\nКонецПроцедуры\nПроцедура Тест()\n    Одно(10, 20, 30);\nКонецПроцедуры\n";
        let (db, file_id) = single_file(source);
        let hints = inlay_hints(&db, file_id, whole_range(source));
        assert_eq!(labels_at(source, &hints), vec![("Значение1:".to_string(), "10".to_string())]);
    }
}
