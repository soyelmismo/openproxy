//! Contrato CodeBuddy fijado al paquete npm `@tencent-ai/codebuddy-code@2.164.0`.
//!
//! **Procedencia**: el paquete npm no expone `gitHead`, por lo que no existe un
//! campo inmutable que ate el artefacto publicado a un commit. La
//! correspondencia queda registrada como
//! [`PackageCommitCorrespondence::Unverified`]. El tarball sí está íntegro
//! (SHA-512 contra `dist.integrity` de npm) y los archivos internos coinciden
//! con los hashes SHA-256 de unpkg, así que el *contenido* es fiable aunque su
//! procedencia de commit no lo sea.
//!
//! **Regla de identidad**: no se activa identidad basándose solo en metadata.
//! [`CodeBuddyContract::identity_activatable`] es fail-closed.
//!
//! **Bundle**: el bundle minificado solo se inspecciona por offsets/regiones
//! verificadas contra un hash previo. Nunca se ejecuta. Los offsets son
//! referencias exclusivas de `2.164.0`, no una API estable de extracción.
//!
//! **Unidades**: `maxAllowedSize` y `contextWindow` no comparten unidad
//! demostrada; se conservan como valores crudos documentados sin fusionarlos.

use serde::Deserialize;

use super::{ContractParseError, ContractResult, json_error};

/// Nombre del paquete npm.
pub const CODEBUDDY_PACKAGE: &str = "@tencent-ai/codebuddy-code";

/// Versión fijada.
pub const CODEBUDDY_VERSION: &str = "2.168.2";

/// Endpoint de producción declarado en `product.json`.
pub const CODEBUDDY_ENDPOINT: &str = "https://www.codebuddy.ai";

/// Endpoint de staging declarado en `product.json`.
pub const CODEBUDDY_STAGING_ENDPOINT: &str = "https://staging-codebuddy.tencent.com";

/// Tipo de despliegue declarado en `product.json`.
pub const CODEBUDDY_DEPLOYMENT_TYPE: &str = "SaaS";

/// Plataforma declarada en `product.json`.
pub const CODEBUDDY_PLATFORM: &str = "CLI";

/// Vendor observado en el catálogo `2.164.0`.
///
/// El significado semántico de cada vendor no está demostrado por la fuente,
/// por lo que se modelan como identificadores opacos en lugar de inferir un
/// proveedor concreto.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodeBuddyVendor {
    /// Vendor `e`.
    E,
    /// Vendor `f`.
    F,
    /// Vendor `i`.
    I,
    /// Vendor `j`.
    J,
}

impl CodeBuddyVendor {
    /// Parsea un vendor crudo rechazando valores desconocidos.
    ///
    /// # Errors
    /// Devuelve [`ContractParseError::UnknownEnum`] si aparece un vendor que
    /// no estaba presente en la revisión fijada.
    pub fn from_raw(value: &str) -> ContractResult<Self> {
        match value {
            "e" => Ok(Self::E),
            "f" => Ok(Self::F),
            "i" => Ok(Self::I),
            "j" => Ok(Self::J),
            other => Err(ContractParseError::UnknownEnum {
                field: "vendor",
                value: other.to_string(),
            }),
        }
    }

    /// Devuelve el código crudo del vendor.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::E => "e",
            Self::F => "f",
            Self::I => "i",
            Self::J => "j",
        }
    }
}

/// Forma de `reasoning` observada en el catálogo.
///
/// En `2.164.0` aparecen dos formas distintas y se modelan explícitamente en
/// lugar de colapsarlas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CodeBuddyReasoningShape {
    /// `{ canDisableThinking, defaultEffort, summary, supportedEfforts }`.
    Extended,
    /// `{ effort, summary }`.
    Simple,
}

/// Bloque `reasoning` de un modelo.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeBuddyReasoning {
    /// Forma detectada.
    pub shape: CodeBuddyReasoningShape,
    /// Resumen declarado (`auto` en la revisión fijada).
    pub summary: String,
    /// Esfuerzo por defecto cuando la forma es `Extended`.
    pub default_effort: Option<String>,
    /// Esfuerzo declarado cuando la forma es `Simple`.
    pub effort: Option<String>,
    /// Esfuerzos soportados cuando la forma es `Extended`.
    pub supported_efforts: Option<Vec<String>>,
    /// Si se puede desactivar el pensamiento cuando la forma es `Extended`.
    pub can_disable_thinking: Option<bool>,
}

/// Modelo relacionado declarado por upstream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeBuddyRelatedModels {
    /// Modelo alternativo ligero.
    pub lite: String,
    /// Modelo de razonamiento.
    pub reasoning: String,
}

