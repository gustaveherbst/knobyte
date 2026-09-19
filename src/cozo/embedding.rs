//! Vector embedding generation for code and markdown entities in Knobyte.
//! Uses deterministic subword/token n-gram hashing with L2 normalization (dim = 128).

use sha2::{Digest, Sha256};

pub const EMBEDDING_DIM: usize = 128;

/// Deterministic 128-dimensional L2-normalized float embedding generator.
pub struct Embedder;

impl Embedder {
    /// Generate a 128-dimensional unit vector embedding for given text or code snippet.
    pub fn embed(text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; EMBEDDING_DIM];
        if text.trim().is_empty() {
            // Return zero or uniform small vector
            vector[0] = 1.0;
            return vector;
        }

        // Tokenize and extract subwords (camelCase, snake_case, identifiers)
        let tokens = tokenize(text);
        if tokens.is_empty() {
            vector[0] = 1.0;
            return vector;
        }

        for token in &tokens {
            // Unigram
            let h = hash_str(token);
            let idx = (h % (EMBEDDING_DIM as u64)) as usize;
            let sign = if (h >> 32) % 2 == 0 { 1.0f32 } else { -1.0f32 };
            vector[idx] += sign * 1.5;

            // Character trigrams for morphological and code similarity
            let chars: Vec<char> = token.chars().collect();
            if chars.len() >= 3 {
                for window in chars.windows(3) {
                    let s: String = window.iter().collect();
                    let th = hash_str(&s);
                    let tidx = (th % (EMBEDDING_DIM as u64)) as usize;
                    let tsign = if (th >> 32) % 2 == 0 { 0.5f32 } else { -0.5f32 };
                    vector[tidx] += tsign;
                }
            }
        }

        // Word bigrams
        for window in tokens.windows(2) {
            let bigram = format!("{}|{}", window[0], window[1]);
            let bh = hash_str(&bigram);
            let bidx = (bh % (EMBEDDING_DIM as u64)) as usize;
            let bsign = if (bh >> 32) % 2 == 0 { 1.2f32 } else { -1.2f32 };
            vector[bidx] += bsign;
        }

        // L2 Normalization: norm = sqrt(sum(x_i^2))
        let norm_sq: f32 = vector.iter().map(|&x| x * x).sum();
        if norm_sq > 1e-10 {
            let norm = norm_sq.sqrt();
            for val in vector.iter_mut() {
                *val /= norm;
            }
        } else {
            vector[0] = 1.0;
        }

        vector
    }

    /// Compute cosine similarity between two vectors (assuming L2 normalized).
    pub fn cosine_similarity(v1: &[f32], v2: &[f32]) -> f32 {
        if v1.len() != v2.len() {
            return 0.0;
        }
        let dot: f32 = v1.iter().zip(v2.iter()).map(|(&a, &b)| a * b).sum();
        dot.clamp(-1.0, 1.0)
    }
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&result[0..8]);
    u64::from_le_bytes(bytes)
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            if ch.is_uppercase() && !current.is_empty() && !current.ends_with(|c: char| c.is_uppercase()) {
                // camelCase split
                let prev = current.to_lowercase();
                if !prev.is_empty() {
                    tokens.push(prev);
                }
                current.clear();
            }
            current.push(ch);
        } else {
            if !current.is_empty() {
                let s = current.to_lowercase();
                if !s.is_empty() {
                    tokens.push(s);
                }
                current.clear();
            }
        }
    }

    if !current.is_empty() {
        let s = current.to_lowercase();
        if !s.is_empty() {
            tokens.push(s);
        }
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_dimension_and_norm() {
        let v = Embedder::embed("fn handle_request(req: Request) -> Response");
        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm: f32 = v.iter().map(|&x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_similarity_related_code() {
        let v1 = Embedder::embed("fn fetch_user_by_id(id: UserId) -> Option<User>");
        let v2 = Embedder::embed("fn get_user_account(user_id: UserId) -> Option<Account>");
        let v3 = Embedder::embed("struct ShaderPipelineUniformBufferGL");

        let sim_1_2 = Embedder::cosine_similarity(&v1, &v2);
        let sim_1_3 = Embedder::cosine_similarity(&v1, &v3);

        assert!(sim_1_2 > sim_1_3, "Related user functions should have higher similarity than shader pipeline: {} vs {}", sim_1_2, sim_1_3);
    }
}
