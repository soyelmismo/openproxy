//! Contrato Codex fijado a la release estable `rust-v0.162.1`.
//!
//! El pin es un **annotated tag** (`refs/tags/rust-v0.162.1`) desreferenciado a
//! su commit, no un `target_commitish` flotante. La versión del workspace
//! (`codex-rs/Cargo.toml`, `[workspace.package] version`) coincide con la
//! release, lo que permite verificar que el catálogo inspeccionado
//! pertenece a esa revisión exacta y no a `main`.
//!
//! Los valores derivados de lógica imperativa (por ejemplo
//! `store: false`, `stream: true`, `tool_choice: "auto"`) se modelan como
//! constantes verificadas por guards de región, **no** se recalculan aquí.

use serde::Deserialize;

use super::{ContractParseError, ContractResult, json_error};

/// Tag de release usado como pin.
pub const CODEX_RELEASE_TAG: &str = "rust-v0.162.1";

/// Versión declarada en `[workspace.package]` de `codex-rs/Cargo.toml`.
pub const CODEX_WORKSPACE_VERSION: &str = "0.162.1";

/// Visibilidad de un modelo en el catálogo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexVisibility {
    /// Visible en la lista pública.
    List,
    /// Oculto del listado.
    Hide,
}

impl CodexVisibility {
    /// Parsea un valor crudo rechazando enums desconocidos.
    ///
    /// # Errors
    /// Devuelve [`ContractParseError::UnknownEnum`] si el valor no está
    /// soportado por el parser.
    pub fn from_raw(field: &'static str, value: &str) -> ContractResult<Self> {
        match value {
            "list" => Ok(Self::List),
            "hide" => Ok(Self::Hide),
            other => Err(ContractParseError::UnknownEnum {
                field,
                value: other.to_string(),
            }),
        }
    }
}

/// Modalidad de entrada soportada por un modelo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexInputModality {
    /// Entrada de texto.
    Text,
    /// Entrada de imagen.
    Image,
}

impl CodexInputModality {
    /// Parsea un valor crudo rechazando enums desconocidos.
    ///
    /// # Errors
    /// Devuelve [`ContractParseError::UnknownEnum`] si el valor no está
    /// soportado por el parser.
    pub fn from_raw(field: &'static str, value: &str) -> ContractResult<Self> {
        match value {
            "text" => Ok(Self::Text),
            "image" => Ok(Self::Image),
            other => Err(ContractParseError::UnknownEnum {
                field,
                value: other.to_string(),
            }),
        }
    }
}

/// Nivel de esfuerzo de razonamiento.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexReasoningEffort {
    /// `low`
    Low,
    /// `medium`
    Medium,
    /// `high`
    High,
    /// `xhigh`
    Xhigh,
    /// `max`
    Max,
    /// `ultra`
    Ultra,
}

impl CodexReasoningEffort {
    /// Parsea un valor crudo rechazando enums desconocidos.
    ///
    /// # Errors
    /// Devuelve [`ContractParseError::UnknownEnum`] si el valor no está
    /// soportado por el parser.
    pub fn from_raw(field: &'static str, value: &str) -> ContractResult<Self> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            "ultra" => Ok(Self::Ultra),
            other => Err(ContractParseError::UnknownEnum {
                field,
                value: other.to_string(),
            }),
        }
    }
}

/// Verbatim solicitado por defecto.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexVerbosity {
    /// `low`
    Low,
    /// `high`
    High,
}

impl CodexVerbosity {
    /// Parsea un valor crudo rechazando enums desconocidos.
    ///
    /// # Errors
    /// Devuelve [`ContractParseError::UnknownEnum`] si el valor no está
    /// soportado por el parser.
    pub fn from_raw(field: &'static str, value: &str) -> ContractResult<Self> {
        match value {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            other => Err(ContractParseError::UnknownEnum {
                field,
                value: other.to_string(),
            }),
        }
    }
}

/// Nivel de razonamiento con su descripción tal como la publica el catálogo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexReasoningLevel {
    /// Nivel de esfuerzo.
    pub effort: CodexReasoningEffort,
    /// Descripción publicada por upstream.
    pub description: String,
}

/// Tier de servicio anunciado en el catálogo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexServiceTier {
    /// Identificador del tier.
    pub id: String,
    /// Nombre legible.
    pub name: String,
    /// Descripción publicada por upstream.
    pub description: String,
}

/// Política de truncado declarada por el modelo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexTruncationPolicy {
    /// Modo de truncado (`tokens` en `0.162.1`).
    pub mode: String,
    /// Límite en tokens.
    pub limit: u64,
}

