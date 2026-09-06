// views/config/shared.ts — shared types, banner state, and render
// helpers for the config view sub-modules.
//
// The four editable sections (timeouts, recording TTL, compression,
// idle-chunk-retryable) each map to their own PUT endpoint and
// auto-save on change. A small banner at the top reflects the
// last-save status. The other three sections (retries,
// circuit_breaker, racing) reflect the loaded `config.toml` and are
// not editable from the dashboard — they live in a collapsed
// `<details class="config-static-region">` as plain mono-font text.
//
// See views/combos.ts for the lit-html migration reference pattern.

import { html, type TemplateResult } from "lit-html";
import { showToast } from "../../components/toast.js";

// ── Types ───────────────────────────────────────────────────────────

export interface FieldOpts {
  editable?: boolean;
  step?: number;
}

// Shape of the /admin/config response. The server flattens the
// four sections (timeouts, retries, circuit_breaker, racing) into
// a single object; each section is optional so a partial payload
// (e.g. on a transient error) doesn't crash the render.
export interface ConfigPayload {
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

export interface TimeoutsState {
  connect_ms: number;
  request_send_ms: number;
  ttft_ms: number;
  idle_chunk_ms: number;
  total_ms: number;
}

export interface VacuumStatus {
  last_run: string | null;
  last_result: string | null;
  in_progress: boolean;
  next_scheduled: string | null;
}

export type TimeoutKey = keyof TimeoutsState;

// ── Constants ───────────────────────────────────────────────────────

export const TIMEOUT_FIELDS: readonly TimeoutKey[] = ["connect_ms", "request_send_ms", "ttft_ms", "idle_chunk_ms", "total_ms"] as const;

export const DEFAULT_TIMEOUTS = {
  connect_ms: 0,
  request_send_ms: 0,
  ttft_ms: 0,
  idle_chunk_ms: 0,
  total_ms: 0,
} satisfies TimeoutsState;

// ── Loaded-config state ─────────────────────────────────────────────
//
// The `/config` payload fetched by mountConfig. The `patch*` helpers
// in editable-cards.ts mutate the object in place (they never
// reassign the variable), so accessor pairs are enough to share it.

let cfg: ConfigPayload | null = null;

export function getConfig(): ConfigPayload | null {
  return cfg;
}

export function setConfig(value: ConfigPayload | null): void {
  cfg = value;
}

// ── Banner state. Set by the patch helpers after each save. ────────

let bannerKind: "info" | "success" = "info";
let bannerTitle = "Live values.";
let bannerBody = "The values below are the ones the server is currently using. Timeouts, Recording TTL, Compression, and the Idle Chunk Retryable flag are editable; the other sections reflect the loaded config.toml. Changes are persisted in the database and apply to the next request (timeouts) or the next prune tick (Recording TTL).";

export function setBanner(kind: "info" | "success", title: string, body: string): void {
  bannerKind = kind;
  bannerTitle = title;
  bannerBody = body;
}

export function getBanner(): { kind: "info" | "success"; title: string; body: string } {
  return { kind: bannerKind, title: bannerTitle, body: bannerBody };
}

// ── Helpers ─────────────────────────────────────────────────────────

/** Pull the human-readable `message` field out of the JSON envelope
 *  produced by the server's `ApiError` impl. The thrower is `api()`,
 *  which raises `new Error("<status>: <body>")`; the JSON body lives
 *  as a string suffix on `e.message`, and we re-parse it here. */
export function errStr(e: unknown): string {
  if (!(e instanceof Error)) return String(e);
  const m = e.message.match(/"error"\s*:\s*\{[\s\S]*?"message"\s*:\s*"((?:[^"\\]|\\.)*)"/);
  if (m) {
    try { return JSON.parse('"' + (m[1] ?? "") + '"') as string; }
    catch (_err: unknown) { return m[1] ?? e.message; }
  }
  return e.message;
}

export function validateNonNegInt(raw: string, fieldName: string): number | null {
  if (raw === "") { showToast(`${fieldName} is required`, "error"); return null; }
  if (!/^\d+$/.test(raw)) { showToast(`${fieldName} must be a non-negative integer`, "error"); return null; }
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0) { showToast(`${fieldName} must be a non-negative integer`, "error"); return null; }
  return n;
}

// ── Templates ───────────────────────────────────────────────────────

export function renderField(
  label: string,
  name: string,
  value: number,
  help: string,
  onChange: (e: Event) => void,
  opts: FieldOpts = {},
): TemplateResult {
  return html`<label class="config-field">
    <span class="config-label">${label}</span>
    <input type="number" inputmode="numeric" name=${name} .value=${String(value)} min="0" step=${opts.step ?? 100}
      ?disabled=${!opts.editable}
      aria-label=${label + (opts.editable ? "" : " (read-only)")}
      @change=${onChange} @input=${onChange}>
    <span class="config-help">${help}</span>
  </label>`;
}

/** Render a read-only key/value pair for the static region. Uses
 *  `.config-static-display .field` so the existing CSS gives us the
 *  uppercase muted label + mono-font value look. */
export function renderStaticField(label: string, value: number | null | undefined): TemplateResult {
  const display: string = (value === null || value === undefined) ? "—" : String(value);
  return html`<div class="field"><span class="label">${label}</span><span class="value">${display}</span></div>`;
}

export function card(title: string | TemplateResult, body: TemplateResult): TemplateResult {
  return html`<section class="card"><div class="section-header"><h3>${title}</h3></div>${body}</section>`;
}
