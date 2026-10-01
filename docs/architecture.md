# openproxy: Architecture

This document describes the current eight-crate workspace, not the original MVP.
The authoritative boundaries are the crate manifests and the modules referenced below.

## 1. Runtime overview

openproxy is a Rust LLM gateway with an embedded administrative dashboard. It accepts
OpenAI-compatible Chat Completions and Responses, Anthropic Messages, and specialized
embeddings, image, audio, tokenization and System One requests. Providers and accounts
are configured in SQLite; model discovery and catalog synchronization update routing
metadata without recompiling the gateway.

The normal deployment is a single `openproxy` executable. Its HTTP server is axum on
Tokio, with a hardened Hyper accept loop. Upstream networking uses the shared
`UpstreamClient` in `openproxy-adapters` (Hyper/rustls), rather than a reqwest client
per handler. Local Laya inference, when enabled and used, runs in an isolated child
process of the same executable, not in the gateway's native address space.

```text
 Clients / SDKs                       Browser dashboard
       |                                    |
       +------------- HTTP -----------------+
                         |
             openproxy-server (axum / Hyper)
             public API / admin REST / admin WS
                         |
        +----------------+------------------+
        |                                   |
 openproxy-core                      openproxy-pipeline
 discovery / OAuth / unary           routing / race / retry / SSE
 background services                 async repository / usage admission
        |                                   |
        +-------------+---------------------+
                      |
           openproxy-adapters        openproxy-compression
           protocols / upstream      message and tool-output reduction
                |                           |
        upstream HTTP APIs           shared domain contracts
        or private Laya IPC                 |
                                      openproxy-types

 core / pipeline / server --> openproxy-db --> SQLite WAL
 external automation --> openproxy-api-client --> admin HTTP API
```

This is a runtime sketch; the exact compile-time dependencies are listed below.

## 2. Crate responsibilities and dependency DAG

| Crate | Responsibility |
| --- | --- |
| `openproxy-types` | Domain structs, typed IDs, errors, configuration contracts, provider/model metadata, request/response DTOs, selection telemetry and usage events. No internal crate dependencies. |
| `openproxy-db` | SQLite connections, numbered migrations, repositories and SQL, pricing/analytics, encrypted credentials, usage transactions and the durable usage journal. |
| `openproxy-adapters` | Provider implementations and registry, target-format serialization, response/SSE normalization, shared upstream transport, SSRF protection, and the optional native Laya worker. |
| `openproxy-compression` | Payload compression policies, Lite/RTK filtering and compression statistics. |
| `openproxy-pipeline` | Combo/target resolution, account rotation, selection, races, retries, quotas, circuit breakers, cooldowns, session affinity, PII processing, SSE execution and usage worker coordination. |
| `openproxy-core` | Headless service orchestration: configuration, provider seeding and catalog synchronization, discovery scheduling, OAuth refresh, proxy services, notifications and non-chat endpoint executors. Reexports preserve existing consumers' interfaces. |
| `openproxy-server` | Executable and server library: listeners, routes, HTTP middleware, authentication, request validation, runtime state, lifecycle wiring, admin API/WebSocket and embedded SPA. |
| `openproxy-api-client` | Typed Rust client for administrative HTTP operations; no direct SQLite access. Uses shared types and the upstream HTTP infrastructure. |

Direct internal dependencies from each manifest's `[dependencies]` section:

```text
openproxy-types       -> (none)
openproxy-db          -> types
openproxy-adapters    -> types
openproxy-compression -> types
openproxy-pipeline    -> types, db, adapters, compression
openproxy-core        -> types, db, adapters, compression, pipeline
openproxy-server      -> types, db, adapters, compression, pipeline, core
openproxy-api-client  -> types, adapters, core
```

Names on the right abbreviate the `openproxy-` prefix. The graph is acyclic; the
server and API client are consumers, and neither is imported by the lower layers.
The frontend is a source tree inside `openproxy-server/web`, not a ninth crate.
SQL belongs in `openproxy-db`; handlers invoke its operations through asynchronous
scheduling boundaries rather than implementing SQL in HTTP code.

## 3. HTTP surfaces and deployment modes

`crates/openproxy-server/src/router.rs` defines three router constructors:

- `build_router`: public and administrative surfaces together (default).
- `build_public_router`: only the public API and public health probe.
- `build_admin_listener_router`: only the administrative surface and its redirects.

