use sha2::{Digest, Sha256};

pub fn cache_key(text: &str, voice: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.update(b"|");
    hasher.update(voice.as_bytes());
    hex::encode(hasher.finalize())
}
