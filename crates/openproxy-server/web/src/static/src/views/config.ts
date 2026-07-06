// views/config.ts — config editor. MIGRATED to lit-html.
//
// The four editable sections (timeouts, recording TTL, compression,
// idle-chunk-retryable) each map to their own PUT endpoint and
// auto-save on change. A small banner at the top reflects the
// last-save status. The other three sections (retries,
// circuit_breaker, racing) reflect the loaded `config.toml` and are
// not editable from the dashboard — they live in a collapsed
// `<details class="config-static-region">` as plain mono-font text.
//
// The four legacy per-section save functions
// (`configSaveTimeouts`, `configSaveRecordingTtl`,
// `configSaveCompression`, `configSaveIdleChunkRetryable`) are kept
// and exported because `handlers/registry.ts` imports them by name.
// They remain functional (each saves just its own section) by
// delegating to the same internal `patch*` helpers the new view
// uses, but they read their inputs from the DOM via
// `document.querySelector` so the registry contract (called with no
// args) keeps working.
//
// See views/combos.ts for the lit-html migration reference pattern.

import { html, type TemplateResult } from "lit-html";
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";

interface FieldOpts {
  editable?: boolean;
  step?: number;
}

// Shape of the /admin/config response. The server flattens the
// four sections (timeouts, retries, circuit_breaker, racing) into
// a single object; each section is optional so a partial payload
// (e.g. on a transient error) doesn't crash the render.
interface ConfigPayload {
  timeouts?: {
    connect_ms?: number | null;
    request_send_ms?: number | null;
    ttft_ms?: number | null;
    idle_chunk_ms?: number | null;
    total_ms?: number | null;
  };
  retries?: {
    max_attempts?: number | null;
    backoff_base_ms?: number | null;
    backoff_factor?: number | null;
    backoff_jitter_pct?: number | null;
    combo_max_attempts?: number | null;
  };
  circuit_breaker?: {
    failure_threshold?: number | null;
    unhealthy_duration_ms?: number | null;
  };
  racing?: {
    default_race_size?: number | null;
    max_race_size?: number | null;
    abort_grace_ms?: number | null;
  };
  recording_ttl_secs?: number | null;
  /** "off" | "lite" | "rtk" | "lite_rtk" */
  compression?: string | null;
  /** When true, idle_chunk timeouts are treated as retryable. */
  idle_chunk_retryable?: boolean | null;
  quota_protection?: {
    enabled?: boolean | null;
    threshold_percentage?: number | null;
  } | null;
  /** Maintenance config (auto_vacuum, interval, retention). */
  maintenance?: {
    auto_vacuum?: boolean | null;
    vacuum_interval_hours?: number | null;
    usage_retention_days?: number | null;
    vacuum_status?: {
      last_run?: string | null;
      last_result?: string | null;
      in_progress?: boolean | null;
      next_scheduled?: string | null;
    } | null;
  } | null;
}

type TimeoutKey = "connect_ms" | "request_send_ms" | "ttft_ms" | "idle_chunk_ms" | "total_ms";
const TIMEOUT_FIELDS: readonly TimeoutKey[] = ["connect_ms", "request_send_ms", "ttft_ms", "idle_chunk_ms", "total_ms"] as const;

const DEFAULT_TIMEOUTS: Record<TimeoutKey, number> = {
  connect_ms: 0,
  request_send_ms: 0,
  ttft_ms: 0,
  idle_chunk_ms: 0,
  total_ms: 0,
};

// ---- View state ----

let loading = true;
let errorMsg: string | null = null;
let cfg: ConfigPayload | null = null;

// Live editable values (what the user sees in the inputs). The
// `patch*` helpers update these optimistically and revert on
// failure. The render function binds the inputs to these values
// via `.value=${...}` so a revert is reflected immediately.
let liveTimeouts: Record<TimeoutKey, number> = { ...DEFAULT_TIMEOUTS };
let liveRecordingTtl = 300;
let liveCompression = "off";
let liveIdleChunkRetryable = false;
let liveQuotaProtectionEnabled = true;
let liveQuotaProtectionThreshold = 10;
// Maintenance / VACUUM state
let liveAutoVacuum = true;
let liveVacuumIntervalHours = 6;
let liveUsageRetentionDays = 7;
let vacuumStatus: { last_run: string | null; last_result: string | null; in_progress: boolean; next_scheduled: string | null } = {
  last_run: null, last_result: null, in_progress: false, next_scheduled: null,
};
let vacuumPollHandle: ReturnType<typeof setInterval> | null = null;

