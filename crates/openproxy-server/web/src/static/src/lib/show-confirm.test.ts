// lib/show-confirm.test.ts — unit tests for showConfirm(), showPrompt(),
// and showCopyError().
//
// These render lit-html modals into a real DOM wrapper under document.body,
// so jsdom suffices. We interact with the rendered buttons and keydown
// events to drive the Promise resolution.

import { describe, it, expect, afterEach, vi } from "vitest";
import { showConfirm, showPrompt, showCopyError } from "./show-confirm.js";

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

describe("showConfirm", () => {
  it("renders a modal with the correct title and message", () => {
    showConfirm({ title: "Delete item?", message: "This cannot be undone." });

    const dialog = document.querySelector('[role="dialog"]');
    expect(dialog).not.toBeNull();
    const h2 = dialog?.querySelector("h2");
    expect(h2?.textContent).toBe("Delete item?");
    const body = dialog?.querySelector(".modal-body");
    expect(body?.textContent).toBe("This cannot be undone.");
  });

  it("resolves true when the user clicks the Confirm button", async () => {
    const promise = showConfirm({ title: "Proceed?", message: "OK?" });

    const confirmBtn = document.querySelector<HTMLButtonElement>(".modal-footer .primary, .modal-footer button:last-child");
    expect(confirmBtn).not.toBeNull();
    confirmBtn?.click();

    await expect(promise).resolves.toBe(true);
  });

  it("resolves false when the user clicks the Cancel button", async () => {
    const promise = showConfirm({ title: "Proceed?", message: "OK?" });

    const cancelBtn = document.querySelector<HTMLButtonElement>(".modal-footer button:first-child");
    expect(cancelBtn).not.toBeNull();
    cancelBtn?.click();

    await expect(promise).resolves.toBe(false);
  });

  it("resolves false when Escape is pressed", async () => {
    const promise = showConfirm({ title: "Esc test", message: "dismiss" });

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));

    await expect(promise).resolves.toBe(false);
  });

  it("resolves true when Enter is pressed", async () => {
    const promise = showConfirm({ title: "Enter test", message: "confirm" });

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));

    await expect(promise).resolves.toBe(true);
  });

  it("resolves false when clicking the X close button", async () => {
    const promise = showConfirm({ title: "Close", message: "via X" });

    const xBtn = document.querySelector<HTMLButtonElement>(".close-btn");
    expect(xBtn).not.toBeNull();
    xBtn?.click();

    await expect(promise).resolves.toBe(false);
  });

  it("resolves false when clicking the backdrop", async () => {
    const promise = showConfirm({ title: "Backdrop", message: "click outside" });

    const backdrop = document.querySelector<HTMLElement>(".modal-bg");
    expect(backdrop).not.toBeNull();
    backdrop?.click();

    await expect(promise).resolves.toBe(false);
  });

  it("applies the danger class to the confirm button when danger=true", () => {
    showConfirm({ title: "Danger", message: "careful", danger: true });

    const confirmBtn = document.querySelector<HTMLButtonElement>(".modal-footer .danger");
    expect(confirmBtn).not.toBeNull();
    expect(confirmBtn?.classList.contains("danger")).toBe(true);
  });

  it("applies the primary class to the confirm button by default", () => {
    showConfirm({ title: "Normal", message: "safe" });

    const confirmBtn = document.querySelector<HTMLButtonElement>(".modal-footer .primary");
    expect(confirmBtn).not.toBeNull();
    expect(confirmBtn?.classList.contains("primary")).toBe(true);
  });

  it("uses custom confirmLabel and cancelLabel", () => {
    showConfirm({
      title: "Custom",
      message: "labels",
      confirmLabel: "Yes, do it",
      cancelLabel: "Nope",
    });

    const footer = document.querySelector(".modal-footer");
    expect(footer?.textContent).toContain("Yes, do it");
    expect(footer?.textContent).toContain("Nope");
  });

  it("removes the wrapper from the DOM after resolution", async () => {
    const promise = showConfirm({ title: "Cleanup", message: "test" });

    const wrapperBefore = document.querySelectorAll('[role="dialog"]');
    expect(wrapperBefore.length).toBe(1);

    document.querySelector<HTMLButtonElement>(".modal-footer button:last-child")?.click();
    await promise;

    // The wrapper div is removed (it's no longer connected).
    const dialog = document.querySelector('[role="dialog"]');
    expect(dialog).toBeNull();
  });
});

describe("showPrompt", () => {
  it("renders an input field with the given initial value", () => {
    showPrompt("Enter name", "What should we call you?", "Alice");

    const input = document.querySelector<HTMLInputElement>("#show-prompt-input");
    expect(input).not.toBeNull();
    expect(input?.value).toBe("Alice");
  });

  it("resolves the trimmed input value when the user confirms", async () => {
    const promise = showPrompt("Name", "Enter value");

    const input = document.querySelector<HTMLInputElement>("#show-prompt-input")!;
    input.value = "  Bob  ";
    input.dispatchEvent(new Event("input"));

    const okBtn = document.querySelector<HTMLButtonElement>(".modal-footer .primary, .modal-footer button:last-child");
    okBtn?.click();

    await expect(promise).resolves.toBe("Bob");
  });

  it("resolves null when the user cancels", async () => {
    const promise = showPrompt("Name", "Enter value");

    const cancelBtn = document.querySelector<HTMLButtonElement>(".modal-footer button:first-child");
    cancelBtn?.click();

    await expect(promise).resolves.toBeNull();
  });

  it("resolves null when Escape is pressed", async () => {
    const promise = showPrompt("Esc prompt", "dismiss");

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));

    await expect(promise).resolves.toBeNull();
  });

  it("resolves the trimmed value when Enter is pressed in the input", async () => {
    const promise = showPrompt("Enter prompt", "type here");

    const input = document.querySelector<HTMLInputElement>("#show-prompt-input")!;
    input.value = "  typed  ";
    input.dispatchEvent(new Event("input"));
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));

    await expect(promise).resolves.toBe("typed");
  });
});

describe("showCopyError", () => {
  it("calls showToast with the formatted error message", async () => {
    const toastMod = await import("../components/toast.js");
    const spy = vi.spyOn(toastMod, "showToast").mockImplementation(() => {});

    showCopyError(new Error("disk full"), "Copy failed");

    expect(spy).toHaveBeenCalledOnce();
    expect(spy).toHaveBeenCalledWith("Copy failed: disk full", "error");
  });
});
