//! CozoDB Datalog relations and HNSW vector index definitions.

pub const CREATE_CODE_NODES: &str = r#"
:create code_nodes {
    id: String
    =>
    file_path: String,
    kind: String,
    name: String,
    start_line: Int,
    end_line: Int,
    body_hash: String,
    embedding: <F32; 128>
}
"#;

pub const CREATE_CODE_EDGES: &str = r#"
:create code_edges {
    source_id: String,
    target_id: String,
    kind: String
    =>
    file_path: String
}
"#;

pub const CREATE_WIKI_ENTITIES: &str = r#"
:create wiki_entities {
    id: String
    =>
    title: String,
    path: String,
    tags: [String],
    summary: String,
    embedding: <F32; 128>
}
"#;

pub const CREATE_CODE_NODES_HNSW: &str = r#"
::hnsw create code_nodes:node_vec {
    dim: 128,
    fields: [embedding],
    distance: Cosine,
    ef_construction: 64,
    m: 16
}
"#;

pub const CREATE_WIKI_ENTITIES_HNSW: &str = r#"
::hnsw create wiki_entities:wiki_vec {
    dim: 128,
    fields: [embedding],
    distance: Cosine,
    ef_construction: 64,
    m: 16
}
"#;
