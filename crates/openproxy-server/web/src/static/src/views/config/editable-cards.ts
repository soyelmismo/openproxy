// views/config/editable-cards.ts — the five editable config sections
// (timeouts, recording TTL, compression, idle-chunk-retryable, quota
// protection). Each section auto-saves on change via its own PUT
// endpoint; values update optimistically and revert on failure.
//
// The four legacy per-section save functions
// (`configSaveTimeouts`, `configSaveRecordingTtl`,
// `configSaveCompression`, `configSaveIdleChunkRetryable`) are kept
// and re-exported by index.ts because `handlers/registry.ts` imports
// them by name. They remain functional (each saves just its own
// section) by delegating to the same internal `patch*` helpers the
// live @change handlers use, but they read their inputs from the DOM
// via `document.querySelector` so the registry contract (called with
// no args) keeps working.

import { html, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";
import { api } from "../../state/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { t } from "../../i18n/index.js";
import {
  card, errStr, getConfig, renderField, setBanner, validateNonNegInt,
  DEFAULT_TIMEOUTS, TIMEOUT_FIELDS,
  type ConfigPayload, type TimeoutsState, type TimeoutKey,
} from "./shared.js";

// ── Live editable values ────────────────────────────────────────────
//
// What the user sees in the inputs. The `patch*` helpers update these
// optimistically and revert on failure. The render functions bind the
// inputs to these values via `.value=${...}` so a revert is reflected
// immediately.

let liveTimeouts: TimeoutsState = { ...DEFAULT_TIMEOUTS };
let liveRecordingTtl = 300;
let liveCompression = "off";
let liveIdleChunkRetryable = false;
let liveQuotaProtectionEnabled = true;
let liveQuotaProtectionThreshold = 10;

/** Seed the live editable values from a freshly fetched `/config`
 *  payload. Called by mountConfig after the fetch succeeds. */
export function applyServerConfig(payload: ConfigPayload): void {
  const t = payload.timeouts || {};
  liveTimeouts = {
    connect_ms: t.connect_ms ?? 0,
    request_send_ms: t.request_send_ms ?? 0,
    ttft_ms: t.ttft_ms ?? 0,
    idle_chunk_ms: t.idle_chunk_ms ?? 0,
    total_ms: t.total_ms ?? 0,
  };
  liveRecordingTtl = payload.recording_ttl_secs ?? 300;
  liveCompression = payload.compression ?? "off";
  liveIdleChunkRetryable = payload.idle_chunk_retryable ?? false;
  liveQuotaProtectionEnabled = payload.quota_protection?.enabled ?? true;
  liveQuotaProtectionThreshold = payload.quota_protection?.threshold_percentage ?? 10;
}

// ── Per-section save helpers (used by both the @change handlers
//    and the four legacy exported functions). ────────────────────────

async function patchTimeouts(values: Record<TimeoutKey, number>): Promise<boolean> {
  try {
    await api("/config/timeouts", { method: "PUT", body: JSON.stringify(values) });
    const cfg = getConfig();
    if (cfg?.timeouts) Object.assign(cfg.timeouts, values);
    showToast(t("config.timeouts.toast.updated"), "success");
    setBanner("success", t("config.banner.live_applies"),
      t("config.banner.live_applies_body"));
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast(t("config.toast.error", { message: errStr(e) }), "error");
    requestUpdate();
    return false;
  }
}

async function patchRecordingTtl(value: number): Promise<boolean> {
  try {
    await api("/config/recording-ttl", { method: "PUT", body: JSON.stringify({ recording_ttl_secs: value }) });
    const cfg = getConfig();
    if (cfg) cfg.recording_ttl_secs = value;
    showToast(t("config.recording_ttl.toast.set", { value }), "success");
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast(t("config.toast.error", { message: errStr(e) }), "error");
    requestUpdate();
    return false;
  }
}

async function patchCompression(mode: string): Promise<boolean> {
  try {
    await api("/config/compression", { method: "PUT", body: JSON.stringify(mode) });
    const cfg = getConfig();
    if (cfg) cfg.compression = mode;
    showToast(t("config.compression.toast.set", { mode }), "success");
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast(t("config.toast.error", { message: errStr(e) }), "error");
    requestUpdate();
    return false;
  }
}

async function patchIdleChunkRetryable(val: boolean): Promise<boolean> {
  const prev = liveIdleChunkRetryable;
  liveIdleChunkRetryable = val; // optimistic — toggle reflects immediately
  requestUpdate();
  try {
    await api("/config/idle-chunk-retryable", { method: "PUT", body: JSON.stringify({ idle_chunk_retryable: val }) });
    const cfg = getConfig();
    if (cfg) cfg.idle_chunk_retryable = val;
    showToast(t("config.idle_chunk.toast.set", { value: String(val) }), "success");
    return true;
  } catch (e: unknown) {
    liveIdleChunkRetryable = prev; // revert
    showToast(t("config.toast.error", { message: errStr(e) }), "error");
    requestUpdate();
    return false;
  }
}

async function patchQuotaProtection(enabled: boolean, threshold: number): Promise<boolean> {
  try {
    await api("/config/quota-protection", {
      method: "PUT",
      body: JSON.stringify({ enabled, threshold_percentage: threshold }),
    });
    const cfg = getConfig();
    if (cfg) {
      cfg.quota_protection = { enabled, threshold_percentage: threshold };
    }
    showToast(t("config.quota.toast.updated"), "success");
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast(t("config.toast.error", { message: errStr(e) }), "error");
    requestUpdate();
    return false;
  }
}

// ── @change / @click handlers (bound from the lit-html template) ────

async function onTimeoutChange(field: TimeoutKey, e: Event): Promise<void> {
  // Rule: only fire on "change" (blur/enter), not on every keystroke.
  if (e.type === "input") return;
  const raw = (e.target as HTMLInputElement).value.trim();
  const n = validateNonNegInt(raw, `timeouts.${field}`);
  if (n == null) {
    requestUpdate(); // revert input to live value
    return;
  }
  const prev = liveTimeouts[field];
  liveTimeouts[field] = n;
  const ok = await patchTimeouts(liveTimeouts);
  if (!ok) liveTimeouts[field] = prev;
}

async function onRecordingTtlChange(e: Event): Promise<void> {
  if (e.type === "input") return;
  const raw = (e.target as HTMLInputElement).value.trim();
  const n = validateNonNegInt(raw, "recording_ttl_secs");
  if (n == null) {
    requestUpdate();
    return;
  }
  const prev = liveRecordingTtl;
  liveRecordingTtl = n;
  const ok = await patchRecordingTtl(n);
  if (!ok) liveRecordingTtl = prev;
}

async function onCompressionChange(e: Event): Promise<void> {
  const mode = (e.target as HTMLSelectElement).value;
  if (mode !== "off" && mode !== "lite" && mode !== "rtk" && mode !== "lite_rtk") {
    showToast(t("config.compression.toast.invalid", { mode }), "error");
    return;
  }
  const prev = liveCompression;
  liveCompression = mode;
  const ok = await patchCompression(mode);
  if (!ok) liveCompression = prev;
}

async function onToggleIdleChunkRetryable(): Promise<void> {
  await patchIdleChunkRetryable(!liveIdleChunkRetryable);
}

async function onToggleQuotaProtection(): Promise<void> {
  const nextEnabled = !liveQuotaProtectionEnabled;
  const ok = await patchQuotaProtection(nextEnabled, liveQuotaProtectionThreshold);
  if (ok) liveQuotaProtectionEnabled = nextEnabled;
}

async function onQuotaThresholdChange(e: Event): Promise<void> {
  if (e.type === "input") return;
  const raw = (e.target as HTMLInputElement).value.trim();
  const n = validateNonNegInt(raw, "threshold_percentage");
  if (n == null) {
    requestUpdate();
    return;
  }
  if (n < 1 || n > 99) {
    showToast(t("config.quota.toast.invalid_threshold"), "error");
    requestUpdate();
    return;
  }
  const prev = liveQuotaProtectionThreshold;
  liveQuotaProtectionThreshold = n;
  const ok = await patchQuotaProtection(liveQuotaProtectionEnabled, n);
  if (!ok) liveQuotaProtectionThreshold = prev;
}

// ── Legacy per-section save functions. ──────────────────────────────
//
// These were the four separate save handlers behind the old four-button
// layout. They remain exported because `handlers/registry.ts` imports
// them by name. The live UI routes everything through the per-field
// @change handlers above; these legacy functions are kept functional
// (they read their inputs from the DOM via document.querySelector and
// delegate to the same `patch*` helpers) so the registry entries
// still resolve and so a future caller can re-use them.

function readTimeoutsFromInputs(): TimeoutsState | null {
  const out: TimeoutsState = { ...DEFAULT_TIMEOUTS };
  for (const f of TIMEOUT_FIELDS) {
    const el = document.querySelector(`input[name="timeouts.${f}"]`) as HTMLInputElement | null;
    if (!el) { showToast(`timeouts.${f} input is missing from the DOM`, "error"); return null; }
    const raw = (el.value || "").trim();
    const n = validateNonNegInt(raw, `timeouts.${f}`);
    if (n == null) return null;
    out[f] = n;
  }
  return out;
}

export async function configSaveTimeouts(): Promise<void> {
  const t = readTimeoutsFromInputs();
  if (!t) return;
  const ok = await patchTimeouts(t);
  if (ok) liveTimeouts = { ...t };
}

export async function configSaveRecordingTtl(): Promise<void> {
  const el = document.querySelector('input[name="recording_ttl_secs"]') as HTMLInputElement | null;
  if (!el) { showToast("recording_ttl_secs input missing from DOM", "error"); return; }
  const raw = (el.value || "").trim();
  const n = validateNonNegInt(raw, "recording_ttl_secs");
  if (n == null) return;
  const ok = await patchRecordingTtl(n);
  if (ok) liveRecordingTtl = n;
}

export async function configSaveCompression(): Promise<void> {
  const el = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
  if (!el) { showToast("compression_mode select missing from DOM", "error"); return; }
  const mode = el.value;
  if (mode !== "off" && mode !== "lite" && mode !== "rtk" && mode !== "lite_rtk") {
    showToast(t("config.compression.toast.invalid", { mode }), "error");
    return;
  }
  const ok = await patchCompression(mode);
  if (ok) liveCompression = mode;
}

export async function configSaveIdleChunkRetryable(): Promise<void> {
  const el = document.querySelector('input[name="idle_chunk_retryable"]') as HTMLInputElement | null;
  if (!el) { showToast("idle_chunk_retryable toggle missing from DOM", "error"); return; }
  // The hidden checkbox is kept in sync with `liveIdleChunkRetryable`
  // via `?checked=${...}` in the template, so `el.checked` reflects
  // the current state. We flip it and re-save.
  const val = !el.checked;
  await patchIdleChunkRetryable(val);
}

// ── Card templates ──────────────────────────────────────────────────

export function renderTimeoutsCard(): TemplateResult {
  return card(unsafeHTML(t("config.timeouts.title")), html`
    <p class="muted">${unsafeHTML(t("config.timeouts.description"))}</p>
    <div class="config-grid">
      ${renderField("connect_ms", "timeouts.connect_ms", liveTimeouts.connect_ms, "DNS + TCP connect + TLS handshake (upstream phases: dns, dial, tls).", (e) => { void onTimeoutChange("connect_ms", e); }, { editable: true })}
      ${renderField("request_send_ms", "timeouts.request_send_ms", liveTimeouts.request_send_ms, "Max time to write request headers + body (upstream phase: write).", (e) => { void onTimeoutChange("request_send_ms", e); }, { editable: true })}
      ${renderField("ttft_ms", "timeouts.ttft_ms", liveTimeouts.ttft_ms, "Time-to-first-token: wait for response headers (upstream phase: headers).", (e) => { void onTimeoutChange("ttft_ms", e); }, { editable: true })}
      ${renderField("idle_chunk_ms", "timeouts.idle_chunk_ms", liveTimeouts.idle_chunk_ms, "Max gap between SSE chunks (upstream phase: body).", (e) => { void onTimeoutChange("idle_chunk_ms", e); }, { editable: true })}
      ${renderField("total_ms", "timeouts.total_ms", liveTimeouts.total_ms, "Hard ceiling for the whole request (upstream phase: total → reported as headers).", (e) => { void onTimeoutChange("total_ms", e); }, { editable: true })}
    </div>
  `);
}

export function renderRecordingTtlCard(): TemplateResult {
  return card(unsafeHTML(t("config.recording_ttl.title")), html`
    <p class="muted">${t("config.recording_ttl.description")}</p>
    <div class="config-grid">
      ${renderField("recording_ttl_secs", "recording_ttl_secs", liveRecordingTtl, "TTL in seconds. Use 0 to clear bodies on the next prune tick.", (e) => { void onRecordingTtlChange(e); }, { editable: true, step: 1 })}
    </div>
    <div class="config-actions" style="margin-top: 1rem;">
      <button class="primary" data-action="configSaveRecordingTtl" @click=${configSaveRecordingTtl}>${t("config.recording_ttl.save")}</button>
    </div>
  `);
}

export function renderCompressionCard(): TemplateResult {
  return card(t("config.compression.title"), html`
    <p class="muted">${unsafeHTML(t("config.compression.description"))}</p>
    <div class="config-grid">
      <label class="config-field">
        <span class="config-label">${t("config.compression.mode_label")}</span>
        <select name="compression_mode" aria-label="Compression mode" @change=${onCompressionChange}>
          <option value="off" ?selected=${liveCompression === "off"}>${t("config.compression.off")}</option>
          <option value="lite" ?selected=${liveCompression === "lite"}>${t("config.compression.lite")}</option>
          <option value="rtk" ?selected=${liveCompression === "rtk"}>${t("config.compression.rtk")}</option>
          <option value="lite_rtk" ?selected=${liveCompression === "lite_rtk"}>${t("config.compression.lite_rtk")}</option>
        </select>
        <span class="config-help">${unsafeHTML(t("config.compression.help"))}</span>
      </label>
    </div>
  `);
}

export function renderIdleChunkCard(): TemplateResult {
  return card(t("config.idle_chunk.title"), html`
    <p class="muted">${t("config.idle_chunk.description")}</p>
    <div class="config-grid">
      <label class="config-field">
        <span class="config-label">${t("config.idle_chunk.label")}</span>
        <button type="button" role="switch" aria-checked=${liveIdleChunkRetryable ? "true" : "false"}
                class="toggle-btn ${liveIdleChunkRetryable ? "on" : "off"}"
                @click=${() => { void onToggleIdleChunkRetryable(); }}>
          <span class="toggle-thumb"></span>
        </button>
        <input type="checkbox" name="idle_chunk_retryable" ?checked=${liveIdleChunkRetryable} class="sr-only">
        <span class="config-help">${liveIdleChunkRetryable
          ? t("config.idle_chunk.help_on")
          : t("config.idle_chunk.help_off")}</span>
      </label>
    </div>
  `);
}

export function renderQuotaCard(): TemplateResult {
  return card(t("config.quota.title"), html`
    <p class="muted">${t("config.quota.description")}</p>
    <div class="config-grid">
      <label class="config-field">
        <span class="config-label">${t("config.quota.enabled")}</span>
        <button type="button" role="switch" aria-checked=${liveQuotaProtectionEnabled ? "true" : "false"}
                class="toggle-btn ${liveQuotaProtectionEnabled ? "on" : "off"}"
                @click=${() => { void onToggleQuotaProtection(); }}>
          <span class="toggle-thumb"></span>
        </button>
        <span class="config-help">${liveQuotaProtectionEnabled
          ? t("config.quota.help_on")
          : t("config.quota.help_off")}</span>
      </label>
      <label class="config-field">
        <span class="config-label">${t("config.quota.reserve_threshold")}</span>
        <input type="number" min="1" max="99" name="quota_protection.threshold_percentage" .value=${String(liveQuotaProtectionThreshold)}
               @change=${onQuotaThresholdChange} @input=${onQuotaThresholdChange}>
        <span class="config-help">${t("config.quota.help_threshold")}</span>
      </label>
    </div>
  `);
}
