# openproxy — Post-MVP Roadmap & Backlog

This document defines the technical roadmap and structured task list (To-Do List) for planned post-MVP capabilities and features of **openproxy**.

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

*Objective:* Significantly reduce bandwidth and transfer times when loading embedded SPA dashboard assets and bulky JSON responses (model catalog, historical logs, analytics), without impacting TTFT latency on SSE streams.

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
  - [x] Ensure that the SSE stream (`/v1/chat/completions` with `stream: true`) is not subject to compression buffering, preserving the TTFT metric and low chunk-by-chunk latency.

---

### 🧩 P2: MCP (Model Context Protocol) & Agent Support

*Objective:* Connect openproxy to the ecosystem of tools and agent interoperability based on the Model Context Protocol (MCP).

- [ ] **Integrated MCP Client**
  - [ ] Connector for local (stdio) and remote (SSE/HTTP) MCP servers.
  - [ ] Dynamic discovery of tools and resources provided by configured MCP servers.
- [ ] **Tool Injection and Routing**
  - [ ] Automatic mapping of MCP tools to OpenAI / Anthropic / Gemini `tools` schemas.
  - [ ] Intercept and execute `tool_calls` targeting registered MCP servers before returning the response to the client.
- [ ] **Dashboard Administration**
  - [ ] Web dashboard view to register and monitor active MCP servers and their tool inventory.

---

### 🌐 P3: Horizontal Scalability & Shared State

*Objective:* Enable deployment of multiple `openproxy` binary replicas behind an L7 load balancer.

- [ ] **Distributed State Backend (Optional / Pluggable)**
  - [ ] State abstraction with pluggable backend: `Memory` (local single-process) vs `Redis` / `Key-Value Store`.
  - [ ] Synchronization of atomic `round_robin` counters across instances.
- [ ] **Distributed Circuit Breaker and Cooldowns**
  - [ ] Publish/Subscribe (Pub/Sub) for account degradation events and health status.
  - [ ] Synchronization of upstream rate-limiting windows (429 `Retry-After`).
- [ ] **Distributed Client Rate Limiting**
  - [ ] Redis-backed token bucket / leaky bucket algorithm for per-API-key quotas across the cluster.

---

### 🧠 P4: Persistent Memory, Context Stores & Vector Cache

*Objective:* Manage conversational context and semantic storage in the proxy.

- [ ] **Conversation Storage (`session_id` / `thread_id`)**
  - [ ] Persistent chat history storage in SQLite / external database.
  - [ ] Automatic sliding context window and intelligent truncation of older messages.
- [ ] **Semantic Caching (Prompt Semantic Cache)**
  - [ ] Embedding storage and search for cached responses to identical or semantically similar queries.
  - [ ] Configurable cache invalidation via TTL and cosine similarity threshold.

---

### 🤖 P5: Stateful Support — Assistants API & Threads

*Objective:* Expand compatibility with the OpenAI Assistants API standard.

- [ ] **Assistants & Threads Endpoints**
  - [ ] `POST /v1/assistants`, `GET /v1/assistants/{id}`.
  - [ ] `POST /v1/threads`, `POST /v1/threads/{id}/messages`.
  - [ ] `POST /v1/threads/{id}/runs` with polling or streaming cycle.
- [ ] **Run Execution Engine**
  - [ ] State machine to manage the Run lifecycle (`queued` $\to$ `in_progress` $\to$ `requires_action` $\to$ `completed`).

---

### 🛡️ P6: Guardrails, Evals & Content Filtering

*Objective:* Proxy-layer security and continuous quality evaluation.

- [ ] **Guardrails & Safety Filters**
  - [ ] Detection of prompt injections and jailbreaks in incoming requests.
  - [ ] Configurable PII (Personally Identifiable Information) masking / redaction in prompts and outputs.
- [ ] **Production Evaluations and Benchmarking**
  - [ ] Shadow traffic / Shadow evaluation (silent parallel execution against a control model).
  - [ ] A/B Testing controlled by percentage weights in model combos.

---

### 🖥️ P7: Desktop Application & Native Packaging

*Objective:* Provide a standalone experience with a native interface for developers on local workstations.

- [ ] **Desktop Packaging**
  - [ ] Tauri integration to provide a lightweight cross-platform binary (Linux, macOS, Windows).
  - [ ] System Tray with start/stop control, quick logs, and health status.
- [ ] **System Installers and Packages**
  - [ ] Homebrew formula for macOS/Linux.
  - [ ] Debian (`.deb`) / Arch (`PKGBUILD`) / Windows (`winget`, `.msi`) packages.

---

## 3. Change Log

- **2026-08-15**: Initial post-MVP roadmap creation with prioritization of HTTP compression (`gzip`/`br`), MCP, and distributed scalability.
