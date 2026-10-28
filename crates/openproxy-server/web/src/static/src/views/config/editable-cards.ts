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

let liveTimeouts: TimeoutsState = { ...DEFAULT_TIMEOUTS };
let liveRecordingTtl = 300;
let liveCompression = "off";
let liveIdleChunkRetryable = false;
let liveQuotaProtectionEnabled = true;
let liveQuotaProtectionThreshold = 10;
let livePiiEnabled = false;
let livePiiReversible = true;
let livePiiRedactLogs = true;
let livePiiEntities: string[] = ["email", "phone", "ip", "credit_card", "secret", "person"];
let liveNotificationsEnabled = true;
let liveModalityRoutingEnabled = true;
let liveModalityRoutingFallback = true;

function normalizePiiEntity(e: string): string {
  if (e === "card") return "credit_card";
  if (e === "name") return "person";
  if (e === "key" || e === "api_key") return "secret";
  if (e === "ipv4" || e === "ipv6") return "ip";
  return e;
}

export function applyServerConfig(payload: ConfigPayload): void {
  const tm = payload.timeouts || {};
  liveTimeouts = { connect_ms: tm.connect_ms ?? 0, request_send_ms: tm.request_send_ms ?? 0, ttft_ms: tm.ttft_ms ?? 0, idle_chunk_ms: tm.idle_chunk_ms ?? 0, total_ms: tm.total_ms ?? 0 };
  liveRecordingTtl = payload.recording_ttl_secs ?? 300;
  liveCompression = payload.compression ?? "off";
  liveIdleChunkRetryable = payload.idle_chunk_retryable ?? false;
  liveQuotaProtectionEnabled = payload.quota_protection?.enabled ?? true;
  liveQuotaProtectionThreshold = payload.quota_protection?.threshold_percentage ?? 10;
  const p = payload.pii ?? { pii_enabled: payload.pii_enabled, pii_reversible: payload.pii_reversible, pii_redact_logs: payload.pii_redact_logs, pii_entities: payload.pii_entities };
  livePiiEnabled = p.pii_enabled ?? false;
  livePiiReversible = p.pii_reversible ?? true;
  livePiiRedactLogs = p.pii_redact_logs ?? true;
  livePiiEntities = Array.from(new Set((p.pii_entities ?? ["email", "phone", "ip", "credit_card", "secret", "person"]).map(normalizePiiEntity)));
  liveNotificationsEnabled = payload.notifications_enabled ?? true;
  liveModalityRoutingEnabled = payload.modality_routing?.enabled ?? true;
  liveModalityRoutingFallback = payload.modality_routing?.fallback_to_text_only ?? true;
}

async function putConfig(endpoint: string, body: unknown, toastMsg: string): Promise<boolean> {
  try {
    await api(endpoint, { method: "PUT", body: JSON.stringify(body) });
    showToast(toastMsg, "success");
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast(t("config.toast.error", { message: errStr(e) }), "error");
    requestUpdate();
    return false;
  }
}

async function patchTimeouts(values: Record<TimeoutKey, number>): Promise<boolean> {
  const ok = await putConfig("/config/timeouts", values, t("config.timeouts.toast.updated"));
  if (ok) {
    const cfg = getConfig(); if (cfg?.timeouts) Object.assign(cfg.timeouts, values);
    setBanner("success", t("config.banner.live_applies"), t("config.banner.live_applies_body"));
  }
  return ok;
}

async function patchRecordingTtl(val: number): Promise<boolean> {
  const ok = await putConfig("/config/recording-ttl", { recording_ttl_secs: val }, t("config.recording_ttl.toast.set", { value: val }));
  if (ok) { const cfg = getConfig(); if (cfg) cfg.recording_ttl_secs = val; }
  return ok;
}

async function patchCompression(mode: string): Promise<boolean> {
  const ok = await putConfig("/config/compression", mode, t("config.compression.toast.set", { mode }));
  if (ok) { const cfg = getConfig(); if (cfg) cfg.compression = mode; }
  return ok;
}

