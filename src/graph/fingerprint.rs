use sha2::{Digest, Sha256};

pub fn compute_node_id(file_path: &str, kind: &str, qualified_name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}", file_path, kind, qualified_name).as_bytes());
    let hex_digest = hex::encode(hasher.finalize());
    format!("{}:{}", kind, &hex_digest[..32])
}

pub fn compute_body_hash(body: &str) -> String {
    // Deterministic whitespace-normalized body hash: sha256(normalize_spaces(body))
    let mut normalized = String::with_capacity(body.len());
    let mut in_whitespace = false;
    for ch in body.chars() {
        if ch.is_whitespace() {
            if !in_whitespace {
                normalized.push(' ');
                in_whitespace = true;
            }
        } else {
            normalized.push(ch);
            in_whitespace = false;
        }
    }
    let trimmed = normalized.trim();
    let mut hasher = Sha256::new();
    hasher.update(trimmed.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn compute_file_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// MinHash / LSH fingerprints (reconciler tier 2)
// ---------------------------------------------------------------------------

/// MinHash sketch size; `LSH_BANDS * LSH_ROWS` must equal it.
pub const MINHASH_K: usize = 64;
pub const LSH_BANDS: usize = 32;
pub const LSH_ROWS: usize = 2;

/// Node kinds that carry a fingerprint (declarations with a body worth recognising).
pub const FINGERPRINT_KINDS: [&str; 9] = [
    "function", "method", "class", "struct", "enum", "interface", "trait", "type_alias", "impl",
];

const KEYWORDS: [&str; 74] = [
    "fn", "let", "mut", "pub", "impl", "struct", "enum", "trait", "match", "if", "else", "for",
    "while", "loop", "return", "break", "continue", "use", "mod", "self", "Self", "super", "crate",
    "async", "await", "move", "ref", "where", "dyn", "const", "static", "unsafe", "in", "as",
    "function", "var", "class", "extends", "implements", "interface", "new", "this", "export",
    "import", "from", "default", "try", "catch", "finally", "throw", "typeof", "instanceof",
    "def", "lambda", "yield", "with", "pass", "raise", "except", "elif", "not", "and", "or",
    "is", "None", "True", "False", "null", "undefined", "true", "false", "void", "public",
    "private",
];

/// Body tokens with identifier and literal spellings erased: a renamed or moved declaration
/// with the same shape keeps the same token stream.
pub fn normalized_tokens(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '#' && chars.get(i + 1) != Some(&'[') && chars.get(i + 1) != Some(&'!') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' || c == '\'' || c == '`' {
            let q = c;
            i += 1;
            while i < chars.len() && chars[i] != q {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push("STR".to_string());
            continue;
        }
        if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                i += 1;
            }
            out.push("NUM".to_string());
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if KEYWORDS.contains(&word.as_str()) {
                out.push(word);
            } else {
                out.push("ID".to_string());
            }
            continue;
        }
        out.push(c.to_string());
        i += 1;
    }
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

/// MinHash sketch of a declaration body over normalized-token trigrams.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MinHash {
    pub values: Vec<u32>,
    pub token_count: usize,
}

impl MinHash {
    pub fn of_body(body: &str) -> Self {
        Self::of_tokens(&normalized_tokens(body))
    }

    pub fn of_tokens(tokens: &[String]) -> Self {
        let mut minima = vec![u32::MAX; MINHASH_K];
        if tokens.len() >= 3 {
            let mut seen = std::collections::HashSet::new();
            for w in tokens.windows(3) {
                let key = format!("{}\u{0}{}\u{0}{}", w[0], w[1], w[2]);
                if !seen.insert(key.clone()) {
                    continue;
                }
                let h = fnv1a(key.as_bytes());
                for (seed, m) in minima.iter_mut().enumerate() {
                    let v = (splitmix64(h ^ (seed as u64).wrapping_mul(0x9e3779b97f4a7c15)) >> 32) as u32;
                    if v < *m {
                        *m = v;
                    }
                }
            }
        }
        MinHash {
            values: minima,
            token_count: tokens.len(),
        }
    }

    /// Fraction of equal sketch positions (an estimate of trigram-set Jaccard similarity).
    pub fn similarity(&self, other: &MinHash) -> f64 {
        let n = self.values.len().min(other.values.len());
        if n == 0 {
            return 0.0;
        }
        let eq = (0..n).filter(|&i| self.values[i] == other.values[i]).count();
        eq as f64 / n as f64
    }

    /// LSH bucket per band (`LSH_BANDS` values).
    pub fn band_hashes(&self) -> Vec<i64> {
        (0..LSH_BANDS)
            .map(|b| {
                let mut bytes = Vec::with_capacity(LSH_ROWS * 4 + 1);
                bytes.push(b as u8);
                for v in &self.values[b * LSH_ROWS..(b + 1) * LSH_ROWS] {
                    bytes.extend_from_slice(&v.to_be_bytes());
                }
                fnv1a(&bytes) as i64
            })
            .collect()
    }

    pub fn to_blob(&self) -> Vec<u8> {
        self.values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    pub fn from_blob(blob: &[u8], token_count: usize) -> Self {
        MinHash {
            values: blob
                .chunks_exact(4)
                .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
            token_count,
        }
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.to_blob())
    }

    pub fn from_hex(text: &str, token_count: usize) -> Option<Self> {
        let blob = hex::decode(text).ok()?;
        (blob.len() == MINHASH_K * 4).then(|| Self::from_blob(&blob, token_count))
    }
}

/// Jaccard overlap of two neighbour-id sets (1.0 when both are empty).
pub fn neighbor_overlap(a: &[String], b: &[String]) -> f64 {
    let sa: std::collections::HashSet<&String> = a.iter().collect();
    let sb: std::collections::HashSet<&String> = b.iter().collect();
    let union = sa.union(&sb).count();
    if union == 0 {
        return 1.0;
    }
    sa.intersection(&sb).count() as f64 / union as f64
}
