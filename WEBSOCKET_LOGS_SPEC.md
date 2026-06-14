# WebSocket Live Logs Refactor — Specification

## 1. Purpose

Refactor the dashboard Live Logs view from 2-second HTTP polling to a pure push model backed by WebSocket streaming.

Current behavior:

- Frontend polls `/web/api/usage/recent?since_id=N&limit=100` every 2 seconds.
- Minimum visible latency is roughly 2 seconds.
- Poll windows can produce race conditions where rows appear out of order.
- Rows are not clickable and do not expose request/response detail.
- Streaming responses are not visible token-by-token.
- The UI cannot clearly show connection health or reconnect state.

Target behavior:

- Core publishes each completed usage row through a single broadcast channel.
- Admin WebSocket endpoint streams live rows to authenticated clients.
- Web crate proxies the WebSocket endpoint bidirectionally.
- Dashboard opens a WebSocket, renders newest rows first, and opens a detail modal for any row.
- Streaming requests expose live token arrival in the detail modal.
- No polling interval is used for Live Logs.

## 2. Scope

### In scope

1. Core broadcast channel for new usage rows.
2. Server WebSocket endpoint `/v1/admin/usage/stream`.
3. Web crate WebSocket proxy `/web/api/usage/stream`.
4. Frontend WebSocket client in `crates/openproxy-web/src/static/app.js`.
5. Live Logs card UI, detail modal, error details, race details, and streaming token view.
6. Backward-compatible migration from polling to streaming.
7. Error handling, reconnect behavior, and acceptance criteria.

### Out of scope

1. General WebSocket infrastructure for non-log views.
2. Multi-user collaboration state.
3. Persistent event log outside SQLite.
4. Server-side filtering for the first implementation.
5. Token-level streaming for requests that did not already produce streaming SSE data.

## 3. Current Architecture Notes

Relevant existing files:

- `crates/openproxy-core/src/usage.rs`
  - Defines `RecentUsageRow`.
  - Provides `recent(conn, since_id, limit) -> Result<Vec<RecentUsageRow>>`.
  - This read-side query is currently used by the dashboard polling endpoint.
- `crates/openproxy-core/src/cost.rs`
  - `record(conn, input) -> Result<UsageId>` inserts a usage row.
  - This is the place where the pipeline should publish the completed row.
- `crates/openproxy-server/src/handlers/admin.rs`
  - Contains usage analytics handlers and `usage_recent`.
  - Will gain `usage_stream`.
- `crates/openproxy-server/src/router.rs`
  - Routes `/v1/admin/usage/recent`.
  - Will add `/v1/admin/usage/stream`.
- `crates/openproxy-server/src/state.rs`
  - `AppState` owns shared server state.
  - Must own the broadcast sender and recent-row cache.
- `crates/openproxy-web/src/api_proxy.rs`
  - Currently buffers HTTP bodies and proxies `/web/api/*` to `/v1/admin/*`.
  - Must gain special-case WebSocket upgrade handling for `/usage/stream`.
- `crates/openproxy-web/src/static/app.js`
  - Current `renderLogs()` starts `setInterval(pollLogs, 2000)`.
  - Must be replaced with WebSocket client logic.

## 4. Module Boundaries

### 4.1 Core crate

Responsibility:

- Own the canonical source of truth for usage persistence.
- Own the in-process broadcast of newly completed usage rows.
- Expose query helpers for recent history and row detail.
- Keep all secret redaction and privacy filtering in core.

Modules:

1. `crates/openproxy-core/src/usage.rs`

   Responsibilities:

   - Define `RecentUsageRow` and any new detail fields.
   - Provide history queries:
     - `recent(conn, since_id, limit)` for backward compatibility.
     - `recent_desc(conn, limit)` for WebSocket initial history.
     - `detail(conn, request_id)` or `detail_by_id(conn, id)` for modal detail.
   - Provide redaction/filtering helpers for headers and bodies.
   - Provide tests for ordering, limit, detail, and redaction.

2. `crates/openproxy-core/src/cost.rs` or a new `usage::broadcast` module

   Responsibilities:

   - After a usage row is successfully inserted, convert the input plus persisted row id into `RecentUsageRow`.
   - Publish the row to the broadcast sender.

Decision:

- Prefer a new module `crates/openproxy-core/src/usage.rs` for the broadcast sender type and helper functions because `RecentUsageRow` already lives there.
- `cost::record` remains the only insert function. It calls the broadcast helper after `last_insert_rowid()` succeeds.