async function patchIdleChunkRetryable(val: boolean): Promise<boolean> {
  const prev = liveIdleChunkRetryable; liveIdleChunkRetryable = val; requestUpdate();
  const ok = await putConfig("/config/idle-chunk-retryable", { idle_chunk_retryable: val }, t("config.idle_chunk.toast.set", { value: String(val) }));
  if (ok) { const cfg = getConfig(); if (cfg) cfg.idle_chunk_retryable = val; } else liveIdleChunkRetryable = prev;
  return ok;
}

async function patchQuotaProtection(enabled: boolean, threshold: number): Promise<boolean> {
  const ok = await putConfig("/config/quota-protection", { enabled, threshold_percentage: threshold }, t("config.quota.toast.updated"));
  if (ok) { const cfg = getConfig(); if (cfg) cfg.quota_protection = { enabled, threshold_percentage: threshold }; }
  return ok;
}

async function patchModalityRouting(enabled: boolean, fallback: boolean): Promise<boolean> {
  const ok = await putConfig("/config/modality-routing", { enabled, fallback_to_text_only: fallback }, t("config.modality.toast.updated"));
  if (ok) { const cfg = getConfig(); if (cfg) cfg.modality_routing = { enabled, fallback_to_text_only: fallback }; }
  return ok;
}

async function patchPiiConfig(update: { pii_enabled?: boolean; pii_reversible?: boolean; pii_redact_logs?: boolean; pii_entities?: string[] }): Promise<boolean> {
  const body = { pii_enabled: update.pii_enabled ?? livePiiEnabled, pii_reversible: update.pii_reversible ?? livePiiReversible, pii_redact_logs: update.pii_redact_logs ?? livePiiRedactLogs, pii_entities: update.pii_entities ?? livePiiEntities };
  const ok = await putConfig("/config/pii", body, t("config.pii.toast.updated"));
  if (ok) { const cfg = getConfig(); if (cfg) { cfg.pii = body; Object.assign(cfg, body); } }
  return ok;
}

async function patchNotificationsEnabled(enabled: boolean): Promise<boolean> {
  const prev = liveNotificationsEnabled; liveNotificationsEnabled = enabled; requestUpdate();
  const ok = await putConfig("/config/notifications-enabled", { notifications_enabled: enabled }, t("config.notifications.toast.set", { value: String(enabled) }));
  if (ok) { const cfg = getConfig(); if (cfg) cfg.notifications_enabled = enabled; } else liveNotificationsEnabled = prev;
  return ok;
}

async function onTimeoutChange(field: TimeoutKey, e: Event): Promise<void> {
  if (e.type === "input") return;
  const n = validateNonNegInt((e.target as HTMLInputElement).value.trim(), `timeouts.${field}`);
  if (n == null) { requestUpdate(); return; }
  const prev = liveTimeouts[field]; liveTimeouts[field] = n;
  if (!await patchTimeouts(liveTimeouts)) liveTimeouts[field] = prev;
}

async function onRecordingTtlChange(e: Event): Promise<void> {
  if (e.type === "input") return;
  const n = validateNonNegInt((e.target as HTMLInputElement).value.trim(), "recording_ttl_secs");
  if (n == null) { requestUpdate(); return; }
  const prev = liveRecordingTtl; liveRecordingTtl = n;
  if (!await patchRecordingTtl(n)) liveRecordingTtl = prev;
}

async function onCompressionChange(e: Event): Promise<void> {
  const mode = (e.target as HTMLSelectElement).value;
  if (!["off", "lite", "rtk", "lite_rtk"].includes(mode)) { showToast(t("config.compression.toast.invalid", { mode }), "error"); return; }
  const prev = liveCompression; liveCompression = mode;
  if (!await patchCompression(mode)) liveCompression = prev;
}

