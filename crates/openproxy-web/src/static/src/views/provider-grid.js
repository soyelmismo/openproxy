// views/provider-grid.js — the providers landing page. Renders a
// card grid with per-provider stats (accounts, models, active
// models) plus the "Refresh all" and "+ Add provider" toolbar.
//
// Per spec §3 + §13.8 we do not use inline onclick; every
// interactive element is wired via data-action / data-arg-N and
// dispatched through the central shim in app.js.

import { state } from "../state/index.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";

// Per-provider rollups used by the card grid. Built once per
// render so the card HTML can read precomputed counts in O(1).
function computeProviderStats() {
  const stats = {};
  for (const p of (state.providers || [])) {
    const providerAccounts = (state.accounts || []).filter((a) => a.provider_id === p.id);
    const providerModels = (state.models || []).filter((m) => m.provider_id === p.id);
    stats[p.id] = {
      accounts: providerAccounts,
      models: providerModels,
      active_models: providerModels.filter((m) => m.active).length,
    };
  }
  return stats;
}

// Three built-in providers get distinct visual markers so the user
// can scan the grid quickly. Custom providers fall back to the
// first letter of their id (uppercased), which keeps the icon
// area from looking broken while still being informative.
function getProviderIconHtml(providerId, format) {
  const knownLogos = {
    openrouter: "🟢",
    minimax: "🟡",
    "opencode-zen": "🟣",
  };
  const glyph = knownLogos[providerId] || ((providerId[0] || "?").toUpperCase());
  return `<span class="provider-emoji">${glyph}</span>`;
}

function renderProviderCard(p, s) {
  const unhealthyAccs = s.accounts.filter((a) => a.health_status === "unhealthy").length;
  // Card classes encode the visual state:
  // - `has-errors`: red left stripe when at least one account is unhealthy.
  // - `inactive`:   dimmed card when the provider has been deactivated
  //                 (its name picks up a small "(inactive)" suffix).
  // The two flags are independent — an inactive provider with healthy
  // accounts is just dimmed, while an active provider with unhealthy
  // accounts gets the red stripe.
  const cardClasses = [
    "provider-card",
    unhealthyAccs > 0 ? "has-errors" : "",
    p.active ? "" : "inactive",
  ].filter(Boolean).join(" ");
  return `
    <a href="#/providers/${encodeURIComponent(p.id)}" class="${cardClasses}">
      <div class="provider-card-header">
        <div class="provider-icon" data-format="${escapeAttr(p.format)}">${getProviderIconHtml(p.id, p.format)}</div>
        <div class="provider-info">
          <h3>${escapeHtml(p.name)}${p.active ? "" : ' <small class="inactive-suffix">(inactive)</small>'}</h3>
          <code>${escapeHtml(p.id)}</code>
        </div>
      </div>
      <div class="provider-card-body">
        <div class="capabilities">
          <span class="chip" data-format="${escapeAttr(p.format)}">${escapeHtml(p.format)}</span>
          <span class="chip">${escapeHtml(p.auth_type)}</span>
        </div>
      </div>
      <div class="provider-card-footer">
        <div class="stat">
          <label>Accounts</label>
          <value>${s.accounts.length}</value>
          ${unhealthyAccs > 0 ? `<span class="badge badge-error">${unhealthyAccs} down</span>` : ""}
        </div>
        <div class="stat">
          <label>Models</label>
          <value>${s.active_models}/${s.models.length}</value>
        </div>
      </div>
    </a>
  `;
}

export function renderProviderGrid() {
  const list = state.providers || [];
  const stats = computeProviderStats();
  let cardsHtml = "";
  if (list.length === 0) {
    cardsHtml = `
      <div class="empty-state">
        <h3>No providers configured</h3>
        <p>Add a provider to get started.</p>
        <button class="primary" data-action="showCreateProvider">+ Add provider</button>
      </div>
    `;
  } else {
    for (const p of list) cardsHtml += renderProviderCard(p, stats[p.id]);
  }
  const header = pageHeader({
    title: "Providers",
    actions: `
      <button data-action="refreshAllProviders">Refresh all</button>
      <button class="primary" data-action="showCreateProvider">+ Add provider</button>
    `,
  });
  const main = document.getElementById("main");
  main.innerHTML = header + `<div class="provider-grid">${cardsHtml}</div>`;
}
