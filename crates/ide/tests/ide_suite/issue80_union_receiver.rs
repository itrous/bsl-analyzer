use hir::{HirDatabase, InferenceDiagnostic, TypeId};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }

    let test_file = fixture
        .files
        .iter()
        .find(|(_, file)| file.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(file_id, _)| *file_id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(TypeId, TypeId)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => {
                Some((*expected, *actual))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn type_mismatch_silent_on_union_receiver_when_one_arm_accepts() {
    // `Знач` is `Массив | Структура` at the call site. `Структура.Вставить`
    // accepts a String key even though `Массив.Вставить` wants a numeric index.
    // A union receiver is an over-approximation (at most one arm is the runtime
    // type), so an argument accepted by ANY arm must not fire.
    let fixture = r#"
//- /test.bsl
Процедура Тест(Условие)
    Если Условие Тогда
        Знач = Новый Массив;
    Иначе
        Знач = Новый Структура;
    КонецЕсли;
    Знач.Вставить("Ключ", 1);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        mismatches(&db, file_id).is_empty(),
        "union receiver Массив | Структура: a String key is valid via Структура.Вставить, \
         got {:?}",
        mismatches(&db, file_id)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn issue80_union_receiver_rejects_cross_product_arguments() {
    let fixture = r#"
//- /CommonModules/ФабрикаКриптографии/Ext/Module.bsl
// Возвращаемое значение:
//   СертификатКриптографии, КонтейнерКлючейКриптографии
Функция Получить() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Приемник = ФабрикаКриптографии.Получить();
    Приемник.Выгрузить("Первый", "Второй");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let inference = db.infer(file_id);
    let candidate = inference
        .call_arg_bindings
        .iter()
        .map(|binding| &binding.candidate)
        .find(|candidate| {
            candidate.candidates.as_slice().iter().any(|signature| {
                matches!(
                    format!("{:?}", signature.id).as_str(),
                    "Platform { method_id: 4008, signature: Base }"
                        | "Platform { method_id: 4034, signature: Base }"
                )
            })
        })
        .unwrap_or_else(|| {
            panic!(
                "union receiver call must attach candidate semantics: bindings={:?}, vars={:?}",
                inference.call_arg_bindings, inference.var_types,
            )
        });
    let candidate_ids = candidate
        .candidates
        .as_slice()
        .iter()
        .map(|signature| format!("{:?}", signature.id))
        .collect::<Vec<_>>();

    assert_eq!(
        candidate_ids,
        vec![
            "Platform { method_id: 4008, signature: Base }",
            "Platform { method_id: 4008, signature: Variant(0) }",
            "Platform { method_id: 4008, signature: Variant(1) }",
            "Platform { method_id: 4008, signature: Variant(2) }",
            "Platform { method_id: 4034, signature: Base }",
            "Platform { method_id: 4034, signature: Variant(0) }",
            "Platform { method_id: 4034, signature: Variant(1) }",
            "Platform { method_id: 4034, signature: Variant(2) }",
        ]
    );
    assert!(
        format!("{:?}", candidate.resolution.selection).starts_with("Rejected("),
        "mixed arguments must not synthesize a cross-product signature: {:?}",
        candidate.resolution.selection,
    );
}
