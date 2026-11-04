//! openproxy-api-client: cliente HTTP para la admin API de openproxy.
//!
//! Consume los endpoints `/admin/*` de un openproxy-server corriendo.
//! Se usa desde scripts externos y automatización (el dashboard SPA se
//! sirve desde el propio binario openproxy-server vía rust-embed, así que
//! ya no hay un crate `openproxy-web` que lo consuma internamente).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! ## Forma de uso
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use openproxy_api_client::Client;
//! use openproxy_core::usage::UsageFilter;
//! use openproxy_types::ids::ProviderId;
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
//! - `Http` — fallo de transporte (red, DNS, TLS) propagado del UpstreamClient.
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
    accounts,
    admin::{
        AddTargetInput, BulkCreateAccountsInput, BulkCreateAccountsResponse, CreateAccountInput,
        CreateComboInput, CreateProviderInput, UpdateAccountApiKeyInput,
    },
    analytics::{LatencyPercentiles, RaceStats},
    providers,
    usage::{ByAccountRow, ByModelRow, ByStatusRow, ErrorRow, UsageFilter, UsageSummary},
};
use openproxy_types::combos;
use openproxy_types::{
    CoreError,
    ids::{AccountId, ComboId, ModelRowId, ProviderId},
};
use std::fmt::Write as _;

#[derive(serde::Deserialize)]
struct IdEnvelope<T> {
    id: T,
}

#[derive(serde::Deserialize)]
struct TouchedEnvelope {
    touched: usize,
}

#[derive(Clone)]
pub struct Client {
    base_url: String,
    http: std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
}

