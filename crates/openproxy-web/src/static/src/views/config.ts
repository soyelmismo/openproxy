// views/config.ts — single-save config editor.
//
// The four editable sections (timeouts, recording TTL, compression,
// idle chunk retryable) each map to their own PUT endpoint, but the
// UI now exposes ONE "Save changes" button that fires the dirty
// sections in parallel. A small dot (`.config-dirty-indicator`) next
// to each field label signals unsaved changes; a sticky bottom bar
// (`.config-actions-bar`) holds the Save/Revert pair and a live
// "N sections modified" counter.
//
// The other three sections (retries, circuit_breaker, racing) reflect
// the loaded `config.toml` and are not editable from the dashboard —
// they live in a collapsed `<details class="config-static-region">`
// as plain mono-font text.
//
// NOTE: The four legacy per-section save functions
// (`configSaveTimeouts`, `configSaveRecordingTtl`,
// `configSaveCompression`, `configSaveIdleChunkRetryable`) are kept
// and exported because `handlers/registry.ts` imports them. They are
// no longer wired to any button in this view; the new `saveAll` is
// the canonical save path. They remain functional (each saves just
// its own section) in case a future caller wants the per-section
// behaviour back.

import { api } from "../state/api.js";
import { escapeHtml, extractApiErrorMessage } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import { showToast } from "../components/toast.js";

interface FieldOpts {
  editable?: boolean;
  step?: number;
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
}

type TimeoutKey = "connect_ms" | "request_send_ms" | "ttft_ms" | "idle_chunk_ms" | "total_ms";
const TIMEOUT_FIELDS: readonly TimeoutKey[] = ["connect_ms", "request_send_ms", "ttft_ms", "idle_chunk_ms", "total_ms"] as const;

/**
 * Snapshot of the last-saved editable values (from `GET /config` and
 * updated in-place after each successful `saveAll`). The dirty-state
 * tracker compares live inputs against this snapshot. `null` between
 * mounts (or while the GET is in flight). */
interface ConfigSnapshot {
  timeouts: Record<TimeoutKey, number>;
  recording_ttl_secs: number;
  compression: string;
  idle_chunk_retryable: boolean;
}

let snapshot: ConfigSnapshot | null = null;

function renderField(label: string, name: string, value: number | null | undefined, help: string, opts: FieldOpts = {}): string {
  // The `.config-dirty-indicator` span is always present in the DOM
  // (next to the label) but hidden via inline `display:none` until
  // `updateDirtyIndicators()` reveals it. We use inline style instead
  // of the `[hidden]` attribute because `.config-dirty-indicator`'s
  // own `display: inline-block` rule (in views.css) would override
  // the UA `[hidden] { display: none }` rule by specificity.
  return `
    <label class="config-field">
      <span class="config-label">${escapeHtml(label)}<span class="config-dirty-indicator" data-dirty-for="${escapeHtml(name)}" style="display:none"></span></span>
      <input type="number" name="${escapeHtml(name)}"
             value="${escapeHtml(String(value ?? ""))}"
             min="0" step="${opts.step ?? 100}"
             ${opts.editable ? "" : "disabled"}
             aria-label="${escapeHtml(label)}${opts.editable ? "" : " (read-only)"}">
      <span class="config-help">${escapeHtml(help)}</span>
    </label>
  `;
}

/** Render a read-only key/value pair for the static region. Uses
 *  `.config-static-display .field` so the existing CSS gives us the
 *  uppercase muted label + mono-font value look. */
