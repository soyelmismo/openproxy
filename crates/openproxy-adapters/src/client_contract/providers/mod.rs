//! Parsers y contratos específicos por proveedor para fuentes release-pinned.
//!
//! Este módulo es una capa separada y trazable. **No** integra los adapters ni
//! el spoofer: los parsers producen datos tipados y verificados que el
//! integrador podrá consumir más adelante.
//!
//! Decisiones de diseño relevantes:
//!
//! * Los catálogos de cada proveedor son **separados y tipados**: no se
//!   mezclan entradas de proveedores distintos ni se deduce routing entre
//!   superficies sin evidencia explícita.
//! * Los parsers extraen **solo** los datos soportados. Los enums desconocidos
//!   se rechazan (`UnknownEnum`) en lugar de relajarse a un valor por defecto.
//! * No se ejecuta código remoto: los bundles minificados solo se inspeccionan
//!   por offsets/regiones verificadas contra un hash previo.
//! * La procedencia (`SourceProvenance`) se modela explícitamente. Si un
//!   artefacto publicado no prueba la correspondencia paquete→commit, el
//!   estado queda como [`PackageCommitCorrespondence::Unverified`] y **no**
//!   se activa identidad solo por metadata.
//!
//! El módulo base (`client_contract/mod.rs`) es propiedad de otro flujo; aquí
//! no se edita. Este submódulo es autocontenido.

pub mod codebuddy;
pub mod codebuddy_runtime;
pub mod codebuddy_state;
pub mod codex;
pub mod codex_runtime;
pub mod codex_state;
pub mod opencode;

pub use codebuddy::{CodeBuddyCatalog, CodeBuddyContract, CodeBuddyModel, CodeBuddyVendor};
pub use codex::{CodexCatalog, CodexModelInfo};
pub use opencode::{OpenCodeCatalog, OpenCodeModelEntry, OpenCodeRouteFamily};

use serde::Deserialize;

/// Nivel de evidencia disponible para una fuente descargada.
///
/// Se conserva explícito para que el integrador pueda decidir si una fuente
/// es suficiente para activar identidad o solo para publicar catálogo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// Metadata de release/tag resuelta desde la API del proveedor.
    ReleaseMetadata,
    /// Manifest de paquete publicado (npm/unpkg).
    PackageManifest,
    /// Hash de bundle publicado.
    BundleHash,
    /// Catálogo servido en runtime por el proveedor.
    RuntimeCatalog,
}

/// Estado de correspondencia entre un artefacto publicado y un commit/tag.
///
/// `Unverified` es un estado **válido y explícito**: nunca se promueve a
/// `Verified` por ausencia de contradicción.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageCommitCorrespondence {
    /// Existe un tag/commit verificable y el artefacto lo referencia.
    Verified {
        /// Tag o rama release usada como pin.
        tag: String,
        /// SHA de commit completo (40 hex).
        commit: String,
    },
    /// No existe campo de procedencia inmutable que ate el paquete al commit.
    Unverified {
        /// Motivo concreto y auditable.
        reason: String,
    },
}

impl PackageCommitCorrespondence {
    /// `true` solo si la correspondencia está realmente verificada.
    ///
    /// La identidad **no** debe activarse basándose únicamente en metadata de
    /// paquete: este método es la compuerta fail-closed para eso.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

/// Referencia a la fuente upstream de un artefacto.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SourceRef {
    /// URL canónica usada para obtener el artefacto.
    pub url: String,
    /// Revisión/tag/versión exacta consultada.
    pub revision: String,
    /// SHA de commit cuando la fuente lo expone.
    pub commit: Option<String>,
    /// Tipo de evidencia disponible.
    pub evidence_kind: EvidenceKind,
}

/// Error de parseo de un contrato de proveedor.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContractParseError {
    /// El payload no es JSON válido.
    #[error("json inválido: {0}")]
    InvalidJson(String),
    /// Falta un campo obligatorio.
    #[error("campo obligatorio ausente: `{0}`")]
    MissingField(&'static str),
    /// Un enum contiene un valor no soportado por el parser.
    #[error("enum desconocido en `{field}`: {value}")]
    UnknownEnum {
        /// Campo donde apareció el valor.
        field: &'static str,
        /// Valor crudo observado.
        value: String,
    },
    /// Un valor numérico o estructural no cumple una invariante.
    #[error("invariante violada: {0}")]
    Invariant(String),
    /// La fuente no permite verificar la correspondencia paquete→commit.
    #[error("correspondencia paquete→commit no verificable: {0}")]
    UnverifiedCorrespondence(String),
}

/// Resultado conveniente para los parsers de este módulo.
pub type ContractResult<T> = Result<T, ContractParseError>;

/// Convierte un error de serde_json en el error tipado del módulo.
pub(crate) fn json_error(err: &serde_json::Error) -> ContractParseError {
    ContractParseError::InvalidJson(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correspondence_fail_closed_on_unverified() {
        let unverified = PackageCommitCorrespondence::Unverified {
            reason: "sin gitHead".to_string(),
        };
        assert!(!unverified.is_verified());
    }

    #[test]
    fn correspondence_verified_requires_both_fields() {
        let verified = PackageCommitCorrespondence::Verified {
            tag: "v2.0.28".to_string(),
            commit: "026b106bc5d4".to_string(),
        };
        assert!(verified.is_verified());
    }
}
