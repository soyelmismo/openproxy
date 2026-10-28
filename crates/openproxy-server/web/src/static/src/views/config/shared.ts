// views/config/shared.ts — types, banner state, config accessors, helpers.
// The three config.toml sections (retries, circuit_breaker, racing) are
// read-only here and render in a collapsed `.config-static-region`.

import { html, type TemplateResult } from "lit-html";
import { showToast } from "../../components/toast.js";

// ── Types ───────────────────────────────────────────────────────────

export interface FieldOpts {
  editable?: boolean;
  step?: number;
  unit?: string;
}

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
  modality_routing?: {
    enabled?: boolean | null;
    fallback_to_text_only?: boolean | null;
  } | null;
  pii?: {
    pii_enabled?: boolean | null;
    pii_reversible?: boolean | null;
    pii_redact_logs?: boolean | null;
    pii_entities?: string[] | null;
  } | null;
  /** W3: global notifications master switch (top-level in the payload). */
  notifications_enabled?: boolean | null;
  pii_enabled?: boolean | null;
  pii_reversible?: boolean | null;
  pii_redact_logs?: boolean | null;
  pii_entities?: string[] | null;
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
// The `patch*` helpers mutate this object in place, so accessor pairs suffice.

let cfg: ConfigPayload | null = null;

export function getConfig(): ConfigPayload | null {
  return cfg;
}

export function setConfig(value: ConfigPayload | null): void {
  cfg = value;
}


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

/** Extract `message` from the JSON `ApiError` body that `api()` appends to `e.message`. */
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
    <div class="config-input-group ${opts.unit ? "has-unit" : ""}">
      <input type="number" inputmode="numeric" name=${name} .value=${String(value)} min="0" step=${opts.step ?? 100}
        ?disabled=${!opts.editable}
        aria-label=${label + (opts.editable ? "" : " (read-only)")}
        @change=${onChange} @input=${onChange}>
      ${opts.unit ? html`<span class="config-input-unit">${opts.unit}</span>` : ""}
    </div>
    <span class="config-help">${help}</span>
  </label>`;
}

/** Read-only key/value pair for the static region. `.config-static-display .field`
 *  supplies the uppercase muted label + mono value styling. */
export function renderStaticField(label: string, value: number | null | undefined): TemplateResult {
  const display: string = (value === null || value === undefined) ? "—" : String(value);
  return html`<div class="field"><span class="label">${label}</span><span class="value">${display}</span></div>`;
}

export function card(title: unknown, body: TemplateResult, extraClass = ""): TemplateResult {
  return html`<section class="card config-card ${extraClass}"><div class="section-header"><h3>${title}</h3></div><div class="card-body">${body}</div></section>`;
}
