// components/toast.ts — short-lived non-blocking notification.
// The original app used an inline `showToast` function; we keep
// the same call signature so handlers can keep using it.
//
// Migrated to lit-html: the toast content is now built with a
// lit-html template and rendered into the freshly-created
// `<div class="toast">` via `render()`. The function still
// returns `void` (it imperatively appends a DOM node to
// `document.body`), so existing callers are unaffected.

import { html, render } from "lit-html";

export type ToastType = "info" | "success" | "error" | "warning" | string;

export function showToast(message: string, type: ToastType = "info"): void {
  const toast: HTMLDivElement = document.createElement("div");
  toast.className = `toast toast-${type}`;
  // lit-html auto-escapes `${message}` so the toast body is
  // safe even if the message contains user-supplied text.
  render(html`${message}`, toast);
  document.body.appendChild(toast);
  setTimeout(() => toast.classList.add("show"), 10);
  setTimeout(() => {
    toast.classList.remove("show");
    setTimeout(() => toast.remove(), 300);
  }, 3000);
}
