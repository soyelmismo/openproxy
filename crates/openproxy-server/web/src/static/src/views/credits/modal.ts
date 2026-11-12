// views/credits/modal.ts — lightweight single modal for the credits editors.
//
// The editors (tier, package) re-render their body locally on every
// interaction: no view refetch, no focus loss beyond what the handlers
// restore. One modal at a time; backdrop click and Escape close it.

import { html, render as litRender, type TemplateResult } from "lit-html";

export interface ModalHandle {
  /** Re-render the card body (title + content) in place. */
  repaint: (body: TemplateResult) => void;
  /** Close and remove the overlay. */
  close: () => void;
}

let active: ModalHandle | null = null;

export function activeModal(): ModalHandle | null {
  return active;
}

export function openModal(title: string, body: TemplateResult): ModalHandle {
  active?.close();
  const root = document.createElement("div");
  root.style.cssText =
    "position:fixed;inset:0;z-index:900;background:rgba(0, 0, 0, 0.75);" +
    "display:flex;align-items:flex-start;justify-content:center;overflow:auto;padding:24px 12px";
  const card = document.createElement("div");
  card.style.cssText =
    "background:var(--color-surface);color:var(--color-text);" +
    "border:1px solid var(--color-border);" +
    "border-radius:12px;padding:16px;width:100%;max-width:760px;" +
    "max-height:calc(100vh - 48px);overflow:auto";
  root.appendChild(card);
  document.body.appendChild(root);

  const handle: ModalHandle = {
    repaint(inner: TemplateResult): void {
      litRender(
        html`
          <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:12px">
            <div style="font-weight:700;font-size:15px">${title}</div>
            <button class="secondary" style="padding:2px 10px" @click=${() => handle.close()}>×</button>
          </div>
          ${inner}
        `,
        card,
      );
    },
    close(): void {
      root.remove();
      if (active === handle) active = null;
      document.removeEventListener("keydown", onKey);
    },
  };
  const onKey = (e: KeyboardEvent): void => {
    if (e.key === "Escape") handle.close();
  };
  document.addEventListener("keydown", onKey);
  root.addEventListener("mousedown", (e) => {
    if (e.target === root) handle.close();
  });
  active = handle;
  handle.repaint(body);
  return handle;
}

/** Refocus an input inside the repainted body (keeps caret at the end). */
export function refocusInput(id: string): void {
  requestAnimationFrame(() => {
    const el = document.getElementById(id) as HTMLInputElement | null;
    if (!el) return;
    el.focus();
    const end = el.value.length;
    el.setSelectionRange(end, end);
  });
}
