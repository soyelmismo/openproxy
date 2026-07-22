// components/recording-toggle.ts — toggle button for recording.
// Pure visual rendering component focused on UI presentation.

import { state } from "../state/index.js";

export function renderRecordingToggle(recording?: boolean, loading?: boolean): void {
  const btn: HTMLElement | null = document.getElementById("logs-recording-toggle");
  if (!btn) return;
  const on: boolean = recording ?? !!state.logs.recording;
  const isLoading: boolean = loading ?? !!state.logs.recordingLoading;
  // Update the existing button in-place instead of rendering a new
  // one into the parent (which caused the duplicate button bug).
  btn.classList.toggle("on", on);
  btn.classList.toggle("off", !on);
  btn.classList.toggle("loading", isLoading);
  btn.setAttribute("aria-pressed", on ? "true" : "false");
  if (btn instanceof HTMLButtonElement) btn.disabled = isLoading;
  btn.title = on
    ? "Recording is ON — full bodies and headers are being saved. Click to stop."
    : "Recording is OFF — only metadata is being saved. Click to start recording full bodies and headers.";
  const label = btn.querySelector(".logs-recording-label");
  if (label) label.innerHTML = `⏺ Record: <strong>${on ? "ON" : "OFF"}</strong>`;
}

export { fetchRecordingState, toggleRecording } from "../handlers/log-handlers.js";

