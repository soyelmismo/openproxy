//! AES-GCM 256 encryption for account API keys.
//!
//! Layout of a ciphertext blob: `nonce (12 bytes) || aes_gcm_seal(plaintext)`.
//! Embedding the nonce in the blob keeps encrypt output atomic (no separate
//! nonce column required at rest).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use zeroize::Zeroize;

use openproxy_types::{CoreError, Result};

const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const ENV_VAR: &str = "OPENPROXY_MASTER_KEY";
const PREVIOUS_ENV_VAR: &str = "OPENPROXY_MASTER_KEY_PREVIOUS";

/// AES-256 master key. Owns its 32 raw bytes; zeroized on drop via
/// the [`Drop`] implementation below.
#[derive(Clone)]
pub struct MasterKey {
    current: [u8; KEY_LEN],
    /// Previous key for rotation: `decrypt` falls back to it when the current
    /// key fails.
    previous: Option<[u8; KEY_LEN]>,
}

fn decode_key_env_var(var_name: &str) -> Result<[u8; KEY_LEN]> {
    let raw = std::env::var(var_name)
        .map_err(|_| CoreError::Config(format!("env var {var_name} is not set")))?;
    let decoded = BASE64
        .decode(raw.trim())
        .map_err(|e| CoreError::Config(format!("{var_name} is not valid base64: {e}")))?;
    decoded.try_into().map_err(|v: Vec<u8>| {
        CoreError::Config(format!(
            "{var_name} must decode to {KEY_LEN} bytes, got {}",
            v.len()
        ))
    })
}

fn load_optional_previous_key() -> Result<Option<[u8; KEY_LEN]>> {
    if std::env::var_os(PREVIOUS_ENV_VAR).is_none() {
        return Ok(None);
    }
    let bytes = decode_key_env_var(PREVIOUS_ENV_VAR)?;
    tracing::info!("loaded OPENPROXY_MASTER_KEY_PREVIOUS for rotation fallback");
    Ok(Some(bytes))
}

impl MasterKey {
    /// Load from `OPENPROXY_MASTER_KEY` env var, expected base64 of 32 bytes.
    /// Also loads `OPENPROXY_MASTER_KEY_PREVIOUS` if set (for key rotation).
    pub fn from_env() -> Result<Self> {
        let current = decode_key_env_var(ENV_VAR)?;
        let previous = load_optional_previous_key()?;
        Ok(Self { current, previous })
    }

    /// Load from a file containing base64 of 32 bytes (OP-29).
    ///
    /// Implements the documented-but-unimplemented
    /// `encryption_key_source = "file"` option. The file must have mode 0600
    /// (owner-only) on unix: a master key readable by other local users
    /// defeats the at-rest encryption of every provider credential.
    pub fn from_file(path: &str) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(path)
                .map_err(|e| CoreError::Config(format!("master key file {path:?}: {e}")))?;
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(CoreError::Config(format!(
                    "master key file {path:?} must have mode 0600 (got {:o}); \
                     chmod 600 it before starting openproxy",
                    mode & 0o777
                )));
            }
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| CoreError::Config(format!("master key file {path:?}: {e}")))?;
        let decoded = BASE64.decode(raw.trim()).map_err(|e| {
            CoreError::Config(format!("master key file {path:?} is not valid base64: {e}"))
        })?;
        let current: [u8; KEY_LEN] = decoded.try_into().map_err(|v: Vec<u8>| {
            CoreError::Config(format!(
                "master key file {path:?} must decode to {KEY_LEN} bytes, got {}",
                v.len()
            ))
        })?;
        // Rotation fallback stays env-based: OPENPROXY_MASTER_KEY_PREVIOUS.
        let previous = load_optional_previous_key()?;
        tracing::info!(path = %path, "loaded master key from file");
        Ok(Self { current, previous })
    }

    /// Generate a fresh random key. For tests and bootstrapping.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; KEY_LEN];
        getrandom::fill(&mut bytes)
            .map_err(|e| CoreError::Internal(format!("getrandom failed: {e}")))?;
        Ok(Self {
            current: bytes,
            previous: None,
        })
    }

    /// Encrypt a UTF-8 plaintext (API key) into a self-contained blob.
    ///
    /// Output layout: `nonce (12 bytes) || ciphertext_with_tag`.
    pub fn encrypt(&self, plaintext: &str) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(&self.current)
            .map_err(|e| CoreError::Internal(format!("key init failed: {e}")))?;
        let mut nonce_bytes = [0u8; 12];
        getrandom::fill(&mut nonce_bytes)
            .map_err(|e| CoreError::Internal(format!("nonce random failed: {e}")))?;
        let nonce = Nonce::try_from(nonce_bytes.as_slice())
            .map_err(|e| CoreError::Internal(format!("nonce len: {e}")))?;
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
        if blob.len() < NONCE_LEN + 16 {
            return Err(CoreError::Internal(
                "ciphertext blob too short to contain nonce and tag".into(),
            ));
        }
        let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::try_from(nonce_bytes)
            .map_err(|_| CoreError::Internal("failed to parse nonce".into()))?;

        if let Some(res) = try_decrypt(&self.current, &nonce, ct) {
            return res;
        }

        if let Some(prev) = &self.previous
            && let Some(res) = try_decrypt(prev, &nonce, ct)
        {
            tracing::debug!("decrypted with previous master key (rotation fallback)");
            return res;
        }

        Err(CoreError::Internal(
            "aes-gcm decrypt failed with both current and previous keys".into(),
        ))
    }
}

