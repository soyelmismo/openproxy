# Live-Logs Phase Robustness — Spec

## 1. Problem statement

The dashboard's Live Logs view shows per-row phase labels
(`started | connecting | waiting_ttft | streaming | completed | failed`) and a
live latency counter that ticks every 100 ms via `state/ticker.ts`. The
counter stops incrementing only when the row's latest stage is
`completed` or `failed`.

When a successful upstream request finishes (SSE `data: [DONE]`, `finish_reason:
stop`, non-streaming full body received), the **terminal stage event is never
published**. The frontend therefore keeps reading the last non-terminal stage
(`waiting_ttft` for non-streaming, `streaming` for streaming) and the counter
runs forever. This is the user-reported bug: "el reason stop del upstream no
se está tomando en cuenta".

Three root causes, all to be fixed:

- **R1** (primary): the success path of `dispatch_upstream` (non-streaming)
  and `dispatch_upstream_streaming` (streaming) in
  `crates/openproxy-core/src/pipeline.rs` calls `record_attempt_raw_with_tokens`
  without first publishing a terminal `StageEvent`. Compare with
  `record_and_fail` (pipeline.rs:2496) which DOES publish `failed`. Asymmetry.
- **R2**: the non-streaming success path emits `waiting_ttft` (pipeline.rs:1729)
  then jumps straight to the terminal via R1, skipping `streaming`. The
  dashboard's `STAGE_LABELS` distinguishes `waiting_ttft` (no body yet) from
  `streaming` (body started arriving) and the operator expects to see the
  transition.
- **R3** (defense-in-depth): `state/ticker.ts` has no upper bound. A missed
  terminal event from the backend makes the latency counter a runaway clock.
  The frontend MUST not lie indefinitely, even if the backend misses a beat.

## 2. Architecture

- The **backend** is the source of truth for phase transitions. Its contract:
  *for every request that produces a usage row, a terminal stage event
  (`completed` or `failed`) is published on `STAGE_SENDER` before
  `cost::record` returns*. The frontend is a *defense-in-depth* layer; the
  backend is not allowed to rely on the frontend to stop the ticker.
- The **frontend** MAY synthesize a terminal stage event from a finalized
  `recent_row` it received on the `row` envelope, but only as a fallback for
  the case where the stage event was lost in transit. It MUST NOT be the
  primary mechanism. The backend's terminal event is primary.
- DO NOT add new pipeline phases. The 6 existing labels are sufficient.
- DO NOT add new fields to `StageEvent`. The existing 11 fields
  (`request_id`, `trace_id`, `provider_id`, `upstream_model_id`, `stage`,
  `elapsed_ms`, `connect_ms`, `ttft_ms`, `status_code`, `error`, `timestamp`)
  cover the fix.
- DO NOT change the `record_and_fail` signature. The fix unifies emission in
  `record_attempt_raw_with_tokens` and removes the redundant emit from
  `record_and_fail`.

## 3. Backend changes (Rust)

### 3.1 Centralize terminal event emission in `record_attempt_raw_with_tokens`

File: `crates/openproxy-core/src/pipeline.rs`, function at line 2557.

Change: before calling `cost::record(&conn, &input)`, publish a terminal
`StageEvent`:

- If `err.is_none()`: stage = `"completed"`, error = `None`.
- If `err.is_some()`: stage = `"failed"`, error = `Some(err.to_string())`.

`status_code`, `connect_ms`, `ttft_ms`, `total_ms`, `model`, `target`, `req`,
`started` are already in scope; the `elapsed_ms` is `total_ms`; the timestamp
is the same `chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()`
the other emits use. The `trace_id` is the function argument.

Code sketch (matches the `let recording = self.is_recording()` style used
elsewhere in the file, with `recording` already bound at L2578):

```rust
let recording = self.is_recording();
if recording {
    let stage_label = if err.is_none() { "completed" } else { "failed" };
    let error_str = err.map(|e| e.to_string());
    crate::usage::publish_stage_event(crate::usage::StageEvent {
        request_id: req.request_id.to_string(),
        trace_id: trace_id.to_string(),
        provider_id: target.provider_id.to_string(),
        upstream_model_id: model
            .map(|m| m.model_id.as_str().to_string())
            .unwrap_or_default(),
        stage: stage_label.into(),
        elapsed_ms: total_ms,
        connect_ms,
        ttft_ms,
        status_code,
        error: error_str,
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
    });
}
```

