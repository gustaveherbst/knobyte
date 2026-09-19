//! CozoDB neuro-symbolic storage engine integrating Datalog relations,
//! graph algorithms, and HNSW vector similarity search.

pub mod embedding;
pub mod engine;
pub mod schema;

pub use embedding::{Embedder, EMBEDDING_DIM};
pub use engine::{CozoEngine, PageRankResult, VectorMatch};
