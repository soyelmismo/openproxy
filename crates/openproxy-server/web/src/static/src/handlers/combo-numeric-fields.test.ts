import { describe, it, expect, beforeEach, afterEach, vi, type Mock } from "vitest";
import {
  updateCooldownBase,
  updateCooldownFactor,
  updateCooldownMax,
  updateSelectionWindow,
} from "./combo-handlers.js";
import { state } from "../state/index.js";
import * as toastModule from "../components/toast.js";
import * as reactiveModule from "../state/reactive.js";
import type { Combo } from "../lib/types/api.js";

const origFetch = globalThis.fetch;

function makeEvent(value: string | null, type = "change"): Event {
  const input = document.createElement("input");
  if (value !== null) {
    input.value = value;
  }
  const event = new Event(type);
  Object.defineProperty(event, "target", { value: input, configurable: true });
  return event;
}

function requireCall(mock: Mock<typeof fetch>, index = 0) {
  const call = mock.mock.calls[index];
  if (!call) {
    throw new Error(`Expected fetch call at index ${index}`);
  }
  const [url, init] = call;
  const bodyStr = typeof init?.body === "string" ? init.body : "{}";
  const payload = JSON.parse(bodyStr) as Record<string, unknown>;
  return { url, init, payload };
}

const handlers = [
  {
    name: "updateCooldownBase",
    fn: updateCooldownBase,
    field: "cooldown_base_secs" as const,
    label: "Base",
  },
  {
    name: "updateCooldownFactor",
    fn: updateCooldownFactor,
    field: "cooldown_factor" as const,
    label: "Factor",
  },
  {
    name: "updateCooldownMax",
    fn: updateCooldownMax,
    field: "cooldown_max_secs" as const,
    label: "Max",
  },
  {
    name: "updateSelectionWindow",
    fn: updateSelectionWindow,
    field: "selection_window_secs" as const,
    label: "Window",
  },
];

describe("combo numeric field handlers deduplication", () => {
  let fetchMock: Mock<typeof fetch>;

  beforeEach(() => {
    document.body.innerHTML = "";
    const combo: Combo = {
      id: 42,
      name: "test-combo",
      strategy: "priority",
      race_size: 1,
      created_at: "2026-01-01T00:00:00Z",
      cooldown_base_secs: 60,
      cooldown_factor: 2,
      cooldown_max_secs: 3600,
      selection_window_secs: 3600,
    };
    state.combos = [combo];

    fetchMock = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    globalThis.fetch = fetchMock;
  });

  afterEach(() => {
    globalThis.fetch = origFetch;
    vi.restoreAllMocks();
    document.body.innerHTML = "";
  });

  describe("input event ignores API call", () => {
    for (const { name, fn } of handlers) {
      it(`${name} ignores event when type is 'input'`, async () => {
        await fn(42, makeEvent("100", "input"));
        expect(fetchMock).not.toHaveBeenCalled();
      });
    }
  });

  describe("change event with trimmed string sends correct payload", () => {
    for (const { name, fn, field } of handlers) {
      it(`${name} trims whitespace and sends parsed integer for ${field}`, async () => {
        await fn(42, makeEvent("  12  ", "change"));
        expect(fetchMock).toHaveBeenCalledOnce();
        const { url, init, payload } = requireCall(fetchMock);
        expect(url).toBe("/admin/api/combos/42");
        expect(init?.method).toBe("PATCH");
        expect(payload).toEqual({ [field]: 12 });
      });
    }
  });

  describe("empty string or null event sends null payload", () => {
    for (const { name, fn, field } of handlers) {
      it(`${name} sends null when value is empty string`, async () => {
        await fn(42, makeEvent("", "change"));
        expect(fetchMock).toHaveBeenCalledOnce();
        const { payload } = requireCall(fetchMock);
        expect(payload).toEqual({ [field]: null });
      });

      it(`${name} sends null when event is null`, async () => {
        await fn(42, null);
        expect(fetchMock).toHaveBeenCalledOnce();
        const { payload } = requireCall(fetchMock);
        expect(payload).toEqual({ [field]: null });
      });
    }
  });

  describe("invalid non-numeric input triggers toast and requestUpdate without calling API", () => {
    for (const { name, fn, label } of handlers) {
      it(`${name} shows toast '${label} must be a number or empty', calls requestUpdate and does not call fetch`, async () => {
        const toastSpy = vi.spyOn(toastModule, "showToast").mockImplementation(() => {});
        const reqUpdateSpy = vi.spyOn(reactiveModule, "requestUpdate").mockImplementation(() => {});

        await fn(42, makeEvent("abc", "change"));

        expect(toastSpy).toHaveBeenCalledWith(`${label} must be a number or empty`, "error");
        expect(reqUpdateSpy).toHaveBeenCalledOnce();
        expect(fetchMock).not.toHaveBeenCalled();
      });
    }
  });

  describe("decimal truncation and negative value preservation via parseInt", () => {
    for (const { name, fn, field } of handlers) {
      it(`${name} truncates '1.9' to 1`, async () => {
        await fn(42, makeEvent("1.9", "change"));
        const { payload } = requireCall(fetchMock);
        expect(payload).toEqual({ [field]: 1 });
      });

      it(`${name} preserves negative integer '-5'`, async () => {
        await fn(42, makeEvent("-5", "change"));
        const { payload } = requireCall(fetchMock);
        expect(payload).toEqual({ [field]: -5 });
      });
    }
  });

  describe("successful patch updates state in-place without triggering requestUpdate", () => {
    for (const { name, fn, field } of handlers) {
      it(`${name} updates state.combos in-place and avoids DOM rebuild`, async () => {
        const reqUpdateSpy = vi.spyOn(reactiveModule, "requestUpdate").mockImplementation(() => {});

        await fn(42, makeEvent("99", "change"));

        expect(reqUpdateSpy).not.toHaveBeenCalled();
        const combo = state.combos.find((c) => c.id === 42);
        expect(combo).toBeDefined();
        expect(combo?.[field]).toBe(99);
      });
    }
  });

  describe("PATCH error handling preserves original behavior", () => {
    for (const { name, fn } of handlers) {
      it(`${name} captures error gracefully and does not throw`, async () => {
        fetchMock.mockResolvedValueOnce(
          new Response("Server Error", { status: 500, headers: { "Content-Type": "text/plain" } }),
        );
        const toastSpy = vi.spyOn(toastModule, "showToast").mockImplementation(() => {});
        const reqUpdateSpy = vi.spyOn(reactiveModule, "requestUpdate").mockImplementation(() => {});

        await expect(fn(42, makeEvent("50", "change"))).resolves.toBeUndefined();

        expect(toastSpy).toHaveBeenCalledWith(expect.stringContaining("Error: 500: Server Error"), "error");
        expect(reqUpdateSpy).not.toHaveBeenCalled();
      });
    }
  });
});