/// Ventana de contexto declarada por upstream.
///
/// `supported_lengths` y `default_length` se conservan crudos: la unidad no
/// está demostrada por la fuente.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeBuddyContextWindow {
    /// Longitudes soportadas declaradas.
    pub supported_lengths: Vec<u64>,
    /// Longitud por defecto.
    pub default_length: u64,
}

/// Entrada del catálogo CodeBuddy con los campos soportados por el parser.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeBuddyModel {
    /// Identificador del modelo.
    pub id: String,
    /// Nombre legible.
    pub name: String,
    /// Vendor opaco.
    pub vendor: CodeBuddyVendor,
    /// Límite de tokens de entrada declarado.
    pub max_input_tokens: Option<u64>,
    /// Límite de tokens de salida declarado.
    pub max_output_tokens: Option<u64>,
    /// Tamaño máximo permitido declarado (unidad no demostrada).
    pub max_allowed_size: u64,
    /// Ventana de contexto cuando el modelo la declara.
    pub context_window: Option<CodeBuddyContextWindow>,
    /// Si soporta llamadas a herramientas.
    pub supports_tool_call: bool,
    /// Si soporta imágenes.
    pub supports_images: bool,
    /// Si soporta razonamiento.
    pub supports_reasoning: Option<bool>,
    /// Si es solo razonamiento.
    pub only_reasoning: Option<bool>,
    /// Si se puede desactivar el pensamiento.
    pub can_disable_thinking: Option<bool>,
    /// Si está deshabilitado multimodal.
    pub disabled_multimodal: Option<bool>,
    /// Bloque de razonamiento cuando existe.
    pub reasoning: Option<CodeBuddyReasoning>,
    /// Modelos relacionados.
    pub related_models: Option<CodeBuddyRelatedModels>,
    /// Si es el modelo por defecto.
    pub is_default: Option<bool>,
    /// Temperatura declarada.
    pub temperature: Option<f64>,
    /// `top_p` declarado.
    pub top_p: Option<f64>,
    /// Resumen declarado.
    pub summary: Option<String>,
    /// Etiquetas declaradas.
    pub tags: Option<Vec<String>>,
}

/// Catálogo CodeBuddy completo.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeBuddyCatalog {
    /// Modelos en el orden publicado por upstream.
    pub models: Vec<CodeBuddyModel>,
}

impl CodeBuddyCatalog {
    /// Parsea `product.json` de la revisión fijada.
    ///
    /// # Errors
    /// Devuelve un error si el JSON es inválido, falta un campo obligatorio o
    /// aparece un vendor no soportado.
    pub fn parse(json: &str) -> ContractResult<Self> {
        #[derive(Deserialize)]
        struct RawProduct {
            models: Vec<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct RawRelated {
            lite: String,
            reasoning: String,
        }
        #[derive(Deserialize)]
        struct RawContextWindow {
            #[serde(rename = "supportedLengths")]
            supported_lengths: Vec<u64>,
            #[serde(rename = "defaultLength")]
            default_length: u64,
        }

        let raw: RawProduct = serde_json::from_str(json).map_err(|e| json_error(&e))?;
        let mut models = Vec::with_capacity(raw.models.len());

        for entry in &raw.models {
            let id = entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("id"))?
                .to_string();
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or(ContractParseError::MissingField("name"))?
                .to_string();
            let vendor = CodeBuddyVendor::from_raw(
                entry
                    .get("vendor")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(ContractParseError::MissingField("vendor"))?,
            )?;
            let max_input_tokens = entry
                .get("maxInputTokens")
                .and_then(serde_json::Value::as_u64);
            let max_output_tokens = entry
                .get("maxOutputTokens")
                .and_then(serde_json::Value::as_u64);
            let max_allowed_size = entry
                .get("maxAllowedSize")
                .and_then(serde_json::Value::as_u64)
                .ok_or(ContractParseError::MissingField("maxAllowedSize"))?;

            let context_window = entry
                .get("contextWindow")
                .cloned()
                .map(|v| {
                    serde_json::from_value::<RawContextWindow>(v)
                        .map(|cw| CodeBuddyContextWindow {
                            supported_lengths: cw.supported_lengths,
                            default_length: cw.default_length,
                        })
                        .map_err(|e| {
                            ContractParseError::Invariant(format!(
                                "contextWindow no parseable: {e}"
                            ))
                        })
                })
                .transpose()?;

            let supports_tool_call = entry
                .get("supportsToolCall")
                .and_then(serde_json::Value::as_bool)
                .ok_or(ContractParseError::MissingField("supportsToolCall"))?;
            let supports_images = entry
                .get("supportsImages")
                .and_then(serde_json::Value::as_bool)
                .ok_or(ContractParseError::MissingField("supportsImages"))?;
            let supports_reasoning = entry
                .get("supportsReasoning")
                .and_then(serde_json::Value::as_bool);
            let only_reasoning = entry
                .get("onlyReasoning")
                .and_then(serde_json::Value::as_bool);
            let can_disable_thinking = entry
                .get("canDisableThinking")
                .and_then(serde_json::Value::as_bool);
            let disabled_multimodal = entry
                .get("disabledMultimodal")
                .and_then(serde_json::Value::as_bool);

            let reasoning = entry
                .get("reasoning")
                .cloned()
                .map(|v| parse_reasoning(&v))
                .transpose()?;

            let related_models = entry
                .get("relatedModels")
                .cloned()
                .map(|v| {
                    serde_json::from_value::<RawRelated>(v).map(|r| CodeBuddyRelatedModels {
                        lite: r.lite,
                        reasoning: r.reasoning,
                    })
                })
                .transpose()
                .map_err(|e| {
                    ContractParseError::Invariant(format!("relatedModels no parseable: {e}"))
                })?;

            let is_default = entry.get("isDefault").and_then(serde_json::Value::as_bool);
            let temperature = entry.get("temperature").and_then(serde_json::Value::as_f64);
            let top_p = entry.get("top_p").and_then(serde_json::Value::as_f64);
            let summary = entry
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let tags = entry
                .get("tags")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                });

