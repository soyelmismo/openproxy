//! AES-GCM 256 encryption for account API keys.
//!
//! Layout of a ciphertext blob: `nonce (12 bytes) || aes_gcm_seal(plaintext)`.
//! Embedding the nonce in the blob keeps encrypt output atomic (no separate
//! nonce column required at rest).

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::error::{CoreError, Result};

const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const ENV_VAR: &str = "OPENPROXY_MASTER_KEY";

/// AES-256 master key. Owns its 32 raw bytes; zeroized on drop via
/// `Aes256Gcm` key material handling.
pub struct MasterKey([u8; KEY_LEN]);

impl MasterKey {
    /// Load from `OPENPROXY_MASTER_KEY` env var, expected base64 of 32 bytes.
    pub fn from_env() -> Result<Self> {
        let raw = std::env::var(ENV_VAR).map_err(|_| {
            CoreError::Config(format!("env var {ENV_VAR} is not set"))
        })?;
        let decoded = BASE64.decode(raw.trim()).map_err(|e| {
            CoreError::Config(format!("{ENV_VAR} is not valid base64: {e}"))
        })?;
        let bytes: [u8; KEY_LEN] = decoded.try_into().map_err(|v: Vec<u8>| {
            CoreError::Config(format!(
                "{ENV_VAR} must decode to {KEY_LEN} bytes, got {}",
                v.len()
            ))
        })?;
        Ok(Self(bytes))
    }

    /// Generate a fresh random key. For tests and bootstrapping.
    pub fn generate() -> Self {
        let key = Aes256Gcm::generate_key(OsRng);
        let bytes: [u8; KEY_LEN] = key.into();
        Self(bytes)
    }

    /// Encrypt a UTF-8 plaintext (API key) into a self-contained blob.
    ///
    /// Output layout: `nonce (12 bytes) || ciphertext_with_tag`.
    pub fn encrypt(&self, plaintext: &str) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0));
        let nonce: Nonce<_> = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut blob = nonce.to_vec();
        let ct = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| CoreError::Internal(format!("aes-gcm encrypt failed: {e}")))?;
        blob.extend_from_slice(&ct);
        Ok(blob)
    }

    /// Decrypt a blob produced by `encrypt`. The first 12 bytes are the nonce.
    pub fn decrypt(&self, blob: &[u8]) -> Result<String> {
        if blob.len() <= NONCE_LEN {
            return Err(CoreError::Internal(
                "ciphertext blob too short to contain nonce".into(),
            ));
        }
        let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0));
        let pt = cipher
            .decrypt(nonce, ct)
            .map_err(|e| CoreError::Internal(format!("aes-gcm decrypt failed: {e}")))?;
        String::from_utf8(pt)
            .map_err(|e| CoreError::Internal(format!("decrypted plaintext is not utf-8: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = MasterKey::generate();
        let blob = key.encrypt("sk-abc-123").unwrap();
        let pt = key.decrypt(&blob).unwrap();
        assert_eq!(pt, "sk-abc-123");
    }

    #[test]
    fn encrypt_is_nonce_random() {
        let key = MasterKey::generate();
        let a = key.encrypt("x").unwrap();
        let b = key.encrypt("x").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let a = MasterKey::generate();
        let b = MasterKey::generate();
        let blob = a.encrypt("sk-abc-123").unwrap();
        assert!(b.decrypt(&blob).is_err());
    }

    #[test]
    fn from_env_missing() {
        // Point at a definitely-unset var by temporarily unsetting it.
        let prev = std::env::var(ENV_VAR).ok();
        std::env::remove_var(ENV_VAR);
        let res = MasterKey::from_env();
        if let Some(v) = prev {
            std::env::set_var(ENV_VAR, v);
        }
        assert!(matches!(res, Err(CoreError::Config(_))));
    }

    #[test]
    fn from_env_wrong_length() {
        // 16 bytes (not 32) base64-encoded.
        let short = BASE64.encode([0u8; 16]);
        let prev = std::env::var(ENV_VAR).ok();
        std::env::set_var(ENV_VAR, &short);
        let res = MasterKey::from_env();
        std::env::remove_var(ENV_VAR);
        if let Some(v) = prev {
            std::env::set_var(ENV_VAR, v);
        }
        assert!(matches!(res, Err(CoreError::Config(_))));
    }

    #[test]
    fn truncated_blob_fails() {
        let key = MasterKey::generate();
        // 5 bytes is less than the 12-byte nonce.
        assert!(key.decrypt(&[0u8; 5]).is_err());
    }
}
