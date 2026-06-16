// state/ticker.js — 100ms latency ticker for the live-logs view.
// Mutates `.log-phase-sub` and `.log-latency` in place so the
// operator sees a smooth millisecond counter while a request is
// in flight.

import { state } from "./index.js";

function tickLogLatency() {
  const stages = state.logs.stagesByRequestId;
  if (!stages || stages.size === 0) return;
  const now = Date.now();
  const rowEls = document.querySelectorAll("#logs .log-row[data-request-id]");
  if (rowEls.length === 0) return;
  for (const rowEl of rowEls) {
    const requestId = rowEl.dataset.requestId;
    if (!requestId) continue;
    const stage = stages.get(requestId);
    if (!stage) continue;
    if (stage.stage === "completed" || stage.stage === "failed") continue;
    const t = Date.parse(stage.timestamp);
    let live;
    if (isFinite(t)) live = Math.max(0, now - t);
    else live = stage.elapsed_ms || 0;
    const sub = rowEl.querySelector(".log-phase-sub");
    if (sub) {
      let label;
      if (stage.stage === "streaming" && stage.ttft_ms != null) label = `ttft ${stage.ttft_ms}ms`;
      else if ((stage.stage === "waiting_ttft" || stage.stage === "streaming") && stage.connect_ms != null) label = `connect ${stage.connect_ms}ms`;
      else label = `${live}ms`;
      if (sub.textContent !== label) {
        sub.textContent = label;
        sub.classList.add("log-phase-sub--ticking");
      }
    }
    const latencyEl = rowEl.querySelector(".log-latency");
    if (latencyEl) {
      const newLatency = `${live}ms`;
      if (latencyEl.textContent !== newLatency) latencyEl.textContent = newLatency;
    }
  }
}

export function startLogLatencyTicker() {
  if (state.logs.latencyTickerHandle) return;
  state.logs.latencyTickerHandle = setInterval(tickLogLatency, 100);
}

export function stopLogLatencyTicker() {
  if (state.logs.latencyTickerHandle) {
    clearInterval(state.logs.latencyTickerHandle);
    state.logs.latencyTickerHandle = null;
  }
}
