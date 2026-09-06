// lib/show-confirm.ts — Promise-based confirm() / prompt() replacements.
//
// Renders a lit-html modal (same .modal-bg / .modal structure the rest
// of the dashboard uses) into a temporary wrapper under <body> and
// resolves a Promise when the user picks an action or dismisses.
// Escape / backdrop click / X resolve as "cancelled".
//
// Replaces native `confirm()` / `prompt()` per refactor spec Q5:
// native dialogs can't be styled, block the main thread, and leak
// un-translated browser chrome into the UI.

import { html, render, type TemplateResult } from "lit-html";
import { showToast } from "../components/toast.js";

interface ConfirmOptions {
  title: string;
  message: string;
  /** Renders the primary button in the danger (red) style. */
  danger?: boolean;
  /** Label for the confirmation button. Default: "Confirm". */
  confirmLabel?: string;
  /** Label for the dismiss button. Default: "Cancel". */
  cancelLabel?: string;
}

/** Dialog element ids so e2e tests can target the active dialog. */
const DIALOG_ID = "show-confirm-dialog";

/**
 * Show a modal confirmation. Resolves `true` when the user confirms,
 * `false` on dismiss (Cancel button, Escape, backdrop click, X).
 */
export function showConfirm(options: ConfirmOptions): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    const { title, message, danger = false, confirmLabel = "Confirm", cancelLabel = "Cancel" } = options;

    const wrapper = document.createElement("div");
    document.body.appendChild(wrapper);

    const finish = (result: boolean): void => {
      window.removeEventListener("keydown", onKeydown, true);
      wrapper.remove();
      resolve(result);
    };

    const onKeydown = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.stopPropagation();
        finish(false);
      } else if (e.key === "Enter") {
        e.stopPropagation();
        finish(true);
      }
    };

    const template: TemplateResult = html`
      <div
        class="modal-bg"
        id=${DIALOG_ID}
        role="dialog"
        aria-modal="true"
        aria-labelledby=${DIALOG_ID + "-title"}
        @click=${(e: Event) => {
          // Only close on backdrop clicks (the .modal-bg itself).
          if (e.target === wrapper.firstElementChild) finish(false);
        }}
      >
        <div class="modal" @click=${(e: Event) => e.stopPropagation()}>
          <div class="modal-header">
            <h2 id=${DIALOG_ID + "-title"}>${title}</h2>
            <button type="button" class="close-btn" @click=${() => finish(false)} aria-label="Close">&times;</button>
          </div>
          <div class="modal-body">${message}</div>
          <div class="modal-footer">
            <button type="button" @click=${() => finish(false)}>${cancelLabel}</button>
            <button
              type="button"
              class=${danger ? "danger" : "primary"}
              @click=${() => finish(true)}
            >${confirmLabel}</button>
          </div>
        </div>
      </div>
    `;

    render(template, wrapper);
    window.addEventListener("keydown", onKeydown, true);
  });
}

/**
 * Show a modal text prompt. Resolves the trimmed input value when the
 * user confirms, `null` on dismiss (Cancel button, Escape, backdrop
 * click, X). Rejects never.
 */
export function showPrompt(title: string, message: string, initial = ""): Promise<string | null> {
  return new Promise<string | null>((resolve) => {
    const wrapper = document.createElement("div");
    document.body.appendChild(wrapper);

    let draft: string = initial;
    let finished = false;
    const finish = (result: string | null): void => {
      if (finished) return;
      finished = true;
      window.removeEventListener("keydown", onKeydown, true);
      wrapper.remove();
      resolve(result);
    };

    const onKeydown = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.stopPropagation();
        finish(null);
      }
    };

    const template: TemplateResult = html`
      <div
        class="modal-bg"
        role="dialog"
        aria-modal="true"
        aria-labelledby="show-prompt-dialog-title"
        @click=${(e: Event) => {
          if (e.target === wrapper.firstElementChild) finish(null);
        }}
      >
        <div class="modal" @click=${(e: Event) => e.stopPropagation()}>
          <div class="modal-header">
            <h2 id="show-prompt-dialog-title">${title}</h2>
            <button type="button" class="close-btn" @click=${() => finish(null)} aria-label="Close">&times;</button>
          </div>
          <div class="modal-body">
            <div>${message}</div>
            <input
              type="text"
              id="show-prompt-input"
              class="modal-input"
              .value=${initial}
              @input=${(e: Event) => { draft = (e.target as HTMLInputElement).value; }}
              @keydown=${(e: KeyboardEvent) => {
                if (e.key === "Enter") {
                  e.stopPropagation();
                  finish(draft.trim());
                }
              }}
            />
          </div>
          <div class="modal-footer">
            <button type="button" @click=${() => finish(null)}>Cancel</button>
            <button type="button" class="primary" @click=${() => finish(draft.trim())}>OK</button>
          </div>
        </div>
      </div>
    `;

    render(template, wrapper);
    window.addEventListener("keydown", onKeydown, true);
    const input = wrapper.querySelector<HTMLInputElement>("#show-prompt-input");
    if (input) {
      input.focus();
      input.select();
    }
  });
}

/** Convenience: map an unknown error to a message and show an error toast. */
export function showCopyError(err: unknown, context: string): void {
  const msg = err instanceof Error ? err.message : String(err);
  showToast(context + ": " + msg, "error");
}
