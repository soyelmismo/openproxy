# Phase Robustness — Live-Logs Latency Ticker Stuck

**Status**: Diagnosed, not yet fixed.
**Date filed**: 2026-06-18
**Diagnostic source**: `BUG_DIAGNOSIS_latency_ticker_stuck.md` (24 KB, full path-by-path analysis with line numbers).

---

## 1. Bug report (user)

> Cuando una petición ya terminó en el cliente, el sistema aún dice
> "esperando ttft" y el contador de latency sigue summing
> indefinidamente. El `stop_reason` del upstream no se está tomando
> en cuenta.

## 2. TL;DR

The bug is **larger** than the user thought. There are at least
**8 distinct "lost terminal event" paths**. The `isFinished`
formula in `logs.ts:589-593` is a textbook OR-of-redundant-terms
bug: for `is_streaming=true, stream_complete=false, status_code>0`
(the common case for non-OpenAI upstreams that close without
`[DONE]`) all three terms are `false`, so the dashboard never
synthesizes a terminal stage event.

`stop_reason` is correctly identified as never persisted: it's
extracted in `sse.rs:305-313` and `translation.rs:152`, mapped to
OpenAI `finish_reason`, then **discarded** — not stored in
`UpstreamSseChunk`, not in `StageEvent`, not in `RecentUsageRow`,
not in `UsageInput`, no DB column.

The R3 row-finalized freeze in `ticker.ts:72-73` does freeze the
**counter** when `row.total_ms > 0`, but only the counter — the
top label "esperando ttft" stays stuck because
`renderLogPhaseHtml` (log-row.ts) only switches to `"total Xms"`
when the stage itself is `completed`/`failed`.

## 3. Confirmed root causes

| # | Cause | Site | Why |
|---|-------|------|-----|
| C1 | Terminal stage event gated by `if recording` | `pipeline.rs:3205` | With `recording=OFF`, the row reaches the DB but the `stage: "completed"` event never reaches the WS. |
| C2 | `isFinished` formula doesn't detect `is_streaming=true, stream_complete=false, status_code>0` | `logs.ts:589-593` | All three OR terms are `false` for the streaming-without-`[DONE]` case. |
| C3 | `stream_tokens` WS message has no publisher | `logs.ts:458,639` | `grep` confirms zero backend publishers. Handler is dead code. |
| C4 | `stop_reason` is extracted but never persisted | `sse.rs:305-313`, `translation.rs:152` | No field in `UpstreamSseChunk`, `StageEvent`, `RecentUsageRow`, `UsageInput`, no DB column. |
| C5 | `recording=OFF` silences **all** stage events, not just terminal | `pipeline.rs:1023,1483,1822,2019,2626,3205` | With `recording=OFF`, `stagesByTraceId` is empty for the entire request. The ticker at `ticker.ts:47-55` does `if (!stage) continue;` and the row is never touched. |
| C6 | `try_lock_for` timeout returns `Ok(())` without row or stage event | `pipeline.rs:3184-3199` | If writer lock is held by a slow admin query, the row is dropped and the terminal event is never published. |
| C7 | `resyncUsageRows` bypasses the WS `row` handler's synthetic-terminal logic | `logs.ts:436-456` | After a WS resync, the synthetic-terminal-event path is never invoked. The inflight placeholder is also never cleared. |
| C8 | `history` rows have the same `isFinished` blind spot | `logs.ts:519-521` | A page refresh on a stuck-in-`streaming` row doesn't fix it. |

## 4. User-visible symptoms

| Symptom | Triggered by | Underlying cause |
|---------|-------------|------------------|
| "Esperando ttft" / "recibiendo streaming" label stuck after upstream finished | any non-OpenAI streaming upstream that closes without `[DONE]` (most common) | C1 + C2 |
| Live latency counter keeps growing past the actual request duration | long-stuck `streaming` event (consequence of C1, C2) | C8 (ticker.ts:84-87) — the "stale" cap is wall-clock, not actually frozen |
| No phase label updates at all | `recording=OFF` | C5 |
| Row arrives, label stays on last non-terminal | WS lag → `lag_warning` + `resync` | C7 |
| Cannot distinguish `end_turn` / `max_tokens` / `stop_sequence` | always | C4 |

## 5. Fix plan (prioritized)

The full report (`BUG_DIAGNOSIS_latency_ticker_stuck.md`) lists
F1-F8. Recommended order is **F1+F2+F4** in this session (closes
4 of the 8 paths, no DB migration), then **F3** as a follow-up
(adds a `usage.stop_reason` column + thread it through the wire
contract).

### Phase 1 (this session): F1 + F2 + F4

- **F1** (BLOCKER): drop the `if recording` gate around the
  terminal stage event publish in `pipeline.rs:3205`. The heavy
  body/header columns in the DB row stay gated by `recording` —
  only the **stage event** fires unconditionally. This is a
  3-line change.

- **F2** (BLOCKER, paired with F1): replace the `isFinished`
  formula in `logs.ts:589-593` with a single condition:
  `row.status_code > 0 || row.error_message != null`. Apply the
  same simplification to the `history` branch at `logs.ts:519-521`.

