// components/recording-toggle.ts — toggle button for recording.
// Migrated to lit-html: uses render() instead of innerHTML.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { showToast } from "./toast.js";

export async function fetchRecordingState(): Promise<void> {
  try {
    const data: unknown = await api("/recording");
    if (data && typeof data === "object" && "recording" in data) {
      state.logs.recording = !!(data as { recording: unknown })["recording"];
    }
  } catch (err: unknown) {
    console.warn("fetchRecordingState failed", err);
  } finally {
    renderRecordingToggle();
  }
}

export async function toggleRecording(): Promise<void> {
  if (state.logs.recordingLoading) return;
  state.logs.recordingLoading = true;
  renderRecordingToggle();
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
    renderRecordingToggle();
  }
}

export function renderRecordingToggle(): void {
  const btn: HTMLElement | null = document.getElementById("logs-recording-toggle");
  if (!btn) return;
  const on: boolean = !!state.logs.recording;
  const loading: boolean = !!state.logs.recordingLoading;
  // Update the existing button in-place instead of rendering a new
  // one into the parent (which caused the duplicate button bug).
  btn.classList.toggle("on", on);
  btn.classList.toggle("off", !on);
  btn.classList.toggle("loading", loading);
  btn.setAttribute("aria-pressed", on ? "true" : "false");
  if (btn instanceof HTMLButtonElement) btn.disabled = loading;
  btn.title = on
    ? "Recording is ON — full bodies and headers are being saved. Click to stop."
    : "Recording is OFF — only metadata is being saved. Click to start recording full bodies and headers.";
  const label = btn.querySelector(".logs-recording-label");
  if (label) label.innerHTML = `⏺ Record: <strong>${on ? "ON" : "OFF"}</strong>`;
}
