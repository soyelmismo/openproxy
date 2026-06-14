# openproxy — MVP Specification

## 1. Scope

### In scope (MVP)
- OpenAI-compatible HTTP server (`POST /v1/chat/completions`, `GET /v1/models`).
- Three providers: **OpenRouter**, **MiniMax Coding**, **OpenCode Zen**.
- API-key auth per provider; no OAuth, no browser flows.
- Live model discovery from upstream `/models` endpoints.
- Multi-account per provider, with per-account priority and health tracking.
- Combo routing strategies: `priority` and `round_robin`.
- Cost tracking per request, with per-model pricing tables.
- Lightweight analytics: aggregate queries over recorded usage.
- SSE streaming with deterministic, traceable output.
- Explicit per-phase timeouts (connect / read / idle).
- Structured logging with `request_id` and `trace_id`.
- SQLite storage with a versioned migration runner.
- Optional admin API and dashboard (feature-gated).
- Headless by default — server runs without a UI.

### Out of scope (MVP)
- Responses API, Assistants API, function-calling translation, tool use.
- Response compression (`gzip` / `br`).
- MCP (Model Context Protocol), A2A, agent frameworks.
- Persistent memory, conversation history, vector stores.
- Guardrails, content filters, evals, benchmarking.
- Desktop app, system tray, installers.
- i18n / l10n; English-only logs and UI strings.
- OAuth, device-code, or any interactive auth flow.
- Custom pricing models; only the published per-1K-token rates are used.
- Per-user rate limiting (we track provider-side rate limits but do not impose
  client-side quotas in MVP).
- Horizontal scaling (multi-instance) is unsupported in MVP. The `round_robin`
  counter is held **per-process in memory**; running two openproxy instances
  against the same DB will cause each instance to maintain its own counter,
  and retries/circuit-breaker state will not be shared across instances.

## 2. HTTP Endpoints

All public endpoints are OpenAI-compatible. All admin endpoints are JSON over HTTP and
require `Authorization: Bearer <admin_api_key>`. The public `GET /v1/health` endpoint
is unauthenticated and returns `{ "status": "ok", "version": "<semver>" }`.

### 2.1 `POST /v1/chat/completions`

**Request body (OpenAI Chat Completions):**

```json
{
  "model": "openrouter/anthropic/claude-3.5-sonnet",
  "messages": [
    { "role": "system", "content": "You are a helpful assistant." },
    { "role": "user",   "content": "Hello." }
  ],
  "stream": false,
  "temperature": 0.7,
  "max_tokens": 1024,
  "top_p": 1.0,
  "stop": null,
  "presence_penalty": 0,
  "frequency_penalty": 0,
  "user": "end-user-id-optional"
}
```

The `model` field is a `<provider>/<upstream_model_id>` selector. The `provider` prefix
is optional when the model is registered in exactly one provider; if ambiguous, the
combo system resolves it.

**Passthrough rules.** Field forwarding depends on the resolved `target_format`:

- **OpenRouter (`target_format = openai`):** passthrough — fields not listed in the
  OpenAI Chat Completions schema (e.g. `top_k`, `repetition_penalty`, provider-specific
  knobs) are forwarded to the upstream request as-is.
- **OpenAI → Anthropic (e.g. MiniMax Coding, OpenCode Zen Claude):** strict
  allowlist — only the 8 fields listed below are translated; everything else is
  dropped with a debug log:
  `model`, `messages`, `stream`, `temperature`, `top_p`, `max_tokens`, `stop`,
  `user`.

**Non-streaming response (200):**

```json
{
  "id": "chatcmpl-...",
  "object": "chat.completion",
  "created": 1718264400,
  "model": "anthropic/claude-3.5-sonnet",
  "choices": [
    {
      "index": 0,
      "message": { "role": "assistant", "content": "Hi there." },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 12,
    "completion_tokens": 4,
    "total_tokens": 16
  }
}
```

**Streaming response (`text/event-stream`):**

A sequence of `chat.completion.chunk` events, terminated by a `data: [DONE]\n\n` line.
The first chunk and the `done` marker are guaranteed; intermediate chunks are
provider-dependent.

**Errors:**

| Status | Code                  | Meaning                                            |
|--------|-----------------------|----------------------------------------------------|
| 400    | `invalid_request`     | Malformed body, missing `model`, etc.              |
| 401    | `unauthorized`        | No/invalid upstream API key in the chosen account. |
| 404    | `model_not_found`     | No combo or upstream serves this model.            |
| 429    | `rate_limited`        | Upstream rate-limited; `Retry-After` set.          |
| 502    | `upstream_error`      | Non-2xx from upstream.                             |
| 504    | `timeout`             | Per-phase timeout hit.                             |
| 500    | `internal_error`      | Anything unexpected.                               |

### 2.2 `GET /v1/models`

Returns the union of all discovered models across all enabled providers, in OpenAI
format.

```json
{
  "object": "list",
  "data": [
    {
      "id": "openrouter/anthropic/claude-3.5-sonnet",
      "object": "model",
      "created": 1718264400,
      "owned_by": "openrouter"
    }
  ]
}
```

The `id` is the proxy-level identifier (`<provider>/<upstream_model_id>`). The
`owned_by` field carries the provider id. Upstream model ids that already
contain a `/` (e.g. `nex-agi/nex-n2-pro:free` from OpenRouter) are preserved
verbatim, so the proxy-level id becomes `<provider>/<upstream_id_with_slashes>`
(e.g. `openrouter/nex-agi/nex-n2-pro:free`).

### 2.3 Admin endpoints

All under `/v1/admin/*`, all require `Authorization: Bearer <admin_api_key>`.

| Method | Path                          | Purpose                                       |
|--------|-------------------------------|-----------------------------------------------|
| GET    | `/v1/admin/providers`         | List configured providers.                    |
| POST   | `/v1/admin/providers`         | Add a provider.                               |
| DELETE | `/v1/admin/providers/{id}`    | Remove a provider and its accounts/models.    |
| GET    | `/v1/admin/accounts`          | List accounts.                                |
| POST   | `/v1/admin/accounts`          | Add an account (API key stored encrypted).    |
| PATCH  | `/v1/admin/accounts/{id}`     | Update label, priority, health.               |
| DELETE | `/v1/admin/accounts/{id}`     | Remove an account.                            |
| GET    | `/v1/admin/combos`            | List combos.                                  |
| POST   | `/v1/admin/combos`            | Create a combo (strategy + ordered targets).  |
| DELETE | `/v1/admin/combos/{id}`       | Delete a combo.                               |
| POST   | `/v1/admin/refresh-models`    | Force a model discovery sweep.                |
| GET    | `/v1/admin/providers/{id}/timeouts` | Read per-provider timeout overrides.    |
| PUT    | `/v1/admin/providers/{id}/timeouts` | Update per-provider timeout overrides. Body: `{ "connect_ms": int, "request_send_ms": int, "total_ms": int }`. |
| GET    | `/v1/admin/models/{id}/timeouts`    | Read per-model timeout overrides.       |
| PUT    | `/v1/admin/models/{id}/timeouts`    | Update per-model timeout overrides. Body: `{ "ttft_ms"?: int, "idle_chunk_ms"?: int }`. Unknown keys rejected with 400. Empty body clears overrides. |

