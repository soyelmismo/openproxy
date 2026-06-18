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
import { getStageForRow } from "../views/logs.js";
import { phaseSublabel } from "../lib/constants.js";
import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";

function tickLogLatency(): void {
  const stagesByTraceId: Map<string, StageEvent> = state.logs.stagesByTraceId;
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
    const stage: StageEvent | undefined = getStageForRow({ trace_id: traceId, request_id: requestId });
    if (!stage) continue;
    if (stage.stage === "completed" || stage.stage === "failed") continue;
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
    } else if (stage.stage === "streaming") {
      // Stale-`streaming` cap (R3). Once a `streaming` event
      // is older than 2 s without a follow-up event, the
      // counter MUST freeze. We use the same `now - t`
      // formula the rest of the function uses so the next
      // reader doesn't have to reason about a different
      // monotonic-math scheme; the semantic effect is that
      // `live` is no longer recomputed from `stage.elapsed_ms`
      // — it ticks at the wall-clock rate, not the per-event
      // rate. The freeze is semantic, not numerical.
      if (isFinite(t)) {
        const stale: number = now - t;
        if (stale > 2_000) live = stale;
      }
    }
    const sub: Element | null = rowEl.querySelector(".log-phase-sub");
    if (sub) {
      // Use the unified `phaseSublabel` helper (the same one used
      // by `renderLogPhaseHtml` in `components/log-row.ts`) for
      // every branch that has a deterministic answer
      // (`total Nms`, `${total}ms stale`, `ttft Xms`,
      // `connect Yms`). For the live-ticker fallback we keep
      // `${live}ms` (the wall-clock counter) instead of the
      // helper's stage-snapshot fallback, because the ticker is
      // the only path the operator sees actually ticking.
      const helperLabel: string = phaseSublabel(stage, finalizedRow?.total_ms);
      let label: string;
      // If the helper returned anything other than its
      // snapshot fallback (`${stage.elapsed_ms}ms` or `0ms`),
      // it produced a deterministic sublabel — use it.
      // Otherwise we're in the live-fallback case.
      const snapshotMs: number = stage.elapsed_ms || 0;
      const isLiveFallback: boolean =
        helperLabel === `${snapshotMs}ms` || helperLabel === "0ms";
      if (isLiveFallback) {
        label = `${live}ms`;
      } else {
        label = helperLabel;
        // The frozen sublabels (total / stale / ttft / connect)
        // are deterministic — the row is no longer climbing.
        sub.classList.remove("log-phase-sub--ticking");
      }
      if (sub.textContent !== label) {
        sub.textContent = label;
        // Don't add the "ticking" class to a row whose stage
        // is frozen. The class is reserved for rows that are
        // actually still climbing.
        if (!(finalizedRow && finalizedRow.total_ms > 0) && stage.stage === "streaming" && isFinite(t) && (now - t) > 2_000) {
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
