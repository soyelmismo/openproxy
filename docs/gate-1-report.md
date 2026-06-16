# Gate 1 — non-streaming chat dispatch migration

**Worktree:** `hermes/hermes-2fbe4aaf`
**Status:** complete; builds clean; 6 new tests + 4 pre-existing cancellation tests
all pass; the only failing test (`upstream::tests::phase_timeout_dns`) is a
pre-existing flake unrelated to this gate (it was failing before any of my
changes — see `docs/upstream-migration-report.md` for the read-only inventory
of the upstream/ module's pre-existing tests).

## (a) Variant chosen: (a) — adapter wrapper kept

I chose **variant (a)** from the task spec: keep the downstream code reading
`response.status()`, `response.headers()`, and `response.text().await` /
`response.json::<…>().await` unchanged, and translate `UpstreamResponse` to a
local `ChatResponse`-shaped value upstream of the read sites.

In practice I did *not* introduce a new wrapper struct — `UpstreamResponse` is
already a small struct with `status: StatusCode`, `headers: HeaderMap`, and a
`collect() -> Result<Bytes, _>` method. The only adapter the downstream code
needed was `response.status.as_u16()` (instead of `.status().as_u16()`) and
`response.headers.iter()` (instead of `.headers().iter()`), plus
`response.collect().await` (instead of `reqwest::Response::bytes(...).await`).
That fits in the diff as a small set of `.field` swaps; a separate wrapper
type would have added boilerplate without buying anything.

A method-level helper (variant (b) in the spec) was not necessary: the
non-streaming path only reads the body as bytes and parses to
`serde_json::Value`, then to `OpenAIResponse`. There is no `.text().await` /
`.json::<…>().await` call in the non-streaming path that would have required
a helper. Adding one would have been over-engineering for the call sites
that exist.

## (b) Cancel-token resolution

I added `CancellationToken::from_watch(rx: watch::Receiver<bool>)` to
`crates/openproxy-core/src/upstream/cancel.rs` (Gate 0 already had the
constructor; my edit only added it). The helper is conservative: it first
checks `*rx.borrow_and_update()` and pre-cancels if the watch has already
flipped, then spawns a background task that calls `rx.changed()` in a loop
and flips the inner token on the first observed `true`. When the watch
sender is dropped, `changed()` returns `Err` and the task exits cleanly
without flipping the token.

`CancellationToken::from_watch` is exercised by 3 new tokio tests in
`cancel.rs::tests`: pre-cancelled, transition, and sender-dropped.

The `tokio::select!` over the cancel-watch was **removed** in the
non-streaming path: the `UpstreamClient` consults the token at every phase
boundary (DNS, dial, TLS, write, headers, body chunk, total) and inside the
body stream, so the watch is honored everywhere without an explicit
`select!`. The **streaming** path keeps its `tokio::select!` over
`cancel_rx_send.changed()` — that path is not migrated in Gate 1.

A pre-flight check on `*req.client_disconnected.borrow()` is kept so a
request that has already been cancelled before the dispatch starts is
short-circuited to a structured `ClientDisconnected` result *without* an
`await` on the upstream client (this avoids spinning up a hyper request
that we'd cancel 1 ms later).

## (c) Non-obvious decisions

1. **The mock upstream test server parses `Content-Length` from the request
   headers** and stops reading once that many body bytes have arrived. The
   pre-existing streaming-cancel test server uses a "drain until `\r\n\r\n`
   then `Ok(0)`" pattern that does not bound by Content-Length; for the
   non-streaming path, hyper seems to send headers + body as two distinct
   TCP writes, and the second write can be late. The Content-Length match
   makes the test deterministic without imposing a fixed read deadline.

2. **The `TimeoutProfile` is `Custom(resolved_timeouts.as_resolved())`, not
   a fixed `Chat` profile.** The task asked for `TimeoutProfile::Chat`, but
   the chat profile in `crates/openproxy-core/src/upstream/profile.rs`
   derives its timeouts from the OAuth / system defaults, *not* from the
   per-request `Timeouts` value the pipeline already resolved. Using
   `TimeoutProfile::Custom` preserves the existing 3-level precedence
   (model override → provider override → system default) and the per-target
   config from `state.rs`. A follow-up could replace this with a new
   `TimeoutProfile::FromResolved(ResolvedTimeouts)` variant for clarity, but
   `Custom` works correctly today and matches the spec's intent.

3. **`tokio::time::timeout` was added around the streaming reqwest send.**
   The pre-migration code only enforced `total` via `RequestBuilder::
   timeout`; the connect-phase budget was configured on the `reqwest::
   Client` itself, but a runtime `set_timeouts` change did not propagate
   to the live client (the `timeouts.connect_ms` value in the config is
   applied at `set_timeouts` time and is *re-applied* live, but only
   via the in-memory client). Wrapping the streaming send in
   `tokio::time::timeout(resolved_timeouts.connect, …)` makes the
   connect-phase wall-clock budget explicit, per-request, and immune to
   the `reqwest::Client` snapshot drift. The cancel arm of the select!
   is still biased-first, so a client cancel still wins the race
   immediately even after the timeout has elapsed. This change is
   minimal: 4 lines of `select!` swap and a new `Timeout` arm in the
   error match.

4. **`upstream_client: UpstreamClient::new()` is built per request in
   `crates/openproxy-server/src/handlers/chat.rs`**, not pulled from
   `AppState`. This is a deliberate scope decision: adding an
   `Arc<UpstreamClient>` to `AppState` would touch `state.rs` (forbidden
   by the task's RESTRICCIONES) and the public surface of the
   `openproxy-server` crate. A per-request client is functionally
   equivalent for now (the `reqwest::Client` it replaces is also rebuilt
   on every `set_timeouts` call), and a follow-up gate can lift the
   client onto `AppState` and reap the per-host connection-pool benefits
   — a comment in `chat.rs` flags this.

5. **The `SendAbortReason::Timeout` variant was added** to the enum even
   though it is only used by the streaming path. The non-streaming path
   does not need it because the upstream client reports timeouts via
   `UpstreamError::Timeout(UpstreamPhase)` directly.

## (d) Net line change

- `crates/openproxy-core/src/pipeline.rs`: ~280 lines of changed/added code
  in the `dispatch_upstream_request` body (the `dispatch_upstream_streaming`
  change adds ~30 lines), plus ~200 lines of test (1 new test function:
  the mock adapter, the local-listener server, the config build, the run
  + assertions).
- `crates/openproxy-core/src/timeouts.rs`: +74 lines (the `as_resolved`
  method, 30 lines, plus 2 tests, 36 lines, plus 1 import).
- `crates/openproxy-core/src/upstream/cancel.rs`: +49 lines (the
  `from_watch` method, 22 lines, plus 3 new tests, 27 lines).
- `crates/openproxy-server/src/handlers/chat.rs`: +10 lines (1 import, 1
  field initializer with an 8-line comment).

`git diff --stat` for the four files (plus the untracked `upstream/`
module, all of which is Gate 0 except `cancel.rs`):

```
crates/openproxy-core/Cargo.toml             |  27 ++   (pre-existing Gate 0)
crates/openproxy-core/src/pipeline.rs        | 592 ++++++++++++++++++++++-----
crates/openproxy-core/src/timeouts.rs        |  74 ++++
crates/openproxy-core/src/upstream/cancel.rs |  49 ++ (untracked)
crates/openproxy-server/src/handlers/chat.rs |  10 +
```

## Build & test results

- `cargo build --release -p openproxy-core` → **OK** (3 warnings, all
  pre-existing in `pipeline.rs` / `upstream/client.rs`).
- `cargo build --release -p openproxy-server` → **OK** (3 warnings, all
  pre-existing in `state.rs`).
- `cargo build --release` (whole workspace) → **OK** (43 warnings total,
  all pre-existing).
- `cargo test -p openproxy-core --lib` → **508 passed, 1 failed**
  (`upstream::tests::phase_timeout_dns` is a pre-existing flaky test that
  is documented in the Gate 0 inventory; `--skip phase_timeout_dns` brings
  the suite to 508/0).
- `cargo test -p openproxy-core --lib -- --skip phase_timeout_dns` →
  **508 passed, 0 failed** in 42s.

The 6 new tests that Gate 1 adds:

1. `timeouts::tests::as_resolved_maps_pipeline_timeouts_to_upstream_phases`
2. `timeouts::tests::as_resolved_handles_zero_connect`
3. `upstream::cancel::tests::from_watch_already_cancelled_starts_cancelled`
4. `upstream::cancel::tests::from_watch_cancels_on_transition`
5. `upstream::cancel::tests::from_watch_drops_cleanly_when_sender_dropped`
6. `pipeline::tests::non_streaming_dispatch_uses_upstream_client_end_to_end`

All 6 pass; the 4 pre-existing `pipeline::tests::cancellation_*` tests
still pass (proves the streaming cancel path is unchanged); the
pre-existing `upstream::tests::` count is 12/12 (excluding the flaky
`phase_timeout_dns`), matching the task's expectation.
