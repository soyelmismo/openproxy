//! Cryptographic operations for encrypting and decrypting backup bundles.
//!
//! Uses AES-256-GCM with PBKDF2-HMAC-SHA256 key derivation (600 000
//! iterations for new bundles). Bundles produced by older versions that used
//! the legacy iterated-SHA-256 scheme (`kdf: "sha256_iter"`) remain
//! decryptable for migration purposes.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use openproxy_types::backup::{BACKUP_FORMAT_VERSION, BackupBundle, BackupPayload};
use openproxy_types::{CoreError, Result};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

/// OWASP-recommended minimum for PBKDF2-HMAC-SHA256.
pub const DEFAULT_KDF_ITERATIONS: u32 = 600_000;
/// KDF identifier written to new encrypted bundles.
pub const KDF_ALGO: &str = "pbkdf2-hmac-sha256";
/// Legacy identifier (iterated bare SHA-256) — read-only compatibility.
pub const KDF_ALGO_LEGACY: &str = "sha256_iter";
const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;

/// HMAC-SHA256 (RFC 2104) over `data` with `key`.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64; // SHA-256 block size
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let inner = Sha256::new().chain_update(ipad).chain_update(data).finalize();
    Sha256::new()
        .chain_update(opad)
        .chain_update(inner)
        .finalize()
        .into()
}

/// PBKDF2-HMAC-SHA256 (RFC 8018 / RFC 2898).
fn pbkdf2_hmac_sha256(passphrase: &[u8], salt: &[u8], iterations: u32, dk_len: usize) -> Vec<u8> {
    let iterations = iterations.max(1);
    let mut derived = Vec::with_capacity(dk_len);
    let mut block_index: u32 = 1;
    while derived.len() < dk_len {
        let mut salt_block = Vec::with_capacity(salt.len() + 4);
        salt_block.extend_from_slice(salt);
        salt_block.extend_from_slice(&block_index.to_be_bytes());

        // U_1 = PRF(P, S || INT(i)); U_j = PRF(P, U_{j-1}); T = U_1 ^ ... ^ U_c
        let mut u = hmac_sha256(passphrase, &salt_block);
        let mut t = u;
        for _ in 1..iterations {
            u = hmac_sha256(passphrase, &u);
            for (t_byte, u_byte) in t.iter_mut().zip(u.iter()) {
                *t_byte ^= u_byte;
            }
        }
        derived.extend_from_slice(&t);
        block_index = block_index.saturating_add(1);
    }
    derived.truncate(dk_len);
    derived
}

/// Derive a 256-bit AES key from a passphrase and salt using the KDF named by
/// `algo`.
fn derive_key(passphrase: &str, salt: &[u8], iterations: u32, algo: &str) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    match algo {
        KDF_ALGO_LEGACY => out.copy_from_slice(&derive_key_legacy(passphrase, salt, iterations)),
        // Unknown identifiers default to the current scheme: they were written
        // by this code, so anything unrecognized is treated as corrupt-input
        // by the AES-GCM authentication tag anyway.
        _ => out.copy_from_slice(&pbkdf2_hmac_sha256(
            passphrase.as_bytes(),
            salt,
            iterations,
            KEY_LEN,
        )),
    }
    out
}

/// Legacy iterated bare-SHA-256 scheme (superseded by PBKDF2-HMAC-SHA256).
///
/// Security (OP-06): the salt only entered the first round, and plain SHA-256
/// iteration is far cheaper to attack on GPUs than PBKDF2-HMAC. Kept ONLY to
/// decrypt old bundles.
fn derive_key_legacy(passphrase: &str, salt: &[u8], iterations: u32) -> [u8; KEY_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(passphrase.as_bytes());
    let mut key: [u8; 32] = hasher.finalize().into();

    let rounds = iterations.max(1);
    for _ in 1..rounds {
        let mut round_hasher = Sha256::new();
        round_hasher.update(key);
        round_hasher.update(passphrase.as_bytes());
        key = round_hasher.finalize().into();
    }

    key
}