The five usage analytics endpoints live under `/v1/admin/usage/*` and are documented
in §7:

- `GET /v1/admin/usage/summary`
- `GET /v1/admin/usage/by-model`
- `GET /v1/admin/usage/by-account`
- `GET /v1/admin/usage/by-status`
- `GET /v1/admin/usage/errors?limit=100`

In addition:

- `GET /v1/admin/usage/latency?provider=&model=&from=&to=` returns latency
  percentiles (p50/p95) for `connect_ms`, `ttft_ms`, and `tokens_per_sec`. The
  response carries **only aggregated percentiles**; individual token counts or
  raw rows are never exported through this endpoint. Response shape:

  ```json
  {
    "p50_connect_ms": 120,
    "p95_connect_ms": 480,
    "p50_ttft_ms": 850,
    "p95_ttft_ms": 3100,
    "p50_tokens_per_sec": 42.5,
    "samples": 1234
  }
  ```
- `GET /v1/admin/usage/races?from=&to=` returns race statistics over
  `usage` rows where `race_total > 1`. Response shape:

  ```json
  {
    "total_races": 412,
    "avg_winner_position": 1.2,                  // 1.0 = first target always wins
    "wins_by_target": { "42": 380, "43": 32 },  // combo_target_id → wins
    "avg_ttft_savings_ms": 80                    // mean(winner_ttft - first_target_ttft_if_sequential)
  }
  ```

The public unauthenticated health probe is `GET /v1/health` (returns
`{ "status": "ok", "version": "..." }`).

## 3. Provider Adapter Interface

See `architecture.md` §5 for the full trait. MVP-required behavior:

- `id()` returns the stable string used in the database and in the model id prefix.
- `translator_for(model)` returns `OpenAIToOpenAI` when the resolved `target_format`
  is `openai`, and `OpenAIToAnthropic` when it is `anthropic`. OpenCode Zen dispatches
  per model.
- `fetch_models(account)` is the only way to enumerate upstream models; it issues
  `GET <base_url>/models` internally and returns normalized `DiscoveredModel` rows.
  MiniMax Coding returns an empty `Vec` (no model list endpoint) and is seeded
  manually with a fixed model enum.
- `auth_headers()` returns `Authorization: Bearer <key>` for OpenRouter and MiniMax
  Coding, and `x-api-key: <key>` for OpenCode Zen's Claude models.
- `extra_headers()` returns:
  - OpenRouter: `HTTP-Referer`, `X-Title`.
  - MiniMax Coding: `Anthropic-Version: 2023-06-01`.
  - OpenCode Zen: empty by default.

### Provider configuration in the DB

| provider   | base_url                              | auth_type | format     |
|------------|---------------------------------------|-----------|------------|
| openrouter | `https://openrouter.ai/api/v1`        | `bearer`  | `openai`   |
| minimax    | `https://api.minimax.io/anthropic/v1` | `bearer`  | `anthropic`|
| opencode   | `https://opencode.ai/zen/v1`          | `bearer`  | `mixed`    |

`extra_headers_json` is persisted as a JSON object string and applied verbatim per
request.

### `format` vs `target_format` (source of truth)

Two columns govern wire format. The hierarchy is:

1. **`providers.format`** is the **provider-level** format. When it is `'openai'` or
   `'anthropic'`, **every** model served by that provider uses that single format.
   The `target_format` column on `models` is ignored.
2. **`models.target_format`** is the **per-model** format. It is consulted **only**
   when `providers.format = 'mixed'` (currently only OpenCode Zen, which serves
   both OpenAI-shaped and Claude models on the same endpoint).

Resolution rule (engine-level):

```text
fn resolve_target_format(provider, model) -> TargetFormat {
    if provider.format == "openai"   { return TargetFormat::OpenAI; }
    if provider.format == "anthropic"{ return TargetFormat::Anthropic; }
    // provider.format == "mixed"
    return model.target_format;
}
```

The resolved `target_format` is passed into `ProviderAdapter::build_url`,
`translator_for`, and `sse_normalizer_for` at request time (see architecture.md §5).

### Timeout resolution precedence

For each per-phase timeout (`connect`, `request_send`, `ttft`, `idle_chunk`,
`total`) the engine resolves the value at request time as:

1. `models.timeout_overrides_json` for the resolved model (applies to `ttft`
   and `idle_chunk` only).
2. `provider_timeouts` for the resolved provider (applies to `connect`,
   `request_send`, `total` only).
3. `[timeouts]` defaults from `config.toml`.

See architecture.md §8 for the full phase table and provider_timeouts schema
in §8.

Validation for `PUT /v1/admin/models/{id}/timeouts`:
- Parse body with serde_json::Value.
- Reject if body is not a JSON object.
- Allowed keys: ttft_ms, idle_chunk_ms. Reject 400 if unknown keys.
- Each value must be a positive integer <= 600000 (10 min). Reject 400 otherwise.
- Empty body {} clears all overrides.

## 4. Live Model Discovery

Model discovery has two triggers:

1. **Startup sweep.** On server start, the core spawns a task per enabled provider
   and calls `ProviderAdapter::fetch_models(account)`. Providers that return a
   non-empty `Vec` populate the catalog from that result. MiniMax Coding is
   recorded as "manual" with a placeholder model list (price is known, models are
   a fixed enum).
2. **Periodic refresh.** Every `config.model_refresh_interval_secs` (default 900s)
   the registry re-queries enabled providers and upserts results into `models`.
3. **On-demand refresh.** `POST /v1/admin/refresh-models` forces a sweep and returns
   `{ added, updated, removed }` counts.

### Refresh algorithm

For each enabled provider:

1. `GET <base_url>/models` with a 15s timeout, a `bearer` token from a healthy
   account, and the provider's `extra_headers`.
2. Normalize the response to a `Vec<DiscoveredModel { id, display_name, created_at? }>`.
3. Upsert into `models`: insert new rows, update existing `display_name` and
   `discovered_at`, set `expires_at = now + 2 * refresh_interval`.
4. Models whose `expires_at` has passed and which were not refreshed are marked
   inactive (excluded from `GET /v1/models` and from combo selection) but kept in
   the DB for analytics continuity.

