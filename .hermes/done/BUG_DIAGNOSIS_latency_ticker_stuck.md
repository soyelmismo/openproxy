# Bug diagnosis: live-log latency ticker stuck on "esperando ttft"

Bug report (Spanish): *"Cuando una petición ya terminó en el cliente, el sistema aún dice
'esperando ttft' y el contador de latency sigue sumando indefinidamente. El `stop_reason`
del upstream no se está tomando en cuenta."*

## TL;DR

The bug is real and **larger** than the user's hypothesis. There are at least **4
distinct "lost terminal event" paths**, not just the one the user described. The
`isFinished` formula in `logs.ts:589-593` is a textbook OR-of-redundant-terms bug
that lets a fully-complete row masquerade as "still streaming" on the dashboard.
And the R3 row-finalized freeze in `ticker.ts:72-73` *does* freeze the
counter, but **only the counter / sublabel** — the top label "esperando ttft"
keeps showing because the static `renderLogPhaseHtml` (called only on
`renderLogsRows`) re-uses the stale `stage.stage`.

`stop_reason` is correctly identified as never persisted; the chain is confirmed.

---

## Verification of the 3-part hypothesis

### ✅ Hypothesis 1 — terminal stage event gated by `recording` — CONFIRMED

`crates/openproxy-core/src/pipeline.rs:3205`:

```rust
cost::record(&conn, &input)?;                       // row INSERTED always (line 3200)
if recording {                                      // line 3205 — gate
    let stage_label: &str = if err.is_none() { "completed" } else { "failed" };
    ...
    crate::usage::publish_stage_event(...);         // line 3217 — only fires when recording=ON
}
```

* `cost::record` (cost.rs:107-180) is **not** gated by `recording`. The DB row
  is **always** inserted, and `publish_usage_row` (cost.rs:209) **always**
  fires. Only the heavy body/header columns are `None` when `recording=OFF`.
* The terminal stage event publish is gated. So with `recording=OFF` the
  dashboard gets the row (with `is_streaming=true, stream_complete=false,
  status_code=200` for the upstream-silent case) but **never** gets the
  `stage: "completed"` event.

**Stronger than the user thought:** the same `is_recording()` gate is applied
to **every** stage event in the pipeline:

| Site | Line | Stage emitted |
|---|---|---|
| `emit_stage` closure | pipeline.rs:1822-1824 | `started`, `connecting` |
| `emit_stage("waiting_ttft", ...)` | pipeline.rs:1866 | `waiting_ttft` |
| non-streaming R2 fix | pipeline.rs:2019 | `streaming` |
| SSE first-chunk path | pipeline.rs:2626 | `streaming` |
| **terminal (success/fail)** | **pipeline.rs:3205** | **`completed`/`failed`** |

So with `recording=OFF` the dashboard's `stagesByTraceId` map is **empty**
for the entire request lifetime. The ticker at `ticker.ts:47-55` does
`if (!stage) continue;` and the row is skipped — the static inflight
placeholder / row is the only thing the user sees.

#### `isFinished` formula in `logs.ts:589-593` — confirmed broken for streaming-no-DONE

```ts
const isFinished = (
  (!row.is_streaming || row.stream_complete) ||
  (row.status_code > 0 && !row.is_streaming) ||
  (row.status_code > 0 && row.stream_complete)
);
```