fn try_decrypt(
    raw_key: &[u8; KEY_LEN],
    nonce: &Nonce<aes_gcm::aead::consts::U12>,
    ct: &[u8],
) -> Option<Result<String>> {
    let cipher = match Aes256Gcm::new_from_slice(raw_key) {
        Ok(c) => c,
        Err(e) => return Some(Err(CoreError::Internal(format!("key init failed: {e}")))),
    };
    let pt = cipher.decrypt(nonce, ct).ok()?;
    Some(
        String::from_utf8(pt)
            .map_err(|e| CoreError::Internal(format!("decrypted plaintext is not utf-8: {e}"))),
    )
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.current.zeroize();
        if let Some(ref mut prev) = self.previous {
            prev.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = MasterKey::generate().unwrap();
        let blob = key.encrypt("sk-abc-123").unwrap();
        let pt = key.decrypt(&blob).unwrap();
        assert_eq!(pt, "sk-abc-123");
    }

    #[test]
    fn encrypt_is_nonce_random() {
        let key = MasterKey::generate().unwrap();
        let a = key.encrypt("x").unwrap();
        let b = key.encrypt("x").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let a = MasterKey::generate().unwrap();
        let b = MasterKey::generate().unwrap();
        let blob = a.encrypt("sk-abc-123").unwrap();
        assert!(b.decrypt(&blob).is_err());
    }

    #[test]
    fn from_env_missing() {
        let prev = std::env::var(ENV_VAR).ok();
        // SAFETY: tests are single-threaded; no other thread reads this env var.
        unsafe {
            std::env::remove_var(ENV_VAR);
        }
        let res = MasterKey::from_env();
        if let Some(v) = prev {
            // SAFETY: same single-threaded test context.
            unsafe {
                std::env::set_var(ENV_VAR, v);
            }
        }
        assert!(matches!(res, Err(CoreError::Config(_))));
    }

    #[test]
    fn from_env_wrong_length() {
        // 16 bytes, not 32: base64 of the raw key, unpadded.
        let short = BASE64.encode([0u8; 16]);
        let prev = std::env::var(ENV_VAR).ok();
        // SAFETY: tests are single-threaded; no other thread reads this env var.
        unsafe {
            std::env::set_var(ENV_VAR, &short);
        }
        let res = MasterKey::from_env();
        // SAFETY: same single-threaded test context.
        unsafe {
            std::env::remove_var(ENV_VAR);
        }
        if let Some(v) = prev {
            // SAFETY: same single-threaded test context.
            unsafe {
                std::env::set_var(ENV_VAR, v);
            }
        }
        assert!(matches!(res, Err(CoreError::Config(_))));
    }

    #[test]
    fn truncated_blob_fails() {
        let key = MasterKey::generate().unwrap();
        assert!(key.decrypt(&[0u8; 5]).is_err());
    }

    #[test]
    fn rotation_fallback_decrypts_with_previous_key() {
        let old_key = MasterKey::generate().unwrap();
        let new_key = MasterKey::generate().unwrap();
        let rotated_key = MasterKey {
            current: new_key.current,
            previous: Some(old_key.current),
        };
        let blob = old_key.encrypt("secret-value").unwrap();
        let pt = rotated_key.decrypt(&blob).unwrap();
        assert_eq!(pt, "secret-value");
    }
}
