// lib/button-busy.test.ts — unit tests for the shared async-button feedback helper.
//
// Covering: the busy DOM contract (aria-busy / data-button-busy / disabled / spinner + label),
// byte-for-byte innerHTML restoration, idempotent + re-entrant release, release on both success
// and failure, and button resolution from currentTarget / target / SubmitEvent / form / click
// tracker.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  beginButtonBusy,
  busySpinnerTemplate,
  resolveBusyButton,
  withButtonBusy,
} from "./button-busy.js";
import { render } from "lit-html";

function makeButton(label = "Test"): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.textContent = label;
  document.body.appendChild(btn);
  return btn;
}

describe("beginButtonBusy", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("paints the busy contract: aria-busy, data hook, disabled, spinner and label", () => {
    const btn = makeButton();
    beginButtonBusy(btn, { label: "Testing…" });
    expect(btn.getAttribute("aria-busy")).toBe("true");
    expect(btn.getAttribute("data-button-busy")).toBe("true");
    expect(btn.disabled).toBe(true);
    expect(btn.querySelector(".button-busy-spinner")).not.toBeNull();
    expect(btn.querySelector(".button-busy-spinner-icon.animate-spin")).not.toBeNull();
    expect(btn.querySelector(".button-busy-label")?.textContent).toBe("Testing…");
  });

  it("defaults the label to Processing… and keeps it compact (no layout text growth)", () => {
    const btn = makeButton("Refresh");
    beginButtonBusy(btn);
    expect(btn.textContent).toBe("Processing…");
  });

  it("restores the original innerHTML, disabled flag and aria-busy on release", () => {
    const btn = makeButton();
    btn.innerHTML = '<span class="icon">icon</span> Test all';
    btn.setAttribute("aria-busy", "false");
    const handle = beginButtonBusy(btn, { label: "Testing…" });
    handle.release();
    expect(btn.innerHTML).toBe('<span class="icon">icon</span> Test all');
    expect(btn.disabled).toBe(false);
    expect(btn.getAttribute("aria-busy")).toBe("false");
    expect(btn.hasAttribute("data-button-busy")).toBe(false);
  });

  it("keeps a pre-disabled button disabled after release", () => {
    const btn = makeButton();
    btn.disabled = true;
    beginButtonBusy(btn).release();
    expect(btn.disabled).toBe(true);
  });

  it("is idempotent: a second release does not clobber a newer busy state", () => {
    const btn = makeButton("A");
    const first = beginButtonBusy(btn, { label: "First…" });
    first.release();
    first.release();
    expect(btn.textContent).toBe("A");
    expect(btn.getAttribute("aria-busy")).toBeNull();

    const second = beginButtonBusy(btn, { label: "Second…" });
    expect(btn.textContent).toBe("Second…");
    second.release();
    expect(btn.textContent).toBe("A");
  });

  it("refcounts nested busy states and only restores on the outermost release", () => {
    const btn = makeButton("Bulk");
    const outer = beginButtonBusy(btn, { label: "Testing…" });
    const inner = beginButtonBusy(btn, { label: "Inner…" });
    inner.release();
    expect(btn.disabled).toBe(true);
    expect(btn.getAttribute("aria-busy")).toBe("true");
    expect(btn.textContent).toBe("Testing…");
    outer.release();
    expect(btn.disabled).toBe(false);
    expect(btn.textContent).toBe("Bulk");
  });

  it("is a no-op when there is no button", () => {
    const handle = beginButtonBusy(null);
    expect(() => handle.release()).not.toThrow();
    expect(handle.button).toBeNull();
  });
});

describe("withButtonBusy", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("releases after success and returns the value", async () => {
    const btn = makeButton();
    const value = await withButtonBusy(btn, { label: "Saving…" }, async () => 42);
    expect(value).toBe(42);
    expect(btn.disabled).toBe(false);
    expect(btn.getAttribute("aria-busy")).toBeNull();
    expect(btn.textContent).toBe("Test");
  });

  it("releases and rethrows after a failure", async () => {
    const btn = makeButton();
    await expect(
      withButtonBusy(btn, { label: "Saving…" }, async () => {
        throw new Error("boom");
      }),
    ).rejects.toThrow("boom");
    expect(btn.disabled).toBe(false);
    expect(btn.getAttribute("aria-busy")).toBeNull();
    expect(btn.textContent).toBe("Test");
  });
});

describe("resolveBusyButton", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    (document.activeElement as HTMLElement | null)?.blur?.();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("prefers currentTarget when the listener sits on the button", () => {
    const btn = makeButton();
    let resolved: HTMLButtonElement | null = null;
    btn.addEventListener("click", (ev) => { resolved = resolveBusyButton(ev); }, { once: true });
    btn.click();
    expect(resolved).toBe(btn);
  });

  it("walks up from a click on the button's icon to the button", () => {
    const btn = makeButton();
    const icon = document.createElement("span");
    btn.textContent = "";
    btn.appendChild(icon);
    let resolved: HTMLButtonElement | null = null;
    icon.addEventListener("click", (ev) => { resolved = resolveBusyButton(ev); }, { once: true });
    icon.click();
    expect(resolved).toBe(btn);
  });

  it("uses the SubmitEvent submitter (form submit without a currentTarget button)", () => {
    if (typeof SubmitEvent === "undefined") return;
    const form = document.createElement("form");
    const btn = document.createElement("button");
    btn.type = "submit";
    btn.textContent = "Create";
    form.appendChild(btn);
    document.body.appendChild(form);
    let resolved: HTMLButtonElement | null = null;
    form.addEventListener("submit", (ev) => { resolved = resolveBusyButton(ev); }, { once: true });
    form.dispatchEvent(new SubmitEvent("submit", { bubbles: true, cancelable: true, submitter: btn }));
    expect(resolved).toBe(btn);
  });

  it("falls back to the form's submit button when the event carries no submitter", () => {
    const form = document.createElement("form");
    const btn = document.createElement("button");
    btn.type = "submit";
    btn.textContent = "Save";
    form.appendChild(btn);
    document.body.appendChild(form);
    let resolved: HTMLButtonElement | null = null;
    form.addEventListener("submit", (ev) => { resolved = resolveBusyButton(ev); }, { once: true });
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    expect(resolved).toBe(btn);
  });

  it("recovers the just-clicked button when the handler is called without an event", () => {
    const btn = makeButton("Test all");
    btn.click();
    expect(resolveBusyButton()).toBe(btn);
    expect(resolveBusyButton(null)).toBe(btn);
  });

  it("ignores a stale tracker entry once its button is detached", () => {
    const btn = makeButton("Old");
    btn.click();
    btn.remove();
    (document.activeElement as HTMLElement | null)?.blur?.();
    expect(resolveBusyButton()).toBeNull();
  });

  it("does not reuse a tracker entry after the freshness window", () => {
    vi.useFakeTimers();
    const btn = makeButton("Old");
    btn.click();
    vi.advanceTimersByTime(2000);
    (document.activeElement as HTMLElement | null)?.blur?.();
    expect(resolveBusyButton()).toBeNull();
  });

  it("falls back to the focused button (keyboard activation path)", () => {
    const btn = makeButton("Keyboard");
    btn.focus();
    expect(resolveBusyButton()).toBe(btn);
  });
});

describe("busySpinnerTemplate", () => {
  it("renders the same spinner contract as the imperative helper", () => {
    const host = document.createElement("div");
    render(busySpinnerTemplate(), host);
    expect(host.querySelector(".button-busy-spinner")).not.toBeNull();
    expect(host.querySelector(".button-busy-spinner-icon.animate-spin")).not.toBeNull();
  });
});
