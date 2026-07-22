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

export function exportLogsCSV(): void {
  // Reserved for a future feature. Right now we just toast.
  showToast("CSV export is not implemented yet.", "info");
}

