// views/config.ts — read-mostly view of the server config.
// Timeouts are editable and PUT to /v1/admin/config/timeouts.
// Other sections (retries, circuit-breaker, racing) are read-only.

import { api } from "../state/api.js";
import { escapeHtml, extractApiErrorMessage } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import { showToast } from "../components/toast.js";

interface FieldOpts {
  editable?: boolean;
}

// Shape of the /v1/admin/config response. The server flattens the
// four sections (timeouts, retries, circuit_breaker, racing) into
// a single object; each section is optional so a partial payload
// (e.g. on a transient error) doesn't crash the render. The JS
// used to be `cfg.timeouts || {}` for the same reason.
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
}

type TimeoutKey = "connect_ms" | "request_send_ms" | "ttft_ms" | "idle_chunk_ms" | "total_ms";
const TIMEOUT_FIELDS: readonly TimeoutKey[] = ["connect_ms", "request_send_ms", "ttft_ms", "idle_chunk_ms", "total_ms"] as const;

function renderField(label: string, name: string, value: number | null | undefined, help: string, opts: FieldOpts = {}): string {
  return `
    <label class="config-field">
      <span class="config-label">${escapeHtml(label)}</span>
      <input type="number" name="${escapeHtml(name)}"
             value="${escapeHtml(String(value ?? ""))}"
             min="0" step="100"
             ${opts.editable ? "" : "disabled"}
             aria-label="${escapeHtml(label)}${opts.editable ? "" : " (read-only)"}">
      <span class="config-help">${escapeHtml(help)}</span>
    </label>
  `;
}

function readTimeoutsFromInputs(): Record<TimeoutKey, number> | null {
  const out: Partial<Record<TimeoutKey, number>> = {};
  for (const f of TIMEOUT_FIELDS) {
    const el = document.querySelector(`input[name="timeouts.${f}"]`) as HTMLInputElement | null;
    if (!el) { showToast(`timeouts.${f} input is missing from the DOM`, "error"); return null; }
    const raw = (el.value || "").trim();
    if (raw === "") { showToast(`timeouts.${f} is required`, "error"); return null; }
    const n = Number(raw);
    if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n) || !/^\d+$/.test(raw)) {
      showToast(`timeouts.${f} must be a non-negative integer`, "error");
      return null;
    }
    out[f] = n;
  }
  return out as Record<TimeoutKey, number>;
}

function swapBanner(bannerType: string, title: string, body: string): void {
  const el = document.getElementById("config-banner");
  if (!el) return;
  el.classList.remove("banner-info", "banner--success");
  el.classList.add("banner-" + bannerType);
  el.innerHTML = `<strong>${escapeHtml(title)}</strong> ${escapeHtml(body)}`;
}

async function configSaveTimeouts(): Promise<void> {
  const t = readTimeoutsFromInputs();
  if (!t) return;
  try {
    await api("/config/timeouts", { method: "PUT", body: JSON.stringify(t) });
    showToast("Config updated — applies to next requests", "success");
    swapBanner("success", "Live — applies to next requests",
      "The values below are persisted in the database and will " +
      "take effect on the next request. Requests already in flight " +
      "continue with the previous values.");
  } catch (e: unknown) {
    const err = e instanceof Error ? e : null;
    showToast(extractApiErrorMessage(e) || (err ? err.message : String(e)), "error");
  }
}

export { configSaveTimeouts };