### 4.2 Server crate

Responsibility:

- Own admin HTTP and WebSocket endpoints.
- Authenticate the same way as other admin endpoints.
- Send initial history from SQLite.
- Stream new rows from the core broadcast channel.
- Enforce backpressure policy.

Modules:

1. `crates/openproxy-server/src/state.rs`

   Responsibilities:

   - Add `UsageStreamState` or equivalent to `AppState`.
   - Initialize broadcast sender and recent-row ring buffer during `AppState::new`.

2. `crates/openproxy-server/src/handlers/admin.rs`

   Responsibilities:

   - Add `usage_stream` WebSocket upgrade handler.
   - Validate optional client messages:
     - `{"type":"subscribe","since_id":N}`
     - `{"type":"ping"}`
   - Send initial history and live rows.
   - Send errors as JSON messages where possible.

3. `crates/openproxy-server/src/router.rs`

   Responsibilities:

   - Register `GET /v1/admin/usage/stream`.

### 4.3 Web crate

Responsibility:

- Proxy browser WebSocket traffic to core without buffering frames.
- Preserve auth headers and forwarded metadata.
- Leave existing HTTP proxy behavior unchanged.

Module:

- `crates/openproxy-web/src/api_proxy.rs`

Responsibilities:

- Detect `GET /usage/stream` after `/web/api` stripping.
- Upgrade to WebSocket locally.
- Open an upstream WebSocket to `${OPENPROXY_CORE_URL}/v1/admin/usage/stream`.
- Forward frames bidirectionally.
- Pass through authorization headers.
- Return 502/Bad Gateway on upstream connection failure.

### 4.4 Frontend

Responsibility:

- Own UI behavior and client-side state.

Module:

- `crates/openproxy-web/src/static/app.js`

Responsibilities:

- Replace `setInterval` Live Logs polling with WebSocket client.
- Render connection state.
- Prepend newest rows to the log list.
- Open detail modal on row click.
- Render request/response JSON, headers, timing, errors, race info, and copy controls.
- Show live token arrival for streaming responses.

## 5. Data Structures

### 5.1 `RecentUsageRow`

Existing shape:

```rust
pub struct RecentUsageRow {
    pub id: UsageId,
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub status_code: u16,
    pub total_ms: u64,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub cost_usd: Option<f64>,
    pub race_lost: bool,
    pub created_at: String,
}
```

Extended shape for detail and streaming:

```rust
pub struct RecentUsageRow {
    pub id: UsageId,
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub status_code: u16,
    pub total_ms: u64,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub cost_usd: Option<f64>,
    pub race_lost: bool,

    // Existing timing fields.
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,

    // Detail/debugging fields. Optional because rows may be large or sensitive.
    pub request_body_json: Option<serde_json::Value>,
    pub response_body_json: Option<serde_json::Value>,
    pub request_headers: Option<BTreeMap<String, String>>,
    pub response_headers: Option<BTreeMap<String, String>>,
    pub error_message: Option<String>,

    // Race metadata.
    pub race_total: Option<u8>,
    pub race_attempts: Option<u8>,
    pub race_lost: bool,

    // Streaming metadata.
    pub is_streaming: bool,
    pub stream_complete: bool,
}
```

Recommended refinement:

- Do not overload `race_lost` twice. Keep the existing bool and add `race_total`/`race_attempts`.
- Use `error_message` as the public JSON field name, serialized from `error_msg` or `error_msg_redacted`.
- Store only redacted/filtered request and response payloads.
- If payload size exceeds configured cap, set:
  - `request_body_json: {"__truncated": true, "__reason": "size_limit"}`
  - or `null` with `request_body_truncated: true`.
- If a field is not captured, use `null`.

### 5.2 WebSocket envelope

Server → client messages are JSON objects, one per WebSocket message:

#### Initial history batch

```json
{
  "type": "history",
  "rows": [
    { "id": 123, "request_id": "...", "status_code": 200 }
  ]
}
```

#### New row

```json
{
  "type": "row",
  "data": {
    "id": 124,
    "request_id": "...",
    "status_code": 200
  }
}
```

#### Server error

```json
{
  "type": "error",
  "message": "database query failed"
}
```

### 5.3 Client → server messages

Optional, but recommended for reconnect and keepalive.

#### Subscribe from a known id

