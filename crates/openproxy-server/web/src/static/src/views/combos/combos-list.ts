// views/combos/combos-list.ts — responsive table listing for combos: custom
// user-created combos separate from auto-generated provider combos.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../../state/index.js";
import { icons } from "../../lib/icons.js";
import { t } from "../../i18n/index.js";
import { PRIORITY_MODE_LABELS, PRIORITY_MODE_TOOLTIPS } from "../../lib/constants.js";
import { showCreateCombo, cloneCombo, deleteCombo } from "../../handlers/combo-handlers.js";
import type { Combo, PriorityMode } from "../../lib/types/api.js";
import { renderResponsiveCardTable, type ResponsiveColumn } from "../../components/render-responsive-card-table.js";
import { requestUpdate } from "../../state/reactive.js";

const priorityModeOf = (c: Combo): PriorityMode => (c.priority_mode ?? "strict") as PriorityMode;

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
  return n >= 1_000 ? (n / 1_000).toFixed(0) + "k" : String(n);
}

let comboSearchQuery = "";
let comboStrategyFilter = "all";
let comboPriorityModeFilter = "all";

function onComboSearch(e: Event): void {
  comboSearchQuery = (e.target as HTMLInputElement).value;
  requestUpdate();
}

function onStrategyFilterChange(e: Event): void {
  comboStrategyFilter = (e.target as HTMLSelectElement).value;
  requestUpdate();
}

function onPriorityModeFilterChange(e: Event): void {
  comboPriorityModeFilter = (e.target as HTMLSelectElement).value;
  requestUpdate();
}

function getCombosColumns(): ResponsiveColumn<Combo>[] {
  return [
    {
      key: "name",
      label: "Name",
      render: (c) => {
        const pm = priorityModeOf(c);
        return html`
          <div class="combo-table-name-wrap">
            <div class="combo-table-title-row">
              <a class="combo-table-name-link" href="#/combos/${c.id}">
                <strong class="combo-name-title">${c.name}</strong>
              </a>
              <span class="muted combo-id-badge">#${c.id}</span>
            </div>
            <div class="combo-table-badges-row">
              ${pm === "decision" && c.decision_model ? html`
                <span class="subcombo-meta-pill decision" title="Decision model: ${c.decision_model}">
                  Jev: <strong>${c.decision_model}</strong>
                </span>
              ` : ""}
              ${c.preventive_rate_limit ? html`
                <span class="chip chip-rl" title="Predictive Rate Limiting enabled">${icons.lightning()} RL</span>
              ` : ""}
              ${c.sanitize_response ? html`
                <span class="chip chip-sanitize" title="Sanitize Responses: rewrites model and purges reasoning metadata">${icons.check()} Sanitize</span>
              ` : ""}
            </div>
          </div>
        `;
      },
    },
    {
      key: "strategy",
      label: "Strategy",
      render: (c) => html`<span class="chip chip-strategy" title="Routing Strategy: ${c.strategy}">${c.strategy}</span>`,
    },
    {
      key: "priority_mode",
      label: "Priority Mode",
      render: (c) => {
        const pm = priorityModeOf(c);
        return html`<span class="chip chip-pm" title=${PRIORITY_MODE_TOOLTIPS[pm] || ""}>${PRIORITY_MODE_LABELS[pm] || pm}</span>`;
      },
    },
    {
      key: "race_size",
      label: "Race",
      render: (c) => html`<span class="combo-race-badge" title="Race size: ${c.race_size}">race ${c.race_size}</span>`,
    },
    {
      key: "context_window",
      label: "Context",
      render: (c) => html`<span class="combo-context-value">${c.context_window != null ? formatTokens(c.context_window) : "auto"}</span>`,
    },
    {
      key: "actions",
      label: "Actions",
      render: (c) => html`
        <div class="combo-table-actions">
          <a class="button small primary" href="#/combos/${c.id}" title="View details and targets">
            ${icons.pencil()} Details
          </a>
          <button type="button" class="small" title="Clone combo" @click=${() => cloneCombo(c.id, c.name)}>
            ${icons.copy()} Clone
          </button>
          <button type="button" class="small danger" title="Delete combo" @click=${() => deleteCombo(c.id)}>
            ${icons.trash()}
          </button>
        </div>
      `,
    },
  ];
}

