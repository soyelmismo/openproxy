# openproxy: Post-MVP Roadmap & Backlog

Technical roadmap and task list for planned post-MVP capabilities.

---

## 1. Capability Prioritization Matrix

| Tier | Area / Feature | Status | Complexity | Impact |
| :--- | :--- | :--- | :--- | :--- |
| **P1** | **HTTP Transport Compression (`gzip` / `br` / `zstd`)** | ✅ Completed | Low | High |
| **P2** | **MCP (Model Context Protocol) & Tool Gateway** | 📋 Pending | Medium | High |
| **P3** | **Horizontal Scalability & Distributed State** | 📋 Pending | High | High |
| **P4** | **Persistent Memory & Conversation Stores** | 📋 Pending | Medium | Medium |
| **P5** | **Assistants API & Stateful Endpoints** | 📋 Pending | High | Medium |
| **P6** | **Guardrails, Evals & Content Filtering** | 📋 Pending | Medium | Medium |
| **P7** | **Desktop App, System Tray & Native Packaging** | 📋 Pending | High | Low |

---

## 2. Detailed To-Do List by Capability

### 🚀 P1: HTTP Transport Compression (Assets & JSON)

*Objective:* Reduce transfer size for embedded SPA dashboard assets and JSON responses without increasing TTFT latency on SSE streams.

- [x] **Compression Middleware in `openproxy-server`**
  - [x] Integrate `tower-http::compression::CompressionLayer` with support for `gzip`, `brotli` (`br`), and `zstd`.
  - [x] Configure transport compression predicate enforcing strict MIME filtering and bypass rules.
- [x] **Compression of Embedded Static Assets**
  - [x] Enable dynamic on-the-fly compression for frontend bundles (`/admin/dist/*`, CSS, JS, favicons, fonts).
  - [x] Configure cache headers (`Cache-Control: public, max-age=31536000, immutable` for hashed assets).
- [x] **Compression of JSON Data Endpoints**
  - [x] Compress `GET /v1/models` responses (large catalogs with hundreds of models).
  - [x] Compress responses for `/admin/api/usage/*`, `/admin/api/logs`, and `/admin/api/models`.
- [x] **Bypass for SSE Streams (`text/event-stream`)**
  - [x] Ensure SSE streams (`/v1/chat/completions` with `stream: true`) bypass compression buffering, preserving TTFT and per-chunk latency.

---

### 🧩 P2: MCP (Model Context Protocol) & Agent Support

*Objective:* Connect openproxy to tools and agent workflows via the Model Context Protocol.

- [ ] **Integrated MCP Client**
  - [ ] Connector for local (stdio) and remote (SSE/HTTP) MCP servers.
  - [ ] Dynamic discovery of tools and resources provided by configured MCP servers.
- [ ] **Tool Injection and Routing**
  - [ ] Automatic mapping of MCP tools to OpenAI, Anthropic, and Gemini `tools` schemas.
  - [ ] Intercept and execute `tool_calls` targeting registered MCP servers before returning the response to the client.
- [ ] **Dashboard Administration**
  - [ ] Web dashboard view to register and monitor active MCP servers and their tool inventory.

---

### 🌐 P3: Horizontal Scalability & Shared State

*Objective:* Support multiple `openproxy` replicas behind an L7 load balancer.

- [ ] **Distributed State Backend (Optional / Pluggable)**
  - [ ] State abstraction with pluggable backends: in-memory (single process) vs Redis/key-value store.
  - [ ] Synchronize atomic `round_robin` counters across instances.
- [ ] **Distributed Circuit Breaker and Cooldowns**
  - [ ] Pub/Sub for account degradation events and health status.
  - [ ] Synchronize upstream rate-limiting windows (HTTP 429 `Retry-After`).
- [ ] **Distributed Client Rate Limiting**
  - [ ] Redis-backed token bucket algorithm for per-API-key quotas across the cluster.

---

### 🧠 P4: Persistent Memory, Context Stores & Vector Cache

*Objective:* Store conversational context and cache semantic queries.

- [ ] **Conversation Storage (`session_id` / `thread_id`)**
  - [ ] Persistent chat history storage in SQLite or external databases.
  - [ ] Sliding context window and truncation of older messages.
- [ ] **Semantic Caching (Prompt Semantic Cache)**
  - [ ] Embedding storage and lookup for cached responses to semantically similar queries.
  - [ ] Cache invalidation via TTL and cosine similarity threshold.

---

### 🤖 P5: Stateful Support: Assistants API & Threads

*Objective:* Support the OpenAI Assistants API.

- [ ] **Assistants & Threads Endpoints**
  - [ ] `POST /v1/assistants`, `GET /v1/assistants/{id}`.
  - [ ] `POST /v1/threads`, `POST /v1/threads/{id}/messages`.
  - [ ] `POST /v1/threads/{id}/runs` with polling and streaming cycles.
- [ ] **Run Execution Engine**
  - [ ] State machine for the Run lifecycle (`queued` -> `in_progress` -> `requires_action` -> `completed`).

---

### 🛡️ P6: Guardrails, Evals & Content Filtering

*Objective:* Request-layer security inspection and output evaluation.

- [ ] **Guardrails & Safety Filters**
  - [ ] Detect prompt injections and jailbreaks in incoming requests.
  - [ ] PII (Personally Identifiable Information) masking and redaction.
- [ ] **Production Evaluations and Benchmarking**
  - [ ] Shadow evaluation (silent parallel execution against a control model).
  - [ ] A/B routing via weighted combos.

---

### 🖥️ P7: Desktop Application & Native Packaging

*Objective:* Native desktop application and platform packages.

- [ ] **Desktop Packaging**
  - [ ] Tauri integration for cross-platform binaries (Linux, macOS, Windows).
  - [ ] System tray with process control, live logs, and health status.
- [ ] **System Installers and Packages**
  - [ ] Homebrew formula for macOS and Linux.
  - [ ] Debian (`.deb`), Arch (`PKGBUILD`), and Windows (`winget`, `.msi`) packages.

---

## 3. Change Log

- **2026-08-15**: Initial post-MVP roadmap creation prioritizing HTTP compression (`gzip`/`br`), MCP, and distributed state.