With `[server].admin_bind` unset, `[server].bind` serves both surfaces. When
`admin_bind` is set, the server binds two listeners sharing the same `AppState`:
the public bind excludes all `/admin` paths and redirects, and the admin bind
excludes `/v1` paths. Both retain request IDs, body limits and security headers.
Splitting listeners does **not** disable administrative authentication.

```toml
[server]
bind = "127.0.0.1:8787"
admin_bind = "127.0.0.1:8788" # omit for one-listener mode

[storage]
reader_count = 0 # automatic, bounded reader count
```

Configuration validation rejects empty or identical public/admin bind strings.
`OPENPROXY_ADMIN_BIND` and `OPENPROXY_DB_READERS` provide environment overrides.
These are plain HTTP listeners: expose them through a TLS-terminating reverse proxy
when needed. Non-loopback binds generate warnings. Trusted-proxy configuration
governs forwarded client-IP resolution; arbitrary forwarded headers are not trusted.
Each listener uses a connection semaphore and a header-read deadline, and shutdown
stops accepting connections before draining active work and the usage worker.

### Public API

The route assembly lives in `handlers::public_api_routes` and endpoint modules:

| Route | Purpose |
| --- | --- |
| `GET /v1/health` | Unauthenticated liveness (`{"status":"ok"}`), without a version fingerprint. |
| `GET /v1/models` | Model catalog. |
| `POST /v1/chat/completions` | OpenAI Chat Completions, streaming or JSON. |
| `POST /v1/responses` | OpenAI Responses, streaming or JSON. |
| `POST /v1/messages` | Anthropic Messages compatibility. |
| `POST /v1/embeddings` | Embedding generation. |
| `POST /v1/images/generations`, `/edits`, `/variations` | Image operations. |
| `POST /v1/audio/transcriptions` | Multipart transcription. |
| `POST /v1/systemone` | Structured decision protocol. |
| Tokenization routes | Provider-aware token counting; see `handlers/tokenize.rs`. |

Public data/inference endpoints are credential-gated; anonymous access requires the
explicit configuration policy. Inference middleware also applies routing, rate
limits and disconnect tracking as appropriate to the endpoint.

### Administrative API

`/admin/api/*` includes providers, accounts, models, combos/targets, API keys,
usage analytics, model testing, discovery, proxies, runtime configuration,
notifications and maintenance operations. It requires a Bearer API key with
administrative (`manage`) scope. The SPA shell and allowlisted assets load before
authentication. `/admin/health` and the OAuth browser callback are intentionally
public; `/admin/ws` authenticates in its upgrade handler.

Browser WebSockets use `POST /admin/api/ws-ticket` to obtain a single-use,
30-second ticket, then connect to `/admin/ws?ticket=…`. Non-browser clients may
use a Bearer header. Raw API keys are not accepted in WebSocket query strings.

## 4. Providers and protocol boundaries

The registry is generated by `define_provider_adapter!` in
`crates/openproxy-adapters/src/adapters/mod.rs`. It currently registers 22 built-ins:

```text
antigravity, atomesus, cline, cloudflare-workers-ai, codebuddy, codex,
commandcodego, gemini, horde, kilocode, kiro, laya, minimax, nous-research,
nvidia-nim, ollama-cloud, opencode-go, opencode-zen, openrouter, typesafe,
vercel-gateway, zai
```

`CustomAdapter` handles configurable providers; mock support is conditional on tests
or `test-utils`. The current `ProviderAdapter` contract is defined in
`adapters/traits.rs`: configuration/metadata, URL construction, authentication,
model discovery and provider-specific request handling. `AdapterFactory` creates
registered or custom adapters. Adding a built-in means implementing it in this
crate and registering its enum mapping, not editing `openproxy-core` translators.

`ProviderFormat` and `TargetFormat` include OpenAI, Anthropic, Responses, Gemini,
Atomesus, CommandCodeGo and System One. A mixed provider resolves its target format
from model metadata; fixed-format providers use their configured format. Formatters
and normalizers in `openproxy-adapters` bridge the public protocol to the selected
upstream protocol. The internal domain retains the original fields and content.

For strict OpenAI-compatible upstream schemas, `OpenaiFormatter` sanitizes only the
outbound serialization boundary: it flattens structured assistant/system/tool
content, normalizes user multimodal parts, and removes Responses-only extensions
from Chat Completions payloads. This does not strip those fields from shared DTOs.
Anthropic and Gemini mappings preserve tool use and token accounting according to
their own wire contracts. Discovery can use upstream catalogs, provider-specific
fallbacks and catalog enrichment; not every provider exposes a live model endpoint.

