// state/reactive.test.ts — comprehensive tests for multi-container reactivity,
// independent lifecycle, generation guards, unmountView retirement, and recovery.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { html } from "lit-html";
import {
  mountView,
  unmountView,
  requestUpdate,
  forceUpdate,
} from "./reactive.js";

let container1: HTMLElement;
let container2: HTMLElement;

beforeEach(() => {
  container1 = document.createElement("div");
  container1.id = "c1";
  container2 = document.createElement("div");
  container2.id = "c2";
  document.body.appendChild(container1);
  document.body.appendChild(container2);
});

afterEach(() => {
  unmountView(container1);
  unmountView(container2);
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

function flushMicrotasks(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

describe("multi-container reactivity", () => {
  it("mounts and renders multiple containers concurrently", () => {
    let text1 = "initial-1";
    let text2 = "initial-2";

    const cleanup1 = mountView(container1, () => html`<span>${text1}</span>`);
    const cleanup2 = mountView(container2, () => html`<div>${text2}</div>`);

    expect(container1.textContent).toBe("initial-1");
    expect(container2.textContent).toBe("initial-2");

    cleanup1();
    cleanup2();
  });

  it("coalesces requestUpdate across all active containers into one microtask", async () => {
    let renderCount1 = 0;
    let renderCount2 = 0;
    let val1 = "a";
    let val2 = "x";

    mountView(container1, () => {
      renderCount1++;
      return html`<span>${val1}</span>`;
    });
    mountView(container2, () => {
      renderCount2++;
      return html`<span>${val2}</span>`;
    });

    // Initial mount rendered once each synchronously
    expect(renderCount1).toBe(1);
    expect(renderCount2).toBe(1);

    val1 = "b";
    val2 = "y";
    // Multiple calls to requestUpdate
    requestUpdate();
    requestUpdate();
    requestUpdate();

    // Still not rendered before microtask
    expect(renderCount1).toBe(1);
    expect(renderCount2).toBe(1);

    await flushMicrotasks();

    expect(renderCount1).toBe(2);
    expect(renderCount2).toBe(2);
    expect(container1.textContent).toBe("b");
    expect(container2.textContent).toBe("y");
  });

  it("forceUpdate re-renders all mounted containers synchronously", () => {
    let val1 = "f1";
    let val2 = "f2";

    mountView(container1, () => html`<span>${val1}</span>`);
    mountView(container2, () => html`<span>${val2}</span>`);

    val1 = "f1-updated";
    val2 = "f2-updated";

    forceUpdate();

    expect(container1.textContent).toBe("f1-updated");
    expect(container2.textContent).toBe("f2-updated");
  });

  it("supports independent cleanup without affecting other mounted containers", async () => {
    let val1 = "v1";
    let val2 = "v2";

    const cleanup1 = mountView(container1, () => html`<span>${val1}</span>`);
    mountView(container2, () => html`<span>${val2}</span>`);

    cleanup1();

    val1 = "v1-changed";
    val2 = "v2-changed";
    requestUpdate();
    await flushMicrotasks();

    // container1 was cleaned up: should not have re-rendered with new val1
    expect(container1.textContent).toBe("v1");
    // container2 was still mounted: updated to new val2
    expect(container2.textContent).toBe("v2-changed");
  });

  it("generation guard: an old cleanup does not deregister a remounted container", async () => {
    let valOld = "old-gen";
    let valNew = "new-gen";

    const oldCleanup = mountView(container1, () => html`<span>${valOld}</span>`);
    expect(container1.textContent).toBe("old-gen");

    // Remount into the same container element (increments generation)
    const newCleanup = mountView(container1, () => html`<span>${valNew}</span>`);
    expect(container1.textContent).toBe("new-gen");

    // Stale cleanup call from old generation
    oldCleanup();

    valNew = "new-gen-updated";
    requestUpdate();
    await flushMicrotasks();

    expect(container1.textContent).toBe("new-gen-updated");
    newCleanup();
  });

  it("unmountView synchronously deregisters and permanently retires the container", async () => {
    let text = "before-unmount";
    mountView(container1, () => html`<span>${text}</span>`);
    expect(container1.textContent).toBe("before-unmount");

    unmountView(container1);

    // Stale late async mount attempt on retired target must be ignored
    text = "stale-mount";
    const lateCleanup = mountView(container1, () => html`<span>${text}</span>`);
    expect(typeof lateCleanup).toBe("function");

    // Container should not have been updated by stale mount
    expect(container1.textContent).toBe("before-unmount");

    text = "after-update";
    requestUpdate();
    await flushMicrotasks();

    // No re-render should happen on retired container
    expect(container1.textContent).toBe("before-unmount");
  });

  it("does not render into disconnected containers during update", async () => {
    let text = "attached";
    let renderCount = 0;

    mountView(container1, () => {
      renderCount++;
      return html`<span>${text}</span>`;
    });
    expect(renderCount).toBe(1);

    // Detach from DOM
    container1.remove();

    text = "detached";
    requestUpdate();
    await flushMicrotasks();

    expect(renderCount).toBe(1);
    expect(container1.textContent).toBe("attached");
  });

  it("recovers from lit-html render errors on subsequent update", async () => {
    let shouldThrow = true;
    let text = "hello";

    const consoleSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    mountView(container1, () => {
      if (shouldThrow) {
        throw new Error("Render explosion");
      }
      return html`<span>${text}</span>`;
    });

    expect(consoleSpy).toHaveBeenCalled();

    // Next update with error cleared
    shouldThrow = false;
    text = "recovered!";
    requestUpdate();
    await flushMicrotasks();

    expect(container1.textContent).toBe("recovered!");
  });
});
