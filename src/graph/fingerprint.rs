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

pub fn generate_minhash(text: &str) -> Vec<u8> {
    // 64 uint32 MinHash values = 256 bytes
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut sketch = vec![0u8; 256];

    for i in 0..64 {
        let mut min_val = u32::MAX;
        let seed = (i as u32).wrapping_mul(0x9e3779b9);

        for token in &tokens {
            let mut hasher = Sha256::new();
            hasher.update(seed.to_le_bytes());
            hasher.update(token.as_bytes());
            let hash = hasher.finalize();
            let val = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]);
            if val < min_val {
                min_val = val;
            }
        }

        let bytes = min_val.to_be_bytes();
        let offset = i * 4;
        sketch[offset..offset + 4].copy_from_slice(&bytes);
    }

    sketch
}
