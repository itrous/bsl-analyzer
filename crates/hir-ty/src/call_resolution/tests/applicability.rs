#[test]
fn applicability() {
    use super::{
        evaluate_applicability, ArgumentApplicability, ArgumentIndeterminateReason,
        ArityUsage, CandidateApplicability, CandidateEvaluation,
    };
    use support::{positional, signature, variadic, AritySpec};

    let db = InMemoryDb::new();
    let number = db.number(None, None);
    let string = db.string(None, false);
    let typed_array = db.array(Some(number));
    let array = db.array(None);
    let unknown = db.unknown();
    let any = db.any();
    let description_name = "ОбъектМетаданных: Справочник".to_string();
    let description = db.platform_object(description_name.clone());
    let goods_description = db.described_platform_object(
        description_name,
        bsl_types::facet::DescribedObject::new(
            bsl_metadata::MdoType::Catalog,
            "Товары".to_string(),
            bsl_types::facet::DescribedPart::Description,
        ),
    );
    let cases = [
        (
            "exact",
            signature(&db, vec![positional("value", number, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![number],
            vec![ArgumentApplicability::Exact],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "exact through the described object",
            signature(&db, vec![positional("value", description, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![goods_description],
            vec![ArgumentApplicability::Exact],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "assignable",
            signature(&db, vec![positional("value", array, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![typed_array],
            vec![ArgumentApplicability::Assignable],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "argument coercion",
            signature(&db, vec![positional("value", string, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![number],
            vec![ArgumentApplicability::ArgumentCoercion],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "unknown argument",
            signature(&db, vec![positional("value", number, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![unknown],
            vec![ArgumentApplicability::Indeterminate(
                ArgumentIndeterminateReason::UnknownArgument,
            )],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "unknown parameter",
            signature(&db, vec![positional("value", unknown, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![number],
            vec![ArgumentApplicability::Indeterminate(
                ArgumentIndeterminateReason::UnknownParameter,
            )],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "unknown on both sides",
            signature(&db, vec![positional("value", unknown, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![unknown],
            vec![ArgumentApplicability::Indeterminate(
                ArgumentIndeterminateReason::UnknownArgumentAndParameter,
            )],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "Any preserves assignability",
            signature(&db, vec![positional("value", any, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![number],
            vec![ArgumentApplicability::Assignable],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "Any argument preserves assignability",
            signature(&db, vec![positional("value", number, false)], AritySpec {
                required: 1,
                maximum: Some(1),
            }),
            vec![any],
            vec![ArgumentApplicability::Assignable],
            ArityUsage { actual: 1, defaults_used: 0, variadic_arguments: 0 },
        ),
        (
            "optional default",
            signature(&db, vec![
                positional("required", number, false),
                positional("optional", string, true),
            ], AritySpec {
                required: 1,
                maximum: Some(2),
            }),
            vec![number],
            vec![ArgumentApplicability::Exact],
            ArityUsage { actual: 1, defaults_used: 1, variadic_arguments: 0 },
        ),
        (
            "unknown variadic element",
            signature(&db, vec![
                positional("required", number, false),
                variadic("rest", unknown),
            ], AritySpec {
                required: 1,
                maximum: None,
            }),
            vec![number, string, string],
            vec![
                ArgumentApplicability::Exact,
                ArgumentApplicability::Indeterminate(
                    ArgumentIndeterminateReason::UnknownParameter,
                ),
                ArgumentApplicability::Indeterminate(
                    ArgumentIndeterminateReason::UnknownParameter,
                ),
            ],
            ArityUsage { actual: 3, defaults_used: 0, variadic_arguments: 2 },
        ),
    ];

    // Given a table covering each argument relation and both optional and variadic arity.
    for (name, candidate, arguments, expected, expected_usage) in cases {
        // When the pure evaluator checks one complete candidate signature.
        let evaluation = evaluate_applicability(&db, &candidate, &arguments);

        // Then every argument keeps its distinct typed classification.
        let CandidateEvaluation::ArityCompatible { usage, applicability } = evaluation else {
            panic!("{name}: expected compatible arity");
        };
        assert_eq!(usage, expected_usage, "{name}");
        let expected_indeterminate = expected
            .iter()
            .any(|class| matches!(class, ArgumentApplicability::Indeterminate(_)));
        assert_eq!(
            matches!(&applicability, CandidateApplicability::Indeterminate { .. }),
            expected_indeterminate,
            "{name}"
        );
        let evaluations = match applicability {
            CandidateApplicability::Applicable { arguments }
            | CandidateApplicability::Indeterminate { arguments }
            | CandidateApplicability::Incompatible { arguments } => arguments,
        };
        assert_eq!(
            evaluations
                .iter()
                .map(|argument| argument.applicability)
                .collect::<Vec<_>>(),
            expected,
            "{name}"
        );
    }
}
