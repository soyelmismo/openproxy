// components/filter-bar.ts — small search/filter bar with a
// result count. The caller owns the actual filtering logic; we
// only render the input + count slot.

import { escapeHtml } from "../lib/escape.js";

export interface FilterBarProps {
  placeholder?: string;
  count?: number | null;
  extra?: string;
}

export function filterBar(props: FilterBarProps): string {
  const placeholder: string = props.placeholder ?? "Filter...";
  return `
    <div class="filter-bar">
      <input type="text" id="filter-bar-input" placeholder="${escapeHtml(placeholder)}" oninput="window.__filterBar && window.__filterBar(this.value)">
      <span class="filter-info">${props.count != null ? escapeHtml(String(props.count)) + " results" : ""}</span>
      ${props.extra || ""}
    </div>
  `;
}

// ── Analytics filter bar with dropdowns ─────────────────────────────

export interface DropdownOption {
  value: string;
  label: string;
}

export interface AnalyticsFiltersProps {
  providers: DropdownOption[];
  apiKeys: DropdownOption[];
  selectedProvider?: string;
  selectedKeyId?: string;
  onProviderChange?: (id: string) => void;
  onKeyChange?: (id: string) => void;
  onClear?: () => void;
}

/** Render the analytics filter bar with provider + API key dropdowns. */
export function analyticsFilters(props: AnalyticsFiltersProps): string {
  const providerOpts = [
    `<option value="">All providers</option>`,
    ...props.providers.map(p =>
      `<option value="${escapeHtml(p.value)}" ${p.value === props.selectedProvider ? "selected" : ""}>${escapeHtml(p.label)}</option>`
    ),
  ].join("");

  const keyOpts = [
    `<option value="">All API keys</option>`,
    ...props.apiKeys.map(k =>
      `<option value="${escapeHtml(k.value)}" ${k.value === props.selectedKeyId ? "selected" : ""}>${escapeHtml(k.label)}</option>`
    ),
  ].join("");

  return `
    <div class="analytics-filters">
      <select class="filter-dropdown" id="analytics-provider-filter">
        ${providerOpts}
      </select>
      <select class="filter-dropdown" id="analytics-key-filter">
        ${keyOpts}
      </select>
      <button class="btn-link" id="analytics-clear-filters">Clear filters</button>
    </div>
  `;
}

/** Wire up the analytics filter dropdowns. Call after innerHTML. */
export function wireAnalyticsFilters(
  container: HTMLElement,
  onProviderChange: (id: string) => void,
  onKeyChange: (id: string) => void,
  onClear: () => void,
): void {
  const provSel = container.querySelector<HTMLSelectElement>("#analytics-provider-filter");
  const keySel = container.querySelector<HTMLSelectElement>("#analytics-key-filter");
  const clearBtn = container.querySelector<HTMLButtonElement>("#analytics-clear-filters");

  provSel?.addEventListener("change", () => onProviderChange(provSel.value));
  keySel?.addEventListener("change", () => onKeyChange(keySel.value));
  clearBtn?.addEventListener("click", () => onClear());
}