/// Entrada del catálogo Codex con los campos soportados por el parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexModelInfo {
    /// Slug del modelo.
    pub slug: String,
    /// Nombre para mostrar.
    pub display_name: String,
    /// Descripción.
    pub description: String,
    /// Context window soportado.
    pub context_window: u64,
    /// Context window máximo.
    pub max_context_window: u64,
    /// Visibilidad.
    pub visibility: CodexVisibility,
    /// Si el modelo es usable vía API.
    pub supported_in_api: bool,
    /// Versión mínima de cliente requerida.
    pub minimal_client_version: String,
    /// Prioridad de orden.
    pub priority: i64,
    /// Modalidades de entrada.
    pub input_modalities: Vec<CodexInputModality>,
    /// Niveles de razonamiento soportados.
    pub supported_reasoning_levels: Vec<CodexReasoningLevel>,
    /// Nivel de razonamiento por defecto.
    pub default_reasoning_level: CodexReasoningEffort,
    /// Resumen de razonamiento por defecto.
    pub default_reasoning_summary: String,
    /// Si soporta verbosity.
    pub support_verbosity: bool,
    /// Verbosity por defecto.
    pub default_verbosity: CodexVerbosity,
    /// Tiers de servicio soportados.
    pub service_tiers: Vec<CodexServiceTier>,
    /// Tier de servicio por defecto.
    pub default_service_tier: Option<String>,
    /// Si soporta `parallel_tool_calls`.
    pub supports_parallel_tool_calls: bool,
    /// Si usa la variante `responses_lite`.
    pub use_responses_lite: bool,
    /// Si prefiere transporte websocket.
    pub prefer_websockets: bool,
    /// Política de truncado.
    pub truncation_policy: CodexTruncationPolicy,
}

/// Catálogo Codex completo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexCatalog {
    /// Modelos en el orden publicado por upstream.
    pub models: Vec<CodexModelInfo>,
}

