// Provider-detail models table. Split out of the old monolithic
// `renderProviderDetail()` so `updateProviderFilter()` can re-paint just the
// rows; the search input lives outside the tbody, so its focus survives.
//
// The handlers module imports `renderModelRows` from here, and this file
// imports the handlers back: a module cycle, safe because the handlers are only
// referenced inside `@click` / `@change` closures, never at module top level.
// `syncModelRowActive` and the other DOM-patch helpers keep mutating the table
// in place instead of re-rendering.
//
// Every export is a pure function of `(state, props)`. They touch the DOM via
// `render()` on the tbody, never the `state` singleton.

import { html, type TemplateResult } from "lit-html";
import { state } from "../state/index.js";
import { statusPillClass } from "../lib/constants.js";
import { icons } from "../lib/icons.js";
import { formatContextBadge } from "../lib/format.js";
import {
  toggleModelSelection,
  testModel,
  toggleModel,
  deleteModel,
  cycleProviderSort,
} from "../handlers/model-handlers/index.js";
import type { Model } from "../lib/types/api.js";

// HTTP status to a status-pill CSS class. The server stamps `0` when the
// request never reached the upstream (DNS, connect, TLS, timeout), which reads
// as the red "off" pill.
//
// Returns a CSS class name, not a TemplateResult: it feeds `class=${...}`.
export function modelStatusPillClass(status: number | null): string {
  if (status == null) return "off";
  if (status === 0) return "off";
  if (status >= 200 && status < 300) return "on";
  if (status >= 400 && status < 500) return "warn";
  if (status >= 500) return "off";
  return "";
}

// Capability badges. Accepts the wire JSON string or a pre-parsed object.
// Bad input renders as an em-dash rather than throwing.
export function renderCapabilityBadges(json: string | null | undefined, modelType?: string | null): TemplateResult {
  const badges: TemplateResult[] = [];
  if (modelType && modelType !== "chat") {
    badges.push(html`<span class="cap-badge">${modelType}</span>`);
  }
  if (json != null) {
    let caps: unknown;
    if (typeof json === "string") {
      try { caps = JSON.parse(json) as unknown; } catch (_e: unknown) { caps = null; }
    } else {
      caps = json;
    }
    if (caps && typeof caps === "object") {
      const c: Record<string, unknown> = caps as Record<string, unknown>;
      if (c["streaming"] === false) badges.push(html`<span class="cap-badge warn" title="Forced unary (non-streaming)">unary</span>`);
      if (c["streaming"] === true) badges.push(html`<span class="cap-badge" title="Forced streaming (SSE)">stream</span>`);
      if (c["vision"]) badges.push(html`<span class="cap-badge">vision</span>`);
      if (c["tool_calling"]) badges.push(html`<span class="cap-badge">tools</span>`);
      if (c["reasoning"]) badges.push(html`<span class="cap-badge">reasoning</span>`);
      if (c["thinking"]) badges.push(html`<span class="cap-badge">thinking</span>`);
      if (c["structured_output"]) badges.push(html`<span class="cap-badge">json</span>`);
      if (c["attachment"]) badges.push(html`<span class="cap-badge">attach</span>`);
      if (c["decisions"]) badges.push(html`<span class="cap-badge">decisions</span>`);
    }
  }
  return badges.length > 0 ? html`${badges}` : html`<span class="muted">—</span>`;
}

