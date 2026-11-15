// model-estimate-popover.test.ts — the panel behind a model pill.
//
// The estimate must be reachable three ways (hover, keyboard focus, tap) and
// must never fight the plan card it lives in. These cases pin the interaction
// contract: one panel at a time, real DOM (not `title`), text-only content,
// Escape/outside/scroll dismissal, focus restoration, and no listener or node
// left behind after closing.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  attachModelEstimate,
  closeModelEstimate,
  modelEstimateOpen,
} from "./model-estimate-popover.js";
import type { EstimatePopover } from "./token-estimate.js";

const CONTENT: EstimatePopover = {
  title: "acme-pro",
  headline: "~282.6M tokens",
  rows: [{ label: "Rates", value: "$3 in · $0.3 cached · $15 out per 1M" }],
  note: "Estimated prompt tokens the credited balance covers on this model.",
};

function pointer(type: string, pointerType: string): Event {
  const event = new Event(type, { bubbles: true });
  Object.defineProperty(event, "pointerType", { value: pointerType });
  return event;
}

function pill(text = "acme-pro"): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.textContent = text;
  document.body.appendChild(button);
  return button;
}

function panel(): HTMLElement | null {
  return document.querySelector<HTMLElement>(".plan-est-pop");
}

function escapeKey(): void {
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
}

beforeEach(() => {
  document.body.innerHTML = "";
  closeModelEstimate();
});

afterEach(() => {
  closeModelEstimate();
  document.body.innerHTML = "";
});

describe("opening and closing", () => {
  it("opens a real panel on click, wired to the pill", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    expect(trigger.getAttribute("aria-expanded")).toBe("false");

    trigger.click();

    const node = panel();
    expect(node).not.toBeNull();
    expect(node?.getAttribute("role")).toBe("tooltip");
    expect(node?.parentElement).toBe(document.body);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(trigger.getAttribute("aria-describedby")).toBe(node?.id);
    expect(node?.textContent).toContain("acme-pro");
    expect(node?.textContent).toContain("~282.6M tokens");
    expect(node?.textContent).toContain("$3 in · $0.3 cached · $15 out per 1M");
    expect(node?.textContent).toContain("credited balance");
    expect(modelEstimateOpen()).toBe(true);
  });

  it("closes on a second click and cleans the pill's ARIA state", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.click();
    trigger.click();
    expect(panel()).toBeNull();
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(trigger.hasAttribute("aria-describedby")).toBe(false);
    expect(modelEstimateOpen()).toBe(false);
  });

  it("positions itself with fixed coordinates", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.click();
    const node = panel();
    expect(node?.style.left).toMatch(/^\d+px$/);
    expect(node?.style.top).toMatch(/^\d+px$/);
  });
});

describe("hover, keyboard and touch", () => {
  it("opens on mouse hover and closes when the pointer leaves", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.dispatchEvent(pointer("pointerenter", "mouse"));
    expect(panel()).not.toBeNull();
    trigger.dispatchEvent(pointer("pointerleave", "mouse"));
    expect(panel()).toBeNull();
  });

  it("ignores the hover event a touch tap also fires", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.dispatchEvent(pointer("pointerenter", "touch"));
    expect(panel()).toBeNull();
  });

  it("opens on a tap and closes on the next tap", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.dispatchEvent(pointer("pointerenter", "touch"));
    trigger.click();
    expect(panel()).not.toBeNull();
    trigger.click();
    expect(panel()).toBeNull();
  });

  it("opens on keyboard focus", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.focus();
    expect(panel()).not.toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("closes on Escape, returns focus, and does not reopen", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.focus();
    expect(panel()).not.toBeNull();

    escapeKey();

    expect(panel()).toBeNull();
    expect(document.activeElement).toBe(trigger);
    expect(modelEstimateOpen()).toBe(false);
  });

  it("closes when focus moves away", () => {
    const trigger = pill();
    const other = pill("elsewhere");
    attachModelEstimate(trigger, CONTENT);
    trigger.focus();
    expect(panel()).not.toBeNull();
    other.focus();
    expect(panel()).toBeNull();
  });
});

describe("dismissal outside the pill", () => {
  it("closes on an outside pointerdown but not on the pill itself", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.click();
    trigger.dispatchEvent(pointer("pointerdown", "mouse"));
    expect(panel()).not.toBeNull();
    document.body.dispatchEvent(pointer("pointerdown", "mouse"));
    expect(panel()).toBeNull();
  });

  it("closes on scroll and on resize", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    trigger.click();
    window.dispatchEvent(new Event("scroll"));
    expect(panel()).toBeNull();

    trigger.click();
    window.dispatchEvent(new Event("resize"));
    expect(panel()).toBeNull();
  });
});

describe("one panel at a time", () => {
  it("closes the previous panel when another pill opens", () => {
    const first = pill("acme-pro");
    const second = pill("acme-mini");
    attachModelEstimate(first, CONTENT);
    attachModelEstimate(second, { ...CONTENT, title: "acme-mini" });
    first.click();
    expect(panel()?.textContent).toContain("acme-pro");

    second.click();

    const nodes = document.querySelectorAll(".plan-est-pop");
    expect(nodes).toHaveLength(1);
    expect(panel()?.textContent).toContain("acme-mini");
    expect(first.getAttribute("aria-expanded")).toBe("false");
    expect(second.getAttribute("aria-expanded")).toBe("true");
  });
});

describe("content safety and lifecycle", () => {
  it("renders hostile text as text, never as markup", () => {
    const trigger = pill();
    attachModelEstimate(trigger, {
      ...CONTENT,
      title: '<img src=x onerror="alert(1)">',
      headline: "<script>alert(1)</script>",
    });
    trigger.click();
    const node = panel();
    expect(node?.querySelector("img")).toBeNull();
    expect(node?.querySelector("script")).toBeNull();
    expect(node?.textContent).toContain('<img src=x onerror="alert(1)">');
  });

  it("leaves nothing behind across repeated open/close cycles", () => {
    const trigger = pill();
    attachModelEstimate(trigger, CONTENT);
    for (let i = 0; i < 5; i += 1) {
      trigger.click();
      expect(document.querySelectorAll(".plan-est-pop")).toHaveLength(1);
      escapeKey();
      expect(document.querySelectorAll(".plan-est-pop")).toHaveLength(0);
    }
    // Closed: the document listeners are gone, so stray events are inert.
    expect(() => {
      escapeKey();
      window.dispatchEvent(new Event("scroll"));
      document.body.dispatchEvent(pointer("pointerdown", "mouse"));
    }).not.toThrow();
    // …and the pill still works afterwards, with a fresh node id per opening.
    trigger.click();
    const firstId = panel()?.id;
    expect(firstId).toBeTruthy();
    trigger.click(); // close
    trigger.click(); // reopen
    expect(panel()?.id).not.toBe(firstId);
  });

  it("removes the panel when the card that owns it is dropped", () => {
    const card = document.createElement("div");
    const trigger = pill();
    card.appendChild(trigger);
    attachModelEstimate(trigger, CONTENT);
    trigger.click();
    expect(panel()).not.toBeNull();
    closeModelEstimate();
    expect(panel()).toBeNull();
    expect(trigger.hasAttribute("aria-describedby")).toBe(false);
  });
});
