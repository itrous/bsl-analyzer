//! The one wording for "this document is excluded from the project".
//!
//! Three entry points can be asked about a document the project excludes: the frozen snapshot
//! (asynchronous requests), `GlobalStateSnapshot` (synchronous requests such as formatting) and
//! `Workspace`. They used to answer with two different texts, so one excluded document was
//! explained one way under hover and another way under formatting. The text lives here so that
//! it cannot drift apart again, and it names BOTH configuration keys, because either one can be
//! the reason and the reader cannot tell which from the message alone.

use lsp_types::Url;

/// The error returned for a document excluded by `[source].exclude` or `[source].exclude_globs`.
pub fn excluded_from_project(url: &Url) -> anyhow::Error {
    anyhow::anyhow!(
        "File is excluded from the project by [source].exclude or [source].exclude_globs, \
         so it is not analysed: {}",
        url
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_names_both_configuration_keys_and_the_document() {
        let url = Url::parse("file:///c:/p/CommonModules/M/Ext/Module.bsl").expect("a url");
        let message = excluded_from_project(&url).to_string();
        assert!(message.contains("[source].exclude or [source].exclude_globs"), "{message}");
        assert!(message.contains("so it is not analysed"), "{message}");
        assert!(message.contains(url.as_str()), "{message}");
    }
}