### MiniMax Coding special case

MiniMax does not expose a model list. We treat the **published pricing** as the
authoritative source. The `minimax-m2.1` model is seeded at provider registration
time with `target_format = "anthropic"` and the hard-coded price `0.2 / 0.2` USD
per 1M prompt/completion tokens. The quota endpoint is hit only for health checks.

## 5. Combo Engine

A **combo** is an ordered list of (provider, account, model) targets with a strategy.
The combo engine selects one target per request.

### 5.1 Priority

```text
1. Sort combo_targets by priority_order ASC.
2. For each target in order:
   a. If combo_targets.account_id IS NOT NULL
      and the referenced account is not eligible
      (health_status != "healthy" or rate_limited_until > now) → skip.
   b. If combo_targets.account_id IS NULL
      and no healthy, non-rate-limited account is registered for
      this provider → skip.
   c. If model is not active (expired) → skip.
   d. Otherwise → pick this target. If account_id is NULL, pick the
      lowest-priority eligible account for the provider (ties broken
      by account.id ASC).
3. If no target was picked → 404 model_not_found.
```

### 5.2 Round robin

```text
1. Read combo_targets, filter to eligible targets. A target is eligible
   if its referenced account (when account_id IS NOT NULL) is healthy
   and not rate-limited, OR (when account_id IS NULL) the provider has
   at least one healthy, non-rate-limited account.
2. Maintain a per-combo atomic counter in memory
   (persisted counter on disk is out of scope for MVP).
3. counter = counter % eligible.len()
4. Pick eligible[counter]; increment counter.
   - If the target's account_id IS NOT NULL, use that account.
   - If the target's account_id IS NULL, pick the next healthy account
     for the provider using a per-(provider, model) round-robin counter
     (see §5.3).
```

If the eligible set is empty, the same 404 is returned.

### 5.3 Account rotation within a provider

`combo_targets.account_id` is **nullable**. Its semantics are:

- If `combo_targets.account_id IS NOT NULL`, that account is fixed for the
  target; no rotation is performed.
- If `combo_targets.account_id IS NULL`, the engine performs round-robin
  across all **healthy, non-rate-limited accounts** registered for that
  provider. Selection is scoped per `(provider, model)`. With a strategy
  of `priority`, the lowest `account.priority` wins (ties broken by
  `account.id` ASC). With a strategy of `round_robin`, a per-(provider,
  model) atomic counter cycles through the eligible accounts.

### 5.4 Retry policy

- **`max_attempts`** is a config value, default **3**. The value caps retries
  across all combo targets for a single `request_id`.
- **Errors that trigger a retry:** upstream 5xx, upstream 429 (`rate_limited`),
  per-phase timeouts, and network/connect errors.
- **Errors that do NOT trigger a retry:** upstream 4xx other than 429
  (these are returned to the client as-is).
- **Backoff:** exponential with full jitter. Base 200ms, factor 2:
  attempt 1 → 200ms, attempt 2 → 400ms, attempt 3 → 800ms; each delay
  is drawn from `uniform(base, base * 1.5)` to add ±50% jitter.
- **Circuit breaker:** a per-account rolling counter tracks consecutive
  failures. On **5 consecutive failures** the account is marked
  `unhealthy` for **60 seconds**; during that window it is skipped by the
  combo engine. A successful request resets the counter.
- **Usage rows:** every attempt produces an **independent row** in `usage`
  sharing the same `request_id` but with a distinct `trace_id` and
  incrementing `attempt` (1, 2, 3, ...). The final attempt's status is
  the one surfaced to the client.

### 5.5 Race execution

A combo with `race_size > 1` launches multiple upstream requests in parallel
and the **first** valid response wins. Losers are cancelled.

Race semantics vs. retry semantics:
- Race executes ONLY on the first attempt. race_size targets are launched in parallel.
- The "winner" is the first target to send the first valid byte (HTTP body byte after status line) AND that byte parses as 2xx-valid content.
- If the winner's first byte is a 5xx status (e.g. 502/503/504/429), the race CONTINUES: the loser targets are NOT cancelled, and the engine waits for the next target's first valid byte. This is the "race = first valid response" semantic.
- If the winner, after being declared, later fails mid-stream (e.g. upstream dies after first byte), there is NO mid-stream failover. The stream is aborted, status_code=502 is returned to the client, no retry. Consistent with §11 "Failover mid-stream unsupported".
- Retries (max_attempts) apply ONLY when the race_size=1 path is taken OR when all race targets fail before any winner is declared. Retries are sequential over the combo from the beginning (race_size targets re-launched).
- max_attempts=3, race_size=2, 4 combo targets: at most 3 attempts × 2 racing targets = 6 upstream requests per user request.

Algorithm:

```text
1. Resolve eligible targets ordered by priority_order ASC.
2. Take the first min(race_size, eligible.len()) targets.
3. For each target, spawn a tokio task that issues the request and
   signals completion via a tokio::sync::oneshot.
4. Use futures::future::select_all (or join_all + a shared winner flag) so
   that as soon as one future returns Ok(...), the others observe a closed
   oneshot receiver and abort via their AbortHandle.
5. Race winner resolution:
   - TTFT is measured at the HTTP body byte level: from the moment the upstream's TLS handshake completes until the first byte of the response body is received by reqwest. This is the canonical ttft_ms persisted in usage.
   - The race winner is the target whose reqwest response yields the first byte of body AND that byte is a valid 2xx-prefixed HTTP response. The status line is read first; if status >= 500 or status == 429, the target is NOT a winner, the race continues.
   - For non-streaming responses: winner = first target whose body is parseable as JSON object AND status in [200, 299]. The "parse" happens at end-of-body, so the race effectively ends at completion for non-streaming (acceptable: OpenRouter /chat/completions is small JSON).
   - For streaming responses: winner = first target whose body produces the first SSE event line ("data: {" or "event:") OR first byte of body if Content-Type: text/event-stream. Subsequent bytes stream through to the client.
   - Empty 200 (Content-Length: 0 or EOF before parse): NOT a winner. The target is discarded; the race continues.
   - A 200 response with body that fails to parse as JSON (non-streaming) or as SSE (streaming): winner is declared, but the client sees the parse error. The error_msg field captures this. This is "first valid response", not "first correct response".
6. The first 2xx with parseable body wins. Losers are cancelled:
   their AbortHandle is invoked, the upstream connection is dropped,
   and a usage row is written with status_code=499 and
   error_msg='race_lost' BEFORE the abort takes effect (so the row
   is durable).
7. If ALL targets fail → 502 with the last error.
8. Race does NOT retry 5xx winners. If a winner's first byte is a 5xx,
   the race is considered complete with that error. Callers that want
   5xx retries should use race_size=1 with retries, or race_size>=2
   with the expectation that 5xx winners surface as 502.
```