User's worked example: `is_streaming=true, stream_complete=false,
status_code=200` →
- Term 1: `(!true || false)` = `false`
- Term 2: `(200>0 && !true)` = `false`
- Term 3: `(200>0 && false)` = `false`
- ⇒ `isFinished = false` → **no synthetic terminal event** is emitted, so the
  stage map stays stuck on whatever last `waiting_ttft` / `streaming` was.

This row shape is exactly what `pipeline.rs:2863-2864` produces in the
**streaming-success with `done_sent=false`** case (i.e. the upstream didn't
emit a `[DONE]` sentinel; this is the "common case for non-OpenAI providers
that close the connection gracefully" per the comment at line 2464-2468).

> Note: in the **happy path** (OpenAI upstream with explicit `[DONE]`,
> `done_sent=true`), `stream_complete=true` and Term 1 evaluates to true,
> so the synthetic terminal event fires. The bug only shows when
> `done_sent=false`, or on `recording=OFF`, or on `is_streaming=true,
> stream_complete=false` from any other path.

#### Cases where the user's analysis is WRONG

- For a **non-streaming success** (`is_streaming=false, stream_complete=true,
  status_code=200`): Term 1 is `(!false || true) = true` → isFinished works.
- For a **failure** (`status_code=499` etc., `is_streaming=false,
  stream_complete=false`): Term 1 is `(!false || false) = true` → works.
- For **`history` rows** (logs.ts:519-545): the same blind spot is
  replicated — historical rows with `is_streaming=true, stream_complete=false`
  ALSO never get a synthetic terminal event from the history path. So a
  page refresh on a stuck-in-`streaming` row doesn't fix it.

### ✅ Hypothesis 2 — no `stream_tokens` WS publisher — CONFIRMED

`grep -rn "stream_tokens" crates/openproxy-server crates/openproxy-core` →
**zero hits**. The only place that string appears in the entire workspace is
`crates/openproxy-web/src/static/src/views/logs.ts` (lines 458, 639-640):

```ts
} else if (msg.type === "stream_tokens") {     // line 639
  handleStreamTokens(msg);                     // line 640
}
```

`handleStreamTokens` (logs.ts:458-479) is a dead path: its only side effects
on dashboard state are
- writing the running-concat to `state.logs.liveTokens` (used by the token
  panel UI; not the ticker)
- the `if (msg.complete)` branch (lines 471-474) that sets
  `row.stream_complete = true` on a row — **unreachable**, because the
  `complete: true` flag is never set by anything.

So `row.stream_complete` only ever becomes `true` via the **server-side
`cost::record` `record_attempt_raw_with_tokens` call** (pipeline.rs:2864
for streaming, line 2173 for non-streaming). The dashboard-side handler is
dead code.

### ✅ Hypothesis 3 — `stop_reason` not persisted — CONFIRMED

`stop_reason` is **extracted** in two places but **never stored** anywhere
the dashboard can see:

* `sse.rs:305-313` (Anthropic SSE):
  ```rust
  let stop_reason = data.get("delta").and_then(|d| d.get("stop_reason"))...
  let finish_reason = match stop_reason {
      Some("end_turn") | Some("stop_sequence") => Some("stop".to_string()),
      Some("max_tokens") => Some("length".to_string()),
      _ => None,
  };
  ```
  `stop_reason` is a local binding; only `finish_reason` (the OpenAI-mapped
  string) goes into the SSE chunk payload. `UpstreamSseChunk` (sse.rs:11-19)
  has fields `payload`, `done`, `usage` — no `stop_reason`.

* `translation.rs:152` (Anthropic non-streaming):
  `AnthropicResponse { stop_reason: Option<String>, ... }` is built during
  translation but the `stop_reason` is never copied into `UsageInput`,
  `RecentUsageRow`, `StageEvent`, or any DB column.

* The `usage` table has no `stop_reason` column (verified in
  `cost.rs:108-120` — the INSERT list does not contain it). There is no
  corresponding `usage.stop_reason` field in `RecentUsageRow`
  (usage.rs:746-770).

* `StageEvent` (usage.rs:128-167) has no `stop_reason` field. It has
  `error: Option<String>` for failures, but that's the upstream error
  string, not the success-side stop reason (`"end_turn"`, `"max_tokens"`,
  `"stop_sequence"`, `"tool_use"`, etc.).

* `UsageInput` (cost.rs:13-46) has no `stop_reason` field. So even adding
  it to `StageEvent` would require threading it through `pipeline.rs:3122-3128`
  → `record_attempt_raw_with_tokens` → `UsageInput` → `cost::record` →
  DB column.

The streaming loop at `pipeline.rs:2434-2469` does **not** track
`stop_reason` per-chunk (it tracks `usage` and `ttft_ms` only). The
`message_delta` SSE event handler in `sse.rs:303-339` extracts the
`stop_reason` into a local variable and immediately discards it after
mapping to `finish_reason`.

---

## Additional lost-terminal-event paths (the user only listed one)

### P1 — streaming success with `done_sent=false` (the main bug)

`pipeline.rs:2863-2864`:
```rust
true,        // is_streaming (H5)
done_sent,   // stream_complete (H5)
```
When the upstream closes the connection without an explicit `[DONE]`
sentinel (the common case for non-OpenAI providers, per the comment at
pipeline.rs:2464-2468), `done_sent=false`. The row arrives with
`is_streaming=true, stream_complete=false, status_code=200`, which fails
the `isFinished` check (hypothesis 1 above). Even with `recording=ON`,
the dashboard is stuck on `streaming` / `recibiendo streaming` until the
operator refreshes the page (and even then, see P5).

### P2 — `recording=OFF` (every stage event is gated)

This is the strongest version of the bug: with `recording=OFF`, **no**
stage events are published (every call site in `pipeline.rs` checks
`is_recording()` or `recording`). The dashboard's `stagesByTraceId` is
empty for the whole request. The ticker at `ticker.ts:47-55` does
`if (!stage) continue;` and the row is never touched by the ticker. The
static `renderLogPhaseHtml` (log-row.ts:12-45) sees `phase=undefined` →
renders `"—"` (log-row.ts:18). The user wouldn't see "esperando ttft"
in this mode; they'd see `"—"` for every live row. But the **counter**
cell (line 70) shows the row's `total_ms` only when `renderLogsRows` is
called; the ticker never updates it. So the visual is the same broken
state.

### P3 — `try_lock_for` writer-lock timeout (row never inserted)

`pipeline.rs:3184-3199`:
```rust
let conn = match self.conn.try_lock_for(HOT_PATH_LOCK_TIMEOUT) {
    Some(g) => g,
    None => {
        tracing::warn!(...);
        return Ok(());                    // <— early-return, no row, no stage event
    }
};
```
If the writer lock can't be acquired within 100ms (a long admin query
holding the writer), the function returns `Ok(())` **without** inserting
the row and **without** publishing the terminal stage event. The inflight
placeholder row at `pipeline.rs:3265-3266` (`is_streaming=true,
stream_complete=false`) is the only thing the dashboard has. With
`recording=ON`, a `streaming` or `waiting_ttft` event is in the map and
the ticker grows `live = now - t` indefinitely until a 2-second `stale`
cap (ticker.ts:84-87) sets `live = stale`, which **keeps growing** every
tick. The `ticking` CSS class is removed (line 109), but the displayed
number keeps climbing.

### P4 — `resyncUsageRows` bypasses the row-handler logic

`logs.ts:436-456`:
```ts
async function resyncUsageRows(sinceId: number): Promise<void> {
  try {
    const rows = await api(`/usage/recent?since_id=${...}&limit=500`) as RecentUsageRow[] | null;
    if (Array.isArray(rows) && rows.length > 0) {
      state.logs.rows = mergeLogsByDescId(state.logs.rows, rows);
      for (const r of rows) { state.logs.rowById.set(r.id, r); }
      if (state.logs.followTail) state.logs.page = 1;
      renderLogsRows();
    }
  } catch (e) { ... }
}
```

This is a **silent fix hole**. When the dashboard's WS is lagged (drops a
`row` envelope), the server sends `lag_warning` + `resync`. The resync
fetches rows over HTTP and merges them in via `mergeLogsByDescId`. The
`handleLogsMessage` `row` branch (logs.ts:548-633) — which contains the
`isFinished` synthetic-event logic — is **never invoked** for these rows.
The inflight placeholder at `inflightByTraceId` is **not cleared**
(only the WS `row` handler at logs.ts:553-562 does that). The R3 freeze
in `ticker.ts:72-73` does work for the **counter** (because the row is in
`state.logs.rows` and `total_ms > 0`), but the **label** stays stuck on
the last `stage.stage` because no synthetic terminal event was ever
emitted, and `renderLogPhaseHtml` (log-row.ts:35-43) only switches to
`"total Xms"` when `phase === "completed" || "failed"` — otherwise it
shows `"Xms stale"` (line 40) or the stage label itself.

### P5 — `history` rows have the same `isFinished` blind spot

`logs.ts:519-545` synthesizes a terminal event for historical rows under
the same `isFinished`-style guard:
```ts
if ((!r.is_streaming || r.stream_complete) && r.status_code > 0) { ... }
```
This is the **same broken formula** as the WS `row` branch (just
expressed as a single condition). A historical row with `is_streaming=true,
stream_complete=false` (i.e. a P1 row that was committed to the DB) won't
get a synthetic terminal event in the history path either, so a hard page
refresh doesn't help.

### P6 — Stage event overwrites the terminal in the map

If the terminal `stage` event arrives at the dashboard *before* a stale
`streaming` event (e.g. the broadcast channel reorders or duplicates
events), the `setStage` helper at `logs.ts:421-426` does an unconditional
`.set()`:
```ts
state.logs.stagesByTraceId.set(traceId, event);
```
A late `streaming` event with a fresh `timestamp` would clobber the
terminal `completed` event in the map, putting the ticker back into the
growing state. The `existingIsTerminal` check at lines 601-602 only
prevents **synthetic** re-emission from a row, not stage-map clobbering.
Low probability in practice (single-producer pipeline) but a real
concurrency hazard.

### P7 — "stale" cap in ticker is a half-fix

`ticker.ts:84-87`:
```ts
} else if (stage.stage === "streaming") {
  if (isFinite(t)) {
    const stale: number = now - t;
    if (stale > 2_000) live = stale;
  }
}
```
The "freeze" is purely **visual**: `live` is set to `now - t`, which
grows by 100ms every tick. The intent (per the comment) is to stop the
counter from drifting via `stage.elapsed_ms`, but the actual effect is
that the displayed number now tracks wall-clock since the last event —
which keeps growing until a new event arrives. So a long-stuck
`streaming` event makes the latency cell keep climbing for the rest of
the dashboard session, just at a slower rate. The user reported exactly
this symptom: "el contador de latency sigue sumando indefinidamente".

The same cap is **not** applied for `waiting_ttft` or `connecting` stages.
For those, the un-frozen `live = now - t` (line 70) is used directly.

### P8 — Failure recording uses `is_streaming: false, stream_complete: false`

`pipeline.rs:3065-3066`:
```rust
false, // is_streaming (H5): failure path, can't be sure
false, // stream_complete (H5): failure path
```
Failures use `is_streaming=false, stream_complete=false`. So in the
`isFinished` formula:
- Term 1: `(!false || false)` = `true` → works.
- This is fine for the synthetic-event branch.

But: for a failure (e.g. 502, 429, 504), if the streaming connection was
mid-stream, the **inflight placeholder** is the only thing the dashboard
has while the failure is being recorded. The inflight has
`is_streaming=true, stream_complete=false, status_code=0`. The stage in
the map is `streaming` (from the last successful `streaming` event).
If `recording=OFF`, no failure stage event is published; the ticker is
stuck on `streaming` with a growing counter. The failure row arrives
later via `cost::record`, but the synthetic-event logic at logs.ts:589-593
correctly identifies it as finished (status_code >= 400, is_streaming=
false) — so the **next** `renderLogsRows` updates the label. But the
counter was growing for the entire failure-recording window, which can
be seconds.

### P9 — Race-loser row handling (mostly OK, but inconsistent)

Race losers get `is_streaming=false, stream_complete=false,
status_code=499` (`error.rs:172`). `isFinished` is true (Term 1 hits).
The row handler synthesizes a terminal event correctly. So the dashboard
shows "falló" for the race loser. **No bug here.** Listed only for
completeness — the user mentioned "race-loser" in their question.

### P10 — `record_no_healthy_targets_row` doesn't emit a stage event

`pipeline.rs:943-989` inserts the row directly via `cost::record` and
**never** publishes a stage event (not even a terminal one). The
`isFinished` formula handles it correctly (`is_streaming=false →
Term 1=true`), so the synthetic-event logic covers it. Listed only for
completeness.

---

## Prioritized list of "lost terminal event" paths (by likelihood × user impact)

| # | Path | Bug? | Impact | Triggered by |
|---|---|---|---|---|
| **1** | `isFinished` formula doesn't fire for `is_streaming=true, stream_complete=false, status_code>0` (P1 + P5) | yes | high | any non-OpenAI streaming upstream that closes without `[DONE]` (most common); operator-facing "stuck" UI on every such request |
| **2** | `recording=OFF` silences **all** stage events (P2) | yes | high | operator has the recording toggle off (or hasn't enabled it after first install) |
| **3** | `try_lock_for` timeout at pipeline.rs:3184-3199 (P3) | yes | medium | writer held by a slow admin query during a chat request |
| **4** | `resyncUsageRows` bypasses the row-handler `isFinished` logic (P4) | yes | medium | slow consumer; long-poll dashboard; dashboard tab in background |
| **5** | `stream_tokens` WS message is never published (hypothesis 2) | yes (dead code) | low | none — the handler is unreachable |
| **6** | Late `streaming` event clobbers terminal in stage map (P6) | yes | low | broadcast channel reorders events; extremely rare in practice |
| **7** | `ticker.ts` 2s "stale" cap is wall-clock, not actually frozen (P7) | yes | medium | long-stuck `streaming` event (consequence of #1 and #2) |
| **8** | Failure path inflight has `is_streaming=true, status_code=0` (P8) | yes (symptom only) | low | failure during mid-stream; counter grows for the failure-recording window |

The four **realistic** user-visible paths in production are **#1, #2, #3, #4**.
**#7** is the visible symptom of #1 / #2 — the counter keeps growing for
as long as the dashboard session is open, until the operator refreshes.

---

## Prioritized fix list

The user's three-part fix is correct, but incomplete. The order below is
by **blast radius** (lowest-risk changes first) and dependencies.

### F1 (BLOCKER, do first) — emit terminal stage event unconditionally

`crates/openproxy-core/src/pipeline.rs:3205` — drop the `if recording` gate
around the terminal event publish. The heavy body/header columns in the
DB row stay gated by `recording` (the `cost::record` INSERT already
respects that for `request_body_json` etc. at cost.rs:199-160 and
pipeline.rs:3159-3162), but the dashboard's terminal signal should fire
regardless of the operator's recording preference.

The terminal event is the only signal that synchronizes the dashboard's
phase label. The "stale" / "lagged" R3 freeze is a partial backstop but
it leaves the top label stuck on `waiting_ttft` / `streaming` and doesn't
fix the `ticker.ts:84-87` wall-clock counter growth.

### F2 (BLOCKER, do with F1) — fix the `isFinished` formula

`crates/openproxy-web/src/static/src/views/logs.ts:589-593`. The current
formula is redundant and wrong. Replace with a single condition:
```ts
const isFinished = row.status_code > 0;
```
or, if you want to be more conservative and align with the DB semantics:
```ts
const isFinished = row.status_code > 0 || row.error_message != null;
```
Either way, `is_streaming` and `stream_complete` are **metadata** about
the response shape, not about whether the request is still in flight.
The presence of a non-zero `status_code` is the canonical "request
finished" signal — that's what the operator cares about.

The same fix should be applied to the `history` branch at
`logs.ts:519-521` and to the `resyncUsageRows` path (which doesn't
synthesize at all today — see F5).

### F3 (HIGH) — persist `stop_reason` end-to-end

Thread `stop_reason` from the upstream SSE chunk into:
- `crates/openproxy-core/src/sse.rs:11-19` — add `pub stop_reason:
  Option<String>` to `UpstreamSseChunk`.
- `crates/openproxy-core/src/pipeline.rs:2434-2469` — capture the
  `stop_reason` from the `message_delta` chunk in the streaming loop and
  carry it to `record_attempt_raw_with_tokens`.
- `crates/openproxy-core/src/pipeline.rs:3106-3129` — add a
  `stop_reason: Option<String>` parameter to
  `record_attempt_raw_with_tokens`.
- `crates/openproxy-core/src/usage.rs:128-167` — add `pub stop_reason:
  Option<String>` to `StageEvent`.
- `crates/openproxy-core/src/usage.rs:746-770` — add `pub stop_reason:
  Option<String>` to `RecentUsageRow`.
- `crates/openproxy-core/src/cost.rs:13-46` — add to `UsageInput`.
- `crates/openproxy-core/src/cost.rs:107-180` — extend the INSERT with a
  new `usage.stop_reason` column (migration + new index for the
  `errors-by-reason` analytics view).
- `crates/openproxy-web/src/static/src/lib/types/api.ts:292-308` and
  `...:355-378` — add the field to `StageEvent` and `RecentUsageRow`.

This also fixes a parallel concern: the dashboard currently can't
distinguish `end_turn` from `max_tokens` from `stop_sequence` — operators
have no way to tell whether a slow model response hit the token limit or
ran out of tokens. The `error_message` column carries the redacted
upstream error string for failures, but success-side stop semantics are
invisible.

### F4 (HIGH) — make `resyncUsageRows` run the synthetic-terminal logic

`crates/openproxy-web/src/static/src/views/logs.ts:436-456` — after
`mergeLogsByDescId`, iterate `rows` and call the same `isFinished` →
synthetic terminal event logic that the WS `row` branch uses
(logs.ts:588-628). Either extract the logic into a shared helper or
duplicate it (the existing duplication is the kind of debt this fix
should reduce, not extend).

This also needs to clear the inflight placeholder for the affected
`trace_id`s, so the next `renderLogsRows` doesn't show a stale inflight
in addition to the real row.

### F5 (MEDIUM) — fix the "stale" cap in the ticker

`crates/openproxy-web/src/static/src/state/ticker.ts:84-87` — the
current cap is wall-clock (`live = now - t`), which keeps growing. The
fix is to **freeze** to a stable value (e.g. `t + 2_000` from the
event timestamp) instead of recomputing `now - t` every tick. Or, better,
let F1 + F2 do their job and have the ticker short-circuit on
`finalizedRow && finalizedRow.total_ms > 0` (which it already does at
line 72-73) — but only **after** the synthetic terminal event has been
written to the map. The current "stale" hack is a workaround for the
`isFinished` bug; once F1 + F2 land, the cap can be deleted or
simplified to a single ceiling.

### F6 (MEDIUM) — make `stream_tokens` real, or remove the dead handler

`crates/openproxy-web/src/static/src/views/logs.ts:458-479` and
`...:639-640` — the backend never publishes a `stream_tokens` message.
Two options:
- **Implement it**: in the SSE streaming loop at `pipeline.rs:2434-2873`,
  push a `stream_tokens` envelope through a new `STREAM_TOKENS_SENDER`
  broadcast channel (or piggyback on the existing `STAGE_SENDER`) for
  each chunk. This enables the live-token panel without an extra round
  trip and lets the dashboard update `row.stream_complete = true`
  immediately when `[DONE]` arrives.
- **Remove the dead code**: the `handleStreamTokens` function and the
  `case "stream_tokens"` in the dispatcher can be deleted if the live
  token panel is not on the roadmap. This also lets the `WsEnvelope`
  type's `delta` / `complete` / `id` fields be removed from
  `lib/types/api.ts:48-52`.

The user's three-part fix (B) lists publishing `stream_tokens` with
`complete: true` as a fix for the ticker. But the ticker doesn't use
`stream_complete` from the WS row — it uses the **DB row's**
`stream_complete` field (set by the server's `record_attempt_raw_with_
tokens`). So the `stream_tokens` fix is more about the live-token panel
than the ticker. If the goal is the ticker, F1 + F2 are the real fix.

### F7 (LOW) — clear the writer-lock-timeout path

`crates/openproxy-core/src/pipeline.rs:3184-3199` — when the writer lock
times out, currently we return `Ok(())` and silently lose the row + the
terminal event. Two options:
- Block for longer (e.g. 5 s ceiling) instead of 100 ms; this trades
  chat-latency tail for row reliability.
- Publish the terminal stage event **before** the lock attempt, so even
  if the row insert is dropped, the dashboard sees the terminal signal.
  The order in the current code is row-first-then-event, which is the
  opposite of what would be safest here.

The simplest fix: do the stage event publish *outside* the `if recording`
gate, with the same F1 change, and also move the publish *before* the
`cost::record` call. Then if the row is dropped, the dashboard still
freezes correctly via the terminal event.

### F8 (LOW) — fix the `setStage` clobber hazard

`crates/openproxy-web/src/static/src/views/logs.ts:421-426` — the
unconditional `.set()` on a stage event lets a late `streaming` event
overwrite a `completed` / `failed` event in the map. Add a guard:
```ts
function setStage(event, requestId) {
  const existing = state.logs.stagesByTraceId.get(traceId);
  if (existing && (existing.stage === "completed" || existing.stage === "failed")
      && (event.stage !== "completed" && event.stage !== "failed")) {
    return; // terminal events are sticky
  }
  ...
}
```

This is a low-probability race in single-producer Rust code, but the
guard is cheap insurance.

---

## What I changed

Just this file: `/root/proyectos/openproxy/BUG_DIAGNOSIS_latency_ticker_stuck.md`
(analysis only, no code changes per the brief).

## What I'd recommend doing first

1. **F1 + F2** together. They are 2 small edits that close 4 of the 8
   lost-terminal-event paths (#1, #2, #4 partially, #7).
2. **F3** in a follow-up. The migration + API change is bigger but
   the data is currently being thrown away.
3. **F4** to close the resync gap. Small refactor — extract the
   synthetic-terminal logic into a helper used by both the `row` and
   `resync` paths.
4. **F5, F6, F7, F8** as cleanup once F1 + F2 + F3 are in.
