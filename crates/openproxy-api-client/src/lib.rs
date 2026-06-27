//! openproxy-api-client: cliente HTTP para la admin API de openproxy.
//!
//! Consume los endpoints `/admin/*` de un openproxy-server corriendo.
//! Se usa desde scripts externos y automatización (el dashboard SPA se
//! sirve desde el propio binario openproxy-server vía rust-embed, así que
//! ya no hay un crate `openproxy-web` que lo consuma internamente).
//!
//! ## Forma de uso
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use openproxy_api_client::Client;
//! use openproxy_core::usage::UsageFilter;
//! use openproxy_core::ids::ProviderId;
//!
//! let client = Client::new("http://127.0.0.1:8080");
//! let providers = client.list_providers().await?;
//! let summary = client
//!     .usage_summary(&UsageFilter {
//!         provider_id: Some(ProviderId::new("openrouter")),
//!         ..Default::default()
//!     })
//!     .await?;
//! # let _ = (providers, summary);
//! # Ok(()) }
//! ```
//!
//! ## Manejo de errores
//!
//! `ClientError` cubre cuatro familias:
//! - `Http` — fallo de transporte (red, DNS, TLS) propagado de reqwest.
//! - `Api` — `CoreError` mapeado a partir del `code` JSON que el servidor
//!   devuelve en sus respuestas 4xx/5xx (ver `ApiError` en
//!   `openproxy-server`). El `Display` preserva el mensaje del servidor.
//! - `Status` — el servidor devolvió un status >= 400 con un body que o
//!   bien no es JSON, o bien no tiene la forma `{"error": {"code","message"}}`.
//! - `Deserialize` — el body de éxito (2xx) no parsea al tipo pedido.
//!
//! Si un método individual documenta un retorno más específico (e.g.
//! `create_provider` siempre devuelve `ProviderId`), el JSON se inspecciona
//! a través del body crudo del servidor; ver `parse_envelope_id` para
//! el patrón de extracción de `{"id": ...}`.

use openproxy_core::{
    CoreError, accounts,
    admin::{
        AddTargetInput, CreateAccountInput, CreateComboInput, CreateProviderInput,
        UpdateAccountApiKeyInput,
    },
    analytics::{LatencyPercentiles, RaceStats},
    combos,
    ids::{AccountId, ComboId, ModelRowId, ProviderId},
    providers,
    usage::{ByAccountRow, ByModelRow, ByStatusRow, ErrorRow, UsageFilter, UsageSummary},
};
use std::fmt::Write as _;

/// Cliente HTTP para la admin API de openproxy.
///
/// Mantiene una `reqwest::Client` reutilizable y la `base_url` del server.
/// Es `Send + Sync` y barato de clonar (comparte el `reqwest::Client`
/// interior, que ya es `Arc`-interno).
#[derive(Debug, Clone)]
pub struct Client {
    base_url: String,
    http: reqwest::Client,
}

