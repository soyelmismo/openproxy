//! Contrato OpenCode: superficies Zen y Go, con catálogos separados y tipados.
//!
//! **Procedencia (fail-closed).** `@opencode/cli@2.0.28` no expone `gitHead`
//! —ninguna de las 1031 versiones publicadas lo hace— y el tarball publicado
//! es un launcher de 2962 bytes sin el commit de build. El tag fuente
//! `v2.0.28` existe y `packages/cli/package.json` en ese tag declara `2.0.28`,
//! pero **ningún campo inmutable ata el artefacto npm a ese commit**. La
//! correspondencia queda como
//! [`PackageCommitCorrespondence::Unverified`]: no se equipara el artefacto de
//! desarrollo con el paquete publicado.
//!
//! **Catálogos separados.** Zen y Go son superficies distintas y se modelan
//! con tipos propios. No se fusionan ni se deduce routing entre ellas sin
//! evidencia explícita.
//!
//! **Routing.** La familia de endpoint se deriva únicamente de la tabla
//! documentada en `services/www/src/docs/content/console/models.mdx` del tag
//! `v2.0.28`. Un modelo sin fila documentada queda
//! [`OpenCodeRouteFamily::Undocumented`]: no se adivina dialecto ni se publica
//! como soportado.

use serde::Deserialize;

use super::{ContractParseError, ContractResult, json_error};

/// Nombre del paquete npm.
pub const OPENCODE_PACKAGE: &str = "@opencode/cli";

/// Versión fijada del paquete npm.
pub const OPENCODE_NPM_VERSION: &str = "2.0.28";

/// Tag fuente usado como pin de la revisión inspeccionada.
pub const OPENCODE_SOURCE_TAG: &str = "v2.0.28";

/// Familia de endpoint/routing de un modelo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpenCodeRouteFamily {
    /// API Responses de OpenAI.
    Responses,
    /// API Messages de Anthropic.
    Messages,
    /// API Chat Completions de OpenAI.
    ChatCompletions,
    /// Endpoint propio `/v1/systemone`.
    SystemOne,
    /// Endpoint nativo de Gemini (`/v1/models/<id>`).
    Gemini,
    /// Sin fila documentada en la revisión fijada.
    Undocumented,
}

impl OpenCodeRouteFamily {
    /// Nombre legible de la familia.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::ChatCompletions => "chat_completions",
            Self::SystemOne => "systemone",
            Self::Gemini => "gemini",
            Self::Undocumented => "undocumented",
        }
    }
}

/// Entrada de un catálogo runtime de OpenCode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCodeModelEntry {
    /// Identificador del modelo.
    pub id: String,
    /// Tipo de objeto declarado por la API (`model`).
    pub object: String,
    /// Timestamp de creación declarado.
    pub created: u64,
    /// Owner declarado (`opencode`).
    pub owned_by: String,
}

/// Catálogo runtime de una superficie OpenCode.
///
/// Zen y Go usan este mismo tipo porque su payload runtime es idéntico, pero
/// **se mantienen como catálogos separados**: [`OpenCodeCatalog::surface`]
/// los distingue y nunca se mezclan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCodeCatalog {
    /// Superficie a la que pertenece el catálogo.
    pub surface: OpenCodeSurface,
    /// Entradas del catálogo.
    pub models: Vec<OpenCodeModelEntry>,
}

/// Superficie de inferencia de OpenCode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpenCodeSurface {
    /// Catálogo `/inference/v1/models`.
    Zen,
    /// Catálogo `/inference/go/v1/models`.
    Go,
}

impl OpenCodeSurface {
    /// Nombre legible de la superficie.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Zen => "zen",
            Self::Go => "go",
        }
    }
}

