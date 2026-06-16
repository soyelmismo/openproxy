# Gate 2 — streaming chat dispatch migration

**Worktree:** `hermes/hermes-2fbe4aaf`
**Status:** complete; builds clean; 1 new test + the 4 pre-existing
cancellation tests (including `cancellation_mid_sse_stream_aborts_immediately`,
which is the most rigorous mid-stream cancel test) all pass; the only
failing test (`upstream::tests::phase_timeout_dns`) is a pre-existing
flake unrelated to this gate (documented in Gate 0's inventory).

## (a) SSE loop with `UpstreamBodyStream::next_chunk`

`UpstreamBodyStream` does not implement `futures::Stream`, so the
old `stream.next()` (from `futures::StreamExt`) doesn't compile.
The new loop uses `next_chunk_boxed()` (the boxed form gives a
`Pin<Box<dyn Future + Send>>` that's well-typed inside a
`tokio::select!` arm):

```rust
loop {
    let mut cancel_rx_chunk = req.client_disconnected.clone();
    let chunk_result: Option<UpstreamResult<Option<Bytes>>> =
        tokio::select! {
            biased;
            _ = cancel_rx_chunk.changed() => None,
            next = stream.next_chunk_boxed() => Some(next),
        };
    let chunk_result = match chunk_result {
        Some(r) => r,
        None => break,
    };
    let bytes = match chunk_result {
        Ok(Some(b)) => b,
        Ok(None) => break,
        Err(e) => { /* map to CoreError */ }
    };
    buffer.push_str(&String::from_utf8_lossy(&bytes));
    /* ... SSE line splitter unchanged ... */
}
```

The shape is **simpler** than the pre-migration `while let
Some(_) = { select!{ ... stream.next() ... } }` because the
per-chunk gap deadline (`idle_chunk_ms`) is no longer enforced at
the call site — `UpstreamBodyStream::next_chunk` already races the
chunk-arrival future against the per-chunk and total deadlines
internally, so the loop has no `tokio::time::timeout` arm of its
own. The per-chunk `Err(UpstreamError::Timeout(Body))` is mapped
back to `CoreError::UpstreamTimeout { phase: "idle_chunk", .. }` so
the dashboards see the same label they did before.

## (b) Cancel-token resolution

The `client_disconnected` watch is **still consulted** in two places
in the streaming path:

1. **Pre-flight check** before `upstream_client.call` — a
   pre-existing flipped watch short-circuits to a structured
   `ClientDisconnected` result without spinning up a hyper request.
2. **In the body loop's `tokio::select!`** — the cancel arm
   produces `None`, which breaks the loop. The hyper body future
   is dropped (cancelling the underlying body read), and the
   post-loop `is_client_disconnected` checkpoint then emits the
   structured `ClientDisconnected` usage row.

But the heavy lifting is now done by the `CancellationToken`
mirrored from the watch (`CancellationToken::from_watch`) and
handed to `upstream_client.call`. The token is consulted at every
phase boundary (DNS, dial, TLS, write, headers) and inside
`UpstreamBodyStream::next_chunk` between frames — so even if the
explicit watch arm of the body-loop select! never fires, an
in-flight chunk read is interrupted by the token, and the
`Err(UpstreamError::Cancel)` it returns also breaks the loop.

The `cancellation_mid_sse_stream_aborts_immediately` test
(still green) covers the worst case: the upstream is mid-SSE, the
client cancels, and the pipeline must abort within 3s — not wait
for `total_ms` (30s default). That test passes against the new
path.

## (c) Non-obvious decisions

1. **`SendAbortReason` enum removed.** It was a 9-line enum
   designed for the reqwest-specific `tokio::select!` over
   `request_builder.send()`. The new path uses `UpstreamError`,
   which is more granular (per-phase timeouts, distinct `Cancel`,
   separate `Connection` / `Tls` / `Http` / `Decode` / `Invalid`).
   The deletion removes ~10 lines.

2. **The pre-existing `tokio::time::timeout(connect, send)`
   wrapper is gone.** The `UpstreamClient::call` honors the
   `connect` budget via its `headers_deadline` (the closest phase
   boundary to the pre-migration `connect` wall-clock budget,
   which used to cover dial + TLS + wait-for-headers). The
   `request_builder` chain it replaced was wrapped in a
   defensive `tokio::time::timeout` by the Gate 1 builder — that
   is no longer needed because the upstream client enforces the
   per-phase deadline from the `TimeoutProfile::Custom`.

3. **`response.text().await` replaced with `body.collect_all()` +
   `String::from_utf8_lossy`.** `UpstreamResponse` has no `.text()`
   method, so the non-2xx error path now reads the body to a
   `Bytes` and converts. This costs one extra allocation per
   non-2xx response but is the only way to surface the error
   body in `CoreError::UpstreamError.body`.

4. **`use futures::StreamExt` deleted from the streaming path.**
   That was the only use of `futures::StreamExt` in `pipeline.rs`;
   the `tokio` and `bytes` imports the body loop relied on were
   already in scope.

5. **The new `streaming_dispatch_uses_upstream_client_end_to_end`
   test.** Binds a localhost listener, mocks an OpenAI upstream
   that emits 3 SSE chunks then `[DONE]` and closes the socket,
   runs a streaming pipeline, drains the `stream_sink`, and
   asserts (a) at least one chunk was forwarded, (b) `[DONE]`
   appears, (c) every non-`[DONE]` item is a valid OpenAI chunk
   JSON, (d) the concatenated `delta.content` spells
   `"hi there!"` — proving every chunk was forwarded and
   translated, not just the first. Passes in 2.1s.

## (d) Net line change (Gate 2-specific)

The Gate 2 deltas (excluding the Gate 1 cumulative diff):

- `dispatch_upstream_streaming`: 450 lines → 493 lines (~+43 net)
- `SendAbortReason` enum + its 9-line doc comment: deleted
- `request_builder` chain in `dispatch_upstream_request`: 9 lines
  deleted (replaced by the existing `UpstreamRequest::post_json`
  from Gate 1)
- `use futures::StreamExt` import: deleted
- New `streaming_dispatch_uses_upstream_client_end_to_end` test:
  +318 lines

Net for `pipeline.rs`: **~+360 lines** (the migration is a net
addition because of the new test and the more verbose
`UpstreamError` → `CoreError` mapping; the streaming helper
itself is roughly the same size).

`git diff --stat` for the modified file:
```
crates/openproxy-core/src/pipeline.rs | 1107 ++++++++++++++++++++++++++++-----
1 file changed, 955 insertions(+), 152 deletions(-)
```
(the +955 / -152 figure includes the Gate 1 cumulative diff
uncommitted in the worktree; Gate 2 alone is the streaming
path migration + the new test.)

## Build & test results

- `cargo build --release -p openproxy-core` → **OK** (no new
  warnings in `pipeline.rs`; the 4 pre-existing warnings in the
  file are at lines 383, 1550, 2420, 2553 — none in regions I
  modified).
- `cargo build --release -p openproxy-server` → **OK** (3
  pre-existing warnings, all in `state.rs`).
- `cargo build --release` (whole workspace) → **OK**.
- `cargo test -p openproxy-core --lib` → **509 passed, 1 failed**
  (`upstream::tests::phase_timeout_dns` is the pre-existing
  flake).
- `cargo test -p openproxy-core --lib -- --skip phase_timeout_dns`
  → **509 passed, 0 failed** in 43s.

The 1 new test that Gate 2 adds:
1. `pipeline::tests::streaming_dispatch_uses_upstream_client_end_to_end`

The 4 pre-existing `pipeline::tests::cancellation_*` tests
all still pass — including the critical
`cancellation_mid_sse_stream_aborts_immediately` which proves
the mid-stream cancel path works end-to-end against the
hyper-based client.