Loser rows in `usage` are tagged with `race_total = combos.race_size` and
`race_lost = 1`. The winner row has `race_lost = 0`. All rows in a race share
the same `request_id` and have distinct `trace_id`s.

Loser cancellation contract:
1. Race resolution emits "you_lost" signal to all non-winners via tokio::sync::oneshot.
2. Each loser task has up to `racing.abort_grace_ms` (default 500ms) to:
   a. Write its usage row with status_code=499, error_msg='race_lost'.
   b. Consume or drop the response body stream (reqwest best practice: `.text()` to drain).
3. After abort_grace_ms, hard cap: AbortHandle::abort() is invoked. The future is dropped, the connection is closed at the reqwest level (TLS shutdown fires).
4. The loser NEVER blocks the winner. The winner is bounded by its own ttft_ms and the global total_ms.
5. The `usage` row write is best-effort within the grace window. If SQLite is slow and the row is not persisted in time, a WARN log is emitted with phase=race_lost_persist_failed, request_id=X, trace_id=Y. The row is considered "lost".

Error priority for "all targets failed" reporting:
When all race targets fail or are discarded, the client sees 502 with error_msg derived from the highest-priority error:
1. timeout (any phase)
2. 5xx transient (502, 503, 504) — these are pre-body, so the race continued past them
3. 429 (rate limit)
4. 4xx permanent (400, 401, 403, 404) — these are pre-body, the race skipped them
5. network error (TLS, DNS, connection reset)
6. parse error
The "last error" reported to the client is the one with highest priority in this list, NOT the chronologically last one.

Winner timeout contract:
- The winner is bounded by two timeouts: its own ttft_ms (per-model) AND the global total_ms.
- If the winner fails to send the first byte within ttft_ms, the winner is discarded (treated as a pre-body failure). The race continues with the next eligible target if any.
- If the winner is declared and then fails mid-stream, no failover. The stream is aborted, 502 returned. (Already covered by I17 / §11.)
- If all targets fail to produce a winner, the request returns 502 with the highest-priority error (C5).

max_race_size enforcement:
- The engine reads `racing.max_race_size` from config on every combo lookup.
- If a combo's `race_size` exceeds `max_race_size` at request time, the engine clamps the effective race_size to `max_race_size` and logs a WARN with phase=clamp, fields={combo_id, configured=N, effective=M}.
- The schema CHECK is a safeguard only; runtime clamping is the real enforcement.
- Changing `max_race_size` in config requires restart.
- Changing the schema CHECK requires a destructive migration (rare, not MVP).

race_size=1 fast-path:
- If `combos.race_size == 1` AND no retries are scheduled, the engine bypasses the race machinery entirely. The target is invoked directly with the existing single-target path. No `join_all`, no `AbortHandle`, no race log lines. The usage row is written with `race_total=1`, `race_lost=0`, identical to the pre-race schema.
- This guarantees race_size=1 has zero overhead vs. the sequential baseline and identical log shape.
- Acceptance criterion §12 #19: "A combo with race_size=1 produces usage rows with race_total=1, race_lost=0, and no race-related log lines (`race_started`, `race_winner`, `race_loser`)."

Interaction with circuit breaker:
- Health snapshot is taken at race start. Eligible targets = targets whose account is healthy AND not rate-limited at race start.
- Changes in health during the race do not affect already-launched lanes (they will be cancelled by the abort_grace_ms if they lose).
- Acceptance criterion §12 #22: "race_size=2 with one account going unhealthy mid-race (simulated by external PUT /v1/admin/accounts/{id} {health_status: unhealthy} after the race starts): the race continues with the original 2 lanes; the newly-unhealthy account's lane is not retroactively cancelled."

## 6. Cost Calculation

Pricing lives in a small in-memory table keyed by `provider_id` + `model_id` with
`(input_usd_per_1m, output_usd_per_1m)` and an optional `cache_read` / `cache_write`
extension (unused in MVP). MVP source of truth:

- **OpenRouter:** fetched per-model from the `/models` response (which carries
  `pricing.prompt` and `pricing.completion` as USD strings). Cached on insert.
- **MiniMax Coding:** hard-coded in the provider module: `minimax-m2.1 → 0.2 / 0.2`.
- **OpenCode Zen:** a static table in `openproxy-core::providers::opencode::PRICING`,
  reviewed at release time.

### Per-request cost

```text
cost_usd =
    prompt_tokens     * input_usd_per_1m  / 1_000_000
  + completion_tokens * output_usd_per_1m / 1_000_000
```

Recorded in `usage.cost_usd` with two decimal places of precision. If pricing is
missing, the row is recorded with `cost_usd = NULL` and a warning is logged at
`warn` level keyed by `(provider, model)`.

## 7. Analytics Queries

The analytics surface is a small, fixed set of SQL aggregations over `usage`. All
endpoints accept `?from=<rfc3339>&to=<rfc3339>&group_by=<...>` and return JSON.

| Endpoint                                | Output shape                                  |
|-----------------------------------------|-----------------------------------------------|
| `GET /v1/admin/usage/summary`           | totals: requests, prompt/completion tokens, USD |
| `GET /v1/admin/usage/by-model`          | grouped by `(provider_id, model_id)`          |
| `GET /v1/admin/usage/by-account`        | grouped by `(provider_id, account_id)`        |
| `GET /v1/admin/usage/by-status`         | grouped by HTTP `status_code`                 |
| `GET /v1/admin/usage/errors?limit=100`  | recent error rows with `error_msg`            |

Example summary query (illustrative):

```sql
SELECT
  COUNT(*)                                       AS requests,
  COALESCE(SUM(prompt_tokens), 0)                AS prompt_tokens,
  COALESCE(SUM(completion_tokens), 0)            AS completion_tokens,
  COALESCE(SUM(cost_usd), 0)                     AS cost_usd,
  COALESCE(AVG(total_ms), 0)                     AS avg_total_ms
FROM usage
WHERE created_at BETWEEN ?1 AND ?2;
```

A read-only SQLite connection (separate from the writer pool) serves analytics so
that long-running scans never block request handling.

`/v1/admin/usage/latency` percentile algorithm:
- Algorithm: t-digest via the `tdigest` crate. One TDigest per (provider, model, phase) dimension.
- Cardinality budget: <1M rows per 24h window. On-demand recomputation acceptable.
- On each request to /v1/admin/usage/latency:
  1. Parse ?from=&to=&provider=&model= filters.
  2. Stream rows from SQLite (paginated 10K rows at a time).
  3. Feed into per-dimension TDigest. Memory: O(k) per digest where k=200.
  4. Extract p50, p95 from each digest.