// One <tr> for an already-filtered model. The row id is the numeric
// `row_id` primary key, which is what /admin/models/:id/... endpoints key off.
export function renderModelRow(m: Model): TemplateResult {
  const lastTest: TemplateResult = m.last_test_status != null
    ? html`<span class=${"status-pill " + statusPillClass(m.last_test_status)}>${String(m.last_test_status)}</span> <small>${m.last_test_at || ""}</small>`
    : html`<span class="muted">never</span>`;
  const isSelected: boolean = (state.selectedModels as Set<number>).has(m.row_id);
  return html`
    <tr id=${`model-row-${m.row_id}`} class=${(m.active ? "" : "inactive") + (isSelected ? " selected" : "")}>
      <td><input type="checkbox" ?checked=${isSelected} @change=${(e: Event) => toggleModelSelection(m.row_id, e)}></td>
      <td><code>${m.model_id}</code>${m.custom ? html`<span class="badge custom">custom</span>` : html``}</td>
      <td>${m.display_name || "—"}</td>
      <td>${m.target_format || "—"}</td>
      <td>${formatContextBadge(m.context_length)}</td>
      <td>${formatContextBadge(m.max_output_tokens)}</td>
      <td>${renderCapabilityBadges(m.capabilities_json, m.model_type)}${m.family ? html` <small class="muted">${m.family}</small>` : html``}</td>
      <td><span class=${"status-pill " + (m.active ? "on" : "off")}>${m.active ? "active" : "inactive"}</span></td>
      <td class="last-test-cell">${lastTest}</td>
      <td>
        <button class="small" id=${`test-btn-${m.row_id}`} @click=${(e: Event) => testModel(m.row_id, m.model_id, e)}>${icons.flask()} Test</button>
        <button class="small model-toggle-btn" @click=${() => toggleModel(m.row_id, !m.active, null)}>${m.active ? "Disable" : "Enable"}</button>
        <button class="small danger" @click=${() => deleteModel(m.row_id)} title="Delete model">${icons.close()}</button>
      </td>
    </tr>
  `;
}

// Rows for a pre-filtered list (search plus active/inactive already applied),
// as a single TemplateResult for `render()` into the tbody.
export function renderModelRows(rows: readonly Model[]): TemplateResult {
  return html`${rows.map((m) => renderModelRow(m))}`;
}

// Row ids passing the current search+filter, so the master "select all"
// checkbox only catches rows the user can see.
export function getVisibleModelRowIds(): number[] {
  if (!state.currentView || state.currentView.context == null) return [];
  const providerId: string = state.currentView.context;
  const ui: Record<string, unknown> | undefined = (state.providerDetail as Record<string, Record<string, unknown>>)[providerId];
  if (!ui) return [];
  const search: string = (typeof ui["search"] === "string" ? (ui["search"] as string) : "").toLowerCase();
  const filter: string = typeof ui["filter"] === "string" ? (ui["filter"] as string) : "";
  return state.models
    .filter((m) => m.provider_id === providerId)
    .filter((m) => {
      if (filter === "active" && !m.active) return false;
      if (filter === "inactive" && m.active) return false;
      if (search && !m.model_id.toLowerCase().includes(search)) return false;
      return true;
    })
    .map((m) => m.row_id);
}

// Rewrite the filter-tab counts to the provider's totals rather than the
// current filter's. Cheaper than a full re-render.
export function updateFilterTabCounts(providerId: string, allProviderModels: readonly Model[]): void {
  const active: number = allProviderModels.filter((m) => m.active).length;
  const inactive: number = allProviderModels.length - active;
  const allBtn: HTMLElement | null = document.getElementById(`filter-tab-${providerId}-all`);
  const activeBtn: HTMLElement | null = document.getElementById(`filter-tab-${providerId}-active`);
  const inactiveBtn: HTMLElement | null = document.getElementById(`filter-tab-${providerId}-inactive`);
  if (allBtn) allBtn.textContent = `All (${allProviderModels.length})`;
  if (activeBtn) activeBtn.textContent = `Active (${active})`;
  if (inactiveBtn) inactiveBtn.textContent = `Inactive (${inactive})`;
}

