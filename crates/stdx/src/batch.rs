//! Cutting a file list into batches for a streaming pass whose peak memory is one
//! batch's working set.
//!
//! A count-only batch is the wrong unit for that peak: source modules differ in
//! size by four orders of magnitude, so a run of giant modules fills a 500-file
//! batch with a hundred times the bytes of a run of small ones, and the batch's
//! syntax trees, lowered bodies and inference scale with the bytes, not the count.
//! Bounding both keeps the small-file batches as large as before and splits the
//! heavy runs.

/// The two caps one batch is cut by. A batch closes at `max_files` items or once
/// adding the next item would take its weight past `max_bytes`, whichever comes
/// first; a single item always forms a batch on its own, however heavy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchBudget {
    pub max_files: usize,
    pub max_bytes: u64,
}

impl BatchBudget {
    /// Count-only batches of at most `max_files` items: the shape `slice::chunks`
    /// gives, for a caller that has no weights.
    pub const fn files(max_files: usize) -> Self {
        Self { max_files, max_bytes: u64::MAX }
    }

    /// The same count cap with a byte cap beside it.
    pub const fn with_bytes(self, max_bytes: u64) -> Self {
        Self { max_bytes, ..self }
    }
}

/// Split `items` into contiguous batches under `budget`, weighing each item with
/// `weight`. Order is preserved and every item lands in exactly one batch, so a pass
/// over the batches visits the same items in the same order as a pass over `items`.
/// A zero `max_files` is treated as one.
pub fn chunks_by_budget<T>(
    items: &[T],
    weight: impl Fn(&T) -> u64,
    budget: BatchBudget,
) -> Vec<&[T]> {
    let max_files = budget.max_files.max(1);
    let mut batches = Vec::new();
    let mut start = 0;
    let mut bytes: u64 = 0;
    for (i, item) in items.iter().enumerate() {
        let w = weight(item);
        let len = i - start;
        if len > 0 && (len >= max_files || bytes.saturating_add(w) > budget.max_bytes) {
            batches.push(&items[start..i]);
            start = i;
            bytes = 0;
        }
        bytes = bytes.saturating_add(w);
    }
    if start < items.len() {
        batches.push(&items[start..]);
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_only_matches_slice_chunks() {
        let items: Vec<u32> = (0..11).collect();
        let got = chunks_by_budget(&items, |_| 1, BatchBudget::files(4));
        let want: Vec<&[u32]> = items.chunks(4).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn byte_cap_closes_a_batch_before_the_count_cap() {
        // Weights: a run of small items, then two heavy ones, then small again.
        let items = [1u64, 1, 1, 50, 50, 1, 1];
        let got = chunks_by_budget(&items, |w| *w, BatchBudget::files(100).with_bytes(53));
        assert_eq!(got, vec![&items[0..4], &items[4..7]]);
    }

    #[test]
    fn an_item_heavier_than_the_cap_still_forms_a_batch() {
        let items = [10u64, 500, 10];
        let got = chunks_by_budget(&items, |w| *w, BatchBudget::files(100).with_bytes(20));
        assert_eq!(got, vec![&items[0..1], &items[1..2], &items[2..3]]);
    }

    #[test]
    fn every_item_lands_once_in_order() {
        let items: Vec<u64> = (0..97).map(|i| (i * 7919) % 300).collect();
        let got = chunks_by_budget(&items, |w| *w, BatchBudget::files(9).with_bytes(1000));
        let flat: Vec<u64> = got.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(flat, items);
        for batch in &got {
            assert!(batch.len() <= 9);
            assert!(batch.len() == 1 || batch.iter().sum::<u64>() <= 1000);
        }
    }

    #[test]
    fn empty_input_gives_no_batches() {
        let items: [u64; 0] = [];
        assert!(chunks_by_budget(&items, |w| *w, BatchBudget::files(3)).is_empty());
    }

    #[test]
    fn zero_file_cap_is_one() {
        let items = [1u64, 2, 3];
        let got = chunks_by_budget(&items, |w| *w, BatchBudget::files(0));
        assert_eq!(got.len(), 3);
    }
}
