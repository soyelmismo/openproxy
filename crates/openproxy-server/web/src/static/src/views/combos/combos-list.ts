// views/combos/combos-list.ts — table-based responsive listing for combos.
//
// Separates custom user-created combos from auto-generated provider combos,
// providing real-time filtering, strategy/mode badges, and quick actions.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../../state/index.js";
import { icons } from "../../lib/icons.js";
import { t } from "../../i18n/index.js";
import { PRIORITY_MODE_LABELS, PRIORITY_MODE_TOOLTIPS } from "../../lib/constants.js";
import { showCreateCombo } from "../../handlers/combo-handlers.js";
import type { Combo, PriorityMode } from "../../lib/types/api.js";
import { renderResponsiveCardTable, type ResponsiveColumn } from "../../components/render-responsive-card-table.js";
import { requestUpdate } from "../../state/reactive.js";

const priorityModeOf = (c: Combo): PriorityMode => (c.priority_mode ?? "strict") as PriorityMode;

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
  return n >= 1_000 ? (n / 1_000).toFixed(0) + "k" : String(n);
}

let comboSearchQuery = "";
let isAutoExpanded = false;

function onComboSearch(e: Event): void {
  comboSearchQuery = (e.target as HTMLInputElement).value;
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
            <a class="combo-table-name-link" href="#/combos/${c.id}">
              <strong class="combo-name-title">${c.name}</strong>
            </a>
            <span class="muted combo-id-badge">#${c.id}</span>
            ${pm === "decision" && c.decision_model ? html`
              <span class="subcombo-meta-pill decision" title="Decision model: ${c.decision_model}">
                Jev: <strong>${c.decision_model}</strong>
              </span>
            ` : ""}
            ${c.preventive_rate_limit ? html`
              <span class="chip chip-rl" title="Predictive Rate Limiting enabled">${icons.lightning()} RL</span>
            ` : ""}
          </div>
        `;
      },
    },
    {
      key: "strategy",
      label: "Strategy",
      render: (c) => html`<span class="chip chip-strategy">${c.strategy}</span>`,
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
      render: (c) => html`<span>${c.context_window != null ? formatTokens(c.context_window) : "auto"}</span>`,
    },
    {
      key: "actions",
      label: "Actions",
      render: (c) => html`
        <div class="combo-table-actions">
          <a class="button small primary" href="#/combos/${c.id}" title="View details and targets">
            ${icons.pencil()} Details
          </a>
        </div>
      `,
    },
  ];
}

export function renderCombosList(): TemplateResult {
  const list = state.combos || [];
  const q = comboSearchQuery.trim().toLowerCase();

  const matches = (c: Combo): boolean => {
    if (!q) return true;
    return (
      c.name.toLowerCase().includes(q) ||
      String(c.id).includes(q) ||
      (c.decision_model ? c.decision_model.toLowerCase().includes(q) : false) ||
      (c.strategy ? c.strategy.toLowerCase().includes(q) : false) ||
      (c.priority_mode ? c.priority_mode.toLowerCase().includes(q) : false)
    );
  };

  const allCustom = list.filter((c) => !c.name.startsWith("auto:"));
  const allAuto = list.filter((c) => c.name.startsWith("auto:"));

  const filteredCustom = allCustom.filter(matches);
  const filteredAuto = allAuto.filter(matches);

  const columns = getCombosColumns();
  const hasSearch = q.length > 0;
  const showAutoAccordion = allAuto.length > 0;
  const isAccordionOpen = hasSearch || isAutoExpanded;

  return html`
    <div class="page-header">
      <div class="header-title-wrap" style="display: flex; align-items: baseline; gap: var(--space-2);">
        <h2>${t("combos.grid.title")}</h2>
        <span class="muted" style="font-size: var(--fs-xs);">
          (${allCustom.length} custom · ${allAuto.length} auto)
        </span>
      </div>
      <div class="actions">
        <div class="combos-search-wrap">
          <input
            type="search"
            class="cw-input combos-search-input"
            placeholder="Filter combos (e.g. nerd, gemma)..."
            .value=${comboSearchQuery}
            @input=${onComboSearch}>
        </div>
        <button class="primary" @click=${() => showCreateCombo()}>
          ${icons.plus()} ${t("combos.grid.create")}
        </button>
      </div>
    </div>

    ${list.length === 0 ? html`<p class="empty">${t("combos.grid.empty")}</p>` : html`
      <div class="combos-list-container">
        <!-- Custom Combos Section (Always first & prominent) -->
        <section class="combos-section custom-combos-section">
          <div class="section-header">
            <h3>Custom Combos (${filteredCustom.length}${hasSearch ? ` of ${allCustom.length}` : ""})</h3>
          </div>
          ${renderResponsiveCardTable<Combo>({
            columns,
            rows: filteredCustom,
            rowKey: (c) => c.id,
            className: "combos-table custom-combos-table",
            emptyMessage: hasSearch
              ? `No custom combos matching "${comboSearchQuery}".`
              : "No custom combos created yet. Click 'Create combo' to add one.",
          })}
        </section>

        <!-- Auto Combos Section (Collapsible dropdown at the end) -->
        ${showAutoAccordion ? html`
          <section class="combos-section auto-combos-section" style="margin-top: var(--space-4);">
            <details
              class="auto-combos-accordion"
              ?open=${isAccordionOpen}
              @toggle=${(e: Event) => {
                isAutoExpanded = (e.currentTarget as HTMLDetailsElement).open;
                requestUpdate();
              }}>
              <summary class="auto-combos-summary">
                <div class="auto-combos-header-left">
                  <span class="auto-combos-toggle-icon">
                    ${isAccordionOpen ? icons.caretDown() : icons.chevronRight()}
                  </span>
                  <h3 style="margin: 0; font-size: var(--fs-sm); font-weight: 600;">
                    Auto-generated Combos (${filteredAuto.length}${hasSearch ? ` of ${allAuto.length}` : ""})
                  </h3>
                </div>
                <span class="muted auto-combos-hint">
                  ${isAccordionOpen ? "Click to collapse" : "Click to expand single-model virtual combos"}
                </span>
              </summary>
              <div class="auto-combos-content">
                ${renderResponsiveCardTable<Combo>({
                  columns,
                  rows: filteredAuto,
                  rowKey: (c) => c.id,
                  className: "combos-table auto-combos-table",
                  emptyMessage: hasSearch
                    ? `No auto combos matching "${comboSearchQuery}".`
                    : "No auto-generated combos available.",
                })}
              </div>
            </details>
          </section>
        ` : ""}
      </div>
    `}
  `;
}