impl Client {
    /// Construye un cliente con un `UpstreamClient` por defecto.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_client(
            base_url,
            openproxy_adapters::upstream::UpstreamClient::new(),
        )
    }

    /// Construye un cliente compartiendo un `UpstreamClient` propio.
    ///
    /// Para timeouts, TLS, proxies o un pool de conexiones a nivel de aplicación.
    pub fn with_client(
        base_url: impl Into<String>,
        http: std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
    ) -> Self {
        let base = base_url.into();
        let base_url = base.trim_end_matches('/').to_string();
        Self { base_url, http }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn req(
        &self,
        req: openproxy_adapters::upstream::UpstreamRequest,
    ) -> Result<openproxy_adapters::upstream::UpstreamResponse, ClientError> {
        let cancel = openproxy_adapters::upstream::CancellationToken::new();
        self.http
            .call(
                req,
                openproxy_adapters::upstream::TimeoutProfile::Quota,
                cancel,
            )
            .await
            .map_err(|e| ClientError::Http(e.to_string()))
    }

    async fn get(
        &self,
        path: &str,
    ) -> Result<openproxy_adapters::upstream::UpstreamResponse, ClientError> {
        self.req(openproxy_adapters::upstream::UpstreamRequest::get(
            self.url(path),
        ))
        .await
    }

    async fn delete(
        &self,
        path: &str,
    ) -> Result<openproxy_adapters::upstream::UpstreamResponse, ClientError> {
        let mut r = openproxy_adapters::upstream::UpstreamRequest::get(self.url(path));
        r.method = http::Method::DELETE;
        self.req(r).await
    }

    async fn post_json(
        &self,
        path: &str,
        body: impl serde::Serialize,
    ) -> Result<openproxy_adapters::upstream::UpstreamResponse, ClientError> {
        let b = bytes::Bytes::from(serde_json::to_vec(&body)?);
        self.req(openproxy_adapters::upstream::UpstreamRequest::post_json(
            self.url(path),
            b,
        ))
        .await
    }

    async fn put_json(
        &self,
        path: &str,
        body: impl serde::Serialize,
    ) -> Result<openproxy_adapters::upstream::UpstreamResponse, ClientError> {
        let b = bytes::Bytes::from(serde_json::to_vec(&body)?);
        let mut req = openproxy_adapters::upstream::UpstreamRequest::post_json(self.url(path), b);
        req.method = http::Method::PUT;
        self.req(req).await
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let resp = self.get(path).await?;
        parse_json(resp).await
    }

    async fn post_json_resp<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: impl serde::Serialize,
    ) -> Result<T, ClientError> {
        let resp = self.post_json(path, body).await?;
        parse_json(resp).await
    }

    async fn put_json_unit(
        &self,
        path: &str,
        body: impl serde::Serialize,
    ) -> Result<(), ClientError> {
        let resp = self.put_json(path, body).await?;
        parse_unit(resp).await
    }

    async fn delete_unit(&self, path: &str) -> Result<(), ClientError> {
        let resp = self.delete(path).await?;
        parse_unit(resp).await
    }

    // Providers

    /// `POST /admin/providers`. Devuelve el `ProviderId` recién creado.
    pub async fn create_provider(
        &self,
        input: CreateProviderInput,
    ) -> Result<ProviderId, ClientError> {
        let env: IdEnvelope<String> = self.post_json_resp("/admin/providers", &input).await?;
        Ok(ProviderId::new(env.id))
    }

    // Accounts

    /// `GET /admin/accounts[?provider_id=...]`.
    pub async fn list_accounts(
        &self,
        provider: Option<&ProviderId>,
    ) -> Result<Vec<accounts::Account>, ClientError> {
        let mut url = self.url("/admin/accounts");
        if let Some(p) = provider {
            let qs = build_query(&[("provider_id", Some(p.as_str()))]);
            url.push('?');
            url.push_str(&qs);
        }
        let resp = self
            .req(openproxy_adapters::upstream::UpstreamRequest::get(url))
            .await?;
        parse_json(resp).await
    }

    /// `POST /admin/accounts`. Devuelve el `AccountId` recién creado.
    pub async fn create_account(
        &self,
        input: CreateAccountInput,
    ) -> Result<AccountId, ClientError> {
        let env: IdEnvelope<i64> = self.post_json_resp("/admin/accounts", &input).await?;
        Ok(AccountId::new(env.id))
    }

    /// `POST /admin/accounts/bulk`. Crea múltiples cuentas en lote.
    pub async fn bulk_create_accounts(
        &self,
        input: BulkCreateAccountsInput,
    ) -> Result<BulkCreateAccountsResponse, ClientError> {
        let resp = self.post_json("/admin/accounts/bulk", &input).await?;
        parse_json(resp).await
    }

    /// `PUT /admin/accounts/:id/api-key`. Encripta y guarda (o limpia)
    /// la API key de una cuenta existente.
    pub async fn update_account_api_key(
        &self,
        id: AccountId,
        input: UpdateAccountApiKeyInput,
    ) -> Result<(), ClientError> {
        let path = format!("/admin/accounts/{}/api-key", id.0);
        self.put_json_unit(&path, &input).await
    }

    /// `POST /admin/accounts/:id/active`. Sets whether this account is active for request routing.
    pub async fn set_account_active(&self, id: AccountId, active: bool) -> Result<(), ClientError> {
        let path = format!("/admin/accounts/{}/active", id.0);
        let _ = self
            .post_json(&path, &serde_json::json!({ "active": active }))
            .await?;
        Ok(())
    }

    // Combos

    /// `POST /admin/combos`. Devuelve el `ComboId` recién creado.
    pub async fn create_combo(&self, input: CreateComboInput) -> Result<ComboId, ClientError> {
        let env: IdEnvelope<i64> = self.post_json_resp("/admin/combos", &input).await?;
        Ok(ComboId(env.id))
    }

    /// `GET /admin/combos/:id/targets`.
    pub async fn list_combo_targets(
        &self,
        combo_id: ComboId,
    ) -> Result<Vec<combos::ComboTarget>, ClientError> {
        let path = format!("/admin/combos/{}/targets", combo_id.0);
        self.get_json(&path).await
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
        let env: IdEnvelope<i64> = self.post_json_resp(&path, &input).await?;
        Ok(env.id)
    }

    // Models

    /// `POST /admin/models/:id/refresh`.
    ///
    /// El server indexa por fila de la tabla `models`, así que el parámetro es
    /// un `ModelRowId` (no un `ProviderId`). Devuelve las filas tocadas
    /// (inserts + updates) que reporta el server.
    pub async fn refresh_models(&self, model_row_id: ModelRowId) -> Result<usize, ClientError> {
        let path = format!("/admin/models/{}/refresh", model_row_id.0);
        let env: TouchedEnvelope = self.post_json_resp(&path, serde_json::json!({})).await?;
        Ok(env.touched)
    }

    // Usage analytics

    async fn get_analytics<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        filter: &UsageFilter,
    ) -> Result<T, ClientError> {
        let url = format!("{}?{}", self.url(endpoint), usage_filter_query(filter));
        let resp = self
            .req(openproxy_adapters::upstream::UpstreamRequest::get(url))
            .await?;
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
            let _ = write!(&mut qs, "&limit={limit}");
        } else {
            let _ = write!(&mut qs, "limit={limit}");
        }
        let url = format!("{}?{}", self.url("/admin/usage/errors"), qs);
        let resp = self
            .req(openproxy_adapters::upstream::UpstreamRequest::get(url))
            .await?;
        parse_json(resp).await
    }
}