- **F4** (HIGH): make `resyncUsageRows` run the synthetic-terminal
  logic that the WS `row` branch uses. Extract the logic into a
  shared helper and call it from both paths. While we're there,
  clear the inflight placeholder for the affected `trace_id`s.

### Phase 2 (next session): F3

Thread `stop_reason` end-to-end:

- `sse.rs:11-19` — add `pub stop_reason: Option<String>` to
  `UpstreamSseChunk`.
- `pipeline.rs:2434-2873` — capture the `stop_reason` from the
  `message_delta` chunk in the streaming loop, carry it to
  `record_attempt_raw_with_tokens`.
- `pipeline.rs:3106-3129` — add a `stop_reason: Option<String>`
  parameter to `record_attempt_raw_with_tokens`.
- `usage.rs:128-167` — add `pub stop_reason: Option<String>` to
  `StageEvent`.
- `usage.rs:746-770` — add `pub stop_reason: Option<String>` to
  `RecentUsageRow`.
- `cost.rs:13-46` — add to `UsageInput`.
- `cost.rs:107-180` — extend the INSERT with a new
  `usage.stop_reason` column (migration + new index for the
  `errors-by-reason` analytics view).
- `lib/types/api.ts:292-308` and `:355-378` — add the field to
  the TS types.

This also fixes a parallel concern: the dashboard currently can't
distinguish `end_turn` from `max_tokens` from `stop_sequence` —
operators have no way to tell whether a slow model response hit
the token limit or ran out of tokens.

### Phase 3 (cleanup, optional): F5 + F6 + F7 + F8

- **F5**: fix the "stale" cap in `ticker.ts:84-87` — currently a
  wall-clock hack that keeps growing; with F1+F2 in place it can
  be deleted or simplified.
- **F6**: make `stream_tokens` real (publish from the streaming
  loop) or remove the dead handler.
- **F7**: clear the writer-lock-timeout path — publish the
  terminal stage event **before** the lock attempt so the
  dashboard still freezes if the row insert is dropped.
- **F8**: fix the `setStage` clobber hazard — guard against a
  late non-terminal event overwriting a terminal one in the map.

## 6. Test coverage expected

For Phase 1, add a test in `crates/openproxy-core/src/pipeline.rs`
(same `run_with_fake_upstream_and_capture_stages` pattern that
already exists for `phase_robustness_*`):

```rust
#[test]
async fn phase_robustness_streaming_without_done_marker_emits_terminal() {
    // Upstream closes the connection without sending [DONE].
    // Verify: row arrives + terminal stage event "completed" reaches
    // the WS subscriber, even with `is_recording() == false`.
}

#[test]
async fn phase_robustness_terminal_event_fires_when_recording_off() {
    // Set recording=OFF. Run a 2xx non-streaming request. Verify:
    // row reaches the DB, terminal stage event reaches the WS
    // subscriber, even though body/header columns are redacted.
}
```

For the frontend, add a TS unit test (if the project has a
`tsconfig.test.json` — verify) that exercises the new
`isFinished` formula against the matrix of
`{is_streaming, stream_complete, status_code}` values.

## 7. Files expected to be touched (Phase 1)

- `crates/openproxy-core/src/pipeline.rs` (F1: drop recording
  gate; F4: extract helper) — small.
- `crates/openproxy-web/src/static/src/views/logs.ts` (F2: fix
  formula; F4: extract helper, call from resync) — small.
- `crates/openproxy-web/src/static/src/state/ticker.ts` (F5
  cleanup) — optional in this phase.
- `crates/openproxy-core/src/pipeline.rs` (new test) — small.

## 8. Reference

- Full diagnosis: `BUG_DIAGNOSIS_latency_ticker_stuck.md` (in
  the same `.hermes/pending/` directory).
- Old spec (already implemented and shipped in master as commit
  `a02b238` and earlier): the previous
  `.hermes/phase-robustness-spec.md` described R1/R2/R3 as
  "publish the terminal event from `record_attempt_raw_with_tokens`
  + emit `streaming` after non-streaming 2xx + ticker upper bound".
  Those R1/R2/R3 fixes were the work of an earlier session and are
  already in master. The current F1+F2+F4 in this spec addresses
  the *remaining* issues that the original R1/R2/R3 spec did not
  cover (specifically: F2's `isFinished` formula, and F1's `if
  recording` gate that the old R1 fix retained).

---

## How to use this file

When starting the fix session, this file is the brief. BUILDER
should:

1. Read `.hermes/pending/phase-robustness-bug-2026-06-18.md` (this file).
2. Read the full `.hermes/pending/BUG_DIAGNOSIS_latency_ticker_stuck.md`
   for line-level evidence.
3. Implement F1+F2+F4 as 3 separate commits (or 1 commit if
   they're all in the same file).
4. Add the test from §6 to lock in the fix.
5. Verify: `cargo build --workspace --tests` (0 errors), `cargo
   test --workspace --lib` (0 failures, more tests than before),
   and `cargo clippy --workspace --lib` (no new warnings).
6. Update this file with the actual diff stats and a link to the
   commit hash.
