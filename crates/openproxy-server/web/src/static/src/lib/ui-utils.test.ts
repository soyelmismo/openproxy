import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";
import { ensureModalRoot, flashButton, showApiError } from "./ui-utils.js";
import * as toastModule from "../components/toast.js";

describe("ui-utils", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  describe("ensureModalRoot", () => {
    it("creates a new modal root element if none exists", () => {
      expect(document.getElementById("modal-root")).toBeNull();
      const root = ensureModalRoot();
      expect(root).toBeInstanceOf(HTMLElement);
      expect(root.id).toBe("modal-root");
      expect(root.style.position).toBe("relative");
      expect(root.style.zIndex).toBe("1000");
      expect(document.body.contains(root)).toBe(true);
    });

    it("returns the existing modal root element if it already exists", () => {
      const existing = document.createElement("div");
      existing.id = "modal-root";
      existing.setAttribute("data-test", "existing");
      document.body.appendChild(existing);

      const root = ensureModalRoot();
      expect(root).toBe(existing);
      expect(root.getAttribute("data-test")).toBe("existing");
      expect(document.querySelectorAll("#modal-root").length).toBe(1);
    });
  });

  describe("flashButton", () => {
    it("handles null button gracefully", () => {
      expect(() => flashButton(null, "Saved", "green")).not.toThrow();
    });

    it("updates text and background, then resets background after 1500ms", () => {
      const btn = document.createElement("button");
      btn.textContent = "Original";
      document.body.appendChild(btn);

      flashButton(btn, "Success!", "red");
      expect(btn.textContent).toBe("Success!");
      expect(btn.style.background).toBe("red");

      vi.advanceTimersByTime(1499);
      expect(btn.style.background).toBe("red");

      vi.advanceTimersByTime(1);
      expect(btn.style.background).toBe("");
    });
  });

  describe("showApiError", () => {
    it("formats Error objects with default prefix and delegates to showToast", () => {
      const spy = vi.spyOn(toastModule, "showToast").mockImplementation(() => {});
      showApiError(new Error("Network connection dropped"));
      expect(spy).toHaveBeenCalledWith("Error: Network connection dropped", "error");
    });

    it("formats non-Error inputs with custom prefix", () => {
      const spy = vi.spyOn(toastModule, "showToast").mockImplementation(() => {});
      showApiError("Request timed out", "Sync Failed");
      expect(spy).toHaveBeenCalledWith("Sync Failed: Request timed out", "error");
    });
  });
});