- For initial MVP, recompute on every request (no cache). If latency > 200ms, add a 60s cache.
- Acceptance criterion §12 #21: "Given a uniform distribution of 10K samples between 0 and 1000ms, /v1/admin/usage/latency returns p50 ≈ 500ms ± 5% and p95 ≈ 950ms ± 5%."

`usage` is_winner semantics:
- `race_lost=1` rows are losers. `race_lost=0` rows are winners OR non-race rows.
- Equivalent: `is_winner = NOT race_lost`.
- For "unique requests" aggregation, COUNT(DISTINCT request_id).
- For "total attempts" (raw rows), COUNT(*).
- Endpoints:
  - /v1/admin/usage/summary: returns both `unique_requests` (DISTINCT request_id) and `total_rows` (COUNT(*)).
  - /v1/admin/usage/latency: filters WHERE race_lost=0 (only winners contribute to latency metrics).
  - /v1/admin/usage/races: COUNT(DISTINCT request_id) WHERE race_total > 1.
- Add acceptance criterion §12 #20: "A combo with race_size=2 and max_attempts=3, where 1 race has 1 winner and 1 loser, and 2 retry rounds each with 1 winner and 1 loser, produces 6 usage rows total: 1 winner + 1 loser (attempt 1), 1 winner + 1 loser (attempt 2), 1 winner + 1 loser (attempt 3). unique_requests=1, total_rows=6."

## 8. SQLite Schema

All tables use `INTEGER PRIMARY KEY` (rowid alias) unless stated otherwise. Timestamps
are stored as ISO-8601 TEXT in UTC.

```sql
CREATE TABLE providers (
  id                  TEXT PRIMARY KEY,                 -- e.g. 'openrouter'
  name                TEXT NOT NULL,
  base_url            TEXT NOT NULL,
  auth_type           TEXT NOT NULL,                    -- 'bearer' | 'x-api-key'
  format              TEXT NOT NULL,                    -- 'openai' | 'anthropic' | 'mixed'
  extra_headers_json  TEXT NOT NULL DEFAULT '{}',
  enabled             INTEGER NOT NULL DEFAULT 1,       -- 0/1
  created_at          TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (auth_type IN ('bearer','x-api-key')),
  CHECK (format IN ('openai','anthropic','mixed'))
);

CREATE TABLE accounts (
  id                   INTEGER PRIMARY KEY,
  provider_id          TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  api_key_encrypted    TEXT NOT NULL,                   -- AES-GCM, key from config
  label                TEXT NOT NULL,
  priority             INTEGER NOT NULL DEFAULT 100,    -- lower = higher priority
  extra_config_json    TEXT NOT NULL DEFAULT '{}',      -- RESERVED, currently unused in MVP.
                                                      -- If a future change stores secrets here, encrypt
                                                      -- with the same master_key used for api_key_encrypted.
  health_status        TEXT NOT NULL DEFAULT 'healthy', -- 'healthy' | 'degraded' | 'unhealthy'
  rate_limited_until   TEXT,                            -- nullable
  created_at           TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (health_status IN ('healthy','degraded','unhealthy'))
);
CREATE INDEX idx_accounts_provider ON accounts(provider_id);

CREATE TABLE models (
  id                     INTEGER PRIMARY KEY,
  provider_id            TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  model_id               TEXT NOT NULL,                 -- upstream id, e.g. 'anthropic/claude-3.5-sonnet'
  display_name           TEXT,
  target_format          TEXT NOT NULL,                 -- 'openai' | 'anthropic'
  pricing_input          REAL,                          -- USD per 1M prompt tokens
  pricing_output         REAL,                          -- USD per 1M completion tokens
  timeout_overrides_json TEXT,                          -- nullable, JSON e.g. {"idle_chunk_ms": 180000, "ttft_ms": 60000}
  discovered_at          TEXT NOT NULL DEFAULT (datetime('now')),
  expires_at             TEXT,
  active                 INTEGER NOT NULL DEFAULT 1,
  UNIQUE(provider_id, model_id),
  CHECK (target_format IN ('openai','anthropic'))
);
CREATE INDEX idx_models_active ON models(active);

CREATE TABLE combos (
  id         INTEGER PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  strategy   TEXT NOT NULL,                     -- 'priority' | 'round_robin'
  race_size  INTEGER NOT NULL DEFAULT 1,         -- 1 = sequential; N = parallel race across first N targets
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (strategy IN ('priority','round_robin')),
  CHECK (race_size >= 1 AND race_size <= 8)
);

CREATE TABLE combo_targets (
  id             INTEGER PRIMARY KEY,
  combo_id       INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
  provider_id    TEXT NOT NULL REFERENCES providers(id),
  account_id     INTEGER REFERENCES accounts(id),  -- nullable; NULL => round-robin over provider accounts
  model_id       INTEGER NOT NULL REFERENCES models(id),
  priority_order INTEGER NOT NULL,               -- used by 'priority' strategy
  UNIQUE(combo_id, provider_id, account_id, model_id)
);
CREATE INDEX idx_combo_targets_combo ON combo_targets(combo_id, priority_order);

CREATE TABLE usage (
  id                 INTEGER PRIMARY KEY,
  request_id         TEXT NOT NULL,
  trace_id           TEXT NOT NULL,                 -- per-attempt, distinct from request_id
  attempt            INTEGER NOT NULL DEFAULT 1,    -- 1..max_attempts
  combo_id           INTEGER REFERENCES combos(id),
  provider_id        TEXT NOT NULL,
  account_id         INTEGER,
  model_id           INTEGER,
  upstream_model_id  TEXT NOT NULL,                 -- snapshot of the upstream model id at request time
  prompt_tokens      INTEGER NOT NULL DEFAULT 0,
  completion_tokens  INTEGER NOT NULL DEFAULT 0,
  cost_usd           REAL,
  connect_ms         INTEGER,                           -- wall-clock from connect() syscall (start of TCP handshake) until TLS handshake completes (start of HTTP request line). Does NOT include request body send. Does NOT include DNS resolution (DNS is included in connect_ms at the reqwest level, not separately measured).
  ttft_ms            INTEGER,                           -- Time To First Token (upstream connect → first byte of response).
                                                        -- ttft_ms semantics:
                                                        -- - Persisted as integer ms if and only if the upstream sent at least one byte of response body.
                                                        -- - NULL if: timeout before first byte, race_lost before first byte, 5xx pre-body, client disconnect before first byte.
                                                        -- - tokens_per_sec guard (C3) handles NULL and zero-difference cases.
                                                        -- - /v1/admin/usage/latency filters WHERE ttft_ms IS NOT NULL for ttft percentiles.
  total_ms           INTEGER NOT NULL,                  -- request enter → last byte out (canonical timing field)
  tokens_per_sec     REAL,                              -- completion_tokens / (total_ms - ttft_ms) * 1000
                                                        -- Formula: completion_tokens * 1000.0 / NULLIF(total_ms - ttft_ms, 0)
                                                        -- Computed at write time in Rust using f64 division with explicit zero-check.
                                                        -- If completion_tokens == 0 OR ttft_ms IS NULL OR (total_ms - ttft_ms) <= 0, tokens_per_sec is persisted as NULL.
                                                        -- A structured log with phase=usage_record, level=WARN, fields={request_id, trace_id, reason} is emitted when the guard fires.
                                                        -- The /v1/admin/usage/latency endpoint filters with WHERE tokens_per_sec IS NOT NULL for tps percentiles.
  race_total         INTEGER NOT NULL DEFAULT 1,        -- race_size of the combo at request time
  race_lost          INTEGER NOT NULL DEFAULT 0,        -- 1 if this attempt lost the race
  status_code        INTEGER NOT NULL,
  error_msg          TEXT,                              -- raw upstream error body, first 512 bytes UTF-8, capped at 2KB
  error_msg_redacted TEXT,                              -- error_msg with secrets redacted (sk-..., x-api-key: ..., Authorization: Bearer ..., headers starting with case-insensitive 'auth' or 'key'), capped at 2KB
  created_at         TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_usage_created_at ON usage(created_at);
CREATE INDEX idx_usage_request_id ON usage(request_id);
CREATE INDEX idx_usage_provider_model ON usage(provider_id, model_id);

CREATE TABLE api_keys (
  id         INTEGER PRIMARY KEY,
  key_hash   TEXT NOT NULL UNIQUE,              -- argon2 hash of the key
  label      TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE provider_timeouts (
  provider_id     TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
  connect_ms      INTEGER NOT NULL DEFAULT 5000,
  request_send_ms INTEGER NOT NULL DEFAULT 10000,
  total_ms        INTEGER NOT NULL DEFAULT 300000,
  created_at      TEXT NOT NULL DEFAULT (datetime('now')),  -- ISO-8601
  updated_at      TEXT NOT NULL DEFAULT (datetime('now'))   -- ISO-8601
);
```