// Banner state. Set by the patch helpers after each save.
let bannerKind: "info" | "success" = "info";
let bannerTitle = "Live values.";
let bannerBody = "The values below are the ones the server is currently using. Timeouts, Recording TTL, Compression, and the Idle Chunk Retryable flag are editable; the other sections reflect the loaded config.toml. Changes are persisted in the database and apply to the next request (timeouts) or the next prune tick (Recording TTL).";

// ---- Helpers ----

/** Pull the human-readable `message` field out of the JSON envelope
 *  produced by the server's `ApiError` impl. The thrower is `api()`,
 *  which raises `new Error("<status>: <body>")`; the JSON body lives
 *  as a string suffix on `e.message`, and we re-parse it here. */
function errStr(e: unknown): string {
  if (!(e instanceof Error)) return String(e);
  const m = e.message.match(/"error"\s*:\s*\{[\s\S]*?"message"\s*:\s*"((?:[^"\\]|\\.)*)"/);
  if (m) {
    try { return JSON.parse('"' + (m[1] ?? "") + '"') as string; }
    catch (_err: unknown) { return m[1] ?? e.message; }
  }
  return e.message;
}

function validateNonNegInt(raw: string, fieldName: string): number | null {
  if (raw === "") { showToast(`${fieldName} is required`, "error"); return null; }
  if (!/^\d+$/.test(raw)) { showToast(`${fieldName} must be a non-negative integer`, "error"); return null; }
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0) { showToast(`${fieldName} must be a non-negative integer`, "error"); return null; }
  return n;
}

function setBanner(kind: "info" | "success", title: string, body: string): void {
  bannerKind = kind;
  bannerTitle = title;
  bannerBody = body;
}

// ---- Per-section save helpers (used by both the @change handlers
//      and the four legacy exported functions). ----

async function patchTimeouts(values: Record<TimeoutKey, number>): Promise<boolean> {
  try {
    await api("/config/timeouts", { method: "PUT", body: JSON.stringify(values) });
    if (cfg?.timeouts) Object.assign(cfg.timeouts, values);
    showToast("Timeouts updated — applies to next requests", "success");
    setBanner("success", "Live — applies to next requests",
      "The values below are persisted in the database and will take effect on the next request. Requests already in flight continue with the previous values.");
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast("Error: " + errStr(e), "error");
    requestUpdate();
    return false;
  }
}

async function patchRecordingTtl(value: number): Promise<boolean> {
  try {
    await api("/config/recording-ttl", { method: "PUT", body: JSON.stringify({ recording_ttl_secs: value }) });
    if (cfg) cfg.recording_ttl_secs = value;
    showToast(`Recording TTL set to ${value}s — applies on next prune tick`, "success");
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast("Error: " + errStr(e), "error");
    requestUpdate();
    return false;
  }
}

async function patchCompression(mode: string): Promise<boolean> {
  try {
    await api("/config/compression", { method: "PUT", body: JSON.stringify(mode) });
    if (cfg) cfg.compression = mode;
    showToast(`Compression mode set to ${mode} — applies to next requests`, "success");
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast("Error: " + errStr(e), "error");
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
    if (cfg) cfg.idle_chunk_retryable = val;
    showToast(`Idle chunk retryable set to ${val} — applies to next requests`, "success");
    return true;
  } catch (e: unknown) {
    liveIdleChunkRetryable = prev; // revert
    showToast("Error: " + errStr(e), "error");
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
    if (cfg) {
      cfg.quota_protection = { enabled, threshold_percentage: threshold };
    }
    showToast("Quota protection updated", "success");
    requestUpdate();
    return true;
  } catch (e: unknown) {
    showToast("Error: " + errStr(e), "error");
    requestUpdate();
    return false;
  }
}

// ---- Maintenance / VACUUM ----

async function patchMaintenance(): Promise<void> {
  try {
    await api("/config/maintenance", {
      method: "PUT",
      body: JSON.stringify({
        auto_vacuum: liveAutoVacuum,
        vacuum_interval_hours: liveVacuumIntervalHours,
        usage_retention_days: liveUsageRetentionDays,
      }),
    });
    showToast("Maintenance config updated", "success");
    requestUpdate();
  } catch (e: unknown) {
    showToast("Error: " + errStr(e), "error");
  }
}