```json
{
  "type": "subscribe",
  "since_id": 12345
}
```

Behavior:

- If received before live streaming starts, server should send rows with `id > since_id` as `row` messages, not a full `history` batch.
- If `since_id` is missing or `0`, use default history size.

#### Ping

```json
{
  "type": "ping"
}
```

Server response:

```json
{
  "type": "pong",
  "server_time": "2026-06-14T12:00:00Z"
}
```

## 6. Core Broadcast Design

### 6.1 Broadcast sender type

Recommended type:

```rust
pub type UsageBroadcastSender = tokio::sync::broadcast::Sender<RecentUsageRow>;
```

Where to store:

- Add to `AppState` in `openproxy-server`.
- Provide a clone to the core broadcast helper during `AppState::new`.

Alternative:

- Store the sender in a core-level static or `AppContext`.
- Not recommended because it couples core behavior to server state and makes testing harder.

### 6.2 Recent-row ring buffer

The broadcast channel is not sufficient for late subscribers because:

- `broadcast::Receiver` only receives messages sent after it subscribes.
- A dashboard reload after a quiet period would receive no rows.

Therefore, `AppState` should also maintain a recent-row ring buffer.

Recommended type:

```rust
pub struct UsageStreamState {
    pub broadcast: UsageBroadcastSender,
    pub recent_rows: tokio::sync::RwLock<VecDeque<RecentUsageRow>>,
    pub history_limit: usize,
}
```

Default:

- `history_limit = 50`.

Behavior:

- Every completed usage row is appended to the ring buffer.
- If buffer length exceeds `history_limit`, remove the oldest row.
- The buffer is protected by `RwLock` because WebSocket handlers read it asynchronously.
- The ring buffer is supplemental. SQLite remains the source of truth.

### 6.3 Publishing after write

Sequence inside `cost::record`:

1. Compute cost.
2. Insert into SQLite.
3. Read `last_insert_rowid()`.
4. Build `RecentUsageRow` from `UsageInput`, computed `cost_usd`, and `UsageId`.
5. Publish to broadcast.
6. Append to recent-row ring buffer.
7. Return `UsageId`.

Ordering guarantee:

- The row must be published only after the database insert succeeds.
- If publish fails because there are no subscribers, ignore the error.
- If ring buffer append fails due to lock poisoning, log a warning but do not fail the request.

## 7. Server WebSocket Endpoint

### 7.1 Route

```http
GET /v1/admin/usage/stream
Upgrade: websocket
Authorization: Bearer <token>
```

Registered in:

```rust
.route("/v1/admin/usage/stream", get(handlers::admin::usage_stream))
```

### 7.2 Authentication

Use the same auth path as other admin endpoints.

Implementation options:

1. If existing admin auth middleware already wraps `admin_routes`, the WebSocket route inherits it.
2. If auth is per-handler, call the same extractor/helper used by existing admin handlers.

Requirements:

- Missing bearer token: `401`.
- Invalid token: `401`.
- Token without admin scope: `403`.
- Successful auth: upgrade to WebSocket.

### 7.3 Initial history

On connect:

1. Authenticate.
2. Read last N rows from SQLite ordered newest first.
3. Send:

```json
{ "type": "history", "rows": [...] }
```

Recommended query:

```sql
SELECT ...
FROM usage
ORDER BY id DESC
LIMIT ?
```

Then reverse in memory before sending if the wire contract expects history in newest-first order.

Default:

- `USAGE_STREAM_DEFAULT_HISTORY_LIMIT = 50`.

Hard cap:

- `USAGE_STREAM_MAX_HISTORY_LIMIT = 500`.

### 7.4 Live streaming

After sending history:

1. Subscribe to `broadcast::Sender<RecentUsageRow>`.
2. Loop receiving rows.
3. For each row, send:

```json
{ "type": "row", "data": row }
```

Ordering guarantee:

- Within a single WebSocket connection, messages are sent in broadcast receive order.
- The frontend is responsible for rendering by `id` descending, not by arrival order alone.

### 7.5 Optional subscribe message

If client sends:

```json
{ "type": "subscribe", "since_id": 12345 }
```

Server behavior options:

1. Simple mode:
   - Ignore `subscribe` because initial history was already sent.
   - This is acceptable for MVP.
2. Preferred mode:
   - If received before history is sent, query rows with `id > since_id` and send as `row` messages, then continue live stream.
   - This supports reconnect without a separate reconnect endpoint.