// Patch one row's active state in place (class, status pill, button label)
// instead of re-rendering, so an open <select> or <input> elsewhere keeps
// focus. Mirrors patchComboField in combo-handlers.ts.
//
// A direct DOM mutation, not a lit-html re-render: the patch is small and
// local, and the next `requestUpdate()` reconciles any drift.
export function syncModelRowActive(rowId: number, active: boolean): void {
  const row = document.getElementById(`model-row-${rowId}`);
  if (!row) return;
  row.classList.toggle("inactive", !active);
  // The active pill sits in a plain <td>; the last-test pill in
  // <td class="last-test-cell">.
  const pill = row.querySelector("td:not(.last-test-cell) > .status-pill");
  if (pill) {
    pill.className = `status-pill ${active ? "on" : "off"}`;
    pill.textContent = active ? "active" : "inactive";
  }
  // Located by the `model-toggle-btn` class. The @click closure captures
  // `!m.active` at click time and `toggleModel` mutates `m.active` in place,
  // so the next click already sees the new state.
  const toggleBtn = row.querySelector<HTMLButtonElement>(".model-toggle-btn");
  if (toggleBtn) {
    toggleBtn.textContent = active ? "Disable" : "Enable";
  }
}

// Master "select all": checked when every visible row is selected,
// indeterminate on a partial selection. Shared by the initial render and the
// partial re-paint in updateProviderFilter.
export function syncSelectAllCheckbox(visibleRowIds: readonly number[]): void {
  const master: HTMLInputElement | null = document.getElementById("model-select-all") as HTMLInputElement | null;
  if (!master) return;
  if (visibleRowIds.length === 0) {
    master.checked = false;
    master.indeterminate = false;
    return;
  }
  const selectedVisible: number = visibleRowIds.filter((id) => (state.selectedModels as Set<number>).has(id)).length;
  if (selectedVisible === 0) {
    master.checked = false;
    master.indeterminate = false;
  } else if (selectedVisible === visibleRowIds.length) {
    master.checked = true;
    master.indeterminate = false;
  } else {
    master.checked = false;
    master.indeterminate = true;
  }
}

// Column sorting. A header click cycles none → asc → desc, and "none" restores
// the upstream order, which is itself meaningful (e.g. OpenRouter's family
// groupings). The choice persists per provider in
// `state.providerDetail[id].sort`. The indicator is a Unicode arrow inline in
// the <th>; sortable headers carry the `sortable` class.

export interface SortableColumn {
  key: string;
  label: string;
  value: (m: Model) => string | number;
}

export const SORTABLE_COLUMNS: readonly SortableColumn[] = [
  // A null extractor means "stable": keep the upstream order.
  { key: "usage_model", label: "Model",      value: (m) => (`${m.provider_id}/${m.model_id}`).toLowerCase() },
  { key: "format",     label: "Format",     value: (m) => (m.target_format || "").toLowerCase() },
  { key: "context",    label: "Context",    value: (m) => m.context_length || 0 },
  { key: "out",        label: "Out",        value: (m) => m.max_output_tokens || 0 },
];

export interface ModelSort {
  key: string;
  dir: "asc" | "desc" | string;
}

// Sorted copy of the row list. Missing sort state returns the input unchanged.
export function applySort(rows: readonly Model[], sort: ModelSort | null): readonly Model[] {
  if (!sort || !sort.key) return rows;
  const col: SortableColumn | undefined = SORTABLE_COLUMNS.find((c) => c.key === sort.key);
  if (!col) return rows;
  const dir: number = sort.dir === "desc" ? -1 : 1;
  // Array.prototype.sort is stable in modern V8, so equal rows keep order.
  const out: Model[] = rows.slice();
  out.sort((a, b) => {
    const va: string | number = col.value(a);
    const vb: string | number = col.value(b);
    if (va < vb) return -1 * dir;
    if (va > vb) return 1 * dir;
    return 0;
  });
  return out;
}

// <th> with the sort indicator. Clicking cycles the sort via `cycleProviderSort`.
export function renderSortableTh(col: SortableColumn, sort: ModelSort | null, providerId: string): TemplateResult {
  const isActive: boolean = !!(sort && sort.key === col.key);
  const indicator: TemplateResult | string = isActive ? (sort && sort.dir === "desc" ? icons.caretDown() : icons.caretUp()) : "";
  return html`<th class=${"sortable" + (isActive ? " sorted" : "")} @click=${() => cycleProviderSort(providerId, col.key, null)}>${col.label}<span class="sort-indicator">${indicator}</span></th>`;
}