async function triggerVacuum(): Promise<void> {
  if (vacuumStatus.in_progress) return;
  vacuumStatus.in_progress = true;
  requestUpdate();
  try {
    const result = await api("/debug/vacuum", { method: "POST" }) as { vacuumed?: boolean; partial?: boolean; integrity_check?: string; message?: string };
    if (result.partial) {
      showToast("VACUUM partial: " + (result.message || "see details"), "warning");
    } else {
      showToast("VACUUM completed successfully", "success");
    }
  } catch (e: unknown) {
    // VACUUM failed — the error message includes repair instructions
    // if the DB is corrupt. Show it as a toast and also try the
    // recover endpoint for diagnostics.
    const errMsg = errStr(e);
    showToast("VACUUM failed: " + errMsg, "error");
    // If the error mentions "disk I/O" or "integrity", auto-trigger
    // the recover diagnostic so the operator sees the repair instructions.
    if (errMsg.includes("disk I/O") || errMsg.includes("integrity")) {
      try {
        const recovery = await api("/debug/recover", { method: "POST" }) as { instructions?: string; tables?: unknown[]; needs_manual_repair?: boolean };
        if (recovery.needs_manual_repair && recovery.instructions) {
          // Show the repair instructions in a more prominent way —
          // a longer-lived toast with the full instructions.
          showToast("DB needs manual repair. Check console for instructions.", "error");
          console.error("=== DATABASE REPAIR INSTRUCTIONS ===\n" + recovery.instructions + "\n=== END INSTRUCTIONS ===");
        }
      } catch {
        // Recovery endpoint also failed — non-fatal, the operator
        // already has the VACUUM error message.
      }
    }
  } finally {
    await pollVacuumStatus();
  }
}

async function pollVacuumStatus(): Promise<void> {
  try {
    const data = await api("/config/vacuum-status") as { last_run?: string | null; last_result?: string | null; in_progress?: boolean; next_scheduled?: string | null };
    vacuumStatus = {
      last_run: data.last_run ?? null,
      last_result: data.last_result ?? null,
      in_progress: data.in_progress ?? false,
      next_scheduled: data.next_scheduled ?? null,
    };
    requestUpdate();
  } catch {
    // Non-fatal — the status will update on the next poll.
  }
}

// ---- @change / @click handlers (bound from the lit-html template) ----

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
    showToast(`Invalid compression mode: ${mode}`, "error");
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
    showToast("Threshold percentage must be between 1 and 99", "error");
    requestUpdate();
    return;
  }
  const prev = liveQuotaProtectionThreshold;
  liveQuotaProtectionThreshold = n;
  const ok = await patchQuotaProtection(liveQuotaProtectionEnabled, n);
  if (!ok) liveQuotaProtectionThreshold = prev;
}

// ---------------------------------------------------------------------------
// Legacy per-section save functions.
//
// These were the four separate save handlers behind the old four-button
// layout. They remain exported because `handlers/registry.ts` imports
// them by name. The new UI routes everything through the per-field
// @change handlers above; these legacy functions are kept functional
// (they read their inputs from the DOM via document.querySelector and
// delegate to the same `patch*` helpers) so the registry entries
// still resolve and so a future caller can re-use them.
// ---------------------------------------------------------------------------

function readTimeoutsFromInputs(): Record<TimeoutKey, number> | null {
  const out: Partial<Record<TimeoutKey, number>> = {};
  for (const f of TIMEOUT_FIELDS) {
    const el = document.querySelector(`input[name="timeouts.${f}"]`) as HTMLInputElement | null;
    if (!el) { showToast(`timeouts.${f} input is missing from the DOM`, "error"); return null; }
    const raw = (el.value || "").trim();
    const n = validateNonNegInt(raw, `timeouts.${f}`);
    if (n == null) return null;
    out[f] = n;
  }
  return out as Record<TimeoutKey, number>;
}

async function configSaveTimeouts(): Promise<void> {
  const t = readTimeoutsFromInputs();
  if (!t) return;
  const ok = await patchTimeouts(t);
  if (ok) liveTimeouts = { ...t };
}

