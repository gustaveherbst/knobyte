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

    // Create the key exclusively so concurrent first uses agree on one key.
    let tmp = local_dir.join(format!(".signing.key.{}.tmp", Uuid::new_v4().simple()));
    if fs::write(&tmp, new_key).is_ok() {
        if fs::hard_link(&tmp, &key_path).is_err() && !key_path.exists() {
            let _ = fs::rename(&tmp, &key_path);
        }
        let _ = fs::remove_file(&tmp);
    }
    match fs::read(&key_path) {
        Ok(key) if key.len() >= 32 => key,
        _ => new_key.to_vec(),
    }
}

pub fn sign_preview_payload(local_dir: &Path, payload: &str) -> String {
    let key = get_or_create_signing_key(local_dir);
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verify a hex-encoded HMAC-SHA256 signature in constant time.
pub fn verify_preview_payload(local_dir: &Path, payload: &str, signature: &str) -> bool {
    let sig_bytes = match hex::decode(signature.trim()) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let key = get_or_create_signing_key(local_dir);
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    mac.verify_slice(&sig_bytes).is_ok()
}
