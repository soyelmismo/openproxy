// checkout/catalog.ts — loads the package offers and fills both selects.
//
// Presets carry the leverage ("pay $5 → spend $30") and the tier name for
// display. The TTL select hides when presets exist: duration is priced
// through the presets (longer hold = better per-day rate), so free amounts
// ride the shortest lifetime and nobody buys a 30-day balance at the 7-day
// price outside the presets.

import { $, err, hide } from "./dom.js";
import { getJson, type PackagesResponse } from "./api.js";

export function loadCatalog(): void {
  getJson<PackagesResponse>("v1/credits/packages", {
    headers: { Accept: "application/json" },
  })
    .then((p) => {
      if (!p.enabled) {
        throw new Error("on-chain top-ups are not enabled on this server");
      }
      const packages = p.packages || [];
      if (packages.some((pk) => pk.package_id != null)) {
        const ttlSel = $<HTMLSelectElement>("ttl");
        const ttlWrap = ttlSel.parentElement;
        if (ttlWrap) ttlWrap.style.display = "none";
      }
      const usd = $<HTMLSelectElement>("usd");
      const myUsd = $<HTMLSelectElement>("my-usd");
      if (myUsd) myUsd.innerHTML = "";
      packages.forEach((pk) => {
        const leveraged = pk.credit_usd != null && pk.credit_usd !== pk.usd;
        const text = pk.package_id != null
          ? pk.label + " — pay $" + pk.usd +
            (leveraged ? " → spend $" + pk.credit_usd : "") +
            " / " + pk.ttl_days + " days" + (pk.tier ? " · " + pk.tier : "")
          : pk.label + " — $" + pk.usd;
        const o = document.createElement("option");
        const mo = document.createElement("option");
        if (pk.package_id != null) {
          o.value = "pkg:" + pk.package_id;
          mo.value = "pkg:" + pk.package_id;
        } else {
          o.value = String(pk.usd);
          mo.value = String(pk.usd);
        }
        o.textContent = text;
        mo.textContent = text;
        usd.appendChild(o);
        if (myUsd) myUsd.appendChild(mo);
      });
      const ttl = $<HTMLSelectElement>("ttl");
      p.ttl_days.forEach((d) => {
        const o = document.createElement("option");
        o.value = String(d);
        o.textContent = d + " days";
        ttl.appendChild(o);
      });
      ttl.value = "15";
    })
    .catch((e: Error) => {
      err("page-err", "Could not load packages: " + e.message);
      hide("step-buy");
    });
}

/** "pkg:12" | "5" → the POST body the gateway expects. */
export function parseSelection(value: string): Record<string, unknown> {
  return value.indexOf("pkg:") === 0
    ? { package_id: parseInt(value.slice(4), 10) }
    : { usd: parseFloat(value) };
}
