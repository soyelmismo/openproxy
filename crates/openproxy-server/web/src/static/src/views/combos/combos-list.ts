// views/combos/combos-list.ts — dense listing primitive for combos view.
//
// Powered by renderDataGrid: one line per row (32px), elastic name column with min,
// semantic max for content columns, space-driven folding, context menu, hover quick actions,
// bulk actions, inline rename (dblclick / F2), and toolbar filters.

import { html, type TemplateResult } from "lit-html";
import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { icons } from "../../lib/icons.js";
import { t } from "../../i18n/index.js";
import { PRIORITY_MODE_LABELS, PRIORITY_MODE_TOOLTIPS } from "../../lib/constants.js";
import { showCreateCombo, cloneCombo, deleteCombo } from "../../handlers/combo-handlers.js";
import { showToast } from "../../components/toast.js";
import { showConfirm } from "../../lib/show-confirm.js";
import type { Combo, PriorityMode } from "../../lib/types/api.js";
import { renderDataGrid, type GridColumn } from "../../components/data-grid.js";
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

async function onRenameCombo(id: number, name: string): Promise<void> {
  try {
    await api(`/combos/${id}`, { method: "PATCH", body: JSON.stringify({ name }) });
    const item = (state.combos || []).find((c) => c.id === id);
    if (item) item.name = name;
    showToast(t("combos.detail.renamed_toast", { name, defaultValue: `Combo renamed to "${name}"` }), "success");
    requestUpdate();
  } catch (err: unknown) {
    showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error");
  }
}

async function onBulkDeleteCombos(picked: Combo[]): Promise<void> {
  if (picked.length === 0) return;
  if (!(await showConfirm({
    title: t("keys.list.bulk.confirm_delete.title", { count: picked.length }),
    message: t("keys.list.bulk.confirm_delete.message", { list: picked.map((c) => c.name || "#" + c.id).join(", ") }),
    danger: true,
    confirmLabel: t("common.delete"),
  }))) return;

  const failed: string[] = [];
  for (const c of picked) {
    try {
      await api(`/combos/${c.id}`, { method: "DELETE" });
      state.combos = ((state.combos || []) as Combo[]).filter((x) => x.id !== c.id);
    } catch {
      failed.push(c.name || "#" + c.id);
    }
  }
  requestUpdate();
  if (failed.length > 0) {
    showToast(t("keys.list.bulk.partial_error", { list: failed.join(", ") }), "error");
  }
}

function getCombosColumns(): GridColumn<Combo>[] {
  return [
    {
      key: "name",
      label: t("common.filter") || "Name",
      min: 140,
      elastic: true,
      render: (c) => {
        const pm = priorityModeOf(c);
        return html`
          <a class="combo-table-name-link" href="#/combos/${c.id}">
            <strong class="combo-name-title">${c.name}</strong>
            <span class="combo-id-badge">#${c.id}</span>
          </a>
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
        `;
      },
    },
    {
      key: "strategy",
      label: "Strategy",
      max: 120,
      fold: 2,
      render: (c) => html`<span class="chip chip-strategy" title="Routing Strategy: ${c.strategy}">${c.strategy}</span>`,
    },
    {
      key: "priority_mode",
      label: "Priority Mode",
      max: 140,
      fold: 1,
      render: (c) => {
        const pm = priorityModeOf(c);
        return html`<span class="chip chip-pm" title=${PRIORITY_MODE_TOOLTIPS[pm] || ""}>${PRIORITY_MODE_LABELS[pm] || pm}</span>`;
      },
    },
    {
      key: "race_size",
      label: "Race",
      max: 80,
      fold: 3,
      render: (c) => html`<span class="combo-race-badge" title="Race size: ${c.race_size}">race ${c.race_size}</span>`,
    },
    {
      key: "context_window",
      label: "Context",
      max: 90,
      fold: 4,
      render: (c) => html`<span class="combo-context-value dg-mono">${c.context_window != null ? formatTokens(c.context_window) : "auto"}</span>`,
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

  const emptyMsg = isFiltered
    ? "No combos matching your filters."
    : (t("combos.grid.empty") || "No combos yet. Create one to start routing.");

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
            ${renderDataGrid<Combo>({
              id: "combos-grid",
              className: "combos-table custom-combos-table",
              columns,
              rows: filteredCustom,
              rowKey: (c) => c.id,
              emptyMessage: emptyMsg,
              renameable: (c) => c.name,
              onRename: (c, value) => { void onRenameCombo(c.id, value); },
              quickActions: () => [
                { action: "details", title: "Details", icon: icons.pencil() },
                { action: "clone", title: "Clone", icon: icons.copy() },
                { action: "delete", title: t("common.delete") || "Delete", icon: icons.trash(), danger: true },
              ],
              menuItems: () => [
                { action: "details", label: "Details" },
                { action: "rename", label: t("grid.menu.rename"), shortcut: "F2" },
                { action: "clone", label: "Clone" },
                { separator: true },
                { action: "copy_name", label: "Copy name" },
                { separator: true },
                { action: "delete", label: t("common.delete") || "Delete", shortcut: "Supr", danger: true },
              ],
              onAction: (c, action) => {
                switch (action) {
                  case "details":
                    location.hash = `#/combos/${c.id}`;
                    return;
                  case "rename":
                    return; // Handled inline by renderDataGrid
                  case "clone":
                    void cloneCombo(c.id, c.name);
                    return;
                  case "copy_name":
                    void navigator.clipboard?.writeText(c.name);
                    showToast(t("grid.toast.copied") || "Copied to clipboard");
                    return;
                  case "delete":
                    void deleteCombo(c.id);
                    return;
                }
              },
              bulkActions: [
                { key: "delete", label: t("common.delete") || "Delete", danger: true, run: (picked) => void onBulkDeleteCombos(picked) },
              ],
              bulkCountLabel: (n) => t("grid.bulk.selected", { n }),
              __refresh: () => requestUpdate(),
            })}
          </section>
        </div>
      `}
    </div>
  `;
}