### 3.2 Remove the redundant `failed` emit from `record_and_fail`

File: `crates/openproxy-core/src/pipeline.rs`, function at line 2474.

`record_and_fail` currently publishes a `failed` event (L2496) before calling
`record_attempt_raw_with_tokens`. With 3.1 in place, the centralized emit in
`record_attempt_raw_with_tokens` covers it. **Remove the block at L2495-L2511**
entirely, including its comment. The failure taxonomy is unchanged.

A test (see §5) MUST assert that exactly one `failed` event is published on
the channel for a failed attempt — guard against the dedup regression where
both the old and new emits fire.

### 3.3 Non-streaming success: emit `streaming` after the body is collected

File: `crates/openproxy-core/src/pipeline.rs`, non-streaming success branch.

After `response.collect().await` returns `Ok(body_bytes)` (around L1779) AND
`status_code` is in `200..300` (the block at L1784 only fires for non-2xx, so
this is the success path):

```rust
if self.is_recording() {
    let model_name = model.model_id.as_str().to_string();
    crate::usage::publish_stage_event(crate::usage::StageEvent {
        request_id: req.request_id.to_string(),
        trace_id: trace_id.to_string(),
        provider_id: target.provider_id.to_string(),
        upstream_model_id: model_name,
        stage: "streaming".into(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        connect_ms: Some(connect_and_send_ms),
        // `ttft_ms` here is the bare `u64` set at L1734
        // (`let ttft_ms = started.elapsed().as_millis() as u64;`).
        // It shadows the streaming path's `Option<u64> ttft_ms`
        // (L2094) — DO NOT rename the local; just wrap it in
        // `Some(...)` here.
        ttft_ms: Some(ttft_ms),
        status_code,
        error: None,
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
    });
}
```

The `ttft_ms` for non-streaming is conservatively set to
`started.elapsed().as_millis() as u64` at L1734 (a comment already exists
explaining this). The `streaming` event uses that same value. The
`cost::compute` step already turns the uninformative `ttft == total` into
`None` when computing tokens/sec, so this is safe. **Caveat for the
implementer**: the `elapsed_ms` and `ttft_ms` in this `streaming` event are
both ≈ `total_ms` because the body has already been read in full by the
single `response.collect().await` call. The dashboard's `ticker.ts:51`
will render `ttft Xms` correctly, but `live` jumps from ticking to
frozen at `total` on the next tick. This is acceptable: the
`completed` event from §3.1 arrives with the same `total_ms`, so the
operator sees a coherent `streaming → completed` pair.

### 3.4 Streaming success: no new event needed

`dispatch_upstream_streaming` already publishes `streaming` on the first SSE
data line (L2209) and the `cost::record` call site (L2363) now picks up
terminal emission via 3.1. No code change in the streaming body loop. The
`record_and_fail` removal in 3.2 covers the streaming cancel path.

### 3.5 Race window: stage event vs. row event on the WebSocket

