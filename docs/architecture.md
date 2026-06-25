# openproxy — Architecture

## 1. Vision and Principles

**openproxy** is a headless, minimal LLM proxy/router written in Rust. It is inspired by
OmniRoute but deliberately stripped of non-essential machinery. It accepts OpenAI-compatible
chat completion requests, routes them across multiple upstream providers and accounts, and
exposes operational telemetry. The dashboard SPA is embedded into the server binary at
compile time via `rust-embed`, so a single `openproxy` binary serves both the API and the
admin UI on the same port.

### Guiding principles

1. **Single binary by default.** The server runs as one self-contained binary that serves
   the OpenAI-compatible API, the admin REST API, and the dashboard SPA on the same port.
2. **Bloat is a bug.** Every feature must justify its weight. We start with three providers
   and two routing strategies. Nothing more.
3. **Live discovery, no hardcoding.** Models are fetched from upstream at runtime. Adding a
   new model does not require a recompile.
4. **Explicit over implicit.** Timeouts, retries, headers, and costs are all named
   constants or config — never magic numbers buried in code.
5. **Observable.** Every request carries a `request_id` / `trace_id` from edge to provider
   and is logged with structured fields.
6. **Composable crates.** The workspace is split so that the server has zero coupling to
   any UI; the UI has zero coupling to provider internals.
7. **Deterministic streaming.** SSE is byte-passthrough with translation at the boundary,
   and each chunk is traceable.

## 2. ASCII Architecture Diagram

