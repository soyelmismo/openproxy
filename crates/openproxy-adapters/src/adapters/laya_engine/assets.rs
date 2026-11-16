//! Pinned, hash-verified artifacts. Downloads stream to an atomic disk cache.
use super::{CoreError, lifecycle::Settings};
#[cfg(any(feature = "upstream-hyper", test))]
use crate::adapters::native_engine_common::TemporaryFile as Temporary;
use crate::adapters::native_engine_common::{
    Artifact, artifact_error, download_artifact, verify_artifact,
};
use std::path::Path;

#[cfg(feature = "upstream-hyper")]
const REVISION: &str = "0966c4fa58da6878b39e7e14cb5e93313b82d828";
#[cfg(feature = "upstream-hyper")]
const REPOSITORY: &str = "https://huggingface.co/soyelmismo/laya-multilingual-onnx/resolve";
#[cfg(not(feature = "upstream-hyper"))]
const REVISION: &str = "";

const ERR_PREFIX: &str = "Laya artifacts";

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
    artifact_error(ERR_PREFIX, e)
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
    verify_artifact(path, artifact, ERR_PREFIX).await
}

async fn download(path: &Path, artifact: &Artifact) -> Result<(), CoreError> {
    #[cfg(feature = "upstream-hyper")]
    let remote_url = format!("{REPOSITORY}/{REVISION}/{}", artifact.name);
    #[cfg(not(feature = "upstream-hyper"))]
    let remote_url = String::new();

    download_artifact(
        path,
        artifact,
        remote_url,
        "OPENPROXY_LAYA_AUTO_DOWNLOAD",
        "automatic downloads disabled; install Laya files manually",
        "Laya",
        REVISION,
        ERR_PREFIX,
    )
    .await
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
