//! Process-wide interner for case-insensitive BSL identifier identity.
//!
//! BSL identifiers are case-insensitive, so every pipeline stage that keys a
//! map by identifier has to case-fold the spelling first. Folding is a
//! per-char UTF-8 walk and was the top self-time hotspot of a cold workspace
//! analyze when repeated per occurrence per stage. Interning inverts the
//! cost: each *distinct spelling* is folded exactly once, and every later
//! occurrence pays one hash lookup returning a `Copy` id.
//!
//! [`NormName`] equality is exactly the [`stdx::case::eq_ignore_case`]
//! relation on the source spellings (the per-char fold, not the contextual
//! `str::to_lowercase` one — see [`stdx::case::fold_lower_per_char`]), which
//! makes it the correct key for identifier match buckets. Strings whose
//! folded form is displayed to users or persisted must keep using
//! `fold_lower`; those are different semantics, not a missed optimisation.

use std::sync::LazyLock;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use rustc_hash::FxBuildHasher;
use smol_str::SmolStr;

/// Interned case-folded identifier spelling.
///
/// `NormName::intern(a) == NormName::intern(b)` iff
/// `stdx::case::eq_ignore_case(a, b)`. Equality and hashing are integer
/// operations on the id.
///
/// Deliberately neither `Ord` nor `PartialOrd`: ids are assigned in intern
/// order, which depends on process history, so sorting by id would make any
/// output built from it nondeterministic across runs. Sort by
/// [`NormName::as_str`] where a stable order is needed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NormName(u32);

impl NormName {
    /// Interns `raw` in the global pool, folding it if this spelling has not
    /// been seen before.
    pub fn intern(raw: &str) -> NormName {
        global().intern(raw)
    }

    /// The folded spelling. Never use this for user-visible text — the fold
    /// discards the case the author wrote.
    pub fn as_str(self) -> &'static str {
        global()
            .norm_strs
            .get(self.0 as usize)
            .expect("NormName id not issued by the global pool")
            .as_str()
    }
}

impl std::fmt::Debug for NormName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The folded string, not the id: ids depend on process history and
        // would make logs and test snapshots nondeterministic.
        write!(f, "NormName({})", self.as_str())
    }
}

impl std::fmt::Display for NormName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Live size of the global pool for the process memory report. The pool is
/// not a Salsa ingredient, so `memory_report` has to account for it with a
/// dedicated row; without one it hides in the untracked RSS remainder.
#[derive(Debug, Clone, Copy)]
pub struct PoolStats {
    /// Distinct folded spellings (== issued ids).
    pub norm_count: usize,
    /// Raw spellings currently held by the accelerator cache.
    pub raw_count: usize,
    /// Approximate heap bytes owned by both layers.
    pub heap_bytes: usize,
}

pub fn pool_stats() -> PoolStats {
    global().stats()
}

/// Drops the raw-spelling accelerator cache (e.g. from an idle memory trim).
/// Ids stay valid: only the canonical folded layer is append-only.
pub fn trim_raw_cache() {
    let pool = global();
    pool.raw_ids.clear();
    pool.raw_ids.shrink_to_fit();
}

/// Accelerator-cache cap: beyond this many distinct raw spellings new ones
/// are folded on every occurrence instead of cached. Bounds the only pool
/// layer that grows with edit churn rather than with the identifier
/// vocabulary (the canonical layer is bounded by distinct folded names).
const RAW_CACHE_CAP: usize = 1 << 20;

struct Pool {
    /// Canonical layer: folded spelling → id. The `entry` lock on this map is
    /// the single linearization point for id assignment; two threads racing
    /// to intern spellings of the same folded form serialize here and
    /// observe one id.
    norm_ids: DashMap<SmolStr, u32, FxBuildHasher>,
    /// id → folded spelling, append-only. Entries never move, so `as_str`
    /// borrows through the `'static` global pool without unsafe code. Each
    /// string is pushed *before* its id is published in `norm_ids`, so any
    /// observable id indexes a written slot.
    norm_strs: boxcar::Vec<SmolStr>,
    /// Accelerator only: raw spelling → id of its folded form, so repeated
    /// occurrences skip the fold. Never a source of truth — trimming or
    /// capping it affects speed, not correctness.
    raw_ids: DashMap<SmolStr, NormName, FxBuildHasher>,
    raw_cache_cap: usize,
}

