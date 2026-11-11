// components/toast.test.ts — presentation + lifecycle contract for the transient toast stack.
//
// Covers the accessibility semantics (region + live roles), the bounded stack, per-type
// durations, hover/focus pause with remaining-time resume, Escape dismissal and the removal of
// the container once the last toast leaves.

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import * as i18n from "../i18n/index.js";
import { showToast, ensureToastContainer } from "./toast.js";

function toastEls(): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>(".toast"));
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.spyOn(i18n, "t").mockImplementation((key: string) =>
    key === "common.close" ? "Close" : key === "notifications.title" ? "Notifications" : key
  );
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback): number => {
    cb(0);
    return 1;
  });
  document.getElementById("toast-container")?.remove();
  document.body.innerHTML = "";
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("toast — semantics", () => {
  it("creates a labelled region and a status toast with an accessible close button", () => {
    showToast("Saved", "success", 1000);
    const container = document.getElementById("toast-container");
    expect(container).not.toBeNull();
    expect(container!.getAttribute("role")).toBe("region");
    expect(container!.getAttribute("aria-label")).toBe("Notifications");

    const toast = toastEls()[0]!;
    expect(toast.className).toContain("toast-success");
    expect(toast.getAttribute("role")).toBe("status");
    expect(toast.getAttribute("aria-atomic")).toBe("true");
    expect(toast.querySelector(".toast-content")!.textContent).toBe("Saved");
    const close = toast.querySelector(".toast-close")!;
    expect(close.getAttribute("aria-label")).toBe("Close");
    expect(close.getAttribute("title")).toBe("Close");
  });

  it("uses an alert role for errors and normalises the warn alias", () => {
    showToast("Boom", "error", 1000);
    showToast("Careful", "warn", 1000);
    showToast("Unknown", "nonsense", 1000);

    const [err, warn, info] = toastEls();
    expect(err!.getAttribute("role")).toBe("alert");
    expect(err!.className).toContain("toast-error");
    expect(warn!.className).toContain("toast-warning");
    expect(info!.className).toContain("toast-info");
    expect(info!.getAttribute("role")).toBe("status");
  });

  it("keeps long URLs and JSON payloads intact in the DOM", () => {
    const payload = '{"error":"rate_limit","endpoint":"https://api.example.com/v1/chat/completions?stream=true"}';
    showToast(payload, "error", 1000);
    expect(toastEls()[0]!.querySelector(".toast-content")!.textContent).toBe(payload);
  });
});

describe("toast — lifecycle", () => {
  it("auto-dismisses after the per-type duration and removes the empty container", async () => {
    showToast("Saved", "success"); // 3500ms default
    const toast = toastEls()[0]!;
    await vi.advanceTimersByTimeAsync(3499);
    expect(toast.isConnected).toBe(true);
    await vi.advanceTimersByTimeAsync(2);
    expect(toast.classList.contains("show")).toBe(false);
    await vi.advanceTimersByTimeAsync(220);
    expect(toast.isConnected).toBe(false);
    expect(document.getElementById("toast-container")).toBeNull();
  });

  it("pauses on hover and resumes with the remaining time", async () => {
    showToast("Paused", "info", 1000);
    const toast = toastEls()[0]!;
    await vi.advanceTimersByTimeAsync(400);

    toast.dispatchEvent(new MouseEvent("mouseenter"));
    await vi.advanceTimersByTimeAsync(5000);
    expect(toast.isConnected).toBe(true);

    toast.dispatchEvent(new MouseEvent("mouseleave"));
    await vi.advanceTimersByTimeAsync(599);
    expect(toast.isConnected).toBe(true);
    await vi.advanceTimersByTimeAsync(2);
    expect(toast.classList.contains("show")).toBe(false);
    await vi.advanceTimersByTimeAsync(220);
    expect(toast.isConnected).toBe(false);
  });

  it("pauses while keyboard focus is inside and resumes on focusout", async () => {
    showToast("Focused", "warning", 1000);
    const toast = toastEls()[0]!;
    const close = toast.querySelector<HTMLElement>(".toast-close")!;
    await vi.advanceTimersByTimeAsync(300);

    close.dispatchEvent(new FocusEvent("focusin", { bubbles: true }));
    await vi.advanceTimersByTimeAsync(3000);
    expect(toast.isConnected).toBe(true);

    close.dispatchEvent(new FocusEvent("focusout", { bubbles: true }));
    await vi.advanceTimersByTimeAsync(699);
    expect(toast.isConnected).toBe(true);
    await vi.advanceTimersByTimeAsync(2);
    expect(toast.classList.contains("show")).toBe(false);
  });

  it("caps the visible stack at three and evicts the oldest immediately", () => {
    showToast("one", "info", 60_000);
    showToast("two", "info", 60_000);
    showToast("three", "info", 60_000);
    showToast("four", "info", 60_000);

    const contents = toastEls().map((t) => t.querySelector(".toast-content")!.textContent);
    expect(contents).toEqual(["two", "three", "four"]);
  });

  it("dismisses the focused toast with Escape", async () => {
    showToast("Escape me", "info", 60_000);
    const toast = toastEls()[0]!;
    toast.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await vi.advanceTimersByTimeAsync(220);
    expect(toast.isConnected).toBe(false);
  });

  it("pauses while the tab is hidden and resumes when visible", async () => {
    showToast("Hidden", "info", 1000);
    const toast = toastEls()[0]!;
    await vi.advanceTimersByTimeAsync(300);

    const hidden = vi.spyOn(document, "hidden", "get").mockReturnValue(true);
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(3000);
    expect(toast.isConnected).toBe(true);

    hidden.mockReturnValue(false);
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(699);
    expect(toast.isConnected).toBe(true);
    await vi.advanceTimersByTimeAsync(2);
    expect(toast.classList.contains("show")).toBe(false);
  });

  it("is idempotent when asked for the container twice", () => {
    const a = ensureToastContainer();
    const b = ensureToastContainer();
    expect(a).toBe(b);
    expect(document.querySelectorAll("#toast-container")).toHaveLength(1);
  });
});