export function configSaveTimeouts(): Promise<void> {
  const out: TimeoutsState = { ...DEFAULT_TIMEOUTS };
  for (const f of TIMEOUT_FIELDS) {
    const el = document.querySelector(`input[name="timeouts.${f}"]`) as HTMLInputElement | null;
    const n = el ? validateNonNegInt(el.value.trim(), `timeouts.${f}`) : null;
    if (n == null) return Promise.resolve();
    out[f] = n;
  }
  return patchTimeouts(out).then((ok) => { if (ok) liveTimeouts = { ...out }; });
}

export function configSaveRecordingTtl(): Promise<void> {
  const el = document.querySelector('input[name="recording_ttl_secs"]') as HTMLInputElement | null;
  const n = el ? validateNonNegInt(el.value.trim(), "recording_ttl_secs") : null;
  return n != null ? patchRecordingTtl(n).then((ok) => { if (ok) liveRecordingTtl = n; }) : Promise.resolve();
}

export function configSaveCompression(): Promise<void> {
  const el = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
  const mode = el?.value;
  return mode && ["off", "lite", "rtk", "lite_rtk"].includes(mode) ? patchCompression(mode).then((ok) => { if (ok) liveCompression = mode; }) : Promise.resolve();
}

export function configSaveIdleChunkRetryable(): Promise<void> {
  const el = document.querySelector('input[name="idle_chunk_retryable"]') as HTMLInputElement | null;
  return patchIdleChunkRetryable(!el?.checked).then(() => {});
}

function renderToggle(label: string, val: boolean, helpOn: string, helpOff: string, onToggle: () => void, inputName?: string): TemplateResult {
  return html`<div class="config-toggle-row">
    <div class="config-toggle-info">
      <span class="config-label">${label}</span>
      <span class="config-help">${val ? helpOn : helpOff}</span>
    </div>
    <button type="button" role="switch" aria-checked=${val ? "true" : "false"}
      class="toggle-btn ${val ? "on" : "off"}"
      data-action=${inputName === "idle_chunk_retryable" ? "toggleIdleChunkRetryable" : ""}
      @click=${onToggle}>
      <span class="toggle-thumb"></span>
    </button>
    ${inputName ? html`<input type="checkbox" name=${inputName} ?checked=${val} class="sr-only">` : ""}
  </div>`;
}

export function renderTimeoutsCard(): TemplateResult {
  return card(unsafeHTML(t("config.timeouts.title")), html`
    <p class="muted">${unsafeHTML(t("config.timeouts.description"))}</p>
    <div class="config-grid config-timeouts-grid">
      ${renderField("connect_ms", "timeouts.connect_ms", liveTimeouts.connect_ms, "DNS + TCP connect + TLS handshake.", (e) => void onTimeoutChange("connect_ms", e), { editable: true, unit: "ms" })}
      ${renderField("request_send_ms", "timeouts.request_send_ms", liveTimeouts.request_send_ms, "Max time to write request headers + body.", (e) => void onTimeoutChange("request_send_ms", e), { editable: true, unit: "ms" })}
      ${renderField("ttft_ms", "timeouts.ttft_ms", liveTimeouts.ttft_ms, "Time-to-first-token: wait for response headers.", (e) => void onTimeoutChange("ttft_ms", e), { editable: true, unit: "ms" })}
      ${renderField("idle_chunk_ms", "timeouts.idle_chunk_ms", liveTimeouts.idle_chunk_ms, "Max gap between SSE chunks.", (e) => void onTimeoutChange("idle_chunk_ms", e), { editable: true, unit: "ms" })}
      ${renderField("total_ms", "timeouts.total_ms", liveTimeouts.total_ms, "Hard ceiling for the whole request.", (e) => void onTimeoutChange("total_ms", e), { editable: true, unit: "ms" })}
    </div>
  `, "card-timeouts-full");
}

