use bsl_platform::PlatformDataInner;

pub fn is_global_function(name: &str, english_name: &str) -> bool {
    let platform = PlatformDataInner::instance();
    platform
        .get_global_function(name)
        .is_some_and(|f| f.english_name.eq_ignore_ascii_case(english_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_is_global_function_bilingual() {
        assert!(is_global_function("НачатьТранзакцию", "BeginTransaction"));
        assert!(is_global_function("BeginTransaction", "BeginTransaction"));
        assert!(is_global_function("НАЧАТЬТРАНЗАКЦИЮ", "BeginTransaction"));
        assert!(is_global_function("begintransaction", "BeginTransaction"));
        assert!(!is_global_function("SomeOtherMethod", "BeginTransaction"));
        assert!(!is_global_function("НачатьТранзакцию", "CommitTransaction"));
    }
}