impl OpenCodeCatalog {
    /// Parsea un catálogo runtime (`GET .../models`).
    ///
    /// # Errors
    /// Devuelve un error si el JSON es inválido o falta un campo obligatorio.
    pub fn parse(surface: OpenCodeSurface, json: &str) -> ContractResult<Self> {
        #[derive(Deserialize)]
        struct RawCatalog {
            data: Vec<serde_json::Value>,
        }

        let raw: RawCatalog = serde_json::from_str(json).map_err(|e| json_error(&e))?;
        let mut models = Vec::with_capacity(raw.data.len());

        for entry in &raw.data {
            let id = entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("id"))?
                .to_string();
            let object = entry
                .get("object")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("object"))?
                .to_string();
            let created = entry
                .get("created")
                .and_then(serde_json::Value::as_u64)
                .ok_or(ContractParseError::MissingField("created"))?;
            let owned_by = entry
                .get("owned_by")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("owned_by"))?
                .to_string();

            models.push(OpenCodeModelEntry {
                id,
                object,
                created,
                owned_by,
            });
        }

        Ok(Self { surface, models })
    }

    /// Busca una entrada por id.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&OpenCodeModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Familia de routing de un modelo según la tabla documentada.
    ///
    /// # Errors
    /// No devuelve error: un modelo ausente de la tabla es
    /// [`OpenCodeRouteFamily::Undocumented`], nunca una familia adivinada.
    #[must_use]
    pub fn route_family(&self, id: &str) -> OpenCodeRouteFamily {
        OPENCODE_DOCUMENTED_ROUTES
            .iter()
            .find(|(mid, _, _)| *mid == id)
            .map_or(OpenCodeRouteFamily::Undocumented, |(_, fam, _)| *fam)
    }

    /// Endpoint documentado para un modelo, si existe.
    #[must_use]
    pub fn documented_endpoint(&self, id: &str) -> Option<&'static str> {
        OPENCODE_DOCUMENTED_ROUTES
            .iter()
            .find(|(mid, _, _)| *mid == id)
            .map(|(_, _, url)| *url)
    }

    /// Modelos del catálogo sin fila documentada.
    ///
    /// Se devuelven explícitamente para que el integrador los trate como
    /// pendientes en lugar de asumir un dialecto.
    #[must_use]
    pub fn undocumented(&self) -> Vec<&str> {
        self.models
            .iter()
            .filter(|m| self.route_family(&m.id) == OpenCodeRouteFamily::Undocumented)
            .map(|m| m.id.as_str())
            .collect()
    }
}

/// Fila de routing documentada en `models.mdx` del tag `v2.0.28`.
pub type OpenCodeRouteRow = (&'static str, OpenCodeRouteFamily, &'static str);