/// Macro declarativa para generar métodos CRUD estándar del cliente SDK sin duplicación.
macro_rules! impl_client_crud_methods {
    (
        $(#[$get_doc:meta])*
        get $get_fn:ident ( $get_path:literal ) -> $get_ret:ty;
        $($rest:tt)*
    ) => {
        impl Client {
            $(#[$get_doc])*
            pub async fn $get_fn(&self) -> Result<$get_ret, ClientError> {
                self.get_json($get_path).await
            }
        }
        impl_client_crud_methods! { $($rest)* }
    };

    (
        $(#[$del_doc:meta])*
        delete $del_fn:ident ( $del_id:ident : $del_id_ty:ty => $del_path:expr );
        $($rest:tt)*
    ) => {
        impl Client {
            $(#[$del_doc])*
            pub async fn $del_fn(&self, $del_id: $del_id_ty) -> Result<(), ClientError> {
                let path = $del_path;
                self.delete_unit(&path).await
            }
        }
        impl_client_crud_methods! { $($rest)* }
    };

    (
        $(#[$analytics_doc:meta])*
        analytics $analytics_fn:ident ( $analytics_path:literal ) -> $analytics_ret:ty;
        $($rest:tt)*
    ) => {
        impl Client {
            $(#[$analytics_doc])*
            pub async fn $analytics_fn(&self, f: &UsageFilter) -> Result<$analytics_ret, ClientError> {
                self.get_analytics($analytics_path, f).await
            }
        }
        impl_client_crud_methods! { $($rest)* }
    };

    () => {};
}

impl_client_crud_methods! {
    /// `GET /admin/health` — liveness con tag de versión.
    get health("/admin/health") -> serde_json::Value;

    /// `GET /admin/providers`.
    get list_providers("/admin/providers") -> Vec<providers::Provider>;

    /// `GET /admin/combos`.
    get list_combos("/admin/combos") -> Vec<combos::Combo>;

    /// `GET /v1/models` (endpoint público, no `/admin/...`).
    ///
    /// Tipo laxo `serde_json::Value`: el shape exacto de la lista de modelos
    /// no ata al cliente a una versión concreta del endpoint.
    get list_models("/v1/models") -> serde_json::Value;

    /// `DELETE /admin/providers/:id`. Idempotente.
    delete delete_provider(id: &ProviderId => format!("/admin/providers/{}", urlencoded(id.as_str())));

    /// `DELETE /admin/accounts/:id`. Idempotente.
    delete delete_account(id: AccountId => format!("/admin/accounts/{}", id.0));

    /// `DELETE /admin/combos/:id`. Idempotente.
    delete delete_combo(id: ComboId => format!("/admin/combos/{}", id.0));

    /// `GET /admin/usage/summary?from=...&to=...&provider_id=...&...`.
    analytics usage_summary("/admin/usage/summary") -> UsageSummary;

    /// `GET /admin/usage/by-model?from=...&...`.
    analytics usage_by_model("/admin/usage/by-model") -> Vec<ByModelRow>;

    /// `GET /admin/usage/by-account?from=...&...`.
    analytics usage_by_account("/admin/usage/by-account") -> Vec<ByAccountRow>;

    /// `GET /admin/usage/by-status?from=...&...`.
    analytics usage_by_status("/admin/usage/by-status") -> Vec<ByStatusRow>;

    /// `GET /admin/usage/latency?from=...&...`.
    analytics usage_latency("/admin/usage/latency") -> LatencyPercentiles;

    /// `GET /admin/usage/races?from=...&...`.
    analytics usage_races("/admin/usage/races") -> RaceStats;
}

impl Client {
    // Backup & Restore

    /// `GET /admin/api/backup/export` (or `?passphrase=...`).
    ///
    /// Exports all providers, accounts, credentials, models, combos, targets,
    /// proxy sources, API keys, and runtime configuration into a [`BackupBundle`].
    pub async fn export_backup(
        &self,
        passphrase: Option<&str>,
    ) -> Result<openproxy_types::backup::BackupBundle, ClientError> {
        let path = match passphrase {
            Some(p) if !p.is_empty() => {
                format!("/admin/api/backup/export?passphrase={}", urlencoded(p))
            }
            _ => "/admin/api/backup/export".to_string(),
        };
        self.get_json(&path).await
    }

    /// `POST /admin/api/backup/validate`.
    ///
    /// Validates a backup bundle (and passphrase if encrypted) without modifying
    /// any database state.
    pub async fn validate_backup(
        &self,
        bundle: &openproxy_types::backup::BackupBundle,
        passphrase: Option<&str>,
    ) -> Result<openproxy_types::backup::BackupValidationSummary, ClientError> {
        let body = serde_json::json!({
            "passphrase": passphrase,
            "bundle": bundle,
        });
        self.post_json_resp("/admin/api/backup/validate", body)
            .await
    }

    /// `POST /admin/api/backup/restore`.
    ///
    /// Atomically restores database state from a backup bundle inside a transaction.
    /// Creates a safety backup before applying changes.
    pub async fn restore_backup(
        &self,
        bundle: &openproxy_types::backup::BackupBundle,
        passphrase: Option<&str>,
        mode: Option<&str>,
    ) -> Result<openproxy_types::backup::RestoreReport, ClientError> {
        let body = serde_json::json!({
            "passphrase": passphrase,
            "mode": mode,
            "bundle": bundle,
        });
        self.post_json_resp("/admin/api/backup/restore", body).await
    }
}

// Error type

/// Errores que puede devolver cualquier método del [`Client`].
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Fallo de transporte (red, DNS, TLS, timeout). Heredado del UpstreamClient.
    #[error("http: {0}")]
    Http(String),

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

// Internals

/// Inspecciona el `status` y el body de una respuesta y la entrega a uno
/// de tres destinos:
///
/// 1. `2xx` y body JSON deserializable a `T` → `Ok(T)`.
/// 2. `4xx/5xx` con body `{"error": {"code", "message"}}` → mapea el
///    `code` a [`CoreError`] y lo envuelve en [`ClientError::Api`]. Un
///    `code` no reconocido devuelve [`ClientError::Status`] con el código
///    y el mensaje crudos.
/// 3. `4xx/5xx` con body que no encaja en el sobre → [`ClientError::Status`].
async fn collect_response_bytes(
    resp: openproxy_adapters::upstream::UpstreamResponse,
) -> Result<(http::StatusCode, bytes::Bytes), ClientError> {
    let status = resp.status;
    let bytes = resp
        .collect()
        .await
        .map_err(|e| ClientError::Http(e.to_string()))?;
    if status.is_success() {
        Ok((status, bytes))
    } else {
        Err(map_error_body(status.as_u16(), &bytes))
    }
}

async fn parse_json<T: serde::de::DeserializeOwned>(
    resp: openproxy_adapters::upstream::UpstreamResponse,
) -> Result<T, ClientError> {
    let (_, bytes) = collect_response_bytes(resp).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn parse_unit(
    resp: openproxy_adapters::upstream::UpstreamResponse,
) -> Result<(), ClientError> {
    collect_response_bytes(resp).await.map(|_| ())
}

/// Convierte un body de error HTTP en un [`ClientError`].
///
/// Reconoce el sobre estándar del server
/// (`{"error": {"code": "...", "message": "..."}}`) y mapea el `code` a
/// [`CoreError`]. Un `code` desconocido conserva `code` y `message` en
/// [`ClientError::Status`]. Un body que no es JSON se reporta truncado.
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
        if let Some(core_err) =
            CoreError::from_code_and_message(&env.error.code, &env.error.message)
        {
            return ClientError::Api(core_err);
        }
        return ClientError::Status(status, format!("{}: {}", env.error.code, env.error.message));
    }

    // Body no es JSON o no encaja en el sobre. Reportamos el cuerpo crudo
    // (truncado) para diagnóstico.
    let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    ClientError::Status(status, snippet.into_owned())
}

/// Construye un query string a partir de pares `(clave, valor)`. Las claves
/// con valor `None` se omiten. `serde_urlencoded` no se usa para no añadir
/// un crate al workspace.
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
    let account_id_str = f.account_id.map(|a| a.0.to_string());
    let combo_id_str = f.combo_id.map(|c| c.0.to_string());

    let pairs: [(&str, Option<&str>); 6] = [
        ("from", f.from.as_deref()),
        ("to", f.to.as_deref()),
        ("provider_id", f.provider_id.as_ref().map(|p| p.0.as_str())),
        ("model_id", f.model_id.as_deref()),
        ("account_id", account_id_str.as_deref()),
        ("combo_id", combo_id_str.as_deref()),
    ];

    build_query(&pairs)
}

/// Percent-encoding mínimo para un único valor de query string.
///
/// Cubre lo que aparece en identificadores, fechas ISO-8601 y nombres de
/// modelos. Fuera de RFC 3986: preferimos un 400 limpio del server antes
/// que añadir un crate.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            // unreserved (RFC 3986 §2.3)
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            // gen-delims que no se rompen en la práctica
            b':' | b'/' => out.push(*b as char),
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
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    HEX.get(n as usize).map_or('0', |&b| b as char)
}

#[cfg(test)]
mod tests;