/// Encrypt a [`BackupPayload`] into an encrypted [`BackupBundle`] using `passphrase`.
pub fn encrypt_bundle_payload(payload: &BackupPayload, passphrase: &str) -> Result<BackupBundle> {
    if passphrase.trim().is_empty() {
        return Err(CoreError::Validation("Passphrase cannot be empty".into()));
    }

    let salt: [u8; SALT_LEN] = rand::random();
    let nonce_bytes: [u8; NONCE_LEN] = rand::random();

    let mut key = derive_key(passphrase, &salt, DEFAULT_KDF_ITERATIONS, KDF_ALGO);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| CoreError::Internal(format!("cipher init failed: {e}")))?;
    key.zeroize();

    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|e| CoreError::Internal(format!("invalid nonce: {e}")))?;
    let json_bytes = serde_json::to_vec(payload)
        .map_err(|e| CoreError::Parse(format!("serialize payload: {e}")))?;

    let ciphertext = cipher
        .encrypt(&nonce, json_bytes.as_slice())
        .map_err(|e| CoreError::Internal(format!("backup encryption failed: {e}")))?;

    let now_utc = chrono::Utc::now().to_rfc3339();

    Ok(BackupBundle {
        version: BACKUP_FORMAT_VERSION,
        exported_at: now_utc,
        openproxy_version: env!("CARGO_PKG_VERSION").to_string(),
        encrypted: true,
        kdf: Some(KDF_ALGO.to_string()),
        kdf_salt: Some(BASE64.encode(salt)),
        kdf_iterations: Some(DEFAULT_KDF_ITERATIONS),
        nonce: Some(BASE64.encode(nonce_bytes)),
        ciphertext: Some(BASE64.encode(ciphertext)),
        payload: None,
    })
}

