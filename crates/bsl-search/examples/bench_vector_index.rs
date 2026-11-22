//! Cold-start breakdown for the semantic vector index.
//!
//! Measures, on a real `bsl-search.db`, where the per-restart "semantic is building"
//! time goes: reading embeddings out of SQLite vs (re)constructing the in-memory usearch
//! HNSW index — and what persisting that index (save + view/load) would cost instead.
//!
//! Usage: cargo run --release -p bsl-search --example bench_vector_index -- <db_path> [dim]

use std::path::Path;
use std::time::Instant;

use bsl_search::Store;
use usearch::ffi::{IndexOptions, MetricKind, ScalarKind};

fn index_options(dim: usize) -> IndexOptions {
    // Mirrors VectorIndex::new exactly so the build cost is representative.
    IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 16,
        expansion_add: 128,
        expansion_search: 64,
        multi: false,
    }
}

fn main() {
    let db = std::env::args().nth(1).expect("usage: bench_vector_index <db_path> [dim]");
    let dim: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1024);

    let store = Store::open(Path::new(&db)).expect("open store");

    // Phase 1: pull every embedding out of SQLite (what cold start does today).
    let t = Instant::now();
    let data = store.load_all_embeddings(dim).expect("load_all_embeddings");
    let t_load = t.elapsed();
    println!("load_all_embeddings : {:>8.2?}   ({} vectors, dim {})", t_load, data.len(), dim);

    // Phase 2: build the HNSW serially via add() (what cold start does today).
    let t = Instant::now();
    let index = usearch::Index::new(&index_options(dim)).expect("new index");
    index.reserve(data.len()).expect("reserve");
    for (id, emb) in &data {
        index.add(*id as u64, emb).expect("add");
    }
    let t_build = t.elapsed();
    println!("VectorIndex::build  : {:>8.2?}   (serial add, size {})", t_build, index.size());

    // Phase 3: persist the built index (the proposed one-time cost on change).
    let path = stdx::test_temp::root().join("bsl_bench_index.usearch");
    let path_s = path.to_str().unwrap();
    let t = Instant::now();
    index.save(path_s).expect("save");
    let t_save = t.elapsed();
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("usearch save        : {:>8.2?}   (file {} MB)", t_save, bytes / 1024 / 1024);

    // Phase 4: view (mmap) the persisted index — the proposed NEW cold start.
    let t = Instant::now();
    let viewed = usearch::Index::new(&index_options(dim)).expect("new index for view");
    viewed.view(path_s).expect("view");
    let t_view = t.elapsed();
    println!("usearch view (mmap) : {:>8.2?}   (size {})", t_view, viewed.size());

    // Phase 5: load (full read into RAM) — alternative if mmap is undesirable.
    let t = Instant::now();
    let loaded = usearch::Index::new(&index_options(dim)).expect("new index for load");
    loaded.load(path_s).expect("load");
    let t_load2 = t.elapsed();
    println!("usearch load (full) : {:>8.2?}   (size {})", t_load2, loaded.size());

    // Sanity: the viewed index must actually answer a query.
    if let Some((_, first)) = data.first() {
        let hits = viewed.search(first.as_slice(), 5).expect("search viewed");
        println!("sanity search hits  : {} (top key {:?})", hits.keys.len(), hits.keys.first());
    }

    let _ = std::fs::remove_file(&path);

    println!("\n  COLD START TODAY      = load_all + build = {:>8.2?}", t_load + t_build);
    println!("  COLD START PERSISTED  = view             = {:>8.2?}", t_view);
    println!("  (one-time save cost on change            = {:>8.2?})", t_save);
}
