//! Integration tests for `openproxy_core::secrets`.
//!
//! These tests live in `tests/` so they exercise the crate's public API
//! the way a downstream consumer would.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use openproxy_core::CoreError;
use openproxy_core::secrets::MasterKey;

#[test]
fn encrypt_then_decrypt_roundtrip() {
    let key = MasterKey::generate();
    let blob = key.encrypt("sk-abc-123").unwrap();
    let pt = key.decrypt(&blob).unwrap();
    assert_eq!(pt, "sk-abc-123");
}

#[test]
fn encrypt_produces_different_ciphertexts() {
    let key = MasterKey::generate();
    let a = key.encrypt("x").unwrap();
    let b = key.encrypt("x").unwrap();
    assert_ne!(
        a, b,
        "two encryptions of the same plaintext must differ (random nonce)"
    );
}

#[test]
fn decrypt_with_wrong_key_fails() {
    let a = MasterKey::generate();
    let b = MasterKey::generate();
    let blob = a.encrypt("sk-abc-123").unwrap();
    let res = b.decrypt(&blob);
    assert!(res.is_err(), "wrong key must fail to decrypt");
    // The error should be a CoreError; we don't pin to a specific variant to
    // keep this resilient to internal error mapping changes.
    assert!(matches!(
        res,
        Err(CoreError::Internal(_)) | Err(CoreError::Auth(_))
    ));
}

#[test]
fn from_env_missing_returns_config_error() {
    let prev = std::env::var("OPENPROXY_MASTER_KEY").ok();
    unsafe { std::env::remove_var("OPENPROXY_MASTER_KEY") };
    let res = MasterKey::from_env();
    if let Some(v) = prev {
        unsafe { std::env::set_var("OPENPROXY_MASTER_KEY", v) };
    }
    assert!(
        matches!(res, Err(CoreError::Config(_))),
        "missing env var must produce CoreError::Config"
    );
}

#[test]
fn from_env_wrong_length_returns_config_error() {
    // 16-byte key (must be 32) — wrong length.
    let bad = BASE64.encode([0u8; 16]);
    let prev = std::env::var("OPENPROXY_MASTER_KEY").ok();
    unsafe { std::env::set_var("OPENPROXY_MASTER_KEY", &bad) };
    let res = MasterKey::from_env();
    unsafe { std::env::remove_var("OPENPROXY_MASTER_KEY") };
    if let Some(v) = prev {
        unsafe { std::env::set_var("OPENPROXY_MASTER_KEY", v) };
    }
    assert!(
        matches!(res, Err(CoreError::Config(_))),
        "wrong-length env var must produce CoreError::Config"
    );
}

#[test]
fn from_env_invalid_base64_returns_config_error() {
    // Bonus: not base64 at all.
    let prev = std::env::var("OPENPROXY_MASTER_KEY").ok();
    unsafe { std::env::set_var("OPENPROXY_MASTER_KEY", "not-valid-base64!!!") };
    let res = MasterKey::from_env();
    unsafe { std::env::remove_var("OPENPROXY_MASTER_KEY") };
    if let Some(v) = prev {
        unsafe { std::env::set_var("OPENPROXY_MASTER_KEY", v) };
    }
    assert!(matches!(res, Err(CoreError::Config(_))));
}

#[test]
fn decrypt_truncated_blob_fails() {
    let key = MasterKey::generate();
    // 5 bytes is shorter than the 12-byte nonce prefix.
    let res = key.decrypt(&[0u8; 5]);
    assert!(res.is_err(), "truncated blob must fail to decrypt");
}

#[test]
fn encrypt_then_decrypt_long_key() {
    // A realistic-looking OpenAI-style key.
    let key = MasterKey::generate();
    let api_key = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
    let blob = key.encrypt(api_key).unwrap();
    let pt = key.decrypt(&blob).unwrap();
    assert_eq!(pt, api_key);
}

#[test]
fn blob_layout_is_nonce_then_ciphertext() {
    let key = MasterKey::generate();
    let blob = key.encrypt("hi").unwrap();
    // Nonce is 12 bytes; aes-gcm adds a 16-byte auth tag.
    // So minimal blob is 12 + 2 + 16 = 30 bytes.
    assert!(
        blob.len() >= 12 + 2 + 16,
        "blob too short: {} bytes",
        blob.len()
    );
}
