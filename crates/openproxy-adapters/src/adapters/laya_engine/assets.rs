//! Pinned, hash-verified artifacts. Downloads stream to an atomic disk cache.
use super::{CoreError, lifecycle::Settings};
use sha2::{Digest, Sha256};
use std::path::Path;
#[cfg(any(feature = "upstream-hyper", test))]
use std::path::PathBuf;
use tokio::io::AsyncReadExt;
#[cfg(feature = "upstream-hyper")]
use tokio::io::AsyncWriteExt;

#[cfg(feature = "upstream-hyper")]
const REVISION: &str = "0966c4fa58da6878b39e7e14cb5e93313b82d828";
#[cfg(feature = "upstream-hyper")]
const REPOSITORY: &str = "https://huggingface.co/soyelmismo/laya-multilingual-onnx/resolve";

struct Artifact {
    name: &'static str,
    size: u64,
    hash: &'static str,
}

const ARTIFACTS: [Artifact; 3] = [
    Artifact {
        name: "model.onnx",
        size: 325734062,
        hash: "d389d2304822a59569387e257067360a84e016aed43b407f1cfde87dadb7e485",
    },
    Artifact {
        name: "tokenizer.json",
        size: 34363188,
        hash: "609d8f4c067cd3950f88594c5a802616cea245823836ef5848ee4fc40aab5b6f",
    },
    Artifact {
        name: "rl_agent_config.json",
        size: 473,
        hash: "9a669a70961064c3c6cc76d2afb8bc5fb10dcd8349bb66e5f7b9b1afb74440d5",
    },
];

fn error(e: impl std::fmt::Display) -> CoreError {
    CoreError::Internal(format!("Laya artifacts: {e}"))
}

pub(super) async fn ensure(settings: &Settings) -> Result<(), CoreError> {
    let paths = [
        super::resolve_model_path(settings.model.as_deref()),
        super::resolve_tokenizer_path(settings.tokenizer.as_deref()),
        super::resolve_config_path(settings.config.as_deref()),
    ];
    let custom = [
        settings.model.is_some() || std::env::var_os("OPENPROXY_LAYA_MODEL").is_some(),
        settings.tokenizer.is_some() || std::env::var_os("OPENPROXY_LAYA_TOKENIZER").is_some(),
        settings.config.is_some() || std::env::var_os("OPENPROXY_LAYA_CONFIG").is_some(),
    ];
    for ((artifact, path), custom) in ARTIFACTS.iter().zip(paths).zip(custom) {
        let path = Path::new(&path);
        if custom || path.file_name().is_some_and(|name| name != artifact.name) {
            // Explicit custom models/configs remain supported, but are never overwritten.
            if !tokio::fs::try_exists(path).await.map_err(error)?
                && artifact.name != "rl_agent_config.json"
            {
                return Err(error(format!(
                    "custom file {} does not exist",
                    path.display()
                )));
            }
            continue;
        }
        if tokio::fs::try_exists(path).await.map_err(error)? {
            verify(path, artifact).await?;
        } else {
            download(path, artifact).await?;
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
    if std::env::var("OPENPROXY_LAYA_AUTO_DOWNLOAD").is_ok_and(|v| v == "0" || v == "false") {
        return Err(error(
            "automatic downloads disabled; install Laya files manually",
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
        "Downloading Laya on demand"
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
        // Publish without replacing a concurrently-installed file.
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
    let mut url = format!("{REPOSITORY}/{REVISION}/{}", artifact.name);
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
            Temporary(std::env::temp_dir().join(format!("laya-hash-{}", uuid::Uuid::new_v4())));
        tokio::fs::write(&temporary.0, b"bad").await.unwrap();
        let artifact = Artifact {
            name: "test",
            size: 3,
            hash: "incorrect",
        };
        assert!(verify(&temporary.0, &artifact).await.is_err());
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    #[ignore = "requires access to the pinned Hugging Face artifact"]
    async fn download_verifies_and_publishes_pinned_config() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temporary =
            Temporary(std::env::temp_dir().join(format!("laya-download-{}", uuid::Uuid::new_v4())));
        download(&temporary.0, &ARTIFACTS[2]).await.unwrap();
        verify(&temporary.0, &ARTIFACTS[2]).await.unwrap();
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    #[ignore = "downloads the 325 MB pinned model from Hugging Face"]
    async fn model_download_streams_redirected_artifact() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temporary = Temporary(
            std::env::temp_dir().join(format!("laya-model-download-{}", uuid::Uuid::new_v4())),
        );
        download(&temporary.0, &ARTIFACTS[0]).await.unwrap();
        verify(&temporary.0, &ARTIFACTS[0]).await.unwrap();
    }
}
