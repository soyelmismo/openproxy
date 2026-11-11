// views/config/credits-gateway.ts — runtime master switch for the credits
// payment gateway. Off = nobody can buy plans or top up (buying surfaces
// return 403 gateway_disabled and the checkout UI closes itself). Balance
// history stays readable; spending an existing balance is unaffected.
import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { card } from "./shared.js";

let gatewayEnabled: boolean | null = null;
let toggling = false;

/** Fetch `/admin/credits/gateway`; called by mountConfig. Non-fatal. */
export async function loadCreditsGatewayState(): Promise<void> {
  try {
    const res = await api("/credits/gateway") as { enabled?: boolean };
    gatewayEnabled = res.enabled ?? true;
  } catch {
    gatewayEnabled = null; // endpoint missing → hide the card
  }
}

async function toggleGateway(): Promise<void> {
  if (toggling || gatewayEnabled === null) return;
  toggling = true;
  const next = !gatewayEnabled;
  try {
    await api("/credits/gateway", {
      method: "POST",
      body: JSON.stringify({ enabled: next }),
    });
    gatewayEnabled = next;
    showToast(next ? "Payment gateway open" : "Payment gateway closed", "success");
    requestUpdate();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "toggle failed", "error");
  } finally {
    toggling = false;
  }
}

export function renderCreditsGatewayCard(): TemplateResult {
  if (gatewayEnabled === null) return html``;
  const on = gatewayEnabled;
  return card("Credits gateway", html`
    <div class="config-toggle-row">
      <div class="config-toggle-info">
        <span class="config-label">Payment gateway</span>
        <span class="config-help">${on
          ? "Open: anonymous purchases, packages and top-ups are accepted."
          : "Closed: nobody can buy plans or top up. Existing balances still spend."}</span>
      </div>
      <button type="button" role="switch" aria-checked=${on ? "true" : "false"}
        class="toggle-btn ${on ? "on" : "off"}" ?disabled=${toggling}
        @click=${() => void toggleGateway()}>
        <span class="toggle-thumb"></span>
      </button>
    </div>
  `);
}
