use hir::graph_index::{GraphIndex, GraphRowEncoder};
use hir::{ConfigsDatabase, GraphNode, MethodCallDigest, ModuleId};
use rustc_hash::FxHashMap;
use vfs::FileId;

use super::{batch_database, build, fixture, ROOT};

#[test]
fn bounded_builder_method_digest_matches_salsa_fold() {
    // Given: a complete fixture database and the builder's one-module batches.
    let files = fixture();
    let modules = [ModuleId::new(FileId(0)), ModuleId::new(FileId(1)), ModuleId::new(FileId(2))];
    let db = batch_database(&files);
    let graph_index = GraphIndex::build(&db, &modules);
    let paths: FxHashMap<_, _> = files
        .iter()
        .enumerate()
        .map(|(index, file)| (FileId(index as u32), file.path.to_string()))
        .collect();

    // When: the real bounded builder emits its compact reverse index.
    let (built, _, _) = build(1);
    let compact = hir::call_hierarchy_method_digest(&built.index, &graph_index, &paths, None);
    let no_objects = hir::graph_index::MdoFiles::default();
    let encoder = GraphRowEncoder::new(&graph_index, &paths, None, &no_objects);
    let salsa = db.workspace_call_graph(ROOT);
    let folded = MethodCallDigest::from_rows(salsa.edges().filter_map(|edge| {
        let (GraphNode::Method(caller), GraphNode::Method(target)) = (&edge.from, &edge.to) else {
            return None;
        };
        Some((
            encoder.encode(&GraphNode::Method(*target)).0,
            encoder.encode(&GraphNode::Method(*caller)).0,
        ))
    }));

    // Then: durable method pairs exactly match the current Salsa incoming graph.
    assert_eq!(compact, folded);
}
