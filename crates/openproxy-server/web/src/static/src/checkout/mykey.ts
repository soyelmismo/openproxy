// checkout/mykey.ts — the "My key" tab: balance, lots, history and top-up
// authenticated with the buyer's own key (Bearer, `chat` scope).

import { $, hide, show } from "./dom.js";
import { getAuth, getJson, postJson, type BalanceResponse, type CheckoutOrder, type TxRow } from "./api.js";
import { renderPayStep, startCountdown, startPolling } from "./pay-step.js";
import { parseSelection } from "./catalog.js";

function keyOf(): string {
  return ($<HTMLInputElement>("mykey")).value.trim();
}

export function initMyKey(): void {
  $("mykey-load").addEventListener("click", () => {
    const key = keyOf();
    const errEl = $("mykey-err");
    errEl.classList.add("hidden");
    if (!key) return;
    getAuth<BalanceResponse>("v1/credits/me/balance", key)
      .then((b) => {
        $("mykey-data").classList.remove("hidden");
        $("my-balance").textContent =
          "$" + parseFloat(b.balance_usd || "0").toFixed(2) + " USDC";
        const lots = (b.lots || []).map(
          (l) => "USDC " + l.remaining_usd + " — expires " + (l.expires_at || "never"),
        );
        $("my-expiry").textContent =
          lots.join(" | ") || "No active credit balance on this key yet.";
        return getJson<TxRow[]>("v1/credits/me/transactions?limit=50", {
          headers: { Authorization: "Bearer " + key },
        });
      })
      .then((txs) => {
        const el = $("my-history");
        el.innerHTML = "";
        (txs || []).slice(0, 20).forEach((t) => {
          const row = document.createElement("div");
          row.style.marginBottom = "4px";
          const link = t.basescan_url
            ? '<a style="color:var(--color-primary)" href="' + t.basescan_url +
              '" target="_blank" rel="noopener">view on BaseScan</a>'
            : "—";
          row.innerHTML =
            t.created_at + " · " + t.kind + " · " +
            (t.kind === "topup" ? "+" : "") + t.amount_usd + " USDC · " +
            t.status + " · " + link;
          el.appendChild(row);
        });
        if (!txs || txs.length === 0) el.textContent = "No transactions yet.";
      })
      .catch((e: Error) => {
        errEl.textContent = e.message;
        errEl.classList.remove("hidden");
      });
  });

  $("my-topup").addEventListener("click", () => {
    const key = keyOf();
    const errEl = $("mykey-err");
    errEl.classList.add("hidden");
    const payload = parseSelection(String($<HTMLSelectElement>("my-usd").value));
    postJson<CheckoutOrder>("v1/credits/me/topup", payload, {
      Authorization: "Bearer " + key,
    })
      .then((res) => {
        if (!res.ok) throw new Error(res.body.error?.message || "top-up failed");
        hide("step-mykey");
        show("step-pay");
        renderPayStep(res.body);
        startCountdown();
        startPolling();
      })
      .catch((e: Error) => {
        errEl.textContent = e.message;
        errEl.classList.remove("hidden");
      });
  });
}