Recommended MVP:

- Accept `subscribe` but do not require it.
- Client may send it immediately after opening.
- Server can send history first, then live rows.
- Frontend deduplicates by `id`.

### 7.6 Ping/pong

Client ping:

```json
{ "type": "ping" }
```

Server response:

```json
{ "type": "pong", "server_time": "..." }
```

If the server receives malformed JSON:

- Send:

```json
{
  "type": "error",
  "message": "invalid client message"
}
```

- Do not close the socket unless messages are repeatedly malformed.

## 8. Web Crate WebSocket Proxy

### 8.1 Route behavior

Existing proxy path:

- Browser: `/web/api/usage/stream`
- After `/web/api` nest strips prefix: upstream path becomes `/usage/stream`
- Required upstream URL:

```text
${OPENPROXY_CORE_URL}/v1/admin/usage/stream
```

### 8.2 Special-case WebSocket

`api_proxy.rs` currently buffers the whole request body. That must not be used for WebSockets.

Required behavior:

1. Detect WebSocket upgrade:
   - `Connection` header contains `upgrade`
   - `Upgrade` header is `websocket`
   - Or use axum `WebSocketUpgrade` extractor.
2. If path is `/usage/stream`, create a local WebSocket upgrade response.
3. Connect to upstream WebSocket URL.
4. Forward frames bidirectionally:
   - Browser → upstream
   - Upstream → browser
5. Preserve auth headers:
   - `authorization`
   - `cookie`, if used by deployment
   - `x-forwarded-host`
   - `x-forwarded-proto`
6. Strip hop-by-hop headers:
   - `connection`
   - `upgrade`
   - `sec-websocket-key`
   - `sec-websocket-version`
   - `sec-websocket-extensions`
   - `sec-websocket-protocol`
   - `host` when reqwest sets it
   - `content-length`
7. Return `502 Bad Gateway` if upstream cannot be reached.

### 8.3 Backpressure in proxy

Preferred:

- Stream frames directly between sockets.
- If either side is closed, close the other.
- If frame conversion fails, log and close.

Avoid:

- Buffering all frames in memory.
- Converting WebSocket frames to HTTP bodies.

## 9. Frontend Design

### 9.1 State additions

Add to `state`:

```js
logs: {
  rows: [],
  rowById: new Map(),
  lastLogId: 0,
  ws: null,
  reconnectAttempt: 0,
  reconnectTimer: null,
  status: 'disconnected', // 'connected' | 'disconnected' | 'reconnecting'
  pendingHistorySinceId: null,
  selectedRow: null,
  liveTokens: new Map(),
}
```

Recommended minimal shape:

```js
const state = {
  // existing fields...
  logsRows: [],
  logsRowById: new Map(),
  lastLogId: 0,
  logsWs: null,
  logsReconnectAttempt: 0,
  logsReconnectTimer: null,
  logsStatus: 'disconnected',
  logsSelectedRow: null,
  logsLiveTokens: new Map(),
};
```

### 9.2 WebSocket URL

```js
const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
const url = `${scheme}//${location.host}/web/api/usage/stream`;
```

### 9.3 Auth header

Browser WebSocket constructor cannot set arbitrary headers.

Options:

1. Use query parameter:

```text
?access_token=...
```

Not recommended unless token is short-lived and storage is secure.

2. Use cookie auth.

Recommended if dashboard already uses cookies/session.

3. If bearer token is stored in memory/localStorage, append it only if deployment explicitly supports it:

```text
?token=<encoded-token>
```

Spec requirement says pass through auth headers. For browser-to-web proxy, this can only happen if the browser includes credentials/cookies or query token. The web proxy must forward any headers it receives during the WebSocket handshake.

Recommended contract:

- If same-origin cookies are used, set `ws.withCredentials = true`.
- If bearer token is required, use a short-lived query token generated by the server, not raw long-lived admin token.
- Do not store bearer tokens in URLs permanently.

### 9.4 Connection lifecycle

Initial connect:

```text
renderLogs()
  -> clear logs
  -> set status reconnecting
  -> open WebSocket
  -> on open:
       status = connected
       reconnectAttempt = 0
       send subscribe since lastLogId if lastLogId > 0
  -> on message:
       parse JSON
       handle history/row/error/pong
  -> on close:
       status = disconnected
       schedule reconnect
  -> on error:
       close socket
