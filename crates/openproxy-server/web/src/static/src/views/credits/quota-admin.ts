// views/credits/quota-admin.ts — registration quota ("registrados / máximo").
//
// A cap on distinct sold keys (`kind='customer'`): once the count reaches the
// max, the checkout rejects NEW orders ("quota full") while already-created
// orders keep completing — nobody who holds a QR gets blocked after paying.
// The max is hot-editable from this card: it lands in app_config
// (`credits_registration_quota`) and the checkout re-reads it per order,
// so changes take effect with no restart.

import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import { requestRefresh } from "./shared.js";

export interface RegistrationQuota {
  registered: number;
  /** null = unlimited (unset or non-positive cap). */
  max: number | null;
}

let state: RegistrationQuota | null = null;
let loading = false;
let editing = false;

/** Plan purchases on EXISTING keys (renew / change plan). null = load failed. */
let renewals: boolean | null = null;
let togglingRenewals = false;

/** Always re-fetch: the caller (view loader) wants fresh numbers per render. */
export async function refreshRegistrationQuota(): Promise<void> {
  if (loading) return;
  loading = true;
  const [quotaRes, renewalsRes] = await Promise.allSettled([
    api("/credits/registration-quota"),
    api("/credits/renewals"),
  ]);
  if (quotaRes.status === "fulfilled") {
    state = quotaRes.value as RegistrationQuota;
  } else {
    showToast(
      quotaRes.reason instanceof Error ? quotaRes.reason.message : "quota load failed",
      "error",
    );
  }
  renewals = renewalsRes.status === "fulfilled" ? ((renewalsRes.value as { enabled?: boolean }).enabled ?? true) : null;
  loading = false;
}

async function toggleRenewals(): Promise<void> {
  if (togglingRenewals || renewals === null) return;
  togglingRenewals = true;
  const next = !renewals;
  try {
    await api("/credits/renewals", {
      method: "POST",
      body: JSON.stringify({ enabled: next }),
    });
    renewals = next;
    showToast(
      next
        ? "Plan purchases on existing keys OPEN"
        : "Plan purchases on existing keys CLOSED — pending orders still complete",
      "success",
    );
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "toggle failed", "error");
  } finally {
    togglingRenewals = false;
  }
}

/** Parse the editor input: blank or 0 clears the cap (unlimited). */
function parseMax(raw: string): number | null {
  const t = raw.trim();
  if (!t || t === "0") return null;
  const n = Number(t);
  if (!Number.isInteger(n) || n < 0) throw new Error("max must be a non-negative integer");
  return n > 0 ? n : null;
}

async function save(): Promise<void> {
  const input = document.getElementById("reg-quota-max") as HTMLInputElement | null;
  if (!input) return;
  let max: number | null;
  try {
    max = parseMax(input.value);
  } catch (err) {
    showToast(err instanceof Error ? err.message : "invalid max", "error");
    return;
  }
  try {
    await api("/credits/registration-quota", {
      method: "POST",
      body: JSON.stringify({ max }),
    });
    editing = false;
    showToast(
      max == null ? "Registration quota removed (unlimited)" : `Registration quota set to ${max}`,
      "success",
    );
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "quota save failed", "error");
  }
}

export function renderRegistrationQuota(): TemplateResult {
  if (!state) return html``;
  const max = state.max;
  const full = max != null && state.registered >= max;
  return html`
    <section class="card" style="margin-bottom:16px">
      <div class="card-title">Registrations</div>
      <div class="card-body">
        ${editing
          ? html`
              <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
                <input
                  id="reg-quota-max"
                  type="number"
                  min="0"
                  step="1"
                  value=${max ?? ""}
                  placeholder="unlimited"
                  style="width:110px"
                />
                <button class="btn primary" style="margin-top:0" @click=${save}>Save</button>
                <button
                  class="btn"
                  style="margin-top:0"
                  @click=${() => {
                    editing = false;
                    requestRefresh();
                  }}
                >
                  Cancel
                </button>
              </div>
              <div style="color:var(--color-text-muted);font-size:12px;margin-top:6px">
                Blank or 0 = no cap. Applies immediately; only blocks NEW orders.
              </div>`
          : html`
              <div style="display:flex;gap:12px;align-items:center;flex-wrap:wrap">
                <span style="font-size:22px;font-weight:700;font-variant-numeric:tabular-nums">
                  ${state.registered} / ${max ?? "∞"}
                </span>
                <span style="color:var(--color-text-muted);font-size:13px">
                  registrados / máximo${full ? " — FULL: new signups rejected" : ""}
                </span>
                <button
                  class="btn"
                  style="margin-top:0"
                  @click=${() => {
                    editing = true;
                    requestRefresh();
                  }}
                >
                  Edit
                </button>
              </div>
              <div style="color:var(--color-text-muted);font-size:12px;margin-top:6px">
                Cap on distinct sold keys. When full, the checkout blocks NEW orders; orders
                created before keep completing, so a paid QR never bounces.
              </div>`}
      </div>
    </section>
  `;
}

/** Hot toggle: can keys that ALREADY exist buy / renew plans? */
export function renderRenewalsCard(): TemplateResult {
  if (renewals === null) return html``;
  const on = renewals;
  return html`
    <section class="card" style="margin-bottom:16px">
      <div class="card-title">Existing keys</div>
      <div class="card-body">
        <div style="display:flex;gap:16px;align-items:center;justify-content:space-between;flex-wrap:wrap">
          <div style="display:flex;flex-direction:column;gap:2px;min-width:220px">
            <span style="font-weight:600">Plan purchases on existing keys</span>
            <span style="color:var(--color-text-muted);font-size:13px">
              ${on
                ? "Open: keys with an API key can buy or renew plans in the checkout."
                : "Closed: existing keys cannot buy more plans. Pending orders still complete; new registrations follow the quota."}
            </span>
          </div>
          <button
            type="button"
            role="switch"
            aria-checked=${on ? "true" : "false"}
            class="toggle-btn ${on ? "on" : "off"}"
            ?disabled=${togglingRenewals}
            @click=${() => void toggleRenewals()}
          >
            <span class="toggle-thumb"></span>
          </button>
        </div>
      </div>
    </section>
  `;
}
