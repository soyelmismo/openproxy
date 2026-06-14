# openproxy

A self-hosted proxy that fronts multiple LLM providers behind a single OpenAI-compatible HTTP API, with combos, race execution, and per-request cost tracking.

It is **not** a hosted SaaS, a model trainer, or a UI for end users — the MVP is a binary plus an optional web scaffold.

See `docs/architecture.md` and `docs/mvp-spec.md` for the full spec.

## Build

```
cargo build --release
```

## Run

```
cargo run --release -p openproxy-server
```

## Acceptance criterion (Phase 0)

`cargo build --workspace` succeeds and produces a `release` binary for `openproxy-server` with no source files implementing business logic.