```

Reconnect backoff:

```js
const delays = [1000, 2000, 4000, 8000, 16000, 30000];
const delay = Math.min(30000, 1000 * 2 ** Math.min(logsReconnectAttempt, 5));
logsReconnectAttempt += 1;
```

Status indicator:

- 🟢 connected
- 🔴 disconnected
- 🟡 reconnecting

UI location:

```html
<div class="logs-header">
  <h2>Live Logs</h2>
  <span id="logs-connection-status" class="status disconnected">🔴 disconnected</span>
</div>
```

### 9.5 Message handling

#### `history`

```js
function handleHistory(rows) {
  if (!Array.isArray(rows)) return;

  // Merge newest-first, dedupe by id.
  const incoming = rows.slice().sort((a, b) => b.id - a.id);
  state.logsRows = mergeLogsByDescId(state.logsRows, incoming);
  state.lastLogId = Math.max(state.lastLogId, ...incoming.map(r => r.id));
  renderLogsRows();
}
```

#### `row`

```js
function handleRow(row) {
  upsertLogRow(row);
  state.lastLogId = Math.max(state.lastLogId, row.id);

  if (row.is_streaming && !row.stream_complete) {
    state.logsLiveTokens.set(row.request_id, []);
  }

  renderLogsRows();
  updateSelectedRowIfOpen(row);
}
```

#### `error`

```js
function handleServerError(message) {
  showLogsError(message);
}
```

#### `pong`

Ignore or update latency.

### 9.6 Rendering log rows

Requirements:

- Each row is a clickable card.
- Newest always at top.
- Visual states:
  - success: green
  - error: red
  - racing loser: dimmed
  - streaming: pulsing
- Columns:
  - time
  - status badge
  - provider
  - model
  - tokens
  - latency
  - cost

Suggested HTML:

```html
<div class="logs-header">
  <h2>Live Logs</h2>
  <span id="logs-connection-status">🟡 reconnecting</span>
</div>

<div id="logs" class="logs-list">
  <div class="empty">No recent requests yet.</div>
</div>
```

Row:

```html
<button class="log-card ok streaming" data-request-id="...">
  <span class="log-time">...</span>
  <span class="status-badge">200</span>
  <span class="provider">openrouter</span>
  <span class="model">openai/gpt-4o</span>
  <span class="tokens">1.0k↓ 500↑</span>
  <span class="latency">1200ms</span>
  <span class="cost">$0.0123</span>
</button>
```

Use `<button>` rather than `<div>` for accessibility and click handling.

### 9.7 Detail modal

Opening:

- Click any row.
- Load detail by `request_id` if not already present.
- Show modal with sections:

1. Summary
   - request id
   - trace id
   - provider
   - model
   - status
   - timing
   - cost
2. Request JSON
3. Response JSON
4. Request headers
5. Response headers
6. Timing breakdown
   - connect
   - ttft
   - total
7. Error details
8. Race info
9. Streaming response
   - live indicator
   - tokens accumulated so far

Modal controls:

- Close.
- Copy request JSON.
- Copy response JSON.
- Copy error details.
- Copy full detail JSON.

### 9.8 Detail endpoint fallback

WebSocket rows may not contain full detail for privacy or size reasons.

Add HTTP fallback:

```http
GET /v1/admin/usage/request/:request_id
```

Web proxy path:

```http
GET /web/api/usage/request/:request_id
```

Response:

```json
{
  "row": { ...RecentUsageRow with detail fields... }
}
```

If `request_id` is not found:

- `404`
- Frontend shows: `Request detail not found`.

### 9.9 Streaming response view

Requirement:

> Use the existing SSE translation logic but expose to UI.

Design:

- Core already translates upstream SSE to OpenAI-compatible chunks in `openproxy-core/src/sse.rs`.
- Add a live token capture path in the chat/stream handling pipeline:
  - When a streaming chunk contains `choices[].delta.content`, append content to an in-memory token buffer keyed by `request_id`.
  - When the stream completes, mark `stream_complete = true`.
  - When usage is recorded, include the captured token buffer or a reference to it in `RecentUsageRow`.

Recommended core structure:

```rust
pub struct StreamCapture {
    pub request_id: RequestId,
    pub chunks: Vec<serde_json::Value>,
    pub token_text: String,
    pub complete: bool,
}
```

Recommended state:

```rust
pub struct StreamCaptureStore {
    captures: tokio::sync::RwLock<HashMap<RequestId, StreamCapture>>,
    max_chunks_per_request: usize,
}
```

Default:

- `max_chunks_per_request = 2000`
- `max_token_text_chars = 200_000`

Behavior:

- If a request is non-streaming, `is_streaming = false`.
- If streaming and still running, `is_streaming = true`, `stream_complete = false`.
- If streaming completed before usage row is recorded, `is_streaming = true`, `stream_complete = true`.
- If detail is opened while stream is active, the modal polls or subscribes to token updates.

For MVP, if true per-token push is too large:

- Send token deltas over the same logs WebSocket using a new message type:

```json
{
  "type": "stream_tokens",
  "request_id": "...",
  "delta": "Hello ",
  "complete": false
}
```

Then:

```json
{
  "type": "stream_tokens",
  "request_id": "...",
  "delta": "world",
  "complete": true
}
```

This satisfies real-time token arrival in the modal without requiring a second WebSocket.

## 10. Sequence Diagrams

### 10.1 New request completion

```text
User/Client
   |
   | chat request
   v