export function renderCombosList(): TemplateResult {
  const list = state.combos || [];
  const q = comboSearchQuery.trim().toLowerCase();

  // Extract unique strategies & priority modes for filters
  const uniqueStrategies = Array.from(new Set(list.map((c) => c.strategy).filter(Boolean)));
  const uniquePriorityModes = Array.from(new Set(list.map((c) => priorityModeOf(c)).filter(Boolean)));

  const matches = (c: Combo): boolean => {
    if (comboStrategyFilter !== "all" && c.strategy !== comboStrategyFilter) return false;
    if (comboPriorityModeFilter !== "all" && priorityModeOf(c) !== comboPriorityModeFilter) return false;
    if (!q) return true;
    return (
      c.name.toLowerCase().includes(q) ||
      String(c.id).includes(q) ||
      (c.decision_model ? c.decision_model.toLowerCase().includes(q) : false) ||
      (c.strategy ? c.strategy.toLowerCase().includes(q) : false) ||
      (c.priority_mode ? c.priority_mode.toLowerCase().includes(q) : false)
    );
  };

  const filteredCustom = list.filter(matches);
  const columns = getCombosColumns();
  const isFiltered = q.length > 0 || comboStrategyFilter !== "all" || comboPriorityModeFilter !== "all";

  return html`
    <div class="combos-view-container">
      <div class="page-header combos-page-header">
        <div class="header-title-wrap">
          <h2>${t("combos.grid.title")}</h2>
          <span class="muted combos-count-badge">
            ${isFiltered ? `${filteredCustom.length} / ${list.length}` : `${list.length}`} total
          </span>
        </div>
        <div class="actions combos-header-actions">
          <div class="combos-filter-bar">
            <div class="combos-search-wrap">
              <input
                type="search"
                class="cw-input combos-search-input"
                placeholder="Filter combos (e.g. nerd, gemma)..."
                .value=${comboSearchQuery}
                @input=${onComboSearch}>
            </div>
            ${uniqueStrategies.length > 1 ? html`
              <select class="combos-filter-select" .value=${comboStrategyFilter} @change=${onStrategyFilterChange} title="Filter by Strategy">
                <option value="all">All Strategies</option>
                ${uniqueStrategies.map((s) => html`<option value=${s} ?selected=${comboStrategyFilter === s}>${s}</option>`)}
              </select>
            ` : ""}
            ${uniquePriorityModes.length > 1 ? html`
              <select class="combos-filter-select" .value=${comboPriorityModeFilter} @change=${onPriorityModeFilterChange} title="Filter by Priority Mode">
                <option value="all">All Modes</option>
                ${uniquePriorityModes.map((pm) => html`<option value=${pm} ?selected=${comboPriorityModeFilter === pm}>${PRIORITY_MODE_LABELS[pm] || pm}</option>`)}
              </select>
            ` : ""}
          </div>
          <button class="primary combo-create-btn" @click=${() => showCreateCombo()}>
            ${icons.plus()} ${t("combos.grid.create")}
          </button>
        </div>
      </div>

      ${list.length === 0 ? html`<p class="empty">${t("combos.grid.empty")}</p>` : html`
        <div class="combos-list-container">
          <section class="combos-section custom-combos-section">
            ${renderResponsiveCardTable<Combo>({
              columns,
              rows: filteredCustom,
              rowKey: (c) => c.id,
              className: "combos-table custom-combos-table",
              emptyMessage: isFiltered
                ? `No combos matching your filters.`
                : "No combos created yet. Click 'Create combo' to add one.",
            })}
          </section>
        </div>
      `}
    </div>
  `;
}
