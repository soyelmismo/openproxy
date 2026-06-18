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
import type { StageEvent } from "../lib/types/api.js";

function tickLogLatency(): void {
  const stagesByTraceId: Map<string, StageEvent> = state.logs.stagesByTraceId;
  const stagesByRequestId: Map<string, StageEvent> = state.logs.stagesByRequestId;
  if (!stagesByTraceId || stagesByTraceId.size === 0) return;
  const now: number = Date.now();
  const rowEls: NodeListOf<Element> = document.querySelectorAll("#logs .log-row[data-request-id]");
  if (rowEls.length === 0) return;
  for (const rowEl of Array.from(rowEls)) {
    const el = rowEl as HTMLElement;
    // Index by `trace_id` first (per attempt). A retry's row
    // has a different `trace_id` from the failed attempt, so
    // looking up by trace_id guarantees the ticker only
    // updates the row whose stage actually matches — and not
    // the historical failed row, which is the
    // "counters-double-on-failed-entries" bug.
    const traceId: string = el.dataset["traceId"] || "";
    let stage: StageEvent | undefined;
    if (traceId) {
      stage = stagesByTraceId.get(traceId);
    } else {
      // Fallback for synthetic events emitted without a
      // `trace_id`. We accept the request_id-keyed map here.
      const requestId: string | undefined = el.dataset["requestId"];
      if (requestId) stage = stagesByRequestId.get(requestId);
    }
    if (!stage) continue;
    if (stage.stage === "completed" || stage.stage === "failed") continue;
    const t: number = Date.parse(stage.timestamp);
    let live: number;
    if (isFinite(t)) live = Math.max(0, now - t);
    else live = stage.elapsed_ms || 0;
    const sub: Element | null = rowEl.querySelector(".log-phase-sub");
    if (sub) {
      let label: string;
      if (stage.stage === "streaming" && stage.ttft_ms != null) label = `ttft ${stage.ttft_ms}ms`;
      else if ((stage.stage === "waiting_ttft" || stage.stage === "streaming") && stage.connect_ms != null) label = `connect ${stage.connect_ms}ms`;
      else label = `${live}ms`;
      if (sub.textContent !== label) {
        sub.textContent = label;
        sub.classList.add("log-phase-sub--ticking");
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