openproxy-server chat handler
   |
   | upstream request
   v
Provider
   |
   | response/stream
   v
openproxy-server chat handler
   |
   | UsageInput
   v
openproxy-core::cost::record(conn, input)
   |
   | INSERT INTO usage
   v
SQLite
   |
   | last_insert_rowid()
   v
cost::record
   |
   | build RecentUsageRow
   v
Usage broadcast sender
   |
   | broadcast::send(row)
   v
Recent-row ring buffer
   |
   | append row, trim oldest
   v
/v1/admin/usage/stream clients
   |
   | {"type":"row","data":row}
   v
/web/api/usage/stream proxy
   |
   | WebSocket frame
   v
Dashboard Live Logs
   |
   | prepend card
   v
UI
```

### 10.2 Initial dashboard load

```text
Browser
   |
   | navigate #/logs
   v
renderLogs()
   |
   | clear rows
   | set status reconnecting
   v
new WebSocket('/web/api/usage/stream')
   |
   | WebSocket handshake + auth
   v
openproxy-web api_proxy
   |
   | proxy WebSocket frames
   v
openproxy-server /v1/admin/usage/stream
   |
   | authenticate admin token
   v
SQLite recent history
   |
   | rows ORDER BY id DESC LIMIT 50
   v
server
   |
   | {"type":"history","rows":[...]}
   v
browser
   |
   | render newest first
   v
Live Logs UI
```

### 10.3 Reconnect after core restart

```text
Browser
   |
   | existing WS closes
   v
onclose
   |
   | status = disconnected
   | schedule reconnect delay
   v
timer expires
   |
   | open new WebSocket('/web/api/usage/stream')
   v
openproxy-web api_proxy
   |
   | connect to core
   v
openproxy-server
   |
   | authenticate
   | query recent history
   v
server
   |
   | {"type":"history","rows":[...]}
   v
browser
   |
   | merge by id
   | lastLogId = max seen id
   | status = connected
   v
Live Logs UI
```

### 10.4 Detail modal open

```text
User
   |
   | click log card
   v
openLogDetail(row)
   |
   | if row.detail fields are complete:
   |   render modal
   | else:
   v
GET /web/api/usage/request/:request_id
   |
   v
openproxy-web proxy
   |
   v
openproxy-server
   |
   v
SQLite detail query
   |
   v
response JSON
   |
   v
modal sections
```

### 10.5 Streaming token arrival

```text
Provider SSE stream
   |
   v
openproxy-server chat handler
   |
   | translated SSE chunk
   v
StreamCaptureStore
   |
   | append token delta
   v
Usage logs WebSocket
   |
   | {"type":"stream_tokens","request_id":"...","delta":"..."}
   v
Dashboard
   |
   | if detail modal open for request:
   |   append token text
   | else:
   |   mark row streaming and store token buffer
