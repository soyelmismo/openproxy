//! Pinned, hash-verified artifacts. Downloads stream to an atomic disk cache.
//!
//! Mirrors laya_engine::assets: a fixed revision, SHA-256 per artifact, and
//! download-then-hard-link publication so concurrent installs never see a
//! partially written model.

use super::{CoreError, Settings};
#[cfg(any(feature = "upstream-hyper", test))]
use crate::adapters::native_engine_common::{
    Artifact, artifact_error, download_artifact, verify_artifact,
};
use std::path::Path;

#[cfg(feature = "upstream-hyper")]
const REVISION: &str = "daa72c51243991dfcaf9f9137d2c573d8f7790c0";
#[cfg(feature = "upstream-hyper")]
const REPOSITORY: &str = "https://huggingface.co/onnx-community/embeddinggemma-2-ONNX/resolve";
#[cfg(not(feature = "upstream-hyper"))]
const REVISION: &str = "";

const ERR_PREFIX: &str = "Embed artifacts";

const ARTIFACTS: [Artifact; 7] = [
    Artifact {
        name: "model_q4f16.onnx",
        size: 495_298,
        hash: "53feeced79582d661e30adeaa9829cea90d92b1c78347e26fd123773580f71d0",
    },
    Artifact {
        name: "model_q4f16.onnx_data",
        size: 156_862_464,
        hash: "c39fbaa1fb4221f04beb82786a06b81999c514fd39e84fcd58ef79413c87fa56",
    },
    Artifact {
        name: "vision_encoder_q4f16.onnx",
        size: 150_693,
        hash: "55975fba53928162eee37e9eea2a69965c10c0411ed14c24ea149efa55a8aa68",
    },
    Artifact {
        name: "vision_encoder_q4f16.onnx_data",
        size: 97_614_336,
        hash: "842f996aff092e300c966266ae0cc60771c6ca16282fee04ca83f0f159bc6b33",
    },
    Artifact {
        name: "audio_encoder_q4f16.onnx",
        size: 252_538,
        hash: "e61ee0d27a4ee2b13cfa4f6492d6bc9e18d23f998992b31e9c7355be6c028d58",
    },
    Artifact {
        name: "audio_encoder_q4f16.onnx_data",
        size: 170_348_800,
        hash: "356991f24ed4daee4728997a71fe11b72ebf5c053410ac29c7ce27d0f3fc7655",
    },
    Artifact {
        name: "tokenizer.json",
        size: 32_170_510,
        hash: "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4",
    },
];

fn error(e: impl std::fmt::Display) -> CoreError {
    artifact_error(ERR_PREFIX, e)
}

fn cache_dir() -> std::path::PathBuf {
    if let Some(model) = std::env::var_os("OPENPROXY_EMBED_MODEL") {
        let path = std::path::PathBuf::from(model);
        if let Some(parent) = path.parent() {
            return parent.to_path_buf();
        }
    }
    std::path::PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "./models/embeddinggemma2".to_string(),
        |h| format!("{h}/.openproxy/models/embeddinggemma2"),
    ))
}

pub(super) async fn ensure(settings: &Settings) -> Result<(), CoreError> {
    let dir = cache_dir();
    tokio::fs::create_dir_all(&dir).await.map_err(error)?;

    let custom_model =
        settings.model.is_some() || std::env::var_os("OPENPROXY_EMBED_MODEL").is_some();
    let custom_tokenizer =
        settings.tokenizer.is_some() || std::env::var_os("OPENPROXY_EMBED_TOKENIZER").is_some();

    for artifact in &ARTIFACTS {
        let path = dir.join(artifact.name);
        let is_custom = (artifact.name.starts_with("model") && custom_model)
            || (artifact.name == "tokenizer.json" && custom_tokenizer);
        if is_custom && path.file_name().is_some_and(|n| n != artifact.name) {
            continue;
        }
        if tokio::fs::try_exists(&path).await.map_err(error)? {
            verify(&path, artifact).await?;
        } else {
            download(&path, artifact).await?;
        }
    }

    Ok(())
}

async fn verify(path: &Path, artifact: &Artifact) -> Result<(), CoreError> {
    verify_artifact(path, artifact, ERR_PREFIX).await
}

async fn download(path: &Path, artifact: &Artifact) -> Result<(), CoreError> {
    #[cfg(feature = "upstream-hyper")]
    let remote_url = {
        let remote = match artifact.name {
            "tokenizer.json" => artifact.name.to_string(),
            _ => format!("onnx/{}", artifact.name),
        };
        format!("{REPOSITORY}/{REVISION}/{remote}")
    };
    #[cfg(not(feature = "upstream-hyper"))]
    let remote_url = String::new();

    download_artifact(
        path,
        artifact,
        remote_url,
        "OPENPROXY_EMBED_AUTO_DOWNLOAD",
        "automatic downloads disabled; install embed files manually",
        "EmbeddingGemma",
        REVISION,
        ERR_PREFIX,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::native_engine_common::TemporaryFile as Temporary;

    #[tokio::test]
    async fn verification_rejects_corrupt_artifact() {
        let temporary =
            Temporary(std::env::temp_dir().join(format!("embed-hash-{}", uuid::Uuid::new_v4())));
        tokio::fs::write(&temporary.0, b"bad").await.unwrap();
        let artifact = Artifact {
            name: "test",
            size: 3,
            hash: "incorrect",
        };
        assert!(verify(&temporary.0, &artifact).await.is_err());
    }

    #[test]
    fn pinned_hashes_are_lowercase_hex_64() {
        for artifact in &ARTIFACTS {
            assert_eq!(artifact.hash.len(), 64, "{}", artifact.name);
            assert!(
                artifact
                    .hash
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{}",
                artifact.name
            );
            assert!(artifact.size > 0, "{}", artifact.name);
        }
    }

    #[test]
    fn total_matches_the_q4f16_set() {
        let sum: u64 = ARTIFACTS.iter().map(|a| a.size).sum();
        assert_eq!(sum, 457_894_639);
    }
}
