# openproxy — A self-hosted OpenAI-compatible proxy that races multiple LLM providers behind one API.

![CI](https://github.com/soyelmismo/openproxy/actions/workflows/ci.yml/badge.svg)
[![Release](https://img.shields.io/github/v/release/soyelmismo/openproxy)](https://github.com/soyelmismo/openproxy/releases)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)
[![Docker](https://img.shields.io/badge/ghcr.io-openproxy-blue)](https://github.com/soyelmismo/openproxy/pkgs/container/openproxy)

## What is openproxy?

openproxy is a self-hosted proxy you run on your own machine or server that
exposes a single OpenAI-compatible HTTP API. Behind that API you can plug in
multiple upstream LLM providers — OpenRouter, MiniMax, OpenCode Zen, Gemini,
Kiro, Antigravity, and more — and route requests to them with strategies like
ordered fallback chains ("combos") or parallel races (multiple providers in
flight, first valid response wins, losers aborted).

It is a **binary**, not a hosted SaaS. You run it; you own the data; you keep
the API keys. There is no signup, no metering from us, no model training. The
server boots in milliseconds against a local SQLite file, exposes
`POST /v1/chat/completions` and `GET /v1/models`, and a small admin surface for
configuring providers, accounts, combos, API keys, and reading back usage /
cost telemetry.

It is **also not a model trainer**. It speaks the OpenAI Chat Completions wire
format on the front, translates to whichever upstream wire format a provider
needs (OpenAI `/chat/completions`, Anthropic `/messages`, Google's Gemini
`contents`/`generationConfig`), and streams SSE responses back transparently.
Any client that already speaks to `api.openai.com` can be repointed at
openproxy with one base-URL change.

## Key features

- **OpenAI-compatible chat completions API** — `POST /v1/chat/completions` with
  streaming (SSE) and non-streaming responses, plus `GET /v1/models` for
  discovery. Existing OpenAI clients work with a base-URL change.
- **Multi-provider support** — Built-in adapters for **OpenRouter, MiniMax,
  OpenCode Zen, Ollama Cloud, Nous Research, NVIDIA NIM, Kilocode, Gemini (AI
  Studio + Cloud Code), Antigravity (+ CLI), Kiro, and Cloudflare Workers
  AI**, across four wire formats: OpenAI, Anthropic, Mixed (per-model), and
  Gemini. Custom providers can be added at runtime.
- **Combos (ordered fallback chains)** — Group `(provider, model, account)`
  targets into a single named combo; on failure the next target is tried.
  Strategies: `priority`, `round_robin`, `shuffle`. Priority mode adds
  `strict`, `lkgp` (least-known-good-provider), `weighted`, `least_used`, and
  `p2c` (power-of-two-choices). Combos can reference other combos as
  sub-combos.
- **Race execution** — Launch N targets in parallel; the first valid response
  wins and the losers are signalled to abort within a configurable grace
  window (`abort_grace_ms`). All attempts — winners and losers — are still
  recorded for telemetry.
- **Per-request cost tracking** — Every attempt is logged with prompt /
  completion token counts, computed USD cost from a pricing table (syncable
  from models.dev), time-to-first-token, total latency, stop reason, and race
  outcome.
- **Per-API-key quota and usage limits** — Issue `op_live_…` API keys with
  `chat` and/or `manage` scopes, allowed-model / allowed-combo whitelists, and
  expiry. Usage is tracked per key.
- **Live model discovery** — Models are fetched from upstream at runtime
  (OpenRouter `/models`, Anthropic `/v1/models`, Gemini `listModels`, etc.),
  with a models.dev sync for pricing and metadata. Adding a new upstream model
  does not require a recompile.
- **Circuit breaker + cooldown per account** — In-memory per-account circuit
  breaker trips after `failure_threshold` consecutive failures and stays open
  for `unhealthy_duration_ms`. Per-combo-target cooldowns can also be set and
  cleared from the admin API.
- **Streaming (SSE) with passthrough and translation** — Byte-passthrough SSE
  from upstream to client, with on-the-fly translation between OpenAI,
  Anthropic, and Gemini streaming chunk shapes.
- **OAuth support for OAuth-only providers** — Device Code grant (RFC 8628)
  for Kiro, Authorization Code + PKCE for Antigravity / Antigravity CLI, and
  Authorization Code with embedded client secret for Gemini CLI. Token
  refresh is handled by a background scheduler; tokens are encrypted at rest.
- **Optional compression modes** — `lite` (whitespace collapse, system-prompt
  dedup, tool-result truncation, image stripping — zero semantic change) and
  `rtk` (command-aware filtering of CLI tool output: git, cargo, npm, docker,
  etc.) can be applied to the request payload before forwarding upstream.
  Modes compose (`lite_rtk`).
- **Admin dashboard** — A separate `openproxy-web` binary serves a web UI for
  managing providers, accounts, API keys, combos, viewing live logs and
  cost analytics. See [Dashboard](#dashboard) for the current caveat.
- **SQLite storage with bundled migrations** — All state (providers, accounts,
  combos, models, usage rows, OAuth tickets) lives in a single SQLite file.
  The `rusqlite` `bundled` feature compiles SQLite from source, so there is
  no external database or system dependency. 35 migrations are applied on
  startup.
- **Secret encryption at rest** — Upstream account API keys and OAuth tokens
  are sealed with AES-256-GCM using a master key loaded from
  `OPENPROXY_MASTER_KEY` (with `OPENPROXY_MASTER_KEY_PREVIOUS` for rotation).
  openproxy's own API keys are hashed with SHA-256 (high-entropy tokens, not
  passwords — Argon2 would be theatre here).

## Quick start

Three install paths. Pick whichever fits your environment.

### Option A: Docker (recommended)

The multi-arch image (`linux/amd64`, `linux/arm64`) is published to GHCR on
every push to `master`.

```bash
# Pull the latest image
docker pull ghcr.io/soyelmismo/openproxy:latest

# Run with a mounted config file and a named volume for the SQLite DB
docker run -d \
  --name openproxy \
  -p 8787:8787 \
  -v $(pwd)/config.toml:/etc/openproxy/config.toml:ro \
  -v openproxy-data:/var/lib/openproxy \
  --restart unless-stopped \
  ghcr.io/soyelmismo/openproxy:latest
```

Or use the included [`docker-compose.yml`](docker-compose.yml):

```bash
cp config.example.toml config.toml
# Edit config.toml — set [server].bind = "0.0.0.0:8787" for host access
docker compose up -d
```

The container runs as a non-root `openproxy` user, uses `tini` as PID 1, and
exposes a healthcheck against `GET /v1/health`.

### Option B: Pre-built binary

Download the latest zip for your platform from
[Releases](https://github.com/soyelmismo/openproxy/releases). Available
targets:

| Target | Platform |
| --- | --- |
| `x86_64-unknown-linux-gnu` | x86_64 Linux |
| `aarch64-unknown-linux-gnu` | ARM64 Linux |
| `armv7-unknown-linux-gnueabihf` | ARMv7 (arm32) Linux |
| `x86_64-pc-windows-msvc` | x86_64 Windows |
| `aarch64-pc-windows-msvc` | ARM64 Windows (experimental) |
| `x86_64-apple-darwin` | Intel macOS |
| `aarch64-apple-darwin` | Apple Silicon macOS |

Each zip contains the `openproxy` (or `openproxy.exe`) binary plus
`config.example.toml`. Unzip, copy `config.example.toml` to `config.toml`,
edit it, and run:

```bash
./openproxy --config config.toml
```

> The binary respects the `OPENPROXY_CONFIG` env var as an alternative to
> `--config`. If neither is set, it looks for `./config.toml`.

### Option C: Build from source

You need Rust 1.85+ (edition 2024) and, only if you want the dashboard
binary, Node 22 + pnpm 9.

```bash
git clone https://github.com/soyelmismo/openproxy.git
cd openproxy

# Build the API server binary (no frontend toolchain needed)
cargo build --release -p openproxy-server
# The binary is at target/release/openproxy
./target/release/openproxy --config config.toml

# Optional: build the dashboard binary (needs pnpm + Node 22)
(cd crates/openproxy-web && pnpm install && pnpm build)
cargo build --release -p openproxy-web
```

The release profile uses LTO, `opt-level = 3`, single codegen unit, and
stripped symbols — expect a ~30–90 s clean build but a small, fast binary.

## Configuration

openproxy reads a TOML config file on startup. Copy
[`config.example.toml`](config.example.toml) to `config.toml` and edit it —
the example file documents every field. The minimal sections are:

```toml
[server]
bind = "127.0.0.1:8787"            # Use 0.0.0.0:8787 for Docker / host access
request_max_body_bytes = 10485760

[storage]
database_path = "~/.openproxy/data.db"
encryption_key_source = "env"       # env | file

[racing]
default_race_size = 1
max_race_size = 8
abort_grace_ms = 500

[timeouts]
connect_ms = 5000
request_send_ms = 10000
ttft_ms = 30000
idle_chunk_ms = 120000
total_ms = 300000

[retries]
max_attempts = 3
backoff_base_ms = 200
backoff_factor = 2
backoff_jitter_pct = 50

[circuit_breaker]
failure_threshold = 5
unhealthy_duration_ms = 60000

[logging]
format = "json"                     # json | text
level = "info"

[compression]
mode = "off"                        # off | lite | rtk | lite_rtk
```

The encryption master key is loaded from the `OPENPROXY_MASTER_KEY`
environment variable (base64 of 32 raw bytes). Generate one with:

```bash
openssl rand -base64 32
```

**Providers, accounts, models, combos, and API keys are NOT configured in the
config file.** They live in the SQLite database and are managed at runtime
through the admin API (`/admin/*` endpoints) or the dashboard. The config
file only covers server-level knobs (bind address, storage path, timeouts,
retries, circuit breaker, logging, compression).

## Dashboard

The `openproxy` binary serves **only** the OpenAI-compatible API and the
admin CRUD surface. There is no UI in that binary.

A separate optional binary, `openproxy-web`, provides a web UI for managing
providers, accounts, API keys, combos, viewing live logs, and cost
analytics. It is a thin proxy that forwards `/web/api/*` calls to a running
`openproxy` server.

**Known limitation.** `openproxy-web` currently reads its static assets (JS,
CSS, fonts) from the source tree at runtime via `CARGO_MANIFEST_DIR`, with
only `index.html` and `callback.html` embedded at compile time. This means
the dashboard binary works best when run from a built source checkout, not
as a standalone downloaded binary. The CI release pipeline does **not**
currently ship a standalone `openproxy-web` binary for this reason.

To use the dashboard today:

```bash
# Terminal 1: run the API server
./target/release/openproxy --config config.toml

# Terminal 2: run the dashboard (needs a built frontend)
(cd crates/openproxy-web && pnpm install && pnpm build)
cargo run -p openproxy-web -- --core-url http://127.0.0.1:8787
# Open http://127.0.0.1:8080 (or whatever port openproxy-web binds to)
```

**Future work.** Bundle the dashboard assets into `openproxy-web` properly
(e.g. via `include_dir!` or a build-time `dist/` embedding step) so it ships
as a single self-contained binary. Tracked informally — contributions
welcome.

## API usage

Once the server is running, point any OpenAI-compatible client at
`http://127.0.0.1:8787`. A minimal `curl`:

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer YOUR_OPENPROXY_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": false
  }'
```

For streaming, set `"stream": true` and the server will emit SSE
`data: …` chunks in the OpenAI shape, regardless of which upstream provider
actually served the request.

The `model` field accepts any model id known to openproxy — either a model
discovered from an upstream provider, a custom model you registered via the
admin API, or a combo id (which triggers the combo / race routing logic).

List known models with:

```bash
curl http://127.0.0.1:8787/v1/models \
  -H "Authorization: Bearer YOUR_OPENPROXY_KEY"
```

Liveness probe (no auth required):

```bash
curl http://127.0.0.1:8787/v1/health
# {"status": "ok", "version": "1.0.0"}
```

## Project layout

openproxy is a Rust workspace of four crates:

```
openproxy/
├── crates/
│   ├── openproxy-core/         # Headless engine: providers, combos, race,
│   │                           # SSE, OAuth, secrets, DB, cost, quotas.
│   │                           # No HTTP server, no UI.
│   ├── openproxy-server/       # Binary `openproxy`. axum HTTP server that
│   │                           # exposes /v1/* (chat, models, health) and
│   │                           # /admin/* (CRUD + telemetry).
│   ├── openproxy-api-client/   # Rust client library for the /admin/* API.
│   │                           # Used by openproxy-web and external scripts.
│   └── openproxy-web/          # Binary `openproxy-web`. Optional dashboard
│                               # UI; proxies /web/api/* to a running
│                               # openproxy server. Frontend is TypeScript
│                               # built with pnpm + esbuild.
├── docs/                       # architecture.md, mvp-spec.md, pending/
├── config.example.toml         # Annotated config starting point
├── Dockerfile                  # Multi-stage build for the API server
└── docker-compose.yml          # Single-service compose for the API server
```

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — Vision, principles, ASCII
  architecture diagram, request lifecycle, and per-module walkthroughs.
- [`docs/mvp-spec.md`](docs/mvp-spec.md) — The full MVP specification:
  endpoints, schema, routing rules, cost model, OAuth flows.
- [`docs/pending/`](docs/pending/) — Open audit findings and follow-up
  tickets. If you want to contribute, this is a good place to look for
  well-scoped work.

## Contributing

Pull requests are welcome against `master`. Please use
[conventional commits](https://www.conventionalcommits.org/) —
`feat:`, `fix:`, `docs:`, `chore:`, etc. — for your commit messages. The CI
pipeline parses commit history since the last `v*` tag to compute the next
release version (`feat` → minor, `fix` / `docs` / `chore` → patch,
`feat!:` or `BREAKING CHANGE` → major) and generates the release notes.

See [`.github/workflows/ci.yml`](.github/workflows/ci.yml) for the full
release process: frontend build → Rust checks → version computation →
7-target matrix build → Docker image → GitHub Release.

For the dashboard frontend, run `pnpm install` then `pnpm typecheck` and
`pnpm build` inside `crates/openproxy-web/` before submitting changes that
touch the UI.

## License

openproxy is licensed under the
[GNU General Public License v3.0](LICENSE).