```

## 11. Error Handling Strategy

### 11.1 Core publish errors

Broadcast send error:

- Expected when no subscribers exist.
- Ignore and continue.

Ring buffer lock error:

- Log warning.
- Continue request.

Database insert success but publish fails:

- Do not roll back.
- The row exists in SQLite and will be available on reconnect/history.

### 11.2 Server WebSocket errors

Authentication failure:

- Do not upgrade.
- Return HTTP `401` or `403`.

SQLite history query failure:

- If before sending any message, close with WebSocket close code `1011` and log error.
- If possible, send:

```json
{ "type": "error", "message": "failed to load usage history" }
```

then close.

Broadcast receive error:

- `Lagged(n)` from broadcast:
  - Send error message:

```json
{
  "type": "error",
  "message": "client lagged by N rows; reloading history"
}
```

  - Reload last N rows from SQLite.
  - Send new `history` message.
  - Continue.

- Channel closed:
  - Close WebSocket gracefully.

Backpressure:

- If `socket.send()` returns `WouldBlock` or equivalent:
  - Drop the oldest pending frame if using an internal queue.
  - If no queue is used, close with code `1008` or `1011`.
- Recommended MVP:
  - No large internal queue.
  - Close on persistent send failure and let client reconnect.

### 11.3 Web proxy errors

Upstream WebSocket connect failure:

- Return `502 Bad Gateway`.

Frame forward failure:

- Log warning.
- Close both sockets.

Header forwarding failure:

- Do not forward invalid header values.
- Log warning.

### 11.4 Frontend errors

WebSocket connection failure:

- Set status `disconnected`.
- Show banner:

```text
Live Logs disconnected. Reconnecting...
```

- Schedule reconnect.

JSON parse failure:

- Log to console.
- Show transient inline warning.
- Do not close socket.

Server `error` message:

- Show inline banner.
- Do not clear existing rows.

Reconnect failure:

- Increment backoff.
- Max delay 30 seconds.
- Continue until page unload.

Detail fetch failure:

- Show modal error:

```text
Could not load request detail: <message>
```

Streaming token failure:

- Show `stream incomplete` indicator.
- Do not block request/response detail.

## 12. Security and Privacy

### 12.1 Header filtering

Never send raw request headers containing secrets.

Blocked header names:

- `authorization`
- `proxy-authorization`
- `cookie`
- `set-cookie`
- `x-api-key`
- `api-key`
- `openai-api-key`
- any header containing `token`, `secret`, `key`, `password`, `credential`, case-insensitive, unless explicitly allowlisted.

Recommended policy:

- Default deny for sensitive names.
- Explicit allowlist for safe headers:
  - `content-type`
  - `accept`
  - `user-agent`
  - `x-request-id`
  - `traceparent`

### 12.2 Body filtering

Request and response bodies can contain secrets.

Rules:

- Redact before storing or sending to WebSocket.
- Cap payload size.
- Never include raw OAuth tokens, API keys, cookies, or bearer strings.
- Reuse existing `cost::redact_error_msg` style patterns and extend to JSON bodies.

Recommended redaction patterns:

```text
sk-...
x-api-key: ...
Authorization: Bearer ...
api_key: ...
apiKey: ...
access_token: ...
refresh_token: ...
client_secret: ...
password: ...
```

### 12.3 Admin auth

- WebSocket endpoint must require admin scope.
- Web proxy must not weaken core auth.
- Browser token handling must avoid long-lived tokens in query strings where possible.

## 13. Performance Requirements

### 13.1 Latency

Acceptance target:

- New requests appear at top of list within 100ms of completion under normal local/network conditions.

Design implications:

- Broadcast immediately after DB write.
- Do not wait for polling interval.
- Do not buffer live rows in the server.
- Send one JSON message per row.

### 13.2 History size

Default:

- 50 rows.

Hard cap:

- 500 rows.

### 13.3 Payload size

Default caps:

- Request body JSON: 64 KiB before redaction/truncation.
- Response body JSON: 256 KiB before redaction/truncation.
- Streaming token text: 200 KiB.
- Headers: 50 headers per direction.

If exceeded:

- Send truncated marker.
- Do not fail the request.

## 14. Migration Path from Polling

### Phase 1 — Backward-compatible core changes

1. Extend `RecentUsageRow` with optional detail fields.
2. Add `recent_desc` query for newest-first history.
3. Add `detail_by_request_id` query.
4. Keep `recent(since_id, limit)` unchanged.
5. Add broadcast sender type and helper functions.

### Phase 2 — Server broadcast integration

1. Add `UsageStreamState` to `AppState`.
2. Initialize broadcast sender and ring buffer in `AppState::new`.
3. Modify `cost::record` to publish after insert.
4. Add tests proving:
   - row is published after insert
   - no subscribers does not fail insert
   - ring buffer keeps newest rows only

### Phase 3 — WebSocket endpoint

1. Add `usage_stream` handler.
2. Register route.
3. Send initial history.
4. Stream broadcast rows.
5. Add auth tests.

### Phase 4 — Web proxy

1. Add WebSocket special-case in `api_proxy.rs`.
2. Keep existing HTTP proxy behavior unchanged.
3. Add tests for:
   - non-WebSocket requests still proxied
   - `/usage/stream` is not buffered
   - auth headers are forwarded

### Phase 5 — Frontend migration

1. Replace `renderLogs()` polling setup with WebSocket setup.
2. Remove `setInterval(pollLogs, 2000)` for logs.
3. Keep `startBackgroundPolling()` for other dashboard resources.
4. Add connection status indicator.
5. Add row rendering and detail modal.
6. Add reconnect logic.
7. Add streaming token handling.

### Phase 6 — Cleanup

1. Keep `/v1/admin/usage/recent` temporarily for compatibility.
2. Mark it deprecated in comments/docs.
3. Remove frontend polling code only after WebSocket is stable.
4. Optionally remove polling endpoint in a later major version.

## 15. Acceptance Criteria

### Functional

1. ✅ Dashboard loads, WebSocket connects within 500ms under normal conditions.
2. ✅ New requests appear at the top of the list within 100ms of completion.
3. ✅ Clicking any row opens a detail modal.
4. ✅ Error rows show error details in the modal.
5. ✅ Streaming requests show live token arrival in the modal.
6. ✅ Reconnection works after core restart.
7. ✅ No polling interval for Live Logs; pure push.
8. ✅ Build passes.
9. ✅ Existing tests pass.
10. ✅ Non-logs dashboard features continue to work.

### API contract

Server → client:

```json
{"type": "history", "rows": [RecentUsageRow, ...]}
{"type": "row", "data": RecentUsageRow}
{"type": "error", "message": "..."}
```

Client → server:

```json
{"type": "subscribe", "since_id": 12345}
{"type": "ping"}
```

Streaming token extension:

```json
{"type": "stream_tokens", "request_id": "...", "delta": "...", "complete": false}
```

### UI contract

- Newest rows are always at the top.
- Rows are clickable cards.
- Status colors:
  - success: green
  - error: red
  - racing loser: dimmed
  - streaming: pulsing
- Connection status:
  - 🟢 connected
  - 🔴 disconnected
  - 🟡 reconnecting

## 16. Testing Plan

### Core tests

1. `recent_desc_returns_newest_first`
2. `detail_by_request_id_returns_full_row`
3. `record_publishes_after_insert`
4. `redact_headers_removes_authorization`
5. `redact_body_removes_api_keys`
6. `ring_buffer_trims_oldest_rows`

### Server tests

1. WebSocket auth rejects missing token.
2. WebSocket auth rejects non-admin token.
3. Authenticated client receives `history` message.
4. New row broadcast produces `row` message.
5. `subscribe` message does not crash handler.
6. Malformed client JSON returns `error` message.

### Web proxy tests

1. `/web/api/usage/stream` upgrades to upstream WebSocket.
2. Browser → upstream frame forwarding works.
3. Upstream → browser frame forwarding works.
4. Authorization header is forwarded.
5. Hop-by-hop headers are not forwarded.
6. Existing HTTP proxy tests still pass.

### Frontend tests/manual checks

1. Open `/#/logs`.
2. Confirm status becomes 🟢 connected.
3. Trigger a chat request.
4. Confirm new log appears at top within 100ms.
5. Click success row and inspect modal.
6. Trigger failing request and inspect error details.
7. Trigger streaming request and inspect live token append.
8. Restart core.
9. Confirm socket reconnects and history reloads.
10. Navigate to providers/accounts/combos and confirm no regression.

## 17. Open Decisions

1. **Browser auth transport**
   - Best option depends on current deployment.
   - Cookies are preferred for WebSocket header forwarding.
   - Query token should be short-lived if used.

2. **Detail payload completeness**
   - If full request/response bodies are too sensitive or large, modal should show redacted/truncated payload plus a clear warning.

3. **Streaming token transport**
   - Preferred: include token deltas as `stream_tokens` messages on the logs WebSocket.
   - Alternative: store completed token text only and show it after completion.
   - The requirement explicitly asks for real-time token arrival, so the preferred option should be implemented if feasible.

4. **Polling endpoint retention**
   - Keep `/usage/recent` during migration.
   - Remove only after confirming no external clients depend on it.