impl CodexCatalog {
    /// Parsea `models-manager/models.json` de la release fijada.
    ///
    /// Extrae únicamente los campos soportados; los campos desconocidos se
    /// ignoran sin relajar los enums tipados.
    ///
    /// # Errors
    /// Devuelve un error si el JSON es inválido, falta un campo obligatorio o
    /// aparece un valor de enum no soportado.
    pub fn parse(json: &str) -> ContractResult<Self> {
        #[derive(Deserialize)]
        struct RawCatalog {
            models: Vec<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct RawReasoningLevel {
            effort: String,
            description: String,
        }
        #[derive(Deserialize)]
        struct RawServiceTier {
            id: String,
            name: String,
            description: String,
        }
        #[derive(Deserialize)]
        struct RawTruncation {
            mode: String,
            limit: u64,
        }

        let raw: RawCatalog = serde_json::from_str(json).map_err(|e| json_error(&e))?;
        let mut models = Vec::with_capacity(raw.models.len());

        for entry in &raw.models {
            let slug = entry
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("slug"))?
                .to_string();
            let display_name = entry
                .get("display_name")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("display_name"))?
                .to_string();
            let description = entry
                .get("description")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("description"))?
                .to_string();
            let context_window = entry
                .get("context_window")
                .and_then(serde_json::Value::as_u64)
                .ok_or(ContractParseError::MissingField("context_window"))?;
            let max_context_window = entry
                .get("max_context_window")
                .and_then(serde_json::Value::as_u64)
                .ok_or(ContractParseError::MissingField("max_context_window"))?;
            let visibility = CodexVisibility::from_raw(
                "visibility",
                entry
                    .get("visibility")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(ContractParseError::MissingField("visibility"))?,
            )?;
            let supported_in_api = entry
                .get("supported_in_api")
                .and_then(serde_json::Value::as_bool)
                .ok_or(ContractParseError::MissingField("supported_in_api"))?;
            let minimal_client_version = entry
                .get("minimal_client_version")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("minimal_client_version"))?
                .to_string();
            let priority = entry
                .get("priority")
                .and_then(serde_json::Value::as_i64)
                .ok_or(ContractParseError::MissingField("priority"))?;

            let input_modalities = entry
                .get("input_modalities")
                .and_then(serde_json::Value::as_array)
                .ok_or(ContractParseError::MissingField("input_modalities"))?
                .iter()
                .map(|v| {
                    CodexInputModality::from_raw(
                        "input_modalities",
                        v.as_str()
                            .ok_or(ContractParseError::MissingField("input_modalities[]"))?,
                    )
                })
                .collect::<ContractResult<Vec<_>>>()?;

            let raw_levels: Vec<RawReasoningLevel> = entry
                .get("supported_reasoning_levels")
                .cloned()
                .ok_or(ContractParseError::MissingField(
                    "supported_reasoning_levels",
                ))
                .and_then(|v| {
                    serde_json::from_value(v).map_err(|e| {
                        ContractParseError::Invariant(format!(
                            "supported_reasoning_levels no parseable: {e}"
                        ))
                    })
                })?;
            let mut supported_reasoning_levels = Vec::with_capacity(raw_levels.len());
            for level in raw_levels {
                supported_reasoning_levels.push(CodexReasoningLevel {
                    effort: CodexReasoningEffort::from_raw(
                        "supported_reasoning_levels[].effort",
                        &level.effort,
                    )?,
                    description: level.description,
                });
            }

            let default_reasoning_level = CodexReasoningEffort::from_raw(
                "default_reasoning_level",
                entry
                    .get("default_reasoning_level")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(ContractParseError::MissingField("default_reasoning_level"))?,
            )?;
            let default_reasoning_summary = entry
                .get("default_reasoning_summary")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField(
                    "default_reasoning_summary",
                ))?
                .to_string();
            let support_verbosity = entry
                .get("support_verbosity")
                .and_then(serde_json::Value::as_bool)
                .ok_or(ContractParseError::MissingField("support_verbosity"))?;
            let default_verbosity = CodexVerbosity::from_raw(
                "default_verbosity",
                entry
                    .get("default_verbosity")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(ContractParseError::MissingField("default_verbosity"))?,
            )?;

            let raw_tiers: Vec<RawServiceTier> = entry
                .get("service_tiers")
                .cloned()
                .ok_or(ContractParseError::MissingField("service_tiers"))
                .and_then(|v| {
                    serde_json::from_value(v).map_err(|e| {
                        ContractParseError::Invariant(format!("service_tiers no parseable: {e}"))
                    })
                })?;
            let service_tiers = raw_tiers
                .into_iter()
                .map(|tier| CodexServiceTier {
                    id: tier.id,
                    name: tier.name,
                    description: tier.description,
                })
                .collect();

            let default_service_tier = entry
                .get("default_service_tier")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);

            let supports_parallel_tool_calls = entry
                .get("supports_parallel_tool_calls")
                .and_then(serde_json::Value::as_bool)
                .ok_or(ContractParseError::MissingField(
                    "supports_parallel_tool_calls",
                ))?;
            let use_responses_lite = entry
                .get("use_responses_lite")
                .and_then(serde_json::Value::as_bool)
                .ok_or(ContractParseError::MissingField("use_responses_lite"))?;
            let prefer_websockets = entry
                .get("prefer_websockets")
                .and_then(serde_json::Value::as_bool)
                .ok_or(ContractParseError::MissingField("prefer_websockets"))?;

            let raw_trunc: RawTruncation = entry
                .get("truncation_policy")
                .cloned()
                .ok_or(ContractParseError::MissingField("truncation_policy"))
                .and_then(|v| {
                    serde_json::from_value(v).map_err(|e| {
                        ContractParseError::Invariant(format!(
                            "truncation_policy no parseable: {e}"
                        ))
                    })
                })?;

            models.push(CodexModelInfo {
                slug,
                display_name,
                description,
                context_window,
                max_context_window,
                visibility,
                supported_in_api,
                minimal_client_version,
                priority,
                input_modalities,
                supported_reasoning_levels,
                default_reasoning_level,
                default_reasoning_summary,
                support_verbosity,
                default_verbosity,
                service_tiers,
                default_service_tier,
                supports_parallel_tool_calls,
                use_responses_lite,
                prefer_websockets,
                truncation_policy: CodexTruncationPolicy {
                    mode: raw_trunc.mode,
                    limit: raw_trunc.limit,
                },
            });
        }

        Ok(Self { models })
    }

    /// Busca un modelo por slug.
    #[must_use]
    pub fn find(&self, slug: &str) -> Option<&CodexModelInfo> {
        self.models.iter().find(|m| m.slug == slug)
    }

    /// `true` si el catálogo contiene un modelo de auto-review.
    #[must_use]
    pub fn has_auto_review(&self) -> bool {
        self.models.iter().any(|m| m.slug.contains("auto-review"))
    }
}

/// Guard de región para lógica imperativa verificada en el pin.
///
/// Cada guard referencia un símbolo/región concreto del código de la release.
/// Si el símbolo desaparece o cambia de forma, el guard falla en lugar de
/// asumir que el comportamiento se mantiene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodexRegionGuard {
    /// Ruta relativa dentro de `codex-rs/`.
    pub path: &'static str,
    /// Símbolo/región esperada.
    pub symbol: &'static str,
    /// Descripción del comportamiento que protege.
    pub purpose: &'static str,
}