export function renderRecordingTtlCard(): TemplateResult {
  return card(unsafeHTML(t("config.recording_ttl.title")), html`
    <p class="muted">${t("config.recording_ttl.description")}</p>
    <div class="config-inline-setting">
      <div class="config-inline-info">
        <span class="config-label">recording_ttl_secs</span>
        <span class="config-help">TTL in seconds for debug traces.</span>
      </div>
      <div class="config-inline-action">
        <div class="config-input-group has-unit">
          <input type="number" inputmode="numeric" name="recording_ttl_secs" min="0" step="1"
            .value=${String(liveRecordingTtl)}
            aria-label="recording_ttl_secs"
            @change=${(e: Event) => void onRecordingTtlChange(e)}
            @input=${(e: Event) => void onRecordingTtlChange(e)}>
          <span class="config-input-unit">s</span>
        </div>
        <button class="primary" data-action="configSaveRecordingTtl" @click=${configSaveRecordingTtl}>
          ${t("config.recording_ttl.save")}
        </button>
      </div>
    </div>
  `);
}

export function renderCompressionCard(): TemplateResult {
  return card(t("config.compression.title"), html`
    <p class="muted">${unsafeHTML(t("config.compression.description"))}</p>
    <div class="config-inline-setting">
      <div class="config-inline-info">
        <span class="config-label">${t("config.compression.mode_label")}</span>
        <span class="config-help">${unsafeHTML(t("config.compression.help"))}</span>
      </div>
      <div class="config-inline-action">
        <select name="compression_mode" class="config-select" aria-label="Compression mode" @change=${onCompressionChange}>
          ${["off", "lite", "rtk", "lite_rtk"].map((m) => html`<option value=${m} ?selected=${liveCompression === m}>${t("config.compression." + m)}</option>`)}
        </select>
      </div>
    </div>
  `);
}

export function renderIdleChunkCard(): TemplateResult {
  return card(t("config.idle_chunk.title"), html`
    <p class="muted">${t("config.idle_chunk.description")}</p>
    ${renderToggle(t("config.idle_chunk.label"), liveIdleChunkRetryable, t("config.idle_chunk.help_on"), t("config.idle_chunk.help_off"), () => void patchIdleChunkRetryable(!liveIdleChunkRetryable), "idle_chunk_retryable")}
  `);
}

export function renderNotificationsCard(): TemplateResult {
  return card(t("config.notifications.title"), html`
    <p class="muted">${t("config.notifications.description")}</p>
    ${renderToggle(t("config.notifications.enabled"), liveNotificationsEnabled, t("config.notifications.help_on"), t("config.notifications.help_off"), () => void patchNotificationsEnabled(!liveNotificationsEnabled))}
  `);
}

export function renderModalityCard(): TemplateResult {
  return card(t("config.modality.title"), html`
    <p class="muted">${t("config.modality.description")}</p>
    <div class="config-rows-list">
      ${renderToggle(t("config.modality.enabled"), liveModalityRoutingEnabled, t("config.modality.help_on"), t("config.modality.help_off"), async () => {
        const next = !liveModalityRoutingEnabled;
        if (await patchModalityRouting(next, liveModalityRoutingFallback)) liveModalityRoutingEnabled = next;
      })}
      ${renderToggle(t("config.modality.fallback"), liveModalityRoutingFallback, t("config.modality.help_fallback_on"), t("config.modality.help_fallback_off"), async () => {
        const next = !liveModalityRoutingFallback;
        if (await patchModalityRouting(liveModalityRoutingEnabled, next)) liveModalityRoutingFallback = next;
      })}
    </div>
  `);
}

