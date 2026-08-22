//! CozoDB Datalog relations and HNSW vector index definitions.
//!
//! The vector dimension of `code_nodes.embedding` / `wiki_entities.embedding` (and their HNSW
//! indices) depends on the active embedding backend; `embedding_meta` records which embedder
//! built each vector relation.

/// Relations that carry an `embedding` column, with their HNSW index name.
pub const VECTOR_RELATIONS: [(&str, &str); 2] =
    [("code_nodes", "node_vec"), ("wiki_entities", "wiki_vec")];

pub fn create_code_nodes(dim: usize) -> String {
    format!(
        r#"
:create code_nodes {{
    id: String
    =>
    file_path: String,
    kind: String,
    name: String,
    start_line: Int,
    end_line: Int,
    body_hash: String,
    embedding: <F32; {dim}>,
    qualified_name: String default ""
}}
"#
    )
}

pub const CREATE_CODE_EDGES: &str = r#"
:create code_edges {
    source_id: String,
    target_id: String,
    kind: String
    =>
    file_path: String
}
"#;

pub fn create_wiki_entities(dim: usize) -> String {
    format!(
        r#"
:create wiki_entities {{
    id: String
    =>
    title: String,
    path: String,
    tags: [String],
    summary: String,
    embedding: <F32; {dim}>
}}
"#
    )
}

/// Candidate list size while inserting into the HNSW index. Insertion cost grows with it
/// (64 made a cold sync of an 8k-node graph take ~25 s in release builds; 32 takes ~9 s) while
/// recall at the corpus sizes Knobyte indexes stays high (see `tests/graph_cozo_sync_test.rs`).
pub const HNSW_EF_CONSTRUCTION: usize = 32;

/// `::hnsw create` for `relation:index` with the given dimension.
pub fn create_hnsw(relation: &str, index: &str, dim: usize) -> String {
    let ef = HNSW_EF_CONSTRUCTION;
    format!(
        r#"
::hnsw create {relation}:{index} {{
    dim: {dim},
    fields: [embedding],
    distance: Cosine,
    ef_construction: {ef},
    m: 16
}}
"#
    )
}

/// `:create` script for a vector relation.
pub fn create_vector_relation(relation: &str, dim: usize) -> String {
    match relation {
        "wiki_entities" => create_wiki_entities(dim),
        _ => create_code_nodes(dim),
    }
}

/// Which embedder (id + dimension) built the vectors of each vector relation.
pub const CREATE_EMBEDDING_META: &str = r#"
:create embedding_meta {
    relation: String
    =>
    embedder_id: String,
    dim: Int
}
"#;
