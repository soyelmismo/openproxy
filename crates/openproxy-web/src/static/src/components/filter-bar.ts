// components/filter-bar.ts — small search/filter bar with a
// result count. The caller owns the actual filtering logic; we
// only render the input + count slot.
//
// Migrated to lit-html: returns a `TemplateResult`. `placeholder`
// and `count` go through normal `${...}` interpolation (lit-html
// auto-escapes both attribute and text content). `props.extra`
// is a raw HTML string from the caller, so it is embedded via
// `unsafeHTML`. The legacy inline `oninput="window.__filterBar…"`
// handler is preserved as a literal attribute — the function is
// currently dead code (no caller wires `window.__filterBar`),
// and converting it to `@input` would require a handler that
// does not exist in the public API.

import { html, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";

export interface FilterBarProps {
  placeholder?: string;
  count?: number | null;
  /** Raw HTML string rendered into the bar (e.g. extra dropdowns). */
  extra?: string;
}

export function filterBar(props: FilterBarProps): TemplateResult {
  const placeholder: string = props.placeholder ?? "Filter...";
  return html`
    <div class="filter-bar">
      <input
        type="text"
        id="filter-bar-input"
        placeholder="${placeholder}"
        oninput="window.__filterBar && window.__filterBar(this.value)"
      />
      <span class="filter-info"
        >${props.count != null ? `${props.count} results` : null}</span
      >
      ${props.extra ? unsafeHTML(props.extra) : null}
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
export function analyticsFilters(props: AnalyticsFiltersProps): TemplateResult {
  return html`
    <div class="analytics-filters">
      <select class="filter-dropdown" id="analytics-provider-filter">
        <option value="">All providers</option>
        ${props.providers.map(
          (p) =>
            html`<option value="${p.value}" ?selected=${p.value === props.selectedProvider}>
              ${p.label}
            </option>`,
        )}
      </select>
      <select class="filter-dropdown" id="analytics-key-filter">
        <option value="">All API keys</option>
        ${props.apiKeys.map(
          (k) =>
            html`<option value="${k.value}" ?selected=${k.value === props.selectedKeyId}>
              ${k.label}
            </option>`,
        )}
      </select>
      <button class="btn-link" id="analytics-clear-filters">Clear filters</button>
    </div>
  `;
}

/** Wire up the analytics filter dropdowns. Call after the result is rendered. */
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