export function renderQuotaCard(): TemplateResult {
  return card(t("config.quota.title"), html`
    <p class="muted">${t("config.quota.description")}</p>
    <div class="config-rows-list">
      ${renderToggle(t("config.quota.enabled"), liveQuotaProtectionEnabled, t("config.quota.help_on"), t("config.quota.help_off"), async () => {
        const next = !liveQuotaProtectionEnabled;
        if (await patchQuotaProtection(next, liveQuotaProtectionThreshold)) liveQuotaProtectionEnabled = next;
      })}
      <div class="config-inline-setting">
        <div class="config-inline-info">
          <span class="config-label">${t("config.quota.reserve_threshold")}</span>
          <span class="config-help">${t("config.quota.help_threshold")}</span>
        </div>
        <div class="config-inline-action">
          <div class="config-input-group has-unit">
            <input type="number" min="1" max="99" name="quota_protection.threshold_percentage"
              .value=${String(liveQuotaProtectionThreshold)}
              aria-label=${t("config.quota.reserve_threshold")}
              @change=${async (e: Event) => {
                const n = validateNonNegInt((e.target as HTMLInputElement).value.trim(), "threshold_percentage");
                if (n == null || n < 1 || n > 99) { requestUpdate(); return; }
                const prev = liveQuotaProtectionThreshold; liveQuotaProtectionThreshold = n;
                if (!await patchQuotaProtection(liveQuotaProtectionEnabled, n)) liveQuotaProtectionThreshold = prev;
              }}>
            <span class="config-input-unit">%</span>
          </div>
        </div>
      </div>
    </div>
  `);
}

const ALL_PII = [
  { id: "email", label: "Email" },
  { id: "phone", label: "Phone" },
  { id: "ip", label: "IP Address" },
  { id: "credit_card", label: "Credit Card (Luhn)" },
  { id: "secret", label: "Secrets / Keys" },
  { id: "person", label: "Person Names" },
];

export function renderPiiCard(): TemplateResult {
  return card(t("config.pii.title"), html`
    <p class="muted">${t("config.pii.description")}</p>
    <div class="config-rows-list">
      ${renderToggle(t("config.pii.enabled"), livePiiEnabled, t("config.pii.help_on"), t("config.pii.help_off"), async () => {
        const p = livePiiEnabled; livePiiEnabled = !p; if (!await patchPiiConfig({ pii_enabled: livePiiEnabled })) livePiiEnabled = p;
      })}
      ${renderToggle(t("config.pii.reversible"), livePiiReversible, t("config.pii.help_reversible_on"), t("config.pii.help_reversible_off"), async () => {
        const p = livePiiReversible; livePiiReversible = !p; if (!await patchPiiConfig({ pii_reversible: livePiiReversible })) livePiiReversible = p;
      })}
      ${renderToggle(t("config.pii.redact_logs"), livePiiRedactLogs, t("config.pii.help_redact_logs_on"), t("config.pii.help_redact_logs_off"), async () => {
        const p = livePiiRedactLogs; livePiiRedactLogs = !p; if (!await patchPiiConfig({ pii_redact_logs: livePiiRedactLogs })) livePiiRedactLogs = p;
      })}
    </div>
    <div class="config-entities-section">
      <span class="config-label">${t("config.pii.entities_label")}</span>
      <div class="config-entities-chips">
        ${ALL_PII.map((ent) => {
          const active = livePiiEntities.includes(ent.id);
          return html`<button type="button" class="config-entity-chip ${active ? "active" : ""}"
            aria-pressed=${active ? "true" : "false"}
            @click=${async () => {
              const norm = normalizePiiEntity(ent.id); const prev = [...livePiiEntities];
              const idx = livePiiEntities.indexOf(norm);
              if (idx >= 0) livePiiEntities.splice(idx, 1); else livePiiEntities.push(norm);
              livePiiEntities = Array.from(new Set(livePiiEntities.map(normalizePiiEntity)));
              if (!await patchPiiConfig({ pii_entities: livePiiEntities })) livePiiEntities = prev;
            }}>
            <span class="chip-symbol">${active ? "✓" : "+"}</span>
            <span>${ent.label}</span>
          </button>`;
        })}
      </div>
    </div>
  `);
}