static POOL: LazyLock<Pool> = LazyLock::new(|| Pool::new(RAW_CACHE_CAP));

fn global() -> &'static Pool {
    LazyLock::force(&POOL)
}

impl Pool {
    fn new(raw_cache_cap: usize) -> Pool {
        Pool {
            norm_ids: DashMap::with_hasher(FxBuildHasher),
            norm_strs: boxcar::Vec::new(),
            raw_ids: DashMap::with_hasher(FxBuildHasher),
            raw_cache_cap,
        }
    }

    fn intern(&self, raw: &str) -> NormName {
        // Hot path: known spelling, one borrowed hash lookup, no allocation.
        if let Some(id) = self.raw_ids.get(raw) {
            return *id;
        }

        let folded = stdx::case::fold_lower_per_char(raw);
        let id = match self.norm_ids.entry(SmolStr::new(&folded)) {
            Entry::Occupied(occupied) => *occupied.get(),
            Entry::Vacant(vacant) => {
                let idx = self.norm_strs.push(vacant.key().clone());
                let id = u32::try_from(idx)
                    .expect("NormName pool overflow: u32::MAX distinct identifiers");
                vacant.insert(id);
                id
            }
        };
        let id = NormName(id);

        if self.raw_ids.len() < self.raw_cache_cap {
            self.raw_ids.insert(SmolStr::new(raw), id);
        }
        id
    }