function renderStaticField(label: string, value: number | null | undefined): string {
  const display: string = (value === null || value === undefined) ? "—" : String(value);
  return `<div class="field"><span class="label">${escapeHtml(label)}</span><span class="value">${escapeHtml(display)}</span></div>`;
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

// ---------------------------------------------------------------------------
// Legacy per-section save functions.
//
// These were the four separate save handlers behind the old four-button
// layout. They remain exported because `handlers/registry.ts` imports
// them by name (see constraint: no edits outside config.ts/sidebar.ts).
// The new UI routes everything through `saveAll` below; these are kept
// functional so the registry entries still resolve and so a future
// caller (e.g. a "save just this section" affordance) can re-use them.
// ---------------------------------------------------------------------------

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

async function configSaveRecordingTtl(): Promise<void> {
  const el = document.querySelector('input[name="recording_ttl_secs"]') as HTMLInputElement | null;
  if (!el) { showToast("recording_ttl_secs input missing from DOM", "error"); return; }
  const raw = (el.value || "").trim();
  if (raw === "") { showToast("recording_ttl_secs is required", "error"); return; }
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n) || !/^\d+$/.test(raw)) {
    showToast("recording_ttl_secs must be a non-negative integer", "error");
    return;
  }
  try {
    await api("/config/recording-ttl", { method: "PUT", body: JSON.stringify({ recording_ttl_secs: n }) });
    showToast(`Recording TTL set to ${n}s — applies on next prune tick`, "success");
  } catch (e: unknown) {
    const err = e instanceof Error ? e : null;
    showToast(extractApiErrorMessage(e) || (err ? err.message : String(e)), "error");
  }
}

async function configSaveCompression(): Promise<void> {
  const el = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
  if (!el) { showToast("compression_mode select missing from DOM", "error"); return; }
  const mode = el.value;
  if (mode !== "off" && mode !== "lite" && mode !== "rtk" && mode !== "lite_rtk") {
    showToast(`Invalid compression mode: ${mode}`, "error");
    return;
  }
  try {
    await api("/config/compression", { method: "PUT", body: JSON.stringify(mode) });
    showToast(`Compression mode set to ${mode} — applies to next requests`, "success");
  } catch (e: unknown) {
    showToast(extractApiErrorMessage(e) || String(e), "error");
  }
}

async function configSaveIdleChunkRetryable(): Promise<void> {
  const el = document.querySelector('input[name="idle_chunk_retryable"]') as HTMLInputElement | null;
  if (!el) { showToast("idle_chunk_retryable toggle missing from DOM", "error"); return; }
  const val = el.checked;
  try {
    await api("/config/idle-chunk-retryable", { method: "PUT", body: JSON.stringify({ idle_chunk_retryable: val }) });
    showToast(`Idle chunk retryable set to ${val} — applies to next requests`, "success");
  } catch (e: unknown) {
    showToast(extractApiErrorMessage(e) || String(e), "error");
  }
}

// ---------------------------------------------------------------------------
// New: dirty-state tracking + single Save / Revert pair.
// ---------------------------------------------------------------------------

/** Toggle a single dirty indicator by its `data-dirty-for` field name. */
function setDirtyIndicator(fieldName: string, isDirty: boolean): void {
  const indicator = document.querySelector(`[data-dirty-for="${CSS.escape(fieldName)}"]`) as HTMLElement | null;
  if (indicator) indicator.style.display = isDirty ? "inline-block" : "none";
}

/** Read the live values out of the editable inputs and compare against
 *  `snapshot`. Updates every dirty indicator, the "N sections modified"
 *  counter, and the Save/Revert disabled state. Safe to call before
 *  the snapshot is set (no-op). */