            models.push(CodeBuddyModel {
                id,
                name,
                vendor,
                max_input_tokens,
                max_output_tokens,
                max_allowed_size,
                context_window,
                supports_tool_call,
                supports_images,
                supports_reasoning,
                only_reasoning,
                can_disable_thinking,
                disabled_multimodal,
                reasoning,
                related_models,
                is_default,
                temperature,
                top_p,
                summary,
                tags,
            });
        }

        Ok(Self { models })
    }

    /// Busca un modelo por id.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&CodeBuddyModel> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Devuelve el modelo marcado como default, si existe.
    #[must_use]
    pub fn default_model(&self) -> Option<&CodeBuddyModel> {
        self.models.iter().find(|m| m.is_default == Some(true))
    }
}

/// Interpreta el bloque `reasoning` distinguiendo las dos formas observadas.
fn parse_reasoning(value: &serde_json::Value) -> ContractResult<CodeBuddyReasoning> {
    let obj = value
        .as_object()
        .ok_or(ContractParseError::MissingField("reasoning"))?;
    let summary = obj
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .ok_or(ContractParseError::MissingField("reasoning.summary"))?
        .to_string();

    let default_effort = obj
        .get("defaultEffort")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let supported_efforts = obj
        .get("supportedEfforts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
    let effort = obj
        .get("effort")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let is_extended = default_effort.is_some() || supported_efforts.is_some();
    if is_extended && effort.is_some() {
        return Err(ContractParseError::Invariant(
            "reasoning mezcla la forma extendida con `effort`".to_string(),
        ));
    }
    if !is_extended && effort.is_none() {
        return Err(ContractParseError::Invariant(
            "reasoning no declara `defaultEffort`/`supportedEfforts` ni `effort`".to_string(),
        ));
    }

    Ok(CodeBuddyReasoning {
        shape: if is_extended {
            CodeBuddyReasoningShape::Extended
        } else {
            CodeBuddyReasoningShape::Simple
        },
        summary,
        default_effort,
        effort,
        supported_efforts,
        can_disable_thinking: obj
            .get("canDisableThinking")
            .and_then(serde_json::Value::as_bool),
    })
}

/// Guard de región del bundle minificado.
///
/// **Referencia exclusiva de `2.164.0`.** El offset se valida contra el hash
/// del bundle antes de usarse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeBuddyBundleGuard {
    /// Offset absoluto en el bundle donde aparece el símbolo.
    pub offset: u64,
    /// Símbolo esperado en esa posición.
    pub symbol: &'static str,
    /// Qué comportamiento protege el guard.
    pub purpose: &'static str,
}

