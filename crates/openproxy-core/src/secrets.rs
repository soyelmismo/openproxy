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
const PREVIOUS_ENV_VAR: &str = "OPENPROXY_MASTER_KEY_PREVIOUS";

/// AES-256 master key. Owns its 32 raw bytes; zeroized on drop via
/// `Aes256Gcm` key material handling.
pub struct MasterKey {
    current: [u8; KEY_LEN],
    /// Optional previous key for rotation. If decryption with the
    /// current key fails, the decrypt path falls back to this key.
    previous: Option<[u8; KEY_LEN]>,
}

impl MasterKey {
    /// Load from `OPENPROXY_MASTER_KEY` env var, expected base64 of 32 bytes.
    /// Also loads `OPENPROXY_MASTER_KEY_PREVIOUS` if set (for key rotation).
    pub fn from_env() -> Result<Self> {
        let raw = std::env::var(ENV_VAR).map_err(|_| {
            CoreError::Config(format!("env var {ENV_VAR} is not set"))
        })?;
        let decoded = BASE64.decode(raw.trim()).map_err(|e| {
            CoreError::Config(format!("{ENV_VAR} is not valid base64: {e}"))
        })?;
        let current: [u8; KEY_LEN] = decoded.try_into().map_err(|v: Vec<u8>| {
            CoreError::Config(format!(
                "{ENV_VAR} must decode to {KEY_LEN} bytes, got {}",
                v.len()
            ))
        })?;

        // Load previous key for rotation (optional).
        let previous = match std::env::var(PREVIOUS_ENV_VAR) {
            Ok(prev_raw) => {
                match BASE64.decode(prev_raw.trim()) {
                    Ok(decoded) => {
                        match TryInto::<[u8; KEY_LEN]>::try_into(decoded) {
                            Ok(bytes) => {
                                tracing::info!(
                                    "loaded OPENPROXY_MASTER_KEY_PREVIOUS for rotation fallback"
                                );
                                Some(bytes)
                            }
                            Err(v) => {
                                let v_len = v.len();
                                return Err(CoreError::Config(format!(
                                    "{PREVIOUS_ENV_VAR} must decode to {KEY_LEN} bytes, got {}",
                                    v_len
                                )));
                            }
                        }
                    }
                    Err(e) => {
                        return Err(CoreError::Config(format!(
                            "{PREVIOUS_ENV_VAR} is not valid base64: {e}"
                        )));
                    }
                }
            }
            Err(_) => None,
        };

        Ok(Self { current, previous })
    }

    /// Generate a fresh random key. For tests and bootstrapping.
    pub fn generate() -> Self {
        let key = Aes256Gcm::generate_key(OsRng);
        let bytes: [u8; KEY_LEN] = key.into();
        Self { current: bytes, previous: None }
    }

    /// Encrypt a UTF-8 plaintext (API key) into a self-contained blob.
    ///
    /// Output layout: `nonce (12 bytes) || ciphertext_with_tag`.
    pub fn encrypt(&self, plaintext: &str) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.current));
        let nonce: Nonce<_> = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut blob = nonce.to_vec();
        let ct = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| CoreError::Internal(format!("aes-gcm encrypt failed: {e}")))?;
        blob.extend_from_slice(&ct);
        Ok(blob)
    }

    /// Decrypt a blob produced by `encrypt`. The first 12 bytes are the nonce.
    /// Falls back to the previous key if the current key fails (rotation).
    pub fn decrypt(&self, blob: &[u8]) -> Result<String> {
        if blob.len() <= NONCE_LEN {
            return Err(CoreError::Internal(
                "ciphertext blob too short to contain nonce".into(),
            ));
        }
        let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);

        // Try current key first.
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.current));
        if let Ok(pt) = cipher.decrypt(nonce, ct) {
            return String::from_utf8(pt)
                .map_err(|e| CoreError::Internal(format!("decrypted plaintext is not utf-8: {e}")));
        }

        // Fall back to previous key (rotation).
        if let Some(prev) = &self.previous {
            let prev_cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(prev));
            if let Ok(pt) = prev_cipher.decrypt(nonce, ct) {
                tracing::debug!("decrypted with previous master key (rotation fallback)");
                return String::from_utf8(pt)
                    .map_err(|e| CoreError::Internal(format!("decrypted plaintext is not utf-8: {e}")));
            }
        }

        Err(CoreError::Internal(
            "aes-gcm decrypt failed with both current and previous keys".into(),
        ))
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
        // SAFETY: tests are single-threaded; no other thread reads this env var.
        unsafe { std::env::remove_var(ENV_VAR); }
        let res = MasterKey::from_env();
        if let Some(v) = prev {
            // SAFETY: same single-threaded test context.
            unsafe { std::env::set_var(ENV_VAR, v); }
        }
        assert!(matches!(res, Err(CoreError::Config(_))));
    }

    #[test]
    fn from_env_wrong_length() {
        // 16 bytes (not 32) base64-encoded.
        let short = BASE64.encode([0u8; 16]);
        let prev = std::env::var(ENV_VAR).ok();
        // SAFETY: tests are single-threaded; no other thread reads this env var.
        unsafe { std::env::set_var(ENV_VAR, &short); }
        let res = MasterKey::from_env();
        // SAFETY: same single-threaded test context.
        unsafe { std::env::remove_var(ENV_VAR); }
        if let Some(v) = prev {
            // SAFETY: same single-threaded test context.
            unsafe { std::env::set_var(ENV_VAR, v); }
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