async function configSaveRecordingTtl(): Promise<void> {
  const el = document.querySelector('input[name="recording_ttl_secs"]') as HTMLInputElement | null;
  if (!el) { showToast("recording_ttl_secs input missing from DOM", "error"); return; }
  const raw = (el.value || "").trim();
  const n = validateNonNegInt(raw, "recording_ttl_secs");
  if (n == null) return;
  const ok = await patchRecordingTtl(n);
  if (ok) liveRecordingTtl = n;
}

async function configSaveCompression(): Promise<void> {
  const el = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
  if (!el) { showToast("compression_mode select missing from DOM", "error"); return; }
  const mode = el.value;
  if (mode !== "off" && mode !== "lite" && mode !== "rtk" && mode !== "lite_rtk") {
    showToast(`Invalid compression mode: ${mode}`, "error");
    return;
  }
  const ok = await patchCompression(mode);
  if (ok) liveCompression = mode;
}

async function configSaveIdleChunkRetryable(): Promise<void> {
  const el = document.querySelector('input[name="idle_chunk_retryable"]') as HTMLInputElement | null;
  if (!el) { showToast("idle_chunk_retryable toggle missing from DOM", "error"); return; }
  // The hidden checkbox is kept in sync with `liveIdleChunkRetryable`
  // via `?checked=${...}` in the template, so `el.checked` reflects
  // the current state. We flip it and re-save.
  const val = !el.checked;
  await patchIdleChunkRetryable(val);
}

export { configSaveTimeouts, configSaveRecordingTtl, configSaveCompression, configSaveIdleChunkRetryable };

// ---- Templates ----

function renderField(
  label: string,
  name: string,
  value: number,
  help: string,
  onChange: (e: Event) => void,
  opts: FieldOpts = {},
): TemplateResult {
  return html`<label class="config-field">
    <span class="config-label">${label}</span>
    <input type="number" name=${name} .value=${String(value)} min="0" step=${opts.step ?? 100}
      ?disabled=${!opts.editable}
      aria-label=${label + (opts.editable ? "" : " (read-only)")}
      @change=${onChange} @input=${onChange}>
    <span class="config-help">${help}</span>
  </label>`;
}

/** Render a read-only key/value pair for the static region. Uses
 *  `.config-static-display .field` so the existing CSS gives us the
 *  uppercase muted label + mono-font value look. */
function renderStaticField(label: string, value: number | null | undefined): TemplateResult {
  const display: string = (value === null || value === undefined) ? "—" : String(value);
  return html`<div class="field"><span class="label">${label}</span><span class="value">${display}</span></div>`;
}

function card(title: string | TemplateResult, body: TemplateResult): TemplateResult {
  return html`<section class="card"><div class="section-header"><h3>${title}</h3></div>${body}</section>`;
}

