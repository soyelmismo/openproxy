// state/ticker.ts — 100ms latency ticker for the live-logs view.
// Mutates `.log-phase-sub` and `.log-latency` in place so the
// operator sees a smooth millisecond counter while a request is
// in flight.
//
// The ticker reads the stage for each row from the
// `stagesByTraceId` map (primary, keyed by `trace_id` — the unique
// id of a single attempt of a request). The row's `data-trace-id`
// attribute (set by `renderLogRowHtml` in `components/log-row.ts`)
// is what we look up; the legacy `data-request-id` is only used
// as a fallback for the trace_id-less synthetic events the
// frontend emits from the row/history completion paths.

import { state } from "./index.js";
import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";

function tickLogLatency(): void {
  const stagesByTraceId: Map<string, StageEvent> = state.logs.stagesByTraceId;
  const stagesByRequestId: Map<string, StageEvent> = state.logs.stagesByRequestId;
  if (!stagesByTraceId || stagesByTraceId.size === 0) return;
  const now: number = Date.now();
  const rowEls: NodeListOf<Element> = document.querySelectorAll("#logs .log-row[data-request-id]");
  if (rowEls.length === 0) return;
  // Build the row-lookup indexes ONCE per tick. The naive
  // `state.logs.rows.find(...)` inside the per-row loop is
  // O(n) per row per tick — at dashboard scale (50 rows ×
  // 10 Hz) the per-tick cost is O(n × m). Hoisting the
  // indexes out of the loop is the single hottest fix in
  // this file; the equivalent of converting a N² search into
  // a N + M pass.
  const rowByTraceId: Map<string, RecentUsageRow> = new Map();
  const rowByRequestId: Map<string, RecentUsageRow> = new Map();
  for (const r of state.logs.rows) {
    if (r.trace_id) rowByTraceId.set(r.trace_id, r);
    if (r.request_id) rowByRequestId.set(r.request_id, r);
  }
  for (const rowEl of Array.from(rowEls)) {
    const el = rowEl as HTMLElement;
    // Index by `trace_id` first (per attempt). A retry's row
    // has a different `trace_id` from the failed attempt, so
    // looking up by trace_id guarantees the ticker only
    // updates the row whose stage actually matches — and not
    // the historical failed row, which is the
    // "counters-double-on-failed-entries" bug.
    const traceId: string = el.dataset["traceId"] || "";
    const requestId: string = el.dataset["requestId"] || "";
    let stage: StageEvent | undefined;
    if (traceId) {
      stage = stagesByTraceId.get(traceId);
    } else {
      // Fallback for synthetic events emitted without a
      // `trace_id`. We accept the request_id-keyed map here.
      if (requestId) stage = stagesByRequestId.get(requestId);
    }
    if (!stage) continue;
    if (stage.stage === "completed" || stage.stage === "failed" || stage.stage === "cancelled") continue;
    // Row-finalized freeze (R3 defense-in-depth). If the
    // request is already represented by a finalized row in
    // `state.logs.rows`, the backend has recorded the
    // completion and the ticker must freeze at `row.total_ms`
    // regardless of whether the terminal `stage` event made
    // it through. Without this, a single dropped broadcast
    // event (slow consumer, lagged subscriber) lets the
    // counter grow forever against a finalized request.
    const finalizedRow: RecentUsageRow | undefined = traceId
      ? rowByTraceId.get(traceId)
      : (requestId ? rowByRequestId.get(requestId) : undefined);
    const t: number = Date.parse(stage.timestamp);
    let live: number;
    if (isFinite(t)) live = Math.max(0, now - t);
    else live = stage.elapsed_ms || 0;
    if (finalizedRow && finalizedRow.total_ms > 0) {
      live = finalizedRow.total_ms;
    } else if (isFinite(t)) {
      // Stale cap for any non-terminal stage. `streaming` caps at
      // 2 s (a stream that hasn't progressed is stuck); other phases
      // (started / connecting / waiting_ttft) cap at a generous 60 s
      // — well beyond upstream timeouts — so a ghost whose terminal
      // event was lost doesn't climb the counter indefinitely before
      // the stale-inflight reaper resolves it.
      const cap = stage.stage === "streaming" ? 2_000 : 60_000;
      if ((now - t) > cap) {
        live = cap;
      }
    }
    const sub: Element | null = rowEl.querySelector(".log-phase-sub");
    if (sub) {
      let label: string;
      if (finalizedRow && finalizedRow.total_ms > 0) {
        // The row is finalized; show the row's `total_ms`
        // rather than the (frozen) live counter. The
        // `renderLogPhaseHtml` in `components/log-row.ts`
        // also handles this, but the ticker is the path the
        // user actually sees ticking — set the sublabel
        // here so the next paint is coherent with the freeze.
        label = `total ${finalizedRow.total_ms}ms`;
        sub.classList.remove("log-phase-sub--ticking");
      } else if (stage.stage === "streaming" && stage.ttft_ms != null) label = `ttft ${stage.ttft_ms}ms`;
      else if ((stage.stage === "waiting_ttft" || stage.stage === "streaming") && stage.connect_ms != null) label = `connect ${stage.connect_ms}ms`;
      else label = `${live}ms`;
      if (sub.textContent !== label) {
        sub.textContent = label;
        // Don't add the "ticking" class to a row whose stage
        // is frozen. The class is reserved for rows that are
        // actually still climbing.
        const staleCap = stage.stage === "streaming" ? 2_000 : 60_000;
        const isStale = isFinite(t) && (now - t) > staleCap;
        if (!(finalizedRow && finalizedRow.total_ms > 0) && isStale) {
          sub.classList.remove("log-phase-sub--ticking");
        } else {
          sub.classList.add("log-phase-sub--ticking");
        }
      }
    }
    const latencyEl: Element | null = rowEl.querySelector(".log-latency");
    if (latencyEl) {
      const newLatency: string = `${live}ms`;
      if (latencyEl.textContent !== newLatency) latencyEl.textContent = newLatency;
    }
  }
}

export function startLogLatencyTicker(): void {
  if (state.logs.latencyTickerHandle) return;
  state.logs.latencyTickerHandle = setInterval(tickLogLatency, 100);
}

export function stopLogLatencyTicker(): void {
  if (state.logs.latencyTickerHandle) {
    clearInterval(state.logs.latencyTickerHandle);
    state.logs.latencyTickerHandle = null;
  }
}