export async function mountConfig(): Promise<void> {
  const main = document.getElementById("main");
  if (!main) return;
  main.innerHTML = pageHeader({ title: "Config" }) + `<div class="loading">Loading...</div>`;
  let cfg: ConfigPayload;
  try {
    cfg = await api("/config") as ConfigPayload;
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    main.innerHTML = pageHeader({ title: "Config" }) +
      `<div class="banner banner-error">${escapeHtml(msg)}</div>`;
    return;
  }
  const t = cfg.timeouts || {};
  const r = cfg.retries || {};
  const cb = cfg.circuit_breaker || {};
  const rc = cfg.racing || {};
  main.innerHTML = `
    ${pageHeader({ title: "Config" })}
    <div id="config-banner" class="banner banner-info">
      <strong>Live values.</strong>
      The values below are the ones the server is currently using.
      Timeouts are editable; the other sections reflect the loaded
      <code>config.toml</code>. Changes to timeouts are persisted in
      the database and apply to the next request.
    </div>
    ${card("Timeouts <small>(ms)</small>", `
      <p class="muted">Precedence (highest wins): <code>model overrides</code> → <code>provider_timeouts</code> → <code>system default</code>. Editing these values takes effect on the next request; in-flight requests keep the previous value.</p>
      <div class="config-grid">
        ${renderField("connect_ms", "timeouts.connect_ms", t.connect_ms, "TCP connect to the upstream.", { editable: true })}
        ${renderField("request_send_ms", "timeouts.request_send_ms", t.request_send_ms, "Max time to flush the request body.", { editable: true })}
        ${renderField("ttft_ms", "timeouts.ttft_ms", t.ttft_ms, "Time-to-first-token for streaming responses.", { editable: true })}
        ${renderField("idle_chunk_ms", "timeouts.idle_chunk_ms", t.idle_chunk_ms, "Max gap between SSE chunks.", { editable: true })}
        ${renderField("total_ms", "timeouts.total_ms", t.total_ms, "Hard ceiling for the whole request.", { editable: true })}
      </div>
    `)}
    ${card("Retries", `<div class="config-grid">
        ${renderField("max_attempts", "retries.max_attempts", r.max_attempts, "Including the first try.")}
        ${renderField("backoff_base_ms", "retries.backoff_base_ms", r.backoff_base_ms, "Initial backoff.")}
        ${renderField("backoff_factor", "retries.backoff_factor", r.backoff_factor, "Exponential factor.")}
        ${renderField("backoff_jitter_pct", "retries.backoff_jitter_pct", r.backoff_jitter_pct, "Random jitter % to avoid thundering herds.")}
    </div>`)}
    ${card("Circuit Breaker", `<div class="config-grid">
        ${renderField("failure_threshold", "circuit_breaker.failure_threshold", cb.failure_threshold, "Consecutive failures before a target is parked.")}
        ${renderField("unhealthy_duration_ms", "circuit_breaker.unhealthy_duration_ms", cb.unhealthy_duration_ms, "How long a parked target stays out of rotation.")}
    </div>`)}
    ${card("Racing", `<div class="config-grid">
        ${renderField("default_race_size", "racing.default_race_size", rc.default_race_size, "Default number of parallel targets per combo.")}
        ${renderField("max_race_size", "racing.max_race_size", rc.max_race_size, "Upper bound the dashboard can set.")}
        ${renderField("abort_grace_ms", "racing.abort_grace_ms", rc.abort_grace_ms, "Grace period before aborting losing branches.")}
    </div>`)}
    <div class="config-actions">
      <button class="primary" type="button" data-action="configSaveTimeouts">Save</button>
      <span class="muted">Saves the timeouts above. The other sections are read-only here; edit <code>config.toml</code> and restart to change them.</span>
    </div>
    <details class="config-details">
      <summary>What does the precedence chain look like?</summary>
      <p>The pipeline resolves the effective timeouts on every request via <code>openproxy_core::timeouts::resolve</code>:</p>
      <ol>
        <li>Start with the system defaults shown above (this view).</li>
        <li>Override <code>connect</code>, <code>request_send</code>, and <code>total</code> from <code>provider_timeouts</code> if a row exists for the selected provider.</li>
        <li>Override <code>ttft</code> and <code>idle_chunk</code> from <code>models.timeout_overrides_json</code> if the target model sets them.</li>
      </ol>
      <p>Provider/model overrides live in the database (not in <code>config.toml</code>), so they <em>can</em> change without a restart — but they are not exposed in this view. Use the Providers / Combos detail screens for those.</p>
    </details>
  `;
}
