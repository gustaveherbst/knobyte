//! Vector embeddings for code and markdown entities in Knobyte.
//!
//! Embeddings are produced by an [`Embedder`] backend:
//!
//! * [`HashedEmbedder`] (default, `hashed-v1`, 128-dim): a *hashed lexical embedding* (the
//!   "hashing trick"). Text is split into identifier-aware tokens (camelCase and snake_case are
//!   split, compound identifiers are kept too), and each token, character trigram and token
//!   bigram is hashed with SHA-256 into one of 128 signed buckets. Cosine similarity measures
//!   weighted token overlap; synonyms with no shared tokens are not related. Deterministic and
//!   needs no model.
//! * [`super::model2vec::Model2VecEmbedder`]: a local Model2Vec static embedding model
//!   (e.g. `minishlab/potion-base-8M`, 256-dim) that captures semantic similarity. The model is
//!   only ever downloaded explicitly with `knobyte cozo model pull`.
//!
//! Code symbols are embedded from their name and qualified name (weighted highest), signature,
//! docstring, kind/path, and identifier tokens from the body ([`CodeSymbolText`]).

use sha2::{Digest, Sha256};

/// Dimension of the hashed lexical embedding.
pub const EMBEDDING_DIM: usize = 128;

/// Embedder id stored in Cozo for the hashed backend.
pub const HASHED_EMBEDDER_ID: &str = "hashed-v1";

/// Max identifier tokens taken from a symbol body.
pub const MAX_BODY_TOKENS: usize = 256;

/// Max bytes of free text (docstrings, wiki bodies) fed into an embedding field.
pub const MAX_FIELD_BYTES: usize = 4000;

/// Text fields of a code symbol used to build its embedding.
#[derive(Debug, Clone, Default)]
pub struct CodeSymbolText<'a> {
    pub kind: &'a str,
    pub name: &'a str,
    pub qualified_name: &'a str,
    pub file_path: &'a str,
    pub signature: Option<&'a str>,
    pub docstring: Option<&'a str>,
    /// Source of the symbol; may be empty when unavailable.
    pub body: &'a str,
}

impl CodeSymbolText<'_> {
    /// Weighted text fields: names x3, signature/docstring x2, kind/path and body identifiers x1.
    pub fn fields(&self) -> Vec<(String, f32)> {
        vec![
            (format!("{} {}", self.name, self.qualified_name), 3.0),
            (
                truncate(self.signature.unwrap_or(""), MAX_FIELD_BYTES).to_string(),
                2.0,
            ),
            (
                truncate(self.docstring.unwrap_or(""), MAX_FIELD_BYTES).to_string(),
                2.0,
            ),
            (format!("{} {}", self.kind, self.file_path), 1.0),
            (body_identifier_terms(self.body, MAX_BODY_TOKENS), 1.0),
        ]
    }
}

/// Weighted text fields of a wiki entity: title x3, summary x2, type and body excerpt x1.
pub fn wiki_fields(
    title: &str,
    summary: &str,
    entity_type: &str,
    body: &str,
) -> Vec<(String, f32)> {
    let body_excerpt: String = body.chars().take(MAX_FIELD_BYTES).collect();
    vec![
        (title.to_string(), 3.0),
        (summary.to_string(), 2.0),
        (entity_type.to_string(), 1.0),
        (body_excerpt, 1.0),
    ]
}

/// An embedding backend. All vectors produced by one embedder share one space; vectors of
/// different embedders (different [`Embedder::id`]) must never be compared.
pub trait Embedder: Send + Sync {
    /// Stable identifier of the embedding space (e.g. `hashed-v1`,
    /// `model2vec:minishlab/potion-base-8M`).
    fn id(&self) -> String;

    /// Vector dimension.
    fn dim(&self) -> usize;

    /// L2-normalized embedding of a free-text query or snippet.
    fn embed(&self, text: &str) -> Vec<f32>;

