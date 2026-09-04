//! Search tool execution paths and stable public entrypoints.

mod acquire;
mod call;
#[cfg(test)]
mod cancel_tests;
mod docs;
mod gating;
mod hybrid;
mod lexical;
mod render;
mod semantic;
mod status;
#[cfg(test)]
mod test_support;
mod types;
mod wait;
#[cfg(test)]
pub(crate) use acquire::try_acquire_engine;
pub(crate) use call::search_call;
pub(crate) use types::search_output_schema;
#[cfg(test)]
pub(crate) use types::AcquireFailure;
pub(crate) use wait::{await_reply, Withdrawn, REPLY_POLL};

pub use docs::{find_docs, search_docs};
#[allow(unused_imports)]
pub use hybrid::{hybrid_code, hybrid_code_cancellable};
pub use status::search_status;
pub(crate) use status::{baseline_warming_not_ready, docs_not_ready, search_not_ready};

#[cfg(test)]
mod tests {
    #[test]
    fn all_search_modes_are_resident_only_under_held_lease() {
        super::hybrid::tests::assert_all_search_modes_are_resident_only_under_held_lease();
    }
}