function renderConfig(): TemplateResult {
  if (loading) {
    return html`<div class="page-header"><h2>Config</h2></div>
      <div class="loading">Loading...</div>`;
  }
  if (errorMsg) {
    return html`<div class="page-header"><h2>Config</h2></div>
      <div class="banner banner-error">${errorMsg}</div>`;
  }
  if (!cfg) {
    return html`<div class="page-header"><h2>Config</h2></div>
      <div class="loading">Loading...</div>`;
  }

  const r = cfg.retries || {};
  const cb = cfg.circuit_breaker || {};
  const rc = cfg.racing || {};

  const timeoutsCard = card(html`Timeouts <small>(ms)</small>`, html`
    <p class="muted">Precedence (highest wins): <code>model overrides</code> → <code>system default</code>. These values are the single source of truth for <code>connect</code>, <code>request_send</code>, and <code>total</code> across all providers. Per-model overrides (set on each model row) only affect <code>ttft</code> and <code>idle_chunk</code>. Editing these values takes effect on the next request; in-flight requests keep the previous value.</p>
    <div class="config-grid">
      ${renderField("connect_ms", "timeouts.connect_ms", liveTimeouts.connect_ms, "DNS + TCP connect + TLS handshake (upstream phases: dns, dial, tls).", (e) => { void onTimeoutChange("connect_ms", e); }, { editable: true })}
      ${renderField("request_send_ms", "timeouts.request_send_ms", liveTimeouts.request_send_ms, "Max time to write request headers + body (upstream phase: write).", (e) => { void onTimeoutChange("request_send_ms", e); }, { editable: true })}
      ${renderField("ttft_ms", "timeouts.ttft_ms", liveTimeouts.ttft_ms, "Time-to-first-token: wait for response headers (upstream phase: headers).", (e) => { void onTimeoutChange("ttft_ms", e); }, { editable: true })}
      ${renderField("idle_chunk_ms", "timeouts.idle_chunk_ms", liveTimeouts.idle_chunk_ms, "Max gap between SSE chunks (upstream phase: body).", (e) => { void onTimeoutChange("idle_chunk_ms", e); }, { editable: true })}
      ${renderField("total_ms", "timeouts.total_ms", liveTimeouts.total_ms, "Hard ceiling for the whole request (upstream phase: total → reported as headers).", (e) => { void onTimeoutChange("total_ms", e); }, { editable: true })}
    </div>
  `);

  const recordingTtlCard = card(html`Recording TTL <small>(seconds)</small>`, html`
    <p class="muted">How long recorded request/response bodies and headers stay in the live-log detail view before being cleared from the database. Metadata rows are kept for analytics.</p>
    <div class="config-grid">
      ${renderField("recording_ttl_secs", "recording_ttl_secs", liveRecordingTtl, "TTL in seconds. Use 0 to clear bodies on the next prune tick.", (e) => { void onRecordingTtlChange(e); }, { editable: true, step: 1 })}
    </div>
    <div class="config-actions" style="margin-top: 1rem;">
      <button class="primary" data-action="configSaveRecordingTtl" @click=${configSaveRecordingTtl}>Save Recording TTL</button>
    </div>
  `);

  const compressionCard = card("Compression", html`
    <p class="muted">Reduce upstream token usage by compressing messages before sending them. <code>Lite</code> applies safe text normalization (zero semantic change); <code>Rtk</code> adds CLI-aware output filtering (git, cargo, etc.). See <a href="https://github.com/rtk-ai/rtk" target="_blank">rtk.ai</a> for details.</p>
    <div class="config-grid">
      <label class="config-field">
        <span class="config-label">mode</span>
        <select name="compression_mode" aria-label="Compression mode" @change=${onCompressionChange}>
          <option value="off" ?selected=${liveCompression === "off"}>Off</option>
          <option value="lite" ?selected=${liveCompression === "lite"}>Lite</option>
          <option value="rtk" ?selected=${liveCompression === "rtk"}>Rtk</option>
          <option value="lite_rtk" ?selected=${liveCompression === "lite_rtk"}>Lite + Rtk</option>
        </select>
        <span class="config-help">Which compression strategy to apply on every request. Changes apply to the <strong>next</strong> request.</span>
      </label>
    </div>
  `);

  const idleChunkCard = card("Idle Chunk Retryable", html`
    <p class="muted">When enabled, idle chunk timeouts (max gap between SSE chunks) are treated as retryable: the pipeline falls through to the next target instead of aborting. When disabled (default), idle chunk timeouts return an error immediately.</p>
    <div class="config-grid">
      <label class="config-field">
        <span class="config-label">idle_chunk_retryable</span>
        <button type="button" role="switch" aria-checked=${liveIdleChunkRetryable ? "true" : "false"}
                class="toggle-btn ${liveIdleChunkRetryable ? "on" : "off"}"
                @click=${() => { void onToggleIdleChunkRetryable(); }}>
          <span class="toggle-thumb"></span>
        </button>
        <input type="checkbox" name="idle_chunk_retryable" ?checked=${liveIdleChunkRetryable} class="sr-only">
        <span class="config-help">${liveIdleChunkRetryable
          ? "ON — idle chunk timeouts allow retry via next target"
          : "OFF — idle chunk timeouts return error immediately (default)"}</span>
      </label>
    </div>
  `);

  const quotaCard = card("Quota Protection", html`
    <p class="muted">Enable dynamic account quota rotation and protection. When active, accounts with exhausted or low quota (below the reserve threshold) are bypassed and rotated dynamically for the requested model.</p>
    <div class="config-grid">
      <label class="config-field">
        <span class="config-label">Enabled</span>
        <button type="button" role="switch" aria-checked=${liveQuotaProtectionEnabled ? "true" : "false"}
                class="toggle-btn ${liveQuotaProtectionEnabled ? "on" : "off"}"
                @click=${() => { void onToggleQuotaProtection(); }}>
          <span class="toggle-thumb"></span>
        </button>
        <span class="config-help">${liveQuotaProtectionEnabled
          ? "ON — Bypasses exhausted/protected accounts"
          : "OFF — Quota-based routing disabled (default)"}</span>
      </label>
      <label class="config-field">
        <span class="config-label">Reserve Threshold (%)</span>
        <input type="number" min="1" max="99" name="quota_protection.threshold_percentage" .value=${String(liveQuotaProtectionThreshold)}
               @change=${onQuotaThresholdChange} @input=${onQuotaThresholdChange}>
        <span class="config-help">Accounts dropping below this remaining fraction percentage are protected and avoided if other candidate accounts have quota (1-99).</span>
      </label>
    </div>
  `);

  const staticRegion = html`<details class="config-static-region">
    <summary>Server defaults (read-only — edit config.toml and restart)</summary>
    ${card("Retries", html`<div class="config-static-display">
      ${renderStaticField("max_attempts", r.max_attempts)}
      ${renderStaticField("backoff_base_ms", r.backoff_base_ms)}
      ${renderStaticField("backoff_factor", r.backoff_factor)}
      ${renderStaticField("backoff_jitter_pct", r.backoff_jitter_pct)}
      ${renderStaticField("combo_max_attempts", r.combo_max_attempts)}
    </div>`)}
    ${card("Circuit Breaker", html`<div class="config-static-display">
      ${renderStaticField("failure_threshold", cb.failure_threshold)}
      ${renderStaticField("unhealthy_duration_ms", cb.unhealthy_duration_ms)}
    </div>`)}
    ${card("Racing", html`<div class="config-static-display">
      ${renderStaticField("default_race_size", rc.default_race_size)}
      ${renderStaticField("max_race_size", rc.max_race_size)}
      ${renderStaticField("abort_grace_ms", rc.abort_grace_ms)}
    </div>`)}
  </details>`;

  // Maintenance / VACUUM card
  const vacuumBtnLabel = vacuumStatus.in_progress
    ? "⏳ VACUUM in progress…"
    : "🧹 Run VACUUM now";
  const lastRunText = vacuumStatus.last_run
    ? new Date(vacuumStatus.last_run).toLocaleString()
    : "never";
  const lastResultText = vacuumStatus.last_result
    ? (vacuumStatus.last_result === "ok" ? "✅ ok" : "❌ " + vacuumStatus.last_result)
    : "—";
  const nextScheduledText = vacuumStatus.next_scheduled
    ? new Date(vacuumStatus.next_scheduled).toLocaleString()
    : (liveAutoVacuum ? "scheduled (next tick)" : "disabled");
  const maintenanceCard = card("Database Maintenance", html`
    <div class="config-field">
      <label class="checkbox-label">
        <input type="checkbox" ?checked=${liveAutoVacuum} @change=${(e: Event) => { liveAutoVacuum = (e.target as HTMLInputElement).checked; void patchMaintenance(); }}>
        <span>Automatic VACUUM</span>
      </label>
      <p class="muted">When enabled, the server runs VACUUM every ${liveVacuumIntervalHours}h to compact freed pages. Disable to run VACUUM only manually.</p>
    </div>
    <div class="config-field">
      <label>VACUUM interval (hours)</label>
      <input type="number" min="1" max="168" .value=${String(liveVacuumIntervalHours)} @change=${(e: Event) => { const v = parseInt((e.target as HTMLInputElement).value, 10); if (v >= 1) { liveVacuumIntervalHours = v; void patchMaintenance(); } }}>
    </div>
    <div class="config-field">
      <label>Usage retention (days)</label>
      <input type="number" min="0" max="365" .value=${String(liveUsageRetentionDays)} @change=${(e: Event) => { const v = parseInt((e.target as HTMLInputElement).value, 10); if (v >= 0) { liveUsageRetentionDays = v; void patchMaintenance(); } }}>
      <p class="muted">Rows older than this are deleted hourly. 0 = keep forever (not recommended).</p>
    </div>
    <div class="config-field">
      <button class="btn ${vacuumStatus.in_progress ? "btn-disabled" : "btn-primary"}"
              ?disabled=${vacuumStatus.in_progress}
              @click=${() => void triggerVacuum()}>
        ${vacuumBtnLabel}
      </button>
    </div>
    <div class="config-field">
      <span class="label">Last run:</span> <span class="value">${lastRunText}</span>
      <span class="label" style="margin-left:1rem;">Result:</span> <span class="value">${lastResultText}</span>
    </div>
    <div class="config-field">
      <span class="label">Next scheduled:</span> <span class="value">${nextScheduledText}</span>
    </div>
  `);

  return html`
    <div class="page-header"><h2>Config</h2></div>
    <div class="banner banner-${bannerKind}">
      <strong>${bannerTitle}</strong>
      ${bannerBody}
    </div>
    <div class="config-editable-region">
      ${timeoutsCard}
      ${recordingTtlCard}
      ${compressionCard}
      ${idleChunkCard}
      ${quotaCard}
      ${maintenanceCard}
    </div>
    ${staticRegion}
    <details class="config-details">
      <summary>What does the precedence chain look like?</summary>
      <p>The pipeline resolves the effective timeouts on every request via <code>openproxy_core::timeouts::resolve</code>:</p>
      <ol>
        <li>Start with the system defaults shown above (this view). These are the single source of truth for <code>connect</code>, <code>request_send</code>, and <code>total</code> — there are no per-provider overrides anymore.</li>
        <li>Override <code>ttft</code> and <code>idle_chunk</code> from <code>models.timeout_overrides_json</code> if the target model sets them.</li>
      </ol>
      <p>Per-model overrides live in the database (not in <code>config.toml</code>), so they <em>can</em> change without a restart — but they are not exposed in this view. Use the Providers / Combos detail screens for those.</p>
    </details>`;
}

