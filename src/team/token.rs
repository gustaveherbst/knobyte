use std::fs;
use std::path::Path;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

pub fn get_or_create_signing_key(local_dir: &Path) -> Vec<u8> {
    let key_path = local_dir.join("signing.key");
    if key_path.exists() {
        if let Ok(key) = fs::read(&key_path) {
            if key.len() >= 32 {
                return key;
            }
        }
    }

    let _ = fs::create_dir_all(local_dir);
    let mut new_key = [0u8; 32];
    for chunk in new_key.chunks_mut(16) {
        let u = Uuid::new_v4();
        chunk.copy_from_slice(u.as_bytes());
    }

    let _ = fs::write(&key_path, new_key);
    new_key.to_vec()
}

pub fn sign_preview_payload(local_dir: &Path, payload: &str) -> String {
    let key = get_or_create_signing_key(local_dir);
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn verify_preview_payload(local_dir: &Path, payload: &str, signature: &str) -> bool {
    let expected = sign_preview_payload(local_dir, payload);
    expected == signature
}