## 5. Request pipeline and streaming

The server parses the public request and resolves authentication/routing context.
`services/pipeline_runner.rs` constructs the shared pipeline request; endpoint
wrappers convert its result into the caller's expected protocol.

```text
public middleware / validation
  -> combo or direct-model resolution
  -> nested-target expansion / account rotation
  -> health, cooldown, quota and predictive-limit filtering
  -> selection / optional System One decision routing
  -> sequential execution or bounded racing
  -> provider formatter -> UpstreamClient
  -> response/SSE normalization -> client protocol emitter
  -> durable usage admission -> background SQLite application
```

Strategies are `priority`, `round_robin` and `shuffle`. Priority modes include
`strict`, `lkgp`, `weighted`, `least_used`, `p2c` and `decision`. Sub-combos are
depth-bounded and cycle-checked. Races select a client response while retaining
per-attempt accounting; cancellation distinguishes client disconnects from race
losers. Retry policy, circuit breakers and persisted cooldowns prevent repeatedly
dispatching to unhealthy targets. Request-scoped compression and PII state are
shared across attempts where appropriate.

SSE is not universally byte-passthrough. The adapter normalizes upstream events and
the public endpoint emits Chat Completions, Responses or Messages events. Streaming
uses bounded channels and incremental parsing rather than requiring the whole
upstream body before forwarding. Chat streams end with `data: [DONE]`; Responses
streams emit their canonical terminal event. Usage accumulation preserves nonzero
prompt/completion counts across partial provider updates. Disconnects cancel active
upstream work and retain partial/error accounting; race-loser rows are not confused
with the row that supplied the client response.

Specialized embeddings, images, audio and System One handlers use executors in
`openproxy-core`; they reuse the database, adapters, transport and health services
without pretending all endpoint payloads are Chat Completions.

## 6. SQLite and asynchronous repository boundary

`openproxy-db::DbPool` uses bundled rusqlite, one serialized writer and independently
opened, mutex-protected reader connections. Cloning the pool shares ownership; it
does not create new SQLite handles. There is no r2d2 dependency.

`[storage].reader_count` accepts `0..=32`: zero selects available CPU parallelism
clamped to **2–8**, while `1..=32` selects that exact number of readers. Opening an
out-of-range count fails configuration validation. Reader acquisition scans the
bounded set opportunistically to avoid waiting behind one busy reader when another
is free. Readers and writer use WAL, foreign keys and SQLite busy timeouts. Current
connections use `synchronous=NORMAL`, disabled mmap, small per-connection caches and
file-backed temporary storage.

SQLite is synchronous. `DbPool::spawn_read` and `spawn_write` acquire guards inside
`tokio::task::spawn_blocking` closures. The pipeline's `AsyncPipelineRepository`
is the object-safe scheduling boundary; `BlockingPipelineRepository` runs compound
operations against the synchronous `PipelineRepository` in a blocking task.
`SqlitePipelineRepository` uses the production pool's readers for reads and writer
for mutations, retaining a single-connection compatibility path for existing callers.
No connection guard belongs across an `.await`, a nested repository lock or a
broadcast callback.

Numbered SQL migrations and their registry live in `openproxy-db`. Upstream secrets
are sealed with AES-256-GCM using `MasterKey`/`OPENPROXY_MASTER_KEY`; administrative
API keys are checked against stored hashes. Schema changes and credential handling
remain database-layer concerns.

## 7. Durable usage admission and atomic acknowledgement

The in-memory worker channel is a bounded wake-up mechanism, not the durable source
of truth. `UsageTracker::enqueue` reserves channel capacity, serializes and commits
the job to SQLite's `usage_journal` on a blocking thread, then sends `JournalWake`.
Admission succeeds only after the journal append commits. Closed workers, database
errors or the **100,000 pending-job** capacity limit reject admission; records are
not silently overwritten to make room.

The worker replays journal entries in ID order, in groups of up to 32, with periodic
wake-ups so committed entries are retried even if a wake signal is lost. Failed
applications remain pending for a later replay or restart.

For an attempt, `usage_writer::record_journaled` applies all of the following in
one SQLite `IMMEDIATE` transaction:

1. Verify the journal entry still exists.
2. Insert the usage row with its pricing/accounting fields.
3. Clear or update the target cooldown when appropriate.
4. Delete the journal entry (ACK).
5. Commit.