    /// L2-normalized embedding of several weighted text fields. The default embeds each
    /// non-empty field and takes the normalized weighted sum.
    fn embed_fields(&self, fields: &[(String, f32)]) -> Vec<f32> {
        let mut acc = vec![0.0f32; self.dim()];
        let mut any = false;
        for (text, weight) in fields {
            if text.trim().is_empty() || *weight <= 0.0 {
                continue;
            }
            let v = self.embed(text);
            if v.len() != acc.len() {
                continue;
            }
            any = true;
            for (a, x) in acc.iter_mut().zip(v) {
                *a += x * weight;
            }
        }
        if !any {
            return unit_fallback(self.dim());
        }
        normalize_or_fallback(acc)
    }

    /// Embedding of a code symbol.
    fn embed_code_symbol(&self, symbol: &CodeSymbolText) -> Vec<f32> {
        self.embed_fields(&symbol.fields())
    }

    /// Embedding of a wiki entity.
    fn embed_wiki(&self, title: &str, summary: &str, entity_type: &str, body: &str) -> Vec<f32> {
        self.embed_fields(&wiki_fields(title, summary, entity_type, body))
    }

    /// Search results scoring below this (1 - cosine distance) are dropped.
    fn min_relevance(&self) -> f64 {
        super::engine::MIN_RELEVANCE_SCORE
    }
}

/// Unit vector along the first axis: the embedding of "nothing" (keeps cosine well-defined).
pub fn unit_fallback(dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim.max(1)];
    v[0] = 1.0;
    v
}

/// L2-normalize `v`, or return [`unit_fallback`] when it is (near) zero.
pub fn normalize_or_fallback(mut v: Vec<f32>) -> Vec<f32> {
    let norm_sq: f32 = v.iter().map(|&x| x * x).sum();
    if norm_sq > 1e-10 && norm_sq.is_finite() {
        let norm = norm_sq.sqrt();
        for x in v.iter_mut() {
            *x /= norm;
        }
        v
    } else {
        unit_fallback(v.len())
    }
}

/// Compute cosine similarity between two vectors (assuming L2 normalized).
pub fn cosine_similarity(v1: &[f32], v2: &[f32]) -> f32 {
    if v1.len() != v2.len() {
        return 0.0;
    }
    let dot: f32 = v1.iter().zip(v2.iter()).map(|(&a, &b)| a * b).sum();
    dot.clamp(-1.0, 1.0)
}

/// Deterministic 128-dimensional L2-normalized hashed lexical embedding generator.
#[derive(Debug, Clone, Copy, Default)]
pub struct HashedEmbedder;

impl Embedder for HashedEmbedder {
    fn id(&self) -> String {
        HASHED_EMBEDDER_ID.to_string()
    }

    fn dim(&self) -> usize {
        EMBEDDING_DIM
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        HashedEmbedder::embed_text(text)
    }

    fn embed_fields(&self, fields: &[(String, f32)]) -> Vec<f32> {
        let borrowed: Vec<(&str, f32)> = fields.iter().map(|(t, w)| (t.as_str(), *w)).collect();
        HashedEmbedder::embed_weighted(&borrowed)
    }
}

/// Common keywords/literals that carry no meaning for search.
const STOPWORDS: &[&str] = &[
    "let",
    "mut",
    "fn",
    "pub",
    "return",
    "if",
    "else",
    "for",
    "while",
    "loop",
    "match",
    "in",
    "self",
    "this",
    "const",
    "var",
    "def",
    "class",
    "import",
    "from",
    "use",
    "crate",
    "impl",
    "struct",
    "enum",
    "trait",
    "async",
    "await",
    "true",
    "false",
    "none",
    "null",
    "undefined",
    "some",
    "ok",
    "err",
    "as",
    "ref",
    "where",
    "type",
    "function",
    "new",
    "the",
    "and",
    "or",
    "not",
    "is",
    "of",
    "to",
    "a",
    "an",
    "pass",
    "super",
    "static",
    "export",
    "default",
    "unwrap",
    "clone",
    "to_string",
    "string",
    "str",
    "i32",
    "i64",
    "u32",
    "u64",
    "usize",
    "f32",
    "f64",
    "bool",
    "vec",
    "option",
    "result",
    "with",
    "try",
    "except",
    "catch",
    "break",
    "continue",
];

