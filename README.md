# openproxy ⚡

**A fast, self-hosted LLM gateway that unifies, races, and fallback-chains multiple AI providers behind a single OpenAI-compatible API.**

[![CI](https://github.com/soyelmismo/openproxy/actions/workflows/ci.yml/badge.svg)](https://github.com/soyelmismo/openproxy/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/soyelmismo/openproxy)](https://github.com/soyelmismo/openproxy/releases)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)
[![Docker](https://img.shields.io/badge/ghcr.io-openproxy-blue)](https://github.com/soyelmismo/openproxy/pkgs/container/openproxy)

```text
               ┌─────────────────────────────────────────────────────────┐
               │    Your AI Client (Cursor, Cline, Continue, Scripts)    │
               └────────────────────────────┬────────────────────────────┘
                                            │ OpenAI /chat/completions API
                                            ▼
               ┌─────────────────────────────────────────────────────────┐
               │                        openproxy                        │
               │  [ Router | Combos | Parallel Race | Telemetry | Auth ] │
               └───────┬────────────────────┬────────────────────┬───────┘
                       │                    │                    │
                       ▼                    ▼                    ▼
             ┌───────────────────┐┌───────────────────┐┌───────────────────┐
             │    OpenCode Zen   ││      Gemini       ││    OpenRouter     │
             │   (Free / Fast)   ││   (1M Context)    ││   (Fallback)      │
             └───────────────────┘└───────────────────┘└───────────────────┘
```

![openproxy Dashboard](docs/dashboard-hero.png)

---

## Why openproxy?

- **Parallel Racing:** Query multiple providers simultaneously — the first valid response streams back to your client instantly while losing requests are cancelled.
- **Combos (Intelligent Fallbacks):** Chain models and accounts together (`strict`, `round_robin`, `p2c`, `least_used`). If provider A hits a 429 rate limit or outage, openproxy automatically fails over to provider B with zero downtime.
- **Zero SaaS / 100% Self-Hosted:** Single binary with an embedded real-time web dashboard and SQLite database. No external databases, no cloud subscriptions, no tracking.
- **Universal Wire Translation:** Transparently translates requests and SSE streams between OpenAI (`/chat/completions`), Anthropic (`/messages`), and Google Gemini formats.
- **Built-in Proxy Engine & Cooldowns:** Built-in free proxies scraping, health testing, and persistent per-provider rate-limit cooldowns.
- **Instant Client Compatibility:** Point any OpenAI-compatible client (**Cursor, Cline, Roo Code, Continue, Claude Code, LiteLLM, LangChain**) to openproxy with a single base-URL change.

---

<!-- PLACEHOLDER: Real-Time Telemetry & Charts -->
<!-- Replace with telemetry screenshot, e.g. docs/images/live-charts.png -->
> ![openproxy Real-Time Charts & Live Logs](docs/images/live-charts.png)

---

## Key Features

- **⚡ Universal OpenAI-Compatible API** — Standard `POST /v1/chat/completions` (streaming SSE & non-streaming) and `GET /v1/models`.
- **🔌 Multi-Provider Support** — Built-in native adapters for **OpenRouter, MiniMax, OpenCode (Zen & Go), Ollama Cloud, Nous Research, NVIDIA NIM, Kilocode, Gemini (AI Studio + Cloud Code), Antigravity (+ CLI), Kiro, and Cloudflare Workers AI**, plus custom provider endpoints at runtime.
- **🏁 Parallel Races** — Launch $N$ targets concurrently; first token wins, losing requests abort cleanly within configurable grace periods.
- **🔄 Smart Combos & Load Balancing** — Group providers, models, and accounts into virtual models with weighted routing, power-of-two-choices (`p2c`), and nested sub-combos.
- **🛡️ Circuit Breakers & Cooldowns** — Automatic fault detection with configurable per-account and per-target backoff cooldowns.
- **🌐 Proxy Rotation & IP Throttling Protection** — Automatic proxy health tracking and isolated per-provider cooldowns upon 429 rate limits.
- **📊 Real-Time Embedded Dashboard** — High-performance web UI (TypeScript + Lit + uPlot) bundled directly into the binary at `/admin`. Live WebSocket activity feed, throughput metrics, latency percentiles ($p50/p95/p99$), and cost tracking.
- **🔔 Notifications Tray** — Real-time discovery alerts for newly available models, auto-activation rules, and drag-and-drop model-to-combo assignment.
- **🔐 Secret Encryption at Rest** — All upstream API keys and OAuth tokens are securely encrypted using **AES-256-GCM** with zero plaintext leakage.
- **🗜️ Payload Compression (Lite & RTK)** — Optional intelligent system-prompt deduplication and CLI tool output compaction (`git`, `cargo`, `npm`, `docker`) to reduce token usage and cost.

---

## Quick Start

### Option A: Docker (Recommended)

Run the official multi-arch image (`linux/amd64`, `linux/arm64`):

```bash
# Pull the latest image
docker pull ghcr.io/soyelmismo/openproxy:latest

# Run with a mounted config file and volume for SQLite data
docker run -d \
  --name openproxy \
  -p 8787:8787 \
  -v $(pwd)/config.toml:/etc/openproxy/config.toml:ro \
  -v openproxy-data:/var/lib/openproxy \
  --restart unless-stopped \
  ghcr.io/soyelmismo/openproxy:latest
```

Or using **Docker Compose**:

```bash
cp config.example.toml config.toml
docker compose up -d
```

---

### Option B: Pre-Built Binary

Download the executable for your architecture from [Releases](https://github.com/soyelmismo/openproxy/releases):

| Target | Platform |
| --- | --- |
| `x86_64-unknown-linux-gnu` | Linux (x86_64) |
| `aarch64-unknown-linux-gnu` | Linux (ARM64) |
| `x86_64-pc-windows-msvc` | Windows (x86_64) |
| `aarch64-apple-darwin` | macOS (Apple Silicon) |

```bash
# Extract and run
./openproxy --config config.toml
```

---

### Option C: Build from Source

Requirements: **Rust 1.80+**, **Node 20+**, and **pnpm**.

```bash
git clone https://github.com/soyelmismo/openproxy.git
cd openproxy

# 1. Build the embedded web dashboard
cd crates/openproxy-server/web && pnpm install && pnpm build && cd ../../..

# 2. Build the single binary
cargo build --release -p openproxy-server

# 3. Start openproxy
./target/release/openproxy --config config.toml
```

The web dashboard will be available at `http://127.0.0.1:8787/admin`.

---

## Usage

Point your AI tool (Cline, Cursor, Roo Code, Python, etc.) to openproxy:

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer YOUR_OPENPROXY_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello world!"}],
    "stream": true
  }'
```

### Compatible Integrations

| Tool | Base URL Configuration |
| :--- | :--- |
| **Cursor / Cline / Roo Code** | `http://127.0.0.1:8787/v1` |
| **Continue.dev** | `http://127.0.0.1:8787/v1` |
| **OpenAI Python SDK** | `client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="...")` |
| **LangChain / LlamaIndex** | `openai_api_base="http://127.0.0.1:8787/v1"` |

---

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — System architecture, routing pipeline, and internals.
- [`docs/mvp-spec.md`](docs/mvp-spec.md) — Specifications for endpoints, schema, and security models.

---

## License

openproxy is open-source software licensed under the [GNU General Public License v3.0](LICENSE).
