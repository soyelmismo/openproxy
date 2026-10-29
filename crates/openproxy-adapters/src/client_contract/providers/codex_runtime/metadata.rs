use std::sync::Arc;

use serde_json::Value;

use super::{
    ACCEPT_RAW, CODEX_RUNTIME_EXTRACTOR_VERSION, CODEX_RUNTIME_RULE_SET_ID,
    CODEX_WORKSPACE_VERSION, Candidate, ClientIdentity, ContractDriver, ContractError, ContractId,
    EvidenceKind, IDENTITY_ORIGIN_OFFICIAL, SourceDescriptor, SourceRef, decode_utf8, is_json_body,
    parse_version, raw_url, step,
};
pub struct ReleaseMetadata {
    /// Tag de la release.
    pub tag: String,
    /// Versión anunciada, derivada del tag y sin prefijo `v`.
    pub version: String,
    /// Marca de prerelease.
    pub prerelease: bool,
    /// Marca de draft.
    pub draft: bool,
}

/// Versión de cliente derivada de un tag de release.
///
/// Exige la forma estable `rust-vX.Y.Z`: prefijo `rust-v`, semver de tres
/// partes numéricas y canal estable. Un prerelease (`rust-v0.163.0-rc.1`), un
/// tag sin el prefijo o una versión incompleta se rechazan: fail-closed,
/// nunca se adopta por omisión.
///
/// # Errors
/// [`ContractError::Rejected`] si el tag no tiene la forma esperada.
pub fn codex_version_from_tag(tag: &str) -> Result<String, ContractError> {
    let Some(rest) = tag.strip_prefix("rust-v") else {
        return Err(ContractError::Rejected(format!(
            "el tag {tag:?} no tiene la forma estable rust-vX.Y.Z"
        )));
    };
    let parsed = parse_version(rest).map_err(|_| {
        ContractError::Rejected(format!(
            "el tag {tag:?} no declara una versión semver de tres partes"
        ))
    })?;
    if parsed.channel != crate::client_contract::VersionChannel::Stable {
        return Err(ContractError::Rejected(format!(
            "el tag {tag:?} es un prerelease: solo el canal estable se adopta"
        )));
    }
    Ok(parsed.normalized)
}

/// Parsea `releases/latest`. La versión se deriva del `tag_name`; un campo de
/// canal ausente se trata como prerelease/draft: fail-closed, nunca se adopta
/// por omisión.
pub fn parse_release_metadata(body: &[u8]) -> Result<ReleaseMetadata, ContractError> {
    let value = parse_json_object(body, "releases/latest")?;
    let tag = string_field(&value, "tag_name", "releases/latest")?;
    // El campo `name` de la API de GitHub es texto libre editable y no se usa:
    // la versión del contrato sale del tag, que es el identificador estable.
    let version = codex_version_from_tag(&tag)?;
    Ok(ReleaseMetadata {
        tag,
        version,
        prerelease: value
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        draft: value.get("draft").and_then(Value::as_bool).unwrap_or(true),
    })
}

/// Un objeto git (`refs` o `tags`) resuelto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitObject {
    /// Sha del objeto apuntado.
    pub sha: String,
    /// Tipo del objeto apuntado (`tag`, `commit`, …).
    pub object_type: String,
}

/// Parsea `git/refs/...` o `git/tags/...`.
pub fn parse_git_object(body: &[u8], label: &str) -> Result<GitObject, ContractError> {
    let value = parse_json_object(body, label)?;
    let object = value
        .get("object")
        .ok_or_else(|| ContractError::Rejected(format!("{label} sin campo `object`")))?;
    Ok(GitObject {
        sha: string_field(object, "sha", label)?,
        object_type: string_field(object, "type", label)?,
    })
}

/// Objeto JSON o rechazo tipado.
fn parse_json_object(body: &[u8], label: &str) -> Result<Value, ContractError> {
    if !is_json_body(body) {
        return Err(ContractError::Rejected(format!("{label} no devolvió JSON")));
    }
    serde_json::from_slice(body).map_err(|error| ContractError::Encoding(Arc::new(error)))
}

/// Campo de texto obligatorio o rechazo tipado.
fn string_field(value: &Value, field: &str, label: &str) -> Result<String, ContractError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ContractError::Rejected(format!("{label} sin campo `{field}`")))
}

/// El último salto del pin, expresado como [`ContractDriver`] del base.
///
/// El `ContractRefresher` del base es dueño de la petición condicional, de la
/// cache y del parse cuando el caller ya resolvió el commit; este driver solo
/// aporta el descriptor del raw y su lectura.
pub struct CodexPinnedRawDriver {
    commit: String,
    path: String,
}

impl CodexPinnedRawDriver {
    /// Raw de `path` en el commit del pin.
    pub fn new(commit: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            commit: commit.into(),
            path: path.into(),
        }
    }

    /// El descriptor de la fuente, reutilizando el tipo del base.
    pub fn descriptor(&self) -> SourceDescriptor {
        step(
            raw_url(&self.commit, &self.path),
            ACCEPT_RAW,
            EvidenceKind::BundleHash,
            Some(self.commit.clone()),
        )
    }
}

/// `parse` no vuelve a pedir nada: consume los bytes que el
/// [`crate::client_contract::ContractRefresher`] ya adquirió y validó.
impl ContractDriver<String> for CodexPinnedRawDriver {
    fn contract_id(&self) -> ContractId {
        ContractId::Codex
    }

    fn rule_set_id(&self) -> &'static str {
        CODEX_RUNTIME_RULE_SET_ID
    }

    fn extractor_version(&self) -> u32 {
        CODEX_RUNTIME_EXTRACTOR_VERSION
    }

    fn source(&self) -> SourceDescriptor {
        self.descriptor()
    }

    fn parse(
        &self,
        body: &[u8],
        revision: Option<&str>,
        body_digest: &str,
    ) -> Result<Candidate<String>, ContractError> {
        let text = decode_utf8(body, &self.path)?;
        Ok(Candidate::without_catalog(
            ContractId::Codex,
            ClientIdentity::versioned(CODEX_WORKSPACE_VERSION, IDENTITY_ORIGIN_OFFICIAL),
            CODEX_RUNTIME_RULE_SET_ID,
            CODEX_RUNTIME_EXTRACTOR_VERSION,
            vec![SourceRef::pinned(
                raw_url(&self.commit, &self.path),
                revision.unwrap_or(&self.commit),
                body_digest,
                EvidenceKind::BundleHash,
            )],
            text,
        ))
    }
}