```
                       ┌──────────────────────────────────────────────┐
                       │           openproxy-server (axum)            │
   HTTP/SSE in  ───────►  /v1/chat/completions   /v1/models          │
   (OpenAI comp.)        /v1/admin/* (api-key guarded)               │
                        /admin/* (dashboard SPA, rust-embedded)      │
                       └────────────┬─────────────────────────────────┘
                                    │
                                    ▼
                       ┌──────────────────────────────────────────────┐
                       │              openproxy-core                  │
                       │                                              │
                       │  ┌────────────┐  ┌────────────┐  ┌────────┐  │
                       │  │ Translation│  │  Combo     │  │ SSE    │  │
                       │  │  Layer     │  │  Engine    │  │ Pipe   │  │
                       │  │ OpenAI<->  │  │ priority / │  │        │  │
                       │  │ Anthropic  │  │ round_robin│  │        │  │
                       │  └─────┬──────┘  └─────┬──────┘  └────┬───┘  │
                       │        │               │              │      │
                       │  ┌─────▼───────────────▼──────────────▼───┐  │
                       │  │       Provider Adapter Registry        │  │
                       │  │  OpenRouter │ MiniMax │ OpenCode Zen    │  │
                       │  └────────────────────┬───────────────────┘  │
                       │                       │                      │
                       │  ┌────────────────────▼───────────────────┐  │
                       │  │   Cost · Analytics · SQLite · Tracing  │  │
                       │  └────────────────────────────────────────┘  │
                       └────────────┬─────────────────────────────────┘
                                    │                │
                          (rustls/reqwest)    (rusqlite)
                                    │                │
                                    ▼                ▼
                       ┌────────────────┐  ┌────────────────────┐
                       │  Upstream APIs │  │  openproxy.db      │
                       │  OpenRouter    │  │  providers,        │
                       │  MiniMax       │  │  accounts, models, │
                       │  OpenCode Zen  │  │  combos, usage     │
                       └────────────────┘  └────────────────────┘

   The dashboard SPA (lit-html + TypeScript, in `crates/openproxy-server/web/`)
   is compiled to a JS bundle by `pnpm build` (esbuild) and embedded into the
   `openproxy-server` binary via `rust-embed` at compile time — there is no
   separate `openproxy-web` crate anymore. The optional `openproxy-api-client`
   crate remains for external automation scripts that want a typed wrapper
   around the admin REST API.

## 3. Crate Boundaries and Responsibilities

### `openproxy-core`
- Provider adapter trait + implementations (OpenRouter, MiniMax, OpenCode Zen).
- Translation layer between OpenAI Chat Completions and Anthropic Messages.
- Combo engine: `priority` and `round_robin` selection algorithms.
- Account rotation within a provider.
- Live model discovery (periodic refresh + on-demand).
- Cost calculation (per-token pricing table + formula).
- SQLite access layer (rusqlite, r2d2 pool, prepared statements).
- Analytics: aggregated queries over `usage`.
- SSE pipeline: parser, emitter, trace context injection.
- Tracing/logging plumbing (`request_id`, `trace_id`).
- **No HTTP server code lives here.**

### `openproxy-server`
- axum router and handlers.
- Bound to a TCP port; serves the public proxy endpoints, the admin API, and the dashboard SPA.
- Public endpoints: `POST /v1/chat/completions`, `GET /v1/models`.
- Admin endpoints: `/admin/api/providers`, `/admin/api/accounts`, `/admin/api/combos`,
  `/admin/api/usage/*` (the five analytics endpoints from mvp-spec §7),
  `/admin/api/refresh-models` — guarded by a hashed API key in the `api_keys` table.
- Dashboard SPA: `GET /admin/*` serves the embedded frontend (lit-html + TypeScript,
  bundled by esbuild, embedded via `rust-embed`).
- Wires together the request pipeline: parse → translate → select combo → pick account
  → forward → stream response → record usage.
- The frontend source tree lives in `crates/openproxy-server/web/` and is built by `pnpm build`.

### `openproxy-api-client`
- Rust client that wraps the admin endpoints of `openproxy-server`.
- Used by external automation scripts and integrations.
- No business logic; no direct DB or provider access.
- Depends only on `openproxy-core` for shared types (e.g. enums, DTOs).

## 4. Dependency DAG

The workspace is a strict acyclic graph. Cycles are rejected at compile time.

```
openproxy-server  ───►  openproxy-core
openproxy-api-client  ───►  openproxy-core
openproxy-core    ───►  (no internal crates; only external deps)
```

External dependency direction is one-way: the server and the api-client crate are leaves
with respect to core. The dashboard SPA is embedded into the server binary, so there is no
runtime wire hop between the UI and the API.

## 5. Provider Adapter Trait

Every provider implementation in `openproxy-core::providers` conforms to a single trait.
Adding a new provider means implementing this trait, registering the factory, and
optionally contributing a translator.

### Format resolution (source of truth)

The wire format used to talk to a provider is resolved **per request** by:

1. If `provider.format == "openai"` → `TargetFormat::OpenAI`.
2. If `provider.format == "anthropic"` → `TargetFormat::Anthropic`.
3. If `provider.format == "mixed"` → `model.target_format` (per-row on `models`).

The resolved `TargetFormat` is the single value threaded through `build_url`,
`translator_for`, and `sse_normalizer_for`. This is the only way adapters learn
the format; they do not consult `provider.format` or `model.target_format`
themselves.

```rust
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// Stable identifier, e.g. "openrouter".
    fn id(&self) -> &'static str;

    /// Authorization headers derived from the account's API key.
    fn auth_headers(&self, account: &Account) -> Vec<(HeaderName, HeaderValue)>;

    /// Extra static headers (e.g. Anthropic-Version, HTTP-Referer).
    fn extra_headers(&self) -> Vec<(HeaderName, HeaderValue)>;

    /// Translator for the model being served. For OpenCode Zen this is
    /// dispatched per model (OpenAI vs Anthropic); for fixed-format providers
    /// (`format = "openai"` or `"anthropic"`) the choice is constant.
    fn translator_for(&self, model: &Model) -> Arc<dyn Translator>;

    /// SSE normalizer for the model being served. Per-model dispatch for
    /// OpenCode Zen; constant for fixed-format providers.
    fn sse_normalizer_for(&self, model: &Model) -> Arc<dyn SseNormalizer>;

    /// Fetch the live model list for the given account, returning normalized
    /// `DiscoveredModel` rows. Implementations issue the HTTP call internally
    /// and apply the provider's auth/extra headers. Returns an empty `Vec` if
    /// the provider has no model list endpoint (e.g. MiniMax Coding).
    async fn fetch_models(
        &self,
        account: &Account,
    ) -> Result<Vec<DiscoveredModel>, ProviderError>;

    /// Build the upstream request URL for a given (account, model, route).
    /// `target_format` is the **resolved** format per the rules above; for
    /// Anthropic routes the adapter must append `/v1/messages` and for
    /// OpenAI routes `/v1/chat/completions` (or the provider's equivalent).
    fn build_url(
        &self,
        account: &Account,
        model: &Model,
        target_format: TargetFormat,
    ) -> Result<Url, ProviderError>;
}
```

### `TargetFormat`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFormat {
    OpenAI,
    Anthropic,
}
```

`build_url`, `translator_for`, and `sse_normalizer_for` all take this enum so
that OpenCode Zen's mixed format can dispatch per model without leaking the
`providers.format = "mixed"` rule into every callsite.

### Plug-in lifecycle

1. Implement the trait in a new module under `openproxy-core/src/providers/<name>.rs`.
2. Register the constructor in `openproxy-core::providers::REGISTRY` (a function-pointer
   table keyed by `Provider::id`).
3. If the provider is non-OpenAI-shaped, also implement `Translator` and `SseNormalizer`.
4. Add a unit-test fixture: a sample request, expected translated request, expected
   translated response.

No reflection, no runtime plugin loading — Rust's type system is the registry.

## 6. Translation Layer

The translation layer sits at the boundary between the public OpenAI Chat Completions
contract and the upstream provider's native format. Only two formats exist in MVP:

- **OpenAI Chat Completions** (used by OpenRouter, and by OpenCode Zen for non-Claude models).
- **Anthropic Messages** (used by MiniMax Coding, and by OpenCode Zen for Claude models).

A future Gemini format will plug in by implementing the same `Translator` trait.

```rust
#[async_trait]
pub trait Translator: Send + Sync {
    /// Convert an OpenAI Chat Completions request into the upstream native request.
    async fn to_upstream(
        &self,
        openai_req: &ChatCompletionRequest,
        model: &Model,
    ) -> Result<UpstreamRequest, TranslateError>;

    /// Convert an upstream native non-streaming response into OpenAI Chat Completions.
    async fn from_upstream_response(
        &self,
        upstream: UpstreamResponse,
        model: &str,
    ) -> Result<ChatCompletionResponse, TranslateError>;

    /// Normalize an upstream SSE event into an OpenAI `chat.completion.chunk`.
    /// Returns `None` to indicate "drop this event".
    fn normalize_sse_event(
        &self,
        raw: &[u8],
        state: &mut SseState,
    ) -> Result<Option<Vec<ChatCompletionChunk>>, TranslateError>;
}
```

### Translation rules (Anthropic Messages)

- `system` messages are extracted to the top-level `system` field.
- `max_tokens` is required; we inject a default from config when absent.
- `stop_reason` is mapped: `end_turn` → `stop`, `max_tokens` → `length`,
  `tool_use` → `tool_calls` (future), other → `stop`.
- `usage.input_tokens` → `prompt_tokens`, `usage.output_tokens` → `completion_tokens`.
- SSE event names `message_start`, `content_block_start`, `content_block_delta`,
  `content_block_stop`, `message_delta`, `message_stop` are folded into OpenAI
  `chat.completion.chunk` events with delta deltas.

## 7. SSE Streaming Pipeline

The pipeline is a series of pure stages. Each stage takes a byte stream and yields a
byte stream. Trace context is threaded through, not parsed from payload.

```
client bytes
   │
   ▼
[ axum body ingest ] ── emits UpstreamEvent { raw, trace_id }
   │
   ▼
[ SseParser ] ─────────── byte-level SSE framing (event:, data:, [DONE])
   │
   ▼
[ SseNormalizer ] ────── provider-specific → ChatCompletionChunk
   │
   ▼
[ axum SSE response ] ── Content-Type: text/event-stream
```

Properties:

- **Deterministic.** The same input bytes produce the same output bytes modulo timing.
- **Traceable.** The `x-request-id` is exposed **only** as an HTTP response header
  on the SSE response, not embedded in the SSE body. Trace context is threaded
  through the pipeline as an in-process value and emitted in structured logs at
  chunk boundaries (`first`, `last`, `error`).
- **Bounded memory.** Frames are parsed in-place; no full upstream response is buffered.
- **Cancellable.** When the client disconnects, the upstream request is aborted
  (Hyper body drop), and the usage record is closed with `status_code = 499`.

## 8. Timeout Model

Timeouts are explicit per phase, configured in TOML, and applied as a layered
`tokio::time::timeout` wrapping. They are never implicit.

### Request phases

| Phase          | Metric                       | Default | Override                              |
|----------------|------------------------------|---------|---------------------------------------|
| `connect`      | TCP+TLS handshake            | 5s      | per provider (`provider_timeouts`)    |
| `request_send` | sending of request body      | 10s     | per provider (`provider_timeouts`)    |
| `ttft`         | request done → first SSE/JSON byte | 30s | per model (`models.timeout_overrides_json`) |
| `idle_chunk`   | gap between consecutive bytes in SSE | 120s | per model (`models.timeout_overrides_json`) |
| `total`        | request enter → done         | 300s    | per provider (`provider_timeouts`)    |

Each phase is bracketed by `Instant::now()` at entry/exit, persisted on the
`usage` row (`connect_ms`, `ttft_ms`, `total_ms`, `tokens_per_sec`), and emitted
in structured logs with `phase=<phase_name>`.

`connect_ms` = wall-clock time from the connect() syscall (start of TCP handshake) until TLS handshake completes (i.e. start of HTTP request line transmission). Does NOT include request body send. Does NOT include DNS resolution (DNS is included in connect_ms at the reqwest level, not separately measured).

### Admin / background phases

| Phase            | Config key                  | Default | Meaning                                         |
|------------------|-----------------------------|---------|-------------------------------------------------|
| Admin request    | `timeouts.admin_ms`         | 10000   | Hard cap for admin endpoints                    |
| Model refresh    | `timeouts.model_refresh_ms` | 15000   | Background fetcher per provider                 |

The default 120s for `idle_chunk` assumes **token-emitting** speed (one or more
chunks every couple of seconds at worst). Reasoning models can take much longer
between visible tokens; for those, a per-model override can be set on
`models.timeout_overrides_json` (e.g. `{"ttft_ms": 60000, "idle_chunk_ms": 180000}`).
The override is merged over the global default and the resulting value is what
the engine arms the per-chunk timeout with.

### Resolution order (per phase)

1. `models.timeout_overrides_json` for the concrete model (applies to `ttft`
   and `idle_chunk` only).
2. `provider_timeouts` for the provider (applies to `connect`, `request_send`,
   `total`).
3. Defaults from `[timeouts]` in `config.toml`.

Each timeout produces a typed error (`Timeout(phase)`), which is mapped to a
structured log entry and an HTTP 504 with a `Retry-After` header.

## 9. Request ID / Trace ID Propagation

A `request_id` is generated at the edge by the server. A `trace_id` is generated per
upstream call (one request can fan out across multiple accounts due to retries; each
attempt gets its own trace_id and the original request_id is preserved).

Propagation rules:

- **Inbound.** If the client sends `x-request-id` (UUID), we adopt it; otherwise we
  generate a fresh v4 UUID.
- **Outbound to upstream.** `x-request-id` and `x-trace-id` headers are added to every
  upstream HTTP call.
- **Logs.** Every structured log line carries `request_id`, `trace_id`, `provider_id`,
  `account_id`, `model_id`, and `phase`.
- **DB.** `usage.request_id` is the canonical key for analytics and incident triage.
- **Response.** `x-request-id` is echoed back to the client on every response,
  including errors.

## 10. Dashboard Integration

The dashboard SPA is part of the `openproxy-server` binary. The frontend source tree
(lit-html + TypeScript) lives in `crates/openproxy-server/web/` and is bundled into a
single `app.js` by `pnpm build` (esbuild). The bundle and the rest of the static tree
(`index.html`, `callback.html`, CSS, fonts, i18n JSON) are embedded into the server
binary at compile time via `rust-embed` (see `crates/openproxy-server/src/admin_ui.rs`).

- A single `cargo build --release -p openproxy-server` produces a binary that serves
  BOTH the API (`/v1/*`, `/admin/api/*`) and the dashboard SPA (`/admin/*`) on the same
  port — no second process, no second port, no proxy hop.
- The build pipeline requires a Node 22 + pnpm toolchain to run `pnpm build` BEFORE
  `cargo build` (because `rust-embed` bakes the `dist/` tree into the binary). The
  Dockerfile and `.github/workflows/ci.yml` orchestrate this; on a fresh clone,
  `cargo build` would still succeed because the rest of the static tree
  (HTML, CSS, fonts, i18n) is checked in and `rust-embed` only walks what
  exists on disk.
- The dashboard has **no direct access** to SQLite or provider adapters. Every action
  goes through the server's admin REST API (typed on the Rust side by the optional
  `openproxy-api-client` crate, which external automation can also consume).
- Auth: the dashboard presents an admin API key prompt; the key is stored in the
  browser's `localStorage` and sent as `Authorization: Bearer <key>` on every
  `/admin/api/*` request. The `/admin/ws` WebSocket does its own auth via a `?token=`
  query parameter (browsers can't set headers on WS handshakes).