/// Decrypt an encrypted [`BackupBundle`] using `passphrase` or extract the unencrypted payload.
pub fn decrypt_bundle_payload(
    bundle: &BackupBundle,
    passphrase: Option<&str>,
) -> Result<BackupPayload> {
    if !bundle.encrypted {
        return bundle
            .payload
            .clone()
            .ok_or_else(|| CoreError::Validation("Backup payload is missing".into()));
    }

    let Some(passphrase) = passphrase else {
        return Err(CoreError::Validation(
            "Backup bundle is encrypted: passphrase is required".into(),
        ));
    };

    if passphrase.trim().is_empty() {
        return Err(CoreError::Validation(
            "Backup bundle is encrypted: passphrase is required".into(),
        ));
    }

    let Some(salt_b64) = &bundle.kdf_salt else {
        return Err(CoreError::Validation(
            "Missing kdf_salt in encrypted backup".into(),
        ));
    };
    let Some(nonce_b64) = &bundle.nonce else {
        return Err(CoreError::Validation(
            "Missing nonce in encrypted backup".into(),
        ));
    };
    let Some(ciphertext_b64) = &bundle.ciphertext else {
        return Err(CoreError::Validation(
            "Missing ciphertext in encrypted backup".into(),
        ));
    };

    let salt = BASE64
        .decode(salt_b64.trim())
        .map_err(|e| CoreError::Validation(format!("invalid salt base64: {e}")))?;
    let nonce_bytes = BASE64
        .decode(nonce_b64.trim())
        .map_err(|e| CoreError::Validation(format!("invalid nonce base64: {e}")))?;
    let ciphertext = BASE64
        .decode(ciphertext_b64.trim())
        .map_err(|e| CoreError::Validation(format!("invalid ciphertext base64: {e}")))?;

    if nonce_bytes.len() != NONCE_LEN {
        return Err(CoreError::Validation(format!(
            "invalid nonce length: expected {NONCE_LEN}, got {}",
            nonce_bytes.len()
        )));
    }

    let iterations = bundle.kdf_iterations.unwrap_or(DEFAULT_KDF_ITERATIONS);
    let algo = bundle.kdf.as_deref().unwrap_or(KDF_ALGO_LEGACY);

    let mut key = derive_key(passphrase, &salt, iterations, algo);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| CoreError::Internal(format!("cipher init failed: {e}")))?;
    key.zeroize();

    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|e| CoreError::Internal(format!("invalid nonce: {e}")))?;
    let decrypted_bytes = cipher.decrypt(&nonce, ciphertext.as_slice()).map_err(|_| {
        CoreError::Validation(
            "Decryption failed: incorrect passphrase or corrupted backup file".into(),
        )
    })?;

    let payload: BackupPayload = serde_json::from_slice(&decrypted_bytes)
        .map_err(|e| CoreError::Parse(format!("deserialize payload: {e}")))?;

    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8018-style PBKDF2-HMAC-SHA256 test vectors (the de-facto standard
    /// set used across implementations).
    #[test]
    fn test_pbkdf2_hmac_sha256_vectors() {
        fn hex(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }
        assert_eq!(
            hex(&pbkdf2_hmac_sha256(b"password", b"salt", 1, 32)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hex(&pbkdf2_hmac_sha256(b"password", b"salt", 2, 32)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            hex(&pbkdf2_hmac_sha256(b"password", b"salt", 4096, 32)),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    #[test]
    fn test_hmac_sha256_vector() {
        // RFC 4231 test case 1
        let out = hmac_sha256(&[0x0b; 20], b"Hi There");
        let hex: String = out.iter().map(|x| format!("{x:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let payload = BackupPayload {
            providers: vec![],
            accounts: vec![],
            models: vec![],
            combos: vec![],
            combo_targets: vec![],
            proxy_sources: vec![],
            api_keys: vec![],
            app_config: vec![],
        };

        let bundle = encrypt_bundle_payload(&payload, "super-secret-key").unwrap();
        assert!(bundle.encrypted);
        assert_eq!(bundle.kdf.as_deref(), Some(KDF_ALGO));
        assert_eq!(bundle.kdf_iterations, Some(DEFAULT_KDF_ITERATIONS));
        assert!(bundle.ciphertext.is_some());
        assert!(bundle.payload.is_none());

        // Decrypt with correct passphrase
        let decrypted = decrypt_bundle_payload(&bundle, Some("super-secret-key")).unwrap();
        assert_eq!(decrypted, payload);

        // Decrypt with wrong passphrase fails cleanly
        let wrong_err = decrypt_bundle_payload(&bundle, Some("wrong-key"));
        assert!(wrong_err.is_err());

        // Decrypt without passphrase fails cleanly
        let no_pass_err = decrypt_bundle_payload(&bundle, None);
        assert!(no_pass_err.is_err());
    }

    #[test]
    fn test_legacy_sha256_iter_bundle_still_decrypts() {
        // A bundle written by the pre-OP-06 code (iterated bare SHA-256) must
        // remain readable so operators can migrate old backups.
        let payload = BackupPayload {
            providers: vec![],
            accounts: vec![],
            models: vec![],
            combos: vec![],
            combo_targets: vec![],
            proxy_sources: vec![],
            api_keys: vec![],
            app_config: vec![],
        };
        let salt: [u8; SALT_LEN] = rand::random();
        let nonce_bytes: [u8; NONCE_LEN] = rand::random();
        let legacy_iterations = 100_000u32;
        let mut key = derive_key_legacy("old-pass", &salt, legacy_iterations);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        key.zeroize();
        let json_bytes = serde_json::to_vec(&payload).unwrap();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), json_bytes.as_slice())
            .unwrap();

        let legacy_bundle = BackupBundle {
            version: BACKUP_FORMAT_VERSION,
            exported_at: chrono::Utc::now().to_rfc3339(),
            openproxy_version: env!("CARGO_PKG_VERSION").to_string(),
            encrypted: true,
            kdf: Some(KDF_ALGO_LEGACY.to_string()),
            kdf_salt: Some(BASE64.encode(salt)),
            kdf_iterations: Some(legacy_iterations),
            nonce: Some(BASE64.encode(nonce_bytes)),
            ciphertext: Some(BASE64.encode(ciphertext)),
            payload: None,
        };

        let decrypted = decrypt_bundle_payload(&legacy_bundle, Some("old-pass")).unwrap();
        assert_eq!(decrypted, payload);
        assert!(decrypt_bundle_payload(&legacy_bundle, Some("wrong")).is_err());
    }
}
