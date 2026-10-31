//! Reading and hashing many files on a pool, largest first, with a cap on the bytes
//! held in memory at once.
//!
//! The content and semantic hashes of a scan are independent per file, so they can
//! run on a pool — but reading every file of a 6 GB metadata tree at once would
//! multiply the scan's resident size by the pool width. The cap keeps the texts in
//! flight (and the DOMs built from them) bounded whatever the pool width is.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// The bytes a parallel scan may hold in flight: enough for a pool to stay busy on
/// typical metadata files, small against the build's own peak. A semantic XML hash
/// holds a DOM of several times its text beside the text itself, and on a large
/// configuration a 256 MiB cap raised the build's peak by over 1 GiB; 128 MiB keeps
/// that growth within the peak the single-file hash already reached.
pub(crate) const SCAN_BYTES_IN_FLIGHT: u64 = 128 * 1024 * 1024;

/// Run `f(index)` for every index of `sizes` on a pool of this call's own and return
/// the results by index. The largest entries go first, each worker pulling the next
/// one from a shared cursor — a greedy longest-first schedule, which a split-and-steal
/// `par_iter` over the sorted list would not give (it hands each worker a contiguous
/// run of the list) — while the entries in flight hold at most `bytes_in_flight`.
pub(crate) fn map_largest_first<R: Send>(
    sizes: &[u64],
    bytes_in_flight: u64,
    f: impl Fn(usize) -> R + Sync,
) -> Vec<R> {
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by(|&a, &b| sizes[b].cmp(&sizes[a]).then(a.cmp(&b)));
    let Ok(pool) = rayon::ThreadPoolBuilder::new().build() else {
        return (0..sizes.len()).map(&f).collect();
    };
    let budget = BytesInFlight::new(bytes_in_flight);
    let next = AtomicUsize::new(0);
    let parts: Vec<Vec<(usize, R)>> = pool.broadcast(|_| {
        let mut done = Vec::new();
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(&index) = order.get(at) else { break };
            done.push((index, budget.with(sizes[index], || f(index))));
        }
        done
    });
    let mut results: Vec<Option<R>> = (0..sizes.len()).map(|_| None).collect();
    for (index, result) in parts.into_iter().flatten() {
        results[index] = Some(result);
    }
    results.into_iter().map(|result| result.expect("every index ran once")).collect()
}

pub(crate) struct BytesInFlight {
    cap: u64,
    held: Mutex<u64>,
    freed: Condvar,
}

impl BytesInFlight {
    pub(crate) fn new(cap: u64) -> Self {
        Self { cap, held: Mutex::new(0), freed: Condvar::new() }
    }

    /// Run `f` once `len` more bytes fit under the cap, holding them for its duration.
    /// A file larger than the whole cap waits for the cap to be empty and then runs
    /// alone, so it is never starved by smaller files slipping in beside it.
    pub(crate) fn with<R>(&self, len: u64, f: impl FnOnce() -> R) -> R {
        {
            let mut held = super::state::lock_recover(&self.held);
            while *held > 0 && held.saturating_add(len) > self.cap {
                held = self.freed.wait(held).unwrap_or_else(|e| e.into_inner());
            }
            *held = held.saturating_add(len);
        }
        let result = f();
        *super::state::lock_recover(&self.held) -= len;
        self.freed.notify_all();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    #[test]
    fn concurrent_holders_never_exceed_the_cap() {
        let budget = Arc::new(BytesInFlight::new(100));
        let held = Arc::new(AtomicU64::new(0));
        let peak = Arc::new(AtomicU64::new(0));
        let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build().unwrap();
        let sizes: Vec<u64> = (0..200).map(|i| 1 + (i * 37) % 60).collect();
        pool.install(|| {
            use rayon::prelude::*;
            sizes.par_iter().for_each(|&len| {
                budget.with(len, || {
                    let now = held.fetch_add(len, Ordering::SeqCst) + len;
                    peak.fetch_max(now, Ordering::SeqCst);
                    held.fetch_sub(len, Ordering::SeqCst);
                });
            });
        });
        assert!(peak.load(Ordering::SeqCst) <= 100, "peak {}", peak.load(Ordering::SeqCst));
    }

    #[test]
    fn a_file_larger_than_the_cap_runs_alone() {
        let budget = Arc::new(BytesInFlight::new(10));
        let held = Arc::new(AtomicU64::new(0));
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        let sizes: Vec<u64> = vec![3, 50, 3, 3, 60, 2, 2];
        pool.install(|| {
            use rayon::prelude::*;
            sizes.par_iter().for_each(|&len| {
                budget.with(len, || {
                    let now = held.fetch_add(len, Ordering::SeqCst) + len;
                    if len > 10 {
                        assert_eq!(now, len, "an oversized file holds the cap alone");
                    } else {
                        assert!(now <= 10);
                    }
                    held.fetch_sub(len, Ordering::SeqCst);
                });
            });
        });
    }

    #[test]
    fn results_come_back_by_index_whatever_the_schedule() {
        let sizes: Vec<u64> = (0..300).map(|i| (i * 7919) % 1000).collect();
        let results = map_largest_first(&sizes, 64, |index| index * 3);
        assert_eq!(results, (0..300).map(|i| i * 3).collect::<Vec<_>>());
    }
}
