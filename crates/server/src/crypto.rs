//! AES-256-GCM field encryption for the CA private key (PRD §2.3, §5).

// `aes-gcm 0.10` re-exports `generic-array 0.14`, whose `from_slice` is deprecated;
// no newer `aes-gcm` release exists yet, so suppress locally until we can upgrade.
#![allow(deprecated)]

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};

pub struct FieldCipher {
    cipher: Aes256Gcm,
}

impl FieldCipher {
    pub fn from_b64_key(b64: &str) -> Result<Self> {
        let raw = STANDARD
            .decode(b64)
            .context("field key is not valid base64")?;
        if raw.len() != 32 {
            return Err(anyhow!("field key must be 32 bytes (got {})", raw.len()));
        }
        let key = Key::<Aes256Gcm>::from_slice(&raw);
        Ok(Self {
            cipher: Aes256Gcm::new(key),
        })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| anyhow!("aes-gcm encrypt failed: {e}"))?;
        Ok((ct, nonce_bytes.to_vec()))
    }

    pub fn decrypt(&self, ciphertext: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
        if nonce.len() != 12 {
            return Err(anyhow!("nonce must be 12 bytes (got {})", nonce.len()));
        }
        let nonce = Nonce::from_slice(nonce);
        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow!("aes-gcm decrypt failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    #[test]
    fn round_trips_plaintext() {
        let key_b64 = STANDARD.encode([7u8; 32]);
        let c = FieldCipher::from_b64_key(&key_b64).unwrap();
        let (ct, nonce) = c.encrypt(b"ca-private-key").unwrap();
        assert_ne!(ct, b"ca-private-key");
        assert_eq!(c.decrypt(&ct, &nonce).unwrap(), b"ca-private-key");
    }

    #[test]
    fn rejects_wrong_length_key() {
        let key_b64 = STANDARD.encode([7u8; 16]); // 128-bit, not allowed
        assert!(FieldCipher::from_b64_key(&key_b64).is_err());
    }

    #[test]
    fn rejects_wrong_length_nonce() {
        let c = FieldCipher::from_b64_key(&STANDARD.encode([7u8; 32])).unwrap();
        let (ct, _nonce) = c.encrypt(b"x").unwrap();
        assert!(c.decrypt(&ct, &[0u8; 4]).is_err());
    }
}
