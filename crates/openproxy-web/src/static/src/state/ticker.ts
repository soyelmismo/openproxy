// state/ticker.ts — 100ms latency ticker for the live-logs view.
// Mutates `.log-phase-sub` and `.log-latency` in place so the
// operator sees a smooth millisecond counter while a request is
// in flight.

import { state } from "./index.js";
import type { StageEvent } from "../lib/types/api.js";

function tickLogLatency(): void {
  const stages: Map<string, StageEvent> = state.logs.stagesByRequestId;
  if (!stages || stages.size === 0) return;
  const now: number = Date.now();
  const rowEls: NodeListOf<Element> = document.querySelectorAll("#logs .log-row[data-request-id]");
  if (rowEls.length === 0) return;
  for (const rowEl of Array.from(rowEls)) {
    const requestId: string | undefined = (rowEl as HTMLElement).dataset["requestId"];
    if (!requestId) continue;
    const stage: StageEvent | undefined = stages.get(requestId);
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