/// Regiones verificadas en `dist/codebuddy.js` de `2.164.0`.
pub const CODEBUDDY_BUNDLE_GUARDS: &[CodeBuddyBundleGuard] = &[
    CodeBuddyBundleGuard {
        offset: 7_732_089,
        symbol: "runIdentityHeaders",
        purpose: "inyeccion de headers de identidad del cliente (export I2)",
    },
    CodeBuddyBundleGuard {
        offset: 7_959_638,
        symbol: "x-codebuddy-request",
        purpose: "coalescencia de requests entre cliente e IDE",
    },
    CodeBuddyBundleGuard {
        offset: 9_028_640,
        symbol: "collectProductGatewayDomains",
        purpose: "recopilacion de dominios de gateway de producto",
    },
    CodeBuddyBundleGuard {
        offset: 9_028_902,
        symbol: "officialEndpoints",
        purpose: "filtrado de endpoints oficiales frente a custom",
    },
    CodeBuddyBundleGuard {
        offset: 9_079_356,
        symbol: "X-Domain",
        purpose: "lista de headers internos omitidos en respuestas de error",
    },
    CodeBuddyBundleGuard {
        offset: 18_173_589,
        symbol: "X-Agent-Intent",
        purpose: "headers de producto e identidad del IDE (X-IDE-*)",
    },
];

/// Verifica los guards de bundle contra los bytes del artefacto.
///
/// # Errors
/// Devuelve [`ContractParseError::Invariant`] si algún símbolo no aparece
/// exactamente en el offset declarado.
pub fn verify_bundle_guards(bundle: &[u8]) -> ContractResult<()> {
    for guard in CODEBUDDY_BUNDLE_GUARDS {
        let start = usize::try_from(guard.offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(guard.symbol.len());
        let Some(window) = bundle.get(start..end) else {
            return Err(ContractParseError::Invariant(format!(
                "guard `{}`: offset {} fuera del bundle",
                guard.purpose, guard.offset
            )));
        };
        if window != guard.symbol.as_bytes() {
            return Err(ContractParseError::Invariant(format!(
                "guard `{}`: offset {} no contiene el símbolo esperado",
                guard.purpose, guard.offset
            )));
        }
    }
    Ok(())
}

/// Contrato completo de CodeBuddy con su estado de procedencia.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeBuddyContract {
    /// Catálogo de modelos.
    pub catalog: CodeBuddyCatalog,
    /// Estado de correspondencia paquete→commit.
    pub correspondence: super::PackageCommitCorrespondence,
}

impl CodeBuddyContract {
    /// `true` solo si el paquete demuestra procedencia inmutable hacia un
    /// commit. En `2.164.0` es `false`: el paquete npm no expone `gitHead`.
    ///
    /// La identidad **no** se activa basándose solo en metadata de paquete.
    #[must_use]
    pub fn identity_activatable(&self) -> bool {
        self.correspondence.is_verified()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product() -> &'static str {
        include_str!("../../../../../fixtures/codebuddy-extracted/product.json")
    }

    #[test]
    fn parses_pinned_catalog() -> ContractResult<()> {
        let catalog = CodeBuddyCatalog::parse(product())?;
        assert_eq!(catalog.models.len(), 22);
        assert!(catalog.default_model().is_some());
        Ok(())
    }

    #[test]
    fn vendor_counts_match_pin() -> ContractResult<()> {
        let catalog = CodeBuddyCatalog::parse(product())?;
        let count = |v: CodeBuddyVendor| catalog.models.iter().filter(|m| m.vendor == v).count();
        assert_eq!(count(CodeBuddyVendor::E), 12);
        assert_eq!(count(CodeBuddyVendor::F), 7);
        assert_eq!(count(CodeBuddyVendor::J), 2);
        assert_eq!(count(CodeBuddyVendor::I), 1);
        Ok(())
    }

    #[test]
    fn rejects_unknown_vendor() {
        let err = CodeBuddyVendor::from_raw("z").unwrap_err();
        assert_eq!(
            err,
            ContractParseError::UnknownEnum {
                field: "vendor",
                value: "z".to_string(),
            }
        );
    }

    #[test]
    fn reasoning_shapes_are_distinguished() -> ContractResult<()> {
        let catalog = CodeBuddyCatalog::parse(product())?;
        let shapes = catalog
            .models
            .iter()
            .filter_map(|m| m.reasoning.as_ref())
            .map(|r| r.shape)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(shapes.contains(&CodeBuddyReasoningShape::Extended));
        assert!(shapes.contains(&CodeBuddyReasoningShape::Simple));
        Ok(())
    }

    #[test]
    fn identity_is_not_activatable_from_metadata_alone() {
        let catalog = CodeBuddyCatalog::parse(product()).expect("catálogo válido");
        let contract = CodeBuddyContract {
            catalog,
            correspondence: super::super::PackageCommitCorrespondence::Unverified {
                reason: "el paquete npm no expone gitHead".to_string(),
            },
        };
        assert!(!contract.identity_activatable());
    }
}