impl Client {
    /// Construye un cliente con un `reqwest::Client` por defecto.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_client(base_url, reqwest::Client::new())
    }

    /// Construye un cliente compartiendo un `reqwest::Client` propio.
    ///
    /// Útil cuando el llamador quiere configurar timeouts, TLS, proxies, o
    /// reutilizar un pool de conexiones a nivel de aplicación.
    pub fn with_client(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        let base = base_url.into();
        // Trim del slash final para que `url("/admin/...")` siempre
        // concatene con un único separador, evitando `//admin/...`.
        let base_url = base.trim_end_matches('/').to_string();
        Self { base_url, http }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    // -----------------------------------------------------------------
    // Health
    // -----------------------------------------------------------------

    /// `GET /admin/health` — liveness con tag de versión.
    pub async fn health(&self) -> Result<serde_json::Value, ClientError> {
        let resp = self.http.get(self.url("/admin/health")).send().await?;
        parse_json(resp).await
    }

    // -----------------------------------------------------------------
    // Providers
    // -----------------------------------------------------------------

    /// `GET /admin/providers`.
    pub async fn list_providers(&self) -> Result<Vec<providers::Provider>, ClientError> {
        let resp = self.http.get(self.url("/admin/providers")).send().await?;
        parse_json(resp).await
    }

    /// `POST /admin/providers`. Devuelve el `ProviderId` recién creado.
    pub async fn create_provider(
        &self,
        input: CreateProviderInput,
    ) -> Result<ProviderId, ClientError> {
        let resp = self
            .http
            .post(self.url("/admin/providers"))
            .json(&input)
            .send()
            .await?;
        let body: serde_json::Value = parse_json(resp).await?;
        let id = body.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
            ClientError::Deserialize(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing \"id\" string in create_provider response",
            )))
        })?;
        Ok(ProviderId::new(id.to_string()))
    }

    /// `DELETE /admin/providers/:id`. Idempotente.
    pub async fn delete_provider(&self, id: &ProviderId) -> Result<(), ClientError> {
        let path = format!("/admin/providers/{}", urlencoded(id.as_str()));
        let resp = self.http.delete(self.url(&path)).send().await?;
        parse_unit(resp).await
    }

    // -----------------------------------------------------------------
    // Accounts
    // -----------------------------------------------------------------

    /// `GET /admin/accounts[?provider_id=...]`.
    pub async fn list_accounts(
        &self,
        provider: Option<&ProviderId>,
    ) -> Result<Vec<accounts::Account>, ClientError> {
        let mut url = self.url("/admin/accounts").to_string();
        if let Some(p) = provider {
            let qs = build_query(&[("provider_id", Some(p.as_str()))]);
            write!(&mut url, "?{}", qs).expect("writing to String never fails");
        }
        let resp = self.http.get(url).send().await?;
        parse_json(resp).await
    }

    /// `POST /admin/accounts`. Devuelve el `AccountId` recién creado.
    pub async fn create_account(
        &self,
        input: CreateAccountInput,
    ) -> Result<AccountId, ClientError> {
        let resp = self
            .http
            .post(self.url("/admin/accounts"))
            .json(&input)
            .send()
            .await?;
        let body: serde_json::Value = parse_json(resp).await?;
        let id = body.get("id").and_then(|v| v.as_i64()).ok_or_else(|| {
            ClientError::Deserialize(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing numeric \"id\" in create_account response",
            )))
        })?;
        Ok(AccountId::new(id))
    }

    /// `DELETE /admin/accounts/:id`. Idempotente.
    pub async fn delete_account(&self, id: AccountId) -> Result<(), ClientError> {
        let path = format!("/admin/accounts/{}", id.0);
        let resp = self.http.delete(self.url(&path)).send().await?;
        parse_unit(resp).await
    }

    /// `PUT /admin/accounts/:id/api-key`. Encripta y guarda (o limpia)
    /// la API key de una cuenta existente.
    pub async fn update_account_api_key(
        &self,
        id: AccountId,
        input: UpdateAccountApiKeyInput,
    ) -> Result<(), ClientError> {
        let path = format!("/admin/accounts/{}/api-key", id.0);
        let resp = self.http.put(self.url(&path)).json(&input).send().await?;
        parse_unit(resp).await
    }

    // -----------------------------------------------------------------
    // Combos
    // -----------------------------------------------------------------

    /// `GET /admin/combos`.
    pub async fn list_combos(&self) -> Result<Vec<combos::Combo>, ClientError> {
        let resp = self.http.get(self.url("/admin/combos")).send().await?;
        parse_json(resp).await
    }

    /// `POST /admin/combos`. Devuelve el `ComboId` recién creado.
    pub async fn create_combo(&self, input: CreateComboInput) -> Result<ComboId, ClientError> {
        let resp = self
            .http
            .post(self.url("/admin/combos"))
            .json(&input)
            .send()
            .await?;
        let body: serde_json::Value = parse_json(resp).await?;
        let id = body.get("id").and_then(|v| v.as_i64()).ok_or_else(|| {
            ClientError::Deserialize(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing numeric \"id\" in create_combo response",
            )))
        })?;
        Ok(ComboId(id))
    }

    /// `DELETE /admin/combos/:id`. Idempotente.
    pub async fn delete_combo(&self, id: ComboId) -> Result<(), ClientError> {
        let path = format!("/admin/combos/{}", id.0);
        let resp = self.http.delete(self.url(&path)).send().await?;
        parse_unit(resp).await
    }

    /// `GET /admin/combos/:id/targets`.
    pub async fn list_combo_targets(
        &self,
        combo_id: ComboId,
    ) -> Result<Vec<combos::ComboTarget>, ClientError> {
        let path = format!("/admin/combos/{}/targets", combo_id.0);
        let resp = self.http.get(self.url(&path)).send().await?;
        parse_json(resp).await
    }

    /// `POST /admin/combos/:id/targets`. Devuelve el `combo_target.id`
    /// (un `i64` plano — el crate no expone un `ComboTargetId` en la API
    /// pública de este cliente, así que lo devolvemos crudo).
    pub async fn add_target(
        &self,
        combo_id: ComboId,
        input: AddTargetInput,
    ) -> Result<i64, ClientError> {
        let path = format!("/admin/combos/{}/targets", combo_id.0);
        let resp = self.http.post(self.url(&path)).json(&input).send().await?;
        let body: serde_json::Value = parse_json(resp).await?;
        let id = body.get("id").and_then(|v| v.as_i64()).ok_or_else(|| {
            ClientError::Deserialize(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing numeric \"id\" in add_target response",
            )))
        })?;
        Ok(id)
    }

    // -----------------------------------------------------------------
    // Models
    // -----------------------------------------------------------------

    /// `GET /v1/models` (endpoint público, no `/admin/...`).
    ///
    /// El server devuelve la lista de modelos en formato OpenAI
    /// (`{"object": "list", "data": [...]}`). Mantenemos el tipo laxo
    /// `serde_json::Value` para no atar el cliente a una versión concreta
    /// del shape; los consumidores que necesiten los campos pueden
    /// deserializar desde aquí.
    pub async fn list_models(&self) -> Result<serde_json::Value, ClientError> {
        let resp = self.http.get(self.url("/v1/models")).send().await?;
        parse_json(resp).await
    }

    /// `POST /admin/models/:id/refresh`.
    ///
    /// El parámetro es un `ModelRowId` (no un `ProviderId`) porque la ruta
    /// del server indexa por fila de la tabla `models`. El nombre de
    /// parámetro del spec original era "provider", pero el contrato del
    /// server exige un id numérico; se documenta aquí para no repetir la
    /// confusión más adelante.
    ///
    /// Devuelve el número de filas tocadas (inserts + updates) en la tabla
    /// `models`, según reporta el server.
    pub async fn refresh_models(&self, model_row_id: ModelRowId) -> Result<usize, ClientError> {
        let path = format!("/admin/models/{}/refresh", model_row_id.0);
        let resp = self.http.post(self.url(&path)).send().await?;
        let body: serde_json::Value = parse_json(resp).await?;
        let touched = body
            .get("touched")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                ClientError::Deserialize(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing \"touched\" in refresh_models response",
                )))
            })?;
        usize::try_from(touched).map_err(|_| {
            ClientError::Deserialize(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("\"touched\" does not fit in usize: {}", touched),
            )))
        })
    }

    // -----------------------------------------------------------------
    // Usage analytics
    // -----------------------------------------------------------------

    /// `GET /admin/usage/summary?from=...&to=...&provider_id=...&...`.
    pub async fn usage_summary(&self, f: &UsageFilter) -> Result<UsageSummary, ClientError> {
        let url = format!(
            "{}?{}",
            self.url("/admin/usage/summary"),
            usage_filter_query(f)
        );
        let resp = self.http.get(url).send().await?;
        parse_json(resp).await
    }

    /// `GET /admin/usage/by-model?from=...&...`.
    pub async fn usage_by_model(&self, f: &UsageFilter) -> Result<Vec<ByModelRow>, ClientError> {
        let url = format!(
            "{}?{}",
            self.url("/admin/usage/by-model"),
            usage_filter_query(f)
        );
        let resp = self.http.get(url).send().await?;
        parse_json(resp).await
    }

    /// `GET /admin/usage/by-account?from=...&...`.
    pub async fn usage_by_account(
        &self,
        f: &UsageFilter,
    ) -> Result<Vec<ByAccountRow>, ClientError> {
        let url = format!(
            "{}?{}",
            self.url("/admin/usage/by-account"),
            usage_filter_query(f)
        );
        let resp = self.http.get(url).send().await?;
        parse_json(resp).await
    }

    /// `GET /admin/usage/by-status?from=...&...`.
    pub async fn usage_by_status(&self, f: &UsageFilter) -> Result<Vec<ByStatusRow>, ClientError> {
        let url = format!(
            "{}?{}",
            self.url("/admin/usage/by-status"),
            usage_filter_query(f)
        );
        let resp = self.http.get(url).send().await?;
        parse_json(resp).await
    }

    /// `GET /admin/usage/errors?from=...&...&limit=N`.
    pub async fn usage_errors(
        &self,
        f: &UsageFilter,
        limit: u32,
    ) -> Result<Vec<ErrorRow>, ClientError> {
        let mut qs = usage_filter_query(f);
        if !qs.is_empty() {
            write!(&mut qs, "&limit={}", limit).expect("writing to String never fails");
        } else {
            write!(&mut qs, "limit={}", limit).expect("writing to String never fails");
        }
        let url = format!("{}?{}", self.url("/admin/usage/errors"), qs);
        let resp = self.http.get(url).send().await?;
        parse_json(resp).await
    }

    /// `GET /admin/usage/latency?from=...&...`.
    pub async fn usage_latency(&self, f: &UsageFilter) -> Result<LatencyPercentiles, ClientError> {
        let url = format!(
            "{}?{}",
            self.url("/admin/usage/latency"),
            usage_filter_query(f)
        );
        let resp = self.http.get(url).send().await?;
        parse_json(resp).await
    }

    /// `GET /admin/usage/races?from=...&...`.
    pub async fn usage_races(&self, f: &UsageFilter) -> Result<RaceStats, ClientError> {
        let url = format!(
            "{}?{}",
            self.url("/admin/usage/races"),
            usage_filter_query(f)
        );
        let resp = self.http.get(url).send().await?;
        parse_json(resp).await
    }
}

