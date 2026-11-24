use super::*;
use hir::DefDatabase;

#[test]
fn replacement_proof_distinguishes_present_absent_and_undecided_export_guards() {
    for (guard, expected) in [
        ("Сервер", hir::ReplacementApplicability::Applicable),
        ("Клиент", hir::ReplacementApplicability::Unavailable),
        ("НеизвестныйСимвол", hir::ReplacementApplicability::Unknown),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
        configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
        let source = format!("#Если {guard} Тогда\nПроцедура СообщитьПользователю(Текст) Экспорт\nКонецПроцедуры\n#КонецЕсли");
        let library = super::deprecated_replacements::library(
            &root.join("modern"),
            "ОбщегоНазначения",
            &source,
            "<Server>true</Server>",
        );
        let code = "&НаСервере\nПроцедура Проверить()\n    Сообщить(\"text\");\nКонецПроцедуры";
        let modern = external(&root.join("modern-external"), "ModernProcessor", code);
        external(&root.join("legacy-external"), "LegacyProcessor", "");
        let project = project(root);
        let (host, _, ids) = host(&project, &[(modern, code), (library, &source)]);
        let db = host.raw_database();
        let interface = db.module_interface(hir::ModuleId::new(ids[0]));
        let method = interface.find_method(&hir::Name::new("Проверить")).unwrap();
        let actual = hir::replacement_applicability(
            db,
            ids[0],
            hir::DefWithBodyId::Method(method.id.local_id),
            "ОбщегоНазначения.СообщитьПользователю",
            None,
            None,
        );
        assert_eq!(actual, expected, "{guard}");
    }
}
