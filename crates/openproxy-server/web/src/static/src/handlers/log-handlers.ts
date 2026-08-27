// handlers/log-handlers.ts — log view handlers and network logic.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { showToast } from "../components/toast.js";
import { renderRecordingToggle } from "../components/recording-toggle.js";

export async function fetchRecordingState(): Promise<void> {
  try {
    const data: unknown = await api("/recording");
    if (data && typeof data === "object" && "recording" in data) {
      state.logs.recording = !!(data as { recording: unknown })["recording"];
    }
  } catch (err: unknown) {
    console.warn("fetchRecordingState failed", err);
  } finally {
    renderRecordingToggle(state.logs.recording, state.logs.recordingLoading);
  }
}

export async function toggleRecording(): Promise<void> {
  if (state.logs.recordingLoading) return;
  state.logs.recordingLoading = true;
  renderRecordingToggle(state.logs.recording, state.logs.recordingLoading);
  const desired: boolean = !state.logs.recording;
  try {
    const data: unknown = await api("/recording", { method: "POST", body: JSON.stringify({ enabled: desired }) });
    if (data && typeof data === "object" && "recording" in data) {
      state.logs.recording = !!(data as { recording: unknown })["recording"];
    }
  } catch (err: unknown) {
    showToast("Failed to toggle recording: " + (err instanceof Error ? err.message : String(err)), "error");
  } finally {
    state.logs.recordingLoading = false;
    renderRecordingToggle(state.logs.recording, state.logs.recordingLoading);
  }
}

import { liveLogsStore } from "../state/live-logs-store.js";

export function exportLogsCSV(): void {
  const rows = liveLogsStore.selectFinishedRows();
  if (rows.length === 0) {
    showToast("No log rows available to export", "warn");
    return;
  }
  const headers = ["Timestamp", "Request ID", "Trace ID", "Provider", "Model", "Status", "Elapsed (ms)", "Prompt Tokens", "Completion Tokens", "Cost USD", "Error"];
  const csvLines = [headers.join(",")];
  for (const r of rows) {
    const time = new Date(r.startedAtMs).toISOString();
    const reqId = (r.requestId || "").replace(/"/g, '""');
    const traceId = (r.traceId || "").replace(/"/g, '""');
    const prov = (r.providerId || "").replace(/"/g, '""');
    const model = (r.upstreamModelId || "").replace(/"/g, '""');
    const status = r.statusCode != null ? String(r.statusCode) : (r.terminalKind || "");
    const elapsed = r.elapsedMsAtEvent != null ? String(r.elapsedMsAtEvent) : "";
    const pTokens = r.row?.prompt_tokens != null ? String(r.row.prompt_tokens) : "";
    const cTokens = r.row?.completion_tokens != null ? String(r.row.completion_tokens) : "";
    const cost = r.row?.cost_usd != null ? String(r.row.cost_usd) : "";
    const err = (r.error || "").replace(/"/g, '""');
    csvLines.push([
      `"${time}"`,
      `"${reqId}"`,
      `"${traceId}"`,
      `"${prov}"`,
      `"${model}"`,
      `"${status}"`,
      elapsed,
      pTokens,
      cTokens,
      cost,
      `"${err}"`
    ].join(","));
  }
  const blob = new Blob([csvLines.join("\n")], { type: "text/csv;charset=utf-8;" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `openproxy-logs-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-")}.csv`;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
  showToast(`Exported ${rows.length} rows to CSV`, "success");
}