function updateDirtyIndicators(): void {
  if (!snapshot) return;

  let dirtySections = 0;

  // --- Timeouts ---
  let timeoutsDirty = false;
  for (const f of TIMEOUT_FIELDS) {
    const el = document.querySelector(`input[name="timeouts.${f}"]`) as HTMLInputElement | null;
    if (!el) continue;
    const raw = (el.value || "").trim();
    // Treat empty / non-numeric as dirty so the user sees the marker
    // and the Save button stays armed; `saveAll` will refuse to send
    // until the value validates.
    const n: number = raw === "" ? NaN : Number(raw);
    const isDirty: boolean = !(n === snapshot.timeouts[f]);
    timeoutsDirty = timeoutsDirty || isDirty;
    setDirtyIndicator(`timeouts.${f}`, isDirty);
  }
  if (timeoutsDirty) dirtySections++;

  // --- Recording TTL ---
  {
    const el = document.querySelector('input[name="recording_ttl_secs"]') as HTMLInputElement | null;
    if (el) {
      const raw = (el.value || "").trim();
      const n: number = raw === "" ? NaN : Number(raw);
      const isDirty: boolean = !(n === snapshot.recording_ttl_secs);
      if (isDirty) dirtySections++;
      setDirtyIndicator("recording_ttl_secs", isDirty);
    }
  }

  // --- Compression ---
  {
    const el = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
    if (el) {
      const isDirty: boolean = el.value !== snapshot.compression;
      if (isDirty) dirtySections++;
      setDirtyIndicator("compression_mode", isDirty);
    }
  }

  // --- Idle Chunk Retryable ---
  {
    const el = document.querySelector('input[name="idle_chunk_retryable"]') as HTMLInputElement | null;
    if (el) {
      const isDirty: boolean = el.checked !== snapshot.idle_chunk_retryable;
      if (isDirty) dirtySections++;
      setDirtyIndicator("idle_chunk_retryable", isDirty);
    }
  }

  const countEl = document.getElementById("config-dirty-count");
  if (countEl) {
    countEl.textContent = dirtySections === 0
      ? "No changes"
      : `${dirtySections} section${dirtySections === 1 ? "" : "s"} modified`;
  }
  const saveBtn = document.getElementById("config-save-btn") as HTMLButtonElement | null;
  if (saveBtn) saveBtn.disabled = dirtySections === 0;
  const revertBtn = document.getElementById("config-revert-btn") as HTMLButtonElement | null;
  if (revertBtn) revertBtn.disabled = dirtySections === 0;
}

/** Read all editable inputs, validate them, fire the PUTs for every
 *  dirty section in parallel via `Promise.all`, and on success update
 *  the in-memory snapshot so the dirty indicators clear. Shows a
 *  single success toast (or a single error toast on failure). */
