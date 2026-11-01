//! Pinned, hash-verified artifacts. Downloads stream to an atomic disk cache.
//!
//! Mirrors laya_engine::assets: a fixed revision, SHA-256 per artifact, and
//! download-then-hard-link publication so concurrent installs never see a
//! partially written model.
//!
//! The engine is multimodal: a text backbone plus vision and audio encoders
//! that project into the same 768-dim space. All three ship as `q4f16`
//! (4-bit weights, fp16 activations) because quantizing activations to int4
//! damages the vector geometry far more in vision and audio than in text.

use super::{CoreError, Settings};
use sha2::{Digest, Sha256};
use std::path::Path;
#[cfg(any(feature = "upstream-hyper", test))]
use std::path::PathBuf;
use tokio::io::AsyncReadExt;
#[cfg(feature = "upstream-hyper")]
use tokio::io::AsyncWriteExt;

#[cfg(feature = "upstream-hyper")]
const REVISION: &str = "daa72c51243991dfcaf9f9137d2c573d8f7790c0";
#[cfg(feature = "upstream-hyper")]
const REPOSITORY: &str = "https://huggingface.co/onnx-community/embeddinggemma-2-ONNX/resolve";

struct Artifact {
    name: &'static str,
    size: u64,
    hash: &'static str,
}

/// Files land in the cache root; every graph sits next to its `.onnx_data`
/// external weights, which is what ONNX Runtime requires.
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
    CoreError::Internal(format!("Embed artifacts: {e}"))
}

/// Cache directory for the engine artifacts.
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

/// Download and verify every artifact into the cache root.
pub(super) async fn ensure(settings: &Settings) -> Result<(), CoreError> {
    let dir = cache_dir();
    tokio::fs::create_dir_all(&dir).await.map_err(error)?;

    // Custom paths are honored but never overwritten.
    let custom_model =
        settings.model.is_some() || std::env::var_os("OPENPROXY_EMBED_MODEL").is_some();
    let custom_tokenizer =
        settings.tokenizer.is_some() || std::env::var_os("OPENPROXY_EMBED_TOKENIZER").is_some();

    for artifact in &ARTIFACTS {
        let path = dir.join(artifact.name);
        let is_custom = (artifact.name.starts_with("model") && custom_model)
            || (artifact.name == "tokenizer.json" && custom_tokenizer);
        if is_custom {
            // A user-supplied model already resolves through its own path;
            // the sibling data file lands next to it, so only verify if the
            // canonical name matches.
            if path.file_name().is_some_and(|n| n != artifact.name) {
                continue;
            }
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
    let mut file = tokio::fs::File::open(path).await.map_err(error)?;
    if file.metadata().await.map_err(error)?.len() != artifact.size {
        return Err(error(format!("{} has an unexpected size", path.display())));
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let len = file.read(&mut buffer).await.map_err(error)?;
        if len == 0 {
            break;
        }
        hash.update(&buffer[..len]);
    }
    verify_digest(hash, artifact)
}

fn verify_digest(hash: Sha256, artifact: &Artifact) -> Result<(), CoreError> {
    let actual: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != artifact.hash {
        return Err(error(format!(
            "SHA-256 mismatch for {}; refusing to load",
            artifact.name
        )));
    }
    Ok(())
}

#[cfg(any(feature = "upstream-hyper", test))]
struct Temporary(PathBuf);
#[cfg(any(feature = "upstream-hyper", test))]
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(feature = "upstream-hyper")]
async fn download(path: &Path, artifact: &Artifact) -> Result<(), CoreError> {
    if std::env::var("OPENPROXY_EMBED_AUTO_DOWNLOAD").is_ok_and(|v| v == "0" || v == "false") {
        return Err(error(
            "automatic downloads disabled; install embed files manually",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| error("missing cache directory"))?;
    tokio::fs::create_dir_all(parent).await.map_err(error)?;
    let temporary =
        Temporary(parent.join(format!(".{}.{}.part", artifact.name, uuid::Uuid::new_v4())));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary.0).await.map_err(error)?;
    tracing::info!(
        artifact = artifact.name,
        revision = REVISION,
        "Downloading EmbeddingGemma on demand"
    );
    tokio::time::timeout(std::time::Duration::from_secs(600), async {
        let mut response = fetch(artifact).await?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = response.body.next_chunk().await.map_err(error)? {
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| error("size overflow"))?;
            if size > artifact.size {
                return Err(error("download exceeds pinned size"));
            }
            hash.update(&chunk);
            file.write_all(&chunk).await.map_err(error)?;
        }
        if size != artifact.size {
            return Err(error("incomplete artifact download"));
        }
        verify_digest(hash, artifact)?;
        file.sync_all().await.map_err(error)?;
        drop(file);
        tokio::fs::hard_link(&temporary.0, path)
            .await
            .map_err(error)?;
        Ok(())
    })
    .await
    .map_err(|_| error("download timed out"))?
}

#[cfg(not(feature = "upstream-hyper"))]
async fn download(_path: &Path, _artifact: &Artifact) -> Result<(), CoreError> {
    Err(error("automatic downloads require upstream-hyper"))
}

#[cfg(feature = "upstream-hyper")]
async fn fetch(artifact: &Artifact) -> Result<crate::upstream::UpstreamResponse, CoreError> {
    use crate::upstream::{
        CancellationToken, ResolvedTimeouts, TimeoutProfile, UpstreamClient, UpstreamRequest,
    };
    let client = UpstreamClient::new();
    // HF stores LFS payloads under `onnx/`; each graph and its weights must be
    // fetched from the same directory the session will read.
    let remote = match artifact.name {
        "tokenizer.json" => artifact.name.to_string(),
        _ => format!("onnx/{}", artifact.name),
    };
    let mut url = format!("{REPOSITORY}/{REVISION}/{remote}");
    for _ in 0..6 {
        let uri: http::Uri = url.parse().map_err(error)?;
        if uri.scheme_str() != Some("https") {
            return Err(error("insecure artifact redirect"));
        }
        let response = client
            .call(
                UpstreamRequest::get(&url),
                TimeoutProfile::Custom(ResolvedTimeouts {
                    headers_ms: 30000,
                    body_chunk_ms: 60000,
                    total_ms: 600000,
                    ..ResolvedTimeouts::SYSTEM_DEFAULTS
                }),
                CancellationToken::new(),
            )
            .await
            .map_err(error)?;
        if response.status.is_success() {
            return Ok(response);
        }
        if !response.status.is_redirection() {
            return Err(error(format!("download HTTP {}", response.status)));
        }
        let location = response
            .headers
            .get(http::header::LOCATION)
            .ok_or_else(|| error("redirect without location"))?
            .to_str()
            .map_err(error)?;
        url = if location.starts_with('/') {
            format!(
                "https://{}{location}",
                uri.authority().ok_or_else(|| error("missing host"))?
            )
        } else {
            location.to_owned()
        };
    }
    Err(error("too many artifact redirects"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 150 MiB text + 93 MiB vision + 162 MiB audio + tokenizer.
        let sum: u64 = ARTIFACTS.iter().map(|a| a.size).sum();
        assert_eq!(sum, 457_894_639);
    }
}