`models.timeout_overrides_json` (nullable) carries per-model overrides for the
`ttft` and `idle_chunk` phases only. Shape:

```json
{
  "ttft_ms": 60000,
  "idle_chunk_ms": 180000
}
```

Unknown keys are ignored. Resolution order for a given phase is:

1. `models.timeout_overrides_json` (per-model).
2. `provider_timeouts` (per-provider; `connect_ms`, `request_send_ms`, `total_ms`).
3. `[timeouts]` defaults from `config.toml`.

## 9. Migration Strategy

Migration files (under crates/openproxy-core/migrations/):
- 000001_initial_schema.sql
    CREATE TABLE providers
    CREATE TABLE accounts
    CREATE TABLE models
    CREATE TABLE combos
    CREATE TABLE combo_targets
    CREATE TABLE usage
    CREATE TABLE api_keys
    CREATE TABLE schema_migrations
- 000002_add_timing_to_usage.sql
    ALTER TABLE usage ADD COLUMN connect_ms INTEGER;
    ALTER TABLE usage ADD COLUMN ttft_ms INTEGER;
    ALTER TABLE usage ADD COLUMN total_ms INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE usage ADD COLUMN tokens_per_sec REAL;
- 000003_add_race_to_usage.sql
    ALTER TABLE usage ADD COLUMN race_total INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE usage ADD COLUMN race_lost INTEGER NOT NULL DEFAULT 0;
- 000004_add_race_size_to_combos.sql
    ALTER TABLE combos ADD COLUMN race_size INTEGER NOT NULL DEFAULT 1
      CHECK (race_size >= 1 AND race_size <= 8);
