// handlers/async-button-feedback.test.ts — regression tests for immediate, resilient execution
// feedback on the dashboard's async buttons.
//
// These exercise the real handlers end-to-end through the fetch-backed `api()` (only `fetch` is
// stubbed), so the busy contract is asserted against the same code path the dashboard runs:
// while the request is pending the button must show aria-busy + spinner + processing text and
// be disabled; after success *or* failure it must be restored. The historical bug left
// "Testing..." buttons disabled forever when the request rejected.

import { describe, it, expect, beforeEach, afterEach, vi, type Mock } from "vitest";
import { state } from "../state/index.js";
import { createCombo, testAllTargets } from "./combo-handlers.js";
import { testModel } from "./model-handlers/row.js";
import { bulkTestSelected } from "./model-handlers/bulk.js";

const origFetch = globalThis.fetch;

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (err: unknown) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function makeButton(label: string, id?: string): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.textContent = label;
  if (id) btn.id = id;
  document.body.appendChild(btn);
  return btn;
}

/** The event a real lit `@click` handler receives (currentTarget is only valid during dispatch). */
function clickEvent(btn: HTMLButtonElement): Event {
  let captured: Event | null = null;
  btn.addEventListener("click", (ev) => { captured = ev; }, { once: true });
  btn.click();
  if (!captured) throw new Error("click event was not dispatched");
  return captured;
}

/** Confirm the real `showConfirm` modal, as an operator would. */
async function confirmDialog(): Promise<void> {
  await Promise.resolve();
  const dialog = document.getElementById("show-confirm-dialog");
  const confirm = dialog?.querySelector<HTMLButtonElement>(".modal-footer .primary");
  if (!confirm) throw new Error("confirm dialog did not open");
  confirm.click();
  await Promise.resolve();
}

function postCalls(fetchMock: Mock<typeof fetch>): number {
  return fetchMock.mock.calls.filter(([, init]) => init?.method === "POST").length;
}

describe("async button feedback", () => {
  let fetchMock: Mock<typeof fetch>;

  beforeEach(() => {
    document.body.innerHTML = "";
    state.selectedModels = new Set<unknown>();
    fetchMock = vi.fn<typeof fetch>();
    globalThis.fetch = fetchMock;
  });

  afterEach(() => {
    globalThis.fetch = origFetch;
    vi.restoreAllMocks();
    vi.useRealTimers();
    document.body.innerHTML = "";
  });

  describe("testAllTargets", () => {
    it("shows the busy contract while the run is pending and restores on success", async () => {
      const req = deferred<Response>();
      fetchMock.mockReturnValue(req.promise);
      const btn = makeButton("Test all");
      const ev = clickEvent(btn);

      const run = testAllTargets(7, ev);
      expect(btn.getAttribute("aria-busy")).toBe("true");
      expect(btn.disabled).toBe(true);
      expect(btn.querySelector(".button-busy-spinner")).not.toBeNull();
      expect(btn.textContent).toBe("Testing…");

      // A second click while busy must not fire another upstream run.
      btn.click();
      expect(postCalls(fetchMock)).toBe(1);

      req.resolve(jsonResponse([]));
      await run;
      expect(btn.getAttribute("aria-busy")).toBeNull();
      expect(btn.disabled).toBe(false);
      expect(btn.textContent).toBe("Test all");
    });

    it("restores the button when the run rejects", async () => {
      fetchMock.mockRejectedValue(new Error("upstream down"));
      const btn = makeButton("Test all");
      const ev = clickEvent(btn);

      await testAllTargets(7, ev);
      expect(btn.disabled).toBe(false);
      expect(btn.getAttribute("aria-busy")).toBeNull();
      expect(btn.textContent).toBe("Test all");
    });
  });

  describe("testModel", () => {
    it("shows Testing… while pending, flashes the result, then restores the label", async () => {
      vi.useFakeTimers();
      const req = deferred<Response>();
      fetchMock.mockReturnValue(req.promise);
      const btn = makeButton("Test", "test-btn-4");
      const ev = clickEvent(btn);

      const run = testModel(4, "m-4", ev);
      expect(btn.getAttribute("aria-busy")).toBe("true");
      expect(btn.textContent).toBe("Testing…");

      req.resolve(jsonResponse({ status: 200, elapsed_ms: 12, row_id: 4 }));
      await run;
      expect(btn.textContent).toBe("✓");
      expect(btn.disabled).toBe(true);

      await vi.advanceTimersByTimeAsync(1500);
      expect(btn.textContent).toBe("Test");
      expect(btn.disabled).toBe(false);
      expect(btn.getAttribute("aria-busy")).toBeNull();
    });

    it("never leaves the button stuck on Testing… after a network error", async () => {
      vi.useFakeTimers();
      fetchMock.mockRejectedValue(new Error("timeout"));
      const btn = makeButton("Test", "test-btn-5");
      const ev = clickEvent(btn);

      await testModel(5, "m-5", ev);
      expect(btn.getAttribute("aria-busy")).toBeNull();
      expect(btn.textContent).toBe("✗");

      await vi.advanceTimersByTimeAsync(1500);
      expect(btn.disabled).toBe(false);
      expect(btn.textContent).toBe("Test");
    });
  });

  describe("bulkTestSelected", () => {
    it("restores every row button even when one test rejects (no permanent disabled state)", async () => {
      vi.useFakeTimers();
      state.selectedModels = new Set<unknown>([9]);
      fetchMock.mockRejectedValue(new Error("boom"));
      const bulkBtn = makeButton("Test selected");
      const rowBtn = makeButton("Test", "test-btn-9");
      const ev = clickEvent(bulkBtn);

      const run = bulkTestSelected("p", ev);
      await confirmDialog();
      await run;

      expect(bulkBtn.getAttribute("aria-busy")).toBeNull();
      expect(bulkBtn.disabled).toBe(false);
      expect(rowBtn.getAttribute("aria-busy")).toBeNull();
      expect(rowBtn.textContent).toBe("✗");

      await vi.advanceTimersByTimeAsync(1500);
      expect(rowBtn.disabled).toBe(false);
      expect(rowBtn.textContent).toBe("Test");
    });
  });

  describe("createCombo", () => {
    it("disables the submit button with Creating… until the POST + refetch settle", async () => {
      const post = deferred<Response>();
      fetchMock.mockImplementation((input, init) => {
        const url = String(input);
        if (url.endsWith("/combos") && init?.method === "POST") return post.promise;
        return Promise.resolve(jsonResponse([]));
      });
      const form = document.createElement("form");
      const btn = document.createElement("button");
      btn.type = "submit";
      btn.textContent = "Create";
      const name = document.createElement("input");
      name.name = "name";
      name.value = "fast";
      form.append(name, btn);
      document.body.appendChild(form);

      let run!: Promise<void>;
      form.addEventListener("submit", (ev) => { run = createCombo(ev); });
      form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));

      expect(btn.getAttribute("aria-busy")).toBe("true");
      expect(btn.disabled).toBe(true);
      expect(btn.textContent).toBe("Creating…");

      post.resolve(jsonResponse({ id: 1 }));
      await run;
      expect(btn.getAttribute("aria-busy")).toBeNull();
      expect(btn.disabled).toBe(false);
      expect(btn.textContent).toBe("Create");
    });
  });
});