/// Regiones verificadas en `rust-v0.162.1`.
pub const CODEX_REGION_GUARDS: &[CodexRegionGuard] = &[
    CodexRegionGuard {
        path: "login/src/auth/default_client.rs",
        symbol: r#"pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";"#,
        purpose: "originator por defecto del cliente Codex",
    },
    CodexRegionGuard {
        path: "login/src/auth/default_client.rs",
        symbol: r#""{}/{build_version} ({} {}; {}) {}","#,
        purpose: "plantilla de user-agent del cliente",
    },
    CodexRegionGuard {
        path: "model-provider-info/src/lib.rs",
        symbol: r#"pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";"#,
        purpose: "base URL del provider ChatGPT Codex",
    },
    CodexRegionGuard {
        path: "model-provider-info/src/lib.rs",
        symbol: r#"pub const RESIDENCY_HEADER_NAME: &str = "x-openai-internal-codex-residency";"#,
        purpose: "header de residency",
    },
    CodexRegionGuard {
        path: "codex-api/src/requests/headers.rs",
        symbol: r#"insert_header(&mut headers, "session-id", &id);"#,
        purpose: "header de sesión",
    },
    CodexRegionGuard {
        path: "codex-api/src/requests/headers.rs",
        symbol: r#"insert_header(&mut headers, "thread-id", &id);"#,
        purpose: "header de thread",
    },
    CodexRegionGuard {
        path: "core/src/client.rs",
        symbol: r#"pub const X_CODEX_ROUTING_HINT_HEADER: &str = "x-codex-routing-hint";"#,
        purpose: "header de routing hint",
    },
    CodexRegionGuard {
        path: "core/src/client.rs",
        symbol: r#""responses_websockets=2026-02-06""#,
        purpose: "valor beta usado solo en transporte websocket",
    },
    CodexRegionGuard {
        path: "core/src/client.rs",
        symbol: "tool_choice: \"auto\".to_string(),",
        purpose: "tool_choice fijado en auto al construir ResponsesApiRequest",
    },
    CodexRegionGuard {
        path: "core/src/client.rs",
        symbol: "store: false,",
        purpose: "store desactivado por defecto",
    },
    CodexRegionGuard {
        path: "core/src/client.rs",
        symbol: "stream: true,",
        purpose: "stream activado por defecto",
    },
    CodexRegionGuard {
        path: "core/src/client.rs",
        symbol: "parallel_tool_calls: prompt.parallel_tool_calls",
        purpose: "derivación de parallel_tool_calls desde prompt y use_responses_lite",
    },
    CodexRegionGuard {
        path: "codex-api/src/endpoint/models.rs",
        symbol: "fn path() -> &'static str {",
        purpose: "endpoint de modelos",
    },
    CodexRegionGuard {
        path: "codex-api/src/endpoint/models.rs",
        symbol: "client_version={client_version}",
        purpose: "versión de cliente en la query del endpoint de modelos",
    },
    CodexRegionGuard {
        path: "models-manager/src/lib.rs",
        symbol: "include_str!(\"../models.json\")",
        purpose: "catálogo embebido en el binario",
    },
];

/// Verifica que todos los guards de región existen exactamente una vez en la
/// fuente correspondiente.
///
/// # Errors
/// Devuelve [`ContractParseError::Invariant`] si algún guard no aparece
/// exactamente una vez (ausente o duplicado), lo que indica que la revisión
/// inspeccionada no es la esperada.
pub fn verify_region_guards(files: &[(&'static str, &str)]) -> ContractResult<()> {
    for guard in CODEX_REGION_GUARDS {
        let Some(source) = files.iter().find(|(p, _)| *p == guard.path) else {
            return Err(ContractParseError::Invariant(format!(
                "archivo ausente para guard {}: {}",
                guard.purpose, guard.path
            )));
        };
        let count = source.1.matches(guard.symbol).count();
        if count != 1 {
            return Err(ContractParseError::Invariant(format!(
                "guard `{}` en `{}`: esperado 1 ocurrencia, encontrado {count}",
                guard.purpose, guard.path
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pinned_catalog() -> ContractResult<()> {
        let json = include_str!(
            "../../../../../fixtures/codex-rust-v0.162.1/codex-rs/models-manager/models.json"
        );
        let catalog = CodexCatalog::parse(json)?;
        assert_eq!(catalog.models.len(), 11);
        assert!(catalog.find("gpt-6-sol").is_some());
        assert!(catalog.find("gpt-5.4").is_none());
        assert!(catalog.has_auto_review());
        Ok(())
    }

    #[test]
    fn rejects_unknown_enum() {
        let err = CodexVisibility::from_raw("visibility", "banana").unwrap_err();
        assert_eq!(
            err,
            ContractParseError::UnknownEnum {
                field: "visibility",
                value: "banana".to_string(),
            }
        );
    }

    #[test]
    fn every_model_is_api_supported_in_pin() -> ContractResult<()> {
        let json = include_str!(
            "../../../../../fixtures/codex-rust-v0.162.1/codex-rs/models-manager/models.json"
        );
        let catalog = CodexCatalog::parse(json)?;
        assert!(catalog.models.iter().all(|m| m.supported_in_api));
        Ok(())
    }
}
