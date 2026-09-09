// views/config/index.ts — config editor entry point (lit-html).
//
// Thin orchestrator: fetches the `/config` payload, seeds the
// sub-module state, and assembles the page from the editable cards
// (editable-cards.ts), the Database Maintenance card
// (maintenance.ts), and the read-only static region (retries /
// circuit_breaker / racing, loaded from config.toml — not editable
// from the dashboard).
//
// The four legacy per-section save functions
// (`configSaveTimeouts`, `configSaveRecordingTtl`,
// `configSaveCompression`, `configSaveIdleChunkRetryable`) are
// re-exported because `handlers/registry.ts` imports them by name.

import { html, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";
import { api } from "../../state/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { createView } from "../../lib/view-utils.js";
import { t } from "../../i18n/index.js";
import {
  card, getConfig, getBanner, renderStaticField, setBanner, setConfig,
  type ConfigPayload,
} from "./shared.js";
import {
  applyServerConfig,
  renderCompressionCard, renderIdleChunkCard, renderQuotaCard,
  renderRecordingTtlCard, renderTimeoutsCard,
} from "./editable-cards.js";
import { loadMaintenanceState, pollVacuumStatus, renderMaintenanceCard } from "./maintenance.js";

// Re-exported for handlers/registry.ts (imported by name).
export {
  configSaveTimeouts, configSaveRecordingTtl,
  configSaveCompression, configSaveIdleChunkRetryable,
} from "./editable-cards.js";

// ── View state ──────────────────────────────────────────────────────

let loading = true;
let errorMsg: string | null = null;

// Vacuum status poll handle — owned here because mountConfig starts
// the interval and the returned cleanup cancels it.
let vacuumPollHandle: ReturnType<typeof setInterval> | null = null;

// ── Read-only static region (config.toml sections) ──────────────────

function renderStaticRegion(cfg: ConfigPayload): TemplateResult {
  const r = cfg.retries || {};
  const cb = cfg.circuit_breaker || {};
  const rc = cfg.racing || {};
  return html`<details class="config-static-region">
    <summary>${t("config.static.readonly_summary")}</summary>
    ${card(t("config.static.retries"), html`<div class="config-static-display">
      ${renderStaticField("max_attempts", r.max_attempts)}
      ${renderStaticField("backoff_base_ms", r.backoff_base_ms)}
      ${renderStaticField("backoff_factor", r.backoff_factor)}
      ${renderStaticField("backoff_jitter_pct", r.backoff_jitter_pct)}
      ${renderStaticField("combo_max_attempts", r.combo_max_attempts)}
    </div>`)}
    ${card(t("config.static.circuit_breaker"), html`<div class="config-static-display">
      ${renderStaticField("failure_threshold", cb.failure_threshold)}
      ${renderStaticField("unhealthy_duration_ms", cb.unhealthy_duration_ms)}
    </div>`)}
    ${card(t("config.static.racing"), html`<div class="config-static-display">
      ${renderStaticField("default_race_size", rc.default_race_size)}
      ${renderStaticField("max_race_size", rc.max_race_size)}
      ${renderStaticField("abort_grace_ms", rc.abort_grace_ms)}
    </div>`)}
  </details>`;
}

// ── Render ──────────────────────────────────────────────────────────

function renderConfig(): TemplateResult {
  if (loading) {
    return html`<div class="page-header"><h2>${t("config.title")}</h2></div>
      <div class="loading">${t("common.loading")}</div>`;
  }
  if (errorMsg) {
    return html`<div class="page-header"><h2>${t("config.title")}</h2></div>
      <div class="banner banner-error">${errorMsg}</div>`;
  }
  const cfg = getConfig();
  if (!cfg) {
    return html`<div class="page-header"><h2>${t("config.title")}</h2></div>
      <div class="loading">${t("common.loading")}</div>`;
  }
  const banner = getBanner();

  return html`
    <div class="page-header"><h2>${t("config.title")}</h2></div>
    <div class="banner banner-${banner.kind}">
      <strong>${banner.title}</strong>
      ${banner.body}
    </div>
    <div class="config-editable-region">
      ${renderTimeoutsCard()}
      ${renderRecordingTtlCard()}
      ${renderCompressionCard()}
      ${renderIdleChunkCard()}
      ${renderQuotaCard()}
      ${renderMaintenanceCard()}
    </div>
    ${renderStaticRegion(cfg)}
    <details class="config-details">
      <summary>${t("config.precedence.title")}</summary>
      <p>${unsafeHTML(t("config.precedence.body"))}</p>
      <ol>
        <li>${unsafeHTML(t("config.precedence.step1"))}</li>
        <li>${unsafeHTML(t("config.precedence.step2"))}</li>
      </ol>
      <p>${unsafeHTML(t("config.precedence.per_model_note"))}</p>
    </details>`;
}

// ── Mount ───────────────────────────────────────────────────────────

export async function mountConfig(): Promise<(() => void) | void> {
  loading = true;
  errorMsg = null;
  setConfig(null);
  const cleanupView = await createView(
    renderConfig,
    async () => {
      const payload = await api("/config") as ConfigPayload;
      setConfig(payload);
      applyServerConfig(payload);
      // Load maintenance config + vacuum status
      await loadMaintenanceState();
      // Start polling vacuum status every 5s (so the button updates
      // when a VACUUM completes)
      if (vacuumPollHandle) clearInterval(vacuumPollHandle);
      vacuumPollHandle = setInterval(() => void pollVacuumStatus(), 5000);
      setBanner("info", t("config.banner.live_values"),
        t("config.banner.live_values_body"));
      loading = false;
      requestUpdate();
    },
    (msg) => { errorMsg = msg; loading = false; },
  );
  return () => {
    if (vacuumPollHandle) {
      clearInterval(vacuumPollHandle);
      vacuumPollHandle = null;
    }
    if (cleanupView) cleanupView();
  };
}
