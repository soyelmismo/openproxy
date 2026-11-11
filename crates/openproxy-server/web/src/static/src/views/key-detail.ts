// Integrated API key properties. Reuses the same form/payload contract as creation.
import { html, render } from "lit-html";
import { api } from "../state/api.js";
import { state } from "../state/index.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { ensureCombosLoaded } from "../state/models-sync.js";
import { keyFormTemplate, buildKeyBodyFromForm } from "../handlers/key-handlers.js";
import { renderAllowedModelsChips, renderBlacklistedModelsChips, renderAllowedCombosChips } from "../components/model-picker.js";
import { showToast } from "../components/toast.js";
import { t } from "../i18n/index.js";
import type { ApiKey, KeyQuotaResponse, Model, Provider } from "../lib/types/api.js";

export async function mountKeyDetail(
  id: number,
  target?: HTMLElement | null,
): Promise<() => void> {
  const container = target ?? document.getElementById("main");
  if (!container || !container.isConnected) return () => {};
  let disposed = false;
  let saving = false;
  let cleanup = (): void => {};
  render(html`<p role="status">${t("workspace.loading")}</p>`, container);
  try {
    const [loaded, quotaResp] = await Promise.all([
      api(`/keys/${id}`) as Promise<ApiKey>,
      api(`/keys/${id}/quota`) as Promise<KeyQuotaResponse>,
      state.modelsComplete ? Promise.resolve() : api("/models").then(models => {
        state.models = models as Model[];
        state.modelsComplete = true;
      }),
      state.providers.length ? Promise.resolve() : api("/providers").then(providers => { state.providers = providers as Provider[]; }),
      ensureCombosLoaded(),
    ]);
    if (disposed || !container.isConnected) return () => {};
    let key = loaded;
    let expiryBaseline: string[] = [];
    const expiryValues = (form: HTMLFormElement): string[] => [
      (form.elements.namedItem("expires_amount") as HTMLInputElement | null)?.value ?? "",
      (form.elements.namedItem("expires_unit") as HTMLSelectElement | null)?.value ?? "",
    ];
    const status = (): string => t(key.revoked_at ? "keys.list.status.revoked" : key.is_active ? "keys.list.status.active" : "keys.list.status.inactive");
    // Compact one-line dates keep the summary strip to a single row; the full
    // timestamp stays available on hover via `title`.
    const shortDate = (iso?: string | null): string => iso
      ? new Date(iso).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" })
      : "—";
    const fullDate = (iso?: string | null): string => iso ? new Date(iso).toLocaleString() : "";
    const statusClass = key.revoked_at ? "is-revoked" : key.is_active ? "is-active" : "is-inactive";
    const save = async (event: Event): Promise<void> => {
      const form = event.target;
      if (!(form instanceof HTMLFormElement) || saving || disposed || !container.isConnected) return;
      const body = buildKeyBodyFromForm(form);
      if (!body) return;
      // Opening an existing key must not round or extend its absolute expiry.
      if (expiryValues(form).every((v, i) => v === expiryBaseline[i])) body.expires_at = key.expires_at;
      saving = true;
      const submit = form.querySelector<HTMLButtonElement>('button[type="submit"]');
      if (submit) submit.disabled = true;
      try {
        await api(`/keys/${id}`, { method: "PATCH", body: JSON.stringify(body) });
        const updated = await api(`/keys/${id}`) as ApiKey;
        if (disposed || !container.isConnected) return;
        key = updated;
        expiryBaseline = expiryValues(form);
        state.apiKeys = state.apiKeys.map(row => row.id === key.id ? key : row);
        showToast(t("keys.detail.saved"), "success");
        requestUpdate();
        queueMicrotask(() => { if (!disposed && container.isConnected) expiryBaseline = expiryValues(form); });
      } catch (error: unknown) {
        if (!disposed && container.isConnected) showToast(error instanceof Error ? error.message : String(error), "error");
      } finally {
        saving = false;
        if (submit) submit.disabled = false;
      }
    };
    cleanup = mountView(container, () => html`<section class="key-detail">
      <header class="cx-key-header">
        <a href="#/keys" class="back-link cx-back-link">${t("keys.detail.back")}</a>
        <h1 title=${t("keys.detail.subtitle")} aria-description=${t("keys.detail.subtitle")}>${key.label || `#${key.id}`}</h1>
        <a class="btn cx-key-usage" href=${`#/keys/${id}/usage`}>${t("keys.list.btn.usage")}</a>
      </header>
      <dl class="cx-key-summary">
        <div class="cx-key-stat cx-key-stat--prefix"><dt>${t("keys.detail.prefix")}</dt><dd><code>${key.key_prefix || "—"}</code></dd></div>
        <div class=${`cx-key-stat cx-key-stat--status ${statusClass}`}><dt>${t("keys.detail.status")}</dt><dd>${status()}</dd></div>
        <div class="cx-key-stat"><dt>${t("keys.detail.created")}</dt><dd title=${fullDate(key.created_at)}>${shortDate(key.created_at)}</dd></div>
        <div class="cx-key-stat"><dt>${t("keys.detail.last_used")}</dt><dd title=${fullDate(key.last_used_at)}>${shortDate(key.last_used_at)}</dd></div>
      </dl>
      ${keyFormTemplate({ mode: "edit", key, quota: quotaResp ? (quotaResp.caps ?? null) : null, wrapper: container, inline: true, onSubmit: event => { void save(event); }, onCancel: () => { location.hash = "#/keys"; } })}
    </section>`);
    renderAllowedModelsChips();
    renderBlacklistedModelsChips();
    renderAllowedCombosChips();
    const form = container.querySelector("form");
    if (form) expiryBaseline = expiryValues(form);
  } catch (error: unknown) {
    if (!disposed && container.isConnected) render(html`<div class="banner banner-error" role="alert">${error instanceof Error ? error.message : String(error)}</div><a href="#/keys">${t("keys.detail.back")}</a>`, container);
  }
  return () => { disposed = true; cleanup(); };
}