Winner/client-response marking likewise updates the row and acknowledges its journal
job in one transaction. A failed transaction leaves both application and ACK
uncommitted; an already-acknowledged ID is not applied again. Usage broadcasts occur
only after commit and after the writer guard is released.

Shutdown closes admission, drains accepted work and joins the worker. Pending journal
depth and failed batches are available as worker statistics; a drain that leaves
pending durable jobs reports an error rather than claiming a clean shutdown.
The guarantee covers **successfully admitted** jobs, not calls rejected before
journal commit. SQLite WAL with `synchronous=NORMAL` supports process-crash/restart
replay but is not a promise that every acknowledged commit survives sudden power
loss. See [usage-persistence.md](usage-persistence.md) for the detailed contract.

## 8. Local Laya worker isolation

The default-enabled `laya-engine` feature wires native inference through a supervisor
in `openproxy-adapters`. Activation enables lazy inference; it does not immediately
download weights or allocate an ONNX session. On first work, the supervisor ensures
assets and launches the same executable with `--laya-worker`. That mode is handled
before gateway configuration, database, credentials, telemetry or Tokio startup.

The child receives only allowlisted model/runtime environment variables, never master
keys, OAuth tokens, proxy settings or arbitrary loader variables. JSON IPC uses
private anonymous stdin/stdout pipes with a length prefix and a **4 MiB** frame
limit. The supervisor queue has eight slots; native requests are bounded to 64
questions and 1–128 options per question. Startup and inference exchanges have
deadlines. Native sandbox setup must succeed before loading the engine; unavailable
sandbox support fails closed rather than running unisolated inference.

Native inference is enabled on Linux x86_64/aarch64. The supervisor distinguishes
enabled from resident state, stops the child on errors/cancellation and unloads it
after an idle timeout (five minutes by default, configurable with
`OPENPROXY_LAYA_IDLE_TIMEOUT_MINUTES`). Disabling the provider or shutting down joins
the supervisor and terminates the worker. Decision routing can fall back to the
configured HTTP path after a local worker failure; self-hosted Laya remains an
adapter-supported HTTP option.

## 9. Timeouts and observability

Default upstream phase budgets from `openproxy-types::TimeoutsConfig`:

| Phase | Default |
| --- | --- |
| Connect | 5,000 ms |
| Request send | 10,000 ms |
| Time to first token/response | 6,000 ms |
| Idle chunk | 120,000 ms |
| Total | 300,000 ms |

Global configuration (including persisted runtime overrides) supplies the base
values. `models.timeout_overrides_json` overrides `ttft_ms` and `idle_chunk_ms`.
`openproxy-pipeline::timeouts::resolve` applies this two-level merge; it does not
read a separate `provider_timeouts` table. The adapter transport maps the result
to DNS, dial, TLS, write, headers, body-chunk and total deadlines. Timeout/error
mapping is handled by the transport, pipeline and HTTP error layer; a phase is
not assumed to produce the same status or retry action in every context.

The outer request-ID middleware adopts a valid inbound `x-request-id` UUID or creates
one, stores it in request extensions and echoes it on responses, including errors.
Pipeline trace/attempt identifiers correlate routing, upstream attempts and usage
rows. Timings such as connect, TTFT and total, token counts, costs, endpoint kind,
race/response flags and optional redacted recordings feed analytics and live admin
events. Trace context is not injected into SSE payloads merely for logging.

## 10. Dashboard and reproducible builds

The SPA uses TypeScript, lit-html, vanilla CSS and uPlot under
`crates/openproxy-server/web`. esbuild bundles `app.js`; declaration output and
static HTML, styles, fonts and i18n packs live under `web/src/static`. `rust-embed`
packages the assets into the executable. Build the frontend before compiling the
server so the embedded bundle matches the source; successful Rust compilation
alone does not prove the SPA bundle exists.

The browser never accesses SQLite or provider adapters directly. Dashboard actions
use authenticated admin HTTP endpoints, with ticket-authenticated WebSocket updates.
In split-listener deployments, serve the dashboard from the administrative origin.

The workspace declares **Rust 1.96** as its MSRV. `rust-toolchain.toml` pins the
development/release compiler to **1.97.1**, separately from that minimum. CI uses
the same pinned release for checks, coverage, release targets and E2E builds,
verifies the toolchain-file/CI constant match, and has an independent locked
workspace check on **1.96.0**. The frontend CI uses Node 24 and pnpm 9, uploads
`web-dist` once, and Rust jobs download it before server compilation. The Docker
job packages the previously built architecture-specific binaries.