// ---- Mount ----

export async function mountConfig(): Promise<(() => void) | void> {
  const el = document.getElementById("main");
  if (!el) return;

  loading = true;
  errorMsg = null;
  cfg = null;
  const cleanup = mountView(el, renderConfig);

  try {
    cfg = await api("/config") as ConfigPayload;
    const t = cfg.timeouts || {};
    liveTimeouts = {
      connect_ms: t.connect_ms ?? 0,
      request_send_ms: t.request_send_ms ?? 0,
      ttft_ms: t.ttft_ms ?? 0,
      idle_chunk_ms: t.idle_chunk_ms ?? 0,
      total_ms: t.total_ms ?? 0,
    };
    liveRecordingTtl = cfg.recording_ttl_secs ?? 300;
    liveCompression = cfg.compression ?? "off";
    liveIdleChunkRetryable = cfg.idle_chunk_retryable ?? false;
    liveQuotaProtectionEnabled = cfg.quota_protection?.enabled ?? true;
    liveQuotaProtectionThreshold = cfg.quota_protection?.threshold_percentage ?? 10;
    // Load maintenance config + vacuum status
    try {
      const maint = await api("/config/maintenance") as {
        auto_vacuum?: boolean; vacuum_interval_hours?: number; usage_retention_days?: number;
        vacuum_status?: { last_run?: string | null; last_result?: string | null; in_progress?: boolean; next_scheduled?: string | null };
      };
      liveAutoVacuum = maint.auto_vacuum ?? true;
      liveVacuumIntervalHours = maint.vacuum_interval_hours ?? 6;
      liveUsageRetentionDays = maint.usage_retention_days ?? 7;
      if (maint.vacuum_status) {
        vacuumStatus = {
          last_run: maint.vacuum_status.last_run ?? null,
          last_result: maint.vacuum_status.last_result ?? null,
          in_progress: maint.vacuum_status.in_progress ?? false,
          next_scheduled: maint.vacuum_status.next_scheduled ?? null,
        };
      }
    } catch {
      // Maintenance endpoint not available — keep defaults
    }
    // Start polling vacuum status every 5s (so the button updates
    // when a VACUUM completes)
    if (vacuumPollHandle) clearInterval(vacuumPollHandle);
    vacuumPollHandle = setInterval(() => void pollVacuumStatus(), 5000);
    setBanner("info", "Live values.",
      "The values below are the ones the server is currently using. Timeouts, Recording TTL, Compression, the Idle Chunk Retryable flag, and Database Maintenance are editable; the other sections reflect the loaded config.toml.");
    loading = false;
    requestUpdate();
  } catch (e: unknown) {
    errorMsg = e instanceof Error ? e.message : String(e);
    loading = false;
    requestUpdate();
  }
  return () => {
    if (vacuumPollHandle) {
      clearInterval(vacuumPollHandle);
      vacuumPollHandle = null;
    }
    cleanup();
  };
}