- 000005_add_provider_timeouts.sql
    CREATE TABLE provider_timeouts (
      provider_id     TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
      connect_ms      INTEGER NOT NULL DEFAULT 5000,
      request_send_ms INTEGER NOT NULL DEFAULT 10000,
      total_ms        INTEGER NOT NULL DEFAULT 300000,
      created_at      TEXT NOT NULL DEFAULT (datetime('now')),
      updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
- 000006_add_model_timeout_overrides.sql
    ALTER TABLE models ADD COLUMN timeout_overrides_json TEXT;
- 000008_add_error_msg_redacted.sql
    ALTER TABLE usage ADD COLUMN error_msg_redacted TEXT;
    -- See §8 for redaction policy and §6/§9 for the error capture contract.

All ALTER TABLE statements use "ADD COLUMN" (idempotent via the schema_migrations tracking, not via IF NOT EXISTS, to keep SQLite 3.35 not required).
If a migration needs to re-run (e.g. corrupt schema_migrations row), the runner detects the mismatch and refuses to start; manual intervention required.

Error message capture:
- error_msg stores the upstream's first 512 bytes of error body, UTF-8.
- error_msg_redacted stores a version with secrets redacted: regex removes sk-..., x-api-key: ..., Authorization: Bearer ..., and any header that starts with case-insensitive "auth" or "key".
- Both fields are capped at 2KB.
- The /v1/admin/usage/errors endpoint returns error_msg_redacted, NEVER error_msg.
- The unredacted error_msg is only used in structured logs (with phase=error) for debugging and is NEVER returned in any API response.

- Migrations are SQL files under `openproxy-core/migrations/`, named
  `NNNNNN_description.sql` (six-digit monotonic sequence).
- A `schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT)` table tracks
  applied versions.
- On startup, the runner compares the highest applied version with the embedded
  migration list; missing versions are applied in order inside a single transaction.
- Migrations are append-only. Destructive changes use the
  `NNNNNN_drop_xxx.sql → NNNNNN_recreate_xxx.sql` pattern.
- The writer pool holds the migration lock (SQLite `BEGIN IMMEDIATE`); the reader
  pool only opens after migrations are applied.
- **Idempotence test (required):** the runner is exercised by running the full
  migration set against an empty DB, then invoking the runner a second time on the
  same untouched DB file. The test asserts that the second run applies **zero** new
  versions and that `SELECT COUNT(*) FROM schema_migrations` is identical between
  the two runs. This guarantees that restarts cannot re-apply migrations.
- **Reset helper (test-only):** a `reset_db(path)` test helper deletes the SQLite
  file and recreates the schema from scratch by re-running all migrations. Tests
  that need a clean DB call this helper in their setup phase; the idempotence test
  above explicitly does **not** use it (its purpose is to validate the runner, not
  to bypass it).

## 10. Configuration

Configuration is loaded from a single TOML file (`config.toml`) with env-var overrides
prefixed `OPENPROXY_`. The schema is versioned.

```toml
[server]
bind = "0.0.0.0:8080"
admin_bind = "127.0.0.1:8081"   # admin API on a separate port

[storage]
path = "./openproxy.db"
encryption_key = "base64:..."   # 32 bytes, used for api_key_encrypted

[timeouts]  -- canonical block
connect_ms = 5000
request_send_ms = 10000
ttft_ms = 30000
idle_chunk_ms = 120000
total_ms = 300000

Deprecated aliases (accepted, logged as WARN at startup, mapped to canonical): read_chunk_ms -> idle_chunk_ms, idle_ms -> idle_chunk_ms.
```

Per-provider overrides live in the `provider_timeouts` DB table
(§8) and are managed via `GET/PUT /v1/admin/providers/{id}/timeouts`.

[racing]
default_race_size = 1           # default for newly-created combos
max_race_size     = 8           # hard cap; matches combos.race_size CHECK
abort_grace_ms    = 500         # grace for losers to flush usage row before hard abort

[retries]
max_attempts = 3
backoff_base_ms = 200
backoff_factor = 2
backoff_jitter_pct = 50

[circuit_breaker]
failure_threshold = 5
unhealthy_duration_ms = 60000

[model_refresh]
interval_secs = 900
run_on_start  = true

[logging]
level  = "info"                 # trace|debug|info|warn|error
format = "json"                 # json|text

### Log line schema

When `format = "json"`, the server emits one JSON object per line via
`tracing-subscriber`'s JSON formatter. Required fields on every line emitted by
request-handling code:

```json
{
  "timestamp": "2025-06-13T12:34:56.789Z",
  "level": "INFO",
  "target": "openproxy::combo",
  "request_id": "9f2c1a4e-...",
  "trace_id": "b41f-...",
  "provider_id": "openrouter",
  "account_id": 17,
  "model_id": "anthropic/claude-sonnet-4",
  "phase": "dispatch",
  "message": "selected target (priority=1, account=17)"
}
```

`phase` is one of `ingress`, `translate`, `dispatch`, `upstream_call`, `stream`,
`usage_record`, `egress`. Lines emitted by infrastructure (startup, migrations)
omit the request-scoped fields.

[features]
dashboard = false               # gates the openproxy-web build
```

Env-var mapping example: `OPENPROXY_SERVER__BIND=0.0.0.0:9090` overrides
`[server].bind` (double underscore = nested key). The same rule applies to the
new sections, e.g.:

- `OPENPROXY_RACING__DEFAULT_RACE_SIZE=2`
- `OPENPROXY_RACING__MAX_RACE_SIZE=4`
- `OPENPROXY_RACING__ABORT_GRACE_MS=750`
- `OPENPROXY_TIMEOUTS__TTFT_MS=45000`
- `OPENPROXY_TIMEOUTS__IDLE_CHUNK_MS=180000`

**Reload semantics (MVP).** All `[racing]` and `[timeouts]` values are read
once at process start. Changes to `config.toml` require a process restart.
Hot-reload of timeouts is **not** part of MVP. Per-provider timeouts
(`provider_timeouts` table) are reloadable without restart because they are
read from the DB on each request.

provider_timeouts cache:
- Read on every request, no cache. Justification: the row is a single PK lookup, latency < 1ms with the r2d2 pool. The simplicity of no cache invalidation is worth the SELECT overhead.
- If profiling shows the SELECT is hot, add a `tokio::sync::RwLock<HashMap<ProviderId, ProviderTimeouts>>` with 5s TTL, invalidated on PUT.
- MVP: no cache. Document in §11 as a profiling TODO.

max_race_size enforcement (runtime):
- The engine reads `racing.max_race_size` from config on every combo lookup.
- If a combo's `race_size` exceeds `max_race_size` at request time, the engine clamps the effective race_size to `max_race_size` and logs a WARN with phase=clamp, fields={combo_id, configured=N, effective=M}.
- The schema CHECK is a safeguard only; runtime clamping is the real enforcement.
- Changing `max_race_size` in config requires restart.
- Changing the schema CHECK requires a destructive migration (rare, not MVP).

## 11. Test Plan

### Unit tests
- Translator: OpenAI → Anthropic and back, with snapshots for representative inputs
  (system message, multi-turn, tool use placeholder, max_tokens defaulting, stop_reason
  mapping).
- SSE normalizer: golden trace of raw upstream events → expected OpenAI chunks.
- Combo engine: priority and round_robin on a fixed target set with mixed health
  states.
- **Combo engine — `combo_targets.account_id` semantics:**
  - (a) `account_id` set: the engine always returns that exact account, regardless
    of how many other healthy accounts the provider has.
  - (b) `account_id` NULL with **1** healthy account: the engine returns that
    single account on every call.
  - (c) `account_id` NULL with **3** healthy accounts and strategy `round_robin`:
    consecutive calls cycle through all three in order.
- **Circuit breaker:** seed an account with 5 consecutive failed attempts and
  assert that the 6th request skips the account (`combo miss` or falls through to
  the next combo target). After the 60s window, a stub returning 200 resets the
  counter and re-enables the account.
- **Retry policy:** a stub that returns 500 on attempts 1–2 and 200 on attempt 3
  must produce three `usage` rows with the same `request_id`, distinct `trace_id`,
  and `attempt` ∈ {1,2,3}, and the client must see a 200.
- Cost calculator: pricing table edge cases (zero, missing, very large).
- Model discovery: mocked HTTP responses for each provider, asserting DB upserts.
- **Migration idempotence:** run the migration runner twice against the same DB
  file without modification. Assert `COUNT(*) FROM schema_migrations` is identical
  and the second run applies zero versions. The reset helper is **not** used by
  this test.

### Integration tests
- Spin up the axum server on a random port with a SQLite file in `tmp/`.
- Stub upstream providers with `wiremock` (or a custom `tokio` listener returning
  canned responses).
- Drive end-to-end: a non-streaming request, a streaming request, a forced upstream
  500, a forced timeout, a forced rate-limit, a combo miss.
- Assert: response body, headers (`x-request-id`, `x-trace-id`), usage row contents.
- **Failover mid-stream (unsupported).** A stub that delivers one valid SSE chunk
  and then drops the connection must produce an HTTP **502 `upstream_error`** to
  the client (not a half-streamed response and not a silent retry). Verify the
  usage row has `status_code = 502` and `error_msg` mentions the mid-stream
  failure. Failover is intentionally not attempted once bytes have been sent.

### Property tests
- SSE byte-level parser: feeding arbitrary byte sequences never panics and is
  prefix-stable (prefix of input → prefix of output).
- Combo selection: any input set of healthy/unhealthy/rate-limited targets yields
  a deterministic selection under a fixed seed.

### Soak / smoke
- A 60-second test issuing 100 req/s with mixed streaming and non-streaming,
  asserting no leaked connections, no SQLite lock errors, stable memory.

## 12. Acceptance Criteria

The MVP is "done" when **all** of the following hold:

1. `cargo build --release` produces a single `openproxy-server` binary that starts
   on `0.0.0.0:8080` and serves `GET /v1/health` returning 200 with
   `{ "status": "ok", "version": "..." }` (no auth required).
2. `POST /v1/chat/completions` against a configured OpenRouter account returns a
   valid OpenAI Chat Completions response (non-streaming) for at least one
   discovered model.
3. The same endpoint, with `"stream": true`, returns a valid SSE stream ending with
   `data: [DONE]\n\n` for the same model.
4. `GET /v1/models` returns the union of models from at least one OpenRouter account
   and the seeded MiniMax Coding model.
5. A combo configured with strategy `priority` selects the lowest `priority_order`
   target that is healthy and not rate-limited; the same combo with strategy
   `round_robin` cycles through eligible targets.
6. With two accounts registered for the same provider, the engine alternates
   accounts under `round_robin` within that provider.
7. A `usage` row is written for every request — successful or not — with non-null
   `request_id`, `status_code`, `total_ms`, and token counts when available.
8. Forcing an upstream timeout (via a stub that hangs > `idle_chunk_ms`) produces an
   HTTP 504 with `Retry-After` and a `usage` row with `status_code = 504`.
9. The `x-request-id` header sent by the client (or generated if absent) appears
   in the response headers, the upstream request headers, the `usage` row, and
   every structured log line for the request.
10. With the `dashboard` feature disabled, the server binary contains **no** code
    from `openproxy-web`; verified by `cargo bloat --release` not listing any
    dashboard symbols.
11. Migration runner applies all bundled migrations on an empty DB and is
    idempotent on restart (no duplicate `schema_migrations` rows). The
    `migration_idempotence` integration test (described in §11) passes.
12. All unit, integration, and property tests pass under `cargo test --workspace`
    in CI.
13. Every `usage` row persists `connect_ms`, `ttft_ms`, `total_ms`, and
    `tokens_per_sec` for each request — successful or not — and the per-phase
    percentiles are exposed via `GET /v1/admin/usage/latency`.
14. A combo with `race_size = 2` and two healthy targets: the first response
    wins, the loser is cancelled in under 500ms, both rows exist in `usage`
    with the same `request_id` and distinct `trace_id`s, and the loser row has
    `race_lost = 1`, `status_code = 499`, `error_msg = 'race_lost'`.
15. Race+retry cardinality: `max_attempts=3` and `race_size=2` with 4 combo
    targets produce at most 6 upstream requests per user request (3 attempts
    × 2 racing targets). Verified by counting `usage` rows sharing the same
    `request_id` and asserting `count <= 6`. Race executes only on the first
    attempt; subsequent attempts re-launch the race from target 1.
16. `race_size=2` with one upstream that hangs the TCP connection (wiremock
    accepts TCP but never sends a response): the winner completes normally,
    the loser is cancelled in `<= abort_grace_ms + 100ms`, and a WARN log
    is emitted with `phase=race_lost_persist_failed` if the usage row was
    not persisted within the grace window.
17. `race_size=2` with one target returning 503 pre-body and another
    returning a valid 200: the 200 target wins, the 503 is discarded, the
    client receives 200, both rows exist in `usage`, the 503 row has
    `status_code=503` and `race_lost=1`.
18. Modifying a value in `provider_timeouts` via
    `PUT /v1/admin/providers/{id}/timeouts` is reflected in the next request
    without restart. Modifying `[timeouts]` in `config.toml` requires restart
    (verified by a test that changes the file, makes a request, and observes
    the old value).
19. A combo with `race_size=1` produces usage rows with `race_total=1`,
    `race_lost=0`, and no race-related log lines (`race_started`,
    `race_winner`, `race_loser`).
20. A combo with `race_size=2` and `max_attempts=3`, where 1 race has 1
    winner and 1 loser, and 2 retry rounds each have 1 winner and 1 loser,
    produces 6 usage rows total: 1 winner + 1 loser (attempt 1), 1 winner
    + 1 loser (attempt 2), 1 winner + 1 loser (attempt 3).
    `unique_requests=1`, `total_rows=6`.
21. Given a uniform distribution of 10K samples between 0 and 1000ms,
    `GET /v1/admin/usage/latency` returns `p50 ≈ 500ms ± 5%` and
    `p95 ≈ 950ms ± 5%`.
22. `race_size=2` with one account going unhealthy mid-race (simulated by
    external `PUT /v1/admin/accounts/{id} {"health_status": "unhealthy"}`
    after the race starts): the race continues with the original 2 lanes;
    the newly-unhealthy account's lane is not retroactively cancelled.

## 13. Implementation Phases

The MVP is broken into six phases. Each phase ends with a green `cargo test` and a
demoable artifact.

### Phase 0 — Workspace skeleton (½ day)
- Create the workspace `Cargo.toml` and the four crate skeletons with empty `lib.rs` /
  `main.rs`.
- Verify the dependency DAG compiles.
- CI: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`.

### Phase 1 — Storage and migrations (1 day)
- Implement `openproxy-core::storage` (rusqlite pool, migration runner).
- Add all tables from §8 plus the migration files.
- Add storage tests: schema round-trip, migration idempotence, FK enforcement.

### Phase 2 — Provider adapters (2 days)
- Implement the `ProviderAdapter` trait and the three MVP providers.
- Implement the `OpenAIToOpenAI` and `OpenAIToAnthropic` translators.
- Implement SSE normalizers for both formats.
- Unit tests with golden traces; integration tests with a wiremock-style stub.

### Phase 3 — HTTP server and combo engine (2 days)
- axum router: `POST /v1/chat/completions`, `GET /v1/models`.
- Combo engine: priority and round_robin, with account rotation.
- Wire timeout model and `request_id` / `trace_id` propagation.
- Integration tests covering streaming, timeouts, rate-limits, combo misses.

### Phase 4 — Cost and analytics (1 day)
- Pricing tables; per-request cost computation.
- Analytics queries (§7) on a read-only connection.
- Admin endpoints under `/v1/admin/*` with bearer-key auth.

### Phase 5 — Polish and release (1 day)
- Structured logging, configuration loading, env overrides.
- Documentation: this file, `architecture.md`, `README.md` quickstart.
- Final acceptance pass against §12; tag `v0.1.0`.

The dashboard (`openproxy-web`) is intentionally **not** part of the MVP phases. It
ships later as a separate workstream gated by the `dashboard` feature flag.
