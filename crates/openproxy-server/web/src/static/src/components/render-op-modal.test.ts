// components/render-op-modal.test.ts — unit tests for the
// renderOpModal() helper. Validates the DOM shape, ARIA wiring,
// escape/backdrop/close-button dismissal, focus trap, and the
// onClose callback without booting a browser (jsdom suffices).
//
// We mount each modal into the real document.body so focus events
// (`document.activeElement`) work, and tear it down in afterEach
// so tests don't leak DOM between cases.

import { describe, it, expect, afterEach, vi } from "vitest";
import { html } from "lit-html";
import { renderOpModal } from "./render-op-modal.js";

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

describe("renderOpModal", () => {
  it("mounts a .modal-bg > .modal > [header, body] tree into #modal-root", () => {
    const handle = renderOpModal({
      title: "Hello",
      body: html`<p>Body text</p>`,
    });
    expect(handle.el.parentElement?.id).toBe("modal-root");
    const modalBg = handle.el.querySelector(".modal-bg");
    expect(modalBg).not.toBeNull();
    const modal = modalBg?.querySelector(".modal");
    expect(modal).not.toBeNull();
    expect(modal?.querySelector(".modal-header h2")?.textContent).toBe("Hello");
    expect(modal?.querySelector(".modal-body")?.textContent?.trim()).toBe("Body text");
  });

  it("exposes role=dialog, aria-modal=true, and aria-labelledby pointing to the title", () => {
    const handle = renderOpModal({
      title: "My dialog",
      body: html`<p>body</p>`,
    });
    const dialog = handle.el.querySelector('[role="dialog"]') as HTMLElement | null;
    expect(dialog).not.toBeNull();
    expect(dialog?.getAttribute("aria-modal")).toBe("true");
    const labelledBy = dialog?.getAttribute("aria-labelledby");
    expect(labelledBy).toBeTruthy();
    const titleEl = labelledBy ? document.getElementById(labelledBy) : null;
    expect(titleEl?.textContent).toBe("My dialog");
  });

  it("renders the actions slot in .modal-footer when provided", () => {
    const handle = renderOpModal({
      title: "Confirm",
      body: html`<p>Are you sure?</p>`,
      actions: html`<button type="button">Yes</button><button type="button">No</button>`,
    });
    const footer = handle.el.querySelector(".modal-footer");
    expect(footer).not.toBeNull();
    expect(footer?.textContent).toContain("Yes");
    expect(footer?.textContent).toContain("No");
  });

  it("omits the footer when no actions slot is provided", () => {
    const handle = renderOpModal({
      title: "Info",
      body: html`<p>No actions here.</p>`,
    });
    expect(handle.el.querySelector(".modal-footer")).toBeNull();
  });

  it("tags the footer with the .danger class when danger=true", () => {
    const handle = renderOpModal({
      title: "Delete",
      body: html`<p>careful</p>`,
      actions: html`<button>Delete</button>`,
      danger: true,
    });
    const footer = handle.el.querySelector(".modal-footer");
    expect(footer?.classList.contains("danger")).toBe(true);
  });

  it("close() removes the wrapper from the DOM and is idempotent", () => {
    const handle = renderOpModal({
      title: "x",
      body: html`<p>x</p>`,
    });
    const root = handle.el.parentElement;
    expect(root?.contains(handle.el)).toBe(true);
    handle.close();
    expect(handle.el.isConnected).toBe(false);
    // Second call must not throw.
    expect(() => handle.close()).not.toThrow();
  });

  it("invokes onClose exactly once even if close() is called twice", () => {
    const onClose = vi.fn();
    const handle = renderOpModal({
      title: "x",
      body: html`<p>x</p>`,
      onClose,
    });
    handle.close();
    handle.close();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("Escape key on window closes the modal", () => {
    const handle = renderOpModal({
      title: "x",
      body: html`<p>x</p>`,
    });
    const event = new KeyboardEvent("keydown", { key: "Escape", bubbles: true });
    window.dispatchEvent(event);
    expect(handle.el.isConnected).toBe(false);
  });

  it("clicking the X button closes the modal", () => {
    const handle = renderOpModal({
      title: "x",
      body: html`<p>x</p>`,
    });
    const x = handle.el.querySelector<HTMLButtonElement>(".close-btn");
    expect(x).not.toBeNull();
    x?.click();
    expect(handle.el.isConnected).toBe(false);
  });

  it("clicking the .modal-bg backdrop closes the modal", () => {
    const handle = renderOpModal({
      title: "x",
      body: html`<p>x</p>`,
    });
    const modalBg = handle.el.querySelector<HTMLElement>(".modal-bg");
    modalBg?.click();
    expect(handle.el.isConnected).toBe(false);
  });

  it("clicking inside .modal does NOT close the modal (stopPropagation)", () => {
    const handle = renderOpModal({
      title: "x",
      body: html`<p>x</p>`,
    });
    const modal = handle.el.querySelector<HTMLElement>(".modal");
    modal?.click();
    expect(handle.el.isConnected).toBe(true);
  });

  it("focus trap: Tab on the last focusable wraps to the first", () => {
    const handle = renderOpModal({
      title: "trap",
      body: html`
        <input id="first" type="text" />
        <input id="last" type="text" />
      `,
    });
    // The first auto-focused element is the modal's close button
    // (it's the first focusable in DOM order, before any body
    // inputs). Move focus explicitly to our known "last" input
    // and dispatch Tab — the trap should wrap to the *first*
    // focusable in the focusable list, which is the close button.
    const firstFocusable = handle.el.querySelector<HTMLElement>(".close-btn");
    const last = handle.el.querySelector<HTMLInputElement>("#last");
    expect(firstFocusable && last).toBeTruthy();
    last?.focus();
    expect(document.activeElement).toBe(last);
    const ev = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
    window.dispatchEvent(ev);
    expect(document.activeElement).toBe(firstFocusable);
    // close to avoid leaking DOM
    handle.close();
  });
});