async function saveAll(): Promise<void> {
  if (!snapshot) return;

  const tasks: Promise<unknown>[] = [];
  const dirtySectionNames: string[] = [];

  // --- Timeouts (validate all-or-nothing per section) ---
  let timeoutsDirty = false;
  const timeoutsBody: Partial<Record<TimeoutKey, number>> = {};
  for (const f of TIMEOUT_FIELDS) {
    const el = document.querySelector(`input[name="timeouts.${f}"]`) as HTMLInputElement | null;
    if (!el) { showToast(`timeouts.${f} input is missing from the DOM`, "error"); return; }
    const raw = (el.value || "").trim();
    if (raw === "") { showToast(`timeouts.${f} is required`, "error"); return; }
    const n = Number(raw);
    if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n) || !/^\d+$/.test(raw)) {
      showToast(`timeouts.${f} must be a non-negative integer`, "error");
      return;
    }
    timeoutsBody[f] = n;
    if (n !== snapshot.timeouts[f]) timeoutsDirty = true;
  }
  if (timeoutsDirty) {
    tasks.push(api("/config/timeouts", { method: "PUT", body: JSON.stringify(timeoutsBody) }));
    dirtySectionNames.push("timeouts");
  }

  // --- Recording TTL ---
  {
    const el = document.querySelector('input[name="recording_ttl_secs"]') as HTMLInputElement | null;
    if (!el) { showToast("recording_ttl_secs input missing from DOM", "error"); return; }
    const raw = (el.value || "").trim();
    if (raw === "") { showToast("recording_ttl_secs is required", "error"); return; }
    const n = Number(raw);
    if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n) || !/^\d+$/.test(raw)) {
      showToast("recording_ttl_secs must be a non-negative integer", "error");
      return;
    }
    if (n !== snapshot.recording_ttl_secs) {
      tasks.push(api("/config/recording-ttl", { method: "PUT", body: JSON.stringify({ recording_ttl_secs: n }) }));
      dirtySectionNames.push("recording_ttl");
    }
  }

  // --- Compression ---
  {
    const el = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
    if (!el) { showToast("compression_mode select missing from DOM", "error"); return; }
    const mode = el.value;
    if (mode !== "off" && mode !== "lite" && mode !== "rtk" && mode !== "lite_rtk") {
      showToast(`Invalid compression mode: ${mode}`, "error");
      return;
    }
    if (mode !== snapshot.compression) {
      tasks.push(api("/config/compression", { method: "PUT", body: JSON.stringify(mode) }));
      dirtySectionNames.push("compression");
    }
  }

  // --- Idle Chunk Retryable ---
  {
    const el = document.querySelector('input[name="idle_chunk_retryable"]') as HTMLInputElement | null;
    if (!el) { showToast("idle_chunk_retryable toggle missing from DOM", "error"); return; }
    const val = el.checked;
    if (val !== snapshot.idle_chunk_retryable) {
      tasks.push(api("/config/idle-chunk-retryable", { method: "PUT", body: JSON.stringify({ idle_chunk_retryable: val }) }));
      dirtySectionNames.push("idle_chunk_retryable");
    }
  }

  if (tasks.length === 0) {
    showToast("No changes to save", "info");
    return;
  }

  // Disable the Save button while in flight so the user can't
  // double-submit. Re-enabled by `updateDirtyIndicators()` after the
  // Promise settles (success OR error).
  const saveBtn = document.getElementById("config-save-btn") as HTMLButtonElement | null;
  if (saveBtn) saveBtn.disabled = true;

  try {
    await Promise.all(tasks);
    // Commit the just-saved values into the snapshot so the dirty
    // indicators clear. We read straight from the DOM (the source of
    // truth the user just confirmed) rather than re-fetching.
    for (const f of TIMEOUT_FIELDS) {
      const el = document.querySelector(`input[name="timeouts.${f}"]`) as HTMLInputElement | null;
      if (el) {
        const n = Number((el.value || "").trim());
        if (Number.isFinite(n)) snapshot.timeouts[f] = n;
      }
    }
    const ttlEl = document.querySelector('input[name="recording_ttl_secs"]') as HTMLInputElement | null;
    if (ttlEl) {
      const n = Number((ttlEl.value || "").trim());
      if (Number.isFinite(n)) snapshot.recording_ttl_secs = n;
    }
    const compEl = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
    if (compEl) snapshot.compression = compEl.value;
    const icrEl = document.querySelector('input[name="idle_chunk_retryable"]') as HTMLInputElement | null;
    if (icrEl) snapshot.idle_chunk_retryable = icrEl.checked;

    showToast(
      `Saved ${dirtySectionNames.length} section${dirtySectionNames.length === 1 ? "" : "s"} — applies to next requests`,
      "success",
    );
    swapBanner("success", "Live — applies to next requests",
      "The values below are persisted in the database and will " +
      "take effect on the next request. Requests already in flight " +
      "continue with the previous values.");
    updateDirtyIndicators();
  } catch (e: unknown) {
    const err = e instanceof Error ? e : null;
    showToast(extractApiErrorMessage(e) || (err ? err.message : String(e)), "error");
    // Re-arm the Save button so the user can retry after fixing the
    // issue. Dirty indicators stay as they were (snapshot unchanged).
    updateDirtyIndicators();
  }
}

/** Reset every editable input back to the last-saved snapshot value
 *  and re-run the dirty tracker. The toggle button's on/off class and
 *  help text are also restored so the UI matches the snapshot exactly. */
function revertConfig(): void {
  if (!snapshot) return;

  for (const f of TIMEOUT_FIELDS) {
    const el = document.querySelector(`input[name="timeouts.${f}"]`) as HTMLInputElement | null;
    if (el) el.value = String(snapshot.timeouts[f]);
  }
  const ttlEl = document.querySelector('input[name="recording_ttl_secs"]') as HTMLInputElement | null;
  if (ttlEl) ttlEl.value = String(snapshot.recording_ttl_secs);

  const compEl = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
  if (compEl) compEl.value = snapshot.compression;

  const icrEl = document.querySelector('input[name="idle_chunk_retryable"]') as HTMLInputElement | null;
  if (icrEl) icrEl.checked = snapshot.idle_chunk_retryable;
  const icrBtn = document.querySelector('button[data-action="toggleIdleChunkRetryable"]') as HTMLButtonElement | null;
  if (icrBtn) {
    icrBtn.classList.toggle("on", snapshot.idle_chunk_retryable);
    icrBtn.classList.toggle("off", !snapshot.idle_chunk_retryable);
    icrBtn.setAttribute("aria-checked", String(snapshot.idle_chunk_retryable));
    const help = icrBtn.parentElement?.querySelector(".config-help");
    if (help) {
      help.textContent = snapshot.idle_chunk_retryable
        ? "ON — idle chunk timeouts allow retry via next target"
        : "OFF — idle chunk timeouts return error immediately (default)";
    }
  }

  updateDirtyIndicators();
  showToast("Reverted to last saved values", "info");
}

