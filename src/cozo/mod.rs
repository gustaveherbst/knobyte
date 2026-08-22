//! CozoDB storage engine integrating Datalog relations,
//! graph algorithms, and HNSW vector similarity search.

pub mod embedding;
pub mod engine;
pub mod model2vec;
pub mod schema;
pub mod sled_store;

pub use embedding::{
    cosine_similarity, CodeSymbolText, Embedder, HashedEmbedder, EMBEDDING_DIM, HASHED_EMBEDDER_ID,
};
pub use engine::{
    CozoEngine, PageRankResult, PathStep, VectorMatch, VectorSearchOptions, VectorSearchOutcome,
    MAX_RELEVANCE_DISTANCE, MIN_RELEVANCE_SCORE,
};
pub use model2vec::{
    embedder_from_config, embedding_status, model_dir, model_present, pull_model, EmbeddingStatus,
    Model2VecEmbedder, PullReport,
};