impl HashedEmbedder {
    /// Generate a 128-dimensional unit vector embedding for a text or code snippet.
    pub fn embed_text(text: &str) -> Vec<f32> {
        Self::embed_weighted(&[(text, 1.0)])
    }

    /// Embed several text fields with per-field weights (e.g. name x3, body x1).
    pub fn embed_weighted(fields: &[(&str, f32)]) -> Vec<f32> {
        let mut vector = vec![0.0f32; EMBEDDING_DIM];
        let mut any = false;

        for (text, weight) in fields {
            if text.trim().is_empty() || *weight <= 0.0 {
                continue;
            }
            let tokens = tokenize(text);
            if tokens.is_empty() {
                continue;
            }
            any = true;
            accumulate(&mut vector, &tokens, *weight);
        }

        if !any {
            vector[0] = 1.0;
            return vector;
        }

        // L2 Normalization: norm = sqrt(sum(x_i^2))
        let norm_sq: f32 = vector.iter().map(|&x| x * x).sum();
        if norm_sq > 1e-10 {
            let norm = norm_sq.sqrt();
            for val in vector.iter_mut() {
                *val /= norm;
            }
        } else {
            vector = vec![0.0f32; EMBEDDING_DIM];
            vector[0] = 1.0;
        }

        vector
    }
}

fn accumulate(vector: &mut [f32], tokens: &[String], weight: f32) {
    for token in tokens {
        // Unigram
        let h = hash_str(token);
        let idx = (h % (EMBEDDING_DIM as u64)) as usize;
        let sign = if (h >> 32) & 1 == 0 { 1.0f32 } else { -1.0f32 };
        vector[idx] += sign * 1.5 * weight;

        // Character trigrams for morphological similarity
        let chars: Vec<char> = token.chars().collect();
        if chars.len() >= 3 {
            for window in chars.windows(3) {
                let s: String = window.iter().collect();
                let th = hash_str(&s);
                let tidx = (th % (EMBEDDING_DIM as u64)) as usize;
                let tsign = if (th >> 32) & 1 == 0 { 0.5f32 } else { -0.5f32 };
                vector[tidx] += tsign * weight;
            }
        }
    }

    // Word bigrams
    for window in tokens.windows(2) {
        let bigram = format!("{}|{}", window[0], window[1]);
        let bh = hash_str(&bigram);
        let bidx = (bh % (EMBEDDING_DIM as u64)) as usize;
        let bsign = if (bh >> 32) & 1 == 0 { 1.2f32 } else { -1.2f32 };
        vector[bidx] += bsign * weight;
    }
}

fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&result[0..8]);
    u64::from_le_bytes(bytes)
}

/// Split one identifier into lowercase sub-words: `fetchUserByID` -> [fetch, user, by, id],
/// `HTTPServer_v2` -> [http, server, v2].
pub fn split_identifier(ident: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for chunk in ident.split('_').filter(|c| !c.is_empty()) {
        let chars: Vec<char> = chunk.chars().collect();
        let mut current = String::new();
        for i in 0..chars.len() {
            let c = chars[i];
            if !current.is_empty() && c.is_uppercase() {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).map(|n| n.is_lowercase()).unwrap_or(false);
                // aB -> a|B ; ABc -> A|Bc ; 1A -> 1|A
                if prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_lower) {
                    parts.push(current.to_lowercase());
                    current.clear();
                }
            }
            current.push(c);
        }
        if !current.is_empty() {
            parts.push(current.to_lowercase());
        }
    }
    parts
}

/// Identifier-aware tokenizer: sub-words of every identifier, plus the whole compound
/// identifier when it was split (so exact identifier matches score higher).
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.is_empty() {
            continue;
        }
        let parts = split_identifier(raw);
        if parts.len() > 1 {
            let compound = raw.to_lowercase();
            tokens.extend(parts);
            tokens.push(compound);
        } else {
            tokens.extend(parts);
        }
    }
    tokens
}

