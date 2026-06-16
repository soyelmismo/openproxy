// components/recording-toggle.js — the toggle button on the live-
// logs header. Mirrors the original `toggleRecording()` /
// `fetchRecordingState()` / `renderRecordingToggle()` flow.

import { state } from "../state/index.js";
import { api } from "../state/api.js";

export async function fetchRecordingState() {
  try {
    const data = await api("/recording");
    state.logs.recording = !!data.recording;
  } catch (err) {
    console.warn("fetchRecordingState failed", err);
  } finally {
    renderRecordingToggle();
  }
}

export async function toggleRecording() {
  if (state.logs.recordingLoading) return;
  state.logs.recordingLoading = true;
  renderRecordingToggle();
  const desired = !state.logs.recording;
  try {
    const data = await api("/recording", { method: "POST", body: JSON.stringify({ enabled: desired }) });
    state.logs.recording = !!data.recording;
  } catch (err) {
    console.error("toggleRecording failed", err);
    alert("Failed to toggle recording: " + err.message);
  } finally {
    state.logs.recordingLoading = false;
    renderRecordingToggle();
  }
}

export function renderRecordingToggle() {
  const btn = document.getElementById("logs-recording-toggle");
  if (!btn) return;
  const on = !!state.logs.recording;
  const loading = !!state.logs.recordingLoading;
  btn.classList.toggle("on", on);
  btn.classList.toggle("off", !on);
  btn.classList.toggle("loading", loading);
  btn.setAttribute("aria-pressed", on ? "true" : "false");
  btn.disabled = loading;
  const label = btn.querySelector(".logs-recording-label");
  if (label) label.innerHTML = `⏺ Record: <strong>${on ? "ON" : "OFF"}</strong>`;
  btn.title = on
    ? "Recording is ON — full bodies and headers are being saved. Click to stop."
    : "Recording is OFF — only metadata is being saved. Click to start recording full bodies and headers.";
}
