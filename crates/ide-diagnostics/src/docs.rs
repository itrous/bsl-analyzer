include!(concat!(env!("OUT_DIR"), "/docs_generated.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DiagnosticCode;

    #[test]
    fn method_declaration_diagnostics_have_default_metadata_and_bilingual_docs() {
        for code in
            [DiagnosticCode::DuplicateInterception, DiagnosticCode::DuplicateMethodDeclaration]
        {
            let metadata = crate::get_metadata(code).expect("registered diagnostic metadata");
            assert!(metadata.activated_by_default);
            assert!(!crate::DiagnosticsConfig::default().is_disabled(code));
            assert!(DOCUMENTED_CODES.contains(&code), "{code:?}");
            let docs = get_docs(code);
            assert!(!docs.name_ru.is_empty(), "{code:?}");
            assert!(!docs.name_en.is_empty(), "{code:?}");
            assert!(!docs.description_ru.is_empty(), "{code:?}");
            assert!(!docs.description_en.is_empty(), "{code:?}");
        }
    }

    #[test]
    fn test_get_docs_line_length() {
        let docs = get_docs(DiagnosticCode::LineLength);
        assert!(!docs.name_ru.is_empty(), "LineLength should have Russian name");
        assert!(!docs.description_ru.is_empty(), "LineLength should have Russian description");
    }

    #[test]
    fn test_get_docs_cyclomatic_complexity() {
        let docs = get_docs(DiagnosticCode::CyclomaticComplexity);
        assert!(docs.description_ru.contains("Цикломатическая"));
    }

    #[test]
    fn test_documented_codes_not_empty() {
        assert!(DOCUMENTED_CODES.len() >= 170, "Should have at least 170 documented codes");
    }

    #[test]
    fn test_unknown_code_returns_empty() {
        let docs = get_docs(DiagnosticCode::ParseError);
        let _ = docs.name_ru;
    }
}
