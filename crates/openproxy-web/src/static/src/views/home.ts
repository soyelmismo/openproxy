// views/home.ts — landing view. Summarises the current state of
// the proxy: total providers, accounts, models, combos, and the
// overall health. A "Recent usage" mini-block pulls the last
// 5 rows from /usage/recent.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml } from "../lib/escape.js";
import { statusPillClass } from "../lib/constants.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import type { Account, Combo, Model, Provider, RecentUsageRow } from "../lib/types/api.js";

export async function mountHome(): Promise<void> {
  const main = document.getElementById("main");
  if (!main) return;
  main.innerHTML = pageHeader({ title: "Overview" }) +
    `<div class="loading">Loading...</div>`;
  // The home view used to read counts and health straight from
  // `state.*`, on the assumption that the 3s background poll had
  // already populated them. After the poll stopped re-rendering
  // (so it wouldn't destroy input focus) that assumption broke:
  // a cold paint of `#/` would show all counts as 0 and
  // `Status: —`, and the user had to wait for the next poll to
  // re-render — which never came. So now the home view fetches
  // what it needs at mount time, just like providers.ts does.
  // The state cache is still consulted first as a fast path for
  // the common "user just navigated here from another view that
  // already fetched" case.
  // The recent-usage endpoint can return an error (e.g. when the
  // usage table is empty/missing). The `.catch` returns an empty
  // array so the rest of the render keeps working. We cast the
  // whole tuple to keep the destructure types narrow (the un-narrowed
  // inferred tuple is `unknown[]`-flavoured because of the catch).
  const [providers, accounts, models, combos, keys, recent] = await Promise.all([
    (state.providers && state.providers.length) ? Promise.resolve(state.providers) : api("/providers") as Promise<Provider[]>,
    (state.accounts  && state.accounts.length)  ? Promise.resolve(state.accounts)  : api("/accounts") as Promise<Account[]>,
    (state.models    && state.models.length)    ? Promise.resolve(state.models)    : api("/models") as Promise<Model[]>,
    (state.combos    && state.combos.length)    ? Promise.resolve(state.combos)    : api("/combos") as Promise<Combo[]>,
    (state.apiKeys   && state.apiKeys.length)   ? Promise.resolve(state.apiKeys)   : api("/keys") as Promise<unknown[]>,
    api("/usage/recent?limit=5").catch((): RecentUsageRow[] => []),
  ]) as readonly [Provider[], Account[], Model[], Combo[], unknown[], RecentUsageRow[]];
  // Backfill the state caches so the next navigation to home (or
  // any view that reads these) gets the data without a re-fetch.
  // The bg-poll will overwrite these with fresher values on its
  // 3s tick — that's expected.
  if (providers) state.providers = providers;
  if (accounts)  state.accounts  = accounts;
  if (models)    state.models    = models;
  if (combos)    state.combos    = combos;
  if (keys)      state.apiKeys   = keys;
  // Pull the health from the state cache (the bg-poll's
  // healthTick populates this on a 1s tick). If the user landed
  // on home on a fresh page load, the health may be null until
  // the first healthTick fires; in that case we kick one off
  // explicitly so the "Health" card shows real data on the first
  // paint rather than "—".
  if (!state.health) {
    try { state.health = await api("/health") as { status: string; message?: string }; } catch (_e) { /* keep null */ }
  }
  const summary = {
    providers: (providers || []).length,
    accounts:  (accounts  || []).length,
    models:    (models    || []).length,
    combos:    (combos    || []).length,
    keys:      (keys      || []).length,
  };
  const healthCard = card("Health", `
    <p>Status: <strong>${state.health ? escapeHtml(state.health.status) : "—"}</strong></p>
    ${state.health && state.health.message ? `<p class="muted">${escapeHtml(state.health.message)}</p>` : ""}
  `);
  const countsCard = card("Inventory", `
    <div class="metrics">
      <div><label>Providers</label><div class="value">${summary.providers}</div></div>
      <div><label>Accounts</label><div class="value">${summary.accounts}</div></div>
      <div><label>Models</label><div class="value">${summary.models}</div></div>
      <div><label>Combos</label><div class="value">${summary.combos}</div></div>
      <div><label>API keys</label><div class="value">${summary.keys}</div></div>
    </div>
  `);
  const recentRows = (recent || []).map((r) => {
    const cls = statusPillClass(r.status_code);
    return `<tr>
      <td>${escapeHtml(r.created_at || "")}</td>
      <td>${escapeHtml(r.provider_id || "")}</td>
      <td>${escapeHtml(r.upstream_model_id || "")}</td>
      <td><span class="status-pill ${cls}">${r.status_code ?? "—"}</span></td>
      <td>${r.total_ms || 0}ms</td>
      <td>$${(r.cost_usd || 0).toFixed(4)}</td>
    </tr>`;
  }).join("");
  const recentCard = card("Recent usage", `
    ${recent.length ? `<table>
      <thead><tr><th>Time</th><th>Provider</th><th>Model</th><th>Status</th><th>Latency</th><th>Cost</th></tr></thead>
      <tbody>${recentRows}</tbody>
    </table>` : `<p class="empty">No recent requests yet.</p>`}
    <p style="margin-top:0.5rem;"><a href="#/logs">Open live logs →</a></p>
  `);
  main.innerHTML = pageHeader({ title: "Overview" }) + healthCard + countsCard + recentCard;
}
