// lib/icons.ts — Centralized SVG icon helpers for Lit-HTML.
// Zero dependencies, crisp vector glyphs aligned with design tokens.

import { html, type TemplateResult } from "lit-html";

export const icons = {
  // Navigation & Shell
  sun: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="8" cy="8" r="3.5"/><path d="M8 1v2M8 13v2M1 8h2M13 8h2M3.05 3.05l1.41 1.41M11.54 11.54l1.41 1.41M3.05 12.95l1.41-1.41M11.54 4.46l1.41-1.41"/></svg>`,
  moon: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M13.5 9.5A6 6 0 1 1 6.5 2.5a4.5 4.5 0 0 0 7 7z"/></svg>`,
  menu: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true"><path d="M2.5 4h11M2.5 8h11M2.5 12h11"/></svg>`,
  close: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><path d="M3.5 3.5l9 9M12.5 3.5l-9 9"/></svg>`,
  logout: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 2H3.5a1.5 1.5 0 0 0-1.5 1.5v9A1.5 1.5 0 0 0 3.5 14H6M10.5 11.5L14 8l-3.5-3.5M14 8H5.5"/></svg>`,
  search: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true"><circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5L14 14"/></svg>`,

  // Controls & Carets
  caretDown: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="12" height="12" fill="currentColor" aria-hidden="true"><path d="M4 6.5l4 4 4-4z"/></svg>`,
  caretUp: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="12" height="12" fill="currentColor" aria-hidden="true"><path d="M4 9.5l4-4 4 4z"/></svg>`,
  chevronLeft: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 12L6 8l4-4"/></svg>`,
  chevronRight: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 4l4 4-4 4"/></svg>`,
  chevronsLeft: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8.5 12L4.5 8l4-4M12.5 12L8.5 8l4-4"/></svg>`,
  chevronsRight: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3.5 4l4 4-4 4M7.5 4l4 4-4 4"/></svg>`,
  arrowLeft: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M13 8H3M7 4L3 8l4 4"/></svg>`,
  arrowUp: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="11" height="11" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 13.5V2.5M3.5 7L8 2.5 12.5 7"/></svg>`,
  arrowDown: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="11" height="11" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 2.5v11M3.5 9L8 13.5 12.5 9"/></svg>`,
  dragHandle: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="currentColor" aria-hidden="true"><circle cx="5" cy="3.5" r="1.2"/><circle cx="11" cy="3.5" r="1.2"/><circle cx="5" cy="8" r="1.2"/><circle cx="11" cy="8" r="1.2"/><circle cx="5" cy="12.5" r="1.2"/><circle cx="11" cy="12.5" r="1.2"/></svg>`,

  // Actions
  play: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="currentColor" aria-hidden="true"><path d="M4 2.5l9 5.5-9 5.5z"/></svg>`,
  pause: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="currentColor" aria-hidden="true"><rect x="3.5" y="2.5" width="3" height="11" rx="0.5"/><rect x="9.5" y="2.5" width="3" height="11" rx="0.5"/></svg>`,
  record: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="12" height="12" fill="currentColor" aria-hidden="true"><circle cx="8" cy="8" r="5"/></svg>`,
  export: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.5 10.5v3a1 1 0 0 0 1 1h9a1 1 0 0 0 1-1v-3M8 1.5v8.5M4.5 7L8 10.5 11.5 7"/></svg>`,
  plus: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M8 3v10M3 8h10"/></svg>`,
  pencil: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M11.5 2.5l2 2L5 13H3v-2l8.5-8.5z"/></svg>`,
  copy: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1"/><path d="M3.5 10.5H2.5a1 1 0 0 1-1-1v-7a1 1 0 0 1 1-1h7a1 1 0 0 1 1 1v1"/></svg>`,
  trash: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.5 4.5h11M6 2h4M4 4.5v9a1 1 0 0 0 1 1h6a1 1 0 0 0 1-1v-9M6.5 7v4.5M9.5 7v4.5"/></svg>`,
  refresh: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M13.5 2.5v4h-4M2.5 13.5v-4h4"/><path d="M3.5 6a5.5 5.5 0 0 1 9-1.5L13.5 6.5M12.5 10a5.5 5.5 0 0 1-9 1.5L2.5 9.5"/></svg>`,
  send: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="currentColor" aria-hidden="true"><path d="M2 2.5l12 5.5-12 5.5 1.5-5.5L8 8 3.5 7.5z"/></svg>`,
  flask: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 2h4M7 2v4L3.5 12.5a1.5 1.5 0 0 0 1.3 2.2h6.4a1.5 1.5 0 0 0 1.3-2.2L9 6V2M5 10h6"/></svg>`,
  key: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="5" cy="5" r="2.5"/><path d="M6.8 6.8 L13 13 M11 11 L13 9 M9 13 L11 11"/></svg>`,
  desktop: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="2" y="2.5" width="12" height="8.5" rx="1"/><path d="M5.5 14h5M8 11v3"/></svg>`,
  lightning: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="currentColor" aria-hidden="true"><path d="M9 1.5L2.5 9h5L6.5 14.5 13.5 7h-5z"/></svg>`,
  target: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="8" cy="8" r="6.5"/><circle cx="8" cy="8" r="3.5"/><circle cx="8" cy="8" r="1" fill="currentColor"/></svg>`,
  warning: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 2L1.5 13.5h13L8 2zM8 6.5v3M8 11.5v.5"/></svg>`,
  check: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 8.5l3.5 3.5L13 5.5"/></svg>`,
  tag: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 2h4.5L14 9.5 8.5 15 1 7.5V2z"/><circle cx="4.5" cy="4.5" r="1" fill="currentColor"/></svg>`,

  // Endpoints / Kinds
  chat: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.5 3h11a1 1 0 0 1 1 1v6.5a1 1 0 0 1-1 1H5.5L2 14.5V4a1 1 0 0 1 .5-1z"/></svg>`,
  image: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="2" y="2.5" width="12" height="11" rx="1.5"/><circle cx="5.5" cy="6" r="1.5"/><path d="M2 11.5l3.5-3.5 3.5 3.5 2-2 3 3"/></svg>`,
  audio: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="5.5" y="2" width="5" height="7.5" rx="2.5"/><path d="M3 7v1a5 5 0 0 0 10 0V7M8 13v2M5.5 15h5"/></svg>`,
  embedding: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2 14h12M2 14L14 2M6 14v-4M10 14V6M14 14V2"/></svg>`,
  video: (cls = "icon"): TemplateResult => html`<svg class="${cls}" viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="2" y="3" width="9" height="10" rx="1"/><path d="M11 6.5l3.5-2.5v8l-3.5-2.5"/></svg>`,
};

export function endpointIcon(kind: string, cls = "icon"): TemplateResult {
  switch ((kind || "").toLowerCase()) {
    case "audio":
      return icons.audio(cls);
    case "image":
      return icons.image(cls);
    case "embedding":
      return icons.embedding(cls);
    case "video":
      return icons.video(cls);
    case "chat":
    default:
      return icons.chat(cls);
  }
}
