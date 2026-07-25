//! Session token minting and hashing.
//!
//! The cookie carries 32 random bytes; only their sha256 reaches the database.
//! A database read -- a backup, a replica, a leaked dump -- therefore yields no
//! usable session tokens. Mirrors `enrollment_tokens.token_hash`.

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Returns `(cookie value, sha256 to store)`.
pub fn new_session_token() -> (String, Vec<u8>) {
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let hash = hash_token(&token);
    (token, hash)
}

/// Consumed by the `require_auth` middleware (`crate::auth::require_auth`),
/// which hashes the incoming cookie value before looking it up.
pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_the_hash_is_not_the_token() {
        let (t1, h1) = new_session_token();
        let (t2, _) = new_session_token();
        assert_ne!(t1, t2, "each session must get a fresh token");
        assert_eq!(h1.len(), 32, "sha256");
        assert_ne!(h1, t1.as_bytes(), "the stored value must not be the token");
        assert_eq!(hash_token(&t1), h1, "hashing must be deterministic");
    }
}