The WebSocket handler `stream_usage_rows` (admin.rs:1121) subscribes to both
`usage_tx` (full rows) and `stage_tx` (transient stage events). After this
fix, the stage event is published *before* `cost::record` writes the row, so
the stage event reaches the broadcast channel first. The WebSocket handler
delivers them in order received; the frontend's `handleStageEvent` updates the
stage map and the row handler updates the rows list, both with their own
re-render trigger. The existing comment at L2490 ("ordering inside the
broadcast is not user-visible") remains accurate for the stage→row pair too.

## 4. Frontend defensive layer (TypeScript)

### 4.1 Freeze the ticker when a row is finalized

File: `crates/openproxy-web/src/static/src/state/ticker.ts`.

When a row's stage is `streaming` AND no new stage event has been received
for >2 s, the row is **stale-pending** and the counter MUST freeze at the
last received `elapsed_ms` plus the time since the last received event.
Implementation:

```ts
// inside tickLogLatency, before the existing skip-if-terminal check:
if (stage.stage === "streaming") {
  const t = Date.parse(stage.timestamp);
  if (isFinite(t)) {
    const stale = now - t;
    if (stale > 2_000) {
      // Monotonic freeze: past the 2s cap, `live` equals
      // `now - stage.timestamp` (the same formula the rest of
      // the function uses), NOT `stage.elapsed_ms + (stale - 2000)`.
      // The two are mathematically equivalent for the moment
      // past the cap, but the simpler formula matches the
      // existing convention and is what the next reader will
      // expect.
      live = stale;
    }
  }
}
```

`live` is what feeds `.log-latency` and the `Xms` sublabel. After the
freeze, `live` continues at the same wall-clock rate (`now - t`) — the
freeze is semantic, not numerical. The pill sublabel keeps its
`"Xms"` value but no longer advances because the `ticker.ts:51-53`
branch only updates the sublabel when the stage is non-terminal, and
once a `completed`/`failed` event arrives the early-skip at L43 fires.

**Note on the `--ticking` / `--stale` classes**: the spec does NOT
add a new `log-phase-sub--stale` CSS class. The existing
`log-phase-sub--ticking` class is removed on freeze (it implies
"live" in the operator's mental model).

Additionally, when the row is found in `state.logs.rows` (i.e. a
finalized `RecentUsageRow` was already received from the `row` envelope),
the ticker MUST freeze at `row.total_ms` regardless of the latest stage
event. Implementation:

```ts
// in tickLogLatency, after the stage lookup, before the skip-if-terminal check:
// BUILD ROW-LOOKUP INDEX OUTSIDE THE PER-ROW LOOP. The naive
// `state.logs.rows.find(...)` is O(n) per row per tick and at
// dashboard scale (50 rows × 10 Hz) the per-tick cost is
// O(n × m). Hoist the index once per tick.
const rowByTraceId = new Map<string, RecentUsageRow>();
const rowByRequestId = new Map<string, RecentUsageRow>();
for (const r of state.logs.rows) {
  if (r.trace_id) rowByTraceId.set(r.trace_id, r);
  if (r.request_id) rowByRequestId.set(r.request_id, r);
}
// inside the per-row loop:
const finalizedRow = traceId
  ? rowByTraceId.get(traceId)
  : rowByRequestId.get(requestId);
if (finalizedRow && finalizedRow.total_ms > 0) {
  live = finalizedRow.total_ms;
  // do not let the `--ticking` class stay on
  sub.classList.remove("log-phase-sub--ticking");
  // fall through to render frozen `total_ms`
}
```

### 4.2 Synthesize terminal stage from the row envelope (fallback only)

File: `crates/openproxy-web/src/static/src/views/logs.ts`, the `row` message
handler (around L493 where the synthetic event is currently built for
inflight placeholders).

When a `row` message arrives and the latest stage for that
`(request_id, trace_id)` is non-terminal, build a synthetic terminal stage
event from the row's `status_code`, `total_ms`, `connect_ms`, `ttft_ms`, and
`error_message`. Push it through `setStage` and trigger a re-render. This
is the fallback path: the backend's primary terminal event is still the
preferred one and the synthesis is for the rare case where the stage event
was lost in transit or lagged out of the broadcast channel.

### 4.3 Phase pill: render `total_ms` as the final sublabel

File: `crates/openproxy-web/src/static/src/components/log-row.ts`,
`renderLogRowHtml` (the `phase` parameter, around L18-25).

When the row has a finalized `recent_row` and the stage is
`completed`/`failed`, the sublabel MUST be the row's `total_ms` (e.g.
`"total 4231ms"`), not the live ticker. The function gets a new optional
`total_ms?: number | null` parameter; the views layer passes
`row.total_ms` when present.

When the stage is non-terminal but the row is already finalized (the
defense-in-depth case from 4.1), the sublabel reads `"Xms stale"` so the
operator sees the row is not actually live anymore.

## 5. Tests

### 5.1 Rust unit test — successful non-streaming emits `streaming` AND `completed`

File: the inline `mod tests` at the bottom of
`crates/openproxy-core/src/pipeline.rs` (around L2697+), alongside the
other `#[tokio::test]` functions for `Pipeline`. (External integration
tests in `crates/openproxy-core/tests/` exercise `Pipeline::run`
end-to-end; the new tests need to subscribe to `stage_broadcast()` and
count events, which is a unit-level concern for the centralized emit.)

Setup: a fake upstream that returns a 200 with a valid OpenAI JSON body.
Subscribe to `STAGE_SENDER.subscribe()` before invoking the pipeline, drain
the receiver, assert the events are in order:

1. `stage: "started"`, `status_code: 0`
2. `stage: "connecting"`, `status_code: 0`
3. `stage: "waiting_ttft"`, `status_code: 200`
4. `stage: "streaming"`, `status_code: 200`, `ttft_ms.is_some()`
5. `stage: "completed"`, `status_code: 200`, `error: None`

The `cost::record` side effect (DB row inserted) is asserted separately
through the existing harness.

### 5.2 Rust unit test — successful streaming emits `streaming` AND `completed`

Same shape as 5.1 but the fake upstream returns
`text/event-stream` with at least one `data: {...}` line ending in
`data: [DONE]`. The expected event sequence:

1. `started`
2. `connecting`
3. `waiting_ttft`
4. `streaming` (on the first data line, with a real `ttft_ms`)
5. `completed` (after the loop exits)

### 5.3 Rust unit test — failure emits exactly one `failed`

Drive the pipeline to failure (e.g. fake upstream returns 500). Subscribe
to `STAGE_SENDER`, count the events with `stage: "failed"`, assert it is
exactly 1. This guards against the dedup regression after 3.2.

### 5.4 e2e test — ticker freezes on stale `streaming` and on finalized row

File: `crates/openproxy-web/tests/e2e/phase-robustness.spec.ts`.

Pattern: copy the synthetic-event injection from
`live-logs-retry.spec.ts` (`window.__openproxyState.logs` hook is the
test seam).

Two scenarios:

- **Stale cap**: inject a `streaming` stage event with a
  `timestamp: 5 s ago`, wait 100 ms for the ticker to tick, assert the
  row's `.log-latency` text is no longer growing between two reads
  separated by 500 ms. (The frozen value is the `elapsed_ms` of the
  stale event plus 3 s of "stale" time — the test asserts it's a
  finite number close to that, not infinity.)
- **Row-finalized freeze**: inject a `streaming` stage event and a
  `row` envelope with a finalized `RecentUsageRow` for the same
  `(request_id, trace_id)`, wait 500 ms, assert the row's
  `.log-phase` text contains "completado" (the `completed` label
  per `crates/openproxy-web/src/static/src/lib/constants.ts:22`)
  and the `.log-phase-sub` reads the row's `total_ms`, not the live
  counter.

## 6. Acceptance criteria

- For every successful non-streaming request, `started → connecting →
  waiting_ttft → streaming → completed` events are published on
  `STAGE_SENDER` in that order, before the corresponding usage row is
  written to the DB.
- For every successful streaming request, `started → connecting →
  waiting_ttft → streaming → completed` events are published in that
  order.
- For every failed request, exactly one `failed` event is published.
- The frontend ticker never displays a continuously-growing latency for
  a request whose usage row is finalized in the DB. The defensive cap
  from §4.1 makes the counter freeze within 100 ms of either
  (a) a `row` envelope arriving, or (b) 2 s of silence after a
  `streaming` event.
- All existing e2e tests pass; new e2e test from §5.4 passes.

## 7. Out of scope

- No new pipeline phases. The 6 existing labels
  (`started | connecting | waiting_ttft | streaming | completed |
  failed`) are sufficient.
- No new fields on `StageEvent`. The existing 11 fields cover the fix.
- No new WebSocket envelope types. The `stage` and `row` envelopes are
  reused.
- No change to the failure taxonomy (`record_and_fail`'s signature).
- No front-end-only "synthesize terminal on row" as the primary fix;
  the synthesis is a fallback only.
- No `#[allow(...)]` to silence new warnings introduced by the fix.
  All warnings must be eliminated by removing or refactoring.