/// Rewrite every identifier in `text` as space-separated lowercase sub-words
/// (`fetchUserByID` -> `fetch user by id`, `user_id` -> `user id`), leaving other characters
/// in place. Natural-language tokenizers (WordPiece etc.) see real words this way.
pub fn humanize_identifiers(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            let parts = split_identifier(word);
            if parts.is_empty() {
                out.push_str(word);
            } else {
                out.push_str(&parts.join(" "));
            }
            word.clear();
        }
    };
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Distinct, meaningful identifier tokens from a code body (keywords and 1-char names dropped),
/// in first-seen order, at most `max` of them.
pub fn body_identifier_terms(body: &str, max: usize) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in body.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if out.len() >= max {
            break;
        }
        if raw.len() < 2 || raw.chars().next().map(|c| c.is_numeric()).unwrap_or(true) {
            continue;
        }
        let lower = raw.to_lowercase();
        if STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if seen.insert(lower) {
            out.push(raw.to_string());
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_dimension_and_norm() {
        let v = HashedEmbedder::embed_text("fn handle_request(req: Request) -> Response");
        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm: f32 = v.iter().map(|&x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_similarity_related_code() {
        let v1 = HashedEmbedder::embed_text("fn fetch_user_by_id(id: UserId) -> Option<User>");
        let v2 =
            HashedEmbedder::embed_text("fn get_user_account(user_id: UserId) -> Option<Account>");
        let v3 = HashedEmbedder::embed_text("struct ShaderPipelineUniformBufferGL");

        let sim_1_2 = cosine_similarity(&v1, &v2);
        let sim_1_3 = cosine_similarity(&v1, &v3);

        assert!(
            sim_1_2 > sim_1_3,
            "Related user functions should have higher similarity than shader pipeline: {} vs {}",
            sim_1_2,
            sim_1_3
        );
    }

    #[test]
    fn test_identifier_splitting() {
        assert_eq!(
            split_identifier("fetchUserByID"),
            vec!["fetch", "user", "by", "id"]
        );
        assert_eq!(split_identifier("HTTPServer"), vec!["http", "server"]);
        assert_eq!(
            split_identifier("validate_token"),
            vec!["validate", "token"]
        );
        let t = tokenize("validate_token");
        assert!(t.contains(&"validate".to_string()) && t.contains(&"validate_token".to_string()));
    }

    #[test]
    fn test_body_terms_drive_similarity() {
        let e = HashedEmbedder;
        let sym = |file_path, body| CodeSymbolText {
            kind: "function",
            name: "run",
            qualified_name: "run",
            file_path,
            signature: Some("fn run()"),
            docstring: None,
            body,
        };
        let with_body = e.embed_code_symbol(&sym(
            "src/x.rs",
            "fn run() { let token = decode_jwt(header); verify_signature(token) }",
        ));
        let other =
            e.embed_code_symbol(&sym("src/y.rs", "fn run() { draw_canvas(width, height) }"));
        let q = e.embed("verify jwt signature");
        assert!(cosine_similarity(&q, &with_body) > cosine_similarity(&q, &other));
    }

    #[test]
    fn test_hashed_code_symbol_matches_weighted_fields() {
        // The trait path must reproduce the original weighted hashed embedding exactly.
        let sym = CodeSymbolText {
            kind: "function",
            name: "login",
            qualified_name: "auth::login",
            file_path: "src/auth.rs",
            signature: Some("fn login(user: &str)"),
            docstring: Some("Log a user in."),
            body: "fn login(user: &str) { check_password(user) }",
        };
        let expected = HashedEmbedder::embed_weighted(&[
            ("login auth::login", 3.0),
            ("fn login(user: &str)", 2.0),
            ("Log a user in.", 2.0),
            ("function src/auth.rs", 1.0),
            ("login user check_password", 1.0),
        ]);
        assert_eq!(HashedEmbedder.embed_code_symbol(&sym), expected);
    }

    #[test]
    fn test_humanize_identifiers() {
        assert_eq!(
            humanize_identifiers("fn fetchUserByID(user_id: u32)"),
            "fn fetch user by id(user id: u32)"
        );
    }
}
