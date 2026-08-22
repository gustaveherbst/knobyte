//! Embedding backends: hashed default, Model2Vec (with a tiny synthetic offline fixture),
//! dimension switches and missing-model errors. No test touches the network.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use knobyte::config::{EmbeddingBackend, EmbeddingConfig, KnobyteConfig};
use knobyte::cozo::{
    embedder_from_config, embedding_status, CozoEngine, Embedder, HashedEmbedder,
    Model2VecEmbedder, EMBEDDING_DIM, HASHED_EMBEDDER_ID,
};
use knobyte::graph::GraphEngine;
use safetensors::tensor::{Dtype, TensorView};

const FIXTURE_DIM: usize = 16;
const FIXTURE_REPO: &str = "test/tiny-model2vec";

/// Word clusters of the synthetic model: every word of a cluster points (mostly) along the
/// cluster's axis, so synonyms without any lexical overlap end up close together.
const CLUSTERS: &[(usize, &[&str])] = &[
    (
        1,
        &[
            "login",
            "user",
            "password",
            "authenticate",
            "credentials",
            "sign",
            "session",
            "auth",
            "verify",
        ],
    ),
    (2, &["render", "canvas", "draw", "pixels", "paint", "frame"]),
    (
        3,
        &[
            "function", "src", "rs", "fn", "pub", "str", "bool", "check", "the", "a",
        ],
    ),
];

/// Write a minimal Model2Vec model (WordLevel tokenizer + f32 safetensors matrix).
fn write_fixture_model(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let mut vocab = serde_json::Map::new();
    vocab.insert("[UNK]".to_string(), serde_json::json!(0));
    let mut rows: Vec<Vec<f32>> = vec![vec![0.0; FIXTURE_DIM]];
    for (axis, words) in CLUSTERS {
        for (i, w) in words.iter().enumerate() {
            vocab.insert(w.to_string(), serde_json::json!(rows.len()));
            let mut row = vec![0.0f32; FIXTURE_DIM];
            row[*axis] = 1.0;
            // A little per-word variation on the tail dimensions.
            row[4 + (i % (FIXTURE_DIM - 4))] = 0.15;
            rows.push(row);
        }
    }
    let tokenizer = serde_json::json!({
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": [],
        "normalizer": { "type": "Lowercase" },
        "pre_tokenizer": { "type": "Whitespace" },
        "post_processor": null,
        "decoder": null,
        "model": { "type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]" }
    });
    fs::write(dir.join("tokenizer.json"), tokenizer.to_string()).unwrap();
    fs::write(
        dir.join("config.json"),
        serde_json::json!({ "model_type": "model2vec", "hidden_dim": FIXTURE_DIM, "normalize": true })
            .to_string(),
    )
    .unwrap();

    let n_rows = rows.len();
    let bytes: Vec<u8> = rows
        .into_iter()
        .flatten()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    let view = TensorView::new(Dtype::F32, vec![n_rows, FIXTURE_DIM], &bytes).unwrap();
    let mut tensors = BTreeMap::new();
    tensors.insert("embeddings".to_string(), view);
    let data = safetensors::serialize(&tensors, &None).unwrap();
    fs::write(dir.join("model.safetensors"), data).unwrap();
}

fn fixture_embedder(dir: &Path) -> Arc<dyn Embedder> {
    write_fixture_model(dir);
    Arc::new(Model2VecEmbedder::load(dir, FIXTURE_REPO).expect("fixture model loads"))
}

/// A tiny project where the auth function shares no words with the query
/// "authenticate credentials" except through the model's semantics.
fn build_project(root: &Path) -> std::path::PathBuf {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/auth.rs"),
        "pub fn login(user: &str, password: &str) -> bool { check(user, password) }\nfn check(user: &str, password: &str) -> bool { !user.is_empty() && !password.is_empty() }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/canvas.rs"),
        "pub fn render(canvas: &str) { draw(canvas) }\nfn draw(pixels: &str) { paint(pixels) }\nfn paint(frame: &str) {}\n",
    )
    .unwrap();
    let graph_path = root.join("graph.db");
    let mut graph = GraphEngine::open(&graph_path).unwrap();
    graph.rebuild(root).unwrap();
    graph_path
}

fn embedding_column_dim(engine: &CozoEngine, relation: &str) -> String {
    let cols = engine
        .datalog_query(&format!("::columns {}", relation), serde_json::json!({}))
        .unwrap();
    let rows = cols["rows"].as_array().unwrap();
    let row = rows.iter().find(|r| r[0] == "embedding").unwrap();
    row.to_string()
}