/** Wire up the input/change listeners that drive `updateDirtyIndicators`
 *  plus the Save / Revert button click handlers. Called once at the
 *  end of `mountConfig` after the DOM is painted. The toggle button
 *  keeps its `data-action="toggleIdleChunkRetryable"` registry handler
 *  (which flips the checkbox); we attach a deferred click listener
 *  that re-runs the dirty tracker AFTER the registry handler has run
 *  (the registry handler is on `document`, so it fires during the
 *  bubbling phase AFTER our button-level listener — hence the
 *  `setTimeout(…, 0)` to defer to the next macrotask). */
function attachConfigListeners(): void {
  const numericNames: readonly string[] = [
    "timeouts.connect_ms",
    "timeouts.request_send_ms",
    "timeouts.ttft_ms",
    "timeouts.idle_chunk_ms",
    "timeouts.total_ms",
    "recording_ttl_secs",
  ];
  for (const name of numericNames) {
    const el = document.querySelector(`input[name="${name}"]`) as HTMLInputElement | null;
    if (el) el.addEventListener("input", updateDirtyIndicators);
  }
  const compEl = document.querySelector('select[name="compression_mode"]') as HTMLSelectElement | null;
  if (compEl) compEl.addEventListener("change", updateDirtyIndicators);

  const toggleBtn = document.querySelector('button[data-action="toggleIdleChunkRetryable"]');
  if (toggleBtn) {
    toggleBtn.addEventListener("click", () => {
      // Defer so the document-level registry handler (which flips
      // the checkbox) runs first.
      setTimeout(updateDirtyIndicators, 0);
    });
  }

  const saveBtn = document.getElementById("config-save-btn");
  if (saveBtn) saveBtn.addEventListener("click", () => { void saveAll(); });

  const revertBtn = document.getElementById("config-revert-btn");
  if (revertBtn) revertBtn.addEventListener("click", revertConfig);
}