// =====================================================================
// Error type
// =====================================================================

/// Errores que puede devolver cualquier método del [`Client`].
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Fallo de transporte (red, DNS, TLS, timeout). Heredado de reqwest.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// El server devolvió un error tipado (`{"error": {"code", "message"}}`).
    /// El `CoreError` se reconstruye a partir del `code`; el `Display`
    /// preserva el mensaje del server.
    #[error("api: {0}")]
    Api(#[from] CoreError),

    /// El server devolvió un status >= 400 con un body que o bien no
    /// era JSON, o bien no seguía el sobre `{"error": ...}`. Conservamos
    /// el status y el cuerpo crudo para diagnóstico.
    #[error("status {0}: {1}")]
    Status(u16, String),

    /// El body de una respuesta 2xx no deserializó al tipo pedido.
    #[error("deserialize: {0}")]
    Deserialize(#[from] serde_json::Error),
}

// =====================================================================
// Internals
// =====================================================================

/// Inspecciona el `status` y el body de una respuesta y la entrega a uno
/// de tres destinos:
///
/// 1. `2xx` y body JSON deserializable a `T` → `Ok(T)`.
/// 2. `4xx/5xx` con body `{"error": {"code", "message"}}` → mapea el
///    `code` a [`CoreError`] y lo envuelve en [`ClientError::Api`]. Si el
///    `code` no se reconoce, devuelve [`ClientError::Status`] con el
///    código y mensaje crudos.
/// 3. `4xx/5xx` con body que no encaja en el sobre → [`ClientError::Status`].
async fn parse_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, ClientError> {
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if status.is_success() {
        Ok(serde_json::from_slice(&bytes)?)
    } else {
        Err(map_error_body(status.as_u16(), &bytes))
    }
}

/// Variante para endpoints que devuelven `{"deleted": ...}` u otro body
/// informativo. No necesitamos el body, solo verificar que el status
/// sea 2xx y que, si no lo es, el body se traduzca a `ClientError`.
async fn parse_unit(resp: reqwest::Response) -> Result<(), ClientError> {
    let status = resp.status();
    if status.is_success() {
        // Drenamos el body para liberar la conexión al pool, pero no lo
        // inspeccionamos: los endpoints de delete devuelven `{"deleted":
        // ...}` y esa información no es relevante para el llamador.
        let _ = resp.bytes().await?;
        Ok(())
    } else {
        let bytes = resp.bytes().await?;
        Err(map_error_body(status.as_u16(), &bytes))
    }
}

/// Convierte un body de error HTTP en un [`ClientError`].
///
/// Intenta primero el sobre estándar del server
/// (`{"error": {"code": "...", "message": "..."}}`). Si lo reconoce,
/// mapea el `code` a [`CoreError`]; si no, conserva `code` y `message`
/// en [`ClientError::Status`]. Si el body ni siquiera es JSON, devuelve
/// [`ClientError::Status`] con el cuerpo crudo.
fn map_error_body(status: u16, bytes: &[u8]) -> ClientError {
    #[derive(serde::Deserialize)]
    struct Envelope {
        error: EnvelopeError,
    }
    #[derive(serde::Deserialize)]
    struct EnvelopeError {
        code: String,
        message: String,
    }

    if let Ok(env) = serde_json::from_slice::<Envelope>(bytes) {
        match core_error_from_code(&env.error.code, &env.error.message) {
            Some(core_err) => return ClientError::Api(core_err),
            None => {
                return ClientError::Status(
                    status,
                    format!("{}: {}", env.error.code, env.error.message),
                );
            }
        }
    }

    // Body no es JSON o no encaja en el sobre. Reportamos el cuerpo crudo
    // (truncado) para diagnóstico.
    let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    ClientError::Status(status, snippet.into_owned())
}

/// Mapea un `code` textual de la API al variante correspondiente de
/// [`CoreError`]. Devuelve `None` si el `code` no se reconoce, en cuyo
/// caso el llamador decide si preservarlo como `Status` o tratarlo como
/// un error de servidor genérico.
///
/// Solo conocemos los códigos que [`CoreError::code`] puede emitir —
/// cualquier otra cosa (e.g. códigos personalizados del server para
/// cosas que aún no se han modelado en core) se trata como desconocida.
fn core_error_from_code(code: &str, message: &str) -> Option<CoreError> {
    match code {
        "auth" => Some(CoreError::Auth(message.to_string())),
        "validation" => Some(CoreError::Validation(message.to_string())),
        "provider_not_found" => Some(CoreError::ProviderNotFound(message.to_string())),
        "account_not_found" => parse_i64(message).map(CoreError::AccountNotFound),
        "combo_not_found" => parse_i64(message).map(CoreError::ComboNotFound),
        "model_not_found" => Some(CoreError::ModelNotFound {
            // El server codifica `provider=... model=...` en el Display;
            // no podemos desambiguarlo con certeza sin parsear el formato
            // concreto, así que pasamos el mensaje crudo. La alternativa
            // sería añadir un campo estructurado al envelope.
            provider: "<see message>".to_string(),
            model: message.to_string(),
        }),
        "no_healthy_targets" => parse_i64(message).map(CoreError::NoHealthyTargets),
        "upstream_timeout" => Some(CoreError::UpstreamTimeout {
            // El mensaje del server es "upstream timeout in phase X after Nms".
            // No intentamos parsearlo: dejamos phase="<unknown>" y copiamos
            // el mensaje crudo al sidecar del Display vía el mensaje.
            phase: "<unknown>".to_string(),
            ms: 0,
        }),
        "upstream_connection" => Some(CoreError::UpstreamConnection(message.to_string())),
        "upstream_error" => Some(CoreError::UpstreamError {
            status: 0,
            provider: "<see message>".to_string(),
            model: "<see message>".to_string(),
            body: message.to_string(),
        }),
        "rate_limited" => Some(CoreError::RateLimited {
            provider: "<see message>".to_string(),
            retry_after_ms: 0,
        }),
        "parse_error" => Some(CoreError::Parse(message.to_string())),
        "client_disconnected" => Some(CoreError::ClientDisconnected),
        "race_lost" => Some(CoreError::RaceLost),
        "database" | "migration" => Some(CoreError::Internal(message.to_string())),
        "config" => Some(CoreError::Config(message.to_string())),
        "internal" => Some(CoreError::Internal(message.to_string())),
        _ => None,
    }
}

fn parse_i64(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

/// Construye un query string a partir de pares `(clave, valor)`. Las claves
/// con valor `None` se omiten. Las que sí tienen valor se codifican con
/// `urlencoded` (mínimo: espacios, `&`, `=`). No se usa
/// `serde_urlencoded` para no añadir un crate nuevo al workspace.
fn build_query(pairs: &[(&str, Option<&str>)]) -> String {
    let mut out = String::new();
    let mut first = true;
    for (k, v) in pairs {
        if let Some(val) = v {
            if !first {
                out.push('&');
            }
            first = false;
            out.push_str(k);
            out.push('=');
            out.push_str(&urlencoded(val));
        }
    }
    out
}

/// Serializa un [`UsageFilter`] al query string esperado por
/// `GET /admin/usage/*`. Coincide 1:1 con los campos de
/// `handlers::admin::UsageQuery` en el server.
fn usage_filter_query(f: &UsageFilter) -> String {
    let pairs: [(&str, Option<String>); 6] = [
        ("from", f.from.clone()),
        ("to", f.to.clone()),
        ("provider_id", f.provider_id.as_ref().map(|p| p.0.clone())),
        ("model_id", f.model_id.clone()),
        ("account_id", f.account_id.map(|a| a.0.to_string())),
        ("combo_id", f.combo_id.map(|c| c.0.to_string())),
    ];
    let borrowed: Vec<(&str, Option<&str>)> =
        pairs.iter().map(|(k, v)| (*k, v.as_deref())).collect();
    build_query(&borrowed)
}

/// Percent-encoding mínimo para un único valor de query string.
///
/// Cubre los caracteres que pueden aparecer en identificadores, fechas
/// ISO-8601, y nombres de modelos (`anthropic/claude-sonnet-4`,
/// `openai/gpt-4o`, etc.). No intenta ser RFC-3986-completo — si el
/// llamador mete caracteres más exóticos, preferimos aceptar el riesgo
/// de un 400 limpio del server antes que añadir un crate nuevo.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            // unreserved (RFC 3986 §2.3)
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            // sub-delims y gen-delims que no se rompen en práctica
            b':' | b'/' => out.push(*b as char),
            // todo lo demás se escapa como %XX
            _ => {
                out.push('%');
                let hi = (*b >> 4) & 0x0f;
                let lo = *b & 0x0f;
                out.push(hex_digit(hi));
                out.push(hex_digit(lo));
            }
        }
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + (n - 10)) as char,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_slash() {
        let c = Client::new("http://example.com/");
        assert_eq!(c.url("/admin/health"), "http://example.com/admin/health");
    }

    #[test]
    fn urlencoded_keeps_unreserved() {
        assert_eq!(urlencoded("openrouter"), "openrouter");
        assert_eq!(urlencoded("openai/gpt-4o"), "openai/gpt-4o");
        assert_eq!(urlencoded("2026-01-15T00:00:00Z"), "2026-01-15T00:00:00Z");
    }

    #[test]
    fn urlencoded_escapes_spaces_and_amp() {
        assert_eq!(urlencoded("a b"), "a%20b");
        assert_eq!(urlencoded("a&b"), "a%26b");
    }

    #[test]
    fn build_query_skips_nones() {
        let q = build_query(&[
            ("from", Some("2026-01-01T00:00:00Z")),
            ("to", None),
            ("provider_id", Some("openrouter")),
        ]);
        assert_eq!(q, "from=2026-01-01T00:00:00Z&provider_id=openrouter");
    }

    #[test]
    fn usage_filter_query_serializes_known_fields() {
        let f = UsageFilter {
            from: Some("2026-01-01T00:00:00Z".to_string()),
            to: None,
            provider_id: Some(ProviderId::new("openrouter")),
            model_id: Some("openai/gpt-4o".to_string()),
            account_id: Some(AccountId::new(7)),
            combo_id: None,
            api_key_id: None,
        };
        let q = usage_filter_query(&f);
        assert!(q.contains("from=2026-01-01T00:00:00Z"));
        assert!(q.contains("provider_id=openrouter"));
        assert!(q.contains("model_id=openai/gpt-4o"));
        assert!(q.contains("account_id=7"));
        assert!(!q.contains("combo_id="));
        assert!(!q.contains("to="));
    }

    #[test]
    fn map_error_body_recognizes_known_codes() {
        let body = serde_json::json!({
            "error": { "code": "validation", "message": "bad input" }
        })
        .to_string();
        let bytes = body.as_bytes();
        let err = map_error_body(400, bytes);
        match err {
            ClientError::Api(CoreError::Validation(msg)) => assert_eq!(msg, "bad input"),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn map_error_body_unknown_code_falls_back_to_status() {
        let body = serde_json::json!({
            "error": { "code": "made_up_code", "message": "wat" }
        })
        .to_string();
        let err = map_error_body(500, body.as_bytes());
        match err {
            ClientError::Status(500, msg) => assert!(msg.contains("made_up_code")),
            other => panic!("expected Status, got {:?}", other),
        }
    }

    #[test]
    fn map_error_body_non_json_falls_back_to_status() {
        let err = map_error_body(502, b"<html>oops</html>");
        match err {
            ClientError::Status(502, msg) => assert!(msg.contains("oops")),
            other => panic!("expected Status, got {:?}", other),
        }
    }
}