#[test]
fn model2vec_fixture_embeds_with_model_dimension() {
    let dir = tempfile::tempdir().unwrap();
    let e = fixture_embedder(&dir.path().join("model"));
    assert_eq!(e.dim(), FIXTURE_DIM);
    assert_eq!(e.id(), format!("model2vec:{}", FIXTURE_REPO));
    let a = e.embed("authenticate credentials");
    let b = e.embed("login password");
    let c = e.embed("draw pixels");
    let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4);
    let sim = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>();
    assert!(sim(&a, &b) > sim(&a, &c) + 0.5);
    // camelCase / snake_case identifiers are split into words before tokenizing.
    let ident = e.embed("verifyUserSession");
    assert!(sim(&ident, &b) > sim(&ident, &c));
}

#[test]
fn hashed_backend_is_default_and_unchanged() {
    let engine = CozoEngine::in_memory().unwrap();
    assert_eq!(engine.embedder().id(), HASHED_EMBEDDER_ID);
    assert_eq!(engine.embedder().dim(), EMBEDDING_DIM);
    assert_eq!(
        engine.stored_space("code_nodes").unwrap(),
        Some((HASHED_EMBEDDER_ID.to_string(), EMBEDDING_DIM))
    );
    assert!(embedding_column_dim(&engine, "code_nodes").contains("128"));
    assert_eq!(
        HashedEmbedder.embed("fn login()"),
        HashedEmbedder::embed_text("fn login()")
    );
}

#[test]
fn switching_backend_recreates_index_and_reembeds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph_path = build_project(root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(
        docs.join("auth.md"),
        "---\nid: kb_auth\n---\n# Sign in flow\nHow users authenticate with credentials.\n",
    )
    .unwrap();
    let wiki_path = root.join("wiki.db");
    let mut wiki = knobyte::wiki::WikiIndex::open(&wiki_path).unwrap();
    wiki.rebuild(&docs).unwrap();
    let cozo_path = root.join("cozo.db");

    // 1. Hashed (128-dim) index.
    {
        let engine = CozoEngine::open(&cozo_path).unwrap();
        let gconn = rusqlite::Connection::open(&graph_path).unwrap();
        engine.sync_from_graph(&gconn).unwrap();
        engine
            .sync_from_wiki(&rusqlite::Connection::open(&wiki_path).unwrap())
            .unwrap();
        assert_eq!(
            engine.stored_space("code_nodes").unwrap().unwrap().1,
            EMBEDDING_DIM
        );
    }

    // 2. Reopen with the Model2Vec fixture: queries refuse to mix spaces until re-synced.
    let model = fixture_embedder(&root.join("model"));
    {
        let engine = CozoEngine::open_with_embedder(&cozo_path, model.clone()).unwrap();
        assert!(engine.needs_reembed().unwrap());
        let err = engine
            .vector_search("authenticate credentials", "code", 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("knobyte cozo sync"), "{}", err);

        let gconn = rusqlite::Connection::open(&graph_path).unwrap();
        let (nodes, _) = engine.sync_from_graph(&gconn).unwrap();
        assert!(nodes > 0);
        engine
            .sync_from_wiki(&rusqlite::Connection::open(&wiki_path).unwrap())
            .unwrap();
        assert!(!engine.needs_reembed().unwrap());
        assert_eq!(
            engine.stored_space("code_nodes").unwrap(),
            Some((model.id(), FIXTURE_DIM))
        );
        assert_eq!(
            engine.stored_space("wiki_entities").unwrap(),
            Some((model.id(), FIXTURE_DIM))
        );
        assert!(embedding_column_dim(&engine, "code_nodes").contains(&FIXTURE_DIM.to_string()));

        // Every node was re-embedded (all rows present again).
        let count = engine
            .datalog_query("?[count(id)] := *code_nodes{id}", serde_json::json!({}))
            .unwrap();
        assert_eq!(count["rows"][0][0].as_u64().unwrap() as usize, nodes);

        // 3. Semantic ranking: no lexical overlap between query and `login`.
        let matches = engine
            .vector_search("authenticate credentials", "code", 3)
            .unwrap();
        assert!(!matches.is_empty());
        assert_eq!(
            matches[0].metadata["name"], "login",
            "expected login first, got {:?}",
            matches
        );
        assert_eq!(
            matches[0].metadata["ref"], "function:src/auth.rs:login",
            "{:?}",
            matches[0].metadata
        );
        let wiki_matches = engine.vector_search("login session", "wiki", 3).unwrap();
        assert_eq!(wiki_matches[0].id, "kb_auth");
    }

    // 4. Back to hashed: recreated at 128 dims on the next sync.
    {
        let engine = CozoEngine::open(&cozo_path).unwrap();
        assert!(engine.needs_reembed().unwrap());
        engine
            .sync_from_graph(&rusqlite::Connection::open(&graph_path).unwrap())
            .unwrap();
        assert_eq!(
            engine.stored_space("code_nodes").unwrap(),
            Some((HASHED_EMBEDDER_ID.to_string(), EMBEDDING_DIM))
        );
        assert!(engine.vector_search("login", "code", 3).is_ok());
    }
}

