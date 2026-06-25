// components/quota-bar.ts — horizontal progress bar used in the
// account-detail quota view.
//
// Migrated to lit-html: returns a `TemplateResult`. The label is
// interpolated into both the `title` attribute and the visible
// `.quota-bar-label` text — lit-html auto-escapes both. The
// `style="width:..."` attribute uses a computed string but is
// built from a sanitised numeric percentage, so it is safe to
// embed directly.

import { html, type TemplateResult } from "lit-html";

export interface QuotaBarProps {
  label?: string;
  used: number;
  total: number;
  warn?: number;
  error?: number;
}

export function quotaBar(props: QuotaBarProps): TemplateResult {
  const { label, used, total, warn = 0.8, error = 0.95 } = props;
  if (!total || total <= 0) return html``;
  const pct: number = Math.max(0, Math.min(1, used / total));
  const cls: string = pct >= error ? "error" : pct >= warn ? "warn" : "";
  return html`
    <div class="quota-bar" title="${label || ""}">
      <div class="quota-bar-track">
        <div
          class="quota-bar-fill ${cls}"
          style="width:${(pct * 100).toFixed(1)}%"
        ></div>
      </div>
      <div class="quota-bar-label">${label || ""}</div>
    </div>
  `;
}