    fn stats(&self) -> PoolStats {
        use stdx::heap::{map_table_bytes, smol_str_bytes, vec_bytes};

        let norm_count = self.norm_strs.count();
        let raw_count = self.raw_ids.len();

        let mut heap_bytes = map_table_bytes::<SmolStr, u32>(self.norm_ids.len())
            + vec_bytes::<SmolStr>(norm_count)
            + map_table_bytes::<SmolStr, NormName>(raw_count);
        // Spilled folded strings are counted once even though `norm_ids` keys
        // clone them: SmolStr's heap variant is refcounted, so the clone
        // shares the allocation.
        for (_, s) in self.norm_strs.iter() {
            heap_bytes += smol_str_bytes(s.len());
        }
        for entry in self.raw_ids.iter() {
            heap_bytes += smol_str_bytes(entry.key().len());
        }
        PoolStats { norm_count, raw_count, heap_bytes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_matches_eq_ignore_case() {
        // Positive and negative pairs, including the contextual-Unicode ones
        // where `fold_lower` would disagree: the interned identity must be
        // exactly the `eq_ignore_case` relation.
        let cases = [
            ("Процедура", "ПРОЦЕДУРА"),
            ("Процедура", "процедура"),
            ("Procedure", "PROCEDURE"),
            ("Ёлка", "ёлка"),
            ("Таблица1Row", "таблица1row"),
            ("Ёлка", "Елка"),
            ("Процедура", "Процедуры"),
            ("ΟΔΟΣ", "οδοσ"),
            ("ΟΔΟΣ", "οδος"),
            ("İ", "i\u{307}"),
            ("İ", "i"),
            ("", ""),
            ("", "a"),
        ];
        for (a, b) in cases {
            assert_eq!(
                NormName::intern(a) == NormName::intern(b),
                stdx::case::eq_ignore_case(a, b),
                "identity mismatch for {a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn as_str_is_the_per_char_fold() {
        for raw in ["ПроЦеДурА", "PROCEDURE", "Ёлка123_Test", "ΟΔΟΣ"] {
            assert_eq!(NormName::intern(raw).as_str(), stdx::case::fold_lower_per_char(raw));
        }
    }

    #[test]
    fn same_id_for_all_spellings_and_reintern() {
        let a = NormName::intern("ОбщегоНазначения");
        let b = NormName::intern("общегоназначения");
        let c = NormName::intern("ОБЩЕГОНАЗНАЧЕНИЯ");
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(a, NormName::intern("ОбщегоНазначения"));
    }

    #[test]
    fn concurrent_spellings_of_one_norm_get_one_id() {
        // Distinct raw spellings of the same folded form interned from many
        // threads at once must resolve to a single id: the vacant-entry
        // protocol, not luck, is what this exercises.
        let pool = std::sync::Arc::new(Pool::new(usize::MAX));
        let base = "ПараллельноеИмяДляГонки";
        let hang_guard = test_utils::wait::hang_guard(std::time::Duration::from_secs(60));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let mut starts = Vec::new();
        let mut handles = Vec::new();
        for i in 0..8 {
            let pool = std::sync::Arc::clone(&pool);
            let ready_tx = ready_tx.clone();
            let result_tx = result_tx.clone();
            let (start_tx, start_rx) = std::sync::mpsc::channel();
            starts.push(start_tx);
            handles.push(std::thread::spawn(move || {
                // Each thread flips a different char to upper-case so
                // every raw spelling is distinct but folds the same.
                let spelling: String = base
                    .chars()
                    .enumerate()
                    .map(|(j, c)| {
                        if j % 8 == i {
                            c.to_uppercase().next().unwrap()
                        } else {
                            c.to_lowercase().next().unwrap()
                        }
                    })
                    .collect();
                ready_tx.send(()).expect("test receives interning worker readiness");
                start_rx
                    .recv_timeout(hang_guard)
                    .expect("test releases concurrent interning workers");
                result_tx.send(pool.intern(&spelling)).expect("test receives interned id");
            }));
        }
        drop(ready_tx);
        drop(result_tx);
        for _ in 0..8 {
            ready_rx.recv_timeout(hang_guard).expect("all interning workers are ready");
        }
        for start in starts {
            start.send(()).expect("interning worker waits for release");
        }
        let ids: Vec<NormName> = (0..8)
            .map(|_| result_rx.recv_timeout(hang_guard).expect("interning worker completes"))
            .collect();
        for handle in handles {
            handle.join().expect("interning worker exits");
        }
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "ids diverged: {:?}", ids.len());
        assert_eq!(pool.stats().norm_count, 1);
    }

    #[test]
    fn raw_cache_cap_bounds_the_accelerator_but_not_identity() {
        let pool = Pool::new(2);
        let first = pool.intern("ИмяОдин");
        pool.intern("ИмяДва");
        // Beyond the cap: still interned correctly, only the raw cache stops
        // growing.
        let third = pool.intern("ИмяТри");
        assert_eq!(pool.intern("ИМЯТРИ"), third);
        assert_eq!(pool.intern("имяодин"), first);
        assert_eq!(pool.stats().norm_count, 3);
        assert!(pool.stats().raw_count <= 2);
    }

    #[test]
    fn trim_raw_cache_keeps_ids_valid() {
        let id = NormName::intern("ПереживёмТрим");
        trim_raw_cache();
        assert_eq!(NormName::intern("переживёмтрим"), id);
        assert_eq!(id.as_str(), "переживёмтрим");
    }

    #[test]
    fn debug_and_display_print_the_folded_string() {
        let id = NormName::intern("ВидимоеИмя");
        assert_eq!(format!("{id}"), "видимоеимя");
        assert_eq!(format!("{id:?}"), "NormName(видимоеимя)");
    }

    #[test]
    fn stats_count_spilled_strings() {
        let pool = Pool::new(usize::MAX);
        let long = "ОченьДлинноеИмяКотороеТочноНеПоместитсяВInline";
        pool.intern(long);
        let stats = pool.stats();
        assert_eq!(stats.norm_count, 1);
        assert!(stats.heap_bytes > long.len());
    }
}
