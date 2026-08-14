# Plan Integral de Deduplicación, Macros y Arquitectura Plug & Play

**Repositorio:** `openproxy`  
**Fecha:** 2026-08-14  
**Objetivo:** Eliminar ~4,500+ líneas de código duplicado/boilerplate, unificar abstracciones mediante traits y macros declarativas, y habilitar la incorporación inmediata (Plug & Play) de nuevos proveedores, endpoints, formatos de streaming y recursos de base de datos.

---

## Índice

1. [Resumen Ejecutivo de Oportunidades de Refactorización](#1-resumen-ejecutivo-de-oportunidades-de-refactorización)
2. [Crates de Adaptadores y Servidor (`openproxy-adapters`, `openproxy-server`)](#2-crates-de-adaptadores-y-servidor)
   - [2.1 Macro `declare_openai_adapter!` y Defaults de Trait](#21-macro-declare_openai_adapter-y-defaults-de-trait)
   - [2.2 Unificación de OpenCode Go / Zen](#22-unificación-de-opencode-go--zen)
   - [2.3 Trait Unificado de Identidad de Cliente `ClientSpoofer`](#23-trait-unificado-de-identidad-de-cliente-clientspoofer)
   - [2.4 Helper de Orquestación `PipelineRunner`](#24-helper-de-orquestación-pipelinerunner)
   - [2.5 Enmarcado y Sanitización Centralizada de Errores SSE](#25-enmarcado-y-sanitización-centralizada-de-errores-sse)
   - [2.6 Purga de Código Muerto en `handlers/admin/mod.rs`](#26-purga-de-código-muerto-en-handlersadminmodrs)
3. [Crates de Núcleo, Base de Datos y Pipeline (`openproxy-core`, `openproxy-db`, `openproxy-pipeline`, `openproxy-compression`)](#3-crates-de-núcleo-base-de-datos-y-pipeline)
   - [3.1 Unificación de Módulos Clonados (Types vs Core vs DB)](#31-unificación-de-módulos-clonados)
   - [3.2 Helpers y Macro Declarativa para SQLite Batch (`sqlite_batch_insert!`)](#32-helpers-y-macro-declarativa-para-sqlite-batch)
   - [3.3 Trait Modular de Streaming Pipeline `StreamingChunkStage`](#33-trait-modular-de-streaming-pipeline-streamingchunkstage)
   - [3.4 Trait y Visitor Genérico de Compresión `TextCompressor`](#34-trait-y-visitor-genérico-de-compresión-textcompressor)
   - [3.5 Purga de Archivos JSON Huérfanos](#35-purga-de-archivos-json-huérfanos)
4. [Impacto Global y Hoja de Ruta de Implementación](#4-impacto-global-y-hoja-de-ruta-de-implementación)

---

## 1. Resumen Ejecutivo de Oportunidades de Refactorización

| Área / Módulo | Problema Principal | Solución Propuesta | Reducción LOC |
| :--- | :--- | :--- | :--- |
| **Adaptadores OpenAI** | 6 adaptadores implementan manualmente métodos idénticos de 80 líneas | Defaults en `ProviderAdapter` + macro `declare_openai_adapter!` | **~600 LOC** |
| **OpenCode Go & Zen** | Copia 1:1 de 160 líneas cambiando solo la URL base | Adaptador unificado parametrizado | **~150 LOC** |
| **Orquestación Pipeline** | `/chat/completions` y `/messages` duplican ~120 líneas de setup | Helper `PipelineRunner` y `prepare_request` | **~400 LOC** |
| **Errores y Enmarcado SSE** | 5 serializaciones manuales dispersas con redacción de secretos | Métodos nativos en `ApiError::to_sse_error_frame` | **~150 LOC** |
| **Comentarios Zombie Admin** | `handlers/admin/mod.rs` contiene 1,000 líneas de código muerto | Purga y modularización de structs de entrada | **~1,000 LOC** |
| **Módulos Duplicados** | `model_normalize.rs`, `token_estimate.rs`, `pricing.rs` clonados | Centralizar en `openproxy-types` y `openproxy-db` | **~1,200 LOC** |
| **SQLite Batching** | Aritmética manual de placeholders `?{base + i}` y chunking en 6 archivos | Helpers `query_in_chunks` y macro `sqlite_batch_insert!` | **~450 LOC** |
| **Pipeline de Streaming** | Acoplamiento imperativo en `streaming_state.rs` | Trait `StreamingChunkStage` modular | **~350 LOC** |
| **Compresión de Mensajes** | Desempaquetado manual de arrays y duplicación de métricas | Visitor `mutate_message_text` y trait `TextCompressor` | **~300 LOC** |
| **TOTAL ESTIMADO** | | | **~4,600 LOC** |

---

## 2. Crates de Adaptadores y Servidor

### 2.1 Macro `declare_openai_adapter!` y Defaults de Trait

#### Diagnóstico
Archivos idénticos con 80-90 líneas cada uno:
- [`crates/openproxy-adapters/src/adapters/nous_research.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/nous_research.rs)
- [`crates/openproxy-adapters/src/adapters/nvidia_nim.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/nvidia_nim.rs)
- [`crates/openproxy-adapters/src/adapters/kilocode.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/kilocode.rs)
- [`crates/openproxy-adapters/src/adapters/ollama_cloud.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/ollama_cloud.rs)
- [`crates/openproxy-adapters/src/adapters/openrouter.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/openrouter.rs)

#### Solución

1. **Añadir implementaciones por defecto en el trait `ProviderAdapter`:**
```rust
// En openproxy-adapters::adapters::ProviderAdapter
fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
    format!("{}/chat/completions", self.config().base_url)
}

fn models_url(&self) -> Option<String> {
    Some(format!("{}/models", self.config().base_url))
}

async fn fetch_models(&self, upstream: &Arc<UpstreamClient>, api_key: &str) -> Result<Vec<DiscoveredModel>> {
    let url = self.models_url().ok_or_else(|| CoreError::Validation("no models_url".into()))?;
    fetch_openai_models(&url, upstream, api_key, self.id().as_str(), TargetFormat::Openai).await
}
```

2. **Macro declarativa para nuevos adaptadores OpenAI:**
```rust
#[macro_export]
macro_rules! declare_openai_adapter {
    (
        $struct_name:ident,
        id: $id:expr,
        name: $name:expr,
        base_url: $base_url:expr
        $(, extra_headers: [ $( ($k:expr, $v:expr) ),* $(,)? ])?
    ) => {
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
        pub struct $struct_name {
            config: ProviderAdapterConfig,
        }

        impl $struct_name {
            pub fn new() -> Self {
                Self {
                    config: ProviderAdapterConfig {
                        id: ProviderId::new($id),
                        name: $name.into(),
                        anonymous_fallback: false,
                        rate_limit_scope: "account".into(),
                        base_url: $base_url.into(),
                        auth_type: AdapterAuthType::Bearer,
                        format: AdapterFormat::Openai,
                        extra_headers: vec![ $( $( ($k.into(), $v.into()) ),* )? ],
                    },
                }
            }
        }

        $crate::adapters::derive_default_from_new!($struct_name);

        impl ProviderAdapter for $struct_name {
            fn config(&self) -> &ProviderAdapterConfig {
                &self.config
            }
        }
    };
}
```
**Resultado:** Crear un nuevo proveedor OpenAI pasa de requerir 85 líneas en un archivo separado a 1 sola macro de 6 líneas.

---

### 2.2 Unificación de OpenCode Go / Zen

#### Diagnóstico
- [`openproxy-adapters/src/adapters/opencode_go.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/opencode_go.rs) y [`openproxy-adapters/src/adapters/opencode_zen.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/opencode_zen.rs) son 99% idénticos.

#### Solución
Unificar en una estructura parametrizada `OpenCodeAdapter`:
```rust
pub struct OpenCodeAdapter {
    config: ProviderAdapterConfig,
    variant: OpenCodeVariant, // Go | Zen
}
```

---

### 2.3 Trait Unificado de Identidad de Cliente `ClientSpoofer`

#### Diagnóstico
Inconsistencia en inyección de User-Agent y headers simulados:
- [`cline.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/cline.rs) usa un array `CLINE_SPOOFING_HEADERS`.
- [`antigravity_headers.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/antigravity_headers.rs) usa un módulo separado con `OnceLock`.
- [`codex.rs`](file:///root/proyectos/openproxy/crates/openproxy-adapters/src/adapters/codex.rs) usa funciones sueltas `codex_user_agent`.

#### Solución
Trait común en `openproxy-adapters::spoofing`:
```rust
pub trait ClientSpoofer: Send + Sync {
    fn apply_headers(&self, headers: &mut HeaderMap);
}
```

---

### 2.4 Helper de Orquestación `PipelineRunner`

#### Diagnóstico
- [`openproxy-server/src/handlers/chat.rs`](file:///root/proyectos/openproxy/crates/openproxy-server/src/handlers/chat.rs#L136-L267) y [`openproxy-server/src/handlers/messages.rs`](file:///root/proyectos/openproxy/crates/openproxy-server/src/handlers/messages.rs#L47-L150) duplican ~120 líneas de setup: inicialización de `PipelineConfig`, cálculo de watchdog deadlines, instanciación de canales MPSC/oneshot y armado de `PipelineRequest`.

#### Solución
Crear `openproxy_server::services::pipeline_runner`:
```rust
pub struct PipelineRunner;

impl PipelineRunner {
    pub fn build(state: &AppState) -> Pipeline {
        Pipeline::with_selection_registry(
            state.db_pool().writer_arc(),
            state.pipeline_config(),
            state.record_bodies_and_flags(),
            state.selection_registry(),
            state.circuit_breaker(),
        )
    }

    pub fn prepare_request(
        state: &AppState,
        headers: &HeaderMap,
        req: Arc<OpenAIRequest>,
        raw_body: Bytes,
        api_key_id: Option<ApiKeyId>,
        combo_id: ComboId,
    ) -> (PipelineRequest, WatchdogGuard) { ... }
}
```

---

### 2.5 Enmarcado y Sanitización Centralizada de Errores SSE

#### Diagnóstico
Duplicación en [`handlers/chat.rs`](file:///root/proyectos/openproxy/crates/openproxy-server/src/handlers/chat.rs#L281), [`handlers/messages.rs`](file:///root/proyectos/openproxy/crates/openproxy-server/src/handlers/messages.rs#L130), [`middleware/auth.rs`](file:///root/proyectos/openproxy/crates/openproxy-server/src/middleware/auth.rs#L192) y [`error.rs`](file:///root/proyectos/openproxy/crates/openproxy-server/src/error.rs#L65).

#### Solución
```rust
// En openproxy-server::error::ApiError
impl ApiError {
    pub fn to_sse_error_frame(&self, format: TargetFormat) -> Bytes {
        let msg = truncate_error_message(&openproxy_core::cost::redact_error_msg(&self.0.to_string()).0);
        let val = match format {
            TargetFormat::Anthropic => serde_json::json!({
                "type": "error",
                "error": { "type": self.0.code(), "message": msg }
            }),
            _ => serde_json::json!({
                "error": { "message": msg, "type": self.0.code(), "code": self.0.http_status() }
            }),
        };
        let payload = serde_json::to_string(&val).unwrap_or_default();
        let prefix = if format == TargetFormat::Anthropic { "event: error\ndata: " } else { "data: " };
        Bytes::from(format!("{prefix}{payload}\n\n"))
    }
}
```

---

### 2.6 Purga de Código Muerto en `handlers/admin/mod.rs`

- [`crates/openproxy-server/src/handlers/admin/mod.rs:L95-1303`](file:///root/proyectos/openproxy/crates/openproxy-server/src/handlers/admin/mod.rs#L95-L1303): Eliminar más de 1,000 líneas de comentarios huérfanos y mover los structs `*Input` / `*Query` restantes a sus subarchivos (`accounts.rs`, `providers.rs`, `combos.rs`).

---

## 3. Crates de Núcleo, Base de Datos y Pipeline

### 3.1 Unificación de Módulos Clonados

#### Diagnóstico
- [`openproxy-core/src/model_normalize.rs`](file:///root/proyectos/openproxy/crates/openproxy-core/src/model_normalize.rs) vs [`openproxy-types/src/model_normalize.rs`](file:///root/proyectos/openproxy/crates/openproxy-types/src/model_normalize.rs): 256 líneas clonadas.
- [`openproxy-core/src/token_estimate.rs`](file:///root/proyectos/openproxy/crates/openproxy-core/src/token_estimate.rs) vs [`openproxy-types/src/token_estimate.rs`](file:///root/proyectos/openproxy/crates/openproxy-types/src/token_estimate.rs): 428 líneas clonadas.
- [`openproxy-core/src/pricing/mod.rs`](file:///root/proyectos/openproxy/crates/openproxy-core/src/pricing/mod.rs) vs [`openproxy-db/src/pricing.rs`](file:///root/proyectos/openproxy/crates/openproxy-db/src/pricing.rs): 639 líneas clonadas.

#### Solución
1. En `openproxy-core/src/model_normalize.rs`: sustituir por `pub use openproxy_types::model_normalize::*;`.
2. En `openproxy-core/src/token_estimate.rs`: sustituir por `pub use openproxy_types::token_estimate::*;`.
3. Centralizar `PRICING_TABLE` y lógica de precios en `openproxy-db::pricing` y reexportar en `core`.
4. Añadir métodos sobre `OpenAIMessage` en `openproxy-types`: `msg.extract_text()`, eliminando 3 helpers duplicados en `types`, `core` y `pipeline`.

---

### 3.2 Helpers y Macro Declarativa para SQLite Batch

#### Diagnóstico
Fragmentación manual con cálculo de `?{base + i}` y chunking en:
- [`openproxy-core/src/free_proxies.rs`](file:///root/proyectos/openproxy/crates/openproxy-core/src/free_proxies.rs#L613) (11 params)
- [`openproxy-core/src/notifications.rs`](file:///root/proyectos/openproxy/crates/openproxy-core/src/notifications.rs#L236) (4 params)
- [`openproxy-db/src/models.rs`](file:///root/proyectos/openproxy/crates/openproxy-db/src/models.rs#L453) (4 params)
- [`openproxy-core/src/models_dev_sync.rs`](file:///root/proyectos/openproxy/crates/openproxy-core/src/models_dev_sync.rs#L350)
- [`openproxy-db/src/combos.rs`](file:///root/proyectos/openproxy/crates/openproxy-db/src/combos.rs#L760)

#### Solución
```rust
// En openproxy-db::batch

/// Helper genérico para queries chunked con cláusula IN
pub fn query_in_chunks<I, F, R>(
    conn: &rusqlite::Connection,
    base_sql: &str, // "SELECT id FROM t WHERE id IN ({})"
    items: &[I],
    chunk_size: usize,
    mut row_mapper: F,
    ctx: &'static str,
) -> openproxy_types::Result<Vec<R>>
where
    I: rusqlite::ToSql,
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<R>,
{
    if items.is_empty() { return Ok(Vec::new()); }
    let mut results = Vec::with_capacity(items.len());
    for chunk in items.chunks(chunk_size) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = base_sql.replace("{}", &placeholders);
        let mut stmt = conn.prepare(&sql).map_err(crate::error::map_db_error_ctx(ctx))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk), &mut row_mapper)
            .map_err(crate::error::map_db_error_ctx(ctx))?;
        for row in rows {
            results.push(row.map_err(crate::error::map_db_error_ctx(ctx))?);
        }
    }
    Ok(results)
}
```

---

### 3.3 Trait Modular de Streaming Pipeline `StreamingChunkStage`

#### Diagnóstico
- [`openproxy-pipeline/src/streaming_state.rs`](file:///root/proyectos/openproxy/crates/openproxy-pipeline/src/streaming_state.rs#L68-L130): `apply_reasoning_normalizations` combina de forma rígida AST zero-copy, Think extractor y acumuladores de tool calls.

#### Solución
```rust
pub enum StreamAction {
    Forward(UpstreamSseChunk),
    Drop,
    EmitMultiple(Vec<UpstreamSseChunk>),
}

pub trait StreamingChunkStage: Send {
    fn process_chunk(&mut self, chunk: UpstreamSseChunk) -> openproxy_types::Result<StreamAction>;
    fn finalize(&mut self) -> openproxy_types::Result<Option<UpstreamSseChunk>> {
        Ok(None)
    }
}
```

---

### 3.4 Trait y Visitor Genérico de Compresión `TextCompressor`

#### Diagnóstico
- [`openproxy-compression/src/lite.rs`](file:///root/proyectos/openproxy/crates/openproxy-compression/src/lite.rs), [`content_router.rs`](file:///root/proyectos/openproxy/crates/openproxy-compression/src/content_router.rs) y [`rtk/mod.rs`](file:///root/proyectos/openproxy/crates/openproxy-compression/src/rtk/mod.rs) repiten el desempaquetado de `msg.content` (ignorando a veces arrays de partes) y el cálculo de métricas antes/después.

#### Solución
```rust
// Visitor genérico que soporta String y Array de ContentParts de forma transparente
pub fn mutate_message_text<F>(msg: &mut OpenAIMessage, mut transform: F) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    if let Some(ref mut content) = msg.content {
        match content {
            serde_json::Value::String(s) => {
                if let Some(new_s) = transform(s) {
                    *content = serde_json::Value::String(new_s);
                    return true;
                }
            }
            serde_json::Value::Array(parts) => {
                let mut changed = false;
                for part in parts.iter_mut() {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        if let Some(new_text) = transform(text) {
                            if let Some(obj) = part.as_object_mut() {
                                obj.insert("text".into(), serde_json::Value::String(new_text));
                                changed = true;
                            }
                        }
                    }
                }
                return changed;
            }
            _ => {}
        }
    }
    false
}
```

---

### 3.5 Purga de Archivos JSON Huérfanos

- [`crates/openproxy-compression/src/rtk/filters/`](file:///root/proyectos/openproxy/crates/openproxy-compression/src/rtk/filters/): Eliminar los 5 archivos `.json` huérfanos que no se leen en runtime (ya fueron migrados a código Rust en `line_filter.rs`).

---

## 4. Impacto Global y Hoja de Ruta de Implementación

### Plan de Acción por Fases

1. **Fase 1: Unificación de Tipos y Purga de Código Muerto**
   - Eliminar duplicados de `model_normalize.rs`, `token_estimate.rs` y `pricing.rs`.
   - Limpiar ~1,000 líneas de comentarios y código muerto en `handlers/admin/mod.rs`.
   - Eliminar `src/rtk/filters/*.json`.

2. **Fase 2: Abstracciones de Adaptadores y Servidor**
   - Implementar defaults en `ProviderAdapter` y macro `declare_openai_adapter!`.
   - Simplificar `nous_research.rs`, `nvidia_nim.rs`, `kilocode.rs`, `ollama_cloud.rs`.
   - Crear `PipelineRunner` y centralizar formateo SSE en `ApiError`.

3. **Fase 3: Base de Datos y Pipeline de Streaming**
   - Introducir `query_in_chunks` y macros batch en `openproxy-db`.
   - Refactorizar `free_proxies.rs`, `notifications.rs`, `models.rs`.
   - Adoptar `StreamingChunkStage` y `mutate_message_text`.