#[test]
fn legacy_db_without_metadata_is_treated_as_hashed() {
    let dir = tempfile::tempdir().unwrap();
    let cozo_path = dir.path().join("cozo.db");
    {
        let engine = CozoEngine::open(&cozo_path).unwrap();
        engine
            .datalog_query_mutable("::remove embedding_meta", serde_json::json!({}))
            .unwrap();
    }
    let engine = CozoEngine::open(&cozo_path).unwrap();
    assert_eq!(
        engine.stored_space("code_nodes").unwrap(),
        Some((HASHED_EMBEDDER_ID.to_string(), EMBEDDING_DIM))
    );
    assert!(!engine.needs_reembed().unwrap());
}

#[test]
fn config_defaults_to_hashed_and_missing_model_is_a_helpful_error() {
    // Old config without an `embedding` section.
    let dir = tempfile::tempdir().unwrap();
    let scaffold = dir.path().join(".knobyte");
    fs::create_dir_all(&scaffold).unwrap();
    fs::write(
        scaffold.join("config.json"),
        r#"{"version":"0.9.0","scaffold_id":"abc","mode":"code-repo","project_name":"demo"}"#,
    )
    .unwrap();
    let mut config = KnobyteConfig::new(dir.path().to_path_buf(), scaffold.clone());
    assert_eq!(config.embedding.backend, EmbeddingBackend::Hashed);
    assert_eq!(config.embedding, EmbeddingConfig::default());
    assert_eq!(
        embedder_from_config(&config.embedding).unwrap().id(),
        HASHED_EMBEDDER_ID
    );

    // This is the only test that touches KNOBYTE_MODELS_DIR.
    let models = dir.path().join("models");
    std::env::set_var("KNOBYTE_MODELS_DIR", &models);

    let m2v = EmbeddingConfig {
        backend: EmbeddingBackend::Model2vec,
        model: Some("minishlab/potion-base-8M".to_string()),
    };
    let err = embedder_from_config(&m2v)
        .err()
        .expect("missing model must fail");
    assert!(err.contains("knobyte cozo model pull"), "{}", err);
    assert!(err.contains("minishlab/potion-base-8M"), "{}", err);
    let status = embedding_status(&m2v);
    assert!(!status.model_present);
    assert_eq!(status.dim, None);

    // Opening the project's Cozo with a missing model fails instead of falling back.
    config.embedding = m2v.clone();
    let open_err = CozoEngine::open_configured(&config)
        .err()
        .unwrap()
        .to_string();
    assert!(open_err.contains("not downloaded"), "{}", open_err);

    // Once the model files exist (`pull` layout: <root>/<owner>--<name>), it loads.
    let local = EmbeddingConfig {
        backend: EmbeddingBackend::Model2vec,
        model: Some(FIXTURE_REPO.to_string()),
    };
    write_fixture_model(&models.join("test--tiny-model2vec"));
    let e = embedder_from_config(&local).unwrap();
    assert_eq!(e.dim(), FIXTURE_DIM);
    let status = embedding_status(&local);
    assert!(status.model_present);
    assert_eq!(status.dim, Some(FIXTURE_DIM));

    // Path traversal in model ids is rejected.
    let bad = EmbeddingConfig {
        backend: EmbeddingBackend::Model2vec,
        model: Some("../etc".to_string()),
    };
    assert!(embedder_from_config(&bad).is_err());

    // save_embedding writes only the embedding section and keeps the other fields.
    config.save_embedding(local.clone()).unwrap();
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(scaffold.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["scaffold_id"], "abc");
    assert_eq!(saved["version"], "0.9.0");
    assert_eq!(saved["embedding"]["backend"], "model2vec");
    assert_eq!(saved["embedding"]["model"], FIXTURE_REPO);
    let reloaded = KnobyteConfig::new(dir.path().to_path_buf(), scaffold);
    assert_eq!(reloaded.embedding, local);

    std::env::remove_var("KNOBYTE_MODELS_DIR");
}