/// Tabla de routing documentada (89 modelos con familia conocida).
///
/// Derivada de la tabla de `services/www/src/docs/content/console/models.mdx`.
/// Una fila de separación de la tabla markdown (`----------`) se descarta.
pub const OPENCODE_DOCUMENTED_ROUTES: &[OpenCodeRouteRow] = &[
    (
        "big-pickle",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "cat-jam-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "claude-fable-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-fable-5-1",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-haiku-4-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-haiku-5-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-opus-4-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-opus-4-6",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-opus-4-7",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-opus-4-8",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-opus-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-opus-5-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-sonnet-4-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-sonnet-4-6",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-sonnet-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "claude-sonnet-5-5",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "deepseek-v4-flash",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "deepseek-v4-flash-vision-exp",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "deepseek-v4-pro",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "deepseek-v4.1-flash",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "exo-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "gemini-3-flash",
        OpenCodeRouteFamily::Gemini,
        "https://opencode.ai/inference/v1/models/gemini-3-flash",
    ),
    (
        "gemini-3.1-pro",
        OpenCodeRouteFamily::Gemini,
        "https://opencode.ai/inference/v1/models/gemini-3.1-pro",
    ),
    (
        "gemini-3.5-flash",
        OpenCodeRouteFamily::Gemini,
        "https://opencode.ai/inference/v1/models/gemini-3.5-flash",
    ),
    (
        "gemini-3.5-flash-lite",
        OpenCodeRouteFamily::Gemini,
        "https://opencode.ai/inference/v1/models/gemini-3.5-flash-lite",
    ),
    (
        "gemini-3.6-flash",
        OpenCodeRouteFamily::Gemini,
        "https://opencode.ai/inference/v1/models/gemini-3.6-flash",
    ),
    (
        "gemini-3.7-flash",
        OpenCodeRouteFamily::Gemini,
        "https://opencode.ai/inference/v1/models/gemini-3.7-flash",
    ),
    (
        "gemini-3.8-flash",
        OpenCodeRouteFamily::Gemini,
        "https://opencode.ai/inference/v1/models/gemini-3.8-flash",
    ),
    (
        "glm-5",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "glm-5.1",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "glm-5.2",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "glm-5.3",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "glm-5.3-flash",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "gpt-5",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5-codex",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5-nano",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.1",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.1-codex",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.1-codex-max",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.1-codex-mini",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.2",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.2-codex",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.3-codex",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.3-codex-spark",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.4",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.4-mini",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.4-nano",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.4-pro",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.5",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.5-pro",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.6-luna",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.6-sol",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-5.6-terra",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-6-astra",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-6-luna",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-6-sol",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "gpt-6.1-sol",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "grok-4.5",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "grok-4.6",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "grok-4.7",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "grok-build-0.1",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "jev-1.13",
        OpenCodeRouteFamily::SystemOne,
        "https://opencode.ai/inference/v1/systemone",
    ),
    (
        "jev-1.13-free",
        OpenCodeRouteFamily::SystemOne,
        "https://opencode.ai/inference/v1/systemone",
    ),
    (
        "kimi-k2.5",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "kimi-k2.6",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "kimi-k2.7-code",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "kimi-k3",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "ling-3.0-flash-fin-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "ling-3.1-flash-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "longcat-2.5-preview-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "mimo-v2.5-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "mimo-v2.6-flash-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "minimax-m2.5",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "minimax-m2.7",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "minimax-m3",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "mistral-large-4",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "muse-spark-1.2",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "muse-spark-1.3",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "muse-spark-1.3-contributor-free",
        OpenCodeRouteFamily::Responses,
        "https://opencode.ai/inference/v1/responses",
    ),
    (
        "nemotron-3-ultra-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "nemotron-3.5-lightning-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "qwen3.5-plus",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "qwen3.6-plus",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "qwen3.7-max",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "qwen3.7-plus",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "qwen3.8-flash",
        OpenCodeRouteFamily::Messages,
        "https://opencode.ai/inference/v1/messages",
    ),
    (
        "qwen3.8-max",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "space-bunny-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
    (
        "step-5-preview-free",
        OpenCodeRouteFamily::ChatCompletions,
        "https://opencode.ai/inference/v1/chat/completions",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn zen() -> &'static str {
        include_str!("../../../../../fixtures/opencode-runtime/zen-models.json")
    }

    fn go() -> &'static str {
        include_str!("../../../../../fixtures/opencode-runtime/go-models.json")
    }

    #[test]
    fn catalogs_are_separate_and_typed() -> ContractResult<()> {
        let zen = OpenCodeCatalog::parse(OpenCodeSurface::Zen, zen())?;
        let go = OpenCodeCatalog::parse(OpenCodeSurface::Go, go())?;
        assert_eq!(zen.surface, OpenCodeSurface::Zen);
        assert_eq!(go.surface, OpenCodeSurface::Go);
        assert_eq!(zen.models.len(), 86);
        assert_eq!(go.models.len(), 39);
        Ok(())
    }

    #[test]
    fn undocumented_models_are_not_guessed() -> ContractResult<()> {
        let zen = OpenCodeCatalog::parse(OpenCodeSurface::Zen, zen())?;
        assert_eq!(
            zen.route_family("gpt-6-sol"),
            OpenCodeRouteFamily::Responses
        );
        assert_eq!(
            zen.route_family("modelo-que-no-existe"),
            OpenCodeRouteFamily::Undocumented
        );
        assert!(!zen.undocumented().is_empty());
        Ok(())
    }

    #[test]
    fn every_documented_route_has_a_known_family() {
        for (id, family, url) in OPENCODE_DOCUMENTED_ROUTES {
            assert_ne!(*family, OpenCodeRouteFamily::Undocumented);
            assert!(!id.is_empty());
            assert!(url.starts_with("https://opencode.ai/inference/"));
        }
    }

    #[test]
    fn route_table_covers_most_of_zen() -> ContractResult<()> {
        let zen = OpenCodeCatalog::parse(OpenCodeSurface::Zen, zen())?;
        let documented = zen
            .models
            .iter()
            .filter(|m| zen.route_family(&m.id) != OpenCodeRouteFamily::Undocumented)
            .count();
        assert!(documented >= 80, "documentados: {documented}");
        Ok(())
    }
}
