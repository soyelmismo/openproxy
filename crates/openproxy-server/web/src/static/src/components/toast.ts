// components/toast.ts — short-lived non-blocking notification.
// Migrated to lit-html + container stacking: toasts stack vertically
// in #toast-container with individual dismiss controls and hover pause.

import { html, render } from "lit-html";

export type ToastType = "info" | "success" | "error" | "warning" | string;

export function ensureToastContainer(): HTMLElement {
  let container = document.getElementById("toast-container");
  if (!container) {
    container = document.createElement("div");
    container.id = "toast-container";
    container.className = "toast-container";
    document.body.appendChild(container);
  }
  return container;
}

export function showToast(message: string, type: ToastType = "info", durationMs = 3500): void {
  const container = ensureToastContainer();
  const toast: HTMLDivElement = document.createElement("div");
  toast.className = `toast toast-${type}`;

  const dismiss = () => {
    toast.classList.remove("show");
    setTimeout(() => {
      toast.remove();
      if (container.children.length === 0 && container.parentElement) {
        container.remove();
      }
    }, 250);
  };

  render(
    html`
      <div class="toast-content">${message}</div>
      <button type="button" class="toast-close" @click=${dismiss} aria-label="Close">&times;</button>
    `,
    toast
  );

  container.appendChild(toast);

  requestAnimationFrame(() => {
    toast.classList.add("show");
  });

  let timer = setTimeout(dismiss, durationMs);

  toast.addEventListener("mouseenter", () => clearTimeout(timer));
  toast.addEventListener("mouseleave", () => {
    timer = setTimeout(dismiss, 1500);
  });
}