export { configSaveTimeouts, configSaveRecordingTtl, configSaveCompression, configSaveIdleChunkRetryable, saveAll };

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
  const compression = cfg.compression ?? "off";
  const idleChunkRetryable = cfg.idle_chunk_retryable ?? false;
  const recordingTtl = cfg.recording_ttl_secs ?? 300;

  // Seed the snapshot from the GET response. The four editable
  // sections use real defaults (0 / 300 / "off" / false) so the dirty
  // tracker has something concrete to diff against even if the server
  // omitted a field.
  snapshot = {
    timeouts: {
      connect_ms: t.connect_ms ?? 0,
      request_send_ms: t.request_send_ms ?? 0,
      ttft_ms: t.ttft_ms ?? 0,
      idle_chunk_ms: t.idle_chunk_ms ?? 0,
      total_ms: t.total_ms ?? 0,
    },
    recording_ttl_secs: recordingTtl,
    compression,
    idle_chunk_retryable: idleChunkRetryable,
  };

  main.innerHTML = `
    ${pageHeader({ title: "Config" })}
    <div id="config-banner" class="banner banner-info">
      <strong>Live values.</strong>
      The values below are the ones the server is currently using.
      Timeouts, Recording TTL, Compression, and the Idle Chunk Retryable flag
      are editable; the other sections reflect the loaded
      <code>config.toml</code>. Changes are persisted in the database and apply to
      the next request (timeouts) or the next prune tick (Recording TTL).
    </div>
    <div class="config-editable-region">
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
      ${card("Recording TTL <small>(seconds)</small>", `
        <p class="muted">How long recorded request/response bodies and headers stay in the live-log detail view before being cleared from the database. Metadata rows are kept for analytics.</p>
        <div class="config-grid">
          ${renderField("recording_ttl_secs", "recording_ttl_secs", recordingTtl, "TTL in seconds. Use 0 to clear bodies on the next prune tick.", { editable: true, step: 1 })}
        </div>
      `)}
      ${card("Compression", `
        <p class="muted">Reduce upstream token usage by compressing messages before sending them. <code>Lite</code> applies safe text normalization (zero semantic change); <code>Rtk</code> adds CLI-aware output filtering (git, cargo, etc.). See <a href="https://github.com/rtk-ai/rtk" target="_blank">rtk.ai</a> for details.</p>
        <div class="config-grid">
          <label class="config-field">
            <span class="config-label">mode<span class="config-dirty-indicator" data-dirty-for="compression_mode" style="display:none"></span></span>
            <select name="compression_mode" aria-label="Compression mode">
              <option value="off"  ${compression === "off"  ? "selected" : ""}>Off</option>
              <option value="lite" ${compression === "lite" ? "selected" : ""}>Lite</option>
              <option value="rtk"  ${compression === "rtk"  ? "selected" : ""}>Rtk</option>
              <option value="lite_rtk"  ${compression === "lite_rtk" ? "selected" : ""}>Lite + Rtk</option>
            </select>
            <span class="config-help">Which compression strategy to apply on every request. Changes apply to the <strong>next</strong> request.</span>
          </label>
        </div>
      `)}
      ${card("Idle Chunk Retryable", `
        <p class="muted">When enabled, idle chunk timeouts (max gap between SSE chunks) are treated as retryable: the pipeline falls through to the next target instead of aborting. When disabled (default), idle chunk timeouts return an error immediately.</p>
        <div class="config-grid">
          <label class="config-field">
            <span class="config-label">idle_chunk_retryable<span class="config-dirty-indicator" data-dirty-for="idle_chunk_retryable" style="display:none"></span></span>
            <button type="button" role="switch" aria-checked="${idleChunkRetryable}"
                    data-action="toggleIdleChunkRetryable"
                    class="toggle-btn ${idleChunkRetryable ? "on" : "off"}">
              <span class="toggle-thumb"></span>
            </button>
            <input type="checkbox" name="idle_chunk_retryable" ${idleChunkRetryable ? "checked" : ""} class="sr-only">
            <span class="config-help">${idleChunkRetryable ? "ON — idle chunk timeouts allow retry via next target" : "OFF — idle chunk timeouts return error immediately (default)"}</span>
          </label>
        </div>
      `)}
    </div>
    <details class="config-static-region">
      <summary>Server defaults (read-only — edit config.toml and restart)</summary>
      ${card("Retries", `<div class="config-static-display">
        ${renderStaticField("max_attempts", r.max_attempts)}
        ${renderStaticField("backoff_base_ms", r.backoff_base_ms)}
        ${renderStaticField("backoff_factor", r.backoff_factor)}
        ${renderStaticField("backoff_jitter_pct", r.backoff_jitter_pct)}
        ${renderStaticField("combo_max_attempts", r.combo_max_attempts)}
      </div>`)}
      ${card("Circuit Breaker", `<div class="config-static-display">
        ${renderStaticField("failure_threshold", cb.failure_threshold)}
        ${renderStaticField("unhealthy_duration_ms", cb.unhealthy_duration_ms)}
      </div>`)}
      ${card("Racing", `<div class="config-static-display">
        ${renderStaticField("default_race_size", rc.default_race_size)}
        ${renderStaticField("max_race_size", rc.max_race_size)}
        ${renderStaticField("abort_grace_ms", rc.abort_grace_ms)}
      </div>`)}
    </details>
    <div class="config-actions-bar">
      <button class="primary" type="button" id="config-save-btn" disabled>Save changes</button>
      <button type="button" id="config-revert-btn" disabled>Revert</button>
      <span class="dirty-count" id="config-dirty-count">No changes</span>
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

  attachConfigListeners();
  updateDirtyIndicators();
}
